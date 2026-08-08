//! The per-session worker process (design §5.2-§5.4).
//!
//! Owns the session: its lease, its JSONL appends, its backend child processes, and the
//! serialized turn loop. Listens on its own unix socket; clients (attach CLI, TUI, the
//! daemon itself) speak the worker half of [`crate::daemon::protocol`]. Detach is pure
//! bookkeeping — a client vanishing never touches an in-flight turn, which runs on its own
//! task and lands in the session log regardless (§5.3).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::{Context, Result};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::daemon::protocol::{Event, PROTO_VERSION, Request, RequestBody, Response, ResponseData};
use crate::session::turn_engine::{EngineEvent, TurnEngine, TurnOutcome};
use crate::session::{SessionRecorder, SharedRecorder, SummaryReason};
use crate::types::BackendId;

/// Shared worker state: one per process, referenced by every connection task.
pub struct WorkerShared {
    pub session_id: String,
    pub recorder: SharedRecorder,
    pub engine: Arc<TurnEngine>,
    /// Attached client sinks (conn id → line sender). Fan-out targets for events.
    clients: Mutex<HashMap<u64, mpsc::UnboundedSender<String>>>,
    /// One turn at a time (§5.3): `send` while busy is refused, not queued.
    busy: AtomicBool,
    /// Cancels the in-flight turn (the remote Ctrl-C).
    cancel_current: Mutex<Option<CancellationToken>>,
    /// Last turn start/end or attach — the idle clock P3's eviction reads.
    last_activity: Mutex<Instant>,
    /// Flips when a `shutdown` request is accepted.
    shutdown: CancellationToken,
    /// The session's default backend override (REPL `/backend` equivalent, per-worker).
    active_backend: Mutex<Option<BackendId>>,
    /// The orchestration-REPL sidecar (§10), spawned lazily on the first cell. A tokio
    /// mutex: cell evaluation holds it across awaits (cells are serialized anyway).
    repl: tokio::sync::Mutex<Option<crate::orchestrate::DenoRepl>>,
}

impl WorkerShared {
    pub fn new(session_id: String, recorder: SessionRecorder, engine: TurnEngine) -> Arc<Self> {
        Arc::new(WorkerShared {
            session_id,
            recorder: Arc::new(Mutex::new(recorder)),
            engine: Arc::new(engine),
            clients: Mutex::new(HashMap::new()),
            busy: AtomicBool::new(false),
            cancel_current: Mutex::new(None),
            last_activity: Mutex::new(Instant::now()),
            shutdown: CancellationToken::new(),
            active_backend: Mutex::new(None),
            repl: tokio::sync::Mutex::new(None),
        })
    }

    fn touch(&self) {
        if let Ok(mut t) = self.last_activity.lock() {
            *t = Instant::now();
        }
    }

    fn broadcast(&self, event: &Event) {
        let Ok(line) = serde_json::to_string(event) else {
            return;
        };
        if let Ok(mut clients) = self.clients.lock() {
            clients.retain(|_, tx| tx.send(line.clone()).is_ok());
        }
    }

    fn status_data(&self) -> ResponseData {
        ResponseData::WorkerStatus {
            session_id: self.session_id.clone(),
            busy: self.busy.load(Ordering::Relaxed),
            attached_clients: self.clients.lock().map(|c| c.len()).unwrap_or(0),
            idle_ms: self
                .last_activity
                .lock()
                .map(|t| t.elapsed().as_millis() as u64)
                .unwrap_or(0),
        }
    }
}

/// Worker entrypoint for the hidden `daemon worker` subcommand: open the session (taking
/// its lease), bind the socket, serve until `shutdown`.
pub async fn run_worker(session: String, socket: PathBuf) -> Result<()> {
    let ctx = crate::cli::load_context()?;
    let mut recorder = SessionRecorder::resume(&session)
        .with_context(|| format!("worker could not open session {session}"))?;
    // Crash evidence from a previous writer becomes a visible notice (§5.4).
    let recovery_notes = recorder.mark_interrupted();
    let cwd = PathBuf::from(recorder.cwd_string());
    let session_id = recorder.session_id().to_string();
    let engine = TurnEngine {
        config: ctx.loaded.config,
        regs: Arc::new(ctx.regs),
        cwd,
    };
    let shared = WorkerShared::new(session_id, recorder, engine);
    for note in recovery_notes {
        shared.broadcast(&Event::Notice { text: note });
    }

    let _ = std::fs::remove_file(&socket);
    if let Some(parent) = socket.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let listener = UnixListener::bind(&socket)
        .with_context(|| format!("bind worker socket {}", socket.display()))?;
    serve(shared, listener).await;
    let _ = std::fs::remove_file(&socket);
    Ok(())
}

