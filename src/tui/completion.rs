//! The slash-command dropdown (design D4) — Claude Code's popup, over agentpit's shared
//! registry.
//!
//! Pure state, the way [`super::input`] and [`super::views`] are: the menu is a filtered
//! view of the candidates [`crate::cli::slash`] declares for [`Surface::Tui`], plus a
//! selection index. It never draws, never dispatches, and holds no terminal — the app
//! loop feeds it keys and renders [`SlashMenu::matches`] above the input line.
//!
//! The popup is a *view of the text*: [`SlashMenu::refresh`] recomputes it from the line
//! after every edit, so there is no second copy of "what is being typed" to drift. A line
//! is a candidate for completion only while it is a bare `/word` — the first space means
//! the name is settled and arguments have started, which is also what closes the popup
//! after an accept.
//!
//! ## Who owns ↑↓ (the reason this module exists)
//!
//! [`InputState`] already binds ↑↓ to history browsing, and the popup wants the same two
//! keys for its selection. The arbitration is therefore written down in exactly one
//! place — [`handle_key`] — instead of being spread across the app loop's key match:
//!
//! * popup OPEN — ↑↓ move the highlight; Enter/Tab accept; Esc dismisses. The history is
//!   not consulted at all. This matters beyond tidiness: a recalled history line that has
//!   since been edited leaves `InputState`'s browse cursor live, so an ↑ that also reached
//!   the input would silently replace the line the user is completing.
//! * popup CLOSED — ↑↓ browse history and Enter submits, exactly as before.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Rect;

use crate::cli::slash::{Registry, Surface, registry};

use super::input::InputState;
use super::views::ListCursor;

/// Rows the popup shows at once; beyond this it scrolls with the selection.
pub const MAX_ROWS: usize = 8;

/// One row of the popup.
#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    /// The name to insert, without the leading slash.
    pub name: &'static str,
    /// How the row reads: `/tree`, `/branch <id>`.
    pub label: String,
    /// The registry's description for the primary form.
    pub description: &'static str,
}

/// Every command this screen serves, in help order — one row per name the surface answers
/// to, aliases included (typing `/ex` must find `/exit`).
///
/// Reads a resolved [`Registry`], so a runtime entry that claims [`Surface::Tui`] is
/// offered here on exactly the same terms as a built-in.
fn all_candidates_in(reg: &'static Registry) -> Vec<Candidate> {
    reg.help_order(Surface::Tui)
        .into_iter()
        .flat_map(|spec| {
            spec.names().map(move |name| Candidate {
                name,
                label: match spec.arg_hint() {
                    "" => format!("/{name}"),
                    args => format!("/{name} {args}"),
                },
                description: spec.description(),
            })
        })
        .collect()
}

/// The command token the line is typing, or `None` when the line is not a bare `/word`
/// (no leading slash, or an argument has already been started).
fn typed_name(text: &str) -> Option<&str> {
    let rest = text.strip_prefix('/')?;
    (!rest.contains(char::is_whitespace)).then_some(rest)
}

/// The dropdown's state: what matches the line, and which row is highlighted.
#[derive(Debug)]
pub struct SlashMenu {
    /// The command set this popup offers — the process registry, unless a test swaps in
    /// one resolved from a fixture.
    registry: &'static Registry,
    matches: Vec<Candidate>,
    cursor: ListCursor,
    /// Set by Esc: stay closed until the `/` token is gone, so a dismissed popup does not
    /// pop straight back up on the next keystroke.
    dismissed: bool,
}

impl Default for SlashMenu {
    fn default() -> SlashMenu {
        SlashMenu::over(registry())
    }
}

impl SlashMenu {
    /// A popup over a specific registry.
    pub fn over(registry: &'static Registry) -> SlashMenu {
        SlashMenu {
            registry,
            matches: Vec::new(),
            cursor: ListCursor::default(),
            dismissed: false,
        }
    }

    /// Open = there is something to show. An empty match set never renders.
    pub fn is_open(&self) -> bool {
        !self.matches.is_empty()
    }

    pub fn matches(&self) -> &[Candidate] {
        &self.matches
    }

