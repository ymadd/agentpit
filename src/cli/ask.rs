//! `agentpit ask` — the CLI twin of the MCP `ask_human` tool, for codex / shell-out managers
//! that have no MCP channel. Unlike the MCP path, this runs as a normal CLI process (NOT inside
//! `agentpit mcp serve`), so stdout is the legitimate result channel: we print the human's
//! answer — or the `HUMAN_UNAVAILABLE` sentinel — there for the manager to read back.

use anyhow::Result;
use tokio_util::sync::CancellationToken;

use crate::ask::{self, AskKind, AskOutcome, AskRequest};
use crate::cli::install_ctrlc_cancel;
use crate::workflow::guard::ENV_PARENT_RUN_ID;

/// Post a question to the human and print the answer to stdout.
pub async fn run(
    prompt: String,
    options: Vec<String>,
    kind: Option<String>,
    timeout_secs: Option<u64>,
) -> Result<()> {
    // Structural worker-isolation gate: only the manager leg carries AGENTPIT_ASK_ALLOWED equal
    // to its run id. Workers inherit AGENTPIT_PARENT_RUN_ID but NOT the allow token (exec::base
    // strips it on every backend spawn), so a worker that tries `agentpit ask` is denied and
    // gets the safe sentinel rather than reaching the human.
    if !ask_allowed() {
        println!("{}", ask::HUMAN_UNAVAILABLE);
        return Ok(());
    }

    let cancel = CancellationToken::new();
    install_ctrlc_cancel(cancel.clone());

    let req = AskRequest {
        prompt,
        options,
        kind: AskKind::parse_or_default(kind.as_deref()),
        timeout_secs: timeout_secs.unwrap_or(0),
    };
    match ask::ask(req, cancel).await {
        AskOutcome::Answered(answer) => println!("{answer}"),
        AskOutcome::Unavailable => println!("{}", ask::HUMAN_UNAVAILABLE),
    }
    Ok(())
}

/// The manager-only gate: `AGENTPIT_ASK_ALLOWED` must be present and equal to
/// `AGENTPIT_PARENT_RUN_ID`. Any worker — which lacks the allow token — fails this.
fn ask_allowed() -> bool {
    match (
        std::env::var(ask::ENV_ASK_ALLOWED).ok(),
        std::env::var(ENV_PARENT_RUN_ID).ok(),
    ) {
        (Some(token), Some(run_id)) => !token.is_empty() && token == run_id,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ask_allowed_only_when_token_matches_run_id() {
        // Serialize env mutation with the rest of the crate's state-dir tests.
        let _g = crate::ask::STATE_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        // SAFETY: single-threaded under STATE_ENV_LOCK.
        unsafe {
            std::env::remove_var(ask::ENV_ASK_ALLOWED);
            std::env::remove_var(ENV_PARENT_RUN_ID);
        }
        // No env at all → denied.
        assert!(!ask_allowed());

        // A worker inherits only the parent run id, not the allow token → denied.
        unsafe {
            std::env::set_var(ENV_PARENT_RUN_ID, "run-7");
        }
        assert!(!ask_allowed());

        // Token present but mismatched → denied.
        unsafe {
            std::env::set_var(ask::ENV_ASK_ALLOWED, "run-OTHER");
        }
        assert!(!ask_allowed());

        // Token equals the run id (the manager leg) → allowed.
        unsafe {
            std::env::set_var(ask::ENV_ASK_ALLOWED, "run-7");
        }
        assert!(ask_allowed());

        unsafe {
            std::env::remove_var(ask::ENV_ASK_ALLOWED);
            std::env::remove_var(ENV_PARENT_RUN_ID);
        }
    }
}
