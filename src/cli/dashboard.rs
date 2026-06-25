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
    let mut cmd = Command::new(&bin);
    // The dashboard must resolve the SAME state dir (asks/, runs/, events.jsonl) as this CLI, or
    // the Needs-You inbox would read a different `asks/` than the manager writes. `Command`
    // already inherits our environment, so a custom XDG_STATE_HOME carries over — set it
    // explicitly too so the alignment survives any future change to how we spawn. (A GUI/Finder-
    // launched dashboard under a custom XDG_STATE_HOME can still diverge — a known limitation.)
    if let Ok(dir) = std::env::var("XDG_STATE_HOME")
        && !dir.is_empty()
    {
        cmd.env("XDG_STATE_HOME", dir);
    }
    cmd.spawn()
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
