//! Run a project's own check against each arena submission, before its worktree is destroyed.
//!
//! The arena judges a diff, and a diff does not tell you whether it builds. Dogfooding this on
//! agentpit itself (2026-08-01) produced a round where one submission failed `cargo fmt --check`
//! — a fact no amount of careful reading of the patch would have surfaced, and one that would
//! have let a tidier-looking change win while turning CI red.
//!
//! **A failed check is reported, never disqualifying.** The whole premise of the arena is that
//! the verdict is the human's; turning a red check into an automatic loss would quietly replace
//! that verdict with a proxy, which is the thing the gold bench is already for. A submission that
//! fails its check is shown failing, next to its diff, and the judge decides what that is worth —
//! sometimes a one-line formatting slip on the better design, sometimes a fatal flaw.
//!
//! The check runs in the contender's own worktree, so it sees exactly that contender's tree and
//! nothing of its rivals'.

use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

/// Wall-clock ceiling for one submission's check. A round already costs one agentic run per
/// contender; a hung test suite must not add to that indefinitely.
const VERIFY_TIMEOUT_SECS: u64 = 600;

/// How much of the check's output is kept for the judge. Enough to see the failure, bounded so a
/// runaway suite cannot bloat every stored round.
const MAX_OUTPUT_BYTES: usize = 4 * 1024;

/// What the project's own check said about one submission.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerifyOutcome {
    /// The command that was run, so a stored round explains itself later.
    pub command: String,
    pub passed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    /// Tail of the combined output, truncated. Empty when the command said nothing.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub output: String,
}

impl VerifyOutcome {
    /// A one-line summary for a listing.
    pub fn summary(&self) -> String {
        match (self.passed, self.exit_code) {
            (true, _) => "check passed".into(),
            (false, Some(code)) => format!("check failed (exit {code})"),
            (false, None) => "check did not complete".into(),
        }
    }
}

/// Run `command` in `cwd` and reduce it to a [`VerifyOutcome`]. `None` when no check is
/// configured — the arena stays usable without one, it just cannot tell the judge anything.
///
/// A launch failure, a timeout, and a cancellation are all recorded as a NOT-passed outcome with
/// the reason in `output`, rather than as an error that would lose the whole round: the round's
/// expensive part is already done by the time this runs.
pub async fn run(
    command: Option<&str>,
    cwd: &Path,
    cancel: &CancellationToken,
) -> Option<VerifyOutcome> {
    let command = command.map(str::trim).filter(|c| !c.is_empty())?;
    let mut outcome = VerifyOutcome {
        command: command.to_string(),
        passed: false,
        exit_code: None,
        output: String::new(),
    };

    let child = tokio::process::Command::new("sh")
        .args(["-c", command])
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true)
        .output();

    tokio::select! {
        result = child => match result {
            Ok(out) => {
                outcome.passed = out.status.success();
                outcome.exit_code = out.status.code();
                outcome.output = tail(&combined(&out.stdout, &out.stderr));
            }
            Err(e) => outcome.output = format!("the check failed to launch: {e}"),
        },
        _ = cancel.cancelled() => outcome.output = "the check was cancelled".into(),
        _ = tokio::time::sleep(Duration::from_secs(VERIFY_TIMEOUT_SECS)) => {
            outcome.output = format!("the check timed out after {VERIFY_TIMEOUT_SECS}s");
        }
    }
    Some(outcome)
}

fn combined(stdout: &[u8], stderr: &[u8]) -> String {
    let mut text = String::from_utf8_lossy(stdout).trim().to_string();
    let err = String::from_utf8_lossy(stderr);
    let err = err.trim();
    if !err.is_empty() {
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(err);
    }
    text
}

/// The LAST `MAX_OUTPUT_BYTES` of the output, on a char boundary. The tail, not the head: a test
/// runner puts its failures at the end, and the head is compilation noise.
fn tail(text: &str) -> String {
    if text.len() <= MAX_OUTPUT_BYTES {
        return text.to_string();
    }
    let mut start = text.len() - MAX_OUTPUT_BYTES;
    while start < text.len() && !text.is_char_boundary(start) {
        start += 1;
    }
    format!("[earlier output truncated]\n{}", &text[start..])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn no_configured_check_means_nothing_to_report() {
        let live = CancellationToken::new();
        assert!(run(None, Path::new("."), &live).await.is_none());
        // A blank setting is the same as none, not a command called "".
        assert!(run(Some("   "), Path::new("."), &live).await.is_none());
    }

    #[tokio::test]
    async fn exit_status_becomes_the_verdict_and_output_is_kept() {
        let live = CancellationToken::new();
        let ok = run(Some("echo fine"), Path::new("."), &live).await.unwrap();
        assert!(ok.passed);
        assert_eq!(ok.exit_code, Some(0));
        assert_eq!(ok.output, "fine");

        let bad = run(Some("echo boom >&2; exit 3"), Path::new("."), &live)
            .await
            .unwrap();
        assert!(!bad.passed);
        assert_eq!(bad.exit_code, Some(3));
        assert_eq!(bad.output, "boom");
        assert_eq!(bad.summary(), "check failed (exit 3)");
    }

    /// A cancelled or hung check must not lose the round — the expensive part is already done —
    /// so it comes back as a not-passed outcome carrying the reason.
    #[tokio::test]
    async fn cancellation_yields_a_recorded_non_pass_rather_than_an_error() {
        let cancelled = CancellationToken::new();
        cancelled.cancel();
        let out = run(Some("sleep 60"), Path::new("."), &cancelled)
            .await
            .unwrap();
        assert!(!out.passed);
        assert!(out.output.contains("cancelled"), "{}", out.output);
        assert_eq!(out.summary(), "check did not complete");
    }

    #[test]
    fn long_output_keeps_its_tail_where_the_failures_are() {
        let text = format!("{}\nFAILED: the thing", "compile noise\n".repeat(2000));
        let kept = tail(&text);
        assert!(kept.len() <= MAX_OUTPUT_BYTES + 40);
        assert!(kept.ends_with("FAILED: the thing"), "the tail must survive");
        assert!(kept.starts_with("[earlier output truncated]"));
        assert!(std::str::from_utf8(kept.as_bytes()).is_ok());
    }
}
