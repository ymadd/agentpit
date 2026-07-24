use regex::RegexSet;
use std::sync::OnceLock;

use crate::types::BackendId;

static PATTERNS: OnceLock<RegexSet> = OnceLock::new();

fn patterns() -> &'static RegexSet {
    PATTERNS.get_or_init(|| {
        RegexSet::new([
            r"(?i)401\s+unauthorized",
            r"(?i)failed to refresh token",
            r"(?i)not\s+(logged in|authenticated|signed in)",
            r"(?i)please (log|sign) in",
            r"(?i)authentication (failed|error|required)",
            r"(?i)invalid (api[_ ]?key|credentials)",
            r"(?i)no\s+(valid\s+)?(credentials|api key)",
            r"(?i)token\s+(has\s+)?expired",
        ])
        .expect("auth-failure regex set must compile")
    })
}

pub fn is_auth_failure(text: &str) -> bool {
    if text.is_empty() {
        return false;
    }
    patterns().is_match(text)
}

/// Genuine auth failures are terse error dumps; anything longer is work product that may
/// merely *mention* an auth phrase (a review of auth-handling code, a log excerpt).
const AUTH_SCAN_MAX_BYTES: usize = 2048;

/// Classify a finished dispatch's output as an auth failure.
///
/// The raw regex scan misfired on successful runs whose *content* legitimately contained
/// phrases like "401 Unauthorized" (e.g. a security review of auth code), discarding real
/// output and aborting workflows. Two gates fix that:
/// - `exit_ok == Some(true)` (the backend reported success) is never an auth failure —
///   the text is an answer, not an error.
/// - Otherwise the regex only applies to short outputs; a long transcript that failed for
///   another reason is not re-labelled auth just because it quotes an auth phrase.
///
/// Pass `exit_ok: None` for transports without an exit signal (ACP).
pub fn is_auth_failure_outcome(text: &str, exit_ok: Option<bool>) -> bool {
    if exit_ok == Some(true) {
        return false;
    }
    text.len() <= AUTH_SCAN_MAX_BYTES && is_auth_failure(text)
}

pub fn format_auth_failure_message(
    backend: BackendId,
    login_command: &str,
    launch_message: Option<&str>,
) -> String {
    let mut lines = vec![
        format!("[{backend}] authentication appears to have failed during execution."),
        format!("Run `{login_command}` to re-authenticate."),
    ];
    if let Some(msg) = launch_message {
        lines.push(String::new());
        lines.push(msg.to_string());
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_known_failure_phrases() {
        for sample in [
            "stream error: Failed to refresh token: 401 Unauthorized",
            "ERROR: 401 Unauthorized",
            "Please log in to continue.",
            "Authentication failed",
            "Invalid API key",
            "no valid credentials",
            "Token expired",
        ] {
            assert!(is_auth_failure(sample), "expected match for: {sample}");
        }
    }

    #[test]
    fn does_not_flag_innocuous_text() {
        for sample in [
            "OK reply complete",
            "no stdout, exit=0",
            "Found 3 issues in src/foo.ts",
            "",
        ] {
            assert!(!is_auth_failure(sample), "unexpected match: {sample}");
        }
    }

    /// Eval finding 1 (2026-07): a successful run whose *content* mentions auth phrases
    /// (e.g. a security review of auth code) must not be discarded as an auth failure.
    #[test]
    fn successful_or_long_output_is_never_an_auth_failure() {
        let review = "Found issue: the handler returns 401 Unauthorized without logging. \
                      Recommend adding authentication failed telemetry.";
        // Backend reported success: content wins regardless of phrasing.
        assert!(!is_auth_failure_outcome(review, Some(true)));
        // Long output that failed for another reason is not re-labelled auth.
        let long = format!("{}{}", "x".repeat(AUTH_SCAN_MAX_BYTES), " 401 unauthorized");
        assert!(!is_auth_failure_outcome(&long, Some(false)));
        // A genuine terse auth error on a failed / exit-less run still classifies.
        assert!(is_auth_failure_outcome(
            "stream error: Failed to refresh token: 401 Unauthorized",
            Some(false)
        ));
        assert!(is_auth_failure_outcome("Please log in to continue.", None));
    }

    #[test]
    fn formats_message_with_launch_hint() {
        let msg = format_auth_failure_message(BackendId::Gemini, "gemini", Some("Opened Terminal"));
        assert!(msg.contains("[gemini]"));
        assert!(msg.contains("gemini"));
        assert!(msg.contains("Opened Terminal"));
    }
}
