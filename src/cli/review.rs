use anyhow::Result;

use crate::types::BackendId;

pub async fn run(
    target: String,
    focus: Option<String>,
    members: Option<Vec<BackendId>>,
    aggregator: Option<BackendId>,
    cwd: Option<String>,
) -> Result<()> {
    let mut lines = vec![
        format!("Perform a thorough code review of: {target}"),
        "Report concrete issues with file:line citations.".to_string(),
        "Categorize each finding as CRITICAL / HIGH / MEDIUM / LOW.".to_string(),
        "If you cannot access files, say so explicitly.".to_string(),
    ];
    if let Some(f) = focus {
        lines.push(format!("Reviewer focus: {f}."));
    }
    let ctx = super::load_context()?;
    let members =
        members.unwrap_or_else(|| ctx.loaded.config.ensemble.review_members.clone());
    let aggregator = aggregator.or(ctx.loaded.config.ensemble.review_aggregator);
    super::ensemble::run_resolved(ctx, lines.join("\n"), members, aggregator, cwd).await
}
