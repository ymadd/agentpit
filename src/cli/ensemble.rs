use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use anyhow::Result;
use console::style;
use tokio_util::sync::CancellationToken;

use super::common::Context;
use super::{install_ctrlc_cancel, load_context, resolve_cwd};
use crate::auth::check_auth;
use crate::dispatch::{Registries, dispatch, resolve_transport};
use crate::events::{LegStatus, RunKind, RunLogger};
use crate::types::{BackendId, Transport};

pub struct MemberOutcome {
    pub backend: BackendId,
    pub transport: Option<Transport>,
    pub output: Option<String>,
    pub error: Option<String>,
}

pub fn render_concatenated(outcomes: &[MemberOutcome]) -> String {
    let mut sections = Vec::with_capacity(outcomes.len());
    for o in outcomes {
        let transport = o
            .transport
            .map(|t| t.as_str().to_string())
            .unwrap_or_else(|| "skipped".into());
        let header = format!("=== {} (transport={transport}) ===", o.backend);
        let body = if let Some(out) = &o.output {
            out.trim().to_string()
        } else if let Some(err) = &o.error {
            format!("[error] {err}")
        } else {
            "(no output)".to_string()
        };
        sections.push(format!("{header}\n{body}"));
    }
    sections.join("\n\n")
}

/// Per-member cap on how much of each response is embedded in the aggregator prompt. A
/// verbose member could otherwise blow the aggregator's context window (or cost) on its
/// own; the tail is dropped with a marker so the synthesis still sees the bulk of it.
/// Reused by the MCP tool surface ([`crate::mcp`]) to bound each tool result the same way.
pub(crate) const MAX_MEMBER_PROMPT_BYTES: usize = 48 * 1024;

/// Truncate `s` to at most `max` bytes on a char boundary, appending a marker when cut.
pub(crate) fn clamp_for_prompt(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n[truncated: response exceeded {max} bytes]", &s[..end])
}

pub fn build_aggregator_prompt(original: &str, outcomes: &[MemberOutcome]) -> String {
    let mut lines = vec![
        "You are aggregating independent responses from multiple coding agents to the user's original task.".to_string(),
        "Synthesize one best answer. Note disagreements explicitly. Cite each source as [backend].".to_string(),
        String::new(),
        "# Original task".to_string(),
        original.to_string(),
        String::new(),
        "# Responses".to_string(),
    ];
    for o in outcomes {
        if let Some(out) = &o.output {
            lines.push(String::new());
            lines.push(format!("## [{}]", o.backend));
            lines.push(clamp_for_prompt(out.trim(), MAX_MEMBER_PROMPT_BYTES));
        } else if let Some(err) = &o.error {
            lines.push(String::new());
            lines.push(format!("## [{}] (failed)", o.backend));
            lines.push(clamp_for_prompt(err, MAX_MEMBER_PROMPT_BYTES));
        }
    }
    lines.join("\n")
}

/// Run one backend dispatch and map the result into a [`MemberOutcome`]: `not registered` when no
/// transport is wired, an auth-failure marker when the output looks like an auth failure, otherwise
/// the captured output (or the dispatch error). This is the logger-free core shared by
/// [`run_one_member`] (which layers on TTY + event reporting) and the MCP `dispatch_task` /
/// `run_ensemble` tools (which stream nothing) — so the not-registered / auth-failure / error
/// wording lives in exactly one place.
pub(crate) async fn dispatch_to_outcome(
    backend: BackendId,
    prompt: &str,
    cwd: &Path,
    cancel: CancellationToken,
    on_chunk: Arc<dyn Fn(&str) + Send + Sync>,
    regs: &Registries,
    model: Option<&str>,
) -> MemberOutcome {
    let transport = resolve_transport(backend, regs);
    if transport.is_none() {
        return MemberOutcome {
            backend,
            transport: None,
            output: None,
            error: Some("not registered".into()),
        };
    }
    match dispatch(backend, prompt, cwd, cancel, on_chunk, regs, model).await {
        Ok(res) if res.auth_failed => MemberOutcome {
            backend,
            transport: Some(res.transport),
            output: None,
            error: Some("auth failure during execution".into()),
        },
        Ok(res) => MemberOutcome {
            backend,
            transport: Some(res.transport),
            output: Some(res.output),
            error: None,
        },
        Err(err) => MemberOutcome {
            backend,
            transport,
            output: None,
            error: Some(format!("{err:#}")),
        },
    }
}

