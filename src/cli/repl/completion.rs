//! Tab completion and inline hints for the REPL, driven by the same slash-command
//! registry the parser and `/help` read (`crate::cli::slash`).
//!
//! rustyline cannot render an arrow-selectable dropdown the way the TUI's popup does —
//! that asymmetry is deliberate, not a gap to fill here. This module gives the REPL what
//! rustyline *can* do instead: Tab lists candidates and completes the longest unambiguous
//! prefix, and an inline hint (dim text after the cursor) shows the remainder of an
//! unambiguous command name.
//!
//! The candidate computation ([`command_candidates`], [`backend_candidates`]) is plain,
//! directly-testable functions over the registry — [`ReplHelper`]'s trait impls are thin
//! wrappers that locate the word under the cursor and hand it to them.

use rustyline::completion::{Completer, Pair};
use rustyline::highlight::Highlighter;
use rustyline::hint::Hinter;
use rustyline::validate::Validator;
use rustyline::{Context, Helper};

use crate::cli::slash::{self, Registry, Surface};
use crate::types::BackendId;

/// Slash-command names on [`Surface::Repl`] whose name or alias starts with `prefix`
/// (case-insensitive), in the registry's own order — the same order `/help` and the
/// "did you mean" suggester use. A TUI-only command (e.g. `/detach`) never appears: it is
/// simply absent from `names_for(Surface::Repl)`.
pub fn command_candidates(prefix: &str) -> Vec<&'static str> {
    command_candidates_in(slash::registry(), prefix)
}

/// [`command_candidates`] against a specific resolved registry — the built-ins plus
/// whatever runtime entries it carries. Tab therefore offers a discovered command exactly
/// as it offers a built-in one.
pub fn command_candidates_in(reg: &'static Registry, prefix: &str) -> Vec<&'static str> {
    let lower = prefix.to_ascii_lowercase();
    reg.names_for(Surface::Repl)
        .into_iter()
        .filter(|name| name.starts_with(&lower))
        .collect()
}

/// Backend ids whose name starts with `prefix` (case-insensitive) — the completion source
/// for `/backend <id>`.
pub fn backend_candidates(prefix: &str) -> Vec<&'static str> {
    let lower = prefix.to_ascii_lowercase();
    BackendId::ALL
        .iter()
        .map(BackendId::as_str)
        .filter(|id| id.starts_with(&lower))
        .collect()
}

/// The start (byte offset) and text of the whitespace-delimited word the cursor sits in.
/// `start == 0` means the cursor is still inside the first word — the command name,
/// leading slash included.
fn current_word(line: &str, pos: usize) -> (usize, &str) {
    let before = &line[..pos];
    let start = before
        .rfind(char::is_whitespace)
        .map(|i| i + 1)
        .unwrap_or(0);
    (start, &before[start..])
}

/// The REPL's rustyline `Helper`: completion and hints over the shared slash registry.
/// Highlighter and Validator use their default (no-op) behavior — the REPL has no syntax
/// to highlight and no multi-line input to validate.
#[derive(Debug, Clone, Copy)]
pub struct ReplHelper {
    /// The command set Tab and the inline hint offer — the process registry, unless a
    /// test swaps in one resolved from a fixture.
    registry: &'static Registry,
}

impl Default for ReplHelper {
    fn default() -> ReplHelper {
        ReplHelper::over(slash::registry())
    }
}

impl ReplHelper {
    /// A helper over a specific registry.
    pub fn over(registry: &'static Registry) -> ReplHelper {
        ReplHelper { registry }
    }
}

impl Completer for ReplHelper {
    type Candidate = Pair;

    fn complete(
        &self,
        line: &str,
        pos: usize,
        _ctx: &Context<'_>,
    ) -> rustyline::Result<(usize, Vec<Pair>)> {
        if !line.starts_with('/') {
            return Ok((0, Vec::new()));
        }
        let (start, word) = current_word(line, pos);
        if start == 0 {
            // Still typing the command name itself.
            let prefix = word.strip_prefix('/').unwrap_or(word);
            let candidates = command_candidates_in(self.registry, prefix)
                .into_iter()
                .map(|name| Pair {
                    display: format!("/{name}"),
                    replacement: format!("/{name}"),
                })
                .collect();
            return Ok((0, candidates));
        }
        // Past the command name: only `/backend <id>` has a closed, unambiguous
        // vocabulary to complete against — everything else takes free text or forwards
        // words to clap (D3 in slash.rs), where guessing a completion would be wrong.
        let command = line
            .split_whitespace()
            .next()
            .unwrap_or("")
            .trim_start_matches('/');
        if command.eq_ignore_ascii_case("backend") {
            let candidates = backend_candidates(word)
                .into_iter()
                .map(|id| Pair {
                    display: id.to_string(),
                    replacement: id.to_string(),
                })
                .collect();
            return Ok((start, candidates));
        }
        Ok((start, Vec::new()))
    }
}

