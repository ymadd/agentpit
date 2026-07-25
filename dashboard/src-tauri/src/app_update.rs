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
    /// Post-update refresh of the installed Claude Code commands/skills.
    #[serde(default)]
    skills: CliSkillsRefresh,
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
    /// Non-empty when the post-update refresh of the Claude Code commands/skills failed,
    /// so the UI can say the app updated but the skills did not.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skills_error: Option<String>,
}

/// The `skills` object the CLI reports from `update --json` / `init --refresh --json`.
#[derive(Debug, Default, Deserialize)]
struct CliSkillsRefresh {
    #[serde(default)]
    status: String,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    refreshed: usize,
}

/// What `skills_refresh` reports back to the settings screen.
#[derive(Debug, Serialize)]
pub struct SkillsRefreshResult {
    /// Total files rewritten across every detected `.claude/` install.
    pub refreshed: usize,
    /// One entry per detected install; empty means nothing is installed yet.
    pub scopes: Vec<serde_json::Value>,
    /// Human-readable summary in the settings UI's language.
    pub message: String,
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
    let skills_error = (install.skills.status == "failed").then(|| {
        install
            .skills
            .error
            .clone()
            .unwrap_or_else(|| "スキルの更新に失敗しました".into())
    });
    let refreshed = install.skills.refreshed;
    Ok(AppUpdateInstall {
        restart_required: install.dashboard.status == "updated",
        installed_version: install.installed_version.clone(),
        output: if install.updated {
            let skills = match (&skills_error, refreshed) {
                (Some(_), _) => String::new(),
                (None, 0) => String::new(),
                (None, n) => format!(" コマンド／スキル {n} 件も更新しました。"),
            };
            format!(
                "agentpit v{} をインストールしました。{skills}",
                install.installed_version
            )
        } else {
            format!("agentpit v{} は最新版です。", install.installed_version)
        },
        skills_error,
    })
}

/// Re-install this build's Claude Code commands and skills into every detected `.claude/`
/// directory, without waiting for an app update.
///
/// Needed on its own because the update path only refreshes when a new version was actually
/// installed: a user who edited a skill by hand, installed into a second scope, or hit a
/// failed refresh had no way to put the files back from the desktop app.
#[tauri::command]
pub async fn skills_refresh(app: AppHandle) -> Result<SkillsRefreshResult, String> {
    let args = vec!["init".into(), "--refresh".into(), "--json".into()];
    let output = cli_runner::run_bundled(&app, &args, None).await?;
    if !output.success {
        return Err(output.failure_message("skill refresh"));
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let report: serde_json::Value = serde_json::from_str(text.trim())
        .map_err(|error| format!("could not parse the refresh result: {error}"))?;
    let scopes: Vec<serde_json::Value> = report
        .get("scopes")
        .and_then(|s| s.as_array())
        .cloned()
        .unwrap_or_default();
    let refreshed: usize = scopes
        .iter()
        .filter_map(|s| s.get("refreshed").and_then(|v| v.as_u64()))
        .sum::<u64>() as usize;
    let message = if scopes.is_empty() {
        "`.claude/` が見つかりません。ターミナルで `agentpit init` を実行してください。".into()
    } else if refreshed == 0 {
        let total: u64 = scopes
            .iter()
            .filter_map(|s| s.get("total").and_then(|v| v.as_u64()))
            .sum();
        format!("すべて最新です（{total} ファイル）。")
    } else {
        format!("{refreshed} ファイルを更新しました。")
    };
    Ok(SkillsRefreshResult {
        refreshed,
        scopes,
        message,
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

    /// The skills block is additive: a CLI that predates it must still parse (that is the
    /// version actually performing the upgrade to the first build that emits it), and both
    /// the success and failure shapes have to survive.
    #[test]
    fn parses_the_skills_refresh_block_and_tolerates_its_absence() {
        let without = parse_install(
            br#"{"installed_version":"0.2.0","updated":true,"dashboard":{"status":"updated"}}"#,
        )
        .unwrap();
        assert_eq!(without.skills.status, "");
        assert_eq!(without.skills.refreshed, 0);

        let refreshed = parse_install(
            br#"{"installed_version":"0.2.1","updated":true,"dashboard":{"status":"updated"},"skills":{"status":"refreshed","refreshed":4,"scopes":[{"scope":"user","path":"/home/x/.claude","refreshed":4,"total":22}]}}"#,
        )
        .unwrap();
        assert_eq!(refreshed.skills.status, "refreshed");
        assert_eq!(refreshed.skills.refreshed, 4);

        let failed = parse_install(
            br#"{"installed_version":"0.2.1","updated":true,"dashboard":{"status":"updated"},"skills":{"status":"failed","error":"permission denied"}}"#,
        )
        .unwrap();
        assert_eq!(failed.skills.status, "failed");
        assert_eq!(failed.skills.error.as_deref(), Some("permission denied"));
    }
}
