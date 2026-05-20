use anyhow::Result;

use crate::config::RouteKey;
use crate::types::BackendId;

pub async fn run(
    path: String,
    goal: String,
    backend: Option<BackendId>,
    cwd: Option<String>,
) -> Result<()> {
    let lines = vec![
        format!("Refactor target: {path}"),
        format!("Goal: {goal}"),
        "Plan the change first (what changes, why, in what order).".into(),
        "Then propose the concrete edits as a unified diff if possible.".into(),
        "Do not apply destructive operations without explicit user approval.".into(),
    ];
    super::rescue::run_with_route(lines.join("\n"), backend, cwd, true, RouteKey::Refactor).await
}