impl Hinter for ReplHelper {
    type Hint = String;

    fn hint(&self, line: &str, pos: usize, _ctx: &Context<'_>) -> Option<String> {
        // Only hint at end-of-line, while the command name itself is still being typed
        // (no space yet), and only when exactly one command matches — an ambiguous
        // prefix gets Tab's candidate list instead of a guessed hint.
        if pos != line.len() || line.contains(char::is_whitespace) {
            return None;
        }
        let prefix = line.strip_prefix('/')?;
        if prefix.is_empty() {
            return None;
        }
        let mut matches = command_candidates_in(self.registry, prefix).into_iter();
        let only = matches.next()?;
        if matches.next().is_some() {
            return None;
        }
        only.strip_prefix(prefix.to_ascii_lowercase().as_str())
            .filter(|rest| !rest.is_empty())
            .map(str::to_string)
    }
}

impl Highlighter for ReplHelper {}
impl Validator for ReplHelper {}
impl Helper for ReplHelper {}

#[cfg(test)]
mod tests {
    use super::*;

    // ─── command_candidates ───────────────────────────────────────────────────────

    #[test]
    fn a_bare_prefix_yields_every_repl_available_command() {
        assert_eq!(command_candidates(""), slash::names_for(Surface::Repl));
        // Sanity: this is more than a couple of entries, not an accidental empty table.
        assert!(command_candidates("").len() > 10);
    }

    #[test]
    fn a_prefix_narrows_to_matching_names() {
        assert_eq!(command_candidates("back"), vec!["backend"]);
        assert_eq!(command_candidates("q"), vec!["quit"]);
    }

    #[test]
    fn prefix_matching_is_case_insensitive() {
        assert_eq!(command_candidates("BACK"), vec!["backend"]);
        assert_eq!(command_candidates("Back"), vec!["backend"]);
    }

    #[test]
    fn an_unknown_prefix_yields_none() {
        assert!(command_candidates("zzz").is_empty());
        assert!(command_candidates("frobnicate").is_empty());
    }

    #[test]
    fn a_runtime_entry_completes_exactly_as_a_builtin_does() {
        // Tab reads the resolved registry, so a command that was not compiled in is
        // offered on the same terms as one that was — name and alias both.
        let reg = slash::test_registry_with_skill();
        assert_eq!(command_candidates_in(reg, "sk"), vec!["skill", "sk"]);
        assert!(command_candidates_in(reg, "").contains(&"skill"));
        // …while the process registry, which nothing has fed yet, still has no such row.
        assert!(command_candidates("sk").is_empty());

        // The same through the helper rustyline actually calls.
        let (start, names) = complete_at(&ReplHelper::over(reg), "/sk", 3);
        assert_eq!(start, 0);
        assert_eq!(names, vec!["/skill", "/sk"]);
        // And the inline hint finishes it once the prefix is unambiguous.
        assert_eq!(
            hint_at(&ReplHelper::over(reg), "/skil", 5),
            Some("l".to_string())
        );
    }

