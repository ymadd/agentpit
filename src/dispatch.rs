use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Result, anyhow};
use tokio_util::sync::CancellationToken;

use crate::acp::{AcpAdapter, opencode::OpencodeAdapter};
use crate::config::HubConfig;
use crate::exec::{
    ExecAdapter, ExecRunOptions, antigravity::AntigravityExec, claude::ClaudeExec,
    codex::CodexExec, gemini::GeminiExec,
};
use crate::types::{BackendId, Transport};

const DEFAULT_TRANSPORTS: &[(BackendId, Transport)] = &[
    (BackendId::Gemini, Transport::Exec),
    (BackendId::Antigravity, Transport::Exec),
    (BackendId::Claude, Transport::Exec),
    (BackendId::Codex, Transport::Exec),
    (BackendId::Opencode, Transport::Acp),
];

pub struct Registries {
    pub execs: HashMap<BackendId, Box<dyn ExecAdapter>>,
    pub acps: HashMap<BackendId, Box<dyn AcpAdapter>>,
}

impl Registries {
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
            (BackendId::Gemini, Transport::Exec) => {
                execs.insert(BackendId::Gemini, Box::new(GeminiExec));
            }
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
            // For now ACP transport is wired only for opencode; exec for the other three.
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
}

pub async fn dispatch(
    backend: BackendId,
    task: &str,
    cwd: &Path,
    cancel: CancellationToken,
    on_chunk: Arc<dyn Fn(&str) + Send + Sync>,
    regs: &Registries,
) -> Result<DispatchResult> {
    if let Some(exec) = regs.execs.get(&backend) {
        let options = ExecRunOptions {
            cwd: cwd.to_path_buf(),
            cancel: cancel.clone(),
            on_stdout: Some(on_chunk.clone()),
        };
        let outcome = crate::exec::run(exec.as_ref(), task, options).await?;
        return Ok(DispatchResult {
            backend,
            transport: Transport::Exec,
            output: outcome.output,
        });
    }
    if let Some(acp) = regs.acps.get(&backend) {
        let outcome = crate::acp::run(acp.as_ref(), task, cwd, on_chunk, cancel).await?;
        return Ok(DispatchResult {
            backend,
            transport: Transport::Acp,
            output: outcome.output,
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
            BackendId::Gemini
        }
        fn build_spec(&self, _task: &str) -> ExecSpec {
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
        fn spawn_spec(&self) -> crate::acp::SpawnSpec {
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
        r.execs.insert(BackendId::Gemini, Box::new(DummyExec));
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
        r.execs.insert(BackendId::Gemini, Box::new(DummyExec));
        // Inject an acp for gemini to test preference.
        struct GeminiAcp;
        impl AcpAdapter for GeminiAcp {
            fn id(&self) -> BackendId {
                BackendId::Gemini
            }
            fn spawn_spec(&self) -> crate::acp::SpawnSpec {
                crate::acp::SpawnSpec {
                    command_line: "true".into(),
                }
            }
        }
        r.acps.insert(BackendId::Gemini, Box::new(GeminiAcp));
        r
    }

    #[test]
    fn returns_exec_when_only_exec_registered() {
        let r = regs_exec_only();
        assert_eq!(
            resolve_transport(BackendId::Gemini, &r),
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
            resolve_transport(BackendId::Gemini, &r),
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
}
