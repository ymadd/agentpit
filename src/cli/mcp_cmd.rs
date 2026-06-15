//! `agentpit mcp <action>` — the MCP channel subcommand.

use anyhow::Result;
use clap::Subcommand;

#[derive(Subcommand, Debug)]
pub enum Action {
    /// Run a stdio MCP server exposing agentpit's dispatch / ensemble tools.
    Serve,
}

pub async fn run(action: Action) -> Result<()> {
    match action {
        Action::Serve => crate::mcp::serve::run().await,
    }
}
