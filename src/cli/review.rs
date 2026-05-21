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
        "You are reviewing a codebase rooted at the current working directory.".to_string(),
        String::new(),
        format!("Review target: {target}"),
        String::new(),
        "Workflow:".to_string(),
        "1. Resolve <target>:".to_string(),
        "   - file path  → read the file".to_string(),
        "   - directory / glob → walk the tree and read every relevant source file".to_string(),
        "   - git reference / \"last commit\" / \"current diff\" → inspect via git".to_string(),
        "2. Read other files that the target depends on, imports, or is referenced from."
            .to_string(),
        "3. Report concrete issues with file:line citations.".to_string(),
        "4. Categorize each finding as CRITICAL / HIGH / MEDIUM / LOW.".to_string(),
        "5. If you cannot access required files or commands, say so explicitly.".to_string(),
    ];
    if let Some(f) = focus {
        lines.push(String::new());
        lines.push(format!(
            "Focus area (prioritise but do not ignore other issues): {f}"
        ));
    }
    let ctx = super::load_context()?;
    let members =
        members.unwrap_or_else(|| ctx.loaded.config.ensemble.review_members.clone());
    let aggregator = aggregator.or(ctx.loaded.config.ensemble.review_aggregator);
    super::ensemble::run_resolved(ctx, lines.join("\n"), members, aggregator, cwd).await
}
