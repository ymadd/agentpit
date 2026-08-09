//! The TUI's single-line input editor (design §11.3) — deliberately small: insert,
//! delete, cursor movement, history recall. Pure state + pure ops, fully unit-tested;
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
}
