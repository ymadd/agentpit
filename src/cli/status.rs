use std::collections::HashMap;

use anyhow::Result;
use tokio::task::JoinSet;

use super::load_context;
use crate::auth::{AuthStatus, check_auth};
use crate::dispatch::resolve_transport;
use crate::types::BackendId;

/// The tools whose backend a `[routes]` entry pins, as `tool=backend` strings in table
/// order. Pure: reads the config, allocates a fresh `Vec`.
fn pinned_tools(config: &crate::config::HubConfig) -> Vec<String> {
    config
        .routes
        .iter()
        .map(|(tool, backend)| format!("{tool}={backend}"))
        .collect()
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
    }

    Ok(())
}
