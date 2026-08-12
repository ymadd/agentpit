//! One conversational turn, shared by the local REPL and the daemon worker (design §5.2).
//!
//! Routing → auth → session recording → (native|composed) continuation → dispatch →
//! result recording. The caller supplies an event sink for live rendering (terminal or
//! socket) and a cancellation token (Ctrl-C locally, a `cancel` request remotely). The
//! engine itself never prints.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use tokio_util::sync::CancellationToken;

use crate::config::{HubConfig, RouteKey};
use crate::dispatch::{
    Registries, dispatch_continuing, resolve_transport, supports_native_continuation,
};
use crate::events::{LegStatus, RunKind, RunLogger, output_streamer};
use crate::router::{RouteRequest, Router};
use crate::session::{ExchangeStatus, NewExchange, NewResult, SharedRecorder, TurnPlan};
use crate::types::BackendId;

/// Live signals emitted while a turn runs.
#[derive(Debug, Clone)]
pub enum EngineEvent {
    /// The route decision, before dispatch begins.
    Route {
        backend: BackendId,
        transport: &'static str,
        reason: String,
    },
    /// A streamed output chunk (already decoded — display text, not raw JSONL).
    Chunk { text: String },
    /// An out-of-band warning the user must see (e.g. a session-journal write failed:
    /// the answer is real but resume will not know about this turn). Never answer text.
    Notice { text: String },
}

/// How the turn ended. `Completed` covers every dispatched outcome (ok/error/cancelled/
/// auth-failure — mirroring the recorded `result` entry); `Unavailable` never dispatched.
/// Pre-dispatch AUTH probing is deliberately the caller's job (REPL and worker both probe
/// with `check_auth` before calling in) — it keeps the engine free of CLI side effects.
#[derive(Debug, Clone)]
pub enum TurnOutcome {
    Completed {
        backend: BackendId,
        status: ExchangeStatus,
        answer: String,
    },
    /// The resolved backend has no transport registered/available.
    Unavailable {
        backend: BackendId,
        available: Vec<BackendId>,
    },
}

impl TurnOutcome {
    /// The backend this turn resolved to (routing happens even for `Unavailable`).
    pub fn backend(&self) -> BackendId {
        match self {
            TurnOutcome::Completed { backend, .. } | TurnOutcome::Unavailable { backend, .. } => {
                *backend
            }
        }
    }
}

impl TurnEngine {
    /// Resolve the backend this task WOULD route to, without dispatching. Callers use it
    /// to run their auth probe before `run_turn`.
    pub fn resolve_backend(
        &self,
        active_backend: Option<BackendId>,
        explicit: Option<BackendId>,
        task: &str,
    ) -> BackendId {
        let available = self.regs.available();
        let profiles = crate::profile::load_profiles(None).unwrap_or_default();
        let router = Router::new(self.config.clone(), available, profiles)
            .with_suspended(crate::availability::recently_suspended());
        router
            .resolve(&RouteRequest {
                tool: RouteKey::Rescue,
                explicit_backend: explicit.or(active_backend),
                task: Some(task),
            })
            .backend
    }
}

pub struct TurnEngine {
    pub config: HubConfig,
    pub regs: Arc<Registries>,
    pub cwd: PathBuf,
}

