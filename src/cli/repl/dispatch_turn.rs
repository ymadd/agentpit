use std::io::Write;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use console::style;
use tokio_util::sync::CancellationToken;

use super::state::SessionState;
use crate::auth::check_auth;
use crate::config::RouteKey;
use crate::dispatch::{DispatchResult, dispatch, resolve_transport};
use crate::events::{LegStatus, RunKind, RunLogger, output_streamer};
use crate::router::{RouteRequest, Router};
use crate::types::BackendId;

/// Parse an optional inline `@backend` modifier from the start of a free-text turn.
///
/// Returns `(explicit_backend, task_text)`. If the modifier is present but the
/// backend id is invalid, prints an error and returns `None` to signal the caller
/// should re-prompt without dispatching.
pub fn parse_at_modifier(input: &str) -> Option<(Option<BackendId>, String)> {
    let trimmed = input.trim_start();
    if !trimmed.starts_with('@') {
        return Some((None, input.trim().to_string()));
    }
    let rest = trimmed.trim_start_matches('@');
    let (word, remaining) = rest
        .split_once(char::is_whitespace)
        .map(|(w, r)| (w, r.trim_start()))
        .unwrap_or((rest, ""));
    match word.parse::<BackendId>() {
        Ok(id) => Some((Some(id), remaining.to_string())),
        Err(e) => {
            eprintln!("{} unknown backend @{word}: {e}", style("error:").red());
            None
        }
    }
}

/// Drive one free-text turn: route → auth → dispatch (with streaming + event tee).
///
/// Returns the (possibly unchanged) `SessionState`. Errors during dispatch are
/// printed-and-continued rather than propagated, so the REPL loop always gets back
/// a usable state for the next prompt.
pub async fn dispatch_free_text(
    state: SessionState,
    explicit_backend: Option<BackendId>,
    task: String,
) -> Result<SessionState> {
    // Build router from current session state (clone is cheap; HubConfig: Clone).
    let available = state.regs.available();
    // Capability matrix for diagnostic routing; falls back to seeded priors (missing file)
    // or the legacy heuristics (corrupt file) without breaking the turn.
    let profiles = crate::profile::load_profiles(None).unwrap_or_default();
    let router = Router::new(state.config.clone(), available.clone(), profiles)
        .with_suspended(crate::availability::recently_suspended());

    // Honour both the session's active_backend AND any per-turn @modifier.
    let effective_explicit = explicit_backend.or(state.active_backend);

    let decision = router.resolve(&RouteRequest {
        tool: RouteKey::Rescue,
        explicit_backend: effective_explicit,
        task: Some(&task),
    });
    let backend_id = decision.backend;

    if !available.contains(&backend_id) {
        let list: Vec<String> = available.iter().map(|b| b.to_string()).collect();
        eprintln!(
            "{} resolved backend {backend_id} is not available. Available: {}",
            style("error:").red(),
            list.join(", ")
        );
        return Ok(state);
    }

    // Auth check — print hint and re-prompt; never bail in the REPL.
    let auth = check_auth(backend_id).await;
    if !auth.ok {
        eprintln!(
            "{} [{backend_id}] not authenticated. Run `{}` or use /login {backend_id}.",
            style("auth:").yellow(),
            auth.login_command
        );
        return Ok(state);
    }

    // Print route-decision status line before dispatching.
    let transport = resolve_transport(backend_id, &state.regs)
        .map(|t| t.as_str())
        .unwrap_or("none");
    eprintln!(
        "{}",
        style(format!(
            "[→ {backend_id} | {transport} | route={}]",
            decision.reason.as_str()
        ))
        .dim()
    );

    let logger = RunLogger::start(RunKind::Rescue, &[backend_id], &state.cwd);
    decision.log(&logger, &task);
    logger.member_started(backend_id, false);
    let started = Instant::now();

    // Tee output to terminal and to dashboard's capture file.
    // The first chunk arriving clears the working indicator so output starts clean.
    let first_chunk_seen = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let first_chunk_flag = Arc::clone(&first_chunk_seen);
    let to_stdout = crate::cli::stdout_streamer();
    let to_file = output_streamer(logger.run_id(), backend_id, false);
    let on_chunk: Arc<dyn Fn(&str) + Send + Sync> = Arc::new(move |c: &str| {
        // On first chunk, erase the working indicator from stderr before writing output.
        if !first_chunk_flag.swap(true, std::sync::atomic::Ordering::Relaxed) {
            let _ = std::io::stderr().write_all(b"\r\x1b[K");
        }
        to_stdout(c);
        to_file(c);
    });

    // Fresh per-turn cancellation token; cancelled by the tokio::select! ctrl_c branch.
    let cancel = CancellationToken::new();
    let cancel_sig = cancel.clone();

    // Working indicator: print "working… Xs" to stderr every second while dispatch runs.
    // Cancelled when dispatch completes or is Ctrl-C'd.
    let indicator_cancel = CancellationToken::new();
    let indicator_stop = indicator_cancel.clone();
    let indicator_started = started;
    let indicator_seen = Arc::clone(&first_chunk_seen);
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(1));
        interval.tick().await; // skip the immediate first tick
        loop {
            tokio::select! {
                _ = interval.tick() => {
                    // Only show indicator if no output has started streaming yet.
                    if !indicator_seen.load(std::sync::atomic::Ordering::Relaxed) {
                        let elapsed = indicator_started.elapsed().as_secs();
                        let _ = write!(
                            std::io::stderr(),
                            "\r{}",
                            style(format!("working… {elapsed}s")).dim()
                        );
                        let _ = std::io::stderr().flush();
                    }
                }
                _ = indicator_stop.cancelled() => break,
            }
        }
    });

    // Borrow Arc<Registries> without moving state.
    let regs_ref = Arc::clone(&state.regs);
    let cwd = state.cwd.clone();

    let result = tokio::select! {
        res = dispatch(backend_id, &task, &cwd, cancel, on_chunk, &regs_ref, None) => {
            indicator_cancel.cancel();
            // Erase indicator if no streaming output has arrived.
            if !first_chunk_seen.load(std::sync::atomic::Ordering::Relaxed) {
                let _ = std::io::stderr().write_all(b"\r\x1b[K");
            }
            res
        }
        _ = tokio::signal::ctrl_c() => {
            indicator_cancel.cancel();
            cancel_sig.cancel();
            let _ = std::io::stderr().write_all(b"\r\x1b[K");
            eprintln!("{}", style("[cancelled]").yellow());
            // Return a synthetic "cancelled" outcome so the loop continues.
            return Ok(state);
        }
    };

    handle_dispatch_result(result, &logger, backend_id, &auth.login_command, started);

    Ok(state)
}

