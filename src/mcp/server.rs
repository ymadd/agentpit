//! Wire the agentpit MCP tools to the rmcp stdio transport and run until the client closes.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Result, anyhow};
use rmcp::{ServiceExt, transport::stdio};

use super::tools::AgentpitTools;
use crate::config::{HubConfig, RoleConfig};
use crate::dispatch::Registries;

/// Serve the agentpit MCP tools over stdio, blocking until the peer disconnects.
///
/// The manager spawns `agentpit mcp serve` as an MCP server child; stdin/stdout carry the
/// JSON-RPC framing, so nothing else may be written to stdout while this runs. `roles` is the
/// configured `[workflow.roles.*]` map backing `dispatch_task`'s `role` argument; `config` backs
/// the learned router behind an address-less `dispatch_task`.
pub async fn run_stdio(
    regs: Registries,
    cwd: PathBuf,
    roles: BTreeMap<String, RoleConfig>,
    config: HubConfig,
) -> Result<()> {
    let tools = AgentpitTools::new(Arc::new(regs), cwd)
        .with_roles(roles)
        .with_config(config);
    let service = tools
        .serve(stdio())
        .await
        .map_err(|e| anyhow!("failed to start agentpit MCP stdio server: {e}"))?;
    service
        .waiting()
        .await
        .map_err(|e| anyhow!("agentpit MCP server exited with error: {e}"))?;
    Ok(())
}
