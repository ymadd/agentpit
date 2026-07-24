//! Desktop-owned update flow backed by the bundled `agentpit` CLI.
//!
//! The CLI already knows the repository release layout and updates the paired desktop binary.
//! Calling that exact implementation from the app keeps one release protocol while moving the
//! user-facing ownership to the desktop application.

use serde::{Deserialize, Serialize};
use tauri::AppHandle;

use crate::cli_runner;

#[derive(Debug, Deserialize)]
struct CliUpdateCheck {
    current_version: String,
    latest_version: String,
    available: bool,
}

#[derive(Debug, Deserialize)]
struct CliUpdateInstall {
    installed_version: String,
    updated: bool,
    dashboard: CliDashboardInstall,
    /// Set when the CLI replaced binaries inside the .app bundle but could not restore the
    /// bundle seal (ad-hoc codesign failed) — the app may not survive a relaunch.
    #[serde(default)]
    resign_error: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CliDashboardInstall {
    status: String,
    #[serde(default)]
    error: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct AppUpdateInfo {
    pub current_version: String,
    pub bundled_cli_version: String,
    pub latest_version: String,
    pub available: bool,
}

#[derive(Debug, Serialize)]
pub struct AppUpdateInstall {
    pub restart_required: bool,
    pub installed_version: String,
    pub output: String,
}

fn parse_check(stdout: &[u8]) -> Result<CliUpdateCheck, String> {
    let text = String::from_utf8_lossy(stdout);
    serde_json::from_str(text.trim())
        .map_err(|error| format!("could not parse bundled CLI update status: {error}"))
}

fn parse_install(stdout: &[u8]) -> Result<CliUpdateInstall, String> {
    let text = String::from_utf8_lossy(stdout);
    serde_json::from_str(text.trim())
        .map_err(|error| format!("could not parse bundled CLI update result: {error}"))
}

fn version_is_newer(latest: &str, current: &str) -> bool {
    fn parts(version: &str) -> (u64, u64, u64) {
        let mut parts = version
            .trim()
            .trim_start_matches('v')
            .split(['.', '-', '+']);
        (
            parts.next().and_then(|part| part.parse().ok()).unwrap_or(0),
            parts.next().and_then(|part| part.parse().ok()).unwrap_or(0),
            parts.next().and_then(|part| part.parse().ok()).unwrap_or(0),
        )
    }
    parts(latest) > parts(current)
}

#[tauri::command]
pub async fn app_update_check(app: AppHandle) -> Result<AppUpdateInfo, String> {
    let args = vec!["update".into(), "--check".into(), "--json".into()];
    let output = cli_runner::run_bundled(&app, &args, None).await?;
    if !output.success {
        return Err(output.failure_message("update check"));
    }
    let check = parse_check(&output.stdout)?;
    let current_version = env!("CARGO_PKG_VERSION");
    Ok(AppUpdateInfo {
        current_version: current_version.into(),
        bundled_cli_version: check.current_version,
        available: check.available || version_is_newer(&check.latest_version, current_version),
        latest_version: check.latest_version,
    })
}

#[tauri::command]
pub async fn app_update_install(app: AppHandle) -> Result<AppUpdateInstall, String> {
    let args = vec!["update".into(), "--json".into()];
    let output = cli_runner::run_bundled(&app, &args, None).await?;
    if !output.success {
        return Err(output.failure_message("application update"));
    }
    let install = parse_install(&output.stdout)?;
    if let Some(error) = &install.resign_error {
        // Surface before offering a restart: relaunching an unsealed bundle is exactly the
        // failure mode the re-sign exists to prevent.
        return Err(format!(
            "更新は取得しましたが、アプリ署名の修復に失敗しました。ターミナルで `codesign --force --deep --sign - /Applications/agentpit.app` を実行してから再起動してください。詳細: {error}"
        ));
    }
    match install.dashboard.status.as_str() {
        "updated" | "up_to_date" => {}
        "failed" => {
            return Err(install
                .dashboard
                .error
                .unwrap_or_else(|| "desktop update failed".into()));
        }
        "not_installed" => {
            return Err("the desktop executable was not found next to the bundled CLI".into());
        }
        status => {
            return Err(format!(
                "bundled CLI returned an unknown desktop status: {status}"
            ))
        }
    }
    Ok(AppUpdateInstall {
        restart_required: install.dashboard.status == "updated",
        installed_version: install.installed_version.clone(),
        output: if install.updated {
            format!(
                "agentpit v{} をインストールしました。",
                install.installed_version
            )
        } else {
            format!("agentpit v{} は最新版です。", install.installed_version)
        },
    })
}

#[tauri::command]
pub fn app_restart(app: AppHandle) {
    app.restart();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_cli_update_contract() {
        let parsed = parse_check(
            br#"{"current_version":"0.1.31","latest_version":"0.2.0","available":true}"#,
        )
        .unwrap();
        assert_eq!(parsed.current_version, "0.1.31");
        assert_eq!(parsed.latest_version, "0.2.0");
        assert!(parsed.available);
    }

    #[test]
    fn desktop_detects_a_release_even_if_the_sidecar_already_updated() {
        assert!(version_is_newer("0.2.0", "0.1.31"));
        assert!(!version_is_newer("0.1.31", "0.1.31"));
        assert!(!version_is_newer("0.1.30", "0.1.31"));
    }

    #[test]
    fn parses_cli_install_contract() {
        let parsed = parse_install(
            br#"{"installed_version":"0.2.0","updated":true,"dashboard":{"status":"updated","path":"/Applications/agentpit.app/Contents/MacOS/agentpit-dashboard","installed_version":"0.2.0"}}"#,
        )
        .unwrap();
        assert_eq!(parsed.installed_version, "0.2.0");
        assert!(parsed.updated);
        assert_eq!(parsed.dashboard.status, "updated");
        assert!(parsed.dashboard.error.is_none());
        // An older CLI without the resign key still parses (resign_error defaults to None);
        // a `"resign_error": null` from the current CLI is also None.
        assert!(parsed.resign_error.is_none());
        let with_resign = parse_install(
            br#"{"installed_version":"0.2.0","updated":true,"dashboard":{"status":"updated"},"resign_error":"codesign failed"}"#,
        )
        .unwrap();
        assert_eq!(with_resign.resign_error.as_deref(), Some("codesign failed"));
    }
}
