use anyhow::Result;
use console::style;

use super::dispatch_turn::dispatch_free_text;
use super::state::SessionState;
use super::turn::LoopControl;
use crate::cli::slash::{self, Parsed, Surface};
use crate::types::BackendId;

/// A parsed slash command. Declared once, in the shared registry, so every surface
/// speaks the same vocabulary.
pub use crate::cli::slash::SlashCommand;

/// Parse a line that starts with `/` into a `SlashCommand`, returning `None` if the line
/// is not a slash command — or is one the REPL rejects.
///
/// Names, aliases, and argument handling all come from [`crate::cli::slash::COMMANDS`];
/// this function only renders the outcome. A `None` for a line that *does* start with `/`
/// means "rejected, already explained" — the caller must re-prompt rather than fall
/// through to a dispatch (§7.2 A4: a typo must not become a billable LLM call).
pub fn parse_slash(input: &str) -> Option<SlashCommand> {
    parse_slash_in(slash::registry(), input)
}

/// [`parse_slash`] against a specific resolved registry — the seam the tests drive with a
/// registry built from a `SKILL.md` on disk, since the process one is filled at startup by
/// the entry point that knows the session's cwd.
pub fn parse_slash_in(reg: &slash::Registry, input: &str) -> Option<SlashCommand> {
    match reg.parse(input, Surface::Repl) {
        Parsed::NotSlash => None,
        Parsed::Command(cmd) => Some(cmd),
        Parsed::Usage(usage) => {
            eprintln!("{}", style(usage).yellow());
            None
        }
        Parsed::Unknown { typed, suggestion } => {
            let suggestion = suggestion.map(|s| format!(" {s}")).unwrap_or_default();
            eprintln!(
                "{}",
                style(format!(
                    "Unknown command /{typed}.{suggestion} Type /help for available commands."
                ))
                .yellow()
            );
            None
        }
    }
}

/// Was this line typed as a slash command?
///
/// Pairs with [`parse_slash`]: when that returns `None` for a line this says `true` of,
/// the command was rejected (unknown, or missing a required argument) and the caller
/// re-prompts. Free text is everything else.
pub fn is_slash_line(input: &str) -> bool {
    is_slash_line_in(slash::registry(), input)
}

/// [`is_slash_line`] against a specific resolved registry — the other half of the
/// [`parse_slash_in`] seam, so a test can route a line the way the REPL will.
pub fn is_slash_line_in(reg: &slash::Registry, input: &str) -> bool {
    matches!(
        reg.parse(input, Surface::Repl),
        Parsed::Command(_) | Parsed::Usage(_) | Parsed::Unknown { .. }
    )
}

