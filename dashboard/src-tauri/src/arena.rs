//! Desktop surface for `agentpit arena` — start a round, judge it blind, read the standings.
//!
//! Every command here is a thin client over the bundled CLI's `--json` output, the same model
//! `workflow_gen` and `learning` use. The arena's rules — how a round is isolated, which pairs
//! are judgeable, how Bradley–Terry is fitted — live once, in the CLI, and are not reimplemented
//! against the same files from a second process.
//!
//! **Identity is withheld by the CLI, not by this UI.** `arena show` omits which backend produced
//! which submission unless `--reveal` is passed, so the webview never holds the names while a
//! comparison is on screen and cannot leak them by accident. Votes are cast by blind label for
//! the same reason: there is no way to express "vote for codex" through this API.

use serde::Deserialize;
use tauri::AppHandle;

use crate::cli_runner;

async fn cli_json(
    app: &AppHandle,
    args: Vec<String>,
    what: &str,
) -> Result<serde_json::Value, String> {
    let output = cli_runner::run(app, &args, None).await?;
    if !output.success {
        return Err(output.failure_message(what));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    serde_json::from_str::<serde_json::Value>(stdout.trim())
        .map_err(|e| format!("could not parse the {what} output: {e}"))
}

#[tauri::command]
pub async fn arena_templates(app: AppHandle) -> Result<serde_json::Value, String> {
    cli_json(
        &app,
        vec!["arena".into(), "templates".into(), "--json".into()],
        "arena templates",
    )
    .await
}

#[tauri::command]
pub async fn arena_rounds(app: AppHandle) -> Result<serde_json::Value, String> {
    cli_json(
        &app,
        vec!["arena".into(), "rounds".into(), "--json".into()],
        "arena rounds",
    )
    .await
}

/// One round's submissions under their blind labels. Never passes `--reveal`: see the module
/// docs — the identities are not the UI's to withhold, they simply never arrive.
#[tauri::command]
pub async fn arena_round(app: AppHandle, round_id: String) -> Result<serde_json::Value, String> {
    cli_json(
        &app,
        vec!["arena".into(), "show".into(), round_id, "--json".into()],
        "arena round",
    )
    .await
}

/// The identities behind a round's labels. Separate from [`arena_round`] on purpose: revealing is
/// an explicit act the UI performs after the voting is done, not a field it happens to receive.
#[tauri::command]
pub async fn arena_reveal(app: AppHandle, round_id: String) -> Result<serde_json::Value, String> {
    cli_json(
        &app,
        vec![
            "arena".into(),
            "show".into(),
            round_id,
            "--reveal".into(),
            "--json".into(),
        ],
        "arena reveal",
    )
    .await
}

#[tauri::command]
pub async fn arena_leaderboard(app: AppHandle) -> Result<serde_json::Value, String> {
    cli_json(
        &app,
        vec!["arena".into(), "leaderboard".into(), "--json".into()],
        "arena leaderboard",
    )
    .await
}

/// What the UI collected from one comparison: two blind labels and which way it went.
#[derive(Debug, Deserialize)]
pub struct VoteRequest {
    pub round_id: String,
    pub winner: String,
    pub loser: String,
    #[serde(default)]
    pub tie: bool,
}

/// A blind label is a single letter. Validated here rather than trusted, because these values
/// reach a child process's argv.
fn label(raw: &str, which: &str) -> Result<String, String> {
    let t = raw.trim();
    match t.len() == 1 && t.chars().all(|c| c.is_ascii_alphabetic()) {
        true => Ok(t.to_ascii_uppercase()),
        false => Err(format!("{which} must be a single blind label, got {raw:?}")),
    }
}

#[tauri::command]
pub async fn arena_vote(app: AppHandle, req: VoteRequest) -> Result<serde_json::Value, String> {
    let winner = label(&req.winner, "winner")?;
    let loser = label(&req.loser, "loser")?;
    if winner == loser {
        return Err("a submission cannot be compared with itself".into());
    }
    let mut args = vec![
        "arena".into(),
        "vote".into(),
        "--round".into(),
        req.round_id,
    ];
    if req.tie {
        args.push("--tie".into());
        args.push(format!("{winner},{loser}"));
    } else {
        args.extend(["--winner".into(), winner, "--loser".into(), loser]);
    }
    let output = cli_runner::run(&app, &args, None).await?;
    if !output.success {
        return Err(output.failure_message("arena vote"));
    }
    // The standings move with every vote, so hand back the fresh ones rather than making the UI
    // ask again and render a stale table in between.
    arena_leaderboard(app).await
}

/// Start a round. Long-running by nature — one full agentic run per contender — so the UI is
/// expected to await this the way it awaits workflow generation.
#[tauri::command]
pub async fn arena_run(
    app: AppHandle,
    task: Option<String>,
    template: Option<String>,
    target: Option<String>,
    contenders: Vec<String>,
    cwd: Option<String>,
) -> Result<serde_json::Value, String> {
    let mut args: Vec<String> = vec!["arena".into(), "run".into()];
    match (
        task.as_deref().map(str::trim).filter(|t| !t.is_empty()),
        template,
    ) {
        (Some(task), _) => args.push(task.to_string()),
        (None, Some(id)) => {
            args.extend(["--template".into(), id]);
            if let Some(t) = target.as_deref().map(str::trim).filter(|t| !t.is_empty()) {
                args.extend(["--target".into(), t.to_string()]);
            }
        }
        (None, None) => return Err("お題を入力するか、プローブを選んでください。".into()),
    }
    if contenders.len() < 2 {
        return Err("対戦させるバックエンドを2つ以上選んでください。".into());
    }
    args.extend(["--contenders".into(), contenders.join(",")]);
    if let Some(cwd) = cwd.as_deref().map(str::trim).filter(|c| !c.is_empty()) {
        args.extend(["--cwd".into(), cwd.to_string()]);
    }

    let output = cli_runner::run(&app, &args, None).await?;
    if !output.success {
        return Err(output.failure_message("arena run"));
    }
    arena_rounds(app).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_are_single_letters_and_normalised() {
        assert_eq!(label("a", "winner").unwrap(), "A");
        assert_eq!(label(" B ", "loser").unwrap(), "B");
    }

    /// These values become argv for a child process, so anything that is not a bare label is
    /// rejected here rather than passed along and argued about downstream.
    #[test]
    fn anything_that_is_not_a_bare_label_is_refused() {
        for bad in ["", "AB", "codex", "1", "--reveal", "A;rm -rf /"] {
            assert!(label(bad, "winner").is_err(), "accepted {bad:?}");
        }
    }
}
