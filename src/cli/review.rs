use anyhow::Result;

use crate::types::BackendId;

pub async fn run(
    target: String,
    focus: Option<String>,
    members: Option<Vec<BackendId>>,
    routed: Option<Option<usize>>,
    aggregator: Option<BackendId>,
    cwd: Option<String>,
) -> Result<()> {
    let mut lines = vec![
        "You are reviewing a codebase rooted at the current working directory.".to_string(),
        String::new(),
        format!("Review target: {target}"),
        String::new(),
        "Workflow — MUST follow in order. Do not produce findings without reading.".to_string(),
        "1. Read <target> in full before anything else:".to_string(),
        "   - file path  → read the file".to_string(),
        "   - directory / glob → walk the tree and read every relevant source file".to_string(),
        "   - git reference / \"last commit\" / \"current diff\" → inspect via git".to_string(),
        "2. Read every file the target imports, is imported by, or otherwise depends on. Reviewing without reading the surrounding code is not acceptable.".to_string(),
        "3. Report concrete issues with file:line citations. Speculation, paraphrased findings, or generic advice without a specific cite are not acceptable.".to_string(),
        "4. Categorize each finding as CRITICAL / HIGH / MEDIUM / LOW.".to_string(),
        "5. If a required file or command is genuinely inaccessible, say so explicitly — do not invent findings to fill the gap.".to_string(),
    ];
    if let Some(f) = focus {
        lines.push(String::new());
        lines.push(format!(
            "Focus area (prioritise but do not ignore other issues): {f}"
        ));
    }
    let ctx = super::load_context()?;
    let defaults = ctx.loaded.config.ensemble.review_members.clone();
    let members = members.unwrap_or_else(|| match routed {
        Some(n) => super::ensemble::routed_members(
            &crate::profile::load_profiles(None).unwrap_or_default(),
            crate::profile::TaskCategory::Review,
            &ctx.regs.available(),
            &crate::availability::recently_suspended(),
            n,
            defaults,
        ),
        None => defaults,
    });
    let aggregator = aggregator.or(ctx.loaded.config.ensemble.review_aggregator);
    super::ensemble::run_resolved(
        ctx,
        crate::events::RunKind::Review,
        lines.join("\n"),
        members,
        aggregator,
        None, // model: review has no --model; each member uses its backend default
        routed.is_some(),
        cwd,
    )
    .await
}
