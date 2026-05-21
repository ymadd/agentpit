use anyhow::{Context, Result};

use crate::config::load_config;

pub async fn run() -> Result<()> {
    let loaded = load_config(None)?;
    println!(
        "# source: {} ({})",
        loaded.source.as_str(),
        loaded.path.display()
    );
    let raw = toml::to_string_pretty(&loaded.config)
        .context("failed to serialize config to TOML")?;
    print!("{raw}");
    Ok(())
}