    pub fn index(&self) -> usize {
        self.cursor.index
    }

    fn selected(&self) -> Option<&Candidate> {
        self.matches.get(self.cursor.index)
    }

    /// Recompute from the line. Called after every edit that changes the text.
    fn refresh(&mut self, text: &str) {
        let Some(prefix) = typed_name(text) else {
            // The token is gone (backspaced past the `/`, or a space started the
            // arguments): close, and let a future `/` open a fresh popup.
            self.close();
            self.dismissed = false;
            return;
        };
        if self.dismissed {
            self.matches.clear();
            return;
        }
        let lower = prefix.to_ascii_lowercase();
        self.matches = all_candidates_in(self.registry)
            .into_iter()
            .filter(|c| c.name.starts_with(&lower))
            .collect();
        // Filtering re-ranks the list, so the highlight returns to the top row.
        self.cursor.index = 0;
    }

    fn close(&mut self) {
        self.matches.clear();
        self.cursor.index = 0;
    }

    /// Esc: hide the popup, leaving the typed text exactly as it is.
    fn dismiss(&mut self) {
        self.close();
        self.dismissed = true;
    }

    /// The line is leaving the editor (submitted): forget everything, including the Esc
    /// suppression, so the next `/` opens the popup again.
    fn reset(&mut self) {
        self.close();
        self.dismissed = false;
    }
}

/// What the app loop must still do after the editor has seen a key.
#[derive(Debug, PartialEq)]
pub enum Edit {
    /// Fully handled here.
    Consumed,
    /// Not an editing key — the app loop's own bindings own it.
    Passthrough,
    /// Enter with the popup closed: the line is ready to route and send.
    Submit,
}

/// Feed one key to the editor (input line + popup) and say what is left to do.
///
/// This is the whole ↑↓ / Enter ownership rule; see the module docs.
pub fn handle_key(input: &mut InputState, menu: &mut SlashMenu, key: KeyEvent) -> Edit {
    match (key.code, key.modifiers) {
        // ── popup open: it owns the navigation keys ──────────────────────────
        (KeyCode::Up, _) if menu.is_open() => {
            menu.cursor.up();
            Edit::Consumed
        }
        (KeyCode::Down, _) if menu.is_open() => {
            menu.cursor.down(menu.matches.len());
            Edit::Consumed
        }
        (KeyCode::Tab, _) if menu.is_open() => {
            accept(input, menu);
            Edit::Consumed
        }
        (KeyCode::Enter, _) if menu.is_open() => {
            // Enter completes the row — unless there is nothing left to complete because
            // the typed name already IS the highlighted row, in which case it runs the
            // command. `/tree` + Enter must stay one keystroke, not two.
            let typed = typed_name(input.text())
                .unwrap_or_default()
                .to_ascii_lowercase();
            if menu.selected().is_some_and(|c| c.name == typed) {
                menu.reset();
                Edit::Submit
            } else {
                accept(input, menu);
                Edit::Consumed
            }
        }
        (KeyCode::Esc, _) if menu.is_open() => {
            menu.dismiss();
            Edit::Consumed
        }
        // ── popup closed: the input line behaves exactly as it always has ────
        (KeyCode::Enter, _) => {
            menu.reset();
            Edit::Submit
        }
        (KeyCode::Up, _) => {
            input.history_prev();
            Edit::Consumed
        }
        (KeyCode::Down, _) => {
            input.history_next();
            Edit::Consumed
        }
        // ── edits and cursor motion (only edits can change what matches) ─────
        (KeyCode::Backspace, _) => {
            input.backspace();
            menu.refresh(input.text());
            Edit::Consumed
        }
        (KeyCode::Char(c), m) if m.is_empty() || m == KeyModifiers::SHIFT => {
            input.insert(c);
            menu.refresh(input.text());
            Edit::Consumed
        }
        (KeyCode::Left, _) => {
            input.left();
            Edit::Consumed
        }
        (KeyCode::Right, _) => {
            input.right();
            Edit::Consumed
        }
        (KeyCode::Home, _) => {
            input.home();
            Edit::Consumed
        }
        (KeyCode::End, _) => {
            input.end();
            Edit::Consumed
        }
        _ => Edit::Passthrough,
    }
}

