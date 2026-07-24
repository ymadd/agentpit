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

#[cfg(test)]
mod tests {
    use super::*;

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
