//! The TUI completion popups: `/` commands from agentpit's shared registry and `@`
//! project-file selection rooted at the attached session's working directory.
//!
//! Pure state, the way [`super::input`] and [`super::views`] are: each menu is a filtered
//! view plus a selection index. Neither draws, dispatches, nor holds a terminal — the app
//! loop feeds them keys and renders the active menu above the input line.
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

use std::fs;
use std::path::Path;
use std::process::Command;

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

/// The `@token` immediately before the input cursor. `prefix` drives filtering while
/// `start..end` identifies the whole token to replace, including any suffix after a cursor
/// that was moved into the middle of the line.
#[derive(Debug, Clone, PartialEq, Eq)]
struct AtToken {
    start: usize,
    end: usize,
    prefix: String,
}

fn at_token(text: &str, cursor: usize) -> Option<AtToken> {
    let chars: Vec<char> = text.chars().collect();
    let cursor = cursor.min(chars.len());
    let start = chars[..cursor]
        .iter()
        .rposition(|c| c.is_whitespace())
        .map_or(0, |i| i + 1);
    if start >= cursor || chars.get(start) != Some(&'@') {
        return None;
    }
    let end = chars[cursor..]
        .iter()
        .position(|c| c.is_whitespace())
        .map_or(chars.len(), |i| cursor + i);
    Some(AtToken {
        start,
        end,
        prefix: chars[start + 1..cursor].iter().collect(),
    })
}

/// Project-file completion state. Paths are stored relative to the session cwd and use
/// `/` separators even on platforms whose native separator differs.
#[derive(Debug, Default)]
pub struct FileMenu {
    files: Vec<String>,
    matches: Vec<String>,
    cursor: ListCursor,
    /// Esc suppresses the token at this character position until it disappears or the
    /// cursor enters another token.
    dismissed_at: Option<usize>,
}

impl FileMenu {
    pub fn from_cwd(cwd: &Path) -> FileMenu {
        FileMenu::with_files(project_files(cwd))
    }

    pub fn with_files(mut files: Vec<String>) -> FileMenu {
        files.sort_by_key(|path| path.to_ascii_lowercase());
        files.dedup();
        FileMenu {
            files,
            ..FileMenu::default()
        }
    }

    pub fn is_open(&self) -> bool {
        !self.matches.is_empty()
    }

    pub fn matches(&self) -> &[String] {
        &self.matches
    }

    pub fn index(&self) -> usize {
        self.cursor.index
    }

    fn selected(&self) -> Option<&str> {
        self.matches.get(self.cursor.index).map(String::as_str)
    }

    fn refresh(&mut self, text: &str, cursor: usize) {
        let Some(token) = at_token(text, cursor) else {
            self.close();
            self.dismissed_at = None;
            return;
        };
        if self.dismissed_at.is_some_and(|start| start == token.start) {
            self.close();
            return;
        }
        self.dismissed_at = None;
        let prefix = token.prefix.to_ascii_lowercase();
        let mut ranked: Vec<(u8, String)> = self
            .files
            .iter()
            .filter_map(|path| {
                let lower = path.to_ascii_lowercase();
                let rank = if prefix.is_empty() || lower.starts_with(&prefix) {
                    0
                } else if lower.split('/').any(|part| part.starts_with(&prefix)) {
                    1
                } else if lower.contains(&prefix) {
                    2
                } else {
                    return None;
                };
                Some((rank, path.clone()))
            })
            .collect();
        ranked.sort_by(|(rank_a, path_a), (rank_b, path_b)| {
            rank_a.cmp(rank_b).then_with(|| {
                path_a
                    .to_ascii_lowercase()
                    .cmp(&path_b.to_ascii_lowercase())
            })
        });
        self.matches = ranked.into_iter().map(|(_, path)| path).collect();
        self.cursor.index = 0;
    }

    fn close(&mut self) {
        self.matches.clear();
        self.cursor.index = 0;
    }

    fn dismiss(&mut self, text: &str, cursor: usize) {
        self.dismissed_at = at_token(text, cursor).map(|token| token.start);
        self.close();
    }

    fn reset(&mut self) {
        self.close();
        self.dismissed_at = None;
    }
}

