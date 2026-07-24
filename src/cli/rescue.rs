use std::time::Instant;

use anyhow::Result;
use tokio_util::sync::CancellationToken;

use super::{install_ctrlc_cancel, load_context, resolve_cwd, stdout_streamer};
use crate::auth::{
    check_auth, format_auth_failure_message, is_auth_failure, launch_login, launch_terminal_login,
};
use crate::config::RouteKey;
use crate::dispatch::{dispatch, resolve_transport};
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
    run_with_role(task, None, backend, None, cwd, auto_login).await
}

/// `agentpit rescue` entry point used by the CLI command dispatcher, with `--role` + `--model`.
/// `model` is the explicit `--model`; it wins over the role's / backend's configured model.
pub async fn run_with_role(
    task: String,
    role: Option<String>,
    backend: Option<BackendId>,
    model: Option<String>,
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
            )
            .await
        }
        DispatchPlan::Direct(backend) => {
            if backend.is_none() {
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
    // explain/refactor/review reach here with no model concept of their own — pass None/None so
    // the backend's `[backends.<id>].model` default still applies inside run_with_route_inner.
    run_with_route_inner(task, backend, cwd, auto_login, route_key, None, None, None).await
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
) -> Result<()> {
    let ctx = load_context()?;
    let available = ctx.regs.available();
    // Machine-generated capability matrix drives diagnostic routing. A missing file yields
    // seeded priors; a corrupt one degrades gracefully to the legacy heuristics rather than
    // breaking routing.
    let profiles = crate::profile::load_profiles(None).unwrap_or_default();
    let router = Router::new(ctx.loaded.config.clone(), available.clone(), profiles);

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
    let logger = RunLogger::start_with_role(kind, &[backend_id], &cwd, role_label);
    decision.log(&logger, &task);
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

    // Effective model: explicit --model > role.model > [backends.<id>].model default > None.
    let effective_model = crate::workflow::roles::resolve_model(
        model.as_deref(),
        role_model.as_deref(),
        ctx.loaded
            .config
            .backends
            .get(&backend_id)
            .and_then(|o| o.model.as_deref()),
    );
    let result = dispatch(
        backend_id,
        &task,
        &cwd,
        cancel,
        on_chunk,
        &ctx.regs,
        effective_model.as_deref(),
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
            if is_auth_failure(&msg) {
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

/// Run the `[cascade].verify` command in `cwd`; `Ok(true)` = passed. No command = passed.
async fn cascade_verify(verify: Option<&str>, cwd: &std::path::Path) -> bool {
    let Some(command) = verify else {
        return true;
    };
    match tokio::process::Command::new("sh")
        .args(["-c", command])
        .current_dir(cwd)
        .status()
        .await
    {
        Ok(status) => status.success(),
        Err(error) => {
            eprintln!("[cascade] verify command failed to launch: {error}");
            false
        }
    }
}

/// `agentpit rescue --cascade`: dispatch to the cheapest qualifying backend and escalate up
/// the ladder on failure. Every hop is its own run in the event log, so a failed hop's
/// RunFinished(error) feeds the learn fold as a negative label with no extra plumbing.
pub async fn run_cascade(task: String, cwd: Option<String>, model: Option<String>) -> Result<()> {
    let ctx = load_context()?;
    let available = ctx.regs.available();
    let profiles = crate::profile::load_profiles(None).unwrap_or_default();
    let cascade_cfg = ctx.loaded.config.cascade.clone();

    let diagnosis = crate::diagnose::diagnose(&task);
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
        return run_with_route_inner(task, None, cwd, true, RouteKey::Rescue, None, model, None)
            .await;
    }

    let resolved_cwd = resolve_cwd(cwd)?;
    let total = ladder.len();
    for (hop, backend_id) in ladder.into_iter().enumerate() {
        println!(
            "[cascade hop {}/{total} backend={backend_id} category={} cost={}]",
            hop + 1,
            diagnosis.primary.as_str(),
            cost_of(backend_id),
        );
        let cancel = CancellationToken::new();
        install_ctrlc_cancel(cancel.clone());
        let logger = RunLogger::start(RunKind::Rescue, &[backend_id], &resolved_cwd);
        logger.route_decided(
            backend_id,
            "cascade",
            Some(diagnosis.primary.as_str()),
            None,
            Some(diagnosis.confidence),
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
        let effective_model = crate::workflow::roles::resolve_model(
            model.as_deref(),
            None,
            ctx.loaded
                .config
                .backends
                .get(&backend_id)
                .and_then(|o| o.model.as_deref()),
        );

        let outcome = dispatch(
            backend_id,
            &task,
            &resolved_cwd,
            cancel,
            on_chunk,
            &ctx.regs,
            effective_model.as_deref(),
        )
        .await;
        let elapsed = started.elapsed().as_millis() as u64;
        let failure: Option<String> = match &outcome {
            Ok(res) if res.auth_failed => Some("auth failure during execution".into()),
            Ok(_) => {
                if cascade_verify(cascade_cfg.verify.as_deref(), &resolved_cwd).await {
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
    anyhow::bail!("cascade exhausted: every hop failed")
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
        };
        let candidates = vec![
            (BackendId::Claude, score(88)),   // cost 80
            (BackendId::Opencode, score(65)), // cost 0
            (BackendId::Gemini, score(75)),   // cost 20
            (BackendId::Codex, score(55)),    // below min_score, out
        ];
        let cost = |b: BackendId| match b {
            BackendId::Opencode => 0,
            BackendId::Gemini => 20,
            BackendId::Claude => 80,
            _ => 50,
        };
        // Cheapest-first ladder, capped at max_hops+1 entries.
        assert_eq!(
            cascade_ladder(&candidates, 60, 2, cost),
            vec![BackendId::Opencode, BackendId::Gemini, BackendId::Claude]
        );
        assert_eq!(
            cascade_ladder(&candidates, 60, 0, cost),
            vec![BackendId::Opencode]
        );
        // Nobody qualifies → empty (caller falls back to the normal route).
        assert!(cascade_ladder(&candidates, 90, 2, cost).is_empty());
    }

    #[tokio::test]
    async fn cascade_verify_maps_exit_status() {
        let dir = std::env::temp_dir();
        assert!(cascade_verify(None, &dir).await);
        assert!(cascade_verify(Some("true"), &dir).await);
        assert!(!cascade_verify(Some("false"), &dir).await);
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
