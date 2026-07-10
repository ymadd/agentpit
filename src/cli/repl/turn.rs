use anyhow::Result;
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

/// Drive one full turn: show status line → readline (blocking, off tokio threads) →
/// route to slash or free-text dispatch → return updated state, editor, and
/// loop-control decision.
///
/// Returns `Ok((state, editor, LoopControl::Exit))` on Ctrl-D or `/quit`.
/// Returns `Ok((state, editor, LoopControl::Continue))` in all other cases, including
/// Ctrl-C during readline, cancelled dispatch, and dispatch errors.
/// Returns `Err` only on an unrecoverable runtime error (e.g. `spawn_blocking` panic).
pub async fn run_one_turn(
    state: SessionState,
    editor: Box<DefaultEditor>,
) -> Result<(SessionState, Box<DefaultEditor>, LoopControl)> {
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
                return Ok((state, editor, LoopControl::Continue));
            }

            // Check for slash command.
            if let Some(cmd) = parse_slash(trimmed) {
                let (new_state, control) = handle_slash(cmd, state).await?;
                return Ok((new_state, editor, control));
            }

            // Free-text turn: parse optional @backend modifier then dispatch.
            match parse_at_modifier(trimmed) {
                None => {
                    // Invalid @backend — error already printed; re-prompt.
                    Ok((state, editor, LoopControl::Continue))
                }
                Some((explicit_backend, task)) => {
                    if task.is_empty() {
                        // e.g. user typed just `@claude` with no prompt text.
                        eprintln!("No task text after backend modifier — please type a task.");
                        Ok((state, editor, LoopControl::Continue))
                    } else {
                        let new_state = dispatch_free_text(state, explicit_backend, task).await?;
                        Ok((new_state, editor, LoopControl::Continue))
                    }
                }
            }
        }

        // Ctrl-C during readline: re-prompt.
        Err(ReadlineError::Interrupted) => Ok((state, editor, LoopControl::Continue)),

        // Ctrl-D or piped/closed stdin: clean exit.
        Err(ReadlineError::Eof) => {
            eprintln!("[no TTY or stdin closed — exiting]");
            Ok((state, editor, LoopControl::Exit))
        }

        // Propagate unexpected readline errors.
        Err(e) => Err(anyhow::Error::new(e)),
    }
}
