//! What the TUI does with a submitted line (design D2).
//!
//! The Enter handler used to compare three literal strings, so every other `/word` fell
//! into the `else` arm and became a `RequestBody::Send` — a typo was a billable turn.
//! Routing now goes through the shared registry ([`crate::cli::slash`]) on
//! [`Surface::Tui`]: a line beginning with `/` can only come back as something the TUI
//! runs itself or as a refusal, never as free text.
//!
//! [`route`] is pure — line in, decision out — so the decision is unit-tested without a
//! terminal, a daemon, or a worker. `App` only *executes* what comes back:
//!
//! * [`Local`]    — handled on this screen (exit, help overlay).
//! * [`Protocol`] — one worker round-trip; [`Protocol::request`] is the whole mapping to
//!   the wire verb, and the result renders in-screen the way `/tree` already did.
//! * [`Suspend`]  — leave the alternate screen, run the CLI implementation, come back.
//! * [`Route::Compose`] — a discovered skill's composed turn, sent like any other turn.

use crate::cli::slash::{self, Parsed, Registry, SlashCommand, Surface};
use crate::daemon::protocol::RequestBody;

/// The decision for one submitted line.
#[derive(Debug, PartialEq)]
pub enum Route {
    /// Blank line: nothing happens.
    Ignore,
    /// Handled by the TUI itself, without the worker and without leaving the screen.
    Local(Local),
    /// One worker request whose result renders inside the screen.
    Protocol(Protocol),
    /// A CLI subcommand that needs the real terminal back for the duration.
    Suspend(Suspend),
    /// A discovered skill: one turn whose text the registry entry composed. Separate from
    /// [`Route::FreeText`] only because of what the transcript shows — `provenance` above
    /// `label`, rather than a skill's whole body scrolling past as if the user had typed
    /// it. What is *sent* is a `RequestBody::Send` either way.
    Compose {
        /// The dim line printed before the turn: which skill, how big, out of which file.
        provenance: String,
        /// The user line the transcript echoes in place of the composed body.
        label: String,
        /// The composed turn itself.
        text: String,
    },
    /// A cached MCP prompt. Separate from [`Route::Compose`] because the turn does not exist
    /// yet: the body is on the server, so the app awaits
    /// [`crate::mcp::prompts::invoke`] and then either composes exactly as `Compose` does or
    /// prints the refusal it came back with. Routing itself still spawns nothing.
    McpPrompt(Box<crate::mcp::prompts::Invocation>),
    /// A slash line the TUI will not run — unknown name, or a bad argument. Carries the
    /// message to print into the transcript. Never a `Send`: a typo is not a task
    /// (§7.2 A4).
    Unknown(String),
    /// Ordinary text: one conversational turn.
    FreeText(String),
}

/// TUI-local commands.
#[derive(Debug, PartialEq)]
pub enum Local {
    /// `/quit`, `/exit`, `/detach` — close the TUI; the session keeps running.
    Exit,
    /// `/help` — the same overlay `?` opens.
    Help,
}

/// Commands the worker serves over the wire protocol.
#[derive(Debug, PartialEq)]
pub enum Protocol {
    Tree,
    Branch(String),
    Fork(Option<String>),
    Compact,
}

impl Protocol {
    /// The wire verb this command is. One place, so a new protocol command cannot drift
    /// from the request it claims to send.
    pub fn request(&self) -> RequestBody {
        match self {
            Protocol::Tree => RequestBody::Tree,
            Protocol::Branch(target) => RequestBody::Branch {
                target: target.clone(),
                // The TUI never offers the LLM-written leave-behind summary: a keystroke
                // must not start a dispatch.
                summary: None,
            },
            Protocol::Fork(at) => RequestBody::Fork { at: at.clone() },
            Protocol::Compact => RequestBody::Compact,
        }
    }
}

/// Commands run by suspending the alternate screen and calling the CLI implementation.
///
/// Every variant is a subcommand that prints to stdout or opens a cliclack menu, so handing
/// the real terminal back for its duration is the whole mechanism it needs — none of them
/// has a worker verb, and inventing one would only put a second implementation of
/// `agentpit <name>` behind the protocol.
#[derive(Debug, PartialEq)]
pub enum Suspend {
    /// `agentpit status`.
    Status,
    /// `agentpit login <backend>` — `None` means the session's active backend.
    Login(Option<String>),
    /// `agentpit learning`.
    Learning,
    /// `agentpit arena <words>` — `vote` is an interactive cliclack flow.
    Arena(Vec<String>),
    /// `agentpit profile <words>` — no words is `show`.
    Profile(Vec<String>),
    /// `agentpit similarity <words>`.
    #[cfg(feature = "similarity")]
    Similarity(Vec<String>),
    /// `agentpit outcome <verdict> [run-id]`.
    Outcome {
        verdict: String,
        run_id: Option<String>,
    },
    /// `agentpit doctor [--fix]`.
    Doctor { fix: bool },
    /// `agentpit diagnose <task>`.
    Diagnose(String),
    /// `agentpit sessions <words>` — no words is `list`.
    Sessions(Vec<String>),
    /// `agentpit mcp <words>` — no words is `list`. `serve` cannot arrive here: the
    /// registry refuses the word and `mcp_cmd`'s slash grammar has no such action.
    Mcp(Vec<String>),
}

