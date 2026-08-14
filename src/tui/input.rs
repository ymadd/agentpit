//! The TUI input editor (design §11.3) — deliberately small: multiline insert/paste,
//! delete, cursor movement, and history recall. Pure state + pure ops, fully unit-tested;
//! the render layer draws `text` with a cursor at `cursor` (a char index).

#[derive(Debug, Default)]
pub struct InputState {
    text: String,
    /// Char index (not byte) of the cursor.
    cursor: usize,
    history: Vec<String>,
    /// `None` = editing a fresh line; `Some(i)` = browsing history entry i.
    browse: Option<usize>,
    /// The fresh line stashed while browsing history.
    stash: String,
}

impl InputState {
    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    pub fn insert(&mut self, c: char) {
        let byte = self.byte_at(self.cursor);
        self.text.insert(byte, c);
        self.cursor += 1;
        self.browse = None;
    }

    /// Insert a paste as one edit. CRLF/bare CR are normalized so a paste copied from
    /// another platform renders as the same multiline buffer instead of visible `\r`s.
    /// Other control characters are dropped, except tab and newline which are meaningful
    /// prompt text.
    pub fn insert_paste(&mut self, pasted: &str) {
        let normalized = pasted.replace("\r\n", "\n").replace('\r', "\n");
        let clean: String = normalized
            .chars()
            .filter(|c| !c.is_control() || matches!(c, '\n' | '\t'))
            .collect();
        if clean.is_empty() {
            return;
        }
        let byte = self.byte_at(self.cursor);
        let added = clean.chars().count();
        self.text.insert_str(byte, &clean);
        self.cursor += added;
        self.browse = None;
    }

    /// (zero-based row, display-cell column) of the cursor in the multiline buffer.
    pub fn cursor_row_col(&self) -> (usize, usize) {
        let before: String = self.text.chars().take(self.cursor).collect();
        let row = before.chars().filter(|&c| c == '\n').count();
        let line = before.rsplit('\n').next().unwrap_or("");
        let col = line
            .chars()
            .map(|c| unicode_width::UnicodeWidthChar::width(c).unwrap_or(0))
            .sum();
        (row, col)
    }

    pub fn line_count(&self) -> usize {
        self.text.chars().filter(|&c| c == '\n').count() + 1
    }

