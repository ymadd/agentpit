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

/// How much of the output's tail the auth scan looks at. An auth failure is the last thing
/// a backend emits — the run stops there — while a transcript that merely *discusses* auth
/// carries the phrase in its body.
const AUTH_SCAN_TAIL_BYTES: usize = 2048;

/// The last [`AUTH_SCAN_TAIL_BYTES`] of `text`, trimmed to a `char` boundary.
fn scan_tail(text: &str) -> &str {
    if text.len() <= AUTH_SCAN_TAIL_BYTES {
        return text;
    }
    let mut start = text.len() - AUTH_SCAN_TAIL_BYTES;
    while start < text.len() && !text.is_char_boundary(start) {
        start += 1;
    }
    &text[start..]
}

/// Classify a finished dispatch's output as an auth failure.
///
/// The raw regex scan misfired on successful runs whose *content* legitimately contained
/// phrases like "401 Unauthorized" (e.g. a security review of auth code), discarding real
/// output and aborting workflows. Two gates fix that without going blind:
/// - `exit_ok == Some(true)` (the backend reported success) is never an auth failure —
///   the text is an answer, not an error.
/// - Otherwise only the output's *tail* is scanned. A whole-output length cap was the
///   first attempt and was wrong for ACP: opencode transcripts routinely exceed any sane
///   cap, so a real auth failure arriving after substantial work went undetected. Scanning
///   the tail keeps long work product from being relabelled while still catching the
///   failure that ended the run.
///
/// Pass `exit_ok: None` for transports with no exit signal (ACP). Note that `exec` only
/// yields an `Ok` outcome on exit 0 (`run_spec` errors otherwise), so in practice its call
/// always takes the first gate; the argument is threaded anyway so the classification stays
/// correct if that contract ever changes.
pub fn is_auth_failure_outcome(text: &str, exit_ok: Option<bool>) -> bool {
    if exit_ok == Some(true) {
        return false;
    }
    is_auth_failure(scan_tail(text))
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
    fn successful_run_is_never_an_auth_failure() {
        let review = "Found issue: the handler returns 401 Unauthorized without logging. \
                      Recommend adding authentication failed telemetry.";
        // Backend reported success: content wins regardless of phrasing.
        assert!(!is_auth_failure_outcome(review, Some(true)));
        // A genuine terse auth error on a failed / exit-less run still classifies.
        assert!(is_auth_failure_outcome(
            "stream error: Failed to refresh token: 401 Unauthorized",
            Some(false)
        ));
        assert!(is_auth_failure_outcome("Please log in to continue.", None));
    }

    /// Review finding (2026-07-25): the first fix capped the scan by TOTAL output length,
    /// which blinded the ACP path — opencode transcripts exceed any cap, so an auth failure
    /// after real work went undetected, and ACP has no exit signal to fall back on. The
    /// scan now reads the tail: long work product is still safe, the ending failure is not.
    #[test]
    fn long_acp_transcript_is_classified_by_its_tail() {
        let body = "reviewed the module and it looks fine. ".repeat(400);
        assert!(
            body.len() > AUTH_SCAN_TAIL_BYTES * 2,
            "need a long transcript"
        );

        // Auth failure at the END of a long ACP run (exit_ok = None): detected.
        let ended_badly = format!("{body}\nAuthentication failed — please log in again.");
        assert!(is_auth_failure_outcome(&ended_badly, None));

        // The same phrase in the BODY of a long, successfully-finished transcript: not an
        // auth failure (this is the false positive the gate exists to prevent).
        let discusses_auth = format!("Authentication failed handling is missing here.\n{body}");
        assert!(!is_auth_failure_outcome(&discusses_auth, None));

        // Multibyte tail: the boundary trim must not panic or lose the marker.
        let jp = format!("{}\n認証エラー: Please log in.", "作業ログ。".repeat(500));
        assert!(is_auth_failure_outcome(&jp, None));
    }

    #[test]
    fn formats_message_with_launch_hint() {
        let msg = format_auth_failure_message(BackendId::Gemini, "gemini", Some("Opened Terminal"));
        assert!(msg.contains("[gemini]"));
        assert!(msg.contains("gemini"));
        assert!(msg.contains("Opened Terminal"));
    }
}
