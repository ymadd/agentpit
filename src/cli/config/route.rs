use anyhow::{Result, anyhow};
use console::style;

use crate::cli::cancel::{self, Nav};
use crate::config::{RouteKey, load_config, save_config};
use crate::dispatch::build_registries;
use crate::types::BackendId;

pub async fn run(tool: RouteKey, backend: Option<BackendId>) -> Result<()> {
    let mut loaded = load_config(None)?;
    let regs = build_registries(&loaded.config);
    let mut available: Vec<BackendId> = regs.available().into_iter().collect();
    available.sort();
    if available.is_empty() {
        anyhow::bail!("no backends registered");
    }

    // When `backend` is supplied via CLI flag, run the non-interactive path
    // whose output must remain byte-for-byte identical to before.
    if let Some(b) = backend {
        if !available.contains(&b) {
            anyhow::bail!("backend {b} is not registered");
        }
        loaded.config.routes.insert(tool, b);
        let path = save_config(&loaded.config)?;
        // Preserve the exact non-interactive output format (no intro/outro/confirm_change).
        println!(
            "set route.{tool} = {} in {}",
            style(b).cyan(),
            path.display()
        );
        return Ok(());
    }

    // ── Interactive path ────────────────────────────────────────────────────

    let current = loaded
        .config
        .routes
        .get(&tool)
        .copied()
        .map(|b| b.to_string())
        .unwrap_or_else(|| "(none)".into());

    cliclack::intro(style(format!(" route: {tool} ")).on_cyan().black())
        .map_err(|e| anyhow!("intro failed: {e}"))?;

    let mut sel = cliclack::select(format!(
        "Default backend for `{tool}`  (current: {current})"
    ));
    for b in &available {
        sel = sel.item(*b, b.to_string(), "");
    }
    if let Some(cur) = loaded.config.routes.get(&tool).copied()
        && available.contains(&cur)
    {
        sel = sel.initial_value(cur);
    }

    let chosen = match cancel::prompt(sel.interact())? {
        Nav::Value(b) => b,
        Nav::Back => {
            cliclack::outro("(cancelled — no changes made)")
                .map_err(|e| anyhow!("outro failed: {e}"))?;
            return Ok(());
        }
    };

    // Capture the prior value (immutable read) before mutating state.
    let prior_str = loaded
        .config
        .routes
        .get(&tool)
        .copied()
        .map(|b| b.to_string())
        .unwrap_or_else(|| "(none)".into());

    loaded.config.routes.insert(tool, chosen);
    let path = save_config(&loaded.config)?;

    cancel::confirm_change(&format!("route.{tool}"), &prior_str, &chosen.to_string());

    cliclack::outro(format!(
        "Saved to {}",
        style(path.display().to_string()).dim(),
    ))
    .map_err(|e| anyhow!("outro failed: {e}"))?;
    Ok(())
}
