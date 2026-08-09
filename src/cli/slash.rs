//! The shared slash-command registry (design D1/D2/D3).
//!
//! Every interactive surface — the in-process REPL and the daemon-backed `attach`
//! client — reads its command list, its help text, and its parse rules from the
//! [`Registry`] here. A command is declared exactly once: add a row and every surface
//! that the row claims picks it up, with no second list to keep in sync.
//!
//! ## Two layers
//!
//! A [`Registry`] is [`BUILTINS`] — the static table below, compiled in — plus a list of
//! *runtime* entries discovered at startup ([`crate::cli::skills`] reads them off disk).
//! The two layers are not peers:
//!
//! * A built-in is `const`-constructed, so the compiler still checks every row.
//! * A runtime entry owns its strings and carries a closure, so it can be built from
//!   something read at startup — which a `&'static str` and a bare `fn` cannot express.
//! * A runtime entry can only ADD a name. One that collides with a built-in — by name or
//!   by alias — is dropped whole ([`Registry::resolve`]), so nothing on disk can take
//!   `/compact` away from the command the user already knows.
//!
//! Every surface goes through [`registry`], the one resolved registry for the process, so
//! there is a single merge rather than one per caller. [`install`] is what fills the second
//! layer, once, from the entry point that knows the session's working directory; a process
//! that never calls it (any of the non-interactive subcommands) sees the built-ins alone.
//!
//! The registry is surface-agnostic on purpose. It knows *what* a command is and *how
//! to read its arguments*; it never prints and never executes. Surfaces decide how to
//! render the outcome of [`parse`] and how to run a [`SlashCommand`].

use std::borrow::Cow;
use std::sync::{Arc, OnceLock};

/// An interactive surface that accepts slash commands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Surface {
    /// `agentpit repl` — the in-process client; the full command set.
    Repl,
    /// `agentpit attach` — the daemon-backed client; the commands a worker can serve.
    Attach,
    /// `agentpit tui` — the fullscreen client: the verbs its worker serves, plus the CLI
    /// subcommands it suspends the alternate screen to run.
    Tui,
}

/// Help grouping. Within a group the table order is the display order; the terminal
/// group (screen control and exit) is always rendered last.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Category {
    /// The CLI itself: help, config, menus.
    Meta,
    /// Backend selection, status, and auth.
    Backend,
    /// Work handed to backend agents — one of them or several.
    Agents,
    /// The learned routing layer: what it knows, and the human verdicts that train it.
    Learning,
    /// Health checks and dry runs — they inspect, they do not change a conversation.
    Diagnostic,
    /// The conversation and its session log.
    Session,
    /// Screen control and leaving.
    Terminal,
}

/// What running a command can cost.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecKind {
    /// Runs in-process and never reaches a backend.
    Local,
    /// May dispatch to a backend, i.e. may spend tokens.
    Dispatch,
}

/// One usage form of a command: the argument hint plus what *that* form does.
///
/// Most commands have a single form. A few (`/backend`, `/cwd`) read as one command with
/// two behaviors — show with no argument, set with one — and get a form each.
#[derive(Clone)]
pub struct Form {
    /// Argument hint as shown in help, e.g. `"<prompt>"`, `"[id]"`, or `""` for none.
    pub args: Cow<'static, str>,
    /// What this form does. `{backends}` expands to the list of valid backend ids.
    pub description: Cow<'static, str>,
}

/// Reading a command's (already trimmed) argument text: the command, or the usage line
/// the surface shows instead of dispatching.
pub type ParseResult = Result<SlashCommand, &'static str>;

/// A parser a runtime entry brings with it, closing over whatever it was built from.
pub type OwnedParser = Arc<dyn Fn(&str) -> ParseResult + Send + Sync>;

/// How a command reads its argument text.
///
/// Two shapes because the two layers can afford different things: a built-in is a literal
/// in a `static`, so it keeps the plain `fn` pointer the table has always used and stays
/// compile-time checked; a runtime entry has to carry the data it was discovered with
/// (a path, a body), which only a closure can hold.
#[derive(Clone)]
pub enum ParseRule {
    /// A built-in's parser.
    Fn(fn(&str) -> ParseResult),
    /// A runtime entry's parser, with whatever it captured.
    Owned(OwnedParser),
}

impl ParseRule {
    /// Wrap a closure for a runtime entry.
    pub fn owned(f: impl Fn(&str) -> ParseResult + Send + Sync + 'static) -> ParseRule {
        ParseRule::Owned(Arc::new(f))
    }

    /// Read `rest` (already trimmed) into a command.
    pub fn apply(&self, rest: &str) -> ParseResult {
        match self {
            ParseRule::Fn(f) => f(rest),
            ParseRule::Owned(f) => f(rest),
        }
    }
}

/// One command in the registry — built-in or runtime; the surfaces cannot tell which.
///
/// The `Cow` fields are what make the second layer expressible: a built-in row is
/// `Cow::Borrowed` throughout and costs nothing, while a discovered entry owns its
/// strings.
pub struct SlashSpec {
    /// Canonical name, without the leading slash. Matched case-insensitively.
    pub name: Cow<'static, str>,
    /// Alternative names that resolve to the same command.
    pub aliases: Cow<'static, [Cow<'static, str>]>,
    /// Usage forms, primary first. Never empty.
    pub forms: Cow<'static, [Form]>,
    pub category: Category,
    /// Surfaces on which this command is available.
    pub surfaces: Cow<'static, [Surface]>,
    pub exec: ExecKind,
    /// Build the command from its (already trimmed) argument text. `Err` carries the
    /// usage line the surface should show instead of dispatching.
    pub parse: ParseRule,
}

impl SlashSpec {
    /// The primary form's argument hint.
    pub fn arg_hint(&self) -> &str {
        &self.forms[0].args
    }

    /// The primary form's description.
    pub fn description(&self) -> &str {
        &self.forms[0].description
    }

    /// Every name this command answers to: the canonical one first, then its aliases.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        std::iter::once(self.name.as_ref()).chain(self.aliases.iter().map(Cow::as_ref))
    }

    /// Does `name` address this command? Case-insensitive: a runtime entry's name comes
    /// from outside this file, so the comparison cannot assume it was lowercased.
    pub fn matches(&self, name: &str) -> bool {
        self.names().any(|n| n.eq_ignore_ascii_case(name))
    }

    pub fn available_on(&self, surface: Surface) -> bool {
        self.surfaces.contains(&surface)
    }
}

/// A parsed slash command — the surface-agnostic *intent*. Executing it is a surface's
/// job (the REPL does so in `repl::commands::handle_slash`).
///
/// Payload shapes mirror the CLI subcommand each one stands for. Commands whose CLI form
/// takes a clap sub-action (`/arena`, `/profile`, `/similarity`, `/sessions`) carry the
/// raw argv words instead of a re-modelled enum: the words go to the CLI's own clap
/// definition, so the slash form cannot drift from `agentpit <name>`.
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
    Rescue(String),
    Refactor {
        path: String,
        goal: String,
    },
    Explain(String),
    SecurityReview(String),
    AdversarialReview(String),
    Learning,
    Arena(Vec<String>),
    Profile(Vec<String>),
    #[cfg(feature = "similarity")]
    Similarity(Vec<String>),
    Outcome {
        verdict: String,
        run_id: Option<String>,
    },
    Doctor {
        fix: bool,
    },
    Diagnose(String),
    /// `/mcp [list|refresh|import]` — the raw argv words, handed to `mcp_cmd`'s own clap
    /// grammar. That grammar has no `serve`, which is what keeps the stdio server
    /// unreachable from a conversation.
    Mcp(Vec<String>),
    Login(Option<String>),
    Cwd(Option<String>),
    Clear,
    Quit,
    SessionInfo,
    Sessions(Vec<String>),
    Tree,
    Branch(String),
    Fork(Option<String>),
    CloneSession,
    Compact,
    /// A cached MCP prompt: everything one `/<server>:<prompt>` invocation needs, recorded
    /// but not yet run.
    ///
    /// Unlike [`SlashCommand::Skill`], the turn is NOT composed here — a skill's body is on
    /// disk and was read at startup, while a prompt's body is on its server and has to be
    /// fetched. Parsing stays synchronous and spawns nothing; the surface awaits
    /// [`crate::mcp::prompts::invoke`], which either returns the turn plus its provenance
    /// line or an error the surface shows as a refusal.
    ///
    /// Boxed because the payload carries a whole server definition, which would otherwise
    /// set the size of every variant of this enum.
    McpPrompt(Box<crate::mcp::prompts::Invocation>),
    /// A discovered skill: one composed turn, built by the entry's own closure from the
    /// file it was read out of plus whatever the user typed after the name.
    Skill {
        /// The command name, for the transcript echo — the prompt is far too long for it.
        name: String,
        /// What the surface shows before sending: the skill, its size, and its file
        /// ([`crate::cli::skills::provenance`]). Carried rather than rebuilt per surface,
        /// so the REPL and the TUI cannot disagree about what a turn is about to cost.
        provenance: String,
        /// The turn text to dispatch.
        prompt: String,
    },
}

