use std::time::Instant;

use anyhow::Result;
use tokio_util::sync::CancellationToken;

use super::{install_ctrlc_cancel, load_context, resolve_cwd, stdout_streamer};
use crate::auth::{
    check_auth, format_auth_failure_message, is_auth_failure_outcome, launch_login,
    launch_terminal_login,
};
use crate::config::RouteKey;
use crate::dispatch::{dispatch, resolve_transport};
use crate::effort::Effort;
use crate::events::{LegStatus, RunKind, RunLogger};
use crate::router::{RouteRequest, Router};
use crate::types::BackendId;

/// Pure dispatch-plan decision for `agentpit rescue`: whether this invocation targets a named
/// `--role` persona or a direct (possibly ensemble-eligible) `--backend`/default dispatch.
/// `--role` and `--backend` together is a hard error — role dispatch is always single-backend
/// (the role itself resolves which backend plays it), so mixing the two would leave it
/// ambiguous which one wins.
#[derive(Debug, Clone, PartialEq)]
enum DispatchPlan {
    Role(String),
    Direct(Option<BackendId>),
}

fn plan_dispatch(role: Option<String>, backend: Option<BackendId>) -> Result<DispatchPlan> {
    match (role, backend) {
        (Some(_), Some(_)) => anyhow::bail!("--role and --backend are mutually exclusive"),
        (Some(r), None) => Ok(DispatchPlan::Role(r)),
        (None, b) => Ok(DispatchPlan::Direct(b)),
    }
}

/// Legacy 4-arg entry point (no `--role` support): kept byte-identical in signature for callers
/// that predate `--role` (e.g. the interactive menu's free-text rescue prompt). Delegates to
/// [`run_with_role`] with `role: None`, which is exactly the old behavior.
pub async fn run(
    task: String,
    backend: Option<BackendId>,
    cwd: Option<String>,
    auto_login: bool,
) -> Result<()> {
    run_with_role(task, None, backend, None, None, cwd, auto_login).await
}

/// `agentpit rescue` entry point used by the CLI command dispatcher, with `--role`, `--model`
/// and `--effort`. `model` / `effort` are the explicit flags; each wins over the role's and the
/// backend's configured value.
#[allow(clippy::too_many_arguments)]
pub async fn run_with_role(
    task: String,
    role: Option<String>,
    backend: Option<BackendId>,
    model: Option<String>,
    effort: Option<Effort>,
    cwd: Option<String>,
    auto_login: bool,
) -> Result<()> {
    match plan_dispatch(role, backend)? {
        DispatchPlan::Role(role_name) => {
            // One context load to resolve the role against configured `[workflow.roles.<name>]`
            // entries and the currently available backends; `run_with_route_inner` (reused
            // unchanged below) loads its own context to resolve auth/transport/router state, the
            // same double-load shape the rescue_members ensemble shortcut below already has.
            let ctx = super::load_context()?;
            let available: Vec<BackendId> = ctx.regs.available().into_iter().collect();
            let resolved = crate::workflow::roles::resolve_role(
                &role_name,
                &ctx.loaded.config.workflow.roles,
                &available,
            )?;
            let wrapped_task = crate::workflow::roles::persona_task(
                &resolved.name,
                resolved.prompt.as_deref(),
                &task,
            );
            // A role dispatch is always single-backend: skip the rescue_members ensemble
            // shortcut entirely and go straight to the resolved backend's explicit route. The
            // role's model is the mid-precedence source (below an explicit --model).
            run_with_route_inner(
                wrapped_task,
                Some(resolved.backend),
                cwd,
                auto_login,
                RouteKey::Rescue,
                Some(resolved.name.as_str()),
                model,
                resolved.model,
                effort,
                resolved.effort,
            )
            .await
        }
        DispatchPlan::Direct(backend) => {
            // The rescue_members ensemble shortcut applies only to a top-level bare rescue.
            // Inside a workflow (depth > 0) a bare `rescue "<task>"` is the manager asking the
            // learned router to pick ONE worker for a sub-task; hijacking it into an N-member
            // ensemble would multiply cost invisibly — the manager fans out explicitly via
            // `ensemble` when it wants that.
            if backend.is_none() && crate::workflow::guard::current_depth() == 0 {
                let ctx = super::load_context()?;
                let members = ctx.loaded.config.ensemble.rescue_members.clone();
                if !members.is_empty() {
                    let aggregator = ctx.loaded.config.ensemble.rescue_aggregator;
                    return super::ensemble::run_resolved(
                        ctx,
                        crate::events::RunKind::Rescue,
                        task,
                        members,
                        aggregator,
                        model,
                        effort,
                        false,
                        cwd,
                    )
                    .await;
                }
            }
            run_with_route_inner(
                task,
                backend,
                cwd,
                auto_login,
                RouteKey::Rescue,
                None,
                model,
                None,
                effort,
                None,
            )
            .await
        }
    }
}

