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
        "You are an ADVERSARIAL reviewer of a codebase rooted at the current working directory.".to_string(),
        String::new(),
        format!("Adversarial review target: {target}"),
        String::new(),
        "Your job is to find what is wrong, not to be balanced. Assume the code is broken until you have proven otherwise; default to skepticism, not charity. The author needs to see what they missed, not be reassured.".to_string(),
        String::new(),
        "Workflow — MUST follow in order. Do not produce findings without reading.".to_string(),
        "1. Read <target> in full first:".to_string(),
        "   - file path  → read the file".to_string(),
        "   - directory / glob → walk the tree and read every relevant source file".to_string(),
        "   - git reference / \"last commit\" / \"current diff\" → inspect via git".to_string(),
        "   Then read every file the target imports, is imported by, or otherwise depends on. Speculation without reading the surrounding code is not acceptable.".to_string(),
        "2. For every claim the code makes (in names, comments, types, error messages), find a concrete scenario where the claim is false. Cite file:line.".to_string(),
        "3. For every input, ask: what is the worst LEGAL value? what is the worst ILLEGAL one? does the code handle both?".to_string(),
        "4. For every external dependency (filesystem, network, time, env, RNG, child process, allocator), assume it WILL fail, be slow, return adversarial bytes, or be interrupted. Trace what happens and where state is left.".to_string(),
        "5. For every invariant the code assumes (ordering, exclusivity, idempotency, atomicity, \"X is always Y\"), construct an execution that breaks it — concurrency, retries, partial failures, signals, time skew.".to_string(),
        "6. For every abstraction, ask: is this premature? is it under-engineered for the known scale? does it leak its own implementation through its API?".to_string(),
        "7. Categorize each finding as CRITICAL / HIGH / MEDIUM / LOW with a concrete reproducer or trace (steps / inputs / sequence) — not \"could potentially…\". Include a one-line fix suggestion.".to_string(),
        String::new(),
        "Adversarial checklist — apply every relevant item:".to_string(),
        "- Error paths: which error branches were never exercised by the author? what invariant do they leave broken?".to_string(),
        "- Concurrency: data races, lock ordering, TOCTOU, partial writes, abandoned in-flight state on cancel/crash.".to_string(),
        "- Resource limits: unbounded buffers / queues / retries; leaks of FDs / threads / connections / allocations; DoS via large or pathological input.".to_string(),
        "- Off-by-one and arithmetic: overflow, underflow, signedness, off-by-one in ranges / slices / loop bounds.".to_string(),
        "- Wrong defaults: insecure, lossy, or surprising defaults; values that work in the happy path and lie in others.".to_string(),
        "- State after failure: partial writes, half-applied mutations, dirty caches, orphaned temp files; what does retry see?".to_string(),
        "- API contracts: caller assumes X, callee provides X-only-usually. Spec vs. implementation drift.".to_string(),
        "- Tests: what does the test suite actively avoid testing? Mocked-away failure modes, golden files no one reads, assertions on noise.".to_string(),
        "- Naming lies: function name claims X, body does Y. Misnomers that route review attention to the wrong place.".to_string(),
        "- Performance pitfalls: hidden O(n²), per-iteration allocation, syscalls in hot loops, locks held across I/O.".to_string(),
        "- Dead or dangerous code: unreachable branches that look reachable, \"TODO: handle X\" left unhandled, error swallowing.".to_string(),
        String::new(),
        "Negative results count: if a section is clean after honest scrutiny, say so explicitly AND name the evidence that convinced you. Do NOT invent findings to fill the report. Do NOT soften language to be polite — say \"this WILL break when <scenario>\", not \"might\". If a required file or command is genuinely inaccessible, say so explicitly — do not paper over the gap.".to_string(),
    ];
    if let Some(f) = focus {
        lines.push(String::new());
        lines.push(format!(
            "Focus area (attack this first, but do not ignore the other categories): {f}"
        ));
    }
    let ctx = super::load_context()?;
    let defaults = ctx
        .loaded
        .config
        .ensemble
        .adversarial_review_members
        .clone();
    let members = members.unwrap_or_else(|| match routed {
        Some(n) => super::ensemble::routed_members(
            &crate::profile::load_profiles(None)
                .unwrap_or_default()
                .resolved(&crate::profile::Pins::from_config(&ctx.loaded.config)),
            crate::profile::TaskCategory::AdversarialReview,
            &ctx.regs.available(),
            &crate::availability::recently_suspended(),
            n,
            defaults,
        ),
        None => defaults,
    });
    let aggregator = aggregator.or(ctx.loaded.config.ensemble.adversarial_review_aggregator);
    super::ensemble::run_resolved(
        ctx,
        crate::events::RunKind::AdversarialReview,
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