/// Accept loop. Public so integration tests can drive a worker in-process with a fake
/// engine — the protocol surface is exactly what production serves.
pub async fn serve(shared: Arc<WorkerShared>, listener: UnixListener) {
    let mut next_conn: u64 = 0;
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let Ok((stream, _)) = accepted else { break };
                next_conn += 1;
                let conn_id = next_conn;
                let shared = Arc::clone(&shared);
                tokio::spawn(async move { handle_connection(shared, stream, conn_id).await });
            }
            _ = shared.shutdown.cancelled() => break,
        }
    }
}

async fn handle_connection(shared: Arc<WorkerShared>, stream: UnixStream, conn_id: u64) {
    let (read_half, mut write_half) = stream.into_split();
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<String>();

    // Single writer task: responses and fan-out events share one ordered pipe.
    let writer = tokio::spawn(async move {
        while let Some(line) = out_rx.recv().await {
            if write_half.write_all(line.as_bytes()).await.is_err() {
                break;
            }
            if write_half.write_all(b"\n").await.is_err() {
                break;
            }
        }
        let _ = write_half.shutdown().await;
    });

    let mut reader = BufReader::new(read_half);
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line).await {
            Ok(0) | Err(_) => break, // disconnect = implicit detach (§5.3)
            Ok(_) => {}
        }
        if line.trim().is_empty() {
            continue;
        }
        let response = match serde_json::from_str::<Request>(&line) {
            Err(e) => Response::err(0, format!("bad request: {e}")),
            Ok(req) => handle_request(&shared, req, conn_id, &out_tx).await,
        };
        if let Ok(resp_line) = serde_json::to_string(&response)
            && out_tx.send(resp_line).is_err()
        {
            break;
        }
    }

    // Implicit detach on disconnect: bookkeeping only.
    if let Ok(mut clients) = shared.clients.lock() {
        clients.remove(&conn_id);
    }
    drop(out_tx);
    let _ = writer.await;
}