    /// The DoD for the MCP layer at this surface: a prompt a `mcp refresh` cached completes
    /// like any other command, colon included, and invoking it yields the invocation the
    /// REPL fetches and dispatches.
    #[test]
    fn a_refreshed_mcp_prompt_completes_and_composes() {
        let reg = crate::mcp::prompts::test_registry_from_cache();
        assert_eq!(
            command_candidates_in(reg, "ctx7"),
            vec![crate::mcp::prompts::TEST_COMMAND]
        );
        // The colon is part of the name, not the start of an argument.
        let (start, names) = complete_at(&ReplHelper::over(reg), "/ctx7:", 6);
        assert_eq!(start, 0);
        assert_eq!(names, vec!["/ctx7:docs"]);
        // …while the process registry, which nothing has fed here, has no such row.
        assert!(command_candidates("ctx7").is_empty());

        match reg.parse("/ctx7:docs ratatui scrolling", Surface::Repl) {
            slash::Parsed::Command(slash::SlashCommand::McpPrompt(invocation)) => {
                assert_eq!(invocation.name, "ctx7:docs");
                assert_eq!(invocation.arg, "ratatui scrolling");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn tui_only_commands_never_appear() {
        // `/detach` is Surface::Tui only (slash.rs); it must never surface in REPL
        // completion, exactly matching, as a prefix, or as a substring prefix.
        assert!(command_candidates("detach").is_empty());
        assert!(!command_candidates("d").contains(&"detach"));
    }

    // ─── backend_candidates ──────────────────────────────────────────────────────

    #[test]
    fn backend_completes_every_id_with_an_empty_prefix() {
        let expected: Vec<&str> = BackendId::ALL.iter().map(BackendId::as_str).collect();
        assert_eq!(backend_candidates(""), expected);
    }

    #[test]
    fn backend_prefix_narrows_to_matching_ids() {
        assert_eq!(backend_candidates("clau"), vec!["claude"]);
    }

    #[test]
    fn backend_unknown_prefix_yields_none() {
        assert!(backend_candidates("nonexistent-backend").is_empty());
    }

    // ─── current_word ────────────────────────────────────────────────────────────

    #[test]
    fn current_word_finds_the_command_token_before_any_space() {
        assert_eq!(current_word("/back", 5), (0, "/back"));
        assert_eq!(current_word("/", 1), (0, "/"));
    }

    #[test]
    fn current_word_finds_the_argument_token_after_a_space() {
        assert_eq!(current_word("/backend cla", 12), (9, "cla"));
        assert_eq!(current_word("/backend ", 9), (9, ""));
    }

    #[test]
    fn current_word_respects_cursor_position_not_just_line_end() {
        // Cursor in the middle of the command name, with more text already typed after.
        assert_eq!(current_word("/backend claude", 5), (0, "/back"));
    }

    // ─── ReplHelper::complete (Completer) ─────────────────────────────────────────

    fn complete_at(helper: &ReplHelper, line: &str, pos: usize) -> (usize, Vec<String>) {
        let history = rustyline::history::DefaultHistory::new();
        let ctx = Context::new(&history);
        let (start, pairs) = helper.complete(line, pos, &ctx).unwrap();
        (start, pairs.into_iter().map(|p| p.replacement).collect())
    }

    #[test]
    fn complete_on_a_bare_slash_lists_every_repl_command() {
        let (start, names) = complete_at(&ReplHelper::default(), "/", 1);
        assert_eq!(start, 0);
        let expected: Vec<String> = slash::names_for(Surface::Repl)
            .into_iter()
            .map(|n| format!("/{n}"))
            .collect();
        assert_eq!(names, expected);
    }

    #[test]
    fn complete_narrows_on_a_partial_command_name() {
        let (start, names) = complete_at(&ReplHelper::default(), "/back", 5);
        assert_eq!(start, 0);
        assert_eq!(names, vec!["/backend"]);
    }

    #[test]
    fn complete_on_non_slash_input_yields_nothing() {
        let (_, names) = complete_at(&ReplHelper::default(), "hello world", 5);
        assert!(names.is_empty());
    }

    #[test]
    fn complete_after_backend_space_lists_backend_ids() {
        let (start, ids) = complete_at(&ReplHelper::default(), "/backend ", 9);
        assert_eq!(start, 9);
        let expected: Vec<String> = BackendId::ALL
            .iter()
            .map(|b| b.as_str().to_string())
            .collect();
        assert_eq!(ids, expected);
    }

    #[test]
    fn complete_after_backend_prefix_narrows_ids() {
        let (start, ids) = complete_at(&ReplHelper::default(), "/backend cla", 12);
        assert_eq!(start, 9);
        assert_eq!(ids, vec!["claude"]);
    }

    #[test]
    fn complete_does_not_guess_arguments_for_free_text_commands() {
        // /explain takes free text; there is no closed vocabulary to complete against.
        let (_, words) = complete_at(&ReplHelper::default(), "/explain some", 13);
        assert!(words.is_empty());
    }

    // ─── ReplHelper::hint (Hinter) ─────────────────────────────────────────────────

    fn hint_at(helper: &ReplHelper, line: &str, pos: usize) -> Option<String> {
        let history = rustyline::history::DefaultHistory::new();
        let ctx = Context::new(&history);
        helper.hint(line, pos, &ctx)
    }

    #[test]
    fn hint_shows_the_remainder_of_an_unambiguous_command_name() {
        assert_eq!(
            hint_at(&ReplHelper::default(), "/back", 5),
            Some("end".to_string())
        );
    }

    #[test]
    fn hint_is_none_for_an_ambiguous_prefix() {
        // Both /clear and /clone start with "cl".
        assert_eq!(hint_at(&ReplHelper::default(), "/cl", 3), None);
    }

    #[test]
    fn hint_is_none_once_a_full_command_name_is_typed() {
        assert_eq!(hint_at(&ReplHelper::default(), "/backend", 8), None);
    }

    #[test]
    fn hint_is_none_away_from_end_of_line_or_after_a_space() {
        assert_eq!(hint_at(&ReplHelper::default(), "/back", 2), None);
        assert_eq!(hint_at(&ReplHelper::default(), "/backend cla", 12), None);
    }

    #[test]
    fn hint_is_none_for_free_text() {
        assert_eq!(hint_at(&ReplHelper::default(), "hello", 5), None);
    }
}
