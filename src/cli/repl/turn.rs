use std::time::{Duration, Instant};

use anyhow::Result;
use console::style;
use rustyline::error::ReadlineError;

use super::banner::print_status_line;
use super::commands::{SlashCommand, handle_slash, is_slash_line_in, parse_slash_in};
use super::dispatch_turn::{dispatch_free_text, parse_bang_modifier};
use super::entry::ReplEditor;
use super::state::SessionState;
use crate::types::BackendId;

/// Indicates whether the REPL loop should continue or exit after a turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoopControl {
    Continue,
    Exit,
}

/// What `run_one_turn` does with one trimmed input line, decided once by [`route_line`]
/// so the dispatch loop below and the tests at the bottom of this file share the exact
/// same branching — not a parallel re-derivation of it.
///
/// `SlashCommand` only derives `Debug` (its `/arena`-style variants carry raw argv), so
/// this enum does too; tests match on it with `matches!` / `assert!(matches!(...))`
/// rather than `assert_eq!`.
#[derive(Debug)]
enum LineRoute {
    /// Nothing typed — re-prompt.
    Blank,
    /// A recognized slash command, ready to execute.
    Slash(SlashCommand),
    /// The line started with `/` but was not a runnable command (unknown name, or a
    /// known one missing a required argument). `parse_slash` has already printed why.
    /// §7.2 A4: this must re-prompt, never fall through to a backend dispatch — a typo
    /// is not a billable LLM call.
    RejectedSlash,
    /// `!backend` modifier present but the backend id did not parse. Already reported.
    InvalidBackendModifier,
    /// A bare `!backend` with no task text after it.
    EmptyTask,
    /// Ordinary text (optionally routed to an explicit backend) to dispatch.
    FreeText {
        backend: Option<BackendId>,
        task: String,
    },
}

/// Decide what a trimmed input line means, in the same order `run_one_turn` used to
/// inline: blank check → slash parse → rejected-slash guard → `!backend` modifier.
fn route_line(trimmed: &str) -> LineRoute {
    route_line_in(crate::cli::slash::registry(), trimmed)
}

