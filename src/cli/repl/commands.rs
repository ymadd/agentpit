use anyhow::Result;
use console::style;

use super::state::SessionState;
use super::turn::LoopControl;
use crate::types::BackendId;

/// A parsed slash command.
#[derive(Debug)]
pub enum SlashCommand {
    Help,
    BackendShow,
    BackendSet(String),
    Status,
    Config,
    Menu,
    Ensemble(String),
    Review(String),
    Workflow(String),
    Login(Option<String>),
    Cwd(Option<String>),
    Clear,
    Quit,
}

/// Parse a line that starts with `/` into a `SlashCommand`, returning `None` if
/// the line is not a slash command.
pub fn parse_slash(input: &str) -> Option<SlashCommand> {
    let trimmed = input.trim();
    if !trimmed.starts_with('/') {
        return None;
    }
    let without_slash = &trimmed[1..];
    let (cmd, rest) = without_slash
        .split_once(char::is_whitespace)
        .map(|(c, r)| (c, r.trim()))
        .unwrap_or((without_slash, ""));

    let cmd_lower = cmd.to_ascii_lowercase();
    Some(match cmd_lower.as_str() {
        "help" => SlashCommand::Help,
        "backend" => {
            if rest.is_empty() {
                SlashCommand::BackendShow
            } else {
                SlashCommand::BackendSet(rest.to_string())
            }
        }
        "status" => SlashCommand::Status,
        "config" => SlashCommand::Config,
        "menu" => SlashCommand::Menu,
        "ensemble" => SlashCommand::Ensemble(rest.to_string()),
        "review" => SlashCommand::Review(rest.to_string()),
        "workflow" => SlashCommand::Workflow(rest.to_string()),
        "login" => {
            if rest.is_empty() {
                SlashCommand::Login(None)
            } else {
                SlashCommand::Login(Some(rest.to_string()))
            }
        }
        "cwd" => {
            if rest.is_empty() {
                SlashCommand::Cwd(None)
            } else {
                SlashCommand::Cwd(Some(rest.to_string()))
            }
        }
        "clear" => SlashCommand::Clear,
        "quit" | "exit" => SlashCommand::Quit,
        _ => {
            eprintln!(
                "{}",
                style(format!(
                    "Unknown command /{cmd}. Type /help for available commands."
                ))
                .yellow()
            );
            return None;
        }
    })
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

fn print_help() {
    println!(
        "\nAvailable REPL commands:\n\
         \n  /help                  Show this help\
         \n  /backend               Show active backend and transport\
         \n  /backend <id>          Switch active backend ({})\
         \n  /status                Show full backend status\
         \n  /config                Open config menu\
         \n  /menu                  Open the interactive menu\
         \n  /ensemble <prompt>     Fan prompt to all configured backends in parallel\
         \n  /review <target>       Run multi-agent code review\
         \n  /workflow <goal>       Run model-driven workflow\
         \n  /login [backend]       Launch login flow (defaults to active backend)\
         \n  /cwd                   Show current working directory\
         \n  /cwd <path>            Change working directory for this session\
         \n  /clear                 Clear the terminal screen\
         \n  /quit  or  /exit       Exit the REPL\
         \n\nFree text turns are routed to the active backend (or auto-routed) and streamed inline.\
         \nPrefix with @<backend> to route a single turn to that backend without changing the default.\
         \n  e.g.  @claude explain this file\
         \nCtrl-C cancels the in-flight dispatch and returns to the prompt.\
         \nCtrl-D exits cleanly.\n",
        valid_backends_list()
    );
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

    // ─── parse_at_modifier (dispatch_turn) ───────────────────────────────────────
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
