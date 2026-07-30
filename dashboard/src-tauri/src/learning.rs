//! `learning_status` — the learning view's data source.
//!
//! The aggregation lives in the CLI (`agentpit learning --json`), which owns the profile
//! matrix, the event log and the router. This command only shells out to it and hands the
//! parsed JSON to the frontend, so the desktop app and the terminal always report the same
//! numbers from the same code.

use tauri::AppHandle;

use crate::cli_runner;

#[tauri::command]
pub async fn learning_status(app: AppHandle) -> Result<serde_json::Value, String> {
    let args = vec!["learning".to_string(), "--json".to_string()];
    let output = cli_runner::run(&app, &args, None).await?;
    if !output.success {
        return Err(output.failure_message("agentpit learning"));
    }
    serde_json::from_slice(&output.stdout)
        .map_err(|error| format!("agentpit learning returned unreadable JSON: {error}"))
}