/// [`route_line`] against a specific resolved registry — the seam the tests drive with a
/// registry built from a `SKILL.md` on disk, mirroring `tui::slash::route_in`. The process
/// registry is filled at startup, so a unit test cannot reach a discovered command through
/// it and would otherwise be testing the built-ins twice.
fn route_line_in(reg: &crate::cli::slash::Registry, trimmed: &str) -> LineRoute {
    if trimmed.is_empty() {
        return LineRoute::Blank;
    }
    if let Some(cmd) = parse_slash_in(reg, trimmed) {
        return LineRoute::Slash(cmd);
    }
    // A line that *looks* like a slash command but did not parse (unknown name, missing
    // argument) has already been explained by `parse_slash`. It must not fall through to
    // a dispatch — §7.2 A4: a typo is not a billable LLM call. A skill whose file was
    // skipped at discovery lands here too: its `/name` is simply not in the registry, so
    // it is refused by the same guard rather than dispatched as the text of the line.
    if is_slash_line_in(reg, trimmed) {
        return LineRoute::RejectedSlash;
    }
    match parse_bang_modifier(trimmed) {
        None => LineRoute::InvalidBackendModifier,
        Some((_, task)) if task.is_empty() => LineRoute::EmptyTask,
        Some((backend, task)) => LineRoute::FreeText { backend, task },
    }
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
    editor: Box<ReplEditor>,
    last_interrupt: Option<Instant>,
) -> Result<(SessionState, Box<ReplEditor>, LoopControl, Option<Instant>)> {
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

            match route_line(trimmed) {
                LineRoute::Blank => Ok((state, editor, LoopControl::Continue, None)),
                LineRoute::Slash(cmd) => {
                    let (new_state, control) = handle_slash(cmd, state).await?;
                    Ok((new_state, editor, control, None))
                }
                LineRoute::RejectedSlash | LineRoute::InvalidBackendModifier => {
                    Ok((state, editor, LoopControl::Continue, None))
                }
                LineRoute::EmptyTask => {
                    // e.g. user typed just `!claude` with no prompt text.
                    eprintln!("No task text after backend modifier — please type a task.");
                    Ok((state, editor, LoopControl::Continue, None))
                }
                LineRoute::FreeText { backend, task } => {
                    let new_state = dispatch_free_text(state, backend, task).await?;
                    Ok((new_state, editor, LoopControl::Continue, None))
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

#[cfg(test)]
mod tests {
    use super::*;

    // These tests call `route_line` — the exact function `run_one_turn`'s match arms
    // dispatch on above, not a re-derivation from `parse_slash`/`is_slash_line`/
    // `parse_bang_modifier` in isolation. Deleting the `is_slash_line` guard inside
    // `route_line` (collapsing `RejectedSlash` into falling through to the
    // `parse_bang_modifier` branch) makes `a_rejected_slash_line_routes_away_from_free_text`
    // fail: `/frobnicate` would route to `FreeText` instead of `RejectedSlash`, because
    // `parse_bang_modifier` treats any non-`!` line — including a rejected slash line — as
    // plain task text (see the assertion on that raw behavior below).

    #[test]
    fn a_rejected_slash_line_routes_away_from_free_text() {
        // Why the guard inside `route_line` exists: `parse_bang_modifier` alone treats a
        // rejected slash line as ordinary task text (§7.2 A4's failure mode)...
        assert_eq!(
            parse_bang_modifier("/frobnicate"),
            Some((None, "/frobnicate".to_string()))
        );
        // ...so `route_line` must intercept it before that point is ever reached.
        assert!(matches!(
            route_line("/frobnicate"),
            LineRoute::RejectedSlash
        ));
        assert!(!matches!(
            route_line("/frobnicate"),
            LineRoute::FreeText { .. }
        ));
    }

    #[test]
    fn a_slash_command_missing_its_argument_also_never_reaches_free_text() {
        assert!(matches!(route_line("/branch"), LineRoute::RejectedSlash));
    }

    /// The REPL end of E4: a `SKILL.md` on disk becomes a line this router turns into a
    /// composed turn, which `handle_slash` hands to `dispatch_free_text` — the very
    /// function the `FreeText` arm below calls. Not a refusal, and not the raw line.
    #[test]
    fn a_discovered_skill_routes_to_a_composed_turn_not_to_a_refusal() {
        let reg = crate::cli::skills::test_registry_from_disk();
        match route_line_in(reg, "/critique the caching plan") {
            LineRoute::Slash(SlashCommand::Skill {
                name,
                provenance,
                prompt,
            }) => {
                assert_eq!(name, "critique");
                // What `handle_slash` prints before it dispatches: the skill, its size,
                // and the file — the turn the user did not type, named before it is sent.
                assert!(
                    provenance.starts_with("[skill /critique — "),
                    "{provenance}"
                );
                assert!(provenance.contains("critique/SKILL.md]"), "{provenance}");
                // What it dispatches: the file's own body plus what was typed after it.
                assert!(prompt.contains(crate::cli::skills::TEST_BODY), "{prompt}");
                assert!(
                    prompt.ends_with("The user's request: the caching plan"),
                    "{prompt}"
                );
            }
            other => panic!("expected the skill's composed turn, got {other:?}"),
        }
        // A bare invocation runs too — `[text]`, not `<text>`.
        assert!(matches!(
            route_line_in(reg, "/critique"),
            LineRoute::Slash(SlashCommand::Skill { .. })
        ));
    }

    /// The other half: nothing about a skill weakens §7.2 A4. A `/name` the registry does
    /// not carry — never written, or written and skipped at discovery — is refused, and in
    /// particular is never handed to `parse_bang_modifier` and dispatched as its own text.
    #[test]
    fn a_skill_the_registry_does_not_carry_is_refused_rather_than_dispatched() {
        let reg = crate::cli::skills::test_registry_from_disk();
        for line in ["/critiquee the caching plan", "/skipped-skill now"] {
            assert!(
                matches!(route_line_in(reg, line), LineRoute::RejectedSlash),
                "{line:?} did not route to RejectedSlash"
            );
            // Which is the point: `parse_bang_modifier` would happily take it as task text.
            assert_eq!(
                parse_bang_modifier(line),
                Some((None, line.to_string())),
                "the guard is what keeps {line:?} off the dispatch"
            );
        }
        // The process registry has had no skills installed, so the line that composes a
        // turn above is refused through `route_line`: discovery is what makes the command.
        assert!(matches!(
            route_line("/critique the caching plan"),
            LineRoute::RejectedSlash
        ));
    }

    #[test]
    fn a_blank_line_routes_to_blank() {
        assert!(matches!(route_line(""), LineRoute::Blank));
    }

    #[test]
    fn a_known_slash_command_routes_to_slash() {
        assert!(matches!(
            route_line("/help"),
            LineRoute::Slash(SlashCommand::Help)
        ));
    }

    #[test]
    fn an_invalid_bang_modifier_routes_to_invalid_bang_modifier() {
        assert!(matches!(
            route_line("!nonexistent-backend hi"),
            LineRoute::InvalidBackendModifier
        ));
    }

    #[test]
    fn a_bare_bang_modifier_with_no_task_text_routes_to_empty_task() {
        assert!(matches!(route_line("!claude"), LineRoute::EmptyTask));
    }

    #[test]
    fn ordinary_text_routes_to_free_text_with_no_explicit_backend() {
        match route_line("hello world") {
            LineRoute::FreeText { backend, task } => {
                assert_eq!(backend, None);
                assert_eq!(task, "hello world");
            }
            other => panic!("expected FreeText, got {other:?}"),
        }
    }

    #[test]
    fn a_bang_modifier_with_task_text_routes_to_free_text_with_that_backend() {
        match route_line("!claude explain this") {
            LineRoute::FreeText { backend, task } => {
                assert_eq!(backend, Some(BackendId::Claude));
                assert_eq!(task, "explain this");
            }
            other => panic!("expected FreeText, got {other:?}"),
        }
    }
}