/// Where the popup is drawn: directly above the input box, growing upward, capped at
/// [`MAX_ROWS`] and at the room actually above the box. `None` when there is nothing to
/// show or nowhere to show it — the conversation keeps those rows instead.
pub fn popup_area(input_box: Rect, rows: usize) -> Option<Rect> {
    let rows = rows.min(MAX_ROWS) as u16;
    if rows == 0 {
        return None;
    }
    let height = (rows + 2).min(input_box.y); // +2 for the border
    if height < 3 {
        return None; // not even one bordered row fits above the input
    }
    Some(Rect {
        x: input_box.x,
        y: input_box.y - height,
        width: input_box.width,
        height,
    })
}

/// Put the highlighted name in the buffer, ready for its arguments.
///
/// The trailing space is load-bearing twice over: the cursor sits where an argument would
/// be typed, and the whitespace is what makes the line stop being a bare `/word`, which is
/// what closes the popup.
fn accept(input: &mut InputState, menu: &mut SlashMenu) {
    if let Some(candidate) = menu.selected() {
        input.set_line(&format!("/{} ", candidate.name));
    }
    menu.refresh(input.text());
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::slash::{Protocol, Route, route};

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    /// A tiny harness standing in for the app loop: keys in, state out.
    #[derive(Default)]
    struct Editor {
        input: InputState,
        menu: SlashMenu,
    }

    impl Editor {
        /// An editor whose popup reads a specific registry.
        fn over(reg: &'static Registry) -> Editor {
            Editor {
                input: InputState::default(),
                menu: SlashMenu::over(reg),
            }
        }
        fn press(&mut self, code: KeyCode) -> Edit {
            handle_key(&mut self.input, &mut self.menu, key(code))
        }
        fn type_str(&mut self, s: &str) {
            for c in s.chars() {
                self.press(KeyCode::Char(c));
            }
        }
        /// Type a line and submit it, the way the app loop does (route + history).
        fn submit(&mut self, s: &str) {
            self.type_str(s);
            assert_eq!(self.press(KeyCode::Enter), Edit::Submit);
            self.input.submit();
        }
        fn text(&self) -> &str {
            self.input.text()
        }
        fn names(&self) -> Vec<&'static str> {
            self.menu.matches().iter().map(|c| c.name).collect()
        }
    }

    // ─── opening and filtering ───────────────────────────────────────────────

    #[test]
    fn slash_on_an_empty_line_opens_the_whole_menu() {
        let mut e = Editor::default();
        assert!(!e.menu.is_open());
        e.type_str("/");
        assert!(e.menu.is_open());
        // Exactly the names this surface answers to — no more (a row the router refuses)
        // and no fewer (a command with no way to discover it).
        let mut offered = e.names();
        offered.sort_unstable();
        let mut expected = crate::cli::slash::names_for(Surface::Tui);
        expected.sort_unstable();
        assert_eq!(offered, expected);
        // Help order, so the popup reads like /help: the terminal group comes last.
        assert_eq!(e.names().first(), Some(&"help"));
        assert_eq!(e.names().last(), Some(&"detach"));
        assert_eq!(e.menu.index(), 0);
    }

    #[test]
    fn a_slash_that_is_not_the_first_character_opens_nothing() {
        let mut e = Editor::default();
        e.type_str("read src/tui/");
        assert!(!e.menu.is_open(), "a path mid-sentence is not a command");
    }

    #[test]
    fn further_characters_filter_the_list() {
        let mut e = Editor::default();
        e.type_str("/t");
        assert_eq!(e.names(), vec!["tree"]);
        e.type_str("re");
        assert_eq!(e.names(), vec!["tree"]);
        // Case is not a different command.
        let mut upper = Editor::default();
        upper.type_str("/TR");
        assert_eq!(upper.names(), vec!["tree"]);
        // An alias is its own row, so /ex finds /exit.
        let mut alias = Editor::default();
        alias.type_str("/ex");
        assert_eq!(alias.names(), vec!["exit"]);
    }

    #[test]
    fn a_runtime_entry_is_offered_like_any_other_row() {
        // The popup is a view of a resolved registry, so an entry that was not compiled
        // in is discoverable here the same way a built-in is — including its alias.
        let mut e = Editor::over(crate::cli::slash::test_registry_with_skill());
        e.type_str("/sk");
        assert_eq!(e.names(), vec!["skill", "sk"]);
        let row = &e.menu.matches()[0];
        assert_eq!(row.label, "/skill [text]");
        assert_eq!(row.description, "runtime entry skill");
        // …and accepting it leaves the line ready for arguments, as any other row does.
        e.press(KeyCode::Tab);
        assert_eq!(e.text(), "/skill ");

        // The process registry, which nothing has fed yet, offers no such row.
        let mut plain = Editor::default();
        plain.type_str("/sk");
        assert!(!plain.menu.is_open());
    }

    #[test]
    fn a_skill_md_on_disk_is_offered_in_the_dropdown() {
        // The DoD, at this surface: a file in `.claude/skills/` is discoverable here the
        // same way a compiled-in command is, and accepting it leaves a line the router
        // turns into a turn rather than a refusal.
        let reg = crate::cli::skills::test_registry_from_disk();
        let mut e = Editor::over(reg);
        e.type_str("/crit");
        assert_eq!(e.names(), vec!["critique"]);
        let row = &e.menu.matches()[0];
        assert_eq!(row.label, "/critique [text]");
        assert_eq!(row.description, "Argue against the current plan");
        e.press(KeyCode::Tab);
        assert_eq!(e.text(), "/critique ");
        e.type_str("the caching plan");
        assert_eq!(e.press(KeyCode::Enter), Edit::Submit);
        assert!(matches!(
            crate::tui::slash::route_in(reg, e.text()),
            Route::Compose { .. }
        ));

        // The process registry, which no entry point has fed here, offers no such row.
        let mut plain = Editor::default();
        plain.type_str("/crit");
        assert!(!plain.menu.is_open());
    }

    /// The DoD for the MCP layer at this surface: a prompt a `mcp refresh` cached is
    /// offered as `/<server>:<prompt>`, and accepting it leaves a line the router turns
    /// into a composed turn rather than a refusal.
    #[test]
    fn a_refreshed_mcp_prompt_is_offered_in_the_dropdown() {
        let reg = crate::mcp::prompts::test_registry_from_cache();
        let mut e = Editor::over(reg);
        // The server name alone narrows to it — the colon is part of the command name, so
        // typing it does not start an argument.
        e.type_str("/ctx7");
        assert_eq!(e.names(), vec![crate::mcp::prompts::TEST_COMMAND]);
        let row = &e.menu.matches()[0];
        // The hint is the prompt's OWN argument list, so the row says what the server needs
        // before the user presses Enter and finds out from a refusal.
        assert_eq!(row.label, "/ctx7:docs <library>");
        assert_eq!(row.description, "Look up library docs");
        e.press(KeyCode::Tab);
        assert_eq!(e.text(), "/ctx7:docs ");
        e.type_str("ratatui scrolling");
        assert_eq!(e.press(KeyCode::Enter), Edit::Submit);
        match crate::tui::slash::route_in(reg, e.text()) {
            Route::McpPrompt(invocation) => {
                assert_eq!(invocation.name, crate::mcp::prompts::TEST_COMMAND);
                assert_eq!(invocation.arg, "ratatui scrolling");
            }
            other => panic!("{other:?}"),
        }

        // The process registry, which no entry point has fed here, offers no such row.
        let mut plain = Editor::default();
        plain.type_str("/ctx7");
        assert!(!plain.menu.is_open());
    }

    #[test]
    fn a_multibyte_filter_matches_nothing_and_does_not_panic() {
        let mut e = Editor::default();
        e.type_str("/日本語");
        assert_eq!(e.text(), "/日本語");
        assert!(!e.menu.is_open(), "no command starts with 日");
        // Backspacing back through the CJK chars must stay on char boundaries.
        for _ in 0..3 {
            e.press(KeyCode::Backspace);
        }
        assert_eq!(e.text(), "/");
        assert!(
            e.menu.is_open(),
            "the popup returns once the filter matches"
        );
        // A line that is only CJK never opens it at all.
        let mut plain = Editor::default();
        plain.type_str("日本語で説明して");
        assert!(!plain.menu.is_open());
    }

    // ─── the ↑↓ ownership conflict (D4) ──────────────────────────────────────

    #[test]
    fn arrows_move_the_menu_and_never_touch_history_while_it_is_open() {
        let mut e = Editor::default();
        e.submit("/compact");
        e.submit("/tree");
        // Recall the newest entry, then edit it back down to `/`. This is the state the
        // conflict lives in: the popup is open AND InputState's browse cursor is still
        // live, so an ↑ that reached the input would swap the line for /compact.
        assert_eq!(e.press(KeyCode::Up), Edit::Consumed);
        assert_eq!(e.text(), "/tree");
        for _ in 0..4 {
            e.press(KeyCode::Backspace);
        }
        assert_eq!(e.text(), "/");
        assert!(e.menu.is_open());

        e.press(KeyCode::Down);
        assert_eq!(e.menu.index(), 1, "↓ moves the highlight");
        assert_eq!(e.text(), "/", "↓ must not restore the stashed draft");
        e.press(KeyCode::Up);
        assert_eq!(e.menu.index(), 0, "↑ moves the highlight back");
        assert_eq!(e.text(), "/", "↑ must not recall an older entry");
    }

    #[test]
    fn arrows_still_browse_history_when_the_menu_is_closed() {
        // The same expectations as `input::tests::history_browse_preserves_the_draft`,
        // driven through the key handler that now sits in front of InputState.
        let mut e = Editor::default();
        e.submit("one");
        e.submit("two");
        e.press(KeyCode::Up);
        assert_eq!(e.text(), "two");
        e.press(KeyCode::Up);
        assert_eq!(e.text(), "one");
        e.press(KeyCode::Down);
        assert_eq!(e.text(), "two");
        e.press(KeyCode::Down);
        assert_eq!(e.text(), "");
        // A non-empty draft still blocks history (prime's rule), popup or not.
        e.type_str("draft");
        e.press(KeyCode::Up);
        assert_eq!(e.text(), "draft");
    }

    // ─── accept / dismiss ────────────────────────────────────────────────────

    #[test]
    fn enter_and_tab_accept_the_highlighted_name_ready_for_arguments() {
        for accept_key in [KeyCode::Enter, KeyCode::Tab] {
            let mut e = Editor::default();
            e.type_str("/br");
            assert_eq!(e.names(), vec!["branch"]);
            assert_eq!(
                e.press(accept_key),
                Edit::Consumed,
                "accepting must not submit the line"
            );
            assert_eq!(e.text(), "/branch ");
            assert_eq!(e.input.cursor(), "/branch ".chars().count());
            assert!(!e.menu.is_open(), "the trailing space closes the popup");
            // …and the line is ready to carry an argument straight into the router.
            e.type_str("a1b2c3d4");
            assert_eq!(e.press(KeyCode::Enter), Edit::Submit);
            assert!(matches!(route(e.text()), Route::Protocol(_)));
        }
    }

    #[test]
    fn enter_on_a_settled_name_runs_it_instead_of_completing_again() {
        let mut e = Editor::default();
        e.type_str("/tree");
        assert!(e.menu.is_open(), "the exact name still matches its own row");
        assert_eq!(e.press(KeyCode::Enter), Edit::Submit);
        assert_eq!(
            e.text(),
            "/tree",
            "no second Enter, no stray trailing space"
        );
        assert!(!e.menu.is_open());
        assert_eq!(route(e.text()), Route::Protocol(Protocol::Tree));
    }

    #[test]
    fn accepting_a_selected_row_inserts_that_row() {
        let mut e = Editor::default();
        e.type_str("/");
        e.press(KeyCode::Down);
        let picked = e.menu.matches()[1].name;
        e.press(KeyCode::Tab);
        assert_eq!(e.text(), format!("/{picked} "));
    }

    #[test]
    fn esc_dismisses_without_altering_the_typed_text() {
        let mut e = Editor::default();
        e.type_str("/tr");
        assert_eq!(e.press(KeyCode::Esc), Edit::Consumed);
        assert_eq!(e.text(), "/tr", "Esc changes nothing but the popup");
        assert!(!e.menu.is_open());
        // It stays dismissed while the same token is being typed…
        e.type_str("ee");
        assert!(!e.menu.is_open());
        assert_eq!(e.press(KeyCode::Enter), Edit::Submit);
        assert_eq!(e.text(), "/tree");
        e.input.submit();
        // …and the next `/` opens it again.
        e.type_str("/");
        assert!(e.menu.is_open());
    }

    #[test]
    fn backspace_past_the_slash_closes_it() {
        let mut e = Editor::default();
        e.type_str("/tr");
        assert!(e.menu.is_open());
        e.press(KeyCode::Backspace);
        e.press(KeyCode::Backspace);
        assert_eq!(e.text(), "/");
        assert!(e.menu.is_open(), "still a command line");
        e.press(KeyCode::Backspace);
        assert_eq!(e.text(), "");
        assert!(!e.menu.is_open());
        // Backspacing on an empty line is a no-op, not a panic.
        e.press(KeyCode::Backspace);
        assert_eq!(e.text(), "");
    }

    #[test]
    fn typing_a_space_settles_the_name_and_closes_the_popup() {
        let mut e = Editor::default();
        e.type_str("/login ");
        assert!(!e.menu.is_open());
        e.type_str("codex");
        assert!(!e.menu.is_open(), "arguments are not filtered");
        assert_eq!(e.text(), "/login codex");
    }

    // ─── keys the app loop keeps ─────────────────────────────────────────────

    #[test]
    fn unclaimed_keys_pass_through() {
        let mut e = Editor::default();
        assert_eq!(e.press(KeyCode::Esc), Edit::Passthrough);
        assert_eq!(e.press(KeyCode::Tab), Edit::Passthrough);
        assert_eq!(
            handle_key(
                &mut e.input,
                &mut e.menu,
                KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
            ),
            Edit::Passthrough,
            "Ctrl-C is the app loop's, not a character to insert"
        );
        assert_eq!(e.text(), "");
    }

    // ─── geometry ────────────────────────────────────────────────────────────

    #[test]
    fn the_popup_sits_directly_above_the_input_and_leaves_the_rest_alone() {
        // A 24-row screen: transcript 0..19, live 19, input box 20..22, status 23.
        let input_box = Rect::new(0, 20, 80, 3);
        let area = popup_area(input_box, 3).expect("room for three rows");
        assert_eq!(
            area.y + area.height,
            input_box.y,
            "flush with the input box"
        );
        assert_eq!(area.height, 5, "three rows plus the border");
        assert_eq!((area.x, area.width), (input_box.x, input_box.width));
        assert!(area.y > 0, "the conversation keeps the rows above");

        // Long lists scroll inside a capped box rather than eating the screen…
        assert_eq!(
            popup_area(input_box, 40).unwrap().height,
            MAX_ROWS as u16 + 2
        );
        // …and a short screen (or an empty list) gets no popup at all.
        assert_eq!(
            popup_area(Rect::new(0, 2, 80, 3), 5).map(|a| a.height),
            None
        );
        assert_eq!(popup_area(input_box, 0), None);
    }

    // ─── registry agreement ──────────────────────────────────────────────────

    #[test]
    fn every_offered_row_is_a_command_this_screen_runs() {
        // A row the user can accept must never land on "Unknown command": the popup and
        // the router read the same registry, and this is the assertion that keeps them
        // reading it the same way.
        for candidate in all_candidates_in(registry()) {
            let name = candidate.name;
            assert!(candidate.label.starts_with(&format!("/{name}")));
            assert!(
                !candidate.description.is_empty(),
                "/{name} offers no description"
            );
            let arg = crate::cli::slash::probe_arg(name);
            match route(&format!("/{name} {arg}")) {
                Route::Local(_) | Route::Protocol(_) | Route::Suspend(_) => {}
                other => panic!("/{name} is offered by the popup but routed to {other:?}"),
            }
        }
    }
}
