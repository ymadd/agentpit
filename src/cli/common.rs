use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use tokio_util::sync::CancellationToken;

use crate::config::{LoadedConfig, load_config};
use crate::dispatch::{Registries, build_registries};

pub struct Context {
    pub loaded: LoadedConfig,
    pub regs: Registries,
}

pub fn load_context() -> Result<Context> {
    let loaded = load_config(None)?;
    let regs = build_registries(&loaded.config);
    Ok(Context { loaded, regs })
}

pub fn resolve_cwd(cwd: Option<String>) -> Result<PathBuf> {
    if let Some(s) = cwd {
        return Ok(PathBuf::from(s));
    }
    Ok(std::env::current_dir()?)
}

pub fn stdout_streamer() -> Arc<dyn Fn(&str) + Send + Sync> {
    Arc::new(|chunk: &str| {
        let mut stdout = std::io::stdout().lock();
        let _ = stdout.write_all(chunk.as_bytes());
        let _ = stdout.flush();
    })
}

pub fn install_ctrlc_cancel(token: CancellationToken) {
    tokio::spawn(async move {
        if let Ok(()) = tokio::signal::ctrl_c().await {
            token.cancel();
        }
    });
}
