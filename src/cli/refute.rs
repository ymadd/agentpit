//! `agentpit refute` — the CLI twin of the MCP `refute` tool. Runs one ④ refutation pass: an
//! adversarial critic at a stuck candidate, then a defender carrying that critique, and prints the
//! pair for the manager to adjudicate (the third leg). It is **advisory** — a failed leg is
//! reported in the rendered report, never aborts the command — so a manager can lean on it without
//! risking the workflow. The actual orchestration lives in [`crate::workflow::converse`]; this
//! wrapper only resolves backends, streams progress to stderr, and prints the report to stdout.

use anyhow::Result;
use tokio_util::sync::CancellationToken;

use super::{install_ctrlc_cancel, load_context, resolve_cwd};
use crate::events::{LegStatus, RunKind, RunLogger};
use crate::types::BackendId;
use crate::workflow::converse::{render_refute, resolve_pair, run_refute};

pub async fn run(
    candidate: String,
    task: String,
    critic: Option<BackendId>,
    defender: Option<BackendId>,
    cwd: Option<String>,
) -> Result<()> {
    let ctx = load_context()?;
    let available = ctx.regs.available();
    let preferred = ctx
        .loaded
        .config
        .ensemble
        .adversarial_review_members
        .clone();
    let (critic, defender) = resolve_pair(critic, defender, &available, &preferred)?;

    let cwd = resolve_cwd(cwd)?;
    let cancel = CancellationToken::new();
    install_ctrlc_cancel(cancel.clone());

    eprintln!("[refute] critic={critic} defender={defender} — critique → defense (you adjudicate)");
    if critic == defender {
        eprintln!(
            "[refute] only one backend available; the same backend critiques and defends, which weakens the refutation."
        );
    }

    // Make the refutation visible in the dashboard swarm — a two-member (critic, defender) run,
    // or one member when the single-backend fallback critiques and defends itself.
    let members = if critic == defender {
        vec![critic]
    } else {
        vec![critic, defender]
    };
    let logger = RunLogger::start(RunKind::AdversarialReview, &members, &cwd);
    // The critic leg carries the refutation; log it as the run's routed backend.
    logger.route_decided(critic, "refute", None, None, None, &task);

    let bundle = run_refute(
        &task,
        &candidate,
        critic,
        defender,
        &cwd,
        &ctx.regs,
        cancel,
        Some(&logger),
    )
    .await;
    // Advisory: the run's outcome tracks whether the load-bearing critique leg ran, not the
    // command's exit (which always succeeds).
    logger.finished(if bundle.critique.is_ok() {
        LegStatus::Ok
    } else {
        LegStatus::Error
    });
    if bundle.critique.is_err() {
        eprintln!("[refute] critique leg failed; the report explains why.");
    } else if matches!(bundle.defense, Some(Err(_))) {
        eprintln!("[refute] defense leg failed; the report carries the critique alone.");
    }
    println!("{}", render_refute(&bundle));
    Ok(())
}
