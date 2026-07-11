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

/// Blocking: run `agentpit <args…> --json`, optionally piping `stdin` to it, and parse its stdout
/// as JSON. Absorbs the shared spine of every "dashboard command backed by a `agentpit … --json`
/// shell-out": bin resolution, spawn, the optional stdin write (reaping the child on a write
/// failure so it can't zombie), status/stderr error formatting, and the JSON parse. `what` labels
/// the operation in user-facing errors.
fn run_cli_json(args: &[&str], stdin: Option<&[u8]>, what: &str) -> Result<serde_json::Value, String> {
    use std::io::Write;
    let bin = agentpit_bin();
    let mut cmd = Command::new(&bin);
    cmd.args(args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    if stdin.is_some() {
        cmd.stdin(std::process::Stdio::piped());
    }
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("failed to launch {}: {e}", bin.display()))?;
    if let Some(bytes) = stdin {
        // Write then drop the handle so the child sees EOF. On failure (e.g. the child died right
        // after exec → broken pipe), reap it before returning so it can't linger as a zombie
        // (std::process::Child does not wait on Drop).
        let w = child
            .stdin
            .take()
            .ok_or_else(|| "failed to open the child's stdin".to_string())
            .and_then(|mut h| {
                h.write_all(bytes)
                    .map_err(|e| format!("failed to send input to {}: {e}", bin.display()))
            });
        if let Err(e) = w {
            let _ = child.kill();
            let _ = child.wait();
            return Err(e);
        }
    }
    let output = child
        .wait_with_output()
        .map_err(|e| format!("{what} failed: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let msg = stderr.trim();
        return Err(if msg.is_empty() {
            format!("{what} exited with {}", output.status)
        } else {
            format!("{what} failed: {msg}")
        });
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    serde_json::from_str::<serde_json::Value>(stdout.trim())
        .map_err(|e| format!("could not parse the {what} output: {e}"))
}

/// Blocking: run the designer CLI and parse its JSON proposal from stdout.
fn run_designer(description: &str) -> Result<serde_json::Value, String> {
    run_cli_json(&["workflow", "new", description, "--json"], None, "workflow generation")
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

/// Blocking: run `agentpit workflow describe --json` with the workflow `spec` written to the
/// child's STDIN, and parse the `{"description": "..."}` it prints on stdout. Same shell-out model
/// as the designer above — a single CLI implementation, reached over process boundaries.
fn run_describer(spec: &serde_json::Value) -> Result<String, String> {
    let payload =
        serde_json::to_vec(spec).map_err(|e| format!("could not serialize the workflow: {e}"))?;
    let v = run_cli_json(
        &["workflow", "describe", "--json"],
        Some(&payload),
        "description generation",
    )?;
    v.get("description")
        .and_then(|d| d.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .ok_or_else(|| "the describer returned no description".to_string())
}

/// Generate a when-to-use description for the given workflow `spec` (the dashboard's current
/// draft). Runs the slow LLM-backed CLI on a blocking thread; the UI awaits the string.
#[tauri::command]
pub async fn workflow_describe(spec: serde_json::Value) -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || run_describer(&spec))
        .await
        .map_err(|e| format!("describe task failed: {e}"))?
}
