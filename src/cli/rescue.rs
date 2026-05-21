use anyhow::Result;
use tokio_util::sync::CancellationToken;

use super::{install_ctrlc_cancel, load_context, resolve_cwd, stdout_streamer};
use crate::auth::{
    check_auth, format_auth_failure_message, is_auth_failure, launch_login, launch_terminal_login,
};
use crate::config::RouteKey;
use crate::dispatch::{dispatch, resolve_transport};
use crate::router::{RouteRequest, Router};
use crate::types::BackendId;

pub async fn run(
    task: String,
    backend: Option<BackendId>,
    cwd: Option<String>,
    auto_login: bool,
) -> Result<()> {
    if backend.is_none() {
        let ctx = super::load_context()?;
        let members = ctx.loaded.config.ensemble.rescue_members.clone();
        if !members.is_empty() {
            let aggregator = ctx.loaded.config.ensemble.rescue_aggregator;
            return super::ensemble::run_resolved(ctx, task, members, aggregator, cwd).await;
        }
    }
    run_with_route(task, backend, cwd, auto_login, RouteKey::Rescue).await
}

pub async fn run_with_route(
    task: String,
    backend: Option<BackendId>,
    cwd: Option<String>,
    auto_login: bool,
    route_key: RouteKey,
) -> Result<()> {
    let ctx = load_context()?;
    let available = ctx.regs.available();
    let router = Router::new(ctx.loaded.config.clone(), available.clone());

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
    println!(
        "[backend={} transport={} route={}]",
        backend_id,
        transport,
        decision.reason.as_str()
    );
    let on_chunk = stdout_streamer();

    let result = dispatch(backend_id, &task, &cwd, cancel, on_chunk, &ctx.regs).await;
    match result {
        Ok(res) => {
            if is_auth_failure(&res.output) {
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
            if !res.output.ends_with('\n') {
                println!();
            }
            Ok(())
        }
        Err(err) => {
            let msg = format!("{err:#}");
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
