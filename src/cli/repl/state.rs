use std::path::PathBuf;
use std::sync::Arc;

use crate::config::HubConfig;
use crate::dispatch::Registries;
use crate::types::BackendId;

/// Immutable snapshot of session parameters. Replaced (not mutated) on each
/// /backend or /cwd command.
#[derive(Clone)]
pub struct SessionState {
    /// Loaded config (Clone; cheap to store directly).
    pub config: HubConfig,
    /// Backend registries (NOT Clone; shared via Arc).
    pub regs: Arc<Registries>,
    /// Active backend override. None = use router default.
    pub active_backend: Option<BackendId>,
    /// Session working directory.
    pub cwd: PathBuf,
    /// rustyline history file path.
    pub history_file: PathBuf,
    /// The durable session log (always recording, Q2). `None` only when creation failed at
    /// startup — the REPL still works, it just doesn't persist (warned once).
    pub recorder: Option<crate::session::SharedRecorder>,
}

impl SessionState {
    /// Construct from a freshly loaded context. Called ONCE at session start.
    pub fn new(loaded: crate::config::LoadedConfig, regs: Registries, cwd: PathBuf) -> Self {
        let history_file = crate::events::state_dir().join("repl_history");
        SessionState {
            config: loaded.config,
            regs: Arc::new(regs),
            active_backend: None,
            cwd,
            history_file,
            recorder: None,
        }
    }

    /// Return new state with a different active backend (immutable replacement).
    pub fn with_backend(self, backend: Option<BackendId>) -> Self {
        SessionState {
            active_backend: backend,
            ..self
        }
    }

    /// Return new state with a different cwd (immutable replacement).
    pub fn with_cwd(self, cwd: PathBuf) -> Self {
        SessionState { cwd, ..self }
    }
}
