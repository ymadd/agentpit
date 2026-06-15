//! MCP channel: a stdio MCP server exposing agentpit's dispatch/ensemble machinery as
//! structured tools, so a workflow manager can orchestrate via MCP tool calls instead of
//! shelling out to the `agentpit` binary.
//!
//! - [`tools`] — the three tools (`list_backends`, `dispatch_task`, `run_ensemble`).
//! - [`server`] — wires the tools to the rmcp stdio server and awaits it.
//! - [`serve`] — the `agentpit mcp serve` entry point (loads config, resolves cwd, serves).

pub mod serve;
pub mod server;
pub mod tools;