async fn run_one_member(
    backend: BackendId,
    prompt: String,
    cwd: PathBuf,
    cancel: CancellationToken,
    regs: Arc<Registries>,
    logger: RunLogger,
    model: Option<String>,
) -> MemberOutcome {
    let started = Instant::now();
    let on_chunk = crate::events::output_streamer(logger.run_id(), backend, false);
    let outcome =
        dispatch_to_outcome(backend, &prompt, &cwd, cancel, on_chunk, &regs, model.as_deref()).await;
    let elapsed_s = started.elapsed().as_secs_f32();
    let elapsed_ms = started.elapsed().as_millis() as u64;
    match &outcome.error {
        None => {
            let chars = outcome.output.as_ref().map(String::len).unwrap_or(0);
            report_member_done(backend, elapsed_s, Ok(chars));
            logger.member_finished(backend, false, LegStatus::Ok, elapsed_ms, Some(chars), None);
        }
        Some(err) => {
            report_member_done(backend, elapsed_s, Err(err.as_str()));
            logger.member_finished(
                backend,
                false,
                LegStatus::Error,
                elapsed_ms,
                None,
                Some(err.clone()),
            );
        }
    }
    outcome
}

fn report_member_start(backend: BackendId) {
    eprintln!("{} [{}] running...", style("▶").cyan(), backend);
}

fn report_member_done(backend: BackendId, elapsed_s: f32, result: Result<usize, &str>) {
    match result {
        Ok(chars) => eprintln!(
            "{} [{}] done in {:.1}s ({} chars)",
            style("✓").green(),
            backend,
            elapsed_s,
            chars,
        ),
        Err(reason) => eprintln!(
            "{} [{}] failed in {:.1}s: {}",
            style("✗").red(),
            backend,
            elapsed_s,
            reason,
        ),
    }
}

struct PreflightResult {
    runnable: Vec<BackendId>,
    skipped: Vec<(BackendId, String)>,
}

async fn preflight(members: &[BackendId], regs: &Registries) -> PreflightResult {
    let mut handles = Vec::with_capacity(members.len());
    for m in members {
        let m = *m;
        let registered = resolve_transport(m, regs).is_some();
        handles.push(tokio::spawn(async move {
            if !registered {
                return (m, false, "not registered".to_string());
            }
            let auth = check_auth(m).await;
            (m, auth.ok, auth.hint)
        }));
    }
    let mut runnable = Vec::new();
    let mut skipped = Vec::new();
    for h in handles {
        match h.await {
            Ok((m, true, _)) => runnable.push(m),
            Ok((m, false, hint)) => skipped.push((m, hint)),
            Err(_) => {}
        }
    }
    PreflightResult { runnable, skipped }
}

pub async fn run(
    prompt: String,
    members: Option<Vec<BackendId>>,
    aggregator: Option<BackendId>,
    model: Option<String>,
    cwd: Option<String>,
) -> Result<()> {
    let ctx = load_context()?;
    let members = members.unwrap_or_else(|| ctx.loaded.config.ensemble.default_members.clone());
    let aggregator = aggregator.or(ctx.loaded.config.ensemble.aggregator);
    run_resolved(ctx, RunKind::Ensemble, prompt, members, aggregator, model, cwd).await
}