/// Enumerate regular project files. Git repositories use tracked + non-ignored untracked
/// files; elsewhere a recursive fallback prunes VCS metadata and common generated trees.
/// An unreadable entry is skipped rather than making the TUI fail to start.
pub fn project_files(cwd: &Path) -> Vec<String> {
    // Git gives the best project view: tracked files plus non-ignored untracked files. Fall
    // back to a dependency-free walk for non-Git directories or systems without Git.
    if let Ok(output) = Command::new("git")
        .args([
            "-C",
            cwd.to_string_lossy().as_ref(),
            "ls-files",
            "--cached",
            "--others",
            "--exclude-standard",
            "-z",
        ])
        .output()
        && output.status.success()
    {
        let mut files: Vec<String> = output
            .stdout
            .split(|byte| *byte == 0)
            .filter(|raw| !raw.is_empty())
            .map(|raw| String::from_utf8_lossy(raw).replace('\\', "/"))
            .filter(|relative| relative.chars().all(|c| !c.is_control()))
            .filter(|relative| cwd.join(relative).is_file())
            .collect();
        files.sort_by_key(|path| path.to_ascii_lowercase());
        files.dedup();
        return files;
    }

    const PRUNED_DIRS: &[&str] = &[
        ".git",
        "target",
        "node_modules",
        ".next",
        "dist",
        "build",
        "coverage",
        ".idea",
        ".vscode",
    ];

    fn visit(root: &Path, dir: &Path, out: &mut Vec<String>) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_dir() {
                if !PRUNED_DIRS.contains(&entry.file_name().to_string_lossy().as_ref()) {
                    visit(root, &path, out);
                }
            } else if kind.is_file()
                && let Ok(relative) = path.strip_prefix(root)
            {
                let relative = relative.to_string_lossy().replace('\\', "/");
                if relative.chars().all(|c| !c.is_control()) {
                    out.push(relative);
                }
            }
        }
    }

    let mut files = Vec::new();
    visit(cwd, cwd, &mut files);
    files.sort_by_key(|path| path.to_ascii_lowercase());
    files
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
pub fn handle_key_with_files(
    input: &mut InputState,
    menu: &mut SlashMenu,
    files: &mut FileMenu,
    key: KeyEvent,
) -> Edit {
    match (key.code, key.modifiers) {
        // The file popup has priority when open. In practice the two syntaxes are
        // disjoint (`/` at line start versus an `@` token), but making the ownership
        // explicit prevents a future slash argument completion from stealing these keys.
        (KeyCode::Up, _) if files.is_open() => {
            files.cursor.up();
            Edit::Consumed
        }
        (KeyCode::Down, _) if files.is_open() => {
            files.cursor.down(files.matches.len());
            Edit::Consumed
        }
        (KeyCode::Tab | KeyCode::Enter, _) if files.is_open() => {
            accept_file(input, files);
            menu.refresh(input.text());
            Edit::Consumed
        }
        (KeyCode::Esc, _) if files.is_open() => {
            files.dismiss(input.text(), input.cursor());
            Edit::Consumed
        }
        // ── slash popup open: it owns the navigation keys ────────────────────
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
            files.refresh(input.text(), input.cursor());
            Edit::Consumed
        }
        (KeyCode::Enter, _) if menu.is_open() => {
            let typed = typed_name(input.text())
                .unwrap_or_default()
                .to_ascii_lowercase();
            if menu.selected().is_some_and(|c| c.name == typed) {
                menu.reset();
                files.reset();
                Edit::Submit
            } else {
                accept(input, menu);
                files.refresh(input.text(), input.cursor());
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
            files.reset();
            Edit::Submit
        }
        (KeyCode::Up, _) => {
            input.history_prev();
            refresh(input, menu, files);
            Edit::Consumed
        }
        (KeyCode::Down, _) => {
            input.history_next();
            refresh(input, menu, files);
            Edit::Consumed
        }
        // ── edits and cursor motion can both change the active @ prefix ──────
        (KeyCode::Backspace, _) => {
            input.backspace();
            refresh(input, menu, files);
            Edit::Consumed
        }
        (KeyCode::Char(c), m) if m.is_empty() || m == KeyModifiers::SHIFT => {
            input.insert(c);
            refresh(input, menu, files);
            Edit::Consumed
        }
        (KeyCode::Left, _) => {
            input.left();
            refresh(input, menu, files);
            Edit::Consumed
        }
        (KeyCode::Right, _) => {
            input.right();
            refresh(input, menu, files);
            Edit::Consumed
        }
        (KeyCode::Home, _) => {
            input.home();
            refresh(input, menu, files);
            Edit::Consumed
        }
        (KeyCode::End, _) => {
            input.end();
            refresh(input, menu, files);
            Edit::Consumed
        }
        _ => Edit::Passthrough,
    }
}

