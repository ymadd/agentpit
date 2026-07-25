use anyhow::Result;
use tokio::process::Command;
use tokio::task;

use crate::update;

pub async fn run(check_only: bool, json: bool) -> Result<()> {
    if check_only {
        let cache = task::spawn_blocking(update::refresh_cache).await??;
        let current = update::current_version();
        let latest = cache.latest_tag.trim_start_matches('v');
        let available = update::version_is_newer(&cache.latest_tag, current);
        if json {
            println!(
                "{}",
                serde_json::json!({
                    "current_version": current,
                    "latest_version": latest,
                    "available": available,
                })
            );
            return Ok(());
        }
        if available {
            println!("update available: {current} -> {latest} (run `agentpit update`)");
        } else {
            println!("agentpit {current} is up to date (latest: {latest}).");
        }
        return Ok(());
    }

    let outcome = task::spawn_blocking(move || update::perform_update(json)).await??;
    let skills = if outcome.already_up_to_date {
        SkillsRefresh::Skipped
    } else {
        refresh_managed_files(json).await
    };
    if json {
        let dashboard = match &outcome.dashboard {
            update::DashboardUpdateOutcome::NotInstalled => serde_json::json!({
                "status": "not_installed",
            }),
            update::DashboardUpdateOutcome::UpToDate { path } => serde_json::json!({
                "status": "up_to_date",
                "path": path,
            }),
            update::DashboardUpdateOutcome::Updated {
                path,
                installed_version,
            } => serde_json::json!({
                "status": "updated",
                "path": path,
                "installed_version": installed_version,
            }),
            update::DashboardUpdateOutcome::Failed { path, error } => serde_json::json!({
                "status": "failed",
                "path": path,
                "error": error,
            }),
        };
        println!(
            "{}",
            serde_json::json!({
                "installed_version": outcome.installed_version,
                "updated": !outcome.already_up_to_date,
                "dashboard": dashboard,
                "resign_error": outcome.resign_error,
                "skills": skills.to_json(),
            })
        );
        return Ok(());
    }
    if outcome.already_up_to_date {
        println!(
            "agentpit {} is already up to date.",
            outcome.installed_version
        );
    } else {
        println!("agentpit updated to {}.", outcome.installed_version);
    }

    match outcome.dashboard {
        update::DashboardUpdateOutcome::NotInstalled => {
            println!("agentpit dashboard is not installed; skipped dashboard update.");
        }
        update::DashboardUpdateOutcome::UpToDate { path } => {
            println!("agentpit dashboard is up to date ({}).", path.display());
        }
        update::DashboardUpdateOutcome::Updated {
            path,
            installed_version,
        } => {
            println!(
                "agentpit dashboard updated to {} ({}). Restart an open dashboard to use it.",
                installed_version,
                path.display()
            );
        }
        update::DashboardUpdateOutcome::Failed { path, error } => {
            eprintln!(
                "warning: dashboard update failed ({}): {error}. The agentpit CLI remains usable.",
                path.display()
            );
        }
    }
    if let SkillsRefresh::Failed(error) = &skills {
        eprintln!(
            "warning: could not refresh the installed Claude Code commands/skills: {error}. \
             Run `agentpit init --refresh` to retry."
        );
    }
    if let Some(error) = outcome.resign_error {
        eprintln!(
            "warning: could not re-sign the updated app bundle: {error}. \
             macOS may refuse to launch it until you run \
             `codesign --force --deep --sign - /Applications/agentpit.app` (adjust the path)."
        );
    }
    Ok(())
}

/// Outcome of refreshing the installed Claude Code commands/skills after an update.
///
/// Reported rather than discarded: the desktop app runs this path with output suppressed,
/// so a refresh that silently failed left the user on stale skill definitions with nothing
/// on screen to say so.
pub enum SkillsRefresh {
    /// The CLI was already current, so nothing was refreshed.
    Skipped,
    Done(crate::cli::init::RefreshReport),
    Failed(String),
}

impl SkillsRefresh {
    fn to_json(&self) -> serde_json::Value {
        match self {
            SkillsRefresh::Skipped => serde_json::json!({ "status": "skipped" }),
            SkillsRefresh::Done(report) => serde_json::json!({
                "status": "refreshed",
                "refreshed": report.total_refreshed(),
                "scopes": report.scopes,
            }),
            SkillsRefresh::Failed(error) => serde_json::json!({
                "status": "failed",
                "error": error,
            }),
        }
    }
}

/// Re-run `init --refresh` through the *newly installed* binary.
///
/// Deliberately a subprocess rather than a function call: this process is still the old
/// build, and its embedded command/skill files are the old ones. Only the replaced binary
/// carries the definitions that belong with the version just installed.
async fn refresh_managed_files(quiet: bool) -> SkillsRefresh {
    let Ok(self_path) = std::env::current_exe() else {
        return SkillsRefresh::Failed("could not locate the agentpit binary".into());
    };
    let output = Command::new(&self_path)
        .args(["init", "--refresh", "--json"])
        .output()
        .await;
    let output = match output {
        Ok(output) => output,
        Err(err) => return SkillsRefresh::Failed(format!("failed to launch refresh: {err}")),
    };
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return SkillsRefresh::Failed(format!(
            "refresh exited with {}: {}",
            output
                .status
                .code()
                .map(|c| c.to_string())
                .unwrap_or_else(|| "signal".into()),
            stderr.trim()
        ));
    }
    match serde_json::from_slice::<crate::cli::init::RefreshReport>(&output.stdout) {
        Ok(report) => {
            if !quiet {
                for scope in &report.scopes {
                    if scope.refreshed > 0 {
                        println!(
                            "refreshed {} of {} command/skill files in {} scope ({})",
                            scope.refreshed, scope.total, scope.scope, scope.path
                        );
                    }
                }
            }
            SkillsRefresh::Done(report)
        }
        Err(err) => SkillsRefresh::Failed(format!("could not parse refresh output: {err}")),
    }
}
