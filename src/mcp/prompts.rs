//! Cached MCP prompts as slash commands — the second half of the same seam
//! [`crate::cli::skills`] fills.
//!
//! A refreshed server's prompts become one registry row each, named `<server>:<prompt>`. The
//! colon is what keeps a server's vocabulary from colliding with agentpit's own: a built-in
//! never contains one, so no MCP server can take `/compact`, and two servers that both
//! advertise `review` stay distinguishable. (The registry's own collision rule still
//! applies on top — a duplicate name is dropped whole.)
//!
//! ## What the surfaces read, and when a server runs
//!
//! Two phases, and the split is the whole design:
//!
//! * **Listing** ([`discover`]) reads two files — the config plus the project's `.mcp.json`
//!   for the definitions, and the prompt cache for the prompt names. It spawns nothing, so a
//!   plain startup starts no child. A server that has never been refreshed, or whose
//!   definition changed since, contributes no commands and is reported by `mcp list`.
//! * **Invoking** ([`invoke`]) is what the user just typed, so it may cost a process: it
//!   fetches the prompt's body with `prompts/get`, because the body is not cached — only the
//!   listing is. The composed turn is therefore the server's own messages, not a description
//!   of them.
//!
//! A server that cannot be reached at invoke time yields an `Err`, which both surfaces show
//! as a refusal. Nothing falls through to a free-text turn: there is no body to send, and a
//! turn made of the user's own words would be a different request than the one they asked
//! for (§7.2 A4, the same rule that keeps a typo off the dispatch).
//!
//! ## What reaches a UI
//!
//! A prompt's name and description come off a wire. Both go through the skills layer's own
//! gate ([`crate::cli::skills::render_safe`] and its character class) rather than a second
//! sanitizer written here — they land in the same `/help` table and the same TUI popup
//! column, and one of the two passes would eventually drift.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Result, anyhow};

use crate::cli::skills::{PATH_WIDTH, render_path, render_safe, size_label};
use crate::cli::slash::{Category, ExecKind, Form, ParseRule, SlashCommand, SlashSpec, Surface};

use super::cache::{CachedArgument, CachedPrompt, PromptCache};
use super::servers::{self, ServerDef};

/// Clip for a rendered description, matching the skills layer's own budget: these land in
/// the same `/help` table and the same TUI popup column.
const DESCRIPTION_WIDTH: usize = 80;

/// One message of a fetched prompt, flattened to the pair a text turn can carry.
///
/// agentpit's own type rather than rmcp's, so [`compose`] is a pure function over something
/// a test can build in one line — and so a wire-model change is confined to
/// [`super::client::to_fetched`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchedMessage {
    /// `user` or `assistant`.
    pub role: String,
    pub text: String,
}

/// Everything one `/<server>:<prompt>` invocation needs, captured when the line was parsed.
///
/// Parsing stays synchronous and pure — it cannot spawn a server — so it produces this
/// *intent* and the surface executes it. Both surfaces call [`invoke`] on it and get back
/// either a turn to send or a refusal to print.
#[derive(Debug, Clone, PartialEq)]
pub struct Invocation {
    /// The slash name, `<server>:<prompt>`, for the transcript echo and the provenance line.
    pub name: String,
    /// The definition to spawn. Carried rather than re-read so the invocation cannot end up
    /// talking to a server the user redefined mid-session.
    pub def: ServerDef,
    /// The prompt as `prompts/list` advertised it: its wire name (original case) and the
    /// arguments it declares.
    pub prompt: CachedPrompt,
    /// What the user typed after the command name, untouched.
    pub arg: String,
    /// Per-server budget for the handshake and for `prompts/get`.
    pub budget: Duration,
}

/// A fetched prompt, ready to send.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Composed {
    /// The dim line the surface prints BEFORE sending — see [`provenance`].
    pub provenance: String,
    /// The turn text.
    pub turn: String,
}

/// What the user's argument text mapped onto.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Arguments {
    /// Values for arguments the prompt declared. Only these go on the wire.
    pub sent: BTreeMap<String, String>,
    /// Declared-and-required arguments left with no value.
    pub missing: Vec<String>,
    /// Text that mapped onto no declared argument. Not dropped — [`compose`] appends it to
    /// the turn as the user's own request.
    pub leftover: String,
}