pub async fn run_with_route(
    task: String,
    backend: Option<BackendId>,
    cwd: Option<String>,
    auto_login: bool,
    route_key: RouteKey,
) -> Result<()> {
    // explain/refactor/review reach here with no model or effort concept of their own — pass
    // None so the backend's `[backends.<id>]` defaults still apply inside run_with_route_inner.
    run_with_route_inner(
        task, backend, cwd, auto_login, route_key, None, None, None, None, None,
    )
    .await
}

/// Shared implementation behind [`run_with_route`]. `role_label`, when set, is a resolved
/// `--role` name to surface in the leader line as `route=role:<name>` instead of the router's
/// own reason string — the backend was pinned by role resolution, not the router, so the
/// printed route should say so. `None` keeps the original `route=<reason>` format byte-identical
/// (explain/refactor and the plain `rescue --backend` path all go through this branch).
#[allow(clippy::too_many_arguments)]
async fn run_with_route_inner(
    task: String,
    backend: Option<BackendId>,
    cwd: Option<String>,
    auto_login: bool,
    route_key: RouteKey,
    role_label: Option<&str>,
    model: Option<String>,
    role_model: Option<String>,
    effort: Option<Effort>,
    role_effort: Option<Effort>,
) -> Result<()> {
    let ctx = load_context()?;
    let available = ctx.regs.available();
    // Machine-generated capability matrix drives diagnostic routing. A missing file yields
    // seeded priors; a corrupt one degrades gracefully to the legacy heuristics rather than
    // breaking routing.
    let profiles = crate::profile::load_profiles(None).unwrap_or_default();
    let router = Router::new(ctx.loaded.config.clone(), available.clone(), profiles)
        .with_suspended(crate::availability::recently_suspended());

    let decision = router.resolve(&RouteRequest {
        tool: route_key,
        explicit_backend: backend,
        task: Some(&task),
    });
    let backend_id = decision.backend;

    if !available.contains(&backend_id) {
        let available_list: Vec<String> = available.iter().map(|b| b.to_string()).collect();
        anyhow::bail!(
            "Unsupported backend resolved: {} (route: {}). Available: {}",
            backend_id,
            decision.reason.as_str(),
            available_list.join(", ")
        );
    }

    let auth = check_auth(backend_id).await;
    if !auth.ok {
        if !auto_login {
            anyhow::bail!(
                "[{backend_id}] not authenticated. Run `{}`, or call `agentpit login {backend_id}`.",
                auth.login_command
            );
        }
        let (auth, launch) = launch_terminal_login(auth).await;
        eprintln!("[{backend_id}] is not authenticated.");
        eprintln!("{}", auth.hint);
        eprintln!("Login command: {}", auth.login_command);
        if let Some(lo) = launch {
            eprintln!();
            eprintln!("{}", lo.message);
            if lo.launched {
                eprintln!(
                    "Complete the OAuth flow in the opened Terminal window, then re-run the command."
                );
            }
        }
        anyhow::bail!("auth required for {backend_id}");
    }

    let cwd = resolve_cwd(cwd)?;
    let cancel = CancellationToken::new();
    install_ctrlc_cancel(cancel.clone());

    let transport = resolve_transport(backend_id, &ctx.regs)
        .map(|t| t.as_str())
        .unwrap_or("none");
    match role_label {
        Some(role) => println!("[backend={backend_id} transport={transport} route=role:{role}]"),
        None => println!(
            "[backend={} transport={} route={}]",
            backend_id,
            transport,
            decision.reason.as_str()
        ),
    }
    let kind = match route_key {
        RouteKey::Rescue => RunKind::Rescue,
        RouteKey::Review => RunKind::Review,
        RouteKey::Explain => RunKind::Explain,
        RouteKey::Refactor => RunKind::Refactor,
    };
    // Effective model: explicit --model > role.model > [backends.<id>].model default > None.
    // Resolved before the RouteDecided emit so the telemetry records what actually runs.
    let effective_model = crate::workflow::roles::resolve_model(
        model.as_deref(),
        role_model.as_deref(),
        ctx.loaded
            .config
            .backends
            .get(&backend_id)
            .and_then(|o| o.model.as_deref()),
    );
    // Same precedence one rung over, and recorded clamped so telemetry says what actually ran.
    let effective_effort = crate::effort::resolve_effort(
        effort,
        role_effort,
        ctx.loaded
            .config
            .backends
            .get(&backend_id)
            .and_then(|o| o.effort),
    )
    .map(|e| e.clamp_for(backend_id));

    let logger = RunLogger::start_with_role(kind, &[backend_id], &cwd, role_label);
    decision.log(&logger, &task, effective_model.as_deref(), effective_effort);
    logger.member_started(backend_id, false);
    let started = Instant::now();

    // Tee streamed output to both the terminal and the dashboard's capture file.
    let to_stdout = stdout_streamer();
    let to_file = crate::events::output_streamer(logger.run_id(), backend_id, false);
    let on_chunk: std::sync::Arc<dyn Fn(&str) + Send + Sync> =
        std::sync::Arc::new(move |c: &str| {
            to_stdout(c);
            to_file(c);
        });
    let result = dispatch(
        backend_id,
        &task,
        &cwd,
        cancel,
        on_chunk,
        &ctx.regs,
        effective_model.as_deref(),
        effective_effort,
    )
    .await;
    match result {
        Ok(res) => {
            if res.auth_failed {
                logger.member_finished(
                    backend_id,
                    false,
                    LegStatus::Error,
                    started.elapsed().as_millis() as u64,
                    None,
                    Some("auth failure during execution".into()),
                );
                logger.finished(LegStatus::Error);
                let mut launch_message = None;
                if auto_login {
                    let (_, launch) = launch_login(backend_id).await;
                    launch_message = launch.map(|l| l.message);
                }
                anyhow::bail!(format_auth_failure_message(
                    backend_id,
                    &auth.login_command,
                    launch_message.as_deref()
                ));
            }
            logger.member_finished(
                backend_id,
                false,
                LegStatus::Ok,
                started.elapsed().as_millis() as u64,
                Some(res.output.len()),
                None,
            );
            logger.finished(LegStatus::Ok);
            if !res.output.ends_with('\n') {
                println!();
            }
            Ok(())
        }
        Err(err) => {
            let msg = format!("{err:#}");
            logger.member_finished(
                backend_id,
                false,
                LegStatus::Error,
                started.elapsed().as_millis() as u64,
                None,
                Some(msg.clone()),
            );
            logger.finished(LegStatus::Error);
            // A failed dispatch: the formatted error embeds the backend's stdout/stderr, so
            // scan it the same way the success path does (tail only). Scanning the whole
            // blob with the raw regex bypassed that gate and re-created the false positive
            // for any long output that merely mentions an auth phrase.
            if is_auth_failure_outcome(&msg, Some(false)) {
                let mut launch_message = None;
                if auto_login {
                    let (_, launch) = launch_login(backend_id).await;
                    launch_message = launch.map(|l| l.message);
                }
                anyhow::bail!(format_auth_failure_message(
                    backend_id,
                    &auth.login_command,
                    launch_message.as_deref()
                ));
            }
            Err(err)
        }
    }
}

