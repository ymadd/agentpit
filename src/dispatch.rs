use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, anyhow};
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

use crate::acp::{AcpAdapter, opencode::OpencodeAdapter};
use crate::auth::is_auth_failure_outcome;
use crate::config::HubConfig;
use crate::exec::{
    ExecAdapter, ExecRunOptions, antigravity::AntigravityExec, claude::ClaudeExec,
    codex::CodexExec, prime_agent::PrimeAgentExec,
};
use crate::types::{BackendId, Transport};

const DEFAULT_TRANSPORTS: &[(BackendId, Transport)] = &[
    (BackendId::Antigravity, Transport::Exec),
    (BackendId::Claude, Transport::Exec),
    (BackendId::Codex, Transport::Exec),
    (BackendId::Opencode, Transport::Acp),
    (BackendId::PrimeAgent, Transport::Exec),
];

pub struct Registries {
    pub execs: HashMap<BackendId, Box<dyn ExecAdapter>>,
    pub acps: HashMap<BackendId, Box<dyn AcpAdapter>>,
}

impl Registries {
    /// An empty registry with no backends wired. Callers insert the adapters they need —
    /// e.g. the workflow's one-off manager-only registry, or tests.
    pub fn empty() -> Self {
        Registries {
            execs: HashMap::new(),
            acps: HashMap::new(),
        }
    }

    pub fn available(&self) -> std::collections::HashSet<BackendId> {
        let mut set = std::collections::HashSet::new();
        for k in self.execs.keys().chain(self.acps.keys()) {
            set.insert(*k);
        }
        set
    }
}

pub fn build_registries(config: &HubConfig) -> Registries {
    let mut execs: HashMap<BackendId, Box<dyn ExecAdapter>> = HashMap::new();
    let mut acps: HashMap<BackendId, Box<dyn AcpAdapter>> = HashMap::new();

    for (backend, default_transport) in DEFAULT_TRANSPORTS {
        let transport = config
            .backends
            .get(backend)
            .and_then(|o| o.transport)
            .unwrap_or(*default_transport);

        match (backend, transport) {
            (BackendId::Antigravity, Transport::Exec) => {
                execs.insert(BackendId::Antigravity, Box::new(AntigravityExec));
            }
            (BackendId::Claude, Transport::Exec) => {
                execs.insert(BackendId::Claude, Box::new(ClaudeExec));
            }
            (BackendId::Codex, Transport::Exec) => {
                execs.insert(BackendId::Codex, Box::new(CodexExec));
            }
            (BackendId::Opencode, Transport::Acp) => {
                acps.insert(BackendId::Opencode, Box::new(OpencodeAdapter));
            }
            (BackendId::PrimeAgent, Transport::Exec) => {
                execs.insert(BackendId::PrimeAgent, Box::new(PrimeAgentExec));
            }
            // For now ACP transport is wired only for opencode; exec for everyone else.
            // prime-agent also speaks ACP (`prime-agent --mode acp`), but its JSON event stream
            // is the mode its docs recommend for batch runs, so that is what is wired here.
            _ => {}
        }
    }

    Registries { execs, acps }
}

pub fn resolve_transport(backend: BackendId, regs: &Registries) -> Option<Transport> {
    if regs.execs.contains_key(&backend) {
        Some(Transport::Exec)
    } else if regs.acps.contains_key(&backend) {
        Some(Transport::Acp)
    } else {
        None
    }
}

pub struct DispatchResult {
    pub backend: BackendId,
    pub transport: Transport,
    pub output: String,
    /// True when the backend's output looks like an auth failure. Detected once here so
    /// callers act on a typed flag instead of each re-running `is_auth_failure`.
    pub auth_failed: bool,
    /// The backend's own session/thread id captured from the stream, for native
    /// continuation via [`dispatch_continuing`]. `None` on Text streams and the ACP path.
    pub backend_session_ref: Option<String>,
}

/// Whether `backend` can natively continue from a `backend_session_ref` — the caller's
/// pre-dispatch signal for choosing the raw task (native) vs a composed context (§4.3).
pub fn supports_native_continuation(backend: BackendId, regs: &Registries) -> bool {
    regs.execs
        .get(&backend)
        .map(|e| e.supports_resume())
        .unwrap_or(false)
}

