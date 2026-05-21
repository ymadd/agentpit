use anyhow::Result;

use crate::config::RouteKey;
use crate::types::BackendId;

pub async fn run(
    path: String,
    goal: String,
    backend: Option<BackendId>,
    cwd: Option<String>,
) -> Result<()> {
    let lines = [
        format!("Refactor target: {path}"),
        format!("Goal: {goal}"),
        "Plan the change first (what changes, why, in what order).".into(),
        "Then propose the concrete edits as a unified diff if possible.".into(),
        "Do not apply destructive operations without explicit user approval.".into(),
    ];
    let prompt = lines.join("\n");

    if backend.is_none() {
        let ctx = super::load_context()?;
        let members = ctx.loaded.config.ensemble.refactor_members.clone();
        if !members.is_empty() {
            let aggregator = ctx.loaded.config.ensemble.refactor_aggregator;
            return super::ensemble::run_resolved(ctx, prompt, members, aggregator, cwd).await;
        }
    }
    super::rescue::run_with_route(prompt, backend, cwd, true, RouteKey::Refactor).await
}