/// Execute a slash command. Returns updated `SessionState` and a `LoopControl`.
pub async fn handle_slash(
    cmd: SlashCommand,
    state: SessionState,
) -> Result<(SessionState, LoopControl)> {
    match cmd {
        SlashCommand::Help => {
            print_help();
            Ok((state, LoopControl::Continue))
        }

        SlashCommand::BackendShow => {
            let backend = state.active_backend.unwrap_or(state.config.default.backend);
            let transport = crate::dispatch::resolve_transport(backend, &state.regs)
                .map(|t| t.as_str())
                .unwrap_or("none");
            println!("active backend: {backend} ({transport})");
            Ok((state, LoopControl::Continue))
        }

        SlashCommand::BackendSet(id_str) => match id_str.parse::<BackendId>() {
            Ok(id) => {
                let from = state.active_backend.unwrap_or(state.config.default.backend);
                if from != id
                    && let Some(recorder) = &state.recorder
                    && let Ok(mut rec) = recorder.lock()
                {
                    let _ = rec.record_switch(from, id);
                }
                println!("backend set to {id}");
                Ok((state.with_backend(Some(id)), LoopControl::Continue))
            }
            Err(e) => {
                eprintln!(
                    "{} {e}\nValid backends: {}",
                    style("error:").red(),
                    valid_backends_list()
                );
                Ok((state, LoopControl::Continue))
            }
        },

        SlashCommand::Status => {
            // Ignore any error from the status subcommand — don't abort the REPL.
            let _ = crate::cli::status::run(None).await;
            Ok((state, LoopControl::Continue))
        }

        SlashCommand::Config => {
            let _ = crate::cli::menu::run_config().await;
            Ok((state, LoopControl::Continue))
        }

        SlashCommand::Menu => {
            let _ = crate::cli::menu::run_main().await;
            Ok((state, LoopControl::Continue))
        }

        SlashCommand::Ensemble(prompt) => {
            if prompt.is_empty() {
                eprintln!("{}", style("usage: /ensemble <prompt>").yellow());
            } else {
                let cwd_str = state.cwd.display().to_string();
                let _ =
                    crate::cli::ensemble::run(prompt, None, None, None, None, Some(cwd_str)).await;
            }
            Ok((state, LoopControl::Continue))
        }

        SlashCommand::Review(target) => {
            if target.is_empty() {
                eprintln!("{}", style("usage: /review <target>").yellow());
            } else {
                let cwd_str = state.cwd.display().to_string();
                let _ =
                    crate::cli::review::run(target, None, None, None, None, Some(cwd_str)).await;
            }
            Ok((state, LoopControl::Continue))
        }

        SlashCommand::Workflow(goal) => {
            if goal.is_empty() {
                eprintln!("{}", style("usage: /workflow <goal>").yellow());
            } else {
                let cwd_str = state.cwd.display().to_string();
                let _ = crate::cli::workflow::run(
                    goal,
                    None,
                    None,
                    None,
                    None,
                    false,
                    None,
                    None,
                    Some(cwd_str),
                )
                .await;
            }
            Ok((state, LoopControl::Continue))
        }

        // Unreachable: the registry row is `Surface::Tui` only, so `parse` never hands this
        // client a cell. Spelled out rather than swept into a `_` arm so that a command
        // added to the table still fails to compile here until it is given a meaning.
        SlashCommand::ReplCell(_) => {
            eprintln!(
                "{}",
                style("cells run in the TUI or in `agentpit orchestrate` — not here").yellow()
            );
            Ok((state, LoopControl::Continue))
        }

        // ── the CLI subcommands, run in-process against this session's cwd ───────────
        // Each arm calls the same implementation `agentpit <name>` calls, with the flags
        // the slash form does not expose left at their CLI defaults.
        SlashCommand::Rescue(task) => {
            let cwd = Some(state.cwd.display().to_string());
            // Mirror `Command::Rescue`: a configured `[default] cascade = true` changes
            // what a bare `agentpit rescue` does, so it must change /rescue too.
            let result = if state.config.default.cascade {
                crate::cli::rescue::run_cascade(task, cwd, None, None, true).await
            } else {
                crate::cli::rescue::run(task, None, cwd, true).await
            };
            report(result);
            Ok((state, LoopControl::Continue))
        }

        SlashCommand::Refactor { path, goal } => {
            let cwd = Some(state.cwd.display().to_string());
            report(crate::cli::refactor::run(path, goal, None, cwd).await);
            Ok((state, LoopControl::Continue))
        }

        SlashCommand::Explain(target) => {
            let cwd = Some(state.cwd.display().to_string());
            report(crate::cli::explain::run(target, false, None, cwd).await);
            Ok((state, LoopControl::Continue))
        }

        SlashCommand::SecurityReview(target) => {
            let cwd = Some(state.cwd.display().to_string());
            report(crate::cli::security_review::run(target, None, None, None, None, cwd).await);
            Ok((state, LoopControl::Continue))
        }

        SlashCommand::AdversarialReview(target) => {
            let cwd = Some(state.cwd.display().to_string());
            report(crate::cli::adversarial_review::run(target, None, None, None, None, cwd).await);
            Ok((state, LoopControl::Continue))
        }

        SlashCommand::Learning => {
            report(crate::cli::learning::run(false));
            Ok((state, LoopControl::Continue))
        }

        SlashCommand::Arena(words) => {
            report(crate::cli::arena::run_words(words).await);
            Ok((state, LoopControl::Continue))
        }

        SlashCommand::Profile(words) => {
            report(crate::cli::profile::run_words(words).await);
            Ok((state, LoopControl::Continue))
        }

        #[cfg(feature = "similarity")]
        SlashCommand::Similarity(words) => {
            report(crate::cli::similarity_cmd::run_words(words).await);
            Ok((state, LoopControl::Continue))
        }

        SlashCommand::Outcome { verdict, run_id } => {
            report(crate::cli::outcome::run(verdict, run_id).await);
            Ok((state, LoopControl::Continue))
        }

        SlashCommand::Doctor { fix } => {
            report(crate::cli::doctor::run(fix).await);
            Ok((state, LoopControl::Continue))
        }

        SlashCommand::Diagnose(task) => {
            report(crate::cli::diagnose::run(task, false).await);
            Ok((state, LoopControl::Continue))
        }

        SlashCommand::Sessions(words) => {
            report(crate::cli::sessions::run_words(words).await);
            Ok((state, LoopControl::Continue))
        }

        SlashCommand::Mcp(words) => {
            report(crate::cli::mcp_cmd::run_words(words).await);
            Ok((state, LoopControl::Continue))
        }

        SlashCommand::Login(maybe_id) => {
            let backend = match maybe_id {
                None => state.active_backend.unwrap_or(state.config.default.backend),
                Some(id_str) => match id_str.parse::<BackendId>() {
                    Ok(id) => id,
                    Err(e) => {
                        eprintln!(
                            "{} {e}\nValid backends: {}",
                            style("error:").red(),
                            valid_backends_list()
                        );
                        return Ok((state, LoopControl::Continue));
                    }
                },
            };
            let _ = crate::cli::login::run(backend, false).await;
            Ok((state, LoopControl::Continue))
        }

        SlashCommand::Cwd(maybe_path) => {
            match maybe_path {
                None => {
                    println!("{}", state.cwd.display());
                }
                Some(path) => match crate::cli::resolve_cwd(Some(path)) {
                    Ok(new_cwd) => {
                        // Journal the change: without it, resume/attach silently reverts
                        // to the directory the session STARTED in.
                        if let Some(rec) = &state.recorder
                            && let Ok(mut rec) = rec.lock()
                            && let Err(e) = rec.record_cwd_change(&new_cwd.display().to_string())
                        {
                            eprintln!(
                                "{}",
                                style(format!(
                                    "session: cwd change not journaled ({e:#}); resume will \
                                     use the previous directory"
                                ))
                                .yellow()
                            );
                        }
                        println!("cwd set to {}", new_cwd.display());
                        return Ok((state.with_cwd(new_cwd), LoopControl::Continue));
                    }
                    Err(e) => {
                        eprintln!("{} {e}", style("error:").red());
                    }
                },
            }
            Ok((state, LoopControl::Continue))
        }

        SlashCommand::Clear => {
            // ANSI clear screen + move to top-left.
            print!("\x1b[2J\x1b[1;1H");
            use std::io::Write;
            let _ = std::io::stdout().flush();
            Ok((state, LoopControl::Continue))
        }

        SlashCommand::Quit => Ok((state, LoopControl::Exit)),

        SlashCommand::SessionInfo => {
            match &state.recorder {
                None => println!("no session log (creation failed at startup — not resumable)"),
                Some(recorder) => {
                    if let Ok(rec) = recorder.lock() {
                        println!("session:  {}", rec.session_id());
                        println!("file:     {}", rec.path().display());
                        println!("context:  {} entries", rec.context_items().len());
                        println!("resume:   agentpit repl --resume {}", rec.short_id());
                    }
                }
            }
            Ok((state, LoopControl::Continue))
        }

        SlashCommand::Tree => {
            if let Some(rec) = lock_recorder(&state) {
                for line in rec.tree_lines() {
                    println!("{line}");
                }
                println!(
                    "{}",
                    style("← = current position, • = current path. /rewind <id> to move.").dim()
                );
            }
            Ok((state, LoopControl::Continue))
        }

        SlashCommand::Branch(target) => {
            if state.recorder.is_none() {
                warn_no_session();
                return Ok((state, LoopControl::Continue));
            }
            // Validate the target BEFORE the summary prompt/LLM call (L4): a typo must not
            // burn a summarization dispatch. Compute the check and drop the lock before any
            // path that moves `state`.
            let known = state
                .recorder
                .as_ref()
                .and_then(|r| r.lock().ok().map(|rec| rec.has_entry(&target)))
                .unwrap_or(true);
            if !known {
                eprintln!(
                    "{} no entry {target} in this session. Pick an id from /tree.",
                    style("error:").red()
                );
                return Ok((state, LoopControl::Continue));
            }
            // B5: leaving a branch offers a summary of what is being left behind —
            // prime's three choices: none / auto / custom instructions.
            let choice = cliclack::select("Leaving this branch — keep a summary of it?")
                .item("no", "No summary", "")
                .item(
                    "summarize",
                    "Summarize the branch being left",
                    "uses the active backend",
                )
                .item(
                    "custom",
                    "Summarize with custom instructions",
                    "you steer what the summary keeps",
                )
                .item("cancel", "Cancel", "")
                .interact()
                .unwrap_or("cancel");
            if choice == "cancel" {
                return Ok((state, LoopControl::Continue));
            }
            let summary = match choice {
                "summarize" => summarize_context(&state, None).await,
                "custom" => {
                    let instructions: String = cliclack::input("What should the summary focus on?")
                        .interact()
                        .unwrap_or_default();
                    summarize_context(&state, Some(&instructions)).await
                }
                _ => None,
            };
            if let Some(recorder) = &state.recorder
                && let Ok(mut rec) = recorder.lock()
            {
                match rec.branch(&target, summary.as_deref()) {
                    Ok(()) => println!(
                        "moved to {target}. The next turn continues from there{}",
                        if summary.is_some() {
                            " (branch summary kept)"
                        } else {
                            ""
                        }
                    ),
                    Err(e) => eprintln!("{} {e}. Pick an id from /tree.", style("error:").red()),
                }
            }
            Ok((state, LoopControl::Continue))
        }

        SlashCommand::Fork(target) => {
            if let Some(rec) = lock_recorder(&state) {
                match rec.fork(target.as_deref()) {
                    Ok(new_id) => {
                        let tail = &new_id[new_id.len().saturating_sub(12)..];
                        println!(
                            "forked into a new session. Open it with: agentpit repl --resume {tail}"
                        );
                    }
                    Err(e) => eprintln!(
                        "{} {e:#}. Pick an id from /tree, or /fork with no id to fork at the current position.",
                        style("error:").red()
                    ),
                }
            }
            Ok((state, LoopControl::Continue))
        }

        SlashCommand::CloneSession => {
            if let Some(rec) = lock_recorder(&state) {
                match rec.fork(None) {
                    Ok(new_id) => {
                        let tail = &new_id[new_id.len().saturating_sub(12)..];
                        println!(
                            "cloned the current position into a new session. Open it with: agentpit repl --resume {tail}"
                        );
                    }
                    Err(e) => eprintln!("{} {e:#}", style("error:").red()),
                }
            }
            Ok((state, LoopControl::Continue))
        }

        SlashCommand::Compact => {
            if state.recorder.is_none() {
                warn_no_session();
                return Ok((state, LoopControl::Continue));
            }
            match summarize_context(&state, None).await {
                Some(text) => {
                    if let Some(recorder) = &state.recorder
                        && let Ok(mut rec) = recorder.lock()
                    {
                        match rec.record_summary(&text, crate::session::SummaryReason::Manual) {
                            Ok(()) => {
                                println!("context compacted — future turns replay from the summary")
                            }
                            Err(e) => eprintln!("{} {e:#}", style("error:").red()),
                        }
                    }
                }
                None => eprintln!(
                    "{} summarization failed; nothing was compacted. Try again or switch backends with /backend.",
                    style("error:").red()
                ),
            }
            Ok((state, LoopControl::Continue))
        }

        // A discovered skill runs as one ordinary turn, on the same path free text takes:
        // the routing, streaming, cancellation and journaling a turn already has are what
        // a skill needs too, and a second dispatch path here would have none of them.
        //
        // The provenance line comes FIRST, before the dispatch: this is the one turn whose
        // text the user did not type, and printing what it is and how big it is afterwards
        // would be a receipt, not a heads-up.
        SlashCommand::Skill {
            name: _,
            provenance,
            prompt,
        } => {
            eprintln!("{}", style(provenance).dim());
            let new_state = dispatch_free_text(state, None, prompt).await?;
            Ok((new_state, LoopControl::Continue))
        }

        // An MCP prompt is the same shape of turn as a skill, one step later: its body
        // lives on the server, so it is fetched here and only then dispatched down the very
        // path above. A fetch that fails is a message and nothing else — never a turn made
        // of the user's own words, which would be a different request than the one they
        // asked for.
        SlashCommand::McpPrompt(invocation) => {
            match crate::mcp::prompts::invoke(&invocation).await {
                Ok(composed) => {
                    eprintln!("{}", style(&composed.provenance).dim());
                    let new_state = dispatch_free_text(state, None, composed.turn).await?;
                    Ok((new_state, LoopControl::Continue))
                }
                Err(e) => {
                    eprintln!("{} {e:#}", style("error:").red());
                    Ok((state, LoopControl::Continue))
                }
            }
        }
    }
}