/// What a line turned out to be.
#[derive(Debug)]
pub enum Parsed {
    /// Not a slash command at all — the surface may treat it as free text.
    NotSlash,
    /// A command, ready to execute.
    Command(SlashCommand),
    /// A known command whose arguments were wrong; carries the usage line to show.
    /// The surface must NOT dispatch.
    Usage(&'static str),
    /// No such command on this surface. The surface must NOT dispatch: a typo is not a
    /// task (D3 / §7.2 A4).
    Unknown {
        /// The name as the user typed it, without the leading slash.
        typed: String,
        /// "Did you mean /x?", when a close-enough command exists.
        suggestion: Option<String>,
    },
}

/// A borrowed string in the table below. `Cow::Borrowed` spelled out on every field would
/// wrap the rows and cost the table the shape that makes it readable.
const fn cow(s: &'static str) -> Cow<'static, str> {
    Cow::Borrowed(s)
}

/// The alias list of a command that has none.
const NO_ALIASES: Cow<'static, [Cow<'static, str>]> = Cow::Borrowed(&[]);

/// The built-in table — layer one, compiled in. Order here is the parse-agnostic canonical
/// order: it drives both help rendering and the "did you mean" search, so a new command
/// needs exactly one new row.
///
/// Read it through [`Registry::entries`], not directly: a surface must see the runtime
/// entries too.
pub static BUILTINS: &[SlashSpec] = &[
    SlashSpec {
        name: cow("help"),
        aliases: NO_ALIASES,
        forms: Cow::Borrowed(&[Form {
            args: cow(""),
            description: cow("Show this help"),
        }]),
        category: Category::Meta,
        surfaces: Cow::Borrowed(&[Surface::Repl, Surface::Tui]),
        exec: ExecKind::Local,
        parse: ParseRule::Fn(|_| Ok(SlashCommand::Help)),
    },
    SlashSpec {
        name: cow("backend"),
        aliases: NO_ALIASES,
        forms: Cow::Borrowed(&[
            Form {
                args: cow(""),
                description: cow("Show active backend and transport"),
            },
            Form {
                args: cow("<id>"),
                description: cow("Switch active backend ({backends})"),
            },
        ]),
        category: Category::Backend,
        surfaces: Cow::Borrowed(&[Surface::Repl]),
        exec: ExecKind::Local,
        parse: ParseRule::Fn(|rest| {
            Ok(if rest.is_empty() {
                SlashCommand::BackendShow
            } else {
                SlashCommand::BackendSet(rest.to_string())
            })
        }),
    },
    SlashSpec {
        name: cow("status"),
        aliases: NO_ALIASES,
        forms: Cow::Borrowed(&[Form {
            args: cow(""),
            description: cow("Show full backend status"),
        }]),
        category: Category::Backend,
        surfaces: Cow::Borrowed(&[Surface::Repl, Surface::Tui]),
        exec: ExecKind::Local,
        parse: ParseRule::Fn(|_| Ok(SlashCommand::Status)),
    },
    SlashSpec {
        name: cow("config"),
        aliases: NO_ALIASES,
        forms: Cow::Borrowed(&[Form {
            args: cow(""),
            description: cow("Open config menu"),
        }]),
        category: Category::Meta,
        surfaces: Cow::Borrowed(&[Surface::Repl]),
        exec: ExecKind::Local,
        parse: ParseRule::Fn(|_| Ok(SlashCommand::Config)),
    },
    SlashSpec {
        name: cow("menu"),
        aliases: NO_ALIASES,
        forms: Cow::Borrowed(&[Form {
            args: cow(""),
            description: cow("Open the interactive menu"),
        }]),
        category: Category::Meta,
        surfaces: Cow::Borrowed(&[Surface::Repl]),
        exec: ExecKind::Local,
        parse: ParseRule::Fn(|_| Ok(SlashCommand::Menu)),
    },
    SlashSpec {
        name: cow("ensemble"),
        aliases: NO_ALIASES,
        forms: Cow::Borrowed(&[Form {
            args: cow("<prompt>"),
            description: cow("Fan prompt to all configured backends in parallel"),
        }]),
        category: Category::Agents,
        surfaces: Cow::Borrowed(&[Surface::Repl]),
        exec: ExecKind::Dispatch,
        parse: ParseRule::Fn(|rest| Ok(SlashCommand::Ensemble(rest.to_string()))),
    },
    SlashSpec {
        name: cow("review"),
        aliases: NO_ALIASES,
        forms: Cow::Borrowed(&[Form {
            args: cow("<target>"),
            description: cow("Run multi-agent code review"),
        }]),
        category: Category::Agents,
        surfaces: Cow::Borrowed(&[Surface::Repl]),
        exec: ExecKind::Dispatch,
        parse: ParseRule::Fn(|rest| Ok(SlashCommand::Review(rest.to_string()))),
    },
    SlashSpec {
        name: cow("workflow"),
        aliases: NO_ALIASES,
        forms: Cow::Borrowed(&[Form {
            args: cow("<goal>"),
            description: cow("Run model-driven workflow"),
        }]),
        category: Category::Agents,
        surfaces: Cow::Borrowed(&[Surface::Repl]),
        exec: ExecKind::Dispatch,
        parse: ParseRule::Fn(|rest| Ok(SlashCommand::Workflow(rest.to_string()))),
    },
    SlashSpec {
        name: cow("rescue"),
        aliases: NO_ALIASES,
        forms: Cow::Borrowed(&[Form {
            args: cow("<task>"),
            description: cow("Delegate a one-shot task to a backend agent"),
        }]),
        category: Category::Agents,
        surfaces: Cow::Borrowed(&[Surface::Repl]),
        exec: ExecKind::Dispatch,
        parse: ParseRule::Fn(|rest| {
            if rest.is_empty() {
                Err("usage: /rescue <task> — e.g. /rescue make the auth test pass")
            } else {
                Ok(SlashCommand::Rescue(rest.to_string()))
            }
        }),
    },
    SlashSpec {
        name: cow("refactor"),
        aliases: NO_ALIASES,
        forms: Cow::Borrowed(&[Form {
            args: cow("<path> <goal>"),
            description: cow("Plan a refactor of a path toward a goal, via a backend agent"),
        }]),
        category: Category::Agents,
        surfaces: Cow::Borrowed(&[Surface::Repl]),
        exec: ExecKind::Dispatch,
        parse: ParseRule::Fn(|rest| match rest.split_once(char::is_whitespace) {
            Some((path, goal)) if !goal.trim().is_empty() => Ok(SlashCommand::Refactor {
                path: path.to_string(),
                goal: goal.trim().to_string(),
            }),
            // One word is a path with no goal, and `agentpit refactor` requires both.
            _ => Err(
                "usage: /refactor <path> <goal> — e.g. /refactor src/cli/mod.rs split the command table out",
            ),
        }),
    },
    SlashSpec {
        name: cow("explain"),
        aliases: NO_ALIASES,
        forms: Cow::Borrowed(&[Form {
            args: cow("<target>"),
            description: cow("Explain a file, symbol, or concept via a backend agent"),
        }]),
        category: Category::Agents,
        surfaces: Cow::Borrowed(&[Surface::Repl]),
        exec: ExecKind::Dispatch,
        parse: ParseRule::Fn(|rest| {
            if rest.is_empty() {
                Err("usage: /explain <target> — a file, a symbol, or a concept")
            } else {
                Ok(SlashCommand::Explain(rest.to_string()))
            }
        }),
    },
    SlashSpec {
        name: cow("security-review"),
        aliases: NO_ALIASES,
        forms: Cow::Borrowed(&[Form {
            args: cow("<target>"),
            description: cow("Multi-agent security review (OWASP-style checklist)"),
        }]),
        category: Category::Agents,
        surfaces: Cow::Borrowed(&[Surface::Repl]),
        exec: ExecKind::Dispatch,
        parse: ParseRule::Fn(|rest| {
            if rest.is_empty() {
                Err("usage: /security-review <target> — e.g. /security-review src/daemon")
            } else {
                Ok(SlashCommand::SecurityReview(rest.to_string()))
            }
        }),
    },
    SlashSpec {
        name: cow("adversarial-review"),
        aliases: NO_ALIASES,
        forms: Cow::Borrowed(&[Form {
            args: cow("<target>"),
            description: cow("Multi-agent adversarial review (challenges assumptions)"),
        }]),
        category: Category::Agents,
        surfaces: Cow::Borrowed(&[Surface::Repl]),
        exec: ExecKind::Dispatch,
        parse: ParseRule::Fn(|rest| {
            if rest.is_empty() {
                Err(
                    "usage: /adversarial-review <target> — e.g. /adversarial-review the caching plan",
                )
            } else {
                Ok(SlashCommand::AdversarialReview(rest.to_string()))
            }
        }),
    },
    SlashSpec {
        name: cow("learning"),
        aliases: NO_ALIASES,
        forms: Cow::Borrowed(&[Form {
            args: cow(""),
            description: cow("Show what the learned routing layer knows and has changed"),
        }]),
        category: Category::Learning,
        surfaces: Cow::Borrowed(&[Surface::Repl, Surface::Tui]),
        exec: ExecKind::Local,
        parse: ParseRule::Fn(|_| Ok(SlashCommand::Learning)),
    },
    SlashSpec {
        name: cow("arena"),
        aliases: NO_ALIASES,
        forms: Cow::Borrowed(&[Form {
            args: cow("<sub> [flags]"),
            description: cow("Blind head-to-head: run a round, vote on it, read standings"),
        }]),
        category: Category::Learning,
        // `arena run` spends a full agentic run per contender.
        exec: ExecKind::Dispatch,
        surfaces: Cow::Borrowed(&[Surface::Repl, Surface::Tui]),
        parse: ParseRule::Fn(|rest| {
            let words = split_words(rest);
            if words.is_empty() {
                Err(
                    "usage: /arena <run|vote|leaderboard|rounds|show|apply|templates> [flags] — /arena --help for the full grammar",
                )
            } else {
                Ok(SlashCommand::Arena(words))
            }
        }),
    },
    SlashSpec {
        name: cow("profile"),
        aliases: NO_ALIASES,
        forms: Cow::Borrowed(&[Form {
            args: cow("[sub] [flags]"),
            description: cow("Capability matrix (default); --help for seed/run/replay/learn"),
        }]),
        category: Category::Learning,
        // `profile run --backend <id>` measures a backend live.
        exec: ExecKind::Dispatch,
        surfaces: Cow::Borrowed(&[Surface::Repl, Surface::Tui]),
        parse: ParseRule::Fn(|rest| Ok(SlashCommand::Profile(split_words(rest)))),
    },
    #[cfg(feature = "similarity")]
    SlashSpec {
        // Gated exactly as `agentpit similarity` is: a build without the feature has no
        // embedder, so the command does not exist there either.
        name: cow("similarity"),
        aliases: NO_ALIASES,
        forms: Cow::Borrowed(&[Form {
            args: cow("<init|status>"),
            description: cow("kNN routing model: install it, or report what is installed"),
        }]),
        category: Category::Learning,
        surfaces: Cow::Borrowed(&[Surface::Repl, Surface::Tui]),
        exec: ExecKind::Local,
        parse: ParseRule::Fn(|rest| {
            let words = split_words(rest);
            if words.is_empty() {
                Err("usage: /similarity <init|status>")
            } else {
                Ok(SlashCommand::Similarity(words))
            }
        }),
    },
    SlashSpec {
        name: cow("outcome"),
        aliases: NO_ALIASES,
        forms: Cow::Borrowed(&[Form {
            args: cow("<good|bad> [run-id]"),
            description: cow("Label a run good/bad — the strongest signal routing learns from"),
        }]),
        category: Category::Learning,
        surfaces: Cow::Borrowed(&[Surface::Repl, Surface::Tui]),
        exec: ExecKind::Local,
        parse: ParseRule::Fn(|rest| {
            let mut words = split_words(rest).into_iter();
            let verdict = words.next().unwrap_or_default();
            if !matches!(verdict.to_ascii_lowercase().as_str(), "good" | "bad") {
                // Reject here rather than in `outcome::run`: the verdict is the whole
                // command, and a typo must not be recorded as the other verdict.
                return Err(
                    "usage: /outcome <good|bad> [run-id] — omit the id to label the latest run",
                );
            }
            Ok(SlashCommand::Outcome {
                verdict,
                run_id: words.next(),
            })
        }),
    },
    SlashSpec {
        name: cow("doctor"),
        aliases: NO_ALIASES,
        forms: Cow::Borrowed(&[Form {
            args: cow("[--fix]"),
            description: cow("Scan daemon/worker/lease hygiene; --fix clears dead debris only"),
        }]),
        category: Category::Diagnostic,
        surfaces: Cow::Borrowed(&[Surface::Repl, Surface::Tui]),
        exec: ExecKind::Local,
        parse: ParseRule::Fn(|rest| match rest {
            "" => Ok(SlashCommand::Doctor { fix: false }),
            "--fix" => Ok(SlashCommand::Doctor { fix: true }),
            _ => Err("usage: /doctor [--fix]"),
        }),
    },
    SlashSpec {
        name: cow("mcp"),
        aliases: NO_ALIASES,
        forms: Cow::Borrowed(&[Form {
            args: cow("[list|refresh|import]"),
            description: cow("MCP servers: cached prompts, refresh them, import from Claude Code"),
        }]),
        category: Category::Diagnostic,
        surfaces: Cow::Borrowed(&[Surface::Repl, Surface::Tui]),
        // `refresh` starts a server process and `import` writes the config, but neither
        // reaches a backend: no slash form of this command can spend tokens.
        exec: ExecKind::Local,
        // `serve` hands stdin/stdout to a JSON-RPC framing, which on an interactive surface
        // means breaking the surface the user typed it on. It is refused here with a
        // sentence, and is not expressible in the grammar `run_words` parses at all
        // (`mcp_cmd::SlashAction`) — two layers, so removing either one still refuses.
        parse: ParseRule::Fn(|rest| {
            if crate::cli::mcp_cmd::is_serve_word(rest) {
                return Err(
                    "`serve` is a CLI-only action: run `agentpit mcp serve` from a shell. \
                     Here: /mcp [list|refresh|import]",
                );
            }
            Ok(SlashCommand::Mcp(split_words(rest)))
        }),
    },
    SlashSpec {
        name: cow("diagnose"),
        aliases: NO_ALIASES,
        forms: Cow::Borrowed(&[Form {
            args: cow("<task>"),
            description: cow("Dry-run the routing for a task: features → category → backend"),
        }]),
        category: Category::Diagnostic,
        surfaces: Cow::Borrowed(&[Surface::Repl, Surface::Tui]),
        // A dry run: it reads config, profiles and telemetry, and calls no backend.
        exec: ExecKind::Local,
        parse: ParseRule::Fn(|rest| {
            if rest.is_empty() {
                Err("usage: /diagnose <task> — the task text to route, unquoted")
            } else {
                Ok(SlashCommand::Diagnose(rest.to_string()))
            }
        }),
    },
    SlashSpec {
        name: cow("login"),
        aliases: NO_ALIASES,
        forms: Cow::Borrowed(&[Form {
            args: cow("[backend]"),
            description: cow("Launch login flow (defaults to active backend)"),
        }]),
        category: Category::Backend,
        surfaces: Cow::Borrowed(&[Surface::Repl, Surface::Tui]),
        exec: ExecKind::Local,
        parse: ParseRule::Fn(|rest| {
            Ok(SlashCommand::Login(
                (!rest.is_empty()).then(|| rest.to_string()),
            ))
        }),
    },
    SlashSpec {
        name: cow("cwd"),
        aliases: NO_ALIASES,
        forms: Cow::Borrowed(&[
            Form {
                args: cow(""),
                description: cow("Show current working directory"),
            },
            Form {
                args: cow("<path>"),
                description: cow("Change working directory for this session"),
            },
        ]),
        category: Category::Session,
        surfaces: Cow::Borrowed(&[Surface::Repl]),
        exec: ExecKind::Local,
        parse: ParseRule::Fn(|rest| {
            Ok(SlashCommand::Cwd(
                (!rest.is_empty()).then(|| rest.to_string()),
            ))
        }),
    },
    SlashSpec {
        name: cow("clear"),
        aliases: NO_ALIASES,
        forms: Cow::Borrowed(&[Form {
            args: cow(""),
            description: cow("Clear the terminal screen"),
        }]),
        category: Category::Terminal,
        surfaces: Cow::Borrowed(&[Surface::Repl]),
        exec: ExecKind::Local,
        parse: ParseRule::Fn(|_| Ok(SlashCommand::Clear)),
    },
    SlashSpec {
        name: cow("quit"),
        aliases: Cow::Borrowed(&[cow("exit")]),
        forms: Cow::Borrowed(&[Form {
            args: cow(""),
            description: cow("Exit the REPL (the session stays resumable)"),
        }]),
        category: Category::Terminal,
        surfaces: Cow::Borrowed(&[Surface::Repl, Surface::Attach, Surface::Tui]),
        exec: ExecKind::Local,
        parse: ParseRule::Fn(|_| Ok(SlashCommand::Quit)),
    },
    SlashSpec {
        // Same intent as /quit, kept as its own row because only the daemon-backed
        // fullscreen client can promise it: closing the TUI is a pure detach.
        name: cow("detach"),
        aliases: NO_ALIASES,
        forms: Cow::Borrowed(&[Form {
            args: cow(""),
            description: cow("Leave the TUI; the session and any in-flight turn keep running"),
        }]),
        category: Category::Terminal,
        surfaces: Cow::Borrowed(&[Surface::Tui]),
        exec: ExecKind::Local,
        parse: ParseRule::Fn(|_| Ok(SlashCommand::Quit)),
    },
    SlashSpec {
        name: cow("session"),
        aliases: NO_ALIASES,
        forms: Cow::Borrowed(&[Form {
            args: cow(""),
            description: cow("Show this session's id, file, and resume command"),
        }]),
        category: Category::Session,
        surfaces: Cow::Borrowed(&[Surface::Repl]),
        exec: ExecKind::Local,
        parse: ParseRule::Fn(|_| Ok(SlashCommand::SessionInfo)),
    },
    SlashSpec {
        name: cow("sessions"),
        aliases: NO_ALIASES,
        forms: Cow::Borrowed(&[Form {
            args: cow("[sub] [id]"),
            description: cow("List saved sessions; also show <id> / export <id>"),
        }]),
        category: Category::Session,
        surfaces: Cow::Borrowed(&[Surface::Repl, Surface::Tui]),
        exec: ExecKind::Local,
        parse: ParseRule::Fn(|rest| Ok(SlashCommand::Sessions(split_words(rest)))),
    },
    SlashSpec {
        name: cow("tree"),
        aliases: NO_ALIASES,
        forms: Cow::Borrowed(&[Form {
            args: cow(""),
            description: cow("Show the session tree (branches included)"),
        }]),
        category: Category::Session,
        surfaces: Cow::Borrowed(&[Surface::Repl, Surface::Attach, Surface::Tui]),
        exec: ExecKind::Local,
        parse: ParseRule::Fn(|_| Ok(SlashCommand::Tree)),
    },
    SlashSpec {
        name: cow("branch"),
        aliases: NO_ALIASES,
        forms: Cow::Borrowed(&[Form {
            args: cow("<id>"),
            description: cow("Move to a node from /tree; the next turn continues there"),
        }]),
        category: Category::Session,
        // Leaving a branch offers an LLM-written summary of what is left behind.
        exec: ExecKind::Dispatch,
        surfaces: Cow::Borrowed(&[Surface::Repl, Surface::Tui]),
        parse: ParseRule::Fn(|rest| {
            if rest.is_empty() {
                Err("usage: /branch <entry-id> — pick an id from /tree")
            } else {
                Ok(SlashCommand::Branch(rest.to_string()))
            }
        }),
    },
    SlashSpec {
        name: cow("fork"),
        aliases: NO_ALIASES,
        forms: Cow::Borrowed(&[Form {
            args: cow("[id]"),
            description: cow("Copy the path up to a node (default: here) into a new session"),
        }]),
        category: Category::Session,
        surfaces: Cow::Borrowed(&[Surface::Repl, Surface::Tui]),
        exec: ExecKind::Local,
        parse: ParseRule::Fn(|rest| {
            Ok(SlashCommand::Fork(
                (!rest.is_empty()).then(|| rest.to_string()),
            ))
        }),
    },
    SlashSpec {
        name: cow("clone"),
        aliases: NO_ALIASES,
        forms: Cow::Borrowed(&[Form {
            args: cow(""),
            description: cow("Copy the current path into a new session"),
        }]),
        category: Category::Session,
        surfaces: Cow::Borrowed(&[Surface::Repl]),
        exec: ExecKind::Local,
        parse: ParseRule::Fn(|_| Ok(SlashCommand::CloneSession)),
    },
    SlashSpec {
        name: cow("compact"),
        aliases: NO_ALIASES,
        forms: Cow::Borrowed(&[Form {
            args: cow(""),
            description: cow("Summarize history; future turns replay from the summary"),
        }]),
        category: Category::Session,
        surfaces: Cow::Borrowed(&[Surface::Repl, Surface::Tui]),
        exec: ExecKind::Dispatch,
        parse: ParseRule::Fn(|_| Ok(SlashCommand::Compact)),
    },
];

/// Split a command's argument text into argv words, honoring `"` and `'` the way a shell
/// would, so a sub-command that takes a multi-word positional still works from a slash
/// surface (`/arena run "add a login form"`).
///
/// Only the commands whose CLI form takes a clap sub-action use this; free-text commands
/// (`/rescue`, `/explain`, `/diagnose`) keep the whole line, since quoting a task the CLI
/// would have received as one positional would be a second grammar to learn.
///
/// An unterminated quote closes at end of input rather than erroring: this is a prompt,
/// not a shell, and refusing the line would only hide the argument the user meant.
pub fn split_words(rest: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    let mut in_word = false;
    for ch in rest.chars() {
        match quote {
            Some(q) if ch == q => quote = None,
            Some(_) => current.push(ch),
            None if ch == '"' || ch == '\'' => {
                quote = Some(ch);
                in_word = true;
            }
            None if ch.is_whitespace() => {
                if in_word {
                    words.push(std::mem::take(&mut current));
                    in_word = false;
                }
            }
            None => {
                current.push(ch);
                in_word = true;
            }
        }
    }
    if in_word {
        words.push(current);
    }
    words
}

/// An argument the named command accepts, for the "every claimed row runs" probes the
/// registry, the TUI router, and the TUI popup each make over the whole table.
///
/// Most commands take free text or forward their words to clap, so any word will do. The
/// two with a closed vocabulary would (correctly) refuse a nonsense word, so they get one
/// from their own grammar instead of weakening the probe to accept a usage line.
#[cfg(test)]
pub(crate) fn probe_arg(name: &str) -> &'static str {
    match name {
        "outcome" => "good",
        "doctor" => "--fix",
        // Two positionals, so one word would (correctly) be a usage error.
        "refactor" => "some/path a goal",
        _ => "arg",
    }
}