impl TurnEngine {
    /// Run one turn. `recorder` = the session log (None = non-persisted REPL fallback);
    /// `active_backend`/`explicit` mirror the REPL's `/backend` and `!backend` overrides.
    pub async fn run_turn(
        &self,
        recorder: Option<&SharedRecorder>,
        active_backend: Option<BackendId>,
        explicit: Option<BackendId>,
        task: &str,
        cancel: CancellationToken,
        on_event: Arc<dyn Fn(EngineEvent) + Send + Sync>,
    ) -> TurnOutcome {
        let available = self.regs.available();
        let profiles = crate::profile::load_profiles(None).unwrap_or_default();
        let router = Router::new(self.config.clone(), available.clone(), profiles)
            .with_suspended(crate::availability::recently_suspended());

        let decision = router.resolve(&RouteRequest {
            tool: RouteKey::Rescue,
            explicit_backend: explicit.or(active_backend),
            task: Some(task),
        });
        let backend_id = decision.backend;

        if !available.contains(&backend_id) {
            return TurnOutcome::Unavailable {
                backend: backend_id,
                available: available.into_iter().collect(),
            };
        }

        let transport = resolve_transport(backend_id, &self.regs)
            .map(|t| t.as_str())
            .unwrap_or("none");
        on_event(EngineEvent::Route {
            backend: backend_id,
            transport,
            reason: decision.reason.as_str().to_string(),
        });

        let effective_model = crate::workflow::roles::resolve_model(
            None,
            None,
            self.config
                .backends
                .get(&backend_id)
                .and_then(|o| o.model.as_deref()),
        );
        let effective_effort = crate::effort::resolve_effort(
            None,
            None,
            self.config.backends.get(&backend_id).and_then(|o| o.effort),
        )
        .map(|e| e.clamp_for(backend_id));

        let logger = RunLogger::start(RunKind::Rescue, &[backend_id], &self.cwd);
        decision.log(&logger, task, effective_model.as_deref(), effective_effort);
        logger.member_started(
            backend_id,
            false,
            effective_model.as_deref(),
            effective_effort.map(|e| e.as_str()),
        );
        let started = Instant::now();

        // Session recording + continuation planning (§4.3). Lock per operation only.
        let native_ok = supports_native_continuation(backend_id, &self.regs);
        let mut plan = TurnPlan::Fresh;
        let mut exchange_id: Option<String> = None;
        if let Some(recorder) = recorder
            && let Ok(mut rec) = recorder.lock()
        {
            // Plan BEFORE recording this turn's user entry: the composed prompt must carry
            // the PRIOR context only, and a first turn must stay Fresh. (Recording first
            // made every first turn compose itself into its own context.)
            plan = rec.plan_turn(
                backend_id,
                native_ok,
                task,
                self.config.session.compose_window,
            );
            // Journal failures must be SEEN (H: silently ignoring them meant a full disk
            // produced answers that resume knew nothing about, re-running side effects).
            // The turn still proceeds — the answer is worth more than the journal entry —
            // but the user is told the durable story is now incomplete.
            if let Err(e) = rec.record_user(task) {
                on_event(EngineEvent::Notice {
                    text: format!("session journal: user entry not recorded: {e:#}"),
                });
            }
            if let Err(e) =
                rec.record_route("rescue", None, None, backend_id, decision.reason.as_str())
            {
                on_event(EngineEvent::Notice {
                    text: format!("session journal: route entry not recorded: {e:#}"),
                });
            }
            let (prompt_sent, continue_from) = match &plan {
                TurnPlan::Fresh => (task, None),
                TurnPlan::Native { continue_from } => (task, Some(continue_from.as_str())),
                TurnPlan::Composed { prompt } => (prompt.as_str(), None),
            };
            exchange_id = match rec.record_exchange(NewExchange {
                backend: backend_id.as_str(),
                transport,
                run_id: logger.run_id(),
                model: effective_model.as_deref(),
                effort: effective_effort.map(|e| e.as_str()),
                prompt: prompt_sent,
                continue_from,
            }) {
                Ok(id) => Some(id),
                Err(e) => {
                    on_event(EngineEvent::Notice {
                        text: format!(
                            "session journal: exchange not recorded: {e:#} — resume will \
                             not see this turn"
                        ),
                    });
                    None
                }
            };
        }
        let (task_to_send, continue_from): (String, Option<String>) = match &plan {
            TurnPlan::Fresh => (task.to_string(), None),
            TurnPlan::Native { continue_from } => (task.to_string(), Some(continue_from.clone())),
            TurnPlan::Composed { prompt } => (prompt.clone(), None),
        };

        // Tee chunks to the caller's sink and the dashboard capture file.
        let to_file = output_streamer(logger.run_id(), backend_id, false);
        let sink = Arc::clone(&on_event);
        let on_chunk: Arc<dyn Fn(&str) + Send + Sync> = Arc::new(move |c: &str| {
            sink(EngineEvent::Chunk {
                text: c.to_string(),
            });
            to_file(c);
        });

        let result = dispatch_continuing(
            backend_id,
            &task_to_send,
            &self.cwd,
            cancel.clone(),
            on_chunk,
            &self.regs,
            effective_model.as_deref(),
            effective_effort,
            continue_from.as_deref(),
        )
        .await;

        let raw_ref = format!("runs/{}/{}.log", logger.run_id(), backend_id);
        let elapsed_ms = started.elapsed().as_millis() as u64;
        let (status, answer, backend_ref) = match &result {
            // Auth failure: drop the ref (L2). Claude emits a session id even on an auth
            // error, and resuming a session whose only content is that error is useless —
            // the next turn should start fresh once auth is fixed.
            Ok(res) if res.auth_failed => (ExchangeStatus::Auth, res.output.clone(), None),
            Ok(res) => (
                ExchangeStatus::Ok,
                res.output.clone(),
                res.backend_session_ref.clone(),
            ),
            Err(_) if cancel.is_cancelled() => (ExchangeStatus::Cancelled, String::new(), None),
            // A dispatch timeout surfaces as an error string from `with_timeout`; record it
            // as the distinct Timeout status (M5) rather than a generic error, so the
            // Timeout variant and its renderers are reachable.
            Err(e) if format!("{e:#}").contains("timed out") => {
                (ExchangeStatus::Timeout, format!("{e:#}"), None)
            }
            Err(e) => (ExchangeStatus::Error, format!("{e:#}"), None),
        };

        if let (Some(recorder), Some(exchange_id)) = (recorder, &exchange_id)
            && let Ok(mut rec) = recorder.lock()
            && let Err(e) = rec.record_result(
                exchange_id,
                NewResult {
                    status,
                    answer: &answer,
                    exit_code: None,
                    duration_ms: elapsed_ms,
                    backend_session_ref: backend_ref.as_deref(),
                    raw_ref: Some(&raw_ref),
                },
            )
        {
            on_event(EngineEvent::Notice {
                text: format!(
                    "session journal: result not recorded: {e:#} — on resume this turn \
                     will look interrupted and could be repeated"
                ),
            });
        }

        // Run-event bookkeeping mirrors the recorded status.
        let leg = match status {
            ExchangeStatus::Ok => LegStatus::Ok,
            ExchangeStatus::Cancelled => LegStatus::Skipped,
            _ => LegStatus::Error,
        };
        logger.member_finished(
            backend_id,
            false,
            leg,
            elapsed_ms,
            (status == ExchangeStatus::Ok).then_some(answer.len()),
            (leg == LegStatus::Error).then(|| answer.clone()),
        );
        logger.finished(leg);

        TurnOutcome::Completed {
            backend: backend_id,
            status,
            answer,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dispatch::Registries;
    use crate::exec::{ExecAdapter, ExecSpec};
    use std::sync::Mutex;

    /// A fake backend that echoes via `sh` — real process, no LLM, deterministic.
    struct EchoExec;
    impl ExecAdapter for EchoExec {
        fn id(&self) -> BackendId {
            BackendId::Opencode
        }
        fn build_spec(
            &self,
            task: &str,
            _model: Option<&str>,
            _effort: Option<crate::effort::Effort>,
        ) -> ExecSpec {
            ExecSpec {
                command: "sh".into(),
                args: vec![
                    "-c".into(),
                    format!("printf 'echo:%s' {}", shell_quote(task)),
                ],
                env: vec![],
                stdin_input: None,
            }
        }
    }

    fn shell_quote(s: &str) -> String {
        format!("'{}'", s.replace('\'', "'\\''"))
    }

    fn lock_env() -> std::sync::MutexGuard<'static, ()> {
        // XDG_STATE_HOME is process-global; serialize with every other state-dir test.
        crate::ask::STATE_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    fn engine_with_echo(tmp_state: &std::path::Path) -> TurnEngine {
        // Keep telemetry writes inside the test sandbox.
        unsafe { std::env::set_var("XDG_STATE_HOME", tmp_state) };
        let mut regs = Registries::empty();
        regs.execs.insert(BackendId::Opencode, Box::new(EchoExec));
        let mut config = HubConfig::default();
        config.default.backend = BackendId::Opencode;
        TurnEngine {
            config,
            regs: Arc::new(regs),
            cwd: tmp_state.to_path_buf(),
        }
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn turn_dispatches_records_and_reports_chunks() {
        let _env = lock_env();
        let tmp = tempfile::tempdir().unwrap();
        let engine = engine_with_echo(tmp.path());

        let dir = tmp.path().join("sessions");
        let log = agentpit_events::session::SessionLog::create(&dir, "/w", None, None).unwrap();
        let lease = agentpit_events::session_lease::SessionLease::acquire_at(
            &tmp.path().join("leases"),
            log.path(),
        )
        .unwrap();
        let path = log.path().to_path_buf();
        let recorder: SharedRecorder = Arc::new(Mutex::new(
            crate::session::SessionRecorder::from_parts(log, lease),
        ));

        let chunks: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&chunks);
        let outcome = engine
            .run_turn(
                Some(&recorder),
                None,
                Some(BackendId::Opencode),
                "hello engine",
                CancellationToken::new(),
                Arc::new(move |ev| {
                    if let EngineEvent::Chunk { text } = ev {
                        seen.lock().unwrap().push(text);
                    }
                }),
            )
            .await;

        match outcome {
            TurnOutcome::Completed {
                backend,
                status,
                answer,
            } => {
                assert_eq!(backend, BackendId::Opencode);
                assert_eq!(status, ExchangeStatus::Ok);
                assert!(answer.contains("echo:hello engine"), "{answer}");
                // Regression: a FIRST turn must go out as-is, not composed into its own
                // just-recorded context (plan-before-record ordering).
                assert!(
                    !answer.contains("continuing an ongoing session"),
                    "first turn must be Fresh, got: {answer}"
                );
            }
            other => panic!("expected Completed, got {other:?}"),
        }
        assert!(
            chunks
                .lock()
                .unwrap()
                .join("")
                .contains("echo:hello engine"),
            "chunks must stream through the sink"
        );

        // The turn landed in the session file: user → route → exchange → result(ok).
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.contains("\"type\":\"user\""));
        assert!(raw.contains("\"type\":\"exchange\""));
        assert!(raw.contains("\"type\":\"result\""));
        assert!(raw.contains("\"status\":\"ok\""));
        unsafe { std::env::remove_var("XDG_STATE_HOME") };
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn explicit_unavailable_backend_falls_back_via_the_router() {
        // The router only ever returns AVAILABLE backends — an explicit pick that is not
        // registered falls back instead of failing. `TurnOutcome::Unavailable` stays as a
        // defensive arm for a router/registry drift, not a reachable path here.
        let _env = lock_env();
        let tmp = tempfile::tempdir().unwrap();
        let engine = engine_with_echo(tmp.path());
        let outcome = engine
            .run_turn(
                None,
                None,
                Some(BackendId::Claude), // not in the echo-only registry
                "x",
                CancellationToken::new(),
                Arc::new(|_| {}),
            )
            .await;
        match outcome {
            TurnOutcome::Completed {
                backend, status, ..
            } => {
                assert_eq!(
                    backend,
                    BackendId::Opencode,
                    "fell back to the available one"
                );
                assert_eq!(status, ExchangeStatus::Ok);
            }
            other => panic!("expected fallback Completed, got {other:?}"),
        }
        unsafe { std::env::remove_var("XDG_STATE_HOME") };
    }
}
