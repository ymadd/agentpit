use anyhow::{Result, anyhow};
use console::style;

use crate::config::{BackendOverride, load_config, save_config};
use crate::types::{BackendId, Transport};

pub async fn run(id: BackendId) -> Result<()> {
    let mut loaded = load_config(None)?;

    let current = loaded
        .config
        .backends
        .get(&id)
        .and_then(|o| o.transport);
    let current_str = current
        .map(|t| t.as_str().to_string())
        .unwrap_or_else(|| "(default)".into());

    cliclack::intro(style(format!(" backend: {id} ")).on_cyan().black())
        .map_err(|e| anyhow!("intro failed: {e}"))?;

    let transport = cliclack::select(format!(
        "Transport for {id}  (current: {current_str})"
    ))
    .item(Transport::Exec, "exec", "spawn the CLI per request")
    .item(Transport::Acp, "acp", "persistent ACP session")
    .interact()
    .map_err(|e| anyhow!("select failed: {e}"))?;

    loaded
        .config
        .backends
        .entry(id)
        .or_insert_with(BackendOverride::default)
        .transport = Some(transport);

    let path = save_config(&loaded.config)?;
    cliclack::outro(format!(
        "Set {id} transport={} in {}",
        style(transport.as_str()).cyan(),
        path.display()
    ))
    .map_err(|e| anyhow!("outro failed: {e}"))?;
    Ok(())
}
