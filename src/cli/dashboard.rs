use std::path::PathBuf;
use std::process::Command;

use anyhow::{Result, bail};

#[cfg(windows)]
const DASHBOARD_BIN: &str = "agentpit-dashboard.exe";
#[cfg(not(windows))]
const DASHBOARD_BIN: &str = "agentpit-dashboard";

/// Launch the live desktop dashboard. The dashboard is a separate Tauri app
/// (`agentpit-dashboard`); this spawns it detached and returns immediately.
pub async fn run() -> Result<()> {
    let bin = locate()?;
    Command::new(&bin)
        .spawn()
        .map_err(|e| anyhow::anyhow!("failed to launch {}: {e}", bin.display()))?;
    eprintln!("Launched dashboard ({}).", bin.display());
    eprintln!("It updates live as you run agentpit commands. Close its window to quit.");
    Ok(())
}

/// Find the dashboard binary: explicit override, then next to this executable, then PATH.
fn locate() -> Result<PathBuf> {
    if let Ok(p) = std::env::var("AGENTPIT_DASHBOARD_BIN") {
        let pb = PathBuf::from(&p);
        if !p.is_empty() && pb.is_file() {
            return Ok(pb);
        }
    }
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        let cand = dir.join(DASHBOARD_BIN);
        if cand.is_file() {
            return Ok(cand);
        }
    }
    if let Some(p) = find_on_path(DASHBOARD_BIN) {
        return Ok(p);
    }
    bail!(
        "`{DASHBOARD_BIN}` not found. Build it with:\n  \
         cd dashboard/src-tauri && cargo build --release\n\
         then put it next to `agentpit` (e.g. ~/.local/bin/) or set \
         AGENTPIT_DASHBOARD_BIN=/path/to/{DASHBOARD_BIN}."
    )
}

fn find_on_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|cand| cand.is_file())
}
