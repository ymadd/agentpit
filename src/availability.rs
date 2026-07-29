//! Which backends are worth auto-routing to right now, read from `events.jsonl`.
//!
//! Credentials being present is not evidence a backend works: an exhausted quota or a
//! retired client authenticates fine and then fails on the first dispatch (HILLTE-257).
//! This module derives per-backend evidence from the last recorded dispatch:
//! - [`last_failures`] — the most recent finished leg per backend, kept only where it
//!   failed (`agentpit status` renders these as notes).
//! - [`suspended_backends`] — the subset whose failure is *durable* (quota / tier / auth
//!   shaped, not a timeout) and recent. The router keeps these out of the auto-route
//!   stages so long-context and profile traffic stops piling onto a backend that is
//!   quota-dead for days; an explicit `--backend` or a `[routes]` pin is always honored.
//!
//! Availability depends on the user's individual plan and resets on the provider's clock,
//! so it is deliberately NOT baked into seeded capability scores — it is read fresh from
//! telemetry and self-heals via [`SUSPEND_COOLDOWN_MS`].

use std::collections::{BTreeMap, HashSet};

use crate::auth::is_auth_failure;
use crate::events::{Event, LegStatus};
use crate::types::BackendId;

/// How long a durable failure keeps a backend out of the auto-route stages. Long enough
/// that a work session stops hammering a quota-dead backend on every dispatch, short
/// enough to re-probe well before any real quota window (hours to days) expires — one
/// ~10s failed probe per cooldown is the cost of staying self-healing without state.
pub const SUSPEND_COOLDOWN_MS: u64 = 30 * 60 * 1000;

/// The reason note is a one-line hint; the run log under `runs/` keeps the full output.
pub const REASON_MAX_CHARS: usize = 160;

/// The last thing a backend actually did, when that was a failure.
#[derive(Debug, Clone, PartialEq)]
pub struct LastFailure {
    pub ts: u64,
    /// The line most likely to explain it, for display.
    pub reason: String,
    /// True when the failure shape is durable (quota / tier / auth) rather than
    /// task-specific (timeout, crash): classified over the FULL error text, not the
    /// display line.
    pub durable: bool,
}

/// The most recent finished leg per backend, kept only where it failed. A later success
/// drops the entry, so a backend that recovered reports nothing; skipped legs never ran,
/// so they are evidence of neither. Unparseable lines are ignored — the log is
/// best-effort by design.
pub fn last_failures(log: &str) -> BTreeMap<BackendId, LastFailure> {
    let mut last: BTreeMap<BackendId, LastFailure> = BTreeMap::new();
    for line in log.lines() {
        let Ok(Event::MemberFinished {
            ts,
            backend,
            status,
            error,
            ..
        }) = serde_json::from_str::<Event>(line)
        else {
            continue;
        };
        match status {
            LegStatus::Ok => {
                last.remove(&backend);
            }
            LegStatus::Error => {
                last.insert(
                    backend,
                    LastFailure {
                        ts,
                        reason: failure_reason(error.as_deref()),
                        durable: is_durable_failure(error.as_deref().unwrap_or_default()),
                    },
                );
            }
            LegStatus::Skipped => {}
        }
    }
    last
}

/// A failure that will keep happening until the account state changes: quota exhaustion,
/// a retired client tier, or broken authentication. A timeout or crash is task-specific
/// and must NOT suspend the backend — the observed codex case is a 30-minute dispatch
/// timeout on a healthy account.
pub fn is_durable_failure(error: &str) -> bool {
    let lower = error.to_ascii_lowercase();
    ["quota", "rate limit", "ineligible", "auth failure"]
        .iter()
        .any(|signal| lower.contains(signal))
        || is_auth_failure(error)
}