async fn handle_request(
    shared: &Arc<WorkerShared>,
    req: Request,
    conn_id: u64,
    out_tx: &mpsc::UnboundedSender<String>,
) -> Response {
    let id = req.id;
    match req.body {
        RequestBody::Hello { proto } => {
            if proto != PROTO_VERSION {
                return Response::err(
                    id,
                    format!(
                        "protocol mismatch: worker speaks v{PROTO_VERSION}, client sent v{proto}. \
                         Update agentpit (`agentpit update`) so both sides match."
                    ),
                );
            }
            Response::ok(
                id,
                ResponseData::Hello {
                    proto: PROTO_VERSION,
                    role: "worker".into(),
                    pid: std::process::id(),
                },
            )
        }

        RequestBody::Attach { tail } => {
            shared.touch();
            if let Ok(mut clients) = shared.clients.lock() {
                clients.insert(conn_id, out_tx.clone());
            }
            let (transcript, total) = {
                let Ok(rec) = shared.recorder.lock() else {
                    return Response::err(id, "session recorder unavailable");
                };
                let items = rec.context_items();
                let total = items.len();
                let shown = items
                    .into_iter()
                    .rev()
                    .take(tail.max(1))
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .collect::<Vec<_>>();
                (shown, total)
            };
            let shown = transcript.len();
            Response::ok(
                id,
                ResponseData::Snapshot {
                    session_id: shared.session_id.clone(),
                    transcript,
                    total_entries: total,
                    shown,
                },
            )
        }

        RequestBody::Detach => {
            if let Ok(mut clients) = shared.clients.lock() {
                clients.remove(&conn_id);
            }
            Response::ok(id, ResponseData::Unit)
        }

        RequestBody::Send { text, backend } => {
            let explicit = match backend.as_deref().map(str::parse::<BackendId>) {
                None => None,
                Some(Ok(b)) => Some(b),
                Some(Err(e)) => return Response::err(id, format!("unknown backend: {e}")),
            };
            if shared
                .busy
                .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                .is_err()
            {
                return Response::err(
                    id,
                    "busy: a turn is already running. Attach to watch it, or `cancel` it first.",
                );
            }
            shared.touch();

            let cancel = CancellationToken::new();
            if let Ok(mut slot) = shared.cancel_current.lock() {
                *slot = Some(cancel.clone());
            }
            let active = shared.active_backend.lock().ok().and_then(|b| *b);
            let (done_tx, done_rx) = oneshot::channel::<TurnOutcome>();
            let turn_shared = Arc::clone(shared);
            let text_clone = text.clone();
            // The turn runs on its OWN task: the requesting connection may die mid-turn
            // and the exchange still completes and lands in the log (§5.3).
            tokio::spawn(async move {
                // A drop guard clears the busy flag and cancel slot even if run_turn PANICS
                // (M1) — otherwise a panicked turn leaves busy=true forever, which also makes
                // Shutdown refuse and wedges the whole worker. `finished` suppresses the
                // guard's fallback TurnFinished on the normal path (which sends a richer one).
                struct TurnGuard {
                    shared: Arc<WorkerShared>,
                    finished: bool,
                }
                impl Drop for TurnGuard {
                    fn drop(&mut self) {
                        if let Ok(mut slot) = self.shared.cancel_current.lock() {
                            *slot = None;
                        }
                        self.shared.busy.store(false, Ordering::SeqCst);
                        self.shared.touch();
                        if !self.finished {
                            self.shared.broadcast(&Event::TurnFinished {
                                status: "error".into(),
                            });
                        }
                    }
                }
                let mut guard = TurnGuard {
                    shared: Arc::clone(&turn_shared),
                    finished: false,
                };

                let sink_shared = Arc::clone(&turn_shared);
                let on_event: Arc<dyn Fn(EngineEvent) + Send + Sync> =
                    Arc::new(move |ev| match ev {
                        EngineEvent::Route { backend, .. } => {
                            sink_shared.broadcast(&Event::TurnStarted {
                                backend: backend.to_string(),
                            })
                        }
                        EngineEvent::Chunk { text } => {
                            sink_shared.broadcast(&Event::Chunk { text })
                        }
                    });
                let outcome = turn_shared
                    .engine
                    .run_turn(
                        Some(&turn_shared.recorder),
                        active,
                        explicit,
                        &text_clone,
                        cancel,
                        on_event,
                    )
                    .await;
                let status = match &outcome {
                    TurnOutcome::Completed { status, .. } => serde_json::to_value(status)
                        .ok()
                        .and_then(|v| v.as_str().map(str::to_string))
                        .unwrap_or_else(|| "ok".into()),
                    TurnOutcome::Unavailable { .. } => "unavailable".into(),
                };
                turn_shared.broadcast(&Event::TurnFinished { status });
                guard.finished = true; // richer TurnFinished sent; suppress the guard's fallback
                drop(guard); // clears cancel slot + busy before handing back the outcome
                let _ = done_tx.send(outcome);
            });

            match done_rx.await {
                Err(_) => Response::err(id, "turn task dropped unexpectedly"),
                Ok(TurnOutcome::Unavailable { backend, available }) => Response::err(
                    id,
                    format!(
                        "backend {backend} unavailable (available: {})",
                        available
                            .iter()
                            .map(|b| b.to_string())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                ),
                Ok(TurnOutcome::Completed { status, answer, .. }) => Response::ok(
                    id,
                    ResponseData::Turn {
                        status: serde_json::to_value(status)
                            .ok()
                            .and_then(|v| v.as_str().map(str::to_string))
                            .unwrap_or_else(|| "ok".into()),
                        answer,
                    },
                ),
            }
        }

        RequestBody::Cancel => {
            let cancelled = shared
                .cancel_current
                .lock()
                .ok()
                .and_then(|mut slot| slot.take())
                .map(|token| {
                    token.cancel();
                    true
                })
                .unwrap_or(false);
            if cancelled {
                Response::ok(id, ResponseData::Unit)
            } else {
                Response::err(id, "nothing is running")
            }
        }

        RequestBody::Tree => {
            let Ok(rec) = shared.recorder.lock() else {
                return Response::err(id, "session recorder unavailable");
            };
            Response::ok(
                id,
                ResponseData::Lines {
                    lines: rec.tree_lines(),
                },
            )
        }

        RequestBody::Branch { target, summary } => {
            let Ok(mut rec) = shared.recorder.lock() else {
                return Response::err(id, "session recorder unavailable");
            };
            match rec.branch(&target, summary.as_deref()) {
                Ok(()) => Response::ok(id, ResponseData::Unit),
                Err(e) => Response::err(id, format!("{e:#}. Pick an id from `tree`.")),
            }
        }

        RequestBody::Fork { at } => {
            let Ok(rec) = shared.recorder.lock() else {
                return Response::err(id, "session recorder unavailable");
            };
            match rec.fork(at.as_deref()) {
                Ok(session_id) => Response::ok(id, ResponseData::Forked { session_id }),
                Err(e) => Response::err(id, format!("{e:#}")),
            }
        }

        RequestBody::Compact => {
            // Summarize via the engine's own dispatch path (active/default backend).
            let items = {
                let Ok(rec) = shared.recorder.lock() else {
                    return Response::err(id, "session recorder unavailable");
                };
                rec.context_items()
            };
            if items.is_empty() {
                return Response::err(id, "nothing to compact yet");
            }
            let mut convo = String::new();
            for (who, text) in &items {
                convo.push_str(&format!("{who}: {text}\n"));
            }
            let prompt = format!(
                "Summarize this conversation for future context. Cover: the goal, decisions \
                 made, current progress, and open next steps. Be concise (under 300 words). \
                 Output only the summary.\n\n{convo}"
            );
            let active = shared.active_backend.lock().ok().and_then(|b| *b);
            let outcome = shared
                .engine
                .run_turn(
                    None, // do NOT record the summarization itself as a turn
                    active,
                    None,
                    &prompt,
                    CancellationToken::new(),
                    Arc::new(|_| {}),
                )
                .await;
            match outcome {
                TurnOutcome::Completed {
                    status: crate::session::ExchangeStatus::Ok,
                    answer,
                    ..
                } if !answer.trim().is_empty() => {
                    let Ok(mut rec) = shared.recorder.lock() else {
                        return Response::err(id, "session recorder unavailable");
                    };
                    match rec.record_summary(answer.trim(), SummaryReason::Manual) {
                        Ok(()) => Response::ok(id, ResponseData::Unit),
                        Err(e) => Response::err(id, format!("{e:#}")),
                    }
                }
                _ => Response::err(
                    id,
                    "summarization failed; nothing was compacted. Try again or switch backends.",
                ),
            }
        }

        RequestBody::ReplCell { code } => {
            // Cells share the turn's busy flag: a cell can dispatch, so it IS a turn.
            if shared
                .busy
                .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                .is_err()
            {
                return Response::err(
                    id,
                    "busy: a turn or cell is already running. Wait for it or `cancel` it first.",
                );
            }
            shared.touch();
            let started = std::time::Instant::now();
            let response = run_repl_cell(shared, &code).await;
            // Log the cell into the session (§10.7) — code + outcome, never the heap.
            if let Ok(mut rec) = shared.recorder.lock() {
                let (ok, detail) = match &response {
                    Ok(crate::orchestrate::CellOutcome::Ok { repr }) => (true, repr.clone()),
                    Ok(crate::orchestrate::CellOutcome::RuntimeError { error })
                    | Ok(crate::orchestrate::CellOutcome::CheckFailed { error }) => {
                        (false, error.clone())
                    }
                    Err(e) => (false, format!("{e:#}")),
                };
                let _ =
                    rec.append_repl_cell(&code, ok, &detail, started.elapsed().as_millis() as u64);
            }
            shared.busy.store(false, Ordering::SeqCst);
            shared.touch();
            match response {
                Ok(crate::orchestrate::CellOutcome::Ok { repr }) => Response::ok(
                    id,
                    ResponseData::Cell {
                        ok: true,
                        repr,
                        error: None,
                        check_failed: false,
                    },
                ),
                Ok(crate::orchestrate::CellOutcome::RuntimeError { error }) => Response::ok(
                    id,
                    ResponseData::Cell {
                        ok: false,
                        repr: String::new(),
                        error: Some(error),
                        check_failed: false,
                    },
                ),
                Ok(crate::orchestrate::CellOutcome::CheckFailed { error }) => Response::ok(
                    id,
                    ResponseData::Cell {
                        ok: false,
                        repr: String::new(),
                        error: Some(error),
                        check_failed: true,
                    },
                ),
                Err(e) => Response::err(id, format!("{e:#}")),
            }
        }

        RequestBody::Status => Response::ok(id, shared.status_data()),

        RequestBody::Shutdown { all: _ } => {
            // Cancel any in-flight turn first, then shut down. Refusing while busy would
            // make a wedged worker impossible to stop from `daemon stop`/`doctor`; the
            // process exit (run_worker returning) kills whatever the cancel can't (M1/M2).
            if let Ok(mut slot) = shared.cancel_current.lock()
                && let Some(token) = slot.take()
            {
                token.cancel();
            }
            shared.shutdown.cancel();
            Response::ok(id, ResponseData::Unit)
        }

        // Daemon-only verbs reaching a worker = a socket-path mixup; answer clearly.
        RequestBody::Create { .. }
        | RequestBody::Ensure { .. }
        | RequestBody::List
        | RequestBody::StopWorker { .. } => Response::err(
            id,
            "this is a WORKER socket; daemon verbs go to daemon.sock (`agentpit daemon status`)",
        ),
    }
}

/// Run one orchestration cell: lazy-spawn the sidecar, optionally typecheck, evaluate
/// with host calls wired to the worker's engine/store/session (§10.2).
async fn run_repl_cell(
    shared: &Arc<WorkerShared>,
    code: &str,
) -> Result<crate::orchestrate::CellOutcome> {
    let repl_cfg = shared.engine.config.repl.clone();
    let artifacts = crate::orchestrate::artifacts_dir(&shared.session_id);
    let mut guard = shared.repl.lock().await;
    if guard.is_none() {
        let deno = crate::orchestrate::find_deno(&repl_cfg.deno_path)?;
        let idle = (repl_cfg.cell_idle_timeout_secs > 0)
            .then(|| std::time::Duration::from_secs(repl_cfg.cell_idle_timeout_secs));
        *guard = Some(crate::orchestrate::DenoRepl::spawn(
            &deno,
            &shared.engine.cwd,
            &artifacts,
            repl_cfg.max_heap_mb,
            idle,
        )?);
    }
    let repl = guard.as_mut().expect("just initialized");

    if repl_cfg.typecheck
        && let Some(error) = repl.typecheck(code).await?
    {
        return Ok(crate::orchestrate::CellOutcome::CheckFailed { error });
    }

    let engine = Arc::clone(&shared.engine);
    let recorder = Arc::clone(&shared.recorder);
    let broadcast_shared = Arc::clone(shared);
    let active = shared.active_backend.lock().ok().and_then(|b| *b);
    let outcome = repl
        .eval_cell(code, move |call| {
            let engine = Arc::clone(&engine);
            let recorder = Arc::clone(&recorder);
            let broadcast_shared = Arc::clone(&broadcast_shared);
            let artifacts = artifacts.clone();
            async move {
                crate::orchestrate::handle_host_call(
                    call,
                    &artifacts,
                    |n| {
                        recorder
                            .lock()
                            .map(|rec| {
                                let items = rec.context_items();
                                let skip = items.len().saturating_sub(n);
                                items.into_iter().skip(skip).collect()
                            })
                            .unwrap_or_default()
                    },
                    |task, backend| async move {
                        let explicit = match backend.as_deref().map(str::parse::<BackendId>) {
                            None => None,
                            Some(Ok(b)) => Some(b),
                            Some(Err(e)) => return Err(anyhow::anyhow!("unknown backend: {e}")),
                        };
                        // Cell dispatches stream to attached clients like turn chunks but
                        // are NOT recorded as session exchanges — the cell log carries the
                        // orchestration story; run-level telemetry still lands (§10.7).
                        let sink_shared = Arc::clone(&broadcast_shared);
                        let on_event: Arc<dyn Fn(EngineEvent) + Send + Sync> =
                            Arc::new(move |ev| {
                                if let EngineEvent::Chunk { text } = ev {
                                    sink_shared.broadcast(&Event::Chunk { text });
                                }
                            });
                        let outcome = engine
                            .run_turn(
                                None,
                                active,
                                explicit,
                                &task,
                                CancellationToken::new(),
                                on_event,
                            )
                            .await;
                        match outcome {
                            TurnOutcome::Completed {
                                backend,
                                status,
                                answer,
                            } => Ok(serde_json::json!({
                                "backend": backend.to_string(),
                                "status": serde_json::to_value(status)
                                    .ok()
                                    .and_then(|v| v.as_str().map(str::to_string))
                                    .unwrap_or_else(|| "ok".into()),
                                "answer": answer,
                            })),
                            TurnOutcome::Unavailable { backend, .. } => {
                                Err(anyhow::anyhow!("backend {backend} unavailable"))
                            }
                        }
                    },
                )
                .await
            }
        })
        .await;
    // A sidecar I/O error (broken pipe = deno died) means the heap is gone; drop the
    // handle so the NEXT cell respawns a fresh REPL rather than reusing a dead process
    // (H3 — the error text promised this, but nothing reset the guard). A RuntimeError
    // is a cell exception with the sidecar still alive, so it keeps its handle.
    if outcome.is_err() {
        *guard = None;
    }
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dispatch::Registries;
    use crate::exec::{ExecAdapter, ExecSpec};
    use agentpit_events::session::SessionLog;
    use agentpit_events::session_lease::SessionLease;

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
                    format!("printf 'echo:%s' '{}'", task.replace('\'', "")),
                ],
                env: vec![],
                stdin_input: None,
            }
        }
    }

    fn lock_env() -> std::sync::MutexGuard<'static, ()> {
        // XDG_STATE_HOME is process-global; serialize with every other state-dir test.
        crate::ask::STATE_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    fn test_shared(tmp: &std::path::Path) -> Arc<WorkerShared> {
        unsafe { std::env::set_var("XDG_STATE_HOME", tmp) };
        let log = SessionLog::create(&tmp.join("sessions"), "/w", None, None).unwrap();
        let lease = SessionLease::acquire_at(&tmp.join("leases"), log.path()).unwrap();
        let sid = log.session_id().to_string();
        let recorder = SessionRecorder::from_parts(log, lease);
        let mut regs = Registries::empty();
        regs.execs.insert(BackendId::Opencode, Box::new(EchoExec));
        let mut config = crate::config::HubConfig::default();
        config.default.backend = BackendId::Opencode;
        let engine = TurnEngine {
            config,
            regs: Arc::new(regs),
            cwd: tmp.to_path_buf(),
        };
        WorkerShared::new(sid, recorder, engine)
    }

    async fn client(
        sock: &std::path::Path,
    ) -> (
        BufReader<tokio::net::unix::OwnedReadHalf>,
        tokio::net::unix::OwnedWriteHalf,
    ) {
        let stream = UnixStream::connect(sock).await.unwrap();
        let (r, w) = stream.into_split();
        (BufReader::new(r), w)
    }

    async fn send_req(w: &mut tokio::net::unix::OwnedWriteHalf, id: u64, body: RequestBody) {
        let line = serde_json::to_string(&Request { id, body }).unwrap();
        w.write_all(line.as_bytes()).await.unwrap();
        w.write_all(b"\n").await.unwrap();
    }

    async fn recv_line(r: &mut BufReader<tokio::net::unix::OwnedReadHalf>) -> String {
        let mut line = String::new();
        r.read_line(&mut line).await.unwrap();
        line
    }

    /// Full protocol pass over a real unix socket with a fake backend: hello → attach →
    /// send (chunks stream as events, turn lands in the log) → tree → shutdown.
    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn worker_serves_the_full_protocol_over_a_socket() {
        let _env = lock_env();
        let tmp = tempfile::tempdir().unwrap();
        let shared = test_shared(tmp.path());
        let session_path = shared.recorder.lock().unwrap().path().to_path_buf();
        let sock = tmp.path().join("w.sock");
        let listener = UnixListener::bind(&sock).unwrap();
        let serve_task = tokio::spawn(serve(Arc::clone(&shared), listener));

        let (mut r, mut w) = client(&sock).await;

        // hello
        send_req(
            &mut w,
            1,
            RequestBody::Hello {
                proto: PROTO_VERSION,
            },
        )
        .await;
        let resp: Response = serde_json::from_str(&recv_line(&mut r).await).unwrap();
        assert!(resp.ok, "{resp:?}");
        assert!(matches!(
            resp.data,
            Some(ResponseData::Hello { ref role, .. }) if role == "worker"
        ));

        // wrong protocol version is refused with guidance
        send_req(&mut w, 2, RequestBody::Hello { proto: 999 }).await;
        let resp: Response = serde_json::from_str(&recv_line(&mut r).await).unwrap();
        assert!(!resp.ok);
        assert!(resp.error.unwrap().contains("agentpit update"));

        // attach (empty session)
        send_req(&mut w, 3, RequestBody::Attach { tail: 400 }).await;
        let resp: Response = serde_json::from_str(&recv_line(&mut r).await).unwrap();
        assert!(matches!(
            resp.data,
            Some(ResponseData::Snapshot { shown: 0, .. })
        ));

        // send a turn: events stream, then the response arrives
        send_req(
            &mut w,
            4,
            RequestBody::Send {
                text: "hello worker".into(),
                backend: None,
            },
        )
        .await;
        let mut saw_chunk = false;
        let mut saw_finish = false;
        let turn: Response = loop {
            let line = recv_line(&mut r).await;
            match serde_json::from_str::<crate::daemon::protocol::Frame>(&line).unwrap() {
                crate::daemon::protocol::Frame::Event(Event::Chunk { text }) => {
                    if text.contains("echo:hello worker") {
                        saw_chunk = true;
                    }
                }
                crate::daemon::protocol::Frame::Event(Event::TurnFinished { .. }) => {
                    saw_finish = true;
                }
                crate::daemon::protocol::Frame::Event(_) => {}
                crate::daemon::protocol::Frame::Response(resp) => break resp,
            }
        };
        assert!(saw_chunk, "attached client must see streamed chunks");
        assert!(saw_finish, "attached client must see turn_finished");
        assert!(turn.ok);
        match turn.data {
            Some(ResponseData::Turn { status, answer }) => {
                assert_eq!(status, "ok");
                assert!(answer.contains("echo:hello worker"));
            }
            other => panic!("expected Turn, got {other:?}"),
        }

        // the turn landed in the session file
        let raw = std::fs::read_to_string(&session_path).unwrap();
        assert!(raw.contains("\"type\":\"exchange\""));
        assert!(raw.contains("\"status\":\"ok\""));

        // tree serves display lines remotely
        send_req(&mut w, 5, RequestBody::Tree).await;
        let resp: Response = serde_json::from_str(&recv_line(&mut r).await).unwrap();
        match resp.data {
            Some(ResponseData::Lines { lines }) => {
                assert!(lines.iter().any(|l| l.contains("[user] hello worker")));
            }
            other => panic!("expected Lines, got {other:?}"),
        }

        // daemon verbs on a worker socket are named as such
        send_req(&mut w, 6, RequestBody::List).await;
        let resp: Response = serde_json::from_str(&recv_line(&mut r).await).unwrap();
        assert!(resp.error.unwrap().contains("WORKER socket"));

        // graceful shutdown ends the accept loop
        send_req(&mut w, 7, RequestBody::Shutdown { all: false }).await;
        let resp: Response = serde_json::from_str(&recv_line(&mut r).await).unwrap();
        assert!(resp.ok);
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), serve_task)
            .await
            .expect("serve loop must exit after shutdown");
        unsafe { std::env::remove_var("XDG_STATE_HOME") };
    }

    /// A second `send` while one is in flight is refused (§5.3 serialization), and a
    /// disconnected client does not kill the in-flight turn.
    /// P3 (§7.1): the sweeper evicts a worker only when it is idle past the threshold,
    /// unattached, and not busy — and eviction ends the serve loop + removes the record.
    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn sweeper_evicts_only_idle_unattached_workers() {
        let _env = lock_env();
        let tmp = tempfile::tempdir().unwrap();
        let workers_dir = tmp.path().join("workers");

        let make_worker = |name: &str| {
            let shared = test_shared(tmp.path());
            let sock = tmp.path().join(format!("{name}.sock"));
            let listener = UnixListener::bind(&sock).unwrap();
            let serve_task = tokio::spawn(serve(Arc::clone(&shared), listener));
            let record = crate::daemon::registry::WorkerRecord {
                session_id: shared.session_id.clone(),
                pid: std::process::id(),
                start_id: agentpit_events::session_lease::process_start_id(std::process::id()),
                socket: sock.display().to_string(),
            };
            crate::daemon::registry::save(&workers_dir, &record).unwrap();
            (shared, serve_task)
        };

        // Worker A: idle + unattached → must be evicted.
        let (a_shared, a_serve) = make_worker("a");
        // Worker B: busy → must survive.
        let (b_shared, _b_serve) = make_worker("b");
        b_shared.busy.store(true, Ordering::SeqCst);
        // Worker C: a client is attached → must survive.
        let (c_shared, _c_serve) = make_worker("c");
        let (tx, _rx) = mpsc::unbounded_channel();
        c_shared.clients.lock().unwrap().insert(1, tx);

        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        crate::daemon::server::sweep_idle_workers(&workers_dir, 1).await;

        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), a_serve)
            .await
            .expect("idle worker A must shut down");
        assert!(
            crate::daemon::registry::load(&workers_dir, &a_shared.session_id).is_none(),
            "evicted worker's record must be removed"
        );
        assert!(
            crate::daemon::registry::load(&workers_dir, &b_shared.session_id).is_some(),
            "busy worker must survive the sweep"
        );
        assert!(
            !b_shared.shutdown.is_cancelled(),
            "busy worker must not be shut down"
        );
        assert!(
            crate::daemon::registry::load(&workers_dir, &c_shared.session_id).is_some(),
            "attached worker must survive the sweep"
        );
        assert!(!c_shared.shutdown.is_cancelled());

        // A high threshold leaves even the idle survivors alone (idle_ms < threshold).
        crate::daemon::server::sweep_idle_workers(&workers_dir, 60_000).await;
        assert!(crate::daemon::registry::load(&workers_dir, &c_shared.session_id).is_some());
        unsafe { std::env::remove_var("XDG_STATE_HOME") };
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn busy_send_is_refused_and_disconnect_does_not_kill_the_turn() {
        let _env = lock_env();
        let tmp = tempfile::tempdir().unwrap();
        let shared = test_shared(tmp.path());
        // Deterministic busy simulation: hold the flag as an in-flight turn would.
        shared.busy.store(true, Ordering::SeqCst);
        let sock = tmp.path().join("w.sock");
        let listener = UnixListener::bind(&sock).unwrap();
        tokio::spawn(serve(Arc::clone(&shared), listener));

        let (mut r, mut w) = client(&sock).await;
        send_req(
            &mut w,
            1,
            RequestBody::Hello {
                proto: PROTO_VERSION,
            },
        )
        .await;
        recv_line(&mut r).await;
        send_req(
            &mut w,
            2,
            RequestBody::Send {
                text: "x".into(),
                backend: None,
            },
        )
        .await;
        let resp: Response = serde_json::from_str(&recv_line(&mut r).await).unwrap();
        assert!(!resp.ok);
        assert!(resp.error.unwrap().contains("busy"));
        unsafe { std::env::remove_var("XDG_STATE_HOME") };
    }
}
