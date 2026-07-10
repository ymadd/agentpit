use anyhow::Result;

use crate::types::BackendId;

pub mod antigravity;
pub mod autonomy;
mod base;
pub mod claude;
pub mod codex;
pub mod gemini;
pub mod workflow_manager;

pub use autonomy::{AskTier, AutonomyLevel};
pub use base::{ExecOutcome, ExecRunOptions, ExecSpec, run_spec};
pub use workflow_manager::{McpConfigGuard, WorkflowManagerExec, is_supported_manager};

/// Trait implemented by exec-mode backends (direct CLI spawn).
pub trait ExecAdapter: Send + Sync {
    fn id(&self) -> BackendId;
    fn build_spec(&self, task: &str) -> ExecSpec;

    /// The permission posture this backend is spawned with. Centralised in
    /// [`autonomy`] so the security decision is auditable in one place rather than
    /// inferred from each adapter's flags. Defaults to [`AutonomyLevel::FullAutonomy`]
    /// because every real exec backend runs non-interactively.
    fn autonomy(&self) -> AutonomyLevel {
        AutonomyLevel::FullAutonomy
    }
}

pub async fn run<A: ExecAdapter + ?Sized>(
    adapter: &A,
    task: &str,
    options: ExecRunOptions,
) -> Result<ExecOutcome> {
    let spec = adapter.build_spec(task);
    run_spec(adapter.id(), spec, options).await
}