/// Build the cascade's escalation ladder: available backends whose profile score for
/// `category` clears `min_score`, cheapest first (score-desc breaks cost ties), capped at
/// `max_hops + 1` backends. Pure — tested without any dispatch.
fn cascade_ladder(
    candidates: &[(BackendId, crate::profile::Score)],
    min_score: u8,
    max_hops: u32,
    cost_of: impl Fn(BackendId) -> u8,
) -> Vec<BackendId> {
    let mut qualifying: Vec<(BackendId, u8)> = candidates
        .iter()
        .filter(|(_, score)| score.value >= min_score)
        .map(|(backend, score)| (*backend, score.value))
        .collect();
    qualifying.sort_by(|(a_backend, a_score), (b_backend, b_score)| {
        cost_of(*a_backend)
            .cmp(&cost_of(*b_backend))
            .then(b_score.cmp(a_score))
    });
    qualifying.truncate(max_hops as usize + 1);
    qualifying.into_iter().map(|(backend, _)| backend).collect()
}

/// Cap on a `[cascade].verify` command so a hanging verifier can't stall the cascade forever.
const CASCADE_VERIFY_TIMEOUT_SECS: u64 = 600;

/// Run the `[cascade].verify` command in `cwd`; `Ok(true)` = passed. No command = passed.
/// Bounded by [`CASCADE_VERIFY_TIMEOUT_SECS`] and aborted (child killed) on cancellation —
/// a Ctrl-C during verification must stop the cascade, not hang or escalate past it.
async fn cascade_verify(
    verify: Option<&str>,
    cwd: &std::path::Path,
    cancel: &CancellationToken,
) -> bool {
    let Some(command) = verify else {
        return true;
    };
    let mut child = match tokio::process::Command::new("sh")
        .args(["-c", command])
        .current_dir(cwd)
        .kill_on_drop(true)
        .spawn()
    {
        Ok(child) => child,
        Err(error) => {
            eprintln!("[cascade] verify command failed to launch: {error}");
            return false;
        }
    };
    tokio::select! {
        status = child.wait() => status.map(|s| s.success()).unwrap_or(false),
        _ = cancel.cancelled() => {
            eprintln!("[cascade] verify cancelled");
            false
        }
        _ = tokio::time::sleep(std::time::Duration::from_secs(CASCADE_VERIFY_TIMEOUT_SECS)) => {
            eprintln!("[cascade] verify timed out after {CASCADE_VERIFY_TIMEOUT_SECS}s");
            false
        }
    }
}