/// Handle a `Result<DispatchResult>` from `dispatch`. Prints errors/auth hints and
/// logs run bookkeeping. Always returns (never panics, never bails) so the REPL
/// loop can always re-prompt after a turn.
pub(crate) fn handle_dispatch_result(
    result: Result<DispatchResult>,
    logger: &RunLogger,
    backend_id: BackendId,
    auth_login_command: &str,
    started: Instant,
) {
    match result {
        Ok(res) if res.auth_failed => {
            logger.member_finished(
                backend_id,
                false,
                LegStatus::Error,
                started.elapsed().as_millis() as u64,
                None,
                Some("auth failure during execution".into()),
            );
            logger.finished(LegStatus::Error);
            eprintln!(
                "\n{} [{backend_id}] auth failure. Run `{auth_login_command}` to re-authenticate,\n\
                 then try again or use /login {backend_id}.",
                style("auth:").yellow()
            );
            // Re-prompt; do NOT bail.
        }
        Ok(res) => {
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
        }
        Err(e) => {
            logger.member_finished(
                backend_id,
                false,
                LegStatus::Error,
                started.elapsed().as_millis() as u64,
                None,
                Some(format!("{e:#}")),
            );
            logger.finished(LegStatus::Error);
            eprintln!("\n{} [{backend_id}] {e:#}", style("error:").red());
            // Re-prompt; do NOT bail.
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ─── parse_at_modifier ────────────────────────────────────────────────────────

    #[test]
    fn plain_text_has_no_modifier() {
        let result = parse_at_modifier("explain this function");
        assert!(matches!(result, Some((None, ref t)) if t == "explain this function"));
    }

    #[test]
    fn at_prefix_with_valid_backend_parses() {
        let result = parse_at_modifier("@claude fix the bug");
        match result {
            Some((Some(BackendId::Claude), task)) => assert_eq!(task, "fix the bug"),
            other => panic!("expected Some((Some(Claude), text)), got {other:?}"),
        }
    }

    #[test]
    fn at_prefix_with_agy_alias_parses() {
        let result = parse_at_modifier("@agy explain src/lib.rs");
        match result {
            Some((Some(BackendId::Antigravity), task)) => assert_eq!(task, "explain src/lib.rs"),
            other => panic!("expected Some((Some(Antigravity), text)), got {other:?}"),
        }
    }

    #[test]
    fn at_prefix_with_unknown_backend_returns_none() {
        // Unknown @backend: parse_at_modifier prints an error and returns None so the
        // caller can re-prompt without dispatching.
        let result = parse_at_modifier("@nonexistent do the thing");
        assert!(result.is_none());
    }

    #[test]
    fn at_prefix_alone_no_task_text() {
        // Just `@claude` with no trailing text → task is empty.
        let result = parse_at_modifier("@claude");
        match result {
            Some((Some(BackendId::Claude), task)) => assert!(task.is_empty()),
            other => panic!("expected Some((Some(Claude), \"\")), got {other:?}"),
        }
    }

    #[test]
    fn leading_whitespace_before_at_is_accepted() {
        // trim_start is called before the `@` check.
        let result = parse_at_modifier("  @codex refactor this");
        match result {
            Some((Some(BackendId::Codex), task)) => assert_eq!(task, "refactor this"),
            other => panic!("expected Some((Some(Codex), text)), got {other:?}"),
        }
    }

    #[test]
    fn plain_text_with_at_in_middle_is_not_treated_as_modifier() {
        // An `@` that does NOT appear at the start is plain text, passed through as task.
        let result = parse_at_modifier("email me@example.com");
        match result {
            Some((None, task)) => assert_eq!(task, "email me@example.com"),
            other => panic!("expected Some((None, task)), got {other:?}"),
        }
    }
}