/// A synthetic runtime entry: owned strings plus a closure carrying what the entry was
/// built from — exactly what the old `&'static str` + `fn` pair could not express.
///
/// Stands in for a discovered entry where what is under test is the *layer* — aliases,
/// collisions, ordering — rather than any particular source of entries. The real source
/// ([`crate::cli::skills`]) proves the disk path against its own fixtures; keeping this one
/// synthetic is what lets the tests below claim an alias, which no `SKILL.md` can.
#[cfg(test)]
pub(crate) fn test_extension(
    name: &str,
    aliases: &[&str],
    surfaces: &[Surface],
    body: &str,
) -> SlashSpec {
    let body = body.to_string();
    SlashSpec {
        name: Cow::Owned(name.to_string()),
        aliases: Cow::Owned(aliases.iter().map(|a| Cow::Owned(a.to_string())).collect()),
        forms: Cow::Owned(vec![Form {
            args: cow("[text]"),
            description: Cow::Owned(format!("runtime entry {name}")),
        }]),
        category: Category::Agents,
        surfaces: Cow::Owned(surfaces.to_vec()),
        exec: ExecKind::Dispatch,
        parse: ParseRule::owned(move |rest| Ok(SlashCommand::Rescue(format!("{body} {rest}")))),
    }
}

/// The registry a surface would see if `extensions` had been discovered.
///
/// Leaked on purpose: the surfaces take `&'static Registry` because the real one lives as
/// long as the process. A fixture registry is the same shape, minus the process.
#[cfg(test)]
pub(crate) fn test_registry(extensions: Vec<SlashSpec>) -> &'static Registry {
    Box::leak(Box::new(Registry::resolve(extensions)))
}

