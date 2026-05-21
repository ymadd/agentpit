use anyhow::{Context, Result};

use crate::config::{DEFAULT_CONFIG_TOML, default_config_path};

pub async fn run(force: bool) -> Result<()> {
    let path = default_config_path();
    if path.exists() && !force {
        anyhow::bail!(
            "{} already exists. Pass --force to overwrite.",
            path.display()
        );
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    std::fs::write(&path, DEFAULT_CONFIG_TOML)
        .with_context(|| format!("failed to write {}", path.display()))?;
    println!("wrote: {}", path.display());
    Ok(())
}