    pub fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let byte = self.byte_at(self.cursor - 1);
        self.text.remove(byte);
        self.cursor -= 1;
    }

    /// Replace the whole line, cursor at the end — what the slash menu's accept does
    /// (`super::completion`). Ends any history browse: the line is a fresh edit now.
    pub fn set_line(&mut self, text: &str) {
        self.text = text.to_string();
        self.cursor = self.char_len();
        self.browse = None;
    }

    /// Replace a range expressed in character indices and leave the cursor after the
    /// replacement. Completion menus use this instead of rebuilding the whole line so
    /// text on either side of an inline token is preserved.
    pub fn replace_range(&mut self, start: usize, end: usize, replacement: &str) {
        let start_byte = self.byte_at(start);
        let end_byte = self.byte_at(end);
        self.text.replace_range(start_byte..end_byte, replacement);
        self.cursor = start + replacement.chars().count();
        self.browse = None;
    }

    pub fn left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    pub fn right(&mut self) {
        self.cursor = (self.cursor + 1).min(self.char_len());
    }

    pub fn home(&mut self) {
        self.cursor = 0;
    }

    pub fn end(&mut self) {
        self.cursor = self.char_len();
    }

    /// Submit the current line: returns it (trimmed) and records history.
    pub fn submit(&mut self) -> String {
        let line = self.text.trim().to_string();
        if !line.is_empty() && self.history.last().map(String::as_str) != Some(line.as_str()) {
            self.history.push(line.clone());
        }
        self.text.clear();
        self.cursor = 0;
        self.browse = None;
        line
    }

    /// ↑: browse history (older). Only from an empty line or while already browsing —
    /// prime's rule, so an ↑ mid-composition never destroys the draft.
    pub fn history_prev(&mut self) {
        if self.history.is_empty() || (!self.is_empty() && self.browse.is_none()) {
            return;
        }
        let next = match self.browse {
            None => {
                self.stash = std::mem::take(&mut self.text);
                self.history.len() - 1
            }
            Some(0) => 0,
            Some(i) => i - 1,
        };
        self.browse = Some(next);
        self.text = self.history[next].clone();
        self.cursor = self.char_len();
    }

    /// ↓: browse newer; past the newest restores the stashed draft.
    pub fn history_next(&mut self) {
        let Some(i) = self.browse else { return };
        if i + 1 < self.history.len() {
            self.browse = Some(i + 1);
            self.text = self.history[i + 1].clone();
        } else {
            self.browse = None;
            self.text = std::mem::take(&mut self.stash);
        }
        self.cursor = self.char_len();
    }

    fn char_len(&self) -> usize {
        self.text.chars().count()
    }

    fn byte_at(&self, char_idx: usize) -> usize {
        self.text
            .char_indices()
            .nth(char_idx)
            .map(|(b, _)| b)
            .unwrap_or(self.text.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_move_delete_multibyte_safe() {
        let mut s = InputState::default();
        for c in "日本語ok".chars() {
            s.insert(c);
        }
        assert_eq!(s.text(), "日本語ok");
        s.left();
        s.left();
        s.backspace(); // removes 語
        assert_eq!(s.text(), "日本ok");
        s.home();
        s.insert('→');
        assert_eq!(s.text(), "→日本ok");
        s.end();
        s.insert('!');
        assert_eq!(s.text(), "→日本ok!");
    }

    #[test]
    fn replace_range_uses_character_indices_and_preserves_both_sides() {
        let mut s = InputState::default();
        s.set_line("前 @src/古.rs 後");
        s.replace_range(2, 11, "@src/new.rs ");
        assert_eq!(s.text(), "前 @src/new.rs  後");
        assert_eq!(s.cursor(), "前 @src/new.rs ".chars().count());
    }

    #[test]
    fn submit_records_history_and_clears() {
        let mut s = InputState::default();
        for c in "first".chars() {
            s.insert(c);
        }
        assert_eq!(s.submit(), "first");
        assert!(s.is_empty());
        for c in "first".chars() {
            s.insert(c);
        }
        assert_eq!(s.submit(), "first");
        // Consecutive duplicates collapse.
        s.history_prev();
        assert_eq!(s.text(), "first");
        s.history_prev();
        assert_eq!(s.text(), "first", "single deduped entry");
    }

    #[test]
    fn history_browse_preserves_the_draft() {
        let mut s = InputState::default();
        for line in ["one", "two"] {
            for c in line.chars() {
                s.insert(c);
            }
            s.submit();
        }
        // A non-empty draft blocks history (prime's rule).
        s.insert('d');
        s.history_prev();
        assert_eq!(s.text(), "d");
        s.backspace();
        // Empty → browse: two ← one; down restores the (empty) draft.
        s.history_prev();
        assert_eq!(s.text(), "two");
        s.history_prev();
        assert_eq!(s.text(), "one");
        s.history_next();
        assert_eq!(s.text(), "two");
        s.history_next();
        assert_eq!(s.text(), "");
    }

    #[test]
    fn multiline_paste_is_atomic_normalized_and_multibyte_safe() {
        let mut s = InputState::default();
        s.insert_paste("first\r\n日本\rthird\0");
        assert_eq!(s.text(), "first\n日本\nthird");
        assert_eq!(s.line_count(), 3);
        assert_eq!(s.cursor_row_col(), (2, 5));

        s.home();
        s.right();
        s.insert_paste("A\nB");
        assert_eq!(s.text(), "fA\nBirst\n日本\nthird");
        assert_eq!(s.cursor(), 4);
        assert_eq!(s.cursor_row_col(), (1, 1));
    }

    #[test]
    fn cursor_row_col_uses_terminal_cells_for_cjk() {
        let mut s = InputState::default();
        s.insert_paste("one\n日本");
        assert_eq!(s.cursor_row_col(), (1, 4));
    }
}
