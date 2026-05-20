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
    super::ensemble::run_with_defaults(lines.join("\n"), members, aggregator, cwd, true).await
}