/// The registry rows the cache can justify for `cwd`.
///
/// Reads the config itself rather than taking one: the two callers ([`crate::cli::repl`],
/// [`crate::tui`]) already loaded a config, but this must also be callable from a surface
/// that has not, and a failed load here is simply "no MCP commands" rather than a failed
/// startup.
pub fn discover(cwd: &Path) -> Vec<SlashSpec> {
    let Ok(loaded) = crate::config::load_config(None) else {
        return Vec::new();
    };
    let servers = servers::gather(&loaded.config, cwd);
    let budget = super::client::timeout_from(loaded.config.mcp.connect_timeout_secs);
    specs_from(&servers.defs, &PromptCache::load(), budget)
}

/// [`discover`] over explicit inputs — the seam the tests drive without touching the
/// machine's config or state directory.
pub fn specs_from(defs: &[ServerDef], cache: &PromptCache, budget: Duration) -> Vec<SlashSpec> {
    let mut specs = Vec::new();
    for def in defs.iter().filter(|d| d.enabled) {
        let Some(entry) = cache.fresh_for(def) else {
            continue;
        };
        for prompt in &entry.prompts {
            if let Some(spec) = spec(def, prompt, budget) {
                specs.push(spec);
            }
        }
    }
    specs
}

/// `<server>:<prompt>`, lowercased, or `None` when either half cannot be a command name.
///
/// The server name comes from a config file and the prompt name comes off the wire; neither
/// is trusted to be a word. The character class is the skills layer's own: a name with
/// whitespace could never be typed back (the parser splits the command off at the first
/// space), and one with a slash or a control character would render as something it is not.
///
/// Lowercased for the same reason a skill's name is — every surface filters on a lowercased
/// prefix, so an upper-case row would be listed and never match what the user types. The
/// prompt's ORIGINAL name still goes on the wire: [`Invocation`] carries the [`CachedPrompt`],
/// not this string.
pub fn command_name(server: &str, prompt: &str) -> Option<String> {
    fn usable(part: &str) -> bool {
        !part.is_empty()
            && part.len() <= 64
            && part
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    }
    (usable(server) && usable(prompt)).then(|| format!("{server}:{prompt}").to_ascii_lowercase())
}

