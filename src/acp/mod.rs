use std::path::Path;
use std::sync::Arc;

use anyhow::Result;

use crate::types::BackendId;

mod base;
pub mod opencode;

pub use base::{AcpOutcome, SpawnSpec, run_acp_prompt};

/// Trait implemented by ACP-mode backends.
pub trait AcpAdapter: Send + Sync {
    fn id(&self) -> BackendId;
    fn spawn_spec(&self) -> SpawnSpec;
}

pub async fn run<A: AcpAdapter + ?Sized>(
    adapter: &A,
    task: &str,
    cwd: &Path,
    on_chunk: Arc<dyn Fn(&str) + Send + Sync>,
    cancel: tokio_util::sync::CancellationToken,
) -> Result<AcpOutcome> {
    run_acp_prompt(
        adapter.id(),
        adapter.spawn_spec(),
        task,
        cwd,
        on_chunk,
        cancel,
    )
    .await
}