impl Suspend {
    /// How the command is named on the suspended screen and in the transcript note.
    pub fn label(&self) -> String {
        /// `/name a b` — the words as the user would have to type them back.
        fn with_words(name: &str, words: &[String]) -> String {
            if words.is_empty() {
                name.to_string()
            } else {
                format!("{name} {}", words.join(" "))
            }
        }
        match self {
            Suspend::Status => "/status".to_string(),
            Suspend::Login(None) => "/login".to_string(),
            Suspend::Login(Some(backend)) => format!("/login {backend}"),
            Suspend::Learning => "/learning".to_string(),
            Suspend::Arena(words) => with_words("/arena", words),
            Suspend::Profile(words) => with_words("/profile", words),
            #[cfg(feature = "similarity")]
            Suspend::Similarity(words) => with_words("/similarity", words),
            Suspend::Outcome { verdict, run_id } => match run_id {
                Some(id) => format!("/outcome {verdict} {id}"),
                None => format!("/outcome {verdict}"),
            },
            Suspend::Doctor { fix: true } => "/doctor --fix".to_string(),
            Suspend::Doctor { fix: false } => "/doctor".to_string(),
            Suspend::Diagnose(task) => format!("/diagnose {task}"),
            Suspend::Sessions(words) => with_words("/sessions", words),
            Suspend::Mcp(words) => with_words("/mcp", words),
        }
    }
}

/// Decide what one submitted line means on this screen.
pub fn route(line: &str) -> Route {
    route_in(slash::registry(), line)
}

/// [`route`] against a specific resolved registry — the seam the tests drive with a
/// registry built from a `SKILL.md` on disk, since the process one is fed at startup.
pub fn route_in(reg: &Registry, line: &str) -> Route {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return Route::Ignore;
    }
    match reg.parse(trimmed, Surface::Tui) {
        Parsed::NotSlash => Route::FreeText(trimmed.to_string()),
        Parsed::Usage(usage) => Route::Unknown(usage.to_string()),
        Parsed::Unknown { typed, suggestion } => {
            let hint = suggestion.map(|s| format!(" {s}")).unwrap_or_default();
            Route::Unknown(format!(
                "Unknown command /{typed}.{hint} Type /help for what this screen serves."
            ))
        }
        Parsed::Command(command) => command_route(command),
    }
}

