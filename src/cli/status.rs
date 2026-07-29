use std::collections::HashMap;

use anyhow::Result;
use tokio::task::JoinSet;

use super::load_context;
use crate::auth::{AuthStatus, check_auth};
use crate::availability::{LastFailure, last_failures};
use crate::dispatch::resolve_transport;
use crate::events::{events_path, now_ms};
use crate::types::BackendId;

/// Failures older than this say nothing about whether the backend works now. Sized to cover a
/// weekly quota cycle: antigravity's exhaustion message quotes a reset over five days out, so a
/// shorter window would hide a block that is still in force.
const FAILURE_WINDOW_MS: u64 = 7 * 24 * 60 * 60 * 1000;

/// The tools whose backend a `[routes]` entry pins, as `tool=backend` strings in table
/// order. Pure: reads the config, allocates a fresh `Vec`.
fn pinned_tools(config: &crate::config::HubConfig) -> Vec<String> {
    config
        .routes
        .iter()
        .map(|(tool, backend)| format!("{tool}={backend}"))
        .collect()
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

    #[test]
    fn a_failure_is_reported_until_it_stops_meaning_anything() {
        let failure = LastFailure {
            ts: 1_000_000,
            reason: "Error: Individual quota reached.".into(),
            durable: true,
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