fn refresh(input: &InputState, menu: &mut SlashMenu, files: &mut FileMenu) {
    menu.refresh(input.text());
    files.refresh(input.text(), input.cursor());
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

fn accept_file(input: &mut InputState, menu: &mut FileMenu) {
    let Some(path) = menu.selected().map(str::to_owned) else {
        return;
    };
    if let Some(token) = at_token(input.text(), input.cursor()) {
        // Consume one existing delimiter so accepting an inline token never creates a
        // double space, and leave the cursor after the single inserted delimiter.
        let replace_end = if input
            .text()
            .chars()
            .nth(token.end)
            .is_some_and(char::is_whitespace)
        {
            token.end + 1
        } else {
            token.end
        };
        input.replace_range(token.start, replace_end, &format!("@{path} "));
    }
    menu.refresh(input.text(), input.cursor());
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
        files: FileMenu,
    }

    impl Editor {
        /// An editor whose popup reads a specific registry.
        fn over(reg: &'static Registry) -> Editor {
            Editor {
                input: InputState::default(),
                menu: SlashMenu::over(reg),
                files: FileMenu::default(),
            }
        }
        fn with_files(files: &[&str]) -> Editor {
            Editor {
                files: FileMenu::with_files(files.iter().map(|s| (*s).to_string()).collect()),
                ..Editor::default()
            }
        }
        fn press(&mut self, code: KeyCode) -> Edit {
            handle_key_with_files(&mut self.input, &mut self.menu, &mut self.files, key(code))
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
    fn doctor_and_config_are_visible_and_filterable_in_the_tui() {
        let mut doctor = Editor::default();
        doctor.type_str("/doc");
        assert_eq!(doctor.names(), vec!["doctor"]);

        let mut config = Editor::default();
        config.type_str("/conf");
        assert_eq!(config.names(), vec!["config"]);
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
            handle_key_with_files(
                &mut e.input,
                &mut e.menu,
                &mut e.files,
                KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
            ),
            Edit::Passthrough,
            "Ctrl-C is the app loop's, not a character to insert"
        );
        assert_eq!(e.text(), "");
    }

    // ─── project-file completion ─────────────────────────────────────────────

    #[test]
    fn at_token_filters_relative_paths_and_accept_replaces_only_that_token() {
        let mut e = Editor::with_files(&["README.md", "src/main.rs", "src/tui/mod.rs"]);
        e.type_str("please inspect @src/m then");
        // Move the cursor back to immediately after `@src/m`; filtering is based on the
        // cursor prefix, while acceptance replaces the suffix too.
        for _ in 0.." then".chars().count() {
            e.press(KeyCode::Left);
        }
        assert_eq!(e.files.matches(), &["src/main.rs"]);
        assert_eq!(e.press(KeyCode::Enter), Edit::Consumed);
        assert_eq!(e.text(), "please inspect @src/main.rs then");
        assert_eq!(
            e.input.cursor(),
            "please inspect @src/main.rs ".chars().count()
        );
    }

    #[test]
    fn file_filter_finds_a_path_by_component_or_substring() {
        let mut e = Editor::with_files(&["docs/guide.md", "src/main.rs", "src/tui/mod.rs"]);
        e.type_str("look at @main");
        assert_eq!(e.files.matches(), &["src/main.rs"]);

        let mut substring = Editor::with_files(&["docs/architecture.md", "src/lib.rs"]);
        substring.type_str("look at @tect");
        assert_eq!(substring.files.matches(), &["docs/architecture.md"]);
    }

    #[test]
    fn file_menu_owns_arrows_tab_enter_and_esc_without_opening_slash_menu() {
        let mut e = Editor::with_files(&["alpha.rs", "assets/logo.png"]);
        e.type_str("see @a");
        assert!(e.files.is_open());
        assert!(!e.menu.is_open());
        e.press(KeyCode::Down);
        assert_eq!(e.files.index(), 1);
        e.press(KeyCode::Tab);
        assert_eq!(e.text(), "see @assets/logo.png ");

        e.type_str("and @a");
        assert!(e.files.is_open());
        assert_eq!(e.press(KeyCode::Esc), Edit::Consumed);
        assert!(!e.files.is_open());
        assert_eq!(e.text(), "see @assets/logo.png and @a");
        e.type_str("lpha");
        assert!(!e.files.is_open(), "dismissal lasts for the current token");
    }

    #[test]
    fn no_matching_file_and_bang_backend_leave_normal_input_keys_alone() {
        let mut e = Editor::with_files(&["src/main.rs"]);
        e.type_str("@definitely-not-a-file do work");
        assert!(!e.files.is_open());
        assert_eq!(e.press(KeyCode::Enter), Edit::Submit);

        let mut backend = Editor::with_files(&["claude"]);
        backend.type_str("!claude review this");
        assert!(
            !backend.files.is_open(),
            "! is reserved for backend routing"
        );
    }

    #[test]
    fn at_is_exclusively_a_file_picker_even_for_a_backend_named_file() {
        let mut e = Editor::with_files(&["claude"]);
        e.type_str("@claude");
        assert_eq!(e.files.matches(), &["claude"]);
        assert_eq!(e.press(KeyCode::Tab), Edit::Consumed);
        assert_eq!(e.text(), "@claude ");
    }

    #[test]
    fn project_enumeration_prunes_metadata_and_generated_trees() {
        let unique = format!(
            "agentpit-file-menu-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let dir = std::env::temp_dir().join(unique);
        fs::create_dir_all(dir.join("src")).unwrap();
        fs::create_dir_all(dir.join(".git/objects")).unwrap();
        fs::create_dir_all(dir.join("target/debug")).unwrap();
        fs::write(dir.join("README.md"), "readme").unwrap();
        fs::write(dir.join("src/lib.rs"), "lib").unwrap();
        fs::write(dir.join(".git/config"), "git").unwrap();
        fs::write(dir.join("target/debug/app"), "binary").unwrap();

        assert_eq!(project_files(&dir), vec!["README.md", "src/lib.rs"]);
        fs::remove_dir_all(dir).unwrap();
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
