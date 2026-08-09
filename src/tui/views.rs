//! Fullscreen overlays for the TUI (design §11.3): the Agents View roster, the Tree
//! View, and the `?` help — pure list-state + parsing here (unit-tested), drawn by the
//! app loop. Prime's two-surface split: the conversation stays inline (scrollback
//! preserved); only these overlays take the alternate screen.

/// One row of the Agents View.
#[derive(Debug, Clone, PartialEq)]
pub struct RosterRow {
    pub session_id: String,
    pub state: String,
    pub title: Option<String>,
    pub cwd: String,
}

impl RosterRow {
    /// `◇ running` / `● idle` / `✓ inactive` — prime's glyph language (§7.2 B1).
    pub fn glyph(&self) -> &'static str {
        match self.state.as_str() {
            "running" => "◇",
            "idle" => "●",
            _ => "✓",
        }
    }
}

/// Cursor movement over a fixed-length list (shared by every overlay).
#[derive(Debug, Default)]
pub struct ListCursor {
    pub index: usize,
}

impl ListCursor {
    pub fn up(&mut self) {
        self.index = self.index.saturating_sub(1);
    }
    pub fn down(&mut self, len: usize) {
        if len > 0 {
            self.index = (self.index + 1).min(len - 1);
        }
    }
}

/// Extract the entry id from a `/tree` display line
/// (`"← "` / `"• "` / `"  "` marker, indentation, id, `[kind] text…`).
pub fn tree_line_id(line: &str) -> Option<&str> {
    // Strip the marker CHAR (`←` is multi-byte — a byte slice here returned None).
    let mut chars = line.chars();
    chars.next()?;
    chars
        .as_str()
        .split_whitespace()
        .next()
        .filter(|tok| tok.len() >= 4 && tok.chars().all(|c| c.is_ascii_hexdigit()))
}

/// The keybinding table — single source (§11.3): the `?` overlay renders THIS, so a
/// rebind or addition can never leave the help stale.
pub const KEYBINDINGS: &[(&str, &str)] = &[
    ("Enter", "send the line as a turn"),
    (
        "← (empty line)",
        "Agents View: every session, live state; Enter attaches",
    ),
    (
        "/tree",
        "Tree View: branches; Enter moves the leaf, f forks at the cursor",
    ),
    (
        "/ (line start)",
        "command menu: type to filter, ↑↓ select, Tab completes, Esc dismisses",
    ),
    ("↑ / ↓", "input history (from an empty line, menu closed)"),
    (
        "PageUp / PageDown",
        "scroll the transcript; End follows the newest",
    ),
    ("Ctrl-C", "cancel the running turn; twice within 2s exits"),
    (
        "Ctrl-D (empty line)",
        "detach and exit — the session keeps running",
    ),
    ("?", "this help"),
    ("Esc", "leave an overlay"),
];

/// Render the help overlay: the keys from [`KEYBINDINGS`], then the slash commands this
/// screen serves, read from the shared registry (design D2) so a row added there shows up
/// here — and only here — without a second list to keep in sync.
pub fn help_lines() -> Vec<String> {
    use crate::cli::slash::{Surface, form_label, help_order};

    let key_width = KEYBINDINGS
        .iter()
        .map(|(k, _)| k.chars().count())
        .max()
        .unwrap_or(0);
    let commands: Vec<(String, &'static str)> = help_order(Surface::Tui)
        .iter()
        .flat_map(|spec| {
            spec.forms
                .iter()
                .enumerate()
                .map(move |(i, form)| (form_label(spec, i), form.description.as_ref()))
        })
        .collect();
    let label_width = commands
        .iter()
        .map(|(label, _)| label.chars().count())
        .max()
        .unwrap_or(0);

    let mut lines: Vec<String> = KEYBINDINGS
        .iter()
        .map(|(key, what)| format!("  {key:key_width$}  {what}"))
        .collect();
    lines.push(String::new());
    lines.push("  Commands".to_string());
    lines.extend(
        commands
            .iter()
            .map(|(label, what)| format!("  {label:label_width$}  {what}")),
    );
    // What discovery refused, on the same terms the REPL's `/help` states it. A skill that
    // failed to load is invisible by construction — there is no row for a command that does
    // not exist — so the only place the user can learn their file was passed over is here.
    if let Some(note) = crate::cli::skills::skipped_note(crate::cli::skills::skipped()) {
        lines.push(String::new());
        lines.push(format!("  {note}"));
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roster_glyphs_follow_the_shared_language() {
        let row = |state: &str| RosterRow {
            session_id: "s".into(),
            state: state.into(),
            title: None,
            cwd: "/".into(),
        };
        assert_eq!(row("running").glyph(), "◇");
        assert_eq!(row("idle").glyph(), "●");
        assert_eq!(row("inactive").glyph(), "✓");
    }

    #[test]
    fn list_cursor_clamps_at_both_ends() {
        let mut c = ListCursor::default();
        c.up();
        assert_eq!(c.index, 0);
        c.down(3);
        c.down(3);
        c.down(3);
        assert_eq!(c.index, 2, "stops at the end");
    }

    #[test]
    fn tree_line_ids_parse_from_display_lines() {
        assert_eq!(
            tree_line_id("← b2c3d4e5 [user] alternative"),
            Some("b2c3d4e5")
        );
        assert_eq!(tree_line_id("•   a1b2c3d4 [session]"), Some("a1b2c3d4"));
        assert_eq!(
            tree_line_id("     e5f6a7b8 [ok] answer two"),
            Some("e5f6a7b8")
        );
        assert_eq!(tree_line_id("no id here"), None);
    }

    #[test]
    fn help_is_generated_from_the_keybinding_table() {
        let lines = help_lines();
        assert!(lines.iter().any(|l| l.contains("Agents View")));
        assert!(lines.iter().any(|l| l.contains("twice within 2s")));
        for (key, _) in KEYBINDINGS {
            assert!(
                lines.iter().any(|l| l.contains(key)),
                "the {key} binding is missing from /help"
            );
        }
    }

    #[test]
    fn help_lists_every_command_the_tui_claims() {
        use crate::cli::slash::{Surface, form_label, help_order};

        let lines = help_lines();
        assert!(lines.iter().any(|l| l == "  Commands"));
        for spec in help_order(Surface::Tui) {
            for i in 0..spec.forms.len() {
                let label = form_label(spec, i);
                assert!(
                    lines.iter().any(|l| l.contains(&label)),
                    "/help does not mention {label}"
                );
            }
        }
        // …and nothing the TUI cannot run: /menu is REPL-only.
        assert!(!lines.iter().any(|l| l.contains("/menu")));
    }
}
