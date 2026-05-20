use anyhow::Result;

use crate::types::BackendId;

mod base;
pub mod claude;
pub mod codex;
pub mod gemini;

pub use base::{ExecOutcome, ExecRunOptions, ExecSpec, run_spec};

/// Trait implemented by exec-mode backends (direct CLI spawn).
pub trait ExecAdapter: Send + Sync {
    fn id(&self) -> BackendId;
    fn build_spec(&self, task: &str) -> ExecSpec;
}

pub async fn run<A: ExecAdapter + ?Sized>(
    adapter: &A,
    task: &str,
    options: ExecRunOptions,
) -> Result<ExecOutcome> {
    let spec = adapter.build_spec(task);
    run_spec(adapter.id(), spec, options).await
}
