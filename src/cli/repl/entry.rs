use std::sync::{Arc, Mutex};

use anyhow::Result;
use console::style;
use rustyline::DefaultEditor;

use super::banner::{flush_stderr, print_banner};
use super::state::SessionState;
use super::turn::{LoopControl, run_one_turn};
use crate::session::SessionRecorder;

/// Launch the persistent conversational REPL.
///
/// Called both when `agentpit` is run with no arguments and via `agentpit repl`.
/// `resume` reopens an existing session by (partial) id instead of creating a new one.
pub async fn run_repl(resume: Option<String>) -> Result<()> {
    // Load config + build registries once at session start.
    let ctx = crate::cli::load_context()?;
    let cwd = crate::cli::resolve_cwd(None)?;
    let mut state = SessionState::new(ctx.loaded, ctx.regs, cwd.clone());

    // Open the durable session log. A resume failure is a hard error (the user asked for
    // that session); a CREATE failure only degrades to a non-persisted REPL (Q2).
    match &resume {
        Some(needle) => {
            let mut recorder = SessionRecorder::resume(needle)?;
            for w in recorder.warnings() {
                eprintln!("{} {w}", style("session:").yellow());
            }
            for note in recorder.mark_interrupted() {
                eprintln!("{} {note}", style("session:").yellow());
            }
            let n_turns = recorder.context_items().len();
            eprintln!(
                "{}",
                style(format!(
                    "[resumed session {} — {n_turns} context entries]",
                    recorder.short_id()
                ))
                .dim()
            );
            state.recorder = Some(Arc::new(Mutex::new(recorder)));
        }
        None => match SessionRecorder::create(&cwd) {
            Ok(recorder) => {
                state.recorder = Some(Arc::new(Mutex::new(recorder)));
            }
            Err(e) => {
                eprintln!(
                    "{} could not create the session log ({e:#}); this REPL will not be resumable",
                    style("session:").yellow()
                );
            }
        },
    }

    // Initialise rustyline with history.
    let mut editor = Box::new(DefaultEditor::new()?);
    let _ = std::fs::create_dir_all(state.history_file.parent().unwrap_or(&state.history_file));
    let _ = editor.load_history(&state.history_file); // best-effort; missing file is ok

    print_banner();
    flush_stderr();

    // Main REPL loop.
    let mut current_state = state;
    let mut last_interrupt: Option<std::time::Instant> = None;
    loop {
        let history_file = current_state.history_file.clone();
        let (new_state, new_editor, control, interrupt) =
            run_one_turn(current_state, editor, last_interrupt).await?;
        current_state = new_state;
        editor = new_editor;
        last_interrupt = interrupt;

        if control == LoopControl::Exit {
            let _ = editor.save_history(&history_file);
            if let Some(recorder) = &current_state.recorder
                && let Ok(rec) = recorder.lock()
            {
                eprintln!(
                    "{}",
                    style(format!(
                        "[session saved — {}]",
                        crate::cli::guidance::resume_hint(&rec.short_id())
                    ))
                    .dim()
                );
            }
            break;
        }
    }

    Ok(())
}