/// A registry with one runtime entry, `/skill` (alias `/sk`), on the two surfaces a user
/// types into. The shared fixture behind the completer and dropdown tests.
#[cfg(test)]
pub(crate) fn test_registry_with_skill() -> &'static Registry {
    test_registry(vec![test_extension(
        "skill",
        &["sk"],
        &[Surface::Repl, Surface::Tui],
        "SKILL BODY",
    )])
}

/// The command set a surface actually sees: [`BUILTINS`], plus the runtime entries that
/// were allowed to join them.
///
/// Only the second layer is stored — the first is borrowed from the `static`, never
/// copied — which is also what makes the asymmetry structural:
/// [`resolve`](Registry::resolve) is the only way to add an entry, and it can only append.
pub struct Registry {
    extensions: Vec<SlashSpec>,
}

/// Counts, not rows: a registry holds closures, and the surfaces that carry one only ever
/// want to know how big each layer is.
impl std::fmt::Debug for Registry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Registry")
            .field("builtins", &BUILTINS.len())
            .field("extensions", &self.extensions.len())
            .finish()
    }
}

impl Registry {
    /// Merge runtime entries onto the built-in table.
    ///
    /// An entry is DROPPED — not renamed, not merged — when any of the names it claims is
    /// already taken, by a built-in or by an extension resolved before it. Dropping the
    /// whole entry rather than the colliding name is deliberate: a half-registered command
    /// would answer to a name its author never wrote down.
    pub fn resolve(extensions: impl IntoIterator<Item = SlashSpec>) -> Registry {
        let mut accepted: Vec<SlashSpec> = Vec::new();
        for entry in extensions {
            let collides = entry.names().any(|name| {
                BUILTINS
                    .iter()
                    .chain(accepted.iter())
                    .any(|s| s.matches(name))
            });
            if !collides {
                accepted.push(entry);
            }
        }
        Registry {
            extensions: accepted,
        }
    }

