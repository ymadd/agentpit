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
        "You are working in a codebase rooted at the current working directory.".to_string(),
        String::new(),
        format!("Refactor target: {path}"),
        format!("Goal: {goal}"),
        String::new(),
        "Workflow — MUST follow. Do not propose a plan without reading.".to_string(),
        "1. Read <path> in full.".to_string(),
        "2. Read every file <path> depends on or is referenced from. Refactoring without checking ripple effects is not acceptable.".to_string(),
        "3. Plan the change first (what changes, why, in what order).".to_string(),
        "4. Propose the concrete edits as a unified diff if possible.".to_string(),
        "5. Do not apply destructive operations without explicit user approval.".to_string(),
    ];
    let prompt = lines.join("\n");

    if backend.is_none() {
        let ctx = super::load_context()?;
        let members = ctx.loaded.config.ensemble.refactor_members.clone();
        if !members.is_empty() {
            let aggregator = ctx.loaded.config.ensemble.refactor_aggregator;
            return super::ensemble::run_resolved(
                ctx,
                crate::events::RunKind::Refactor,
                prompt,
                members,
                aggregator,
                cwd,
            )
            .await;
        }
    }
    super::rescue::run_with_route(prompt, backend, cwd, true, RouteKey::Refactor).await
}
