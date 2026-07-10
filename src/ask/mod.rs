//! Human back-channel: surface a workflow-manager decision to the supervising human and block
//! for an answer. One [`core::ask`] fn backs both the MCP `ask_human` tool and the `agentpit
//! ask` CLI twin, mirroring how `cli::workflow::run_capture` backs both the CLI and its MCP
//! tool. See [`core`] for the mailbox protocol.

pub mod core;

pub use core::{
    AskKind, AskOutcome, AskRequest, DEFAULT_TIMEOUT_SECS, HUMAN_UNAVAILABLE, MAX_TIMEOUT_SECS, ask,
};

/// Env var that authorizes a process to reach the human via the `agentpit ask` CLI twin. It is
/// set — to the manager's run id — ONLY on the workflow manager leg, and is actively stripped
/// from every backend spawn in [`crate::exec`]'s `run_spec` so workers (which inherit the
/// manager's environment) cannot pass the gate. `agentpit ask` requires it to equal
/// `AGENTPIT_PARENT_RUN_ID`. The MCP `ask_human` path needs no such token: workers there have
/// no MCP channel at all, so isolation is already structural.
pub const ENV_ASK_ALLOWED: &str = "AGENTPIT_ASK_ALLOWED";

/// Crate-wide serialization lock for tests that mutate the process-global `XDG_STATE_HOME`
/// (and therefore share `asks_dir()`). Every async test across the crate that drives the ask
/// mailbox must hold this so two tests never scan the same `asks/` directory concurrently.
#[cfg(test)]
pub(crate) static STATE_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