/// Show a subcommand's failure without ending the REPL. A slash command that fails is a
/// message, not a reason to drop the session.
fn report(result: Result<()>) {
    if let Err(e) = result {
        eprintln!("{} {e:#}", style("error:").red());
    }
}

fn warn_no_session() {
    eprintln!(
        "{}",
        style("no session log for this REPL (creation failed at startup)").yellow()
    );
}

/// Lock the recorder for a short read-only-ish operation, warning when absent.
fn lock_recorder(
    state: &SessionState,
) -> Option<std::sync::MutexGuard<'_, crate::session::SessionRecorder>> {
    match &state.recorder {
        None => {
            warn_no_session();
            None
        }
        Some(recorder) => recorder.lock().ok(),
    }
}

/// Summarize the current branch's conversation via the active backend. Returns `None` on
/// any failure — callers degrade gracefully (branch without summary / no compaction).
async fn summarize_context(state: &SessionState, focus: Option<&str>) -> Option<String> {
    let items = {
        let rec = state.recorder.as_ref()?.lock().ok()?;
        rec.context_items()
    };
    if items.is_empty() {
        return None;
    }
    let mut convo = String::new();
    for (who, text) in &items {
        convo.push_str(&format!("{who}: {text}\n"));
    }
    let steer = focus
        .filter(|f| !f.trim().is_empty())
        .map(|f| format!(" Focus especially on: {f}."))
        .unwrap_or_default();
    let prompt = format!(
        "Summarize this conversation for future context. Cover: the goal, decisions made, \
         current progress, and open next steps.{steer} Be concise (under 300 words). Output \
         only the summary.\n\n{convo}"
    );
    let backend = state.active_backend.unwrap_or(state.config.default.backend);
    eprintln!("{}", style(format!("[summarizing via {backend}…]")).dim());
    let cancel = tokio_util::sync::CancellationToken::new();
    let quiet: std::sync::Arc<dyn Fn(&str) + Send + Sync> = std::sync::Arc::new(|_c: &str| {});
    match crate::dispatch::dispatch(
        backend,
        &prompt,
        &state.cwd,
        cancel,
        quiet,
        &state.regs,
        None,
        None,
    )
    .await
    {
        Ok(res) if !res.auth_failed && !res.output.trim().is_empty() => {
            Some(res.output.trim().to_string())
        }
        _ => None,
    }
}