    /// Every command, built-ins first — so table order stays the canonical order and a
    /// runtime entry can never win a tie against a built-in.
    pub fn entries(&self) -> impl Iterator<Item = &SlashSpec> {
        BUILTINS.iter().chain(self.extensions.iter())
    }

    /// Find the command `name` (any case) addresses on `surface`.
    pub fn lookup(&self, name: &str, surface: Surface) -> Option<&SlashSpec> {
        self.entries()
            .find(|s| s.available_on(surface) && s.matches(name))
    }

    /// Every name a surface answers to — canonical names and aliases, in registry order.
    /// This is what feeds `guidance::suggest_slash`, so registry order is the tie-break
    /// order.
    pub fn names_for(&self, surface: Surface) -> Vec<&str> {
        self.entries()
            .filter(|s| s.available_on(surface))
            .flat_map(|s| s.names())
            .collect()
    }

    /// Commands available on `surface`, in help order: registry order, terminal group last.
    pub fn help_order(&self, surface: Surface) -> Vec<&SlashSpec> {
        let on_surface = || self.entries().filter(|s| s.available_on(surface));
        on_surface()
            .filter(|s| s.category != Category::Terminal)
            .chain(on_surface().filter(|s| s.category == Category::Terminal))
            .collect()
    }

    /// Parse one input line for `surface`. See [`parse`] for the guarantee.
    pub fn parse(&self, input: &str, surface: Surface) -> Parsed {
        let trimmed = input.trim();
        if !trimmed.starts_with('/') {
            return Parsed::NotSlash;
        }
        let without_slash = &trimmed[1..];
        let (cmd, rest) = without_slash
            .split_once(char::is_whitespace)
            .map(|(c, r)| (c, r.trim()))
            .unwrap_or((without_slash, ""));

        match self.lookup(cmd, surface) {
            Some(spec) => match spec.parse.apply(rest) {
                Ok(command) => Parsed::Command(command),
                Err(usage) => Parsed::Usage(usage),
            },
            None => Parsed::Unknown {
                typed: cmd.to_string(),
                suggestion: crate::cli::guidance::suggest_slash(
                    &cmd.to_ascii_lowercase(),
                    &self.names_for(surface),
                ),
            },
        }
    }
}

/// The process's resolved registry, once [`install`] has (or has not) filled it.
static RESOLVED: OnceLock<Registry> = OnceLock::new();

/// Resolve the process registry from `extensions` — the discovered layer.
///
/// Called once, from the entry point that knows the session's working directory
/// (`repl::run_repl`, `tui::run`), because the project scope is relative to it. First call
/// wins: a second one cannot change the vocabulary out from under a surface that has
/// already shown it, and reading [`registry`] first leaves the built-ins alone forever.
pub fn install(extensions: Vec<SlashSpec>) -> &'static Registry {
    RESOLVED.get_or_init(|| Registry::resolve(extensions))
}

/// The process's resolved registry: built-ins plus whatever [`install`] fed in.
///
/// One merge, read by every surface — so the REPL, the TUI and their completers cannot
/// disagree about which commands exist.
pub fn registry() -> &'static Registry {
    RESOLVED.get_or_init(|| Registry::resolve([]))
}

/// Find the command `name` (any case) addresses on `surface`.
pub fn lookup(name: &str, surface: Surface) -> Option<&'static SlashSpec> {
    registry().lookup(name, surface)
}

/// Every name a surface answers to — canonical names and aliases, in registry order.
pub fn names_for(surface: Surface) -> Vec<&'static str> {
    registry().names_for(surface)
}

/// Commands available on `surface`, in help order: registry order, terminal group last.
pub fn help_order(surface: Surface) -> Vec<&'static SlashSpec> {
    registry().help_order(surface)
}

/// The help label for one usage form, e.g. `/backend <id>` or `/quit  or  /exit`.
/// Aliases are shown on the primary form only.
pub fn form_label(spec: &SlashSpec, form_index: usize) -> String {
    let mut label = format!("/{}", spec.name);
    if form_index == 0 {
        for alias in spec.aliases.iter() {
            label.push_str(&format!("  or  /{alias}"));
        }
    }
    let args = &spec.forms[form_index].args;
    if !args.is_empty() {
        label.push(' ');
        label.push_str(args);
    }
    label
}

