//! `agentpit mcp serve` — load config, resolve the working directory, and run the stdio server.

use anyhow::Result;

use crate::cli::{load_context, resolve_cwd};

/// Entry point for `agentpit mcp serve`: build the backend registries from config and serve the
/// MCP tools over stdio in the current working directory.
pub async fn run() -> Result<()> {
    let ctx = load_context()?;
    let cwd = resolve_cwd(None)?;
    let roles = ctx.loaded.config.workflow.roles.clone();
    let config = ctx.loaded.config.clone();
    super::server::run_stdio(ctx.regs, cwd, roles, config).await
}
