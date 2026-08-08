use std::time::{Duration, Instant};

use anyhow::Result;
use console::style;
use rustyline::DefaultEditor;
use rustyline::error::ReadlineError;

use super::banner::print_status_line;
use super::commands::{handle_slash, parse_slash};
use super::dispatch_turn::{dispatch_free_text, parse_at_modifier};
use super::state::SessionState;

/// Indicates whether the REPL loop should continue or exit after a turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoopControl {
    Continue,
    Exit,
}

/// Two-stage Ctrl-C at the prompt (§7.2 A2): the second press within this window exits.
const CTRL_C_EXIT_WINDOW: Duration = Duration::from_secs(2);

/// Drive one full turn: show status line → readline (blocking, off tokio threads) →
/// route to slash or free-text dispatch → return updated state, editor, and
/// loop-control decision.
///
/// `last_interrupt` threads the two-stage Ctrl-C state through the loop: a first Ctrl-C
/// at the prompt prints a hint and arms the window; a second within it exits.
///
/// Returns `Ok((state, editor, control, last_interrupt))`; `Exit` on Ctrl-D, `/quit`, or
/// a double Ctrl-C. Returns `Err` only on an unrecoverable runtime error.
pub async fn run_one_turn(
    state: SessionState,
    editor: Box<DefaultEditor>,
    last_interrupt: Option<Instant>,
) -> Result<(
    SessionState,
    Box<DefaultEditor>,
    LoopControl,
    Option<Instant>,
)> {
    // Print the per-turn status line to stderr before the prompt.
    print_status_line(&state);

    // Move the editor into a blocking task so readline() does not block the tokio runtime.
    let (mut editor, line_res) = tokio::task::spawn_blocking(move || {
        let mut editor = editor; // rebind as mutable inside the closure
        let result = editor.readline("> ");
        (editor, result)
    })
    .await?; // JoinError -> propagate as hard error (should never happen).

    match line_res {
        Ok(line) => {
            // Add non-empty lines to history.
            let trimmed = line.trim();
            if !trimmed.is_empty() {
                let _ = editor.add_history_entry(trimmed);
            }

            if trimmed.is_empty() {
                // Blank line — re-prompt without dispatch.
                return Ok((state, editor, LoopControl::Continue, None));
            }

            // Check for slash command.
            if let Some(cmd) = parse_slash(trimmed) {
                let (new_state, control) = handle_slash(cmd, state).await?;
                return Ok((new_state, editor, control, None));
            }

            // Free-text turn: parse optional @backend modifier then dispatch.
            match parse_at_modifier(trimmed) {
                None => {
                    // Invalid @backend — error already printed; re-prompt.
                    Ok((state, editor, LoopControl::Continue, None))
                }
                Some((explicit_backend, task)) => {
                    if task.is_empty() {
                        // e.g. user typed just `@claude` with no prompt text.
                        eprintln!("No task text after backend modifier — please type a task.");
                        Ok((state, editor, LoopControl::Continue, None))
                    } else {
                        let new_state = dispatch_free_text(state, explicit_backend, task).await?;
                        Ok((new_state, editor, LoopControl::Continue, None))
                    }
                }
            }
        }

        // Ctrl-C at the prompt: two-stage (A2). First press arms a 2s window with a
        // visible hint; a second press inside it exits cleanly.
        Err(ReadlineError::Interrupted) => {
            if let Some(prev) = last_interrupt
                && prev.elapsed() <= CTRL_C_EXIT_WINDOW
            {
                return Ok((state, editor, LoopControl::Exit, None));
            }
            eprintln!("{}", style("(press Ctrl-C again within 2s to exit)").dim());
            Ok((state, editor, LoopControl::Continue, Some(Instant::now())))
        }

        // Ctrl-D or piped/closed stdin: clean exit.
        Err(ReadlineError::Eof) => {
            eprintln!("[no TTY or stdin closed — exiting]");
            Ok((state, editor, LoopControl::Exit, None))
        }

        // Propagate unexpected readline errors.
        Err(e) => Err(anyhow::Error::new(e)),
    }
}
