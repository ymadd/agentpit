//! `workflow_generate` — the dashboard's ✨ generate command.
//!
//! The dashboard crate deliberately does NOT depend on the main `agentpit` crate (same rule as
//! `settings.rs`), so the workflow designer is reached through the bundled CLI sidecar:
//! `agentpit workflow new "<description>" --json`. That keeps a single designer implementation
//! (in the CLI) and returns the structured proposal for the Studio to turn into an editable draft.
//!
//! Arguments are passed directly to the process (never a shell), so shell metacharacters remain
//! inert data.

use tauri::AppHandle;

use crate::cli_runner;

/// Run `agentpit <args…> --json`, optionally piping `stdin` to it, and parse its stdout
/// as JSON. Absorbs the shared spine of every "dashboard command backed by a `agentpit … --json`
/// sidecar call": bundle-aware resolution, status/stderr formatting, and JSON parsing. `what`
/// labels the operation in user-facing errors.
async fn run_cli_json(
    app: &AppHandle,
    args: Vec<String>,
    stdin: Option<&[u8]>,
    what: &str,
) -> Result<serde_json::Value, String> {
    let output = cli_runner::run(app, &args, stdin).await?;
    if !output.success {
        return Err(output.failure_message(what));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    serde_json::from_str::<serde_json::Value>(stdout.trim())
        .map_err(|e| format!("could not parse the {what} output: {e}"))
}

/// Run the designer CLI and parse its JSON proposal from stdout.
async fn run_designer(app: &AppHandle, description: &str) -> Result<serde_json::Value, String> {
    run_cli_json(
        app,
        vec![
            "workflow".into(),
            "new".into(),
            description.into(),
            "--json".into(),
        ],
        None,
        "workflow generation",
    )
    .await
}

/// Generate a workflow proposal from a natural-language `description`. Runs the (slow, LLM-backed)
/// CLI on a blocking thread so the webview stays responsive; the UI awaits the returned proposal.
#[tauri::command]
pub async fn workflow_generate(
    app: AppHandle,
    description: String,
) -> Result<serde_json::Value, String> {
    let description = description.trim().to_string();
    if description.is_empty() {
        return Err("説明を入力してください。".into());
    }
    run_designer(&app, &description).await
}

/// Run `agentpit workflow describe --json` with the workflow `spec` written to the
/// child's STDIN, and parse the `{"description": "..."}` it prints on stdout. Same shell-out model
/// as the designer above — a single CLI implementation, reached over process boundaries.
async fn run_describer(app: &AppHandle, spec: &serde_json::Value) -> Result<String, String> {
    let payload =
        serde_json::to_vec(spec).map_err(|e| format!("could not serialize the workflow: {e}"))?;
    let v = run_cli_json(
        app,
        vec!["workflow".into(), "describe".into(), "--json".into()],
        Some(&payload),
        "description generation",
    )
    .await?;
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
pub async fn workflow_describe(app: AppHandle, spec: serde_json::Value) -> Result<String, String> {
    run_describer(&app, &spec).await
}