fn command_route(command: SlashCommand) -> Route {
    match command {
        SlashCommand::Quit => Route::Local(Local::Exit),
        SlashCommand::Help => Route::Local(Local::Help),
        SlashCommand::Tree => Route::Protocol(Protocol::Tree),
        SlashCommand::Branch(target) => Route::Protocol(Protocol::Branch(target)),
        SlashCommand::Fork(at) => Route::Protocol(Protocol::Fork(at)),
        SlashCommand::Compact => Route::Protocol(Protocol::Compact),
        SlashCommand::Status => Route::Suspend(Suspend::Status),
        SlashCommand::Login(backend) => Route::Suspend(Suspend::Login(backend)),
        SlashCommand::Learning => Route::Suspend(Suspend::Learning),
        SlashCommand::Arena(words) => Route::Suspend(Suspend::Arena(words)),
        SlashCommand::Profile(words) => Route::Suspend(Suspend::Profile(words)),
        #[cfg(feature = "similarity")]
        SlashCommand::Similarity(words) => Route::Suspend(Suspend::Similarity(words)),
        SlashCommand::Outcome { verdict, run_id } => {
            Route::Suspend(Suspend::Outcome { verdict, run_id })
        }
        SlashCommand::Doctor { fix } => Route::Suspend(Suspend::Doctor { fix }),
        SlashCommand::Diagnose(task) => Route::Suspend(Suspend::Diagnose(task)),
        SlashCommand::Sessions(words) => Route::Suspend(Suspend::Sessions(words)),
        SlashCommand::Mcp(words) => Route::Suspend(Suspend::Mcp(words)),
        SlashCommand::Skill {
            name,
            provenance,
            prompt,
        } => Route::Compose {
            provenance,
            label: format!("/{name}"),
            text: prompt,
        },
        SlashCommand::McpPrompt(invocation) => Route::McpPrompt(invocation),
        // Unreachable while the registry and this match agree (`parse` already filtered
        // by surface, and `every_tui_command_has_somewhere_to_run` proves the agreement).
        // If they ever disagree, refuse — never fall through to a dispatch.
        _ => Route::Unknown(
            "That command is not available on this screen. Type /help for what is.".to_string(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ─── registry ↔ router agreement ─────────────────────────────────────────────

    /// Is this a decision that actually runs something?
    fn runs(route: &Route) -> bool {
        matches!(
            route,
            Route::Local(_)
                | Route::Protocol(_)
                | Route::Suspend(_)
                | Route::Compose { .. }
                | Route::McpPrompt(_)
        )
    }

    /// Anti-drift: adding `Surface::Tui` to a registry row without teaching `command_route`
    /// about it would leave a command that is advertised in /help but refuses to run. Every
    /// claimed name must route to something executable — for a discovered row too, which is
    /// why this takes the registry rather than reading the process one.
    fn assert_every_claimed_name_runs(reg: &'static Registry) {
        for spec in reg.entries().filter(|s| s.available_on(Surface::Tui)) {
            for name in spec.names() {
                let arg = slash::probe_arg(&spec.name);
                let with_arg = route_in(reg, &format!("/{name} {arg}"));
                assert!(runs(&with_arg), "/{name} arg routed to {with_arg:?}");
                // Without an argument a command may answer with its usage line instead.
                match route_in(reg, &format!("/{name}")) {
                    r if runs(&r) => {}
                    Route::Unknown(msg) if msg.starts_with("usage:") => {}
                    other => panic!("/{name} routed to {other:?}"),
                }
            }
        }
    }

    #[test]
    fn every_tui_command_has_somewhere_to_run() {
        assert_every_claimed_name_runs(slash::registry());
    }

    #[test]
    fn a_discovered_skill_runs_here_too_and_sends_its_composed_turn() {
        // The end of the chain a `SKILL.md` travels: read off disk, resolved onto the
        // built-ins, offered by the popup, and — here — turned into one ordinary turn
        // instead of the "Unknown command" every other unclaimed `/word` gets.
        let reg = crate::cli::skills::test_registry_from_disk();
        assert_every_claimed_name_runs(reg);
        match route_in(reg, "/critique the caching plan") {
            Route::Compose {
                provenance,
                label,
                text,
            } => {
                // The transcript echoes the command, not the whole skill body…
                assert_eq!(label, "/critique");
                // …under a line saying what is about to be sent and where it came from,
                // so a few KB of instructions the user never typed are not sent silently.
                assert!(
                    provenance.starts_with("[skill /critique — "),
                    "{provenance}"
                );
                assert!(provenance.contains("critique/SKILL.md]"), "{provenance}");
                // …and the turn carries the file's instructions plus what was typed.
                assert!(text.contains(crate::cli::skills::TEST_BODY), "{text}");
                assert!(
                    text.ends_with("The user's request: the caching plan"),
                    "{text}"
                );
            }
            other => panic!("expected a composed turn, got {other:?}"),
        }
        // With no argument the skill still runs — the same composed turn, minus a request
        // section. `[text]`, not `<text>`: most skills read the conversation they are
        // invoked in, so a bare `/critique` is the common case, not a usage error.
        match route_in(reg, "/critique") {
            Route::Compose { text, .. } => {
                assert!(text.contains(crate::cli::skills::TEST_BODY), "{text}");
                assert!(!text.contains("The user's request"), "{text}");
            }
            other => panic!("expected a bare invocation to run, got {other:?}"),
        }
        // A skill the registry does not carry — a name never discovered, or one whose file
        // discovery had to skip — is a refusal, never a turn made of the line's own text.
        for line in ["/critiquee something", "/critique-that-was-skipped now"] {
            match route_in(reg, line) {
                Route::Unknown(msg) => assert!(msg.starts_with("Unknown command"), "{msg}"),
                other => panic!("{line:?} routed to {other:?} instead of being refused"),
            }
        }
        // The process registry has had nothing installed here, so the same line that runs
        // above is refused through `route` — discovery is what makes a skill a command.
        assert!(matches!(
            route("/critique the caching plan"),
            Route::Unknown(_)
        ));
    }

    /// The MCP end of the same chain: a cached prompt list becomes a row this screen serves,
    /// and the row routes to an invocation the app will fetch — never to free text.
    #[test]
    fn a_cached_mcp_prompt_routes_to_an_invocation_not_to_free_text() {
        let reg = crate::mcp::prompts::test_registry_from_cache();
        assert_every_claimed_name_runs(reg);
        match route_in(reg, "/ctx7:docs ratatui scrolling") {
            Route::McpPrompt(inv) => {
                assert_eq!(inv.name, crate::mcp::prompts::TEST_COMMAND);
                assert_eq!(inv.arg, "ratatui scrolling");
                // Routing spawned nothing: the definition is carried, not connected to.
                assert_eq!(inv.def.command, "npx");
            }
            other => panic!("expected an MCP invocation, got {other:?}"),
        }
        // A name the cache does not carry is refused, exactly like any other unknown word —
        // an MCP row that was never refreshed must not become a turn of the line's own text.
        for line in ["/ctx7:docsx now", "/other:docs now"] {
            match route_in(reg, line) {
                Route::Unknown(msg) => assert!(msg.starts_with("Unknown command"), "{msg}"),
                other => panic!("{line:?} routed to {other:?} instead of being refused"),
            }
        }
        // The process registry has had nothing installed, so the same line is refused there.
        assert!(matches!(route("/ctx7:docs ratatui"), Route::Unknown(_)));
    }

    // ─── the commands that existed before, unchanged ─────────────────────────────

    #[test]
    fn the_three_hardcoded_commands_keep_their_meaning() {
        assert_eq!(route("/tree"), Route::Protocol(Protocol::Tree));
        for word in ["/quit", "/exit", "/detach"] {
            assert_eq!(route(word), Route::Local(Local::Exit), "{word}");
        }
        // Surrounding whitespace and case are not a new command.
        assert_eq!(route("  /TREE  "), Route::Protocol(Protocol::Tree));
        assert_eq!(route("/Detach"), Route::Local(Local::Exit));
    }

    // ─── the billable-typo bug ───────────────────────────────────────────────────

    #[test]
    fn an_unknown_slash_command_never_becomes_a_turn() {
        // The bug this unit fixes: before, anything that was not /tree, /quit, /exit or
        // /detach fell through to `RequestBody::Send` and cost tokens.
        for line in [
            "/frobnicate",
            "/",
            "/menu",                 // exists in the REPL, not here
            "/tmp/some/path is bad", // a path, typed by accident
            "/compactt",
        ] {
            match route(line) {
                Route::Unknown(_) => {}
                other => panic!("{line:?} routed to {other:?} instead of being refused"),
            }
        }
    }

    #[test]
    fn a_refusal_names_the_typo_and_points_somewhere() {
        match route("/tre") {
            Route::Unknown(msg) => {
                assert!(msg.contains("Unknown command /tre."), "{msg}");
                assert!(msg.contains("Did you mean /tree?"), "{msg}");
                assert!(msg.contains("/help"), "{msg}");
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
        // Suggestions come from the TUI's own vocabulary, not the REPL's.
        match route("/conf") {
            Route::Unknown(msg) => assert!(!msg.contains("Did you mean"), "{msg}"),
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[test]
    fn a_missing_argument_is_refused_with_its_usage_line() {
        match route("/branch") {
            Route::Unknown(msg) => assert!(msg.starts_with("usage: /branch <entry-id>"), "{msg}"),
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    // ─── the other four decisions ────────────────────────────────────────────────

    #[test]
    fn free_text_is_the_only_route_that_can_reach_a_backend() {
        assert_eq!(
            route("explain this file"),
            Route::FreeText("explain this file".to_string())
        );
        // The @backend modifier is free text too — the worker parses it.
        assert_eq!(
            route("  @claude explain this  "),
            Route::FreeText("@claude explain this".to_string())
        );
        assert_eq!(route(""), Route::Ignore);
        assert_eq!(route("   "), Route::Ignore);
    }

    #[test]
    fn protocol_commands_map_to_their_worker_verb() {
        assert_eq!(Protocol::Tree.request(), RequestBody::Tree);
        assert_eq!(
            Protocol::Branch("a1b2c3d4".into()).request(),
            RequestBody::Branch {
                target: "a1b2c3d4".into(),
                summary: None,
            }
        );
        assert_eq!(
            Protocol::Fork(Some("a1b2c3d4".into())).request(),
            RequestBody::Fork {
                at: Some("a1b2c3d4".into())
            }
        );
        assert_eq!(
            Protocol::Fork(None).request(),
            RequestBody::Fork { at: None }
        );
        assert_eq!(Protocol::Compact.request(), RequestBody::Compact);
    }

    #[test]
    fn protocol_arguments_survive_the_route() {
        assert_eq!(
            route("/branch a1b2c3d4"),
            Route::Protocol(Protocol::Branch("a1b2c3d4".into()))
        );
        assert_eq!(route("/fork"), Route::Protocol(Protocol::Fork(None)));
        assert_eq!(
            route("/fork a1b2c3d4"),
            Route::Protocol(Protocol::Fork(Some("a1b2c3d4".into())))
        );
        assert_eq!(route("/compact"), Route::Protocol(Protocol::Compact));
    }

    #[test]
    fn suspend_commands_carry_their_argument_and_a_label() {
        assert_eq!(route("/status"), Route::Suspend(Suspend::Status));
        assert_eq!(route("/login"), Route::Suspend(Suspend::Login(None)));
        assert_eq!(
            route("/login codex"),
            Route::Suspend(Suspend::Login(Some("codex".into())))
        );
        assert_eq!(Suspend::Status.label(), "/status");
        assert_eq!(Suspend::Login(None).label(), "/login");
        assert_eq!(Suspend::Login(Some("codex".into())).label(), "/login codex");
    }

    // ─── the CLI subcommands this screen suspends for ────────────────────────────

    #[test]
    fn outcome_is_reachable_from_the_screen_the_user_actually_runs() {
        // Bare `agentpit` opens the TUI, and /outcome is the human verdict the learning
        // fold weights above every other evidence source. If it is only in the REPL, the
        // strongest label the layer can get is the one nobody is standing in front of.
        assert_eq!(
            route("/outcome good"),
            Route::Suspend(Suspend::Outcome {
                verdict: "good".into(),
                run_id: None,
            })
        );
        assert_eq!(
            route("/outcome bad run-7"),
            Route::Suspend(Suspend::Outcome {
                verdict: "bad".into(),
                run_id: Some("run-7".into()),
            })
        );
        // A verdict that is neither is refused in-screen, never recorded as the other one.
        match route("/outcome maybe") {
            Route::Unknown(msg) => assert!(msg.starts_with("usage: /outcome"), "{msg}"),
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[test]
    fn the_learning_and_diagnostic_subcommands_suspend_with_their_words() {
        assert_eq!(route("/learning"), Route::Suspend(Suspend::Learning));
        assert_eq!(
            route("/arena vote --round r7"),
            Route::Suspend(Suspend::Arena(vec![
                "vote".into(),
                "--round".into(),
                "r7".into()
            ]))
        );
        assert_eq!(route("/profile"), Route::Suspend(Suspend::Profile(vec![])));
        assert_eq!(
            route("/doctor --fix"),
            Route::Suspend(Suspend::Doctor { fix: true })
        );
        assert_eq!(
            route("/diagnose add a retry to the fetch loop"),
            Route::Suspend(Suspend::Diagnose("add a retry to the fetch loop".into()))
        );
        assert_eq!(
            route("/sessions show a1b2c3d4"),
            Route::Suspend(Suspend::Sessions(vec!["show".into(), "a1b2c3d4".into()]))
        );
        // The label is what the suspended screen and the transcript note say happened.
        assert_eq!(Suspend::Learning.label(), "/learning");
        assert_eq!(Suspend::Profile(vec![]).label(), "/profile");
        assert_eq!(Suspend::Arena(vec!["vote".into()]).label(), "/arena vote");
        assert_eq!(Suspend::Doctor { fix: false }.label(), "/doctor");
        assert_eq!(
            Suspend::Outcome {
                verdict: "good".into(),
                run_id: Some("run-7".into())
            }
            .label(),
            "/outcome good run-7"
        );
    }

    #[test]
    fn the_run_commands_are_not_claimed_here_and_stay_refused() {
        // The five agent-run commands are REPL-only: a multi-minute streaming dispatch
        // would hold the alternate screen while this client stops reading its worker's
        // frames. Claiming them here without a route would be worse than not claiming
        // them — so the check is that they are refused, not silently sent as free text.
        for line in [
            "/rescue make the auth test pass",
            "/refactor src/cli/mod.rs split it up",
            "/explain the lease protocol",
            "/security-review src/daemon",
            "/adversarial-review the caching plan",
        ] {
            match route(line) {
                Route::Unknown(msg) => assert!(msg.starts_with("Unknown command"), "{msg}"),
                other => panic!("{line:?} routed to {other:?} instead of being refused"),
            }
        }
    }

    #[test]
    fn help_is_local_so_it_costs_nothing() {
        assert_eq!(route("/help"), Route::Local(Local::Help));
    }
}
