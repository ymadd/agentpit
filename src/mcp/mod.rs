//! MCP, in both directions.
//!
//! **agentpit as a server** — a stdio MCP server exposing agentpit's dispatch/ensemble
//! machinery as structured tools, so a workflow manager can orchestrate via MCP tool calls
//! instead of shelling out to the `agentpit` binary.
//!
//! - [`tools`] — the three tools (`list_backends`, `dispatch_task`, `run_ensemble`).
//! - [`server`] — wires the tools to the rmcp stdio server and awaits it.
//! - [`serve`] — the `agentpit mcp serve` entry point (loads config, resolves cwd, serves).
//!
//! **agentpit as a client** — other people's MCP servers, whose prompts become slash
//! commands. The split across these four modules is the guarantee that a plain startup
//! spawns nothing:
//!
//! - [`servers`] — where a server *definition* comes from (agentpit's config, the project's
//!   `.mcp.json`). Reads files; spawns nothing.
//! - [`cache`] — the prompt list a refresh wrote down, and the staleness key that says
//!   whether it still applies. Reads/writes one file; spawns nothing.
//! - [`prompts`] — cached prompts as `/<server>:<prompt>` registry rows. Spawns nothing.
//! - [`client`] — the one module that starts a child process, reached only from
//!   `agentpit mcp refresh`.
//! - [`import`] — a one-shot copy of Claude Code's server list into agentpit's own config.

pub mod cache;
pub mod client;
pub mod import;
pub mod prompts;
pub mod serve;
pub mod server;
pub mod servers;
pub mod tools;