/// Safety cap on a single backend dispatch. A wedged backend (produces no output and
/// never exits) would otherwise hang a run forever when no human is watching to Ctrl-C.
/// Generous by default — real coding agents can run for many minutes — and overridable
/// via `AGENTPIT_DISPATCH_TIMEOUT_SECS` (set to `0` to disable the cap entirely).
const DEFAULT_DISPATCH_TIMEOUT_SECS: u64 = 1800;

fn dispatch_timeout() -> Option<Duration> {
    let secs = std::env::var("AGENTPIT_DISPATCH_TIMEOUT_SECS")
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(DEFAULT_DISPATCH_TIMEOUT_SECS);
    (secs > 0).then(|| Duration::from_secs(secs))
}

/// Run a backend future under the dispatch timeout. On elapse, cancel only this dispatch's
/// own `child` token (exec children are also `kill_on_drop`, and the token signals the ACP
/// path) and surface a clear timeout error rather than hanging. The caller passes a *child*
/// token so a single member's timeout cannot cancel its concurrent siblings — only the
/// shared parent (Ctrl-C) cancels everyone.
async fn with_timeout<T>(
    backend: BackendId,
    child: &CancellationToken,
    fut: impl std::future::Future<Output = Result<T>>,
) -> Result<T> {
    match dispatch_timeout() {
        None => fut.await,
        Some(d) => match timeout(d, fut).await {
            Ok(res) => res,
            Err(_) => {
                child.cancel();
                Err(anyhow!(
                    "{backend} dispatch timed out after {}s (set AGENTPIT_DISPATCH_TIMEOUT_SECS to adjust)",
                    d.as_secs()
                ))
            }
        },
    }
}

#[allow(clippy::too_many_arguments)]
pub async fn dispatch(
    backend: BackendId,
    task: &str,
    cwd: &Path,
    cancel: CancellationToken,
    on_chunk: Arc<dyn Fn(&str) + Send + Sync>,
    regs: &Registries,
    model: Option<&str>,
    effort: Option<crate::effort::Effort>,
) -> Result<DispatchResult> {
    dispatch_continuing(
        backend, task, cwd, cancel, on_chunk, regs, model, effort, None,
    )
    .await
}