/// The argument hint the popup and `/help` show: the prompt's own declared arguments,
/// required ones in `<>` and optional ones in `[]`.
///
/// A prompt with no arguments still takes free text (it becomes the request trailer), which
/// is why the fallback is `[text]` rather than nothing.
fn arg_hint(prompt: &CachedPrompt) -> String {
    if prompt.arguments.is_empty() {
        return "[text]".to_string();
    }
    prompt
        .arguments
        .iter()
        .map(|a| {
            let name = render_safe(&a.name, 32);
            if a.required {
                format!("<{name}>")
            } else {
                format!("[{name}]")
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn spec(def: &ServerDef, prompt: &CachedPrompt, budget: Duration) -> Option<SlashSpec> {
    let name = command_name(&def.name, &prompt.name)?;
    let description = render_safe(&prompt.description, DESCRIPTION_WIDTH);
    let description = if description.is_empty() {
        format!(
            "MCP prompt from {}",
            render_safe(&def.name, DESCRIPTION_WIDTH)
        )
    } else {
        description
    };
    let invocation = Invocation {
        name: name.clone(),
        def: def.clone(),
        prompt: prompt.clone(),
        arg: String::new(),
        budget,
    };
    Some(SlashSpec {
        name: Cow::Owned(name),
        aliases: Cow::Owned(Vec::new()),
        forms: Cow::Owned(vec![Form {
            args: Cow::Owned(arg_hint(prompt)),
            description: Cow::Owned(description),
        }]),
        category: Category::Agents,
        surfaces: Cow::Owned(vec![Surface::Repl, Surface::Tui]),
        // Running it spends tokens, exactly like a skill.
        exec: ExecKind::Dispatch,
        // Parsing only records the intent — it never reaches the server. The fetch belongs
        // to `invoke`, which the surface awaits, because a parser that spawned a process
        // would do so on every keystroke-completed line and could not report a failure.
        parse: ParseRule::owned(move |rest| {
            Ok(SlashCommand::McpPrompt(Box::new(Invocation {
                arg: rest.to_string(),
                ..invocation.clone()
            })))
        }),
    })
}

/// Map the user's argument text onto the arguments the prompt declares.
///
/// Pure, and the only place the mapping is decided. Three shapes, in order:
///
/// * `name=value`, where `name` is one the prompt DECLARED — that argument, verbatim.
///   Quoting works because the words are split the way every other slash argument is
///   (`name="two words"`).
/// * anything else, joined back into one string — the first declared argument that has no
///   value yet. This is the ordinary case: `/ctx7:docs ratatui scrolling` fills `library`
///   without the user having to learn a syntax.
/// * whatever is left when there is no declared argument to take it — [`Arguments::leftover`],
///   which [`compose`] appends as the user's request rather than discarding.
///
/// A `name=value` whose name the prompt does NOT declare is deliberately not special-cased
/// into an argument: agentpit never invents a parameter name for someone else's server. It
/// falls through to the positional path, so the text the user typed still reaches the turn.
pub fn map_arguments(declared: &[CachedArgument], text: &str) -> Arguments {
    let mut sent: BTreeMap<String, String> = BTreeMap::new();
    let mut positional: Vec<String> = Vec::new();
    for word in crate::cli::slash::split_words(text) {
        match word.split_once('=') {
            Some((key, value)) if declared.iter().any(|d| d.name == key) => {
                sent.insert(key.to_string(), value.to_string());
            }
            _ => positional.push(word),
        }
    }
    let rest = positional.join(" ");
    let mut leftover = String::new();
    if !rest.is_empty() {
        match declared.iter().find(|d| !sent.contains_key(&d.name)) {
            Some(first) => {
                sent.insert(first.name.clone(), rest);
            }
            None => leftover = rest,
        }
    }
    let missing = declared
        .iter()
        .filter(|d| d.required && !sent.contains_key(&d.name))
        .map(|d| d.name.clone())
        .collect();
    Arguments {
        sent,
        missing,
        leftover,
    }
}

/// The turn a fetched MCP prompt becomes.
///
/// Pure, and the only place the layout is decided, so the REPL and the TUI cannot disagree
/// about what `/<server>:<prompt>` sends. The order mirrors [`crate::cli::skills::compose`]:
/// what to do, then the body, then the user's own words last, nearest the reply.
///
/// Nothing here slices, so every boundary is a char boundary: each message's text and the
/// leftover are appended whole, however many bytes their characters happen to take.
pub fn compose(prompt: &str, server: &str, messages: &[FetchedMessage], leftover: &str) -> String {
    let mut turn =
        format!("Follow the MCP prompt \"{prompt}\" from the server \"{server}\" for this turn.");
    for message in messages {
        turn.push_str(&format!("\n\n[{}]\n{}", message.role, message.text));
    }
    let leftover = leftover.trim();
    if !leftover.is_empty() {
        turn.push_str(&format!("\n\n---\n\nThe user's request: {leftover}"));
    }
    turn
}

/// The line a surface prints BEFORE it sends: which prompt, how much text it adds, and which
/// server definition it came out of.
///
/// Deliberately the same shape as [`crate::cli::skills::provenance`] — `[kind /name — size
/// from source]`, with the same size format — because a reader compares these lines to see
/// what a keystroke just sent, and two spellings would make that a conversion. For a skill
/// the source is the file; here it is the server and the file that defines it.
pub fn provenance(name: &str, server: &str, origin: &str, turn: &str) -> String {
    format!(
        "[mcp /{name} — {} from {} ({})]",
        size_label(turn.len()),
        render_safe(server, PATH_WIDTH),
        render_path(&PathBuf::from(origin), PATH_WIDTH)
    )
}

/// Fetch the prompt this invocation names and compose the turn it becomes.
///
/// The one place a `/<server>:<prompt>` command reaches the network, and the only reason
/// [`super::client`] is called outside `mcp refresh`. Every failure is an `Err` whose message
/// is what the surface shows instead of dispatching:
///
/// * a required argument the user did not supply — refused HERE, before anything is spawned,
///   naming the argument and how to pass it. The server would only answer with its own
///   opaque "invalid arguments", after a process start the user gains nothing from.
/// * the server could not be started, would not shake hands, or refused `prompts/get`.
/// * the server answered with no messages: there is no prompt to follow, and sending the
///   header alone would spend a turn on nothing.
pub async fn invoke(invocation: &Invocation) -> Result<Composed> {
    let Invocation {
        name,
        def,
        prompt,
        arg,
        budget,
    } = invocation;
    let arguments = map_arguments(&prompt.arguments, arg);
    if !arguments.missing.is_empty() {
        return Err(anyhow!(
            "/{name} needs {}. Usage: /{name} {}",
            arguments
                .missing
                .iter()
                .map(|a| format!("`{}`", render_safe(a, 32)))
                .collect::<Vec<_>>()
                .join(" and "),
            arg_hint(prompt)
        ));
    }
    let messages = super::client::get_prompt(def, &prompt.name, &arguments.sent, *budget).await?;
    if messages.iter().all(|m| m.text.trim().is_empty()) {
        return Err(anyhow!(
            "MCP server '{}' returned no prompt text for '{}'; nothing was sent",
            def.name,
            prompt.name
        ));
    }
    let turn = compose(&prompt.name, &def.name, &messages, &arguments.leftover);
    Ok(Composed {
        provenance: provenance(name, &def.name, &def.origin.label(), &turn),
        turn,
    })
}

// ─── test fixtures ───────────────────────────────────────────────────────────────
// Shared with the surface tests (`tui::completion`, `cli::repl::completion`), so what they
// prove is "a refreshed server reaches this surface", not "a hand-built struct does".

/// The fixture server and prompt the surface tests look for: `/ctx7:docs`.
#[cfg(test)]
pub(crate) const TEST_COMMAND: &str = "ctx7:docs";

/// The fixture definition behind [`TEST_COMMAND`].
#[cfg(test)]
pub(crate) fn test_def() -> ServerDef {
    ServerDef {
        name: "ctx7".into(),
        command: "npx".into(),
        args: vec!["-y".into(), "@upstash/context7-mcp".into()],
        env: std::collections::BTreeMap::new(),
        cwd: String::new(),
        enabled: true,
        origin: super::servers::Origin::Config,
    }
}

/// A cache holding [`test_def`]'s one prompt, `docs`, with one required argument.
#[cfg(test)]
pub(crate) fn test_cache(def: &ServerDef) -> PromptCache {
    let mut cache = PromptCache::default();
    cache.put(
        def,
        vec![CachedPrompt {
            name: "docs".into(),
            description: "Look up library docs".into(),
            arguments: vec![CachedArgument {
                name: "library".into(),
                required: true,
            }],
        }],
    );
    cache
}

/// A resolved registry carrying one refreshed server's prompts, built the way a real
/// session builds them: a definition, a cache filled by a refresh, then [`specs_from`].
#[cfg(test)]
pub(crate) fn test_registry_from_cache() -> &'static crate::cli::slash::Registry {
    let def = test_def();
    let cache = test_cache(&def);
    crate::cli::slash::test_registry(specs_from(&[def], &cache, Duration::from_secs(1)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::slash::{Parsed, Registry};
    use crate::mcp::servers::Origin;

    /// A budget no test ever waits out: nothing here reaches a live server.
    fn budget() -> Duration {
        Duration::from_millis(500)
    }

    fn def(name: &str) -> ServerDef {
        ServerDef {
            name: name.into(),
            command: "npx".into(),
            args: vec![],
            env: BTreeMap::new(),
            cwd: String::new(),
            enabled: true,
            origin: Origin::Config,
        }
    }

    fn prompt(name: &str) -> CachedPrompt {
        CachedPrompt {
            name: name.into(),
            description: "Look up library docs".into(),
            arguments: vec![CachedArgument {
                name: "library".into(),
                required: true,
            }],
        }
    }

    fn args(names: &[(&str, bool)]) -> Vec<CachedArgument> {
        names
            .iter()
            .map(|(name, required)| CachedArgument {
                name: (*name).to_string(),
                required: *required,
            })
            .collect()
    }

    fn message(role: &str, text: &str) -> FetchedMessage {
        FetchedMessage {
            role: role.to_string(),
            text: text.to_string(),
        }
    }

    // ─── the rows a cached list becomes ──────────────────────────────────────────

    #[test]
    fn a_refreshed_server_contributes_one_command_per_prompt() {
        let d = def("ctx7");
        let mut cache = PromptCache::default();
        cache.put(&d, vec![prompt("docs"), prompt("examples")]);
        let specs = specs_from(std::slice::from_ref(&d), &cache, budget());
        let names: Vec<&str> = specs.iter().map(|s| s.name.as_ref()).collect();
        assert_eq!(names, vec!["ctx7:docs", "ctx7:examples"]);
        assert!(specs[0].available_on(Surface::Repl));
        assert!(specs[0].available_on(Surface::Tui));
        assert!(!specs[0].available_on(Surface::Attach));
    }

    /// The rows have to be visible where a user finds commands: the REPL completer and the
    /// TUI dropdown both read `names_for`, and neither knows this layer exists.
    #[test]
    fn cached_prompts_are_visible_on_both_interactive_surfaces() {
        let reg = test_registry_from_cache();
        for surface in [Surface::Repl, Surface::Tui] {
            let names = reg.names_for(surface);
            assert_eq!(
                names.last(),
                Some(&TEST_COMMAND),
                "the MCP row belongs after the built-ins on {surface:?}"
            );
        }
        // Not on `attach`: its worker serves a fixed set of verbs.
        assert!(!reg.names_for(Surface::Attach).contains(&TEST_COMMAND));
        // And the two surfaces that do carry it complete it exactly as a built-in.
        assert_eq!(
            crate::cli::repl::completion::command_candidates_in(reg, "ctx7"),
            vec![TEST_COMMAND]
        );
    }

    #[test]
    fn an_unrefreshed_or_stale_or_disabled_server_contributes_nothing() {
        let d = def("ctx7");
        // Never refreshed.
        assert!(specs_from(std::slice::from_ref(&d), &PromptCache::default(), budget()).is_empty());

        // Refreshed, then the definition changed under it.
        let mut cache = PromptCache::default();
        cache.put(&d, vec![prompt("docs")]);
        let mut moved = d.clone();
        moved.args = vec!["--now-with-args".into()];
        assert!(specs_from(std::slice::from_ref(&moved), &cache, budget()).is_empty());

        // Refreshed and unchanged, but switched off.
        let mut off = d.clone();
        off.enabled = false;
        assert!(specs_from(std::slice::from_ref(&off), &cache, budget()).is_empty());
    }

    /// The name comes off the wire. Anything that could not be typed back — or that would
    /// render as something else — costs that one prompt, not the server.
    #[test]
    fn unusable_names_are_skipped_and_the_rest_survive() {
        let d = def("ctx7");
        let mut cache = PromptCache::default();
        let mut with_junk = prompt("has space");
        with_junk.name = "has space".into();
        let mut slashed = prompt("x");
        slashed.name = "/etc/passwd".into();
        let mut ansi = prompt("x");
        ansi.name = "esc\u{1b}[31m".into();
        cache.put(
            &d,
            vec![
                with_junk,
                slashed,
                ansi,
                prompt("fine"),
                CachedPrompt {
                    name: String::new(),
                    description: String::new(),
                    arguments: vec![],
                },
            ],
        );
        let specs = specs_from(std::slice::from_ref(&d), &cache, budget());
        assert_eq!(
            specs.iter().map(|s| s.name.as_ref()).collect::<Vec<_>>(),
            vec!["ctx7:fine"]
        );
    }

    /// A prompt name is lowercased for the same reason a skill's is: every surface filters
    /// on a lowercased prefix. The WIRE name keeps its case — it is the server's.
    #[test]
    fn a_mixed_case_prompt_name_is_lowercased_for_the_surface_only() {
        let d = def("ctx7");
        let mut cache = PromptCache::default();
        let mut shouty = prompt("Docs");
        shouty.name = "Docs".into();
        cache.put(&d, vec![shouty]);
        let reg = Registry::resolve(specs_from(std::slice::from_ref(&d), &cache, budget()));
        assert_eq!(reg.names_for(Surface::Repl).last(), Some(&"ctx7:docs"));
        match reg.parse("/ctx7:docs react", Surface::Repl) {
            Parsed::Command(SlashCommand::McpPrompt(inv)) => {
                assert_eq!(inv.prompt.name, "Docs", "the wire name keeps its case");
            }
            other => panic!("{other:?}"),
        }
    }

    /// A description is outside input on the way to a terminal, and goes through the skills
    /// layer's gate — not a second sanitizer written next to it.
    #[test]
    fn a_description_is_sanitized_and_clipped_exactly_as_a_skill_is() {
        let d = def("s");
        let mut noisy = prompt("p");
        noisy.description = format!("\u{1b}[31mline one\u{1b}[0m\nline\ttwo {}", "x".repeat(200));
        let mut blank = prompt("q");
        blank.description = "\u{1b}[2J".into();
        let mut cache = PromptCache::default();
        cache.put(&d, vec![noisy, blank]);
        let specs = specs_from(std::slice::from_ref(&d), &cache, budget());

        let rendered = specs[0].description();
        assert!(!rendered.chars().any(|c| c.is_control()), "{rendered:?}");
        assert!(!rendered.contains("31m"), "{rendered:?}");
        // Only the first line survives, exactly as a skill's description does.
        assert_eq!(rendered, "line one");
        // One that sanitizes to nothing falls back rather than rendering as a blank row.
        assert_eq!(specs[1].description(), "MCP prompt from s");

        // And a long one is clipped to the shared width, on a char boundary.
        let mut long = prompt("r");
        long.description = "日".repeat(300);
        let mut cache = PromptCache::default();
        cache.put(&d, vec![long]);
        let specs = specs_from(std::slice::from_ref(&d), &cache, budget());
        assert_eq!(specs[0].description().chars().count(), DESCRIPTION_WIDTH);
    }

    /// The hint is what tells a user an argument is required before they press Enter.
    #[test]
    fn the_argument_hint_reads_the_prompts_own_arguments() {
        let mut none = prompt("p");
        none.arguments.clear();
        assert_eq!(arg_hint(&none), "[text]");
        let mut two = prompt("p");
        two.arguments = args(&[("library", true), ("version", false)]);
        assert_eq!(arg_hint(&two), "<library> [version]");
    }

    // ─── argument mapping ────────────────────────────────────────────────────────

    #[test]
    fn argument_mapping_covers_none_one_and_a_name_the_prompt_never_declared() {
        // Zero declared arguments: nothing goes on the wire, and the text is kept as the
        // request trailer rather than being dropped.
        let none = map_arguments(&[], "summarize the diff");
        assert!(none.sent.is_empty());
        assert!(none.missing.is_empty());
        assert_eq!(none.leftover, "summarize the diff");
        // …and a bare invocation of an argument-less prompt asks for nothing at all.
        assert_eq!(map_arguments(&[], "  "), Arguments::default());

        // One declared argument: the whole line fills it, no syntax to learn.
        let one = map_arguments(&args(&[("library", true)]), "ratatui scrolling");
        assert_eq!(one.sent["library"], "ratatui scrolling");
        assert!(one.missing.is_empty() && one.leftover.is_empty());
        // Naming it explicitly works too, quoting included.
        let named = map_arguments(&args(&[("library", true)]), "library=\"ratatui 0.29\"");
        assert_eq!(named.sent["library"], "ratatui 0.29");

        // A name the prompt does NOT declare is never sent as an argument — agentpit does
        // not invent parameters for someone else's server. The text still reaches the turn.
        let unknown = map_arguments(&args(&[("library", true)]), "topic=lifetimes");
        assert_eq!(unknown.sent.len(), 1);
        assert!(!unknown.sent.contains_key("topic"));
        assert_eq!(unknown.sent["library"], "topic=lifetimes");
        let nowhere = map_arguments(&[], "topic=lifetimes");
        assert!(nowhere.sent.is_empty());
        assert_eq!(nowhere.leftover, "topic=lifetimes");

        // Several: named ones bind, the rest fills the first still-empty declared argument.
        let mixed = map_arguments(
            &args(&[("library", true), ("version", false)]),
            "version=0.29 ratatui scrolling",
        );
        assert_eq!(mixed.sent["version"], "0.29");
        assert_eq!(mixed.sent["library"], "ratatui scrolling");
        assert!(mixed.leftover.is_empty());

        // A required argument with nothing to fill it is reported, not guessed at.
        let bare = map_arguments(&args(&[("library", true), ("version", false)]), "");
        assert_eq!(bare.missing, vec!["library"]);
        assert!(bare.sent.is_empty());
        // An optional one never counts as missing.
        assert!(
            map_arguments(&args(&[("version", false)]), "")
                .missing
                .is_empty()
        );
    }

    #[test]
    fn argument_mapping_is_multibyte_safe() {
        let mapped = map_arguments(
            &args(&[("library", true)]),
            "  ラタトゥイ — 🔍 スクロール  ",
        );
        assert_eq!(mapped.sent["library"], "ラタトゥイ — 🔍 スクロール");
    }

    // ─── the composed turn ───────────────────────────────────────────────────────

    #[test]
    fn the_composed_turn_carries_the_servers_own_messages_and_the_request_last() {
        let messages = [
            message("user", "Look up the docs for the named library."),
            message("assistant", "Which version?"),
        ];
        let turn = compose("docs", "ctx7", &messages, "  and cite the source  ");
        assert!(
            turn.starts_with("Follow the MCP prompt \"docs\" from the server \"ctx7\""),
            "{turn}"
        );
        assert!(turn.contains("[user]\nLook up the docs"), "{turn}");
        assert!(turn.contains("[assistant]\nWhich version?"), "{turn}");
        // Order is fixed, not incidental: the body first, the user's words last and nearest
        // the reply, so a long prompt cannot bury the request mid-turn.
        assert!(
            turn.ends_with("The user's request: and cite the source"),
            "{turn}"
        );
        assert!(turn.find("Which version?").unwrap() < turn.find("cite the source").unwrap());

        // With nothing left over there is no request section at all.
        let bare = compose("docs", "ctx7", &messages, "   ");
        assert!(!bare.contains("The user's request"), "{bare}");
        assert!(bare.ends_with("Which version?"), "{bare}");

        // A server that sent nothing composes to the header alone — which `invoke` refuses
        // rather than dispatching (see the test below).
        assert_eq!(
            compose("docs", "ctx7", &[], ""),
            "Follow the MCP prompt \"docs\" from the server \"ctx7\" for this turn."
        );
    }

    /// Composition never slices, so nothing in it can land mid-character — which in Rust
    /// would be a panic, not a mangled string.
    #[test]
    fn composition_is_multibyte_safe_in_the_body_and_in_the_request() {
        let body = "レビュー担当者に、具体的な反論を三つ求めてください。";
        let leftover = "  キャッシュ計画について — 🔍 見てほしい  ";
        let turn = compose("批評", "サーバ", &[message("user", body)], leftover);
        assert!(turn.contains(body), "{turn}");
        assert!(
            turn.starts_with("Follow the MCP prompt \"批評\" from the server \"サーバ\""),
            "{turn}"
        );
        assert!(
            turn.ends_with("The user's request: キャッシュ計画について — 🔍 見てほしい"),
            "{turn}"
        );
        // Every byte of both halves survived: nothing was dropped to make a boundary.
        assert!(turn.len() >= body.len() + leftover.trim().len());
    }

    /// The heads-up a surface prints before it spends the turn — same shape as a skill's,
    /// so the two can be read side by side.
    #[test]
    fn provenance_matches_the_skill_layers_shape() {
        let turn = compose("docs", "ctx7", &[message("user", "body")], "");
        let line = provenance("ctx7:docs", "ctx7", "config", &turn);
        assert_eq!(
            line,
            format!(
                "[mcp /ctx7:docs — {} from ctx7 (config)]",
                size_label(turn.len())
            )
        );
        // Kibibytes once a real prompt body is in it, exactly as a skill reports them.
        let big = provenance("ctx7:docs", "ctx7", "config", &"x".repeat(3 * 1024 + 512));
        assert!(big.contains("3.5 KB from"), "{big}");

        // The origin is a path for a project-scope server, shortened from the front so the
        // end — the half that says WHICH file — survives.
        let deep = provenance(
            "ctx7:docs",
            "ctx7",
            &format!("/{}/.mcp.json", "d".repeat(200)),
            &turn,
        );
        assert!(deep.ends_with("/.mcp.json)]"), "{deep}");
        assert!(deep.contains('…'), "{deep}");

        // A server name is outside input too: nothing in it may still be an escape sequence.
        let hostile = provenance("s:p", "\u{1b}[31mevil", "config", &turn);
        assert!(!hostile.chars().any(|c| c.is_control()), "{hostile:?}");
        assert!(!hostile.contains("31m"), "{hostile:?}");
    }

    // ─── invoking ────────────────────────────────────────────────────────────────

    /// The end of the chain: a cached prompt reaches a resolved registry and parses on both
    /// interactive surfaces into an invocation carrying everything the fetch needs.
    #[test]
    fn invoking_one_parses_into_an_invocation_without_touching_the_server() {
        let d = def("ctx7");
        let mut cache = PromptCache::default();
        cache.put(&d, vec![prompt("docs")]);
        let reg = Registry::resolve(specs_from(std::slice::from_ref(&d), &cache, budget()));

        for surface in [Surface::Repl, Surface::Tui] {
            match reg.parse("/ctx7:docs ratatui scrolling", surface) {
                Parsed::Command(SlashCommand::McpPrompt(inv)) => {
                    assert_eq!(inv.name, "ctx7:docs");
                    assert_eq!(inv.prompt.name, "docs");
                    assert_eq!(inv.def.name, "ctx7");
                    assert_eq!(inv.arg, "ratatui scrolling");
                    // Parsing is pure: what the fetch will send is derived from it, here.
                    let mapped = map_arguments(&inv.prompt.arguments, &inv.arg);
                    assert_eq!(mapped.sent["library"], "ratatui scrolling");
                }
                other => panic!("{surface:?}: {other:?}"),
            }
        }

        // A bare invocation still parses — the refusal for its missing argument belongs to
        // `invoke`, which can say which argument it is.
        assert!(matches!(
            reg.parse("/ctx7:docs", Surface::Repl),
            Parsed::Command(SlashCommand::McpPrompt(_))
        ));
    }

    /// A prompt whose required arguments the user did not supply is refused BEFORE anything
    /// is spawned: the message names the argument and the usage, which is strictly more
    /// useful than the server's own "invalid arguments" after a process start.
    #[tokio::test]
    async fn a_missing_required_argument_is_refused_without_starting_the_server() {
        let invocation = Invocation {
            name: "ctx7:docs".into(),
            // A command that could never run: reaching the spawn at all would fail this.
            def: ServerDef {
                command: "agentpit-no-such-binary-9f3a".into(),
                ..def("ctx7")
            },
            prompt: prompt("docs"),
            arg: String::new(),
            budget: budget(),
        };
        let err = invoke(&invocation).await.expect_err("must refuse");
        let message = format!("{err:#}");
        assert!(message.contains("/ctx7:docs needs `library`"), "{message}");
        assert!(message.contains("Usage: /ctx7:docs <library>"), "{message}");
        // The refusal is local: nothing about the (unrunnable) command appears in it.
        assert!(!message.contains("agentpit-no-such-binary"), "{message}");
    }

    /// The property the whole invoke path exists to protect: a server that cannot be reached
    /// produces a refusal, never a turn. There is no body to send, and sending the user's own
    /// words instead would be a different request than the one they asked for.
    #[tokio::test]
    async fn an_unreachable_server_refuses_rather_than_composing_a_turn() {
        let invocation = Invocation {
            name: "ctx7:docs".into(),
            def: ServerDef {
                command: "agentpit-no-such-binary-9f3a".into(),
                ..def("ctx7")
            },
            prompt: prompt("docs"),
            arg: "ratatui".into(),
            budget: budget(),
        };
        let err = invoke(&invocation).await.expect_err("must refuse");
        let message = format!("{err:#}");
        assert!(
            message.contains("failed to start MCP server 'ctx7'"),
            "{message}"
        );
        // Nothing that could be mistaken for a composed turn came back.
        assert!(!message.contains("Follow the MCP prompt"), "{message}");
    }

    /// A colon cannot appear in a built-in, so this layer can never shadow one — but the
    /// registry's collision rule is what enforces it, and it must keep applying here.
    #[test]
    fn an_mcp_prompt_cannot_take_a_builtin_name() {
        let mut d = def("compact");
        d.name = "compact".into();
        let mut cache = PromptCache::default();
        // A server literally named `compact` with a prompt whose name is empty could only
        // ever produce `compact:` — still not `compact`. Prove the guard from the other
        // side: hand the registry a row that claims the built-in name outright.
        cache.put(&d, vec![prompt("x")]);
        let mut specs = specs_from(std::slice::from_ref(&d), &cache, budget());
        specs.push(crate::cli::slash::test_extension(
            "compact",
            &[],
            &[Surface::Repl],
            "body",
        ));
        let reg = Registry::resolve(specs);
        assert!(matches!(
            reg.parse("/compact", Surface::Repl),
            Parsed::Command(SlashCommand::Compact)
        ));
        assert!(matches!(
            reg.parse("/compact:x", Surface::Repl),
            Parsed::Command(SlashCommand::McpPrompt(_))
        ));
        // Nor by case: a server shouting a built-in's name is still that built-in.
        let mut shouty = def("COMPACT");
        shouty.name = "COMPACT".into();
        let mut cache = PromptCache::default();
        cache.put(&shouty, vec![prompt("x")]);
        let reg = Registry::resolve(specs_from(std::slice::from_ref(&shouty), &cache, budget()));
        assert!(matches!(
            reg.parse("/compact", Surface::Repl),
            Parsed::Command(SlashCommand::Compact)
        ));
        assert!(matches!(
            reg.parse("/compact:x", Surface::Repl),
            Parsed::Command(SlashCommand::McpPrompt(_))
        ));
    }
}
