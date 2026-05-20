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

pub fn format_auth_failure_message(
    backend: BackendId,
    login_command: &str,
    launch_message: Option<&str>,
) -> String {
    let mut lines = vec![
        format!(
            "[{backend}] authentication appears to have failed during execution."
        ),
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

    #[test]
    fn formats_message_with_launch_hint() {
        let msg =
            format_auth_failure_message(BackendId::Gemini, "gemini", Some("Opened Terminal"));
        assert!(msg.contains("[gemini]"));
        assert!(msg.contains("gemini"));
        assert!(msg.contains("Opened Terminal"));
    }
}