/// [`dispatch`] with native session continuation: `continue_from` is an opaque
/// `backend_session_ref` from a prior result, translated into resume flags by the exec
/// adapter (claude/codex today). Ignored on the ACP path and by adapters without
/// [`ExecAdapter::supports_resume`] — callers pre-compose context in those cases (§4.3).
#[allow(clippy::too_many_arguments)]
pub async fn dispatch_continuing(
    backend: BackendId,
    task: &str,
    cwd: &Path,
    cancel: CancellationToken,
    on_chunk: Arc<dyn Fn(&str) + Send + Sync>,
    regs: &Registries,
    model: Option<&str>,
    effort: Option<crate::effort::Effort>,
    continue_from: Option<&str>,
) -> Result<DispatchResult> {
    // A child of the caller's token: the parent (Ctrl-C) still cancels every member, but a
    // per-member timeout cancels only this child, leaving concurrent siblings untouched.
    let child = cancel.child_token();
    if let Some(exec) = regs.execs.get(&backend) {
        let options = ExecRunOptions {
            cwd: cwd.to_path_buf(),
            cancel: child.clone(),
            on_stdout: Some(on_chunk.clone()),
            model: model.map(str::to_string),
            effort,
            continue_from: continue_from.map(str::to_string),
        };
        let fut = crate::exec::run(exec.as_ref(), task, options);
        let outcome = with_timeout(backend, &child, fut).await?;
        return Ok(DispatchResult {
            backend,
            transport: Transport::Exec,
            auth_failed: is_auth_failure_outcome(
                &outcome.output,
                outcome.exit_code.map(|code| code == 0),
            ),
            output: outcome.output,
            backend_session_ref: outcome.backend_session_ref,
        });
    }
    if let Some(acp) = regs.acps.get(&backend) {
        let fut = crate::acp::run(
            acp.as_ref(),
            task,
            cwd,
            on_chunk,
            child.clone(),
            model,
            effort,
        );
        let outcome = with_timeout(backend, &child, fut).await?;
        return Ok(DispatchResult {
            backend,
            transport: Transport::Acp,
            // An `Ok` here means the ACP `PromptRequest` itself completed — the transport
            // reported success, exactly like an exit-0 exec run. Passing `None` here (as
            // "no exit signal") threw that signal away and left the answer's *text* to
            // decide, so an agent that was merely asked to say "401 Unauthorized" had its
            // answer discarded. Protocol status classifies the run; answer text never does.
            auth_failed: is_auth_failure_outcome(&outcome.output, Some(true)),
            output: outcome.output,
            backend_session_ref: None,
        });
    }
    Err(anyhow!("No transport registered for backend {backend}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::ExecSpec;

    struct DummyExec;
    impl ExecAdapter for DummyExec {
        fn id(&self) -> BackendId {
            BackendId::Opencode
        }
        fn build_spec(
            &self,
            _task: &str,
            _model: Option<&str>,
            _effort: Option<crate::effort::Effort>,
        ) -> ExecSpec {
            ExecSpec {
                command: "true".into(),
                args: vec![],
                env: vec![],
                stdin_input: None,
            }
        }
    }

    struct DummyAcp;
    impl AcpAdapter for DummyAcp {
        fn id(&self) -> BackendId {
            BackendId::Opencode
        }
        fn spawn_spec(
            &self,
            _model: Option<&str>,
            _effort: Option<crate::effort::Effort>,
        ) -> crate::acp::SpawnSpec {
            crate::acp::SpawnSpec {
                command_line: "true".into(),
            }
        }
    }

    fn regs_exec_only() -> Registries {
        let mut r = Registries {
            execs: HashMap::new(),
            acps: HashMap::new(),
        };
        r.execs.insert(BackendId::Opencode, Box::new(DummyExec));
        r
    }

    fn regs_acp_only() -> Registries {
        let mut r = Registries {
            execs: HashMap::new(),
            acps: HashMap::new(),
        };
        r.acps.insert(BackendId::Opencode, Box::new(DummyAcp));
        r
    }

    fn regs_both_for_gemini() -> Registries {
        let mut r = Registries {
            execs: HashMap::new(),
            acps: HashMap::new(),
        };
        r.execs.insert(BackendId::Opencode, Box::new(DummyExec));
        // Inject an acp for gemini to test preference.
        struct GeminiAcp;
        impl AcpAdapter for GeminiAcp {
            fn id(&self) -> BackendId {
                BackendId::Opencode
            }
            fn spawn_spec(
                &self,
                _model: Option<&str>,
                _effort: Option<crate::effort::Effort>,
            ) -> crate::acp::SpawnSpec {
                crate::acp::SpawnSpec {
                    command_line: "true".into(),
                }
            }
        }
        r.acps.insert(BackendId::Opencode, Box::new(GeminiAcp));
        r
    }

    #[test]
    fn returns_exec_when_only_exec_registered() {
        let r = regs_exec_only();
        assert_eq!(
            resolve_transport(BackendId::Opencode, &r),
            Some(Transport::Exec)
        );
    }

    #[test]
    fn returns_acp_when_only_acp_registered() {
        let r = regs_acp_only();
        assert_eq!(
            resolve_transport(BackendId::Opencode, &r),
            Some(Transport::Acp)
        );
    }

    #[test]
    fn prefers_exec_when_both_registered() {
        let r = regs_both_for_gemini();
        assert_eq!(
            resolve_transport(BackendId::Opencode, &r),
            Some(Transport::Exec)
        );
    }

    #[test]
    fn returns_none_when_unregistered() {
        let r = Registries {
            execs: HashMap::new(),
            acps: HashMap::new(),
        };
        assert!(resolve_transport(BackendId::Goose, &r).is_none());
    }

    // The dispatch timeout cancels a *child* token so one member's timeout never cancels
    // its concurrent siblings; only the shared parent (Ctrl-C) cancels everyone. This
    // pins the token semantics the per-member timeout relies on.
    #[test]
    fn child_token_timeout_isolates_siblings() {
        let parent = CancellationToken::new();
        let a = parent.child_token();
        let b = parent.child_token();
        a.cancel(); // simulate member A's timeout
        assert!(a.is_cancelled());
        assert!(!b.is_cancelled(), "sibling must not be cancelled");
        assert!(!parent.is_cancelled(), "parent must not be cancelled");
        parent.cancel(); // simulate Ctrl-C
        assert!(b.is_cancelled(), "parent cancels remaining children");
    }
}
