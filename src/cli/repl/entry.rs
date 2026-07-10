use anyhow::Result;
use rustyline::DefaultEditor;

use super::banner::{flush_stderr, print_banner};
use super::state::SessionState;
use super::turn::{LoopControl, run_one_turn};

/// Launch the persistent conversational REPL.
///
/// Called both when `agentpit` is run with no arguments and via `agentpit repl`.
pub async fn run_repl() -> Result<()> {
    // Load config + build registries once at session start.
    let ctx = crate::cli::load_context()?;
    let cwd = crate::cli::resolve_cwd(None)?;
    let state = SessionState::new(ctx.loaded, ctx.regs, cwd);

    // Initialise rustyline with history.
    let mut editor = Box::new(DefaultEditor::new()?);
    let _ = std::fs::create_dir_all(state.history_file.parent().unwrap_or(&state.history_file));
    let _ = editor.load_history(&state.history_file); // best-effort; missing file is ok

    print_banner();
    flush_stderr();

    // Main REPL loop.
    let mut current_state = state;
    loop {
        let history_file = current_state.history_file.clone();
        let (new_state, new_editor, control) = run_one_turn(current_state, editor).await?;
        current_state = new_state;
        editor = new_editor;

        if control == LoopControl::Exit {
            let _ = editor.save_history(&history_file);
            break;
        }
    }

    Ok(())
}