/// Backends whose last dispatch failed durably within [`SUSPEND_COOLDOWN_MS`] — the set
/// the router's auto stages should skip right now.
pub fn suspended_backends(log: &str, now: u64) -> HashSet<BackendId> {
    last_failures(log)
        .into_iter()
        .filter(|(_, failure)| {
            failure.durable && now.saturating_sub(failure.ts) <= SUSPEND_COOLDOWN_MS
        })
        .map(|(backend, _)| backend)
        .collect()
}

/// [`suspended_backends`] over the live event log at the current time — what dispatch call
/// sites hand the router. A missing or unreadable log suspends nothing.
pub fn recently_suspended() -> HashSet<BackendId> {
    suspended_backends(
        &std::fs::read_to_string(crate::events::events_path()).unwrap_or_default(),
        crate::events::now_ms(),
    )
}

/// Strip the noise a captured stderr line carries into the log.
fn informative_line(line: &str) -> &str {
    let line = line.trim();
    line.strip_prefix("stderr:").unwrap_or(line).trim()
}

/// The line most likely to explain a failure. Line one is normally the generic
/// `<backend> exited with code 1`, and CLIs print startup warnings before the real message, so
/// "first line" surfaces noise for exactly the failures worth surfacing. The signals below are
/// deliberately narrow: `limit` is absent because a benign `rendering will be limited` warning
/// matches it, which is how a real captured error would have been misreported.
pub fn failure_reason(error: Option<&str>) -> String {
    const SIGNALS: [&str; 5] = ["error", "quota", "rate limit", "unauthorized", "forbidden"];
    let lines: Vec<&str> = error
        .unwrap_or_default()
        .lines()
        .map(informative_line)
        .filter(|line| !line.is_empty())
        .collect();
    let picked = lines
        .iter()
        .copied()
        .find(|line| {
            let lower = line.to_ascii_lowercase();
            SIGNALS.iter().any(|signal| lower.contains(signal))
        })
        .or_else(|| lines.first().copied())
        .unwrap_or("no reason was recorded");
    if picked.chars().count() <= REASON_MAX_CHARS {
        return picked.to_string();
    }
    let head: String = picked.chars().take(REASON_MAX_CHARS).collect();
    format!("{head}…")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Captured from events.jsonl on 2026-07-25 — the failure that made `auth=ok` misleading.
    const ANTIGRAVITY_QUOTA: &str = "antigravity exited with code 1\nstderr: Error: Individual quota reached. Please upgrade your subscription to increase your limits. Resets in 129h32m53s.";

    /// Also captured (from a since-removed backend), trailing lines elided. Kept because the
    /// shape recurs: startup warnings before the real error — and one of those warnings says
    /// "will be limited", which a `limit` signal would have reported instead of the error.
    const WARNINGS_THEN_ERROR: &str = "gemini exited with code 1\nstderr: Warning: Basic terminal detected (TERM=dumb). Visual rendering will be limited. For the best experience, use a terminal emulator with truecolor support.\nWarning: 256-color support not detected.\nYOLO mode is enabled. All tool calls will be automatically approved.\nError authenticating: IneligibleTierError: This client is no longer supported for Gemini Code Assist for individuals.";

    /// Also captured: a task-specific failure that must never suspend the backend.
    const CODEX_TIMEOUT: &str =
        "codex dispatch timed out after 1800s (set AGENTPIT_DISPATCH_TIMEOUT_SECS to adjust)";

    #[test]
    fn reason_skips_the_exit_line_and_the_startup_warnings() {
        assert_eq!(
            failure_reason(Some(ANTIGRAVITY_QUOTA)),
            "Error: Individual quota reached. Please upgrade your subscription to increase your limits. Resets in 129h32m53s."
        );
        assert_eq!(
            failure_reason(Some(WARNINGS_THEN_ERROR)),
            "Error authenticating: IneligibleTierError: This client is no longer supported for Gemini Code Assist for individuals."
        );
    }

    #[test]
    fn reason_falls_back_to_the_first_line_and_tolerates_a_missing_error() {
        assert_eq!(failure_reason(Some(CODEX_TIMEOUT)), CODEX_TIMEOUT);
        assert_eq!(failure_reason(None), "no reason was recorded");
        assert_eq!(failure_reason(Some("  \n\n ")), "no reason was recorded");
    }

    #[test]
    fn reason_is_clamped_to_a_single_line_of_output() {
        let clamped = failure_reason(Some(&format!("Error: {}", "x".repeat(400))));
        assert_eq!(clamped.chars().count(), REASON_MAX_CHARS + 1);
        assert!(clamped.ends_with('…'));
    }

    #[test]
    fn quota_tier_and_auth_failures_are_durable_but_timeouts_are_not() {
        assert!(is_durable_failure(ANTIGRAVITY_QUOTA));
        assert!(is_durable_failure(WARNINGS_THEN_ERROR));
        // The exact string dispatch writes when the auth scan classifies a run.
        assert!(is_durable_failure("auth failure during execution"));
        // The curated auth patterns still apply to raw captured stderr.
        assert!(is_durable_failure(
            "stream error: Failed to refresh token: 401 Unauthorized"
        ));
        assert!(!is_durable_failure(CODEX_TIMEOUT));
        assert!(!is_durable_failure("exited with signal 9"));
        assert!(!is_durable_failure(""));
    }

    #[test]
    fn a_later_success_clears_the_failure_and_junk_lines_are_skipped() {
        let log = concat!(
            r#"{"event":"member_finished","ts":1000,"run_id":"r-1","backend":"antigravity","status":"error","elapsed_ms":10,"error":"antigravity exited with code 1\nstderr: Error: Individual quota reached."}"#,
            "\nnot json at all\n",
            r#"{"event":"member_finished","ts":2000,"run_id":"r-2","backend":"codex","status":"error","elapsed_ms":10,"error":"auth failure during execution"}"#,
            "\n",
            r#"{"event":"member_finished","ts":3000,"run_id":"r-3","backend":"codex","status":"ok","elapsed_ms":10,"chars":12}"#,
            "\n",
            r#"{"event":"member_finished","ts":4000,"run_id":"r-4","backend":"claude","status":"skipped","elapsed_ms":0}"#,
            "\n",
        );
        let failures = last_failures(log);
        // codex recovered, claude never ran: only the backend whose last leg failed is left.
        assert_eq!(
            failures.keys().copied().collect::<Vec<_>>(),
            vec![BackendId::Antigravity]
        );
        let quota = &failures[&BackendId::Antigravity];
        assert_eq!(quota.ts, 1000);
        assert_eq!(quota.reason, "Error: Individual quota reached.");
        assert!(quota.durable);
    }

    #[test]
    fn suspension_needs_a_durable_recent_failure() {
        fn line(ts: u64, backend: &str, error: &str) -> String {
            format!(
                r#"{{"event":"member_finished","ts":{ts},"run_id":"r","backend":"{backend}","status":"error","elapsed_ms":10,"error":{}}}"#,
                serde_json::to_string(error).unwrap()
            )
        }
        let now = SUSPEND_COOLDOWN_MS * 10;
        let log = [
            // Recent quota failure: suspended.
            line(now - 1000, "antigravity", ANTIGRAVITY_QUOTA),
            // Recent but task-specific: not suspended.
            line(now - 1000, "codex", CODEX_TIMEOUT),
            // Durable but older than the cooldown: not suspended (self-healing re-probe).
            line(
                now - SUSPEND_COOLDOWN_MS - 1,
                "opencode",
                "auth failure during execution",
            ),
        ]
        .join("\n");

        let suspended = suspended_backends(&log, now);
        assert_eq!(
            suspended.into_iter().collect::<Vec<_>>(),
            vec![BackendId::Antigravity]
        );
        // At the exact cooldown boundary the failure still counts.
        let boundary = suspended_backends(
            &line(now - SUSPEND_COOLDOWN_MS, "claude", ANTIGRAVITY_QUOTA),
            now,
        );
        assert!(boundary.contains(&BackendId::Claude));
    }
}
