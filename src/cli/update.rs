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
    if !outcome.already_up_to_date {
        refresh_managed_files(json).await;
    }
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
    if let Some(error) = outcome.resign_error {
        eprintln!(
            "warning: could not re-sign the updated app bundle: {error}. \
             macOS may refuse to launch it until you run \
             `codesign --force --deep --sign - /Applications/agentpit.app` (adjust the path)."
        );
    }
    Ok(())
}

async fn refresh_managed_files(quiet: bool) {
    let Ok(self_path) = std::env::current_exe() else {
        eprintln!("warning: could not locate self binary; skipping skill/command refresh");
        return;
    };
    let mut command = Command::new(&self_path);
    command.args(["init", "--refresh"]);
    if quiet {
        command
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
    }
    let result = command.status().await;
    match result {
        Ok(status) if status.success() => {}
        Ok(status) => eprintln!(
            "warning: skill/command refresh exited with {}",
            status
                .code()
                .map(|c| c.to_string())
                .unwrap_or_else(|| "signal".into())
        ),
        Err(err) => eprintln!("warning: failed to launch refresh: {err}"),
    }
}