#[allow(clippy::too_many_arguments)]
pub async fn run_resolved(
    ctx: Context,
    kind: RunKind,
    prompt: String,
    members: Vec<BackendId>,
    aggregator: Option<BackendId>,
    model: Option<String>,
    cwd: Option<String>,
) -> Result<()> {
    let cwd = resolve_cwd(cwd)?;
    let cancel = CancellationToken::new();
    install_ctrlc_cancel(cancel.clone());

    // Per-backend effective model: an explicit `--model` applies to every member/aggregator, else
    // each falls back to its own `[backends.<id>].model` default. Resolved up front (owned Strings)
    // so the spawn closures capture no config borrow.
    let backend_models = ctx.loaded.config.backends.clone();
    let model_for = |b: BackendId| -> Option<String> {
        crate::workflow::roles::resolve_model(
            model.as_deref(),
            None,
            backend_models.get(&b).and_then(|o| o.model.as_deref()),
        )
    };

    let regs = Arc::new(ctx.regs);
    let logger = RunLogger::start(kind, &members, &cwd);

    let pre = preflight(&members, &regs).await;
    if !pre.skipped.is_empty() {
        eprintln!(
            "{} skipping {} member(s) with missing auth or transport:",
            style("⚠").yellow(),
            pre.skipped.len()
        );
        for (m, hint) in &pre.skipped {
            eprintln!("  [{m}] {hint}");
            logger.member_finished(*m, false, LegStatus::Skipped, 0, None, Some(hint.clone()));
        }
    }
    if pre.runnable.is_empty() {
        logger.finished(LegStatus::Error);
        anyhow::bail!("no members are ready (all skipped). Run `agentpit login <backend>` to fix.");
    }
    eprintln!(
        "{} members ready: {}",
        style("→").bold(),
        pre.runnable
            .iter()
            .map(BackendId::to_string)
            .collect::<Vec<_>>()
            .join(", ")
    );

    let mut handles = Vec::new();
    for m in pre.runnable {
        report_member_start(m);
        logger.member_started(m, false);
        let cwd_c = cwd.clone();
        let cancel_c = cancel.clone();
        let regs_c = regs.clone();
        let prompt_c = prompt.clone();
        let logger_c = logger.clone();
        let model_c = model_for(m);
        let handle = tokio::spawn(async move {
            run_one_member(m, prompt_c, cwd_c, cancel_c, regs_c, logger_c, model_c).await
        });
        handles.push((m, handle));
    }

    let mut outcomes: Vec<MemberOutcome> = Vec::with_capacity(handles.len() + pre.skipped.len());
    for (m, hint) in pre.skipped {
        outcomes.push(MemberOutcome {
            backend: m,
            transport: None,
            output: None,
            error: Some(format!("preflight skip — {hint}")),
        });
    }
    for (backend, h) in handles {
        match h.await {
            Ok(outcome) => outcomes.push(outcome),
            Err(join_err) => outcomes.push(MemberOutcome {
                backend,
                transport: None,
                output: None,
                error: Some(format!("task join error: {join_err}")),
            }),
        }
    }

    let member_section = render_concatenated(&outcomes);
    let any_success = outcomes.iter().any(|o| o.output.is_some());

    if let Some(aggregator_id) = aggregator {
        if !any_success {
            println!("{member_section}");
            logger.finished(LegStatus::Error);
            anyhow::bail!("no members succeeded — skipping aggregator");
        }
        let transport = resolve_transport(aggregator_id, &regs);
        if transport.is_none() {
            eprintln!(
                "{} aggregator [{aggregator_id}] skipped: not registered",
                style("⚠").yellow()
            );
            println!(
                "{member_section}\n\n=== aggregator skipped ===\n{aggregator_id} not registered"
            );
            logger.member_finished(
                aggregator_id,
                true,
                LegStatus::Skipped,
                0,
                None,
                Some("not registered".into()),
            );
            logger.finished(LegStatus::Ok);
            return Ok(());
        }
        let auth = check_auth(aggregator_id).await;
        if !auth.ok {
            eprintln!(
                "{} aggregator [{aggregator_id}] skipped: {}",
                style("⚠").yellow(),
                auth.hint
            );
            println!(
                "{member_section}\n\n=== aggregator skipped ===\nauth missing for {aggregator_id}: {}",
                auth.hint
            );
            logger.member_finished(
                aggregator_id,
                true,
                LegStatus::Skipped,
                0,
                None,
                Some(auth.hint.clone()),
            );
            logger.finished(LegStatus::Ok);
            return Ok(());
        }

        report_member_start(aggregator_id);
        logger.member_started(aggregator_id, true);
        let started = Instant::now();
        let agg_prompt = build_aggregator_prompt(&prompt, &outcomes);
        let on_chunk = crate::events::output_streamer(logger.run_id(), aggregator_id, true);
        let agg_model = model_for(aggregator_id);
        match dispatch(
            aggregator_id,
            &agg_prompt,
            &cwd,
            cancel.clone(),
            on_chunk,
            &regs,
            agg_model.as_deref(),
        )
        .await
        {
            Ok(res) => {
                report_member_done(
                    aggregator_id,
                    started.elapsed().as_secs_f32(),
                    Ok(res.output.len()),
                );
                logger.member_finished(
                    aggregator_id,
                    true,
                    LegStatus::Ok,
                    started.elapsed().as_millis() as u64,
                    Some(res.output.len()),
                    None,
                );
                println!(
                    "{member_section}\n\n=== aggregator [{aggregator_id}] (transport={}) ===\n{}",
                    res.transport.as_str(),
                    res.output.trim()
                );
            }
            Err(err) => {
                let msg = format!("{err:#}");
                report_member_done(aggregator_id, started.elapsed().as_secs_f32(), Err(&msg));
                logger.member_finished(
                    aggregator_id,
                    true,
                    LegStatus::Error,
                    started.elapsed().as_millis() as u64,
                    None,
                    Some(msg.clone()),
                );
                println!("{member_section}\n\n=== aggregator failed ===\n{aggregator_id}: {msg}");
                logger.finished(LegStatus::Error);
                anyhow::bail!("aggregator failed");
            }
        }
        logger.finished(LegStatus::Ok);
        return Ok(());
    }

    println!("{member_section}");
    if !any_success {
        logger.finished(LegStatus::Error);
        anyhow::bail!("no members produced output");
    }
    logger.finished(LegStatus::Ok);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> Vec<MemberOutcome> {
        vec![
            MemberOutcome {
                backend: BackendId::Gemini,
                transport: Some(Transport::Exec),
                output: Some("Looks fine.".into()),
                error: None,
            },
            MemberOutcome {
                backend: BackendId::Opencode,
                transport: Some(Transport::Acp),
                output: Some("Found 2 issues.".into()),
                error: None,
            },
            MemberOutcome {
                backend: BackendId::Claude,
                transport: None,
                output: None,
                error: Some("auth missing".into()),
            },
        ]
    }

    #[test]
    fn emits_section_per_outcome() {
        let text = render_concatenated(&fixture());
        assert!(text.contains("=== gemini (transport=exec) ==="));
        assert!(text.contains("Looks fine."));
        assert!(text.contains("=== opencode (transport=acp) ==="));
        assert!(text.contains("Found 2 issues."));
        assert!(text.contains("=== claude (transport=skipped) ==="));
        assert!(text.contains("[error] auth missing"));
    }

    #[test]
    fn aggregator_prompt_includes_responses() {
        let text = build_aggregator_prompt("review src/", &fixture());
        assert!(text.contains("# Original task"));
        assert!(text.contains("review src/"));
        assert!(text.contains("## [gemini]"));
        assert!(text.contains("Looks fine."));
        assert!(text.contains("## [opencode]"));
        assert!(text.contains("Found 2 issues."));
    }

    #[test]
    fn aggregator_prompt_marks_failed_members() {
        let text = build_aggregator_prompt("review src/", &fixture());
        assert!(text.contains("## [claude] (failed)"));
        assert!(text.contains("auth missing"));
    }

    #[test]
    fn clamp_keeps_short_output_verbatim() {
        assert_eq!(clamp_for_prompt("short", 1024), "short");
    }

    #[test]
    fn clamp_truncates_long_output_on_char_boundary() {
        let big = "あ".repeat(40_000); // 120_000 bytes of 3-byte chars
        let out = clamp_for_prompt(&big, MAX_MEMBER_PROMPT_BYTES);
        assert!(out.len() < big.len());
        assert!(out.contains("[truncated:"));
        // Truncation must not split a multibyte char (the prefix stays valid UTF-8).
        assert!(out.starts_with('あ'));
    }

    #[test]
    fn aggregator_prompt_bounds_each_member() {
        let outcomes = vec![MemberOutcome {
            backend: BackendId::Gemini,
            transport: Some(Transport::Exec),
            output: Some("x".repeat(MAX_MEMBER_PROMPT_BYTES * 2)),
            error: None,
        }];
        let text = build_aggregator_prompt("t", &outcomes);
        assert!(text.contains("[truncated:"));
        assert!(text.len() < MAX_MEMBER_PROMPT_BYTES * 2);
    }
}