/// Parse one input line for `surface`.
///
/// Never prints and never dispatches. A line that starts with `/` can only come back as
/// [`Parsed::Command`], [`Parsed::Usage`], or [`Parsed::Unknown`] — never as free text —
/// so no typo can become a billable backend call.
pub fn parse(input: &str, surface: Surface) -> Parsed {
    registry().parse(input, surface)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repl_specs() -> impl Iterator<Item = &'static SlashSpec> {
        registry()
            .entries()
            .filter(|s| s.available_on(Surface::Repl))
    }

    /// The names a surface answers to, with the feature-gated rows folded in so the pinned
    /// order below reads the same in both builds.
    #[allow(unused_mut)]
    fn expected_names(
        mut base: Vec<&'static str>,
        before: &str,
        gated: &'static str,
    ) -> Vec<&'static str> {
        #[cfg(feature = "similarity")]
        {
            let at = base.iter().position(|n| *n == before).expect("anchor");
            base.insert(at, gated);
        }
        #[cfg(not(feature = "similarity"))]
        {
            let _ = (before, gated);
        }
        base
    }

    /// `/mcp serve` must be unreachable from every slash surface, in every spelling, while
    /// the three client actions work on the two interactive ones.
    ///
    /// `serve` hands stdin/stdout to a JSON-RPC framing; started from a REPL or a TUI that
    /// is *reading* stdin, it breaks the surface the user typed it on. It stays a CLI-only
    /// action, and this is the outer of the two layers that keep it there (the inner one is
    /// that `mcp_cmd`'s slash grammar has no such variant — see its own tests).
    #[test]
    fn mcp_serve_is_not_reachable_from_any_slash_surface() {
        for surface in [Surface::Repl, Surface::Tui, Surface::Attach] {
            for line in [
                "/mcp serve",
                "/mcp  serve",
                "/mcp SERVE",
                "/mcp Serve --anything",
            ] {
                match parse(line, surface) {
                    // Refused with a sentence on a surface that has /mcp…
                    Parsed::Usage(usage) => {
                        assert!(usage.contains("CLI-only"), "{surface:?} {line}: {usage}")
                    }
                    // …and not a command at all on one that does not.
                    Parsed::Unknown { .. } if surface == Surface::Attach => {}
                    other => panic!("{surface:?} {line} reached {other:?}"),
                }
            }
        }
        // The client actions are reachable, and only where /mcp is claimed.
        for line in ["/mcp", "/mcp list", "/mcp refresh", "/mcp import --apply"] {
            for surface in [Surface::Repl, Surface::Tui] {
                assert!(
                    matches!(parse(line, surface), Parsed::Command(SlashCommand::Mcp(_))),
                    "{surface:?} {line} did not parse"
                );
            }
            assert!(matches!(
                parse(line, Surface::Attach),
                Parsed::Unknown { .. }
            ));
        }
        // The words reach mcp_cmd verbatim, so its clap grammar is what reads them.
        match parse("/mcp refresh --server ctx7", Surface::Repl) {
            Parsed::Command(SlashCommand::Mcp(words)) => {
                assert_eq!(words, vec!["refresh", "--server", "ctx7"])
            }
            other => panic!("{other:?}"),
        }
    }

    // ─── registry ↔ parser agreement ─────────────────────────────────────────────
    // These two tests are the anti-drift contract: the table is the only place a
    // command may be declared, and the parser accepts exactly what the table declares.

    #[test]
    fn every_registry_entry_parses_on_every_surface_it_claims() {
        for spec in registry().entries() {
            assert!(
                !spec.forms.is_empty(),
                "/{} declares no usage form",
                spec.name
            );
            assert!(
                !spec.surfaces.is_empty(),
                "/{} is reachable on no surface",
                spec.name
            );
            for surface in spec.surfaces.iter() {
                for name in spec.names() {
                    // With an argument every command parses; without one, a command that
                    // requires an argument is allowed to answer with its usage line.
                    let arg = probe_arg(&spec.name);
                    assert!(
                        matches!(
                            parse(&format!("/{name} {arg}"), *surface),
                            Parsed::Command(_)
                        ),
                        "/{name} did not parse on {surface:?}"
                    );
                    assert!(
                        matches!(
                            parse(&format!("/{name}"), *surface),
                            Parsed::Command(_) | Parsed::Usage(_)
                        ),
                        "/{name} with no argument was not recognized on {surface:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn the_parser_accepts_nothing_the_registry_does_not_declare() {
        // Any name that is not a registry name (here: each name with a suffix, plus a
        // few plausible near-misses) must come back Unknown, never as a command.
        let probes: Vec<String> = registry()
            .entries()
            .flat_map(|s| s.names())
            .map(|n| format!("{n}zz"))
            .chain(["frobnicate".to_string(), "".to_string()])
            .collect();
        for probe in probes {
            assert!(
                matches!(
                    parse(&format!("/{probe}"), Surface::Repl),
                    Parsed::Unknown { .. }
                ),
                "/{probe} is not in the registry but the parser accepted it"
            );
        }
    }

    #[test]
    fn names_are_unique_across_the_registry() {
        let all: Vec<_> = registry().entries().flat_map(|s| s.names()).collect();
        for name in &all {
            assert_eq!(
                all.iter().filter(|n| *n == name).count(),
                1,
                "{name} is declared more than once"
            );
        }
    }

    // ─── the second layer ────────────────────────────────────────────────────────
    // Fixtures stand in for whatever a later layer discovers: these prove the mechanism
    // (an owned entry, resolved onto the built-ins) without anything reading a disk.

    #[test]
    fn a_runtime_entry_joins_exactly_the_surfaces_it_claims() {
        let reg = test_registry_with_skill();
        // Present on the two surfaces the entry claims, canonical name before alias…
        for surface in [Surface::Repl, Surface::Tui] {
            let names = reg.names_for(surface);
            assert_eq!(
                &names[names.len() - 2..],
                ["skill", "sk"],
                "the runtime entry belongs after the built-ins on {surface:?}"
            );
            assert!(reg.lookup("skill", surface).is_some());
            // Case is not a different command, for a discovered name either.
            assert!(reg.lookup("SKILL", surface).is_some());
            assert!(reg.lookup("sk", surface).is_some());
        }
        // …and absent from the surface it does not claim.
        let attach = reg.names_for(Surface::Attach);
        assert_eq!(attach, vec!["quit", "exit", "tree"]);
        assert!(reg.lookup("skill", Surface::Attach).is_none());
        // The built-in order in front of it is untouched.
        assert_eq!(reg.names_for(Surface::Repl)[..2], ["help", "backend"]);
    }

    #[test]
    fn a_runtime_entry_parses_through_the_same_entry_point() {
        let reg = test_registry_with_skill();
        // The captured body comes back with the argument text: the entry carries state a
        // `fn` pointer could not, and it reaches the command through `parse`, not through
        // a second code path.
        match reg.parse("/skill do the thing", Surface::Repl) {
            Parsed::Command(SlashCommand::Rescue(t)) => {
                assert_eq!(t, "SKILL BODY do the thing");
            }
            other => panic!("expected the runtime entry's command, got {other:?}"),
        }
        match reg.parse("/SK  do the thing  ", Surface::Tui) {
            Parsed::Command(SlashCommand::Rescue(t)) => {
                assert_eq!(t, "SKILL BODY do the thing");
            }
            other => panic!("expected the alias to parse, got {other:?}"),
        }
        // A surface it does not claim still refuses it rather than dispatching.
        assert!(matches!(
            reg.parse("/skill do the thing", Surface::Attach),
            Parsed::Unknown { .. }
        ));
        // And it is a candidate for "did you mean", like any other row.
        match reg.parse("/skil", Surface::Repl) {
            Parsed::Unknown { suggestion, .. } => {
                assert_eq!(suggestion.as_deref(), Some("Did you mean /skill?"))
            }
            other => panic!("expected Unknown, got {other:?}"),
        }
    }

    #[test]
    fn a_runtime_entry_can_never_shadow_a_builtin() {
        let reg = test_registry(vec![
            // Collides on the canonical name…
            test_extension("compact", &[], &[Surface::Repl], "HIJACK"),
            // …on a built-in alias…
            test_extension("bail", &["exit"], &[Surface::Repl], "HIJACK"),
            // …and on case alone.
            test_extension("Clear", &[], &[Surface::Repl], "HIJACK"),
        ]);
        // Nothing was accepted, so the surface vocabulary is exactly the built-in one.
        assert_eq!(reg.names_for(Surface::Repl), names_for(Surface::Repl));
        // The built-ins still resolve to their own commands, not to a stand-in.
        assert!(matches!(
            reg.parse("/compact", Surface::Repl),
            Parsed::Command(SlashCommand::Compact)
        ));
        assert!(matches!(
            reg.parse("/exit", Surface::Repl),
            Parsed::Command(SlashCommand::Quit)
        ));
        assert!(matches!(
            reg.parse("/clear", Surface::Repl),
            Parsed::Command(SlashCommand::Clear)
        ));
        // The whole entry is dropped, not just its colliding name: /bail brought a
        // built-in alias with it and does not get to keep the rest.
        assert!(matches!(
            reg.parse("/bail", Surface::Repl),
            Parsed::Unknown { .. }
        ));
    }

    #[test]
    fn two_runtime_entries_claiming_one_name_resolve_to_the_first() {
        let reg = test_registry(vec![
            test_extension("skill", &["sk"], &[Surface::Repl], "FIRST"),
            test_extension("skill", &[], &[Surface::Repl], "SECOND"),
            // Collides only on the alias — still dropped whole.
            test_extension("other", &["sk"], &[Surface::Repl], "THIRD"),
        ]);
        let names = reg.names_for(Surface::Repl);
        assert_eq!(&names[names.len() - 2..], ["skill", "sk"]);
        assert!(!names.contains(&"other"));
        match reg.parse("/skill now", Surface::Repl) {
            Parsed::Command(SlashCommand::Rescue(t)) => assert_eq!(t, "FIRST now"),
            other => panic!("expected the first entry to win, got {other:?}"),
        }
    }

    #[test]
    fn runtime_entries_sit_between_the_builtins_and_the_terminal_group_in_help() {
        let reg = test_registry_with_skill();
        let order: Vec<&str> = reg
            .help_order(Surface::Repl)
            .iter()
            .map(|s| s.name.as_ref())
            .collect();
        assert_eq!(order.first(), Some(&"help"));
        // Screen control and exit stay last, so a discovered command cannot push /quit off
        // the bottom of `/help`.
        assert_eq!(order.last(), Some(&"quit"));
        let skill = order.iter().position(|n| *n == "skill").expect("offered");
        let compact = order
            .iter()
            .position(|n| *n == "compact")
            .expect("built-in");
        assert!(skill > compact, "runtime entries come after the built-ins");
        assert!(skill < order.len() - 1, "…and before the terminal group");
    }

    // ─── surfaces ────────────────────────────────────────────────────────────────

    #[test]
    fn attach_claims_only_what_the_worker_serves() {
        let attach = names_for(Surface::Attach);
        assert_eq!(attach, vec!["quit", "exit", "tree"]);
        // A REPL-only command must not resolve on attach.
        assert!(lookup("compact", Surface::Attach).is_none());
        assert!(matches!(
            parse("/compact", Surface::Attach),
            Parsed::Unknown { .. }
        ));
    }

    #[test]
    fn the_tui_claims_worker_verbs_plus_the_subcommands_it_suspends_for() {
        // /help is in-screen; /tree, /branch, /fork and /compact are worker verbs served
        // over the protocol; the rest run CLI code with the alternate screen handed back.
        // Anything else the REPL offers is NOT reachable in the TUI and must stay unknown
        // there rather than reaching a backend.
        assert_eq!(
            names_for(Surface::Tui),
            expected_names(
                vec![
                    "help", "status", "learning", "arena", "profile", "outcome", "doctor", "mcp",
                    "diagnose", "login", "quit", "exit", "detach", "sessions", "tree", "branch",
                    "fork", "compact",
                ],
                "outcome",
                "similarity",
            )
        );
        assert!(lookup("menu", Surface::Tui).is_none());
        assert!(matches!(
            parse("/menu", Surface::Tui),
            Parsed::Unknown { .. }
        ));
        // /detach is the TUI's word for /quit and belongs to no other surface.
        assert!(matches!(
            parse("/detach", Surface::Tui),
            Parsed::Command(SlashCommand::Quit)
        ));
        assert!(lookup("detach", Surface::Repl).is_none());
        assert!(lookup("detach", Surface::Attach).is_none());
    }

    #[test]
    fn repl_name_order_is_pinned() {
        // Order is behavior: `suggest_slash` returns the first match, so a reshuffle
        // silently changes which command a typo points at (e.g. `/cl` → /clear vs
        // /clone). Adding a row means extending this list on purpose, not by accident.
        assert_eq!(
            names_for(Surface::Repl),
            expected_names(
                vec![
                    "help",
                    "backend",
                    "status",
                    "config",
                    "menu",
                    "ensemble",
                    "review",
                    "workflow",
                    "rescue",
                    "refactor",
                    "explain",
                    "security-review",
                    "adversarial-review",
                    "learning",
                    "arena",
                    "profile",
                    "outcome",
                    "doctor",
                    "mcp",
                    "diagnose",
                    "login",
                    "cwd",
                    "clear",
                    "quit",
                    "exit",
                    "session",
                    "sessions",
                    "tree",
                    "branch",
                    "fork",
                    "clone",
                    "compact",
                ],
                "outcome",
                "similarity",
            )
        );
        // /review still owns the `/re…` prefix, so the hint an existing user relies on is
        // unchanged by /rescue and /refactor joining the table after it.
        assert_eq!(
            crate::cli::guidance::suggest_slash("re", &names_for(Surface::Repl)),
            Some("Did you mean /review?".into())
        );
    }

    // ─── unknown commands never dispatch ─────────────────────────────────────────

    #[test]
    fn unknown_command_yields_a_suggestion_and_no_command() {
        match parse("/sess", Surface::Repl) {
            Parsed::Unknown { typed, suggestion } => {
                assert_eq!(typed, "sess");
                assert_eq!(suggestion.as_deref(), Some("Did you mean /session?"));
            }
            other => panic!("expected Unknown, got {other:?}"),
        }
    }

    #[test]
    fn unknown_command_keeps_the_typed_case_for_the_message() {
        match parse("/Frobnicate", Surface::Repl) {
            Parsed::Unknown { typed, suggestion } => {
                assert_eq!(typed, "Frobnicate");
                assert_eq!(suggestion, None);
            }
            other => panic!("expected Unknown, got {other:?}"),
        }
    }

    #[test]
    fn a_slash_line_is_never_free_text() {
        // The A4 guarantee, stated as a property: nothing beginning with `/` can leave
        // the parser as NotSlash, so no surface can route it to a backend.
        for line in [
            "/frobnicate",
            "/",
            "/branch",
            "/tmp/some/path is broken",
            "/HELP",
            "  /quit  ",
        ] {
            assert!(
                !matches!(parse(line, Surface::Repl), Parsed::NotSlash),
                "{line:?} escaped the slash parser"
            );
        }
    }

    #[test]
    fn free_text_is_not_slash() {
        assert!(matches!(
            parse("hello world", Surface::Repl),
            Parsed::NotSlash
        ));
        assert!(matches!(parse("", Surface::Repl), Parsed::NotSlash));
        assert!(matches!(
            parse("@claude do a thing", Surface::Repl),
            Parsed::NotSlash
        ));
    }

    // ─── usage errors ────────────────────────────────────────────────────────────

    #[test]
    fn branch_without_an_id_returns_its_usage_line() {
        match parse("/branch", Surface::Repl) {
            Parsed::Usage(u) => assert_eq!(u, "usage: /branch <entry-id> — pick an id from /tree"),
            other => panic!("expected Usage, got {other:?}"),
        }
    }

    // ─── the CLI subcommands, as slash commands ──────────────────────────────────

    #[test]
    fn free_text_commands_keep_the_whole_line_as_one_argument() {
        // These stand for CLI positionals a shell user would have quoted; the slash form
        // takes the rest of the line so there is no second quoting rule to learn.
        match parse("/rescue make the auth test pass", Surface::Repl) {
            Parsed::Command(SlashCommand::Rescue(t)) => assert_eq!(t, "make the auth test pass"),
            other => panic!("expected Rescue, got {other:?}"),
        }
        match parse("/explain the daemon lease protocol", Surface::Repl) {
            Parsed::Command(SlashCommand::Explain(t)) => {
                assert_eq!(t, "the daemon lease protocol")
            }
            other => panic!("expected Explain, got {other:?}"),
        }
        match parse("/security-review src/daemon and src/mcp", Surface::Repl) {
            Parsed::Command(SlashCommand::SecurityReview(t)) => {
                assert_eq!(t, "src/daemon and src/mcp")
            }
            other => panic!("expected SecurityReview, got {other:?}"),
        }
        match parse("/adversarial-review the caching plan", Surface::Repl) {
            Parsed::Command(SlashCommand::AdversarialReview(t)) => {
                assert_eq!(t, "the caching plan")
            }
            other => panic!("expected AdversarialReview, got {other:?}"),
        }
        match parse("/diagnose add a retry to the fetch loop", Surface::Repl) {
            Parsed::Command(SlashCommand::Diagnose(t)) => {
                assert_eq!(t, "add a retry to the fetch loop")
            }
            other => panic!("expected Diagnose, got {other:?}"),
        }
        // …and each refuses an empty line rather than dispatching a blank task.
        for line in [
            "/rescue",
            "/explain",
            "/security-review",
            "/adversarial-review",
            "/diagnose",
        ] {
            assert!(
                matches!(parse(line, Surface::Repl), Parsed::Usage(_)),
                "{line} did not answer with a usage line"
            );
        }
    }

    #[test]
    fn refactor_needs_both_of_the_positionals_the_cli_requires() {
        // `agentpit refactor <path> <goal>` takes two positionals, so one word is a usage
        // error rather than a refactor with an empty goal.
        match parse(
            "/refactor src/cli/mod.rs split the command table out",
            Surface::Repl,
        ) {
            Parsed::Command(SlashCommand::Refactor { path, goal }) => {
                assert_eq!(path, "src/cli/mod.rs");
                assert_eq!(goal, "split the command table out");
            }
            other => panic!("expected Refactor, got {other:?}"),
        }
        assert!(matches!(
            parse("/refactor src/cli/mod.rs", Surface::Repl),
            Parsed::Usage(_)
        ));
        assert!(matches!(
            parse("/refactor", Surface::Repl),
            Parsed::Usage(_)
        ));
    }

    #[test]
    fn outcome_records_only_the_two_verdicts_the_cli_accepts() {
        // The verdict IS the command, and it is the strongest label the routing layer
        // trains on: a typo must be refused here, not guessed at.
        match parse("/outcome good", Surface::Tui) {
            Parsed::Command(SlashCommand::Outcome { verdict, run_id }) => {
                assert_eq!(verdict, "good");
                assert_eq!(run_id, None);
            }
            other => panic!("expected Outcome, got {other:?}"),
        }
        match parse("/outcome BAD run-7", Surface::Repl) {
            Parsed::Command(SlashCommand::Outcome { verdict, run_id }) => {
                assert_eq!(verdict, "BAD", "case is normalized downstream, not here");
                assert_eq!(run_id.as_deref(), Some("run-7"));
            }
            other => panic!("expected Outcome, got {other:?}"),
        }
        for line in ["/outcome", "/outcome goof", "/outcome ok run-7"] {
            assert!(
                matches!(parse(line, Surface::Repl), Parsed::Usage(_)),
                "{line} was not refused"
            );
        }
    }

    #[test]
    fn doctor_takes_only_its_one_flag() {
        assert!(matches!(
            parse("/doctor", Surface::Repl),
            Parsed::Command(SlashCommand::Doctor { fix: false })
        ));
        assert!(matches!(
            parse("/doctor --fix", Surface::Repl),
            Parsed::Command(SlashCommand::Doctor { fix: true })
        ));
        assert!(matches!(
            parse("/doctor --wipe", Surface::Repl),
            Parsed::Usage(_)
        ));
    }

    #[test]
    fn sub_action_commands_forward_their_words_to_clap() {
        // /arena, /profile, /sessions (and /similarity) carry argv words rather than a
        // re-modelled enum, so `agentpit <name>`'s own grammar is the only grammar.
        match parse("/arena vote --round r7", Surface::Repl) {
            Parsed::Command(SlashCommand::Arena(words)) => {
                assert_eq!(words, vec!["vote", "--round", "r7"]);
            }
            other => panic!("expected Arena, got {other:?}"),
        }
        // A multi-word positional survives, quoted the way the shell form needs it.
        match parse("/arena run \"add a login form\"", Surface::Repl) {
            Parsed::Command(SlashCommand::Arena(words)) => {
                assert_eq!(words, vec!["run", "add a login form"]);
            }
            other => panic!("expected Arena, got {other:?}"),
        }
        // The optional-sub-action commands are legal bare: they mean their default.
        match parse("/profile", Surface::Repl) {
            Parsed::Command(SlashCommand::Profile(words)) => assert!(words.is_empty()),
            other => panic!("expected Profile, got {other:?}"),
        }
        match parse("/sessions show a1b2c3d4", Surface::Repl) {
            Parsed::Command(SlashCommand::Sessions(words)) => {
                assert_eq!(words, vec!["show", "a1b2c3d4"]);
            }
            other => panic!("expected Sessions, got {other:?}"),
        }
        // …while a required sub-action is a usage error, not an empty word list.
        assert!(matches!(parse("/arena", Surface::Repl), Parsed::Usage(_)));
    }

    #[test]
    fn exec_kind_marks_exactly_the_rows_that_can_reach_a_backend() {
        // ExecKind is the token-spend warning, so it is pinned per row rather than
        // inferred: a Local row that grows a dispatch must be changed here on purpose.
        // Pinned over the built-ins — a runtime entry's ExecKind comes from its author,
        // not from this file.
        let dispatch: Vec<&str> = BUILTINS
            .iter()
            .filter(|s| s.exec == ExecKind::Dispatch)
            .map(|s| s.name.as_ref())
            .collect();
        assert_eq!(
            dispatch,
            vec![
                "ensemble",
                "review",
                "workflow",
                "rescue",
                "refactor",
                "explain",
                "security-review",
                "adversarial-review",
                // `arena run` spends a full agentic run per contender; `profile run
                // --backend <id>` measures a backend live.
                "arena",
                "profile",
                "branch",
                "compact",
            ]
        );
    }

    #[test]
    fn split_words_honors_quotes_and_collapses_whitespace() {
        assert_eq!(split_words(""), Vec::<String>::new());
        assert_eq!(
            split_words("  vote   --round  r7 "),
            vec!["vote", "--round", "r7"]
        );
        assert_eq!(
            split_words("run \"add a login form\" --model opus"),
            vec!["run", "add a login form", "--model", "opus"]
        );
        assert_eq!(
            split_words("show 'round seven' --reveal"),
            vec!["show", "round seven", "--reveal"]
        );
        // An unterminated quote keeps the rest as one word instead of refusing the line.
        assert_eq!(split_words("run \"add a login"), vec!["run", "add a login"]);
        // Quotes glued to a word are removed, not kept as characters.
        assert_eq!(
            split_words("--focus=\"auth only\""),
            vec!["--focus=auth only"]
        );
    }

    // ─── help rendering helpers ──────────────────────────────────────────────────

    #[test]
    fn help_order_puts_terminal_commands_last() {
        let order: Vec<_> = help_order(Surface::Repl)
            .iter()
            .map(|s| s.name.as_ref())
            .collect();
        assert_eq!(order.last(), Some(&"quit"));
        assert_eq!(order[order.len() - 2], "clear");
        assert_eq!(order.first(), Some(&"help"));
        assert_eq!(order.len(), repl_specs().count());
    }

    #[test]
    fn form_labels_render_aliases_and_arguments() {
        let quit = lookup("quit", Surface::Repl).unwrap();
        assert_eq!(form_label(quit, 0), "/quit  or  /exit");
        let backend = lookup("backend", Surface::Repl).unwrap();
        assert_eq!(form_label(backend, 0), "/backend");
        assert_eq!(form_label(backend, 1), "/backend <id>");
    }

    #[test]
    fn primary_form_accessors_read_the_first_form() {
        let backend = lookup("backend", Surface::Repl).unwrap();
        assert_eq!(backend.arg_hint(), "");
        assert_eq!(backend.description(), "Show active backend and transport");
        assert_eq!(backend.category, Category::Backend);
        assert_eq!(backend.exec, ExecKind::Local);
        let compact = lookup("compact", Surface::Repl).unwrap();
        assert_eq!(compact.exec, ExecKind::Dispatch);
    }
}
