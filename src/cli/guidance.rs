//! Shared user-facing hint strings (design §7.2 A1, after prime's auth-guidance.ts).
//!
//! The rule every message here enforces: **end with the next concrete action** — a
//! command to run, not just a diagnosis. Messages used in more than one place live here
//! so the wording cannot drift apart; single-site messages stay inline but follow the
//! same rule.

use crate::types::BackendId;

/// The backend refused/lacks credentials — tell the user exactly how to fix it.
pub fn auth_hint(backend: BackendId, login_command: &str) -> String {
    format!("[{backend}] not authenticated. Run `{login_command}` or use /login {backend}.")
}

/// How to get back into a session from a fresh terminal.
pub fn resume_hint(short_id: &str) -> String {
    format!("resume with `agentpit repl --resume {short_id}` or `agentpit attach {short_id}`")
}

/// The daemon-backed comeback line shown on detach.
pub fn detach_hint(short_id: &str) -> String {
    format!("session {short_id} keeps running; return with `agentpit attach {short_id}`")
}

/// The last 12 chars of a session id — enough to resolve via suffix match.
pub fn short_id(session_id: &str) -> &str {
    &session_id[session_id.len().saturating_sub(12)..]
}

/// Suggest the closest known slash command for an unknown one (§7.2 A4). Prefix and
/// containment beat edit distance at this scale and never suggest nonsense.
pub fn suggest_slash(input: &str, known: &[&str]) -> Option<String> {
    let lower = input.to_ascii_lowercase();
    known
        .iter()
        .find(|k| k.starts_with(&lower) || lower.starts_with(**k))
        .or_else(|| known.iter().find(|k| k.contains(&lower)))
        .map(|k| format!("Did you mean /{k}?"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_hint_ends_with_an_action() {
        // The A1 contract: no message stops at a diagnosis.
        let auth = auth_hint(BackendId::Claude, "claude login");
        assert!(auth.contains("Run `claude login`"));
        assert!(resume_hint("abc123").contains("agentpit repl --resume abc123"));
        assert!(detach_hint("abc123").contains("agentpit attach abc123"));
    }

    #[test]
    fn short_id_takes_the_tail() {
        assert_eq!(
            short_id("0198f3f2-7c1a-7000-8000-3f2a9b1c4d5e"),
            "3f2a9b1c4d5e"
        );
        assert_eq!(short_id("tiny"), "tiny");
    }

    #[test]
    fn slash_suggestions_prefer_prefix_then_containment() {
        let known = ["session", "sessions", "status", "tree"];
        assert_eq!(
            suggest_slash("sess", &known),
            Some("Did you mean /session?".into())
        );
        assert_eq!(
            suggest_slash("ree", &known),
            Some("Did you mean /tree?".into())
        );
        assert_eq!(suggest_slash("zzz", &known), None);
    }
}
