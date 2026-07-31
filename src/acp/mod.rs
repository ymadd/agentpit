use std::path::Path;
use std::sync::Arc;

use anyhow::Result;

use crate::effort::Effort;
use crate::types::BackendId;

mod base;
pub mod opencode;

pub use base::{AcpOutcome, SpawnSpec, run_acp_prompt};

/// Trait implemented by ACP-mode backends.
pub trait AcpAdapter: Send + Sync {
    fn id(&self) -> BackendId;
    /// The spawn command line for the ACP agent. `model` optionally pins the model (emitted as a
    /// CLI flag on the agent binary); `None` = the agent's own default (no flag). `effort` is the
    /// same contract for reasoning effort.
    fn spawn_spec(&self, model: Option<&str>, effort: Option<Effort>) -> SpawnSpec;
}

pub async fn run<A: AcpAdapter + ?Sized>(
    adapter: &A,
    task: &str,
    cwd: &Path,
    on_chunk: Arc<dyn Fn(&str) + Send + Sync>,
    cancel: tokio_util::sync::CancellationToken,
    model: Option<&str>,
    effort: Option<Effort>,
) -> Result<AcpOutcome> {
    run_acp_prompt(
        adapter.id(),
        adapter.spawn_spec(model, effort),
        task,
        cwd,
        on_chunk,
        cancel,
    )
    .await
}
