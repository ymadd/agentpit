use anyhow::Result;

use crate::effort::Effort;
use crate::types::BackendId;

pub mod antigravity;
pub mod autonomy;
mod base;
pub mod claude;
pub mod codex;
pub mod prime_agent;
mod stream;
pub mod workflow_manager;

pub use autonomy::{AskTier, AutonomyLevel};
pub use base::{ExecOutcome, ExecRunOptions, ExecSpec, run_spec};
pub use stream::StreamFormat;
pub use workflow_manager::{McpConfigGuard, WorkflowManagerExec, is_supported_manager};

/// Trait implemented by exec-mode backends (direct CLI spawn).
pub trait ExecAdapter: Send + Sync {
    fn id(&self) -> BackendId;
    /// Build the spawn spec for `task`. `model` optionally pins the backend's model: `Some(m)`
    /// emits the CLI's model flag (e.g. `--model m`), `None` leaves the CLI on its own default
    /// (no flag — the pre-model behaviour, so an unset model is a zero-diff regression guard).
    /// `effort` is the same contract one rung over: `Some(e)` emits the CLI's reasoning-effort
    /// flag at [`e.clamp_for(id)`](Effort::clamp_for), `None` emits nothing.
    fn build_spec(&self, task: &str, model: Option<&str>, effort: Option<Effort>) -> ExecSpec;

    /// Describe how stdout should be decoded before it reaches callers. Structured backends use
    /// this to expose live text/progress while keeping the collected final answer free of JSONL.
    fn stream_format(&self) -> StreamFormat {
        StreamFormat::Text
    }

    /// The permission posture this backend is spawned with. Centralised in
    /// [`autonomy`] so the security decision is auditable in one place rather than
    /// inferred from each adapter's flags. Defaults to [`AutonomyLevel::FullAutonomy`]
    /// because every real exec backend runs non-interactively.
    fn autonomy(&self) -> AutonomyLevel {
        AutonomyLevel::FullAutonomy
    }

    /// Whether this backend can natively continue a previous session from a
    /// `backend_session_ref`. Callers use this to decide BEFORE dispatch whether to send
    /// the raw task (native resume) or a composed context (design §4.3, Q3: claude/codex).
    fn supports_resume(&self) -> bool {
        false
    }

    /// Build a spec that natively continues the backend session identified by
    /// `backend_ref` (opaque — captured from a prior run's stream). `None` = no native
    /// continuation; the caller falls back to a fresh [`ExecAdapter::build_spec`].
    fn build_continuation_spec(
        &self,
        task: &str,
        model: Option<&str>,
        effort: Option<Effort>,
        backend_ref: &str,
    ) -> Option<ExecSpec> {
        let _ = (task, model, effort, backend_ref);
        None
    }
}

pub async fn run<A: ExecAdapter + ?Sized>(
    adapter: &A,
    task: &str,
    options: ExecRunOptions,
) -> Result<ExecOutcome> {
    let spec = options
        .continue_from
        .as_deref()
        .and_then(|r| {
            adapter.build_continuation_spec(task, options.model.as_deref(), options.effort, r)
        })
        .unwrap_or_else(|| adapter.build_spec(task, options.model.as_deref(), options.effort));
    run_spec(adapter.id(), spec, options, adapter.stream_format()).await
}
