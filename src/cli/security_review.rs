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
        "You are performing a SECURITY review of a codebase rooted at the current working directory.".to_string(),
        String::new(),
        format!("Review target: {target}"),
        String::new(),
        "Workflow — MUST follow in order. Do not produce findings without reading.".to_string(),
        "1. Read <target> in full before anything else:".to_string(),
        "   - file path  → read the file".to_string(),
        "   - directory / glob → walk the tree and read every relevant source file".to_string(),
        "   - git reference / \"last commit\" / \"current diff\" → inspect via git".to_string(),
        "2. Read every file the target imports, is imported by, or otherwise depends on. Reviewing without reading the surrounding code is not acceptable.".to_string(),
        "3. Evaluate against the OWASP-style checklist below. Cite concrete file:line for each finding. Speculation, paraphrased findings, or generic advice without a specific cite are not acceptable.".to_string(),
        "4. Categorize each finding as CRITICAL / HIGH / MEDIUM / LOW and include a one-line remediation.".to_string(),
        "5. If a required file or command is genuinely inaccessible, say so explicitly — do not invent findings to fill the gap.".to_string(),
        String::new(),
        "Security checklist — apply every relevant item:".to_string(),
        "- Injection: command / SQL / shell / template — is untrusted input concatenated into a sink?".to_string(),
        "- AuthN / AuthZ: missing checks, insecure default roles, IDOR, token reuse, session fixation.".to_string(),
        "- Secret handling: hard-coded credentials, secrets in logs, secrets committed to VCS or env-dumped.".to_string(),
        "- Input validation: missing length / type / range checks at trust boundaries.".to_string(),
        "- Path traversal / SSRF / XXE / deserialization of untrusted data.".to_string(),
        "- Crypto: weak algorithms, hard-coded IVs, missing MAC, non-constant-time compare, broken TLS pinning.".to_string(),
        "- Memory / concurrency: TOCTOU, use-after-free, data race, unbounded allocations / DoS vectors.".to_string(),
        "- Supply chain: unpinned deps, malicious typosquats, scripts executed from `curl | sh` style installers.".to_string(),
        "- Error handling: information leakage in error messages, panic-as-DoS in long-running services.".to_string(),
        "- Privacy: PII written to logs, telemetry without opt-out.".to_string(),
    ];
    if let Some(f) = focus {
        lines.push(String::new());
        lines.push(format!(
            "Focus area (prioritise but do not ignore other categories): {f}"
        ));
    }
    let ctx = super::load_context()?;
    let defaults = ctx.loaded.config.ensemble.security_review_members.clone();
    let members = members.unwrap_or_else(|| match routed {
        Some(n) => super::ensemble::routed_members(
            &crate::profile::load_profiles(None)
                .unwrap_or_default()
                .resolved(&crate::profile::Pins::from_config(&ctx.loaded.config)),
            crate::profile::TaskCategory::SecurityReview,
            &ctx.regs.available(),
            &crate::availability::recently_suspended(),
            n,
            defaults,
        ),
        None => defaults,
    });
    let aggregator = aggregator.or(ctx.loaded.config.ensemble.security_review_aggregator);
    super::ensemble::run_resolved(
        ctx,
        crate::events::RunKind::SecurityReview,
        lines.join("\n"),
        members,
        aggregator,
        None, // model: no --model; each member uses its backend default
        None, // effort: same — each member uses its backend default
        routed.is_some(),
        cwd,
    )
    .await
}
