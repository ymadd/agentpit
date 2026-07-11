//! `workflow_generate` — the dashboard's ✨ generate command.
//!
//! The dashboard crate deliberately does NOT depend on the main `agentpit` crate (same rule as
//! `settings.rs`), so the workflow designer is reached by SHELLING OUT to the CLI:
//! `agentpit workflow new "<description>" --json`. That keeps a single designer implementation
//! (in the CLI) and returns the structured proposal for the Studio to turn into an editable draft.
//!
//! `Command::arg` passes the description straight to `execve` (no shell), so a description with
//! shell metacharacters is inert data, never a command.

use std::path::PathBuf;
use std::process::Command;

/// Resolve the `agentpit` CLI binary: prefer the sibling next to this dashboard executable (the
/// release bundles both together), else fall back to `agentpit` on `PATH`.
fn agentpit_bin() -> PathBuf {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let name = if cfg!(windows) { "agentpit.exe" } else { "agentpit" };
            let sibling = dir.join(name);
            if sibling.is_file() {
                return sibling;
            }
        }
    }
    PathBuf::from("agentpit")
}

/// Blocking: run the designer CLI and parse its JSON proposal from stdout.
fn run_designer(description: &str) -> Result<serde_json::Value, String> {
    let bin = agentpit_bin();
    let output = Command::new(&bin)
        .arg("workflow")
        .arg("new")
        .arg(description)
        .arg("--json")
        .output()
        .map_err(|e| format!("failed to launch {}: {e}", bin.display()))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let msg = stderr.trim();
        return Err(if msg.is_empty() {
            format!("workflow generation exited with {}", output.status)
        } else {
            format!("workflow generation failed: {msg}")
        });
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    serde_json::from_str::<serde_json::Value>(stdout.trim())
        .map_err(|e| format!("could not parse the generated workflow: {e}"))
}

/// Generate a workflow proposal from a natural-language `description`. Runs the (slow, LLM-backed)
/// CLI on a blocking thread so the webview stays responsive; the UI awaits the returned proposal.
#[tauri::command]
pub async fn workflow_generate(description: String) -> Result<serde_json::Value, String> {
    let description = description.trim().to_string();
    if description.is_empty() {
        return Err("説明を入力してください。".into());
    }
    tauri::async_runtime::spawn_blocking(move || run_designer(&description))
        .await
        .map_err(|e| format!("generation task failed: {e}"))?
}
