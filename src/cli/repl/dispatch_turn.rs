use std::io::Write;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use console::style;
use tokio_util::sync::CancellationToken;

use super::state::SessionState;
use crate::auth::check_auth;
use crate::session::ExchangeStatus;
use crate::session::turn_engine::{EngineEvent, TurnEngine, TurnOutcome};
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

/// Drive one free-text turn through the shared [`TurnEngine`]: auth probe → engine
/// (route/record/dispatch/record) → render the outcome. Errors are printed-and-continued
/// so the REPL loop always gets back a usable state.
pub async fn dispatch_free_text(
    state: SessionState,
    explicit_backend: Option<BackendId>,
    task: String,
) -> Result<SessionState> {
    let engine = TurnEngine {
        config: state.config.clone(),
        regs: Arc::clone(&state.regs),
        cwd: state.cwd.clone(),
    };

    // Pre-dispatch auth probe (the engine deliberately doesn't probe, §5.2).
    let backend_id = engine.resolve_backend(state.active_backend, explicit_backend, &task);
    let auth = check_auth(backend_id).await;
    if !auth.ok {
        eprintln!(
            "{} [{backend_id}] not authenticated. Run `{}` or use /login {backend_id}.",
            style("auth:").yellow(),
            auth.login_command
        );
        return Ok(state);
    }

    // Working indicator: "working… Xs" on stderr until the first chunk arrives.
    let started = Instant::now();
    let first_chunk_seen = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let indicator_cancel = CancellationToken::new();
    {
        let stop = indicator_cancel.clone();
        let seen = Arc::clone(&first_chunk_seen);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(1));
            interval.tick().await;
            loop {
                tokio::select! {
                    _ = interval.tick() => {
                        if !seen.load(std::sync::atomic::Ordering::Relaxed) {
                            let elapsed = started.elapsed().as_secs();
                            let _ = write!(
                                std::io::stderr(),
                                "\r{}",
                                style(format!("working… {elapsed}s")).dim()
                            );
                            let _ = std::io::stderr().flush();
                        }
                    }
                    _ = stop.cancelled() => break,
                }
            }
        });
    }

    // Render engine events: the route line, then streamed chunks to stdout.
    let to_stdout = crate::cli::stdout_streamer();
    let first_chunk_flag = Arc::clone(&first_chunk_seen);
    let on_event: Arc<dyn Fn(EngineEvent) + Send + Sync> = Arc::new(move |ev| match ev {
        EngineEvent::Route {
            backend,
            transport,
            reason,
        } => {
            eprintln!(
                "{}",
                style(format!("[→ {backend} | {transport} | route={reason}]")).dim()
            );
        }
        EngineEvent::Chunk { text } => {
            if !first_chunk_flag.swap(true, std::sync::atomic::Ordering::Relaxed) {
                let _ = std::io::stderr().write_all(b"\r\x1b[K");
            }
            to_stdout(&text);
        }
    });

    // Ctrl-C cancels the token; the engine then records the turn as cancelled and returns
    // (the child is killed by the dispatch layer). Awaiting — rather than dropping — the
    // engine future is what lets the cancelled result reach the session log.
    let cancel = CancellationToken::new();
    let ctrlc_cancel = cancel.clone();
    let ctrlc_task = tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            ctrlc_cancel.cancel();
        }
    });

    let outcome = engine
        .run_turn(
            state.recorder.as_ref(),
            state.active_backend,
            explicit_backend,
            &task,
            cancel.clone(),
            on_event,
        )
        .await;
    ctrlc_task.abort();
    indicator_cancel.cancel();
    if !first_chunk_seen.load(std::sync::atomic::Ordering::Relaxed) {
        let _ = std::io::stderr().write_all(b"\r\x1b[K");
    }

    render_outcome(&outcome, &auth.login_command);
    Ok(state)
}

/// Print the turn's ending to the terminal (the engine itself never prints).
fn render_outcome(outcome: &TurnOutcome, login_command: &str) {
    match outcome {
        TurnOutcome::Unavailable { backend, available } => {
            let list: Vec<String> = available.iter().map(|b| b.to_string()).collect();
            eprintln!(
                "{} resolved backend {backend} is not available. Available: {}",
                style("error:").red(),
                list.join(", ")
            );
        }
        TurnOutcome::Completed {
            backend,
            status,
            answer,
        } => match status {
            ExchangeStatus::Ok => {
                if !answer.ends_with('\n') {
                    println!();
                }
            }
            ExchangeStatus::Auth => {
                eprintln!(
                    "\n{} [{backend}] auth failure. Run `{login_command}` to re-authenticate,\n\
                     then try again or use /login {backend}.",
                    style("auth:").yellow()
                );
            }
            ExchangeStatus::Cancelled => {
                eprintln!("{}", style("[cancelled]").yellow());
            }
            ExchangeStatus::Error | ExchangeStatus::Timeout => {
                eprintln!("\n{} [{backend}] {answer}", style("error:").red());
            }
        },
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