/// Build a comma-separated list of all known backend ids, derived from `BackendId::ALL`
/// so it automatically stays in sync when new backends are added to the enum.
fn valid_backends_list() -> String {
    crate::types::BackendId::ALL
        .iter()
        .map(|b| b.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Minimum width of the command column in `/help`, so descriptions line up. Wide enough
/// for the longest label the BUILT-INS serve (`/adversarial-review <target>`, 28) plus a
/// gap; a discovered skill can be named anything, so [`help_text`] widens past this rather
/// than letting a long label run straight into its description.
const HELP_LABEL_WIDTH: usize = 30;

/// Render `/help` from the registry: one row per usage form, in registry help order.
fn help_text() -> String {
    help_text_in(slash::registry(), crate::cli::skills::skipped())
}

/// [`help_text`] against a specific resolved registry and skip list — the seam a test uses
/// to render a discovered skill's row, since both are filled at startup.
fn help_text_in(reg: &slash::Registry, skipped: &[crate::cli::skills::Skipped]) -> String {
    let backends = valid_backends_list();
    let mut rows_of: Vec<(String, String)> = Vec::new();
    for spec in reg.help_order(Surface::Repl) {
        for (i, form) in spec.forms.iter().enumerate() {
            rows_of.push((
                slash::form_label(spec, i),
                form.description.replace("{backends}", &backends),
            ));
        }
    }
    let width = rows_of
        .iter()
        .map(|(label, _)| label.chars().count() + 2)
        .max()
        .unwrap_or(0)
        .max(HELP_LABEL_WIDTH);
    let mut rows = String::new();
    for (label, description) in &rows_of {
        // `{:<width$}` pads by chars, and a label is ASCII here, but pad explicitly so a
        // discovered name outside ASCII cannot shift the column.
        let pad = " ".repeat(width.saturating_sub(label.chars().count()));
        rows.push_str(&format!("\n  {label}{pad}{description}"));
    }
    // A skill file that could not be read is not silently forgotten: the one place a user
    // looks for the command list is where they find out a command is missing from it.
    let note = crate::cli::skills::skipped_note(skipped)
        .map(|n| format!("\n{n}\n"))
        .unwrap_or_default();
    format!(
        "\nAvailable REPL commands:\n{rows}\n{note}\
         \nFree text turns are routed to the active backend (or auto-routed) and streamed inline.\
         \nPrefix with !<backend> to route a single turn to that backend without changing the default.\
         \n  e.g.  !claude explain this file\
         \nCtrl-C cancels the in-flight dispatch and returns to the prompt.\
         \nCtrl-D exits cleanly.\n"
    )
}

fn print_help() {
    println!("{}", help_text());
}

#[cfg(test)]
mod tests {
    use super::*;

    // ─── parse_slash: non-slash input ────────────────────────────────────────────

    #[test]
    fn non_slash_input_returns_none() {
        assert!(parse_slash("hello world").is_none());
    }

    #[test]
    fn empty_input_returns_none() {
        assert!(parse_slash("").is_none());
    }

    // ─── parse_slash: /help ───────────────────────────────────────────────────────

    #[test]
    fn parses_help() {
        assert!(matches!(parse_slash("/help"), Some(SlashCommand::Help)));
    }

    #[test]
    fn parses_help_case_insensitive() {
        assert!(matches!(parse_slash("/HELP"), Some(SlashCommand::Help)));
        assert!(matches!(parse_slash("/Help"), Some(SlashCommand::Help)));
    }

    // ─── parse_slash: /backend ───────────────────────────────────────────────────

    #[test]
    fn parses_backend_show_when_no_arg() {
        assert!(matches!(
            parse_slash("/backend"),
            Some(SlashCommand::BackendShow)
        ));
    }

    #[test]
    fn parses_backend_set_with_arg() {
        match parse_slash("/backend claude") {
            Some(SlashCommand::BackendSet(id)) => assert_eq!(id, "claude"),
            other => panic!("expected BackendSet, got {other:?}"),
        }
    }

    #[test]
    fn parses_backend_set_trims_whitespace() {
        match parse_slash("/backend   opencode  ") {
            Some(SlashCommand::BackendSet(id)) => assert_eq!(id, "opencode"),
            other => panic!("expected BackendSet, got {other:?}"),
        }
    }

    // ─── parse_slash: /status, /config, /menu ────────────────────────────────────

    #[test]
    fn parses_status() {
        assert!(matches!(parse_slash("/status"), Some(SlashCommand::Status)));
    }

    #[test]
    fn parses_config() {
        assert!(matches!(parse_slash("/config"), Some(SlashCommand::Config)));
    }

    #[test]
    fn parses_menu() {
        assert!(matches!(parse_slash("/menu"), Some(SlashCommand::Menu)));
    }

    // ─── parse_slash: /ensemble, /review, /workflow ──────────────────────────────

    #[test]
    fn parses_ensemble_with_prompt() {
        match parse_slash("/ensemble explain this code") {
            Some(SlashCommand::Ensemble(p)) => assert_eq!(p, "explain this code"),
            other => panic!("expected Ensemble, got {other:?}"),
        }
    }

    #[test]
    fn parses_ensemble_empty_prompt() {
        match parse_slash("/ensemble") {
            Some(SlashCommand::Ensemble(p)) => assert!(p.is_empty()),
            other => panic!("expected Ensemble, got {other:?}"),
        }
    }

    #[test]
    fn parses_review_with_target() {
        match parse_slash("/review src/lib.rs") {
            Some(SlashCommand::Review(t)) => assert_eq!(t, "src/lib.rs"),
            other => panic!("expected Review, got {other:?}"),
        }
    }

    #[test]
    fn parses_workflow_with_goal() {
        match parse_slash("/workflow add tests") {
            Some(SlashCommand::Workflow(g)) => assert_eq!(g, "add tests"),
            other => panic!("expected Workflow, got {other:?}"),
        }
    }

    // ─── parse_slash: the CLI subcommands ────────────────────────────────────────

    #[test]
    fn parses_the_agent_run_commands_with_their_whole_argument() {
        match parse_slash("/rescue make the auth test pass") {
            Some(SlashCommand::Rescue(t)) => assert_eq!(t, "make the auth test pass"),
            other => panic!("expected Rescue, got {other:?}"),
        }
        match parse_slash("/refactor src/cli/mod.rs split the table out") {
            Some(SlashCommand::Refactor { path, goal }) => {
                assert_eq!(path, "src/cli/mod.rs");
                assert_eq!(goal, "split the table out");
            }
            other => panic!("expected Refactor, got {other:?}"),
        }
        match parse_slash("/explain the lease protocol") {
            Some(SlashCommand::Explain(t)) => assert_eq!(t, "the lease protocol"),
            other => panic!("expected Explain, got {other:?}"),
        }
        assert!(matches!(
            parse_slash("/security-review src/daemon"),
            Some(SlashCommand::SecurityReview(_))
        ));
        assert!(matches!(
            parse_slash("/adversarial-review the caching plan"),
            Some(SlashCommand::AdversarialReview(_))
        ));
    }

    #[test]
    fn parses_the_learning_session_and_diagnostic_commands() {
        assert!(matches!(
            parse_slash("/learning"),
            Some(SlashCommand::Learning)
        ));
        match parse_slash("/arena leaderboard --json") {
            Some(SlashCommand::Arena(words)) => assert_eq!(words, vec!["leaderboard", "--json"]),
            other => panic!("expected Arena, got {other:?}"),
        }
        match parse_slash("/profile learn --dry-run") {
            Some(SlashCommand::Profile(words)) => assert_eq!(words, vec!["learn", "--dry-run"]),
            other => panic!("expected Profile, got {other:?}"),
        }
        match parse_slash("/outcome good") {
            Some(SlashCommand::Outcome { verdict, run_id }) => {
                assert_eq!(verdict, "good");
                assert_eq!(run_id, None);
            }
            other => panic!("expected Outcome, got {other:?}"),
        }
        match parse_slash("/sessions export a1b2c3d4") {
            Some(SlashCommand::Sessions(words)) => assert_eq!(words, vec!["export", "a1b2c3d4"]),
            other => panic!("expected Sessions, got {other:?}"),
        }
        assert!(matches!(
            parse_slash("/doctor --fix"),
            Some(SlashCommand::Doctor { fix: true })
        ));
        match parse_slash("/diagnose add a retry to the fetch loop") {
            Some(SlashCommand::Diagnose(t)) => assert_eq!(t, "add a retry to the fetch loop"),
            other => panic!("expected Diagnose, got {other:?}"),
        }
    }

    #[test]
    fn a_new_command_missing_its_argument_is_rejected_without_dispatching() {
        // Same §7.2 A4 guarantee the older commands have: the rejection is recognizably a
        // slash line, so the turn loop re-prompts instead of sending it to a backend.
        for line in [
            "/rescue",
            "/refactor src/cli/mod.rs",
            "/explain",
            "/security-review",
            "/adversarial-review",
            "/arena",
            "/outcome",
            "/outcome maybe",
            "/doctor --wipe",
            "/diagnose",
        ] {
            assert!(parse_slash(line).is_none(), "{line:?} unexpectedly parsed");
            assert!(
                is_slash_line(line),
                "{line:?} would fall through to dispatch"
            );
        }
    }

    // ─── parse_slash: /login ─────────────────────────────────────────────────────

    #[test]
    fn parses_login_no_arg() {
        assert!(matches!(
            parse_slash("/login"),
            Some(SlashCommand::Login(None))
        ));
    }

    #[test]
    fn parses_login_with_backend() {
        match parse_slash("/login codex") {
            Some(SlashCommand::Login(Some(id))) => assert_eq!(id, "codex"),
            other => panic!("expected Login(Some), got {other:?}"),
        }
    }

    // ─── parse_slash: /cwd ───────────────────────────────────────────────────────

    #[test]
    fn parses_cwd_no_arg() {
        assert!(matches!(parse_slash("/cwd"), Some(SlashCommand::Cwd(None))));
    }

    #[test]
    fn parses_cwd_with_path() {
        match parse_slash("/cwd /tmp") {
            Some(SlashCommand::Cwd(Some(p))) => assert_eq!(p, "/tmp"),
            other => panic!("expected Cwd(Some), got {other:?}"),
        }
    }

    // ─── parse_slash: /clear, /quit, /exit ───────────────────────────────────────

    #[test]
    fn parses_clear() {
        assert!(matches!(parse_slash("/clear"), Some(SlashCommand::Clear)));
    }

    #[test]
    fn parses_quit() {
        assert!(matches!(parse_slash("/quit"), Some(SlashCommand::Quit)));
    }

    #[test]
    fn parses_exit_as_quit() {
        assert!(matches!(parse_slash("/exit"), Some(SlashCommand::Quit)));
    }

    // ─── parse_slash: unknown command ────────────────────────────────────────────

    #[test]
    fn unknown_command_returns_none() {
        // Unknown commands print a warning and return None (caller re-prompts).
        assert!(parse_slash("/frobnicate").is_none());
    }

    #[test]
    fn unknown_command_suggests_and_never_dispatches() {
        // The message the REPL prints comes from the registry + guidance::suggest_slash…
        match slash::parse("/sess", Surface::Repl) {
            Parsed::Unknown { typed, suggestion } => {
                assert_eq!(typed, "sess");
                assert_eq!(suggestion.as_deref(), Some("Did you mean /session?"));
            }
            other => panic!("expected Unknown, got {other:?}"),
        }
        // …and the turn loop must re-prompt instead of dispatching: no command to run,
        // but still recognizably a slash line.
        assert!(parse_slash("/sess").is_none());
        assert!(is_slash_line("/sess"));
    }

    #[test]
    fn a_rejected_slash_line_is_never_free_text() {
        // §7.2 A4 as a property: everything starting with `/` is claimed by the slash
        // path, so nothing typed as a command can reach a backend dispatch.
        for line in ["/frobnicate", "/", "/branch", "/tmp/notes.md please read"] {
            assert!(parse_slash(line).is_none(), "{line:?} unexpectedly parsed");
            assert!(
                is_slash_line(line),
                "{line:?} would fall through to dispatch"
            );
        }
        assert!(!is_slash_line("hello world"));
        assert!(!is_slash_line("!claude explain this"));
    }

    #[test]
    fn branch_without_an_id_is_rejected_without_dispatching() {
        assert!(parse_slash("/branch").is_none());
        assert!(is_slash_line("/branch"));
    }

    // ─── valid_backends_list ─────────────────────────────────────────────────────

    #[test]
    fn valid_backends_list_includes_known_backends() {
        let list = valid_backends_list();
        assert!(list.contains("claude"));
        assert!(list.contains("codex"));
        assert!(list.contains("opencode"));
        assert!(list.contains("antigravity"));
        assert!(list.contains("opencode"));
        assert!(list.contains("goose"));
        assert!(list.contains("copilot"));
    }

    #[test]
    fn valid_backends_list_is_comma_separated() {
        let list = valid_backends_list();
        // At least one comma must be present for a multi-item list.
        assert!(list.contains(','));
    }

    // ─── /help is rendered from the registry ─────────────────────────────────────

    #[test]
    fn help_lists_every_repl_command_and_form() {
        let help = help_text();
        for spec in slash::help_order(Surface::Repl) {
            for i in 0..spec.forms.len() {
                let label = slash::form_label(spec, i);
                assert!(
                    help.contains(&format!("\n  {label:<HELP_LABEL_WIDTH$}")),
                    "/help is missing the row for {label}"
                );
                assert!(
                    label.chars().count() < HELP_LABEL_WIDTH,
                    "{label} overflows the command column and runs into its description"
                );
            }
        }
    }

    #[test]
    fn a_discovered_skill_gets_a_help_row_and_widens_the_column_to_fit() {
        // A skill's name comes from disk, so it can be longer than anything the built-in
        // table has to fit. The column has to grow, or the label runs into its description
        // with no gap — which is exactly what a fixed width did.
        let reg = crate::cli::skills::test_registry_from_disk();
        let help = help_text_in(reg, &[]);
        // Every row — built-in and discovered alike — keeps a gap after its label…
        for spec in reg.help_order(Surface::Repl) {
            for i in 0..spec.forms.len() {
                let label = slash::form_label(spec, i);
                let row = help
                    .lines()
                    .find(|l| l.trim_start().starts_with(&label))
                    .unwrap_or_else(|| panic!("/help is missing the row for {label}\n{help}"));
                let gap = row.trim_start().strip_prefix(&label).unwrap();
                assert!(gap.starts_with("  "), "{row:?} has no gap after its label");
                assert!(!gap.trim().is_empty(), "{row:?} has no description");
            }
        }
        // …and the skill's own row reads off its frontmatter.
        assert!(
            help.contains("/critique [text]") && help.contains("Argue against the current plan"),
            "{help}"
        );
        // The row that forced the column open is the reason the width is computed at all.
        assert!(
            crate::cli::skills::TEST_LONG_NAME.len() + "/ [text]".len() > HELP_LABEL_WIDTH,
            "the fixture no longer exercises a label wider than the built-in column"
        );
    }

    /// A skill file that could not be read has to be findable: `/help` is the list of what
    /// exists, so it is also where a user learns something did not make it in.
    #[test]
    fn help_reports_skill_files_discovery_had_to_skip() {
        use crate::cli::skills::{SkipReason, Skipped};
        let reg = crate::cli::skills::test_registry_from_disk();
        let help = help_text_in(
            reg,
            &[Skipped {
                path: std::path::PathBuf::from("/proj/.claude/skills/broken.md"),
                reason: SkipReason::Unreadable,
            }],
        );
        assert!(
            help.contains("Skipped 1 skill entry: /proj/.claude/skills/broken.md (unreadable)"),
            "{help}"
        );
        // With nothing skipped the trailer is untouched — the note is not a blank line the
        // common case pays for.
        assert!(help_text_in(reg, &[]).contains("resumable)\n\nFree text turns are routed"));
    }

    #[test]
    fn help_keeps_its_column_layout_and_trailer() {
        let help = help_text();
        // One row per label shape: bare, argument-carrying, alias-carrying, and the
        // longest label the table serves — the one the column width is sized for.
        assert!(help.contains("\n  /help                         Show this help"));
        assert!(help.contains("\n  /branch <id>                  Move to a node from /tree"));
        assert!(help.contains(
            "\n  /quit  or  /exit              Exit the REPL (the session stays resumable)"
        ));
        assert!(help.contains(
            "\n  /adversarial-review <target>  Multi-agent adversarial review (challenges"
        ));
        // The {backends} placeholder expands, and terminal commands come last.
        assert!(help.contains("\n  /backend <id>                 Switch active backend (claude,"));
        assert!(!help.contains("{backends}"));
        assert!(help.contains(
            "resumable)\n\nFree text turns are routed to the active backend (or auto-routed)"
        ));
        assert!(help.ends_with("Ctrl-D exits cleanly.\n"));
    }

    // ─── parse_bang_modifier (dispatch_turn) ───────────────────────────────────────
    // Thin coverage here; heavier coverage lives in dispatch_turn tests.

    #[test]
    fn parse_slash_preserves_multi_word_rest() {
        match parse_slash("/ensemble fix all the bugs in this codebase") {
            Some(SlashCommand::Ensemble(p)) => {
                assert_eq!(p, "fix all the bugs in this codebase")
            }
            other => panic!("expected Ensemble, got {other:?}"),
        }
    }
}