/// `agentpit rescue --cascade`: dispatch to the cheapest qualifying backend and escalate up
/// the ladder on failure. Every hop is its own run in the event log, so a failed hop's
/// RunFinished(error) feeds the learn fold as a negative label with no extra plumbing.
/// Auth problems are the exception: an unauthenticated backend is *skipped* (LegStatus::
/// Skipped, which the fold ignores) rather than failed — an expired login says nothing
/// about the backend's capability and must not poison the learned scores.
pub async fn run_cascade(
    task: String,
    cwd: Option<String>,
    model: Option<String>,
    effort: Option<Effort>,
    auto_login: bool,
) -> Result<()> {
    let ctx = load_context()?;
    let available = ctx.regs.available();
    let profiles = crate::profile::load_profiles(None).unwrap_or_default();
    let cascade_cfg = ctx.loaded.config.cascade.clone();

    // Same confidence gate as the router's profile stage: a no-signal task diagnoses to an
    // arbitrary category at ~0.1 confidence, and climbing that category's cost ladder would
    // be routing on noise. Fall through to the normal route instead.
    let diagnosis = crate::diagnose::diagnose(&task);
    if diagnosis.confidence < crate::diagnose::LLM_ASSIST_CONFIDENCE_THRESHOLD {
        eprintln!(
            "[cascade] diagnosis too uncertain ({} at {:.2}) — falling back to the normal route.",
            diagnosis.primary.as_str(),
            diagnosis.confidence
        );
        return run_with_route_inner(
            task,
            None,
            cwd,
            auto_login,
            RouteKey::Rescue,
            None,
            model,
            None,
            effort,
            None,
        )
        .await;
    }

    // Score each backend at the variant the cascade would actually dispatch it as.
    let profiles = profiles.resolved(&crate::profile::Pins::from_config(&ctx.loaded.config));
    let candidates = profiles.candidates_for(diagnosis.primary, &available);
    let cost_of = |b: BackendId| {
        ctx.loaded
            .config
            .backends
            .get(&b)
            .and_then(|o| o.cost)
            .unwrap_or(50)
    };
    let ladder = cascade_ladder(
        &candidates,
        cascade_cfg.min_score,
        cascade_cfg.max_hops,
        cost_of,
    );
    if ladder.is_empty() {
        eprintln!(
            "[cascade] no backend clears min_score={} for {} — falling back to the normal route.",
            cascade_cfg.min_score,
            diagnosis.primary.as_str()
        );
        return run_with_route_inner(
            task,
            None,
            cwd,
            auto_login,
            RouteKey::Rescue,
            None,
            model,
            None,
            effort,
            None,
        )
        .await;
    }

    let resolved_cwd = resolve_cwd(cwd)?;
    // One token + one Ctrl-C handler for the whole cascade (a per-hop install would stack
    // signal listeners); a cancelled cascade aborts instead of escalating.
    let cancel = CancellationToken::new();
    install_ctrlc_cancel(cancel.clone());
    let total = ladder.len();
    for (hop, backend_id) in ladder.into_iter().enumerate() {
        if cancel.is_cancelled() {
            anyhow::bail!("cascade cancelled");
        }
        // Auth preflight, mirroring the normal route: an unauthenticated hop is skipped, not
        // failed (and never negative-labelled).
        let auth = check_auth(backend_id).await;
        if !auth.ok {
            eprintln!(
                "[cascade] hop {}/{total} [{backend_id}] skipped: not authenticated ({})",
                hop + 1,
                auth.login_command
            );
            continue;
        }
        println!(
            "[cascade hop {}/{total} backend={backend_id} category={} cost={}]",
            hop + 1,
            diagnosis.primary.as_str(),
            cost_of(backend_id),
        );
        let effective_model = crate::workflow::roles::resolve_model(
            model.as_deref(),
            None,
            ctx.loaded
                .config
                .backends
                .get(&backend_id)
                .and_then(|o| o.model.as_deref()),
        );
        let effective_effort = crate::effort::resolve_effort(
            effort,
            None,
            ctx.loaded
                .config
                .backends
                .get(&backend_id)
                .and_then(|o| o.effort),
        )
        .map(|e| e.clamp_for(backend_id));
        let logger = RunLogger::start(RunKind::Rescue, &[backend_id], &resolved_cwd);
        logger.route_decided(
            backend_id,
            "cascade",
            Some(diagnosis.primary.as_str()),
            None,
            Some(diagnosis.confidence),
            effective_model.as_deref(),
            effective_effort.map(|e| e.as_str()),
            &task,
        );
        logger.member_started(backend_id, false);
        let started = Instant::now();

        let to_stdout = stdout_streamer();
        let to_file = crate::events::output_streamer(logger.run_id(), backend_id, false);
        let on_chunk: std::sync::Arc<dyn Fn(&str) + Send + Sync> =
            std::sync::Arc::new(move |c: &str| {
                to_stdout(c);
                to_file(c);
            });

        let outcome = dispatch(
            backend_id,
            &task,
            &resolved_cwd,
            cancel.clone(),
            on_chunk,
            &ctx.regs,
            effective_model.as_deref(),
            effective_effort,
        )
        .await;
        let elapsed = started.elapsed().as_millis() as u64;

        // A runtime auth failure is a skip (no capability signal), like the preflight above.
        if let Ok(res) = &outcome
            && res.auth_failed
        {
            eprintln!(
                "[cascade] hop {}/{total} [{backend_id}] skipped: auth failure during execution",
                hop + 1
            );
            logger.member_finished(
                backend_id,
                false,
                LegStatus::Skipped,
                elapsed,
                None,
                Some("auth failure during execution".into()),
            );
            logger.finished(LegStatus::Skipped);
            continue;
        }

        let failure: Option<String> = match &outcome {
            Ok(_) => {
                if cascade_verify(cascade_cfg.verify.as_deref(), &resolved_cwd, &cancel).await {
                    None
                } else {
                    Some(format!(
                        "verification failed: `{}`",
                        cascade_cfg.verify.as_deref().unwrap_or_default()
                    ))
                }
            }
            Err(error) => Some(format!("{error:#}")),
        };

        match failure {
            None => {
                let chars = outcome.as_ref().ok().map(|r| r.output.len());
                logger.member_finished(backend_id, false, LegStatus::Ok, elapsed, chars, None);
                logger.finished(LegStatus::Ok);
                if let Ok(res) = outcome
                    && !res.output.ends_with('\n')
                {
                    println!();
                }
                return Ok(());
            }
            Some(reason) => {
                // A cancelled hop must abort the cascade, never escalate to a pricier
                // backend the human just tried to stop.
                if cancel.is_cancelled() {
                    logger.member_finished(
                        backend_id,
                        false,
                        LegStatus::Skipped,
                        elapsed,
                        None,
                        Some("cancelled".into()),
                    );
                    logger.finished(LegStatus::Skipped);
                    anyhow::bail!("cascade cancelled");
                }
                eprintln!(
                    "[cascade] hop {}/{total} [{backend_id}] failed: {reason}",
                    hop + 1
                );
                logger.member_finished(
                    backend_id,
                    false,
                    LegStatus::Error,
                    elapsed,
                    None,
                    Some(reason),
                );
                logger.finished(LegStatus::Error);
            }
        }
    }
    anyhow::bail!("cascade exhausted: every hop failed or was skipped")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cascade_ladder_orders_qualifying_backends_by_cost() {
        use crate::profile::Score;
        let score = |v: u8| Score {
            value: v,
            samples: 5,
            confidence: 0.6,
            source: crate::profile::ProfileSource::Learned,
        };
        let candidates = vec![
            (BackendId::Claude, score(88)),      // cost 80
            (BackendId::Opencode, score(65)),    // cost 0
            (BackendId::Antigravity, score(75)), // cost 20
            (BackendId::Codex, score(55)),       // below min_score, out
        ];
        let cost = |b: BackendId| match b {
            BackendId::Opencode => 0,
            BackendId::Antigravity => 20,
            BackendId::Claude => 80,
            _ => 50,
        };
        // Cheapest-first ladder, capped at max_hops+1 entries.
        assert_eq!(
            cascade_ladder(&candidates, 60, 2, cost),
            vec![
                BackendId::Opencode,
                BackendId::Antigravity,
                BackendId::Claude
            ]
        );
        assert_eq!(
            cascade_ladder(&candidates, 60, 0, cost),
            vec![BackendId::Opencode]
        );
        // Nobody qualifies → empty (caller falls back to the normal route).
        assert!(cascade_ladder(&candidates, 90, 2, cost).is_empty());
    }

    #[tokio::test]
    async fn cascade_verify_maps_exit_status_and_honors_cancellation() {
        let dir = std::env::temp_dir();
        let live = CancellationToken::new();
        assert!(cascade_verify(None, &dir, &live).await);
        assert!(cascade_verify(Some("true"), &dir, &live).await);
        assert!(!cascade_verify(Some("false"), &dir, &live).await);

        // A cancelled token fails a would-be-hanging verifier promptly instead of waiting.
        let cancelled = CancellationToken::new();
        cancelled.cancel();
        let started = std::time::Instant::now();
        assert!(!cascade_verify(Some("sleep 30"), &dir, &cancelled).await);
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
    }

    #[test]
    fn no_role_no_backend_plans_a_direct_default_dispatch() {
        assert_eq!(
            plan_dispatch(None, None).unwrap(),
            DispatchPlan::Direct(None)
        );
    }

    #[test]
    fn backend_only_plans_a_direct_explicit_dispatch() {
        assert_eq!(
            plan_dispatch(None, Some(BackendId::Codex)).unwrap(),
            DispatchPlan::Direct(Some(BackendId::Codex))
        );
    }

    #[test]
    fn role_only_plans_a_role_dispatch() {
        assert_eq!(
            plan_dispatch(Some("reviewer".to_string()), None).unwrap(),
            DispatchPlan::Role("reviewer".to_string())
        );
    }

    #[test]
    fn role_and_backend_together_is_a_hard_error() {
        let err = plan_dispatch(Some("reviewer".to_string()), Some(BackendId::Codex))
            .unwrap_err()
            .to_string();
        assert_eq!(err, "--role and --backend are mutually exclusive");
    }
}
