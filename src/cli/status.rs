use std::collections::{BTreeMap, HashMap};

use anyhow::Result;
use tokio::task::JoinSet;

use super::load_context;
use crate::auth::{AuthStatus, check_auth};
use crate::dispatch::resolve_transport;
use crate::events::{Event, LegStatus, events_path, now_ms};
use crate::types::BackendId;

/// Failures older than this say nothing about whether the backend works now. Sized to cover a
/// weekly quota cycle: antigravity's exhaustion message quotes a reset over five days out, so a
/// shorter window would hide a block that is still in force.
const FAILURE_WINDOW_MS: u64 = 7 * 24 * 60 * 60 * 1000;
/// The note is a one-line hint; the run log under `runs/` keeps the full output.
const REASON_MAX_CHARS: usize = 160;

/// The last thing a backend actually did, when that was a failure.
#[derive(Debug, Clone, PartialEq)]
struct LastFailure {
    ts: u64,
    reason: String,
}

/// The tools whose backend a `[routes]` entry pins, as `tool=backend` strings in table
/// order. Pure: reads the config, allocates a fresh `Vec`.
fn pinned_tools(config: &crate::config::HubConfig) -> Vec<String> {
    config
        .routes
        .iter()
        .map(|(tool, backend)| format!("{tool}={backend}"))
        .collect()
}

/// The most recent finished leg per backend, kept only where it failed. A later success drops
/// the entry, so a backend that recovered reports nothing; skipped legs never ran, so they are
/// evidence of neither. Unparseable lines are ignored — the log is best-effort by design.
fn last_failures(log: &str) -> BTreeMap<BackendId, LastFailure> {
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
                    },
                );
            }
            LegStatus::Skipped => {}
        }
    }
    last
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
fn failure_reason(error: Option<&str>) -> String {
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

/// Coarse age, in the largest unit that is still non-zero.
fn age(elapsed_ms: u64) -> String {
    let minutes = elapsed_ms / 60_000;
    match minutes {
        0 => "just now".into(),
        1..60 => format!("{minutes}m ago"),
        60..1440 => format!("{}h ago", minutes / 60),
        _ => format!("{}d ago", minutes / 1440),
    }
}

/// What to print under a backend whose last dispatch failed, or `None` when that failure is too
/// old to mean anything. A clock that moved backwards yields a future `ts`; saturating keeps the
/// evidence visible instead of hiding it.
fn failure_note(failure: &LastFailure, now: u64) -> Option<String> {
    let elapsed = now.saturating_sub(failure.ts);
    (elapsed <= FAILURE_WINDOW_MS).then(|| {
        format!(
            "note: the last dispatch failed {} — {}",
            age(elapsed),
            failure.reason
        )
    })
}

pub async fn run(filter: Option<BackendId>) -> Result<()> {
    let ctx = load_context()?;
    let available = ctx.regs.available();
    let mut backends: Vec<BackendId> = available.into_iter().collect();
    backends.sort();

    let targets: Vec<BackendId> = match filter {
        Some(b) => vec![b],
        None => backends,
    };

    println!(
        "config: {} ({})",
        ctx.loaded.source.as_str(),
        ctx.loaded.path.display()
    );
    println!("default backend: {}", ctx.loaded.config.default.backend);
    println!(
        "auto_route: {}",
        if ctx.loaded.config.default.auto_route {
            "on"
        } else {
            "off"
        }
    );
    // A `[routes]` pin short-circuits auto_route for that tool, so learned/benchmarked
    // capability never influences it. Silent before: `auto_route: on` read as "capability
    // routing is live" even when every tool was pinned and the profile stage never ran.
    if ctx.loaded.config.default.auto_route {
        let pinned = pinned_tools(&ctx.loaded.config);
        if !pinned.is_empty() {
            println!(
                "  note: [routes] pins {} — auto_route (capability profile / similarity) \
                 does not run for {}. Remove the pin to route by measured capability.",
                pinned.join(", "),
                if pinned.len() == 1 { "it" } else { "them" },
            );
        }
    }
    println!();
    println!("backends:");

    let mut set: JoinSet<(BackendId, AuthStatus)> = JoinSet::new();
    for id in &targets {
        let id = *id;
        set.spawn(async move { (id, check_auth(id).await) });
    }
    let mut auth_map: HashMap<BackendId, AuthStatus> = HashMap::new();
    while let Some(res) = set.join_next().await {
        if let Ok((id, status)) = res {
            auth_map.insert(id, status);
        }
    }

    // Credentials being present says nothing about a backend being usable: an exhausted quota
    // or a retired client authenticates fine and then fails on the first dispatch. Reporting the
    // last real dispatch turns `auth=ok` from a promise into evidence.
    let failures = last_failures(&std::fs::read_to_string(events_path()).unwrap_or_default());
    let now = now_ms();

    for id in targets {
        let transport_str = resolve_transport(id, &ctx.regs)
            .map(|t| t.as_str())
            .unwrap_or("none");
        let auth_line = match auth_map.get(&id) {
            Some(auth) if auth.ok => "auth=ok".to_string(),
            Some(auth) => format!("auth=missing ({})", auth.login_command),
            None => "auth=unknown".to_string(),
        };
        println!("  [{id}] transport={transport_str} {auth_line}");
        if let Some(note) = failures.get(&id).and_then(|f| failure_note(f, now)) {
            println!("    {note}");
        }
    }

    Ok(())
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
        let timeout = "codex dispatch timed out after 1800s (set AGENTPIT_DISPATCH_TIMEOUT_SECS to adjust)";
        assert_eq!(failure_reason(Some(timeout)), timeout);
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
    }

    #[test]
    fn a_failure_is_reported_until_it_stops_meaning_anything() {
        let failure = LastFailure {
            ts: 1_000_000,
            reason: "Error: Individual quota reached.".into(),
        };
        assert_eq!(
            failure_note(&failure, 1_000_000 + 2 * 60 * 60 * 1000).as_deref(),
            Some("note: the last dispatch failed 2h ago — Error: Individual quota reached.")
        );
        assert!(failure_note(&failure, 1_000_000 + FAILURE_WINDOW_MS).is_some());
        assert!(failure_note(&failure, 1_000_000 + FAILURE_WINDOW_MS + 1).is_none());
        // A clock that moved backwards must not hide the evidence.
        assert!(failure_note(&failure, 999).is_some());
    }

    #[test]
    fn age_uses_the_largest_non_zero_unit() {
        assert_eq!(age(30_000), "just now");
        assert_eq!(age(90_000), "1m ago");
        assert_eq!(age(59 * 60_000), "59m ago");
        assert_eq!(age(60 * 60_000), "1h ago");
        assert_eq!(age(25 * 60 * 60_000), "1d ago");
    }
}
