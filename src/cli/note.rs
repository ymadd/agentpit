//! `agentpit note` — append a durable conversation-layer Note (① handoff / ③ shared board) to the
//! run transcript. The CLI twin of the MCP `post_note` tool, for codex / shell-out managers that
//! have no MCP channel. Like `agentpit ask`, it is **manager-gated**: a worker that lacks the
//! allow token is a silent no-op, so the shared transcript can only be written by the supervising
//! manager leg. There is no human in the loop and nothing to wait for — the note is fire-and-forget
//! onto the same best-effort `events.jsonl` append path as every other event.

use anyhow::Result;

use crate::ask;
use crate::events::RunLogger;
use crate::types::BackendId;
use crate::workflow::converse::normalize_kind;
use crate::workflow::guard::ENV_PARENT_RUN_ID;

/// Append a Note to the manager's run transcript. `kind` defaults to "handoff"; `from` names the
/// authoring worker (for a handoff) or is omitted (a manager board post).
pub async fn run(body: String, kind: Option<String>, from: Option<BackendId>) -> Result<()> {
    // Same structural gate as `agentpit ask`: only the manager leg carries AGENTPIT_ASK_ALLOWED
    // equal to its run id (exec::base strips it from every backend spawn). A worker is therefore a
    // silent no-op — it can neither reach the human nor write the shared transcript.
    let Some(run_id) = manager_run_id() else {
        return Ok(());
    };

    RunLogger::adopt(run_id).note(from, &normalize_kind(kind.as_deref()), &body);
    Ok(())
}

/// The manager-only gate, mirroring `cli::ask::ask_allowed`: returns the run id to write to only
/// when `AGENTPIT_ASK_ALLOWED` is present and equals `AGENTPIT_PARENT_RUN_ID`.
fn manager_run_id() -> Option<String> {
    let token = std::env::var(ask::ENV_ASK_ALLOWED).ok()?;
    let run_id = std::env::var(ENV_PARENT_RUN_ID).ok()?;
    (!token.is_empty() && token == run_id).then_some(run_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manager_run_id_returns_id_only_when_token_matches() {
        // Serialize env mutation with the rest of the crate's state-dir tests.
        let _g = crate::ask::STATE_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        // SAFETY: single-threaded under STATE_ENV_LOCK.
        unsafe {
            std::env::remove_var(ask::ENV_ASK_ALLOWED);
            std::env::remove_var(ENV_PARENT_RUN_ID);
        }
        // No env → no run id (worker without the parent run, or a bare invocation).
        assert_eq!(manager_run_id(), None);

        // A worker inherits only the parent run id, not the allow token → denied.
        unsafe {
            std::env::set_var(ENV_PARENT_RUN_ID, "run-9");
        }
        assert_eq!(manager_run_id(), None);

        // Token present but mismatched → denied.
        unsafe {
            std::env::set_var(ask::ENV_ASK_ALLOWED, "run-OTHER");
        }
        assert_eq!(manager_run_id(), None);

        // Token equals the run id (the manager leg) → the run id to write to.
        unsafe {
            std::env::set_var(ask::ENV_ASK_ALLOWED, "run-9");
        }
        assert_eq!(manager_run_id(), Some("run-9".to_string()));

        unsafe {
            std::env::remove_var(ask::ENV_ASK_ALLOWED);
            std::env::remove_var(ENV_PARENT_RUN_ID);
        }
    }
}
