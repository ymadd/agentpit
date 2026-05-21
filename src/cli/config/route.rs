use anyhow::{Result, anyhow};
use console::style;

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

    let backend = match backend {
        Some(b) if available.contains(&b) => b,
        Some(b) => anyhow::bail!("backend {b} is not registered"),
        None => {
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
            if let Some(cur) = loaded.config.routes.get(&tool).copied() {
                if available.contains(&cur) {
                    sel = sel.initial_value(cur);
                }
            }
            sel.interact()
                .map_err(|e| anyhow!("select failed: {e}"))?
        }
    };

    loaded.config.routes.insert(tool, backend);
    let path = save_config(&loaded.config)?;
    println!(
        "set route.{tool} = {} in {}",
        style(backend).cyan(),
        path.display()
    );
    Ok(())
}
