//! Line-oriented markdown styling for the transcript (§11.3, prime-agent's rendered
//! answers). Chunks arrive line-by-line, so this is a streaming renderer: each completed
//! line is styled on its own. Two things are carried across lines — whether a code fence
//! is open, and a pending GFM table, because a table's column widths are only knowable
//! once its last row has arrived. A table therefore lands when it ends (at the first line
//! that is not part of it, or at [`MdRenderer::flush`] when the turn does); every other
//! construct still renders the moment its line does. No layout is changed — a styled line
//! wraps and scrolls exactly like the plain text did, so the scroll math in `scroll.rs`
//! is untouched.

use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use super::theme;

/// Blank columns between two table cells.
const TABLE_GAP: usize = 2;

/// Column alignment, read off the delimiter row's `:` markers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Align {
    Left,
    Center,
    Right,
}

/// A table being read. `Candidate` is the one-line lookahead GFM needs: a `|`-fenced line
/// is only a header once the *next* line turns out to be a delimiter row, so it is held
/// back rather than committed to either reading.
#[derive(Debug)]
enum Table {
    Candidate(String),
    Open {
        /// Header first, then the body; cells already split and trimmed.
        rows: Vec<Vec<String>>,
        aligns: Vec<Align>,
    },
}

/// Styles one markdown line at a time; `in_code` survives across lines so fenced blocks
/// render as code even though the renderer never sees the whole document.
#[derive(Debug, Default)]
pub struct MdRenderer {
    in_code: bool,
    table: Option<Table>,
}

impl MdRenderer {
    /// Forget an unclosed fence (a new turn starts a new document). Callers [`Self::flush`]
    /// first, so a pending table is never dropped with content in it.
    pub fn reset(&mut self) {
        self.in_code = false;
        self.table = None;
    }

    /// Render one line. Usually one line out, but a table's rows come back together when
    /// the table closes, and nothing comes back while one is still being read.
    pub fn render(&mut self, line: &str) -> Vec<Line<'static>> {
        if !self.in_code && is_pipe_row(line) {
            return self.absorb_row(line);
        }
        let mut out = self.flush();
        out.push(self.render_one(line));
        out
    }

    /// Emit whatever is still buffered — the end of the document, or of anything that ends
    /// a table without being a line of its own (a tool row, a finished turn).
    pub fn flush(&mut self) -> Vec<Line<'static>> {
        match self.table.take() {
            None => Vec::new(),
            // A `|` line that never got its delimiter row: ordinary text after all.
            Some(Table::Candidate(line)) => vec![self.render_one(&line)],
            Some(Table::Open { rows, aligns }) => render_table(&rows, &aligns),
        }
    }

    /// Take one `|`-fenced line into the pending table, returning only what that line
    /// displaced (the previous candidate, when two pipe rows meet with no delimiter).
    fn absorb_row(&mut self, line: &str) -> Vec<Line<'static>> {
        match self.table.take() {
            None => {
                self.table = Some(Table::Candidate(line.to_string()));
                Vec::new()
            }
            Some(Table::Candidate(header)) => match delimiter_row(line) {
                Some(aligns) => {
                    self.table = Some(Table::Open {
                        rows: vec![split_row(&header)],
                        aligns,
                    });
                    Vec::new()
                }
                // Two pipe rows in a row is not a table in GFM either — the first is text,
                // and this one gets the same chance the first one had.
                None => {
                    let displaced = self.render_one(&header);
                    self.table = Some(Table::Candidate(line.to_string()));
                    vec![displaced]
                }
            },
            Some(Table::Open { mut rows, aligns }) => {
                rows.push(split_row(line));
                self.table = Some(Table::Open { rows, aligns });
                Vec::new()
            }
        }
    }

    fn render_one(&mut self, line: &str) -> Line<'static> {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") {
            self.in_code = !self.in_code;
            return Line::from(Span::styled(line.to_string(), theme::style_dim()));
        }
        if self.in_code {
            return Line::from(Span::styled(line.to_string(), theme::style_md_code()));
        }
        if let Some(text) = heading_text(trimmed) {
            return Line::from(vec![
                Span::styled("▍ ".to_string(), theme::style_accent()),
                Span::styled(text.to_string(), theme::style_md_heading()),
            ]);
        }
        if is_hr(trimmed) {
            return Line::from(Span::styled("─".repeat(40), theme::style_dim()));
        }
        if let Some(rest) = trimmed.strip_prefix('>') {
            return Line::from(vec![
                Span::styled("▏ ".to_string(), theme::style_dim()),
                Span::styled(rest.trim_start().to_string(), theme::style_md_quote()),
            ]);
        }
        let indent = &line[..line.len() - trimmed.len()];
        for lead in ["- ", "* ", "+ "] {
            if let Some(rest) = trimmed.strip_prefix(lead) {
                let mut spans = vec![
                    Span::raw(indent.to_string()),
                    Span::styled("• ".to_string(), theme::style_accent()),
                ];
                spans.extend(inline_spans(rest));
                return Line::from(spans);
            }
        }
        if let Some((num, rest)) = numbered(trimmed) {
            let mut spans = vec![
                Span::raw(indent.to_string()),
                Span::styled(format!("{num}. "), theme::style_accent()),
            ];
            spans.extend(inline_spans(rest));
            return Line::from(spans);
        }
        Line::from(inline_spans(line))
    }
}

/// `# ` .. `###### ` → the heading text.
fn heading_text(t: &str) -> Option<&str> {
    let hashes = t.chars().take_while(|c| *c == '#').count();
    (1..=6)
        .contains(&hashes)
        .then(|| t[hashes..].strip_prefix(' '))
        .flatten()
        .map(str::trim_start)
}

/// A thematic break: 3+ of the same `-` / `*` / `_` and nothing else.
fn is_hr(t: &str) -> bool {
    t.len() >= 3
        && ["-", "*", "_"]
            .iter()
            .any(|c| t.chars().all(|ch| ch.to_string() == *c))
}

/// A possible table row: `|` first, and at least one more to close a cell.
fn is_pipe_row(line: &str) -> bool {
    let t = line.trim();
    let (_, separators) = pipe_cells(t);
    if t.starts_with('|') && t.ends_with('|') {
        separators >= 2 // a fenced one-column row: `| value |`
    } else {
        separators >= 1 // optional outer pipes: `a | b`
    }
}

/// Split only structural pipes. Escaped pipes and pipes inside inline-code spans remain in
/// their cell, which is essential for the code/shell expressions tables commonly carry.
fn pipe_cells(line: &str) -> (Vec<String>, usize) {
    let mut cells = vec![String::new()];
    let mut separators = 0;
    let mut code_ticks: Option<usize> = None;
    let mut chars = line.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\\' if chars.peek() == Some(&'|') => {
                chars.next();
                cells.last_mut().expect("one cell exists").push('|');
            }
            '`' => {
                let mut run = 1;
                while chars.peek() == Some(&'`') {
                    chars.next();
                    run += 1;
                }
                code_ticks = match code_ticks {
                    Some(open) if open == run => None,
                    None => Some(run),
                    other => other,
                };
                cells
                    .last_mut()
                    .expect("one cell exists")
                    .push_str(&"`".repeat(run));
            }
            '|' if code_ticks.is_none() => {
                separators += 1;
                cells.push(String::new());
            }
            _ => cells.last_mut().expect("one cell exists").push(ch),
        }
    }
    (cells, separators)
}

/// `| a | b |` → `["a", "b"]`. The outer pipes are optional on both ends, as in GFM.
fn split_row(line: &str) -> Vec<String> {
    let t = line.trim();
    let starts_fenced = t.starts_with('|');
    let ends_fenced = t.ends_with('|') && !t.ends_with("\\|");
    let (mut cells, _) = pipe_cells(t);
    if starts_fenced && cells.first().is_some_and(String::is_empty) {
        cells.remove(0);
    }
    if ends_fenced && cells.last().is_some_and(String::is_empty) {
        cells.pop();
    }
    cells
        .into_iter()
        .map(|cell| cell.trim().to_string())
        .collect()
}

/// `|:---|---:|` → the per-column alignment. `None` if any cell is anything but a run of
/// dashes with optional `:` ends — which is what makes the header a header.
fn delimiter_row(line: &str) -> Option<Vec<Align>> {
    split_row(line)
        .into_iter()
        .map(|cell| cell_align(&cell))
        .collect()
}

fn cell_align(cell: &str) -> Option<Align> {
    let (left, rest) = match cell.strip_prefix(':') {
        Some(rest) => (true, rest),
        None => (false, cell),
    };
    let (right, dashes) = match rest.strip_suffix(':') {
        Some(dashes) => (true, dashes),
        None => (false, rest),
    };
    if dashes.is_empty() || !dashes.chars().all(|c| c == '-') {
        return None;
    }
    Some(match (left, right) {
        (true, true) => Align::Center,
        (false, true) => Align::Right,
        _ => Align::Left,
    })
}

/// Lay the collected rows out as aligned columns: the header in bold, a rule under it, and
/// each cell padded to its column. Widths are measured in terminal cells, not chars, so a
/// CJK table lines up the same way `scroll.rs` wraps it.
fn render_table(rows: &[Vec<String>], aligns: &[Align]) -> Vec<Line<'static>> {
    let columns = rows
        .iter()
        .map(Vec::len)
        .chain(std::iter::once(aligns.len()))
        .max()
        .unwrap_or(0);
    if columns == 0 {
        return Vec::new();
    }
    // Style every cell first: padding has to count what is displayed, and inline markup
    // (`code`, **bold**) is not the same width as its source.
    let cells: Vec<Vec<(Vec<Span<'static>>, usize)>> = rows
        .iter()
        .map(|row| {
            (0..columns)
                .map(|column| {
                    let spans = inline_spans(row.get(column).map_or("", String::as_str));
                    let width: usize = spans.iter().map(|s| s.content.width()).sum();
                    (spans, width)
                })
                .collect()
        })
        .collect();
    let widths: Vec<usize> = (0..columns)
        .map(|column| cells.iter().map(|row| row[column].1).max().unwrap_or(0))
        .collect();

    let align_of = |column: usize| aligns.get(column).copied().unwrap_or(Align::Left);
    let mut lines: Vec<Line<'static>> = Vec::with_capacity(rows.len() + 1);
    for (index, row) in cells.iter().enumerate() {
        let mut spans: Vec<Span<'static>> = Vec::new();
        for (column, (cell, width)) in row.iter().enumerate() {
            let pad = widths[column].saturating_sub(*width);
            let (before, after) = match align_of(column) {
                Align::Left => (0, pad),
                Align::Right => (pad, 0),
                Align::Center => (pad / 2, pad - pad / 2),
            };
            if before > 0 {
                spans.push(Span::raw(" ".repeat(before)));
            }
            spans.extend(cell.iter().cloned().map(|mut span| {
                if index == 0 {
                    span.style = span.style.patch(theme::style_md_bold());
                }
                span
            }));
            if column + 1 < columns {
                spans.push(Span::raw(" ".repeat(after + TABLE_GAP)));
            }
        }
        // Padding a row's last filled cells out to their columns would only widen the
        // wrap; a ragged row ends where its content does.
        while spans
            .last()
            .is_some_and(|span| span.content.chars().all(|c| c == ' '))
        {
            spans.pop();
        }
        lines.push(Line::from(spans));
        if index == 0 {
            lines.push(header_rule(&widths));
        }
    }
    lines
}

/// The rule under the header: one dashed run per column, so the columns stay legible
/// without drawing a full box around them.
fn header_rule(widths: &[usize]) -> Line<'static> {
    let gap = " ".repeat(TABLE_GAP);
    let rule = widths
        .iter()
        .map(|width| "─".repeat(*width))
        .collect::<Vec<_>>()
        .join(gap.as_str());
    Line::from(Span::styled(rule, theme::style_dim()))
}

/// `12. rest` → `("12", "rest")`.
fn numbered(t: &str) -> Option<(&str, &str)> {
    let dot = t.find(". ")?;
    let num = &t[..dot];
    (!num.is_empty() && num.chars().all(|c| c.is_ascii_digit())).then(|| (num, &t[dot + 2..]))
}

/// Split a text line into spans for `` `code` ``, `**bold**`, and `*italic*`. Emphasis
/// only opens before a non-space and closes after one (so `2 * 3 * 4` stays literal);
/// an unclosed marker renders literally. No nesting — the first form wins.
fn inline_spans(text: &str) -> Vec<Span<'static>> {
    let chars: Vec<char> = text.chars().collect();
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut lit = String::new();
    let flush = |lit: &mut String, spans: &mut Vec<Span<'static>>| {
        if !lit.is_empty() {
            spans.push(Span::raw(std::mem::take(lit)));
        }
    };
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '`'
            && let Some(end) = chars[i + 1..]
                .iter()
                .position(|c| *c == '`')
                .map(|p| p + i + 1)
            && end > i + 1
        {
            flush(&mut lit, &mut spans);
            let content: String = chars[i + 1..end].iter().collect();
            spans.push(Span::styled(content, theme::style_md_code()));
            i = end + 1;
            continue;
        }
        if chars[i] == '*' {
            let double = chars.get(i + 1) == Some(&'*');
            let start = if double { i + 2 } else { i + 1 };
            if chars.get(start).is_some_and(|c| !c.is_whitespace())
                && let Some(end) = close_star(&chars, start, double)
            {
                flush(&mut lit, &mut spans);
                let content: String = chars[start..end].iter().collect();
                let style = if double {
                    theme::style_md_bold()
                } else {
                    theme::style_md_italic()
                };
                spans.push(Span::styled(content, style));
                i = end + if double { 2 } else { 1 };
                continue;
            }
        }
        lit.push(chars[i]);
        i += 1;
    }
    flush(&mut lit, &mut spans);
    if spans.is_empty() {
        spans.push(Span::raw(String::new()));
    }
    spans
}

/// The closing `*` / `**` at or after `from + 1`, preceded by a non-space.
fn close_star(chars: &[char], from: usize, double: bool) -> Option<usize> {
    (from + 1..chars.len()).find(|&j| {
        chars[j] == '*'
            && !chars[j - 1].is_whitespace()
            && chars[j - 1] != '*'
            && (!double || chars.get(j + 1) == Some(&'*'))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::Modifier;

    fn flat(line: &Line<'static>) -> String {
        line.spans.iter().map(|s| s.content.to_string()).collect()
    }

    /// Render a line that is not part of a table — the 1:1 case every test below but the
    /// table ones is about, with the "exactly one line out" claim asserted rather than
    /// indexed past.
    fn one(md: &mut MdRenderer, line: &str) -> Line<'static> {
        let mut out = md.render(line);
        assert_eq!(out.len(), 1, "{line:?} rendered {} lines", out.len());
        out.remove(0)
    }

    #[test]
    fn headings_lose_their_hashes_and_gain_the_lead() {
        let mut md = MdRenderer::default();
        let line = one(&mut md, "## Results");
        assert_eq!(flat(&line), "▍ Results");
        assert_eq!(line.spans[1].style, theme::style_md_heading());
        // Not a heading without the space, and #7 is too deep.
        assert_eq!(flat(&one(&mut md, "#nope")), "#nope");
        assert_eq!(flat(&one(&mut md, "####### deep")), "####### deep");
    }

    #[test]
    fn code_fences_toggle_block_styling_across_lines() {
        let mut md = MdRenderer::default();
        one(&mut md, "```rust");
        let inside = one(&mut md, "let x = **not bold**;");
        assert_eq!(flat(&inside), "let x = **not bold**;");
        assert_eq!(inside.spans[0].style, theme::style_md_code());
        one(&mut md, "```");
        let outside = one(&mut md, "plain");
        assert_eq!(outside.spans[0].style, ratatui::style::Style::default());
        // reset() closes a dangling fence.
        one(&mut md, "```");
        md.reset();
        assert_eq!(
            one(&mut md, "plain").spans[0].style,
            ratatui::style::Style::default()
        );
    }

    #[test]
    fn inline_bold_code_and_italic_split_into_spans() {
        let mut md = MdRenderer::default();
        let line = one(&mut md, "run `cargo test` for **all** of *it*");
        assert_eq!(flat(&line), "run cargo test for all of it");
        let styled: Vec<(&str, bool)> = line
            .spans
            .iter()
            .map(|s| {
                (
                    s.content.as_ref(),
                    s.style != ratatui::style::Style::default(),
                )
            })
            .collect();
        assert!(styled.contains(&("cargo test", true)));
        assert!(styled.contains(&("all", true)));
        assert!(styled.contains(&("it", true)));
        let bold = line.spans.iter().find(|s| s.content == "all").unwrap();
        assert!(bold.style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn emphasis_needs_tight_markers_and_a_close() {
        let mut md = MdRenderer::default();
        // Spaced stars are arithmetic, not italics; unclosed markers stay literal.
        assert_eq!(one(&mut md, "2 * 3 * 4").spans.len(), 1);
        assert_eq!(flat(&one(&mut md, "2 * 3 * 4")), "2 * 3 * 4");
        assert_eq!(flat(&one(&mut md, "a **dangling")), "a **dangling");
        assert_eq!(flat(&one(&mut md, "`open")), "`open");
    }

    #[test]
    fn lists_quotes_and_rules_get_their_glyphs() {
        let mut md = MdRenderer::default();
        let bullet = one(&mut md, "  - item with `code`");
        assert_eq!(flat(&bullet), "  • item with code");
        let numbered = one(&mut md, "2. second");
        assert_eq!(flat(&numbered), "2. second");
        assert_eq!(numbered.spans[1].style, theme::style_accent());
        assert_eq!(flat(&one(&mut md, "> quoted")), "▏ quoted");
        assert_eq!(flat(&one(&mut md, "---")), "─".repeat(40));
        // A negative number is not a list; a word before the dot is not either.
        assert_eq!(flat(&one(&mut md, "-1. nope")), "-1. nope");
        assert_eq!(flat(&one(&mut md, "v2. nope")), "v2. nope");
    }

    // ─── tables ──────────────────────────────────────────────────────────────────

    /// Every line the renderer produced for `lines`, flattened — a table lands at the end
    /// of its input, so the whole document is what a table test can assert on.
    fn document(lines: &[&str]) -> Vec<String> {
        let mut md = MdRenderer::default();
        let mut out: Vec<Line<'static>> = lines.iter().flat_map(|l| md.render(l)).collect();
        out.extend(md.flush());
        out.iter().map(flat).collect()
    }

    #[test]
    fn a_table_lands_as_padded_columns_under_a_rule() {
        let out = document(&[
            "| コマンド | src/cli/slash.rs |",
            "|---|---|",
            "| /backend /menu | 312, 354 |",
            "| /cwd | 675 |",
        ]);
        // Column 0 is 14 wide (the longest body cell), column 1 is 16 (the header's);
        // the padding is spelled out rather than typed as literal runs of spaces.
        assert_eq!(
            out,
            vec![
                format!("コマンド{}src/cli/slash.rs", " ".repeat(6 + TABLE_GAP)),
                format!("{}  {}", "─".repeat(14), "─".repeat(16)),
                format!("/backend /menu{}312, 354", " ".repeat(TABLE_GAP)),
                format!("/cwd{}675", " ".repeat(10 + TABLE_GAP)),
            ]
        );
    }

    #[test]
    fn column_widths_are_terminal_cells_so_cjk_lines_up() {
        // "コマンド" is 4 chars but 8 columns wide; counting chars would shear every row
        // under it. Header, rule and body all end on the same column or none of them do.
        let out = document(&["| コマンド | b |", "|---|---|", "| /backend /menu | c |"]);
        assert_eq!(out[0].width(), out[1].width());
        assert_eq!(out[1].width(), out[2].width());
    }

    #[test]
    fn the_delimiter_row_sets_alignment_and_the_header_goes_bold() {
        let mut md = MdRenderer::default();
        let mut out: Vec<Line<'static>> = ["| n | x |", "|---:|:--:|", "| 100 | mid |"]
            .iter()
            .flat_map(|l| md.render(l))
            .collect();
        out.extend(md.flush());
        // Both columns are 3 wide: the header's `n` is pushed right, its `x` centred.
        assert_eq!(flat(&out[0]), format!("  n{} x", " ".repeat(TABLE_GAP)));
        assert_eq!(flat(&out[2]), format!("100{}mid", " ".repeat(TABLE_GAP)));
        assert!(
            out[0]
                .spans
                .iter()
                .filter(|s| !s.content.trim().is_empty())
                .all(|s| s.style.add_modifier.contains(Modifier::BOLD)),
            "the header row renders bold"
        );
    }

    #[test]
    fn cells_keep_their_inline_markup_and_pad_by_what_shows() {
        // The padding must count "code" (4), not "`code`" (6), or the column shears.
        let out = document(&["| a | b |", "|---|---|", "| `code` | x |", "| wider | y |"]);
        assert_eq!(out[2], "code   x");
        assert_eq!(out[3], "wider  y");
    }

    #[test]
    fn escaped_and_code_span_pipes_stay_inside_their_cells() {
        let out = document(&[
            "expr | meaning",
            "--- | ---",
            r"a \| b | escaped",
            "`x | y` | code",
        ]);
        assert_eq!(out[0], "expr   meaning");
        assert_eq!(out[2], "a | b  escaped");
        assert_eq!(out[3], "x | y  code");
        assert_eq!(
            split_row("``x | `y` `` | wide code"),
            vec!["``x | `y` ``", "wide code"]
        );
    }

    #[test]
    fn a_pipe_line_without_a_delimiter_row_stays_ordinary_text() {
        // Prose, not a table: nothing is swallowed and nothing is re-laid-out.
        assert_eq!(
            document(&["| not a table", "| nor this |", "after"]),
            vec!["| not a table", "| nor this |", "after"]
        );
    }

    #[test]
    fn a_table_ends_at_the_first_line_that_is_not_one() {
        let out = document(&["intro", "| a |", "|---|", "| 1 |", "after"]);
        assert_eq!(out, vec!["intro", "a", "─", "1", "after"]);
    }

    #[test]
    fn a_pipe_line_inside_a_fence_is_left_alone() {
        let out = document(&["```", "| a | b |", "|---|---|", "```"]);
        assert_eq!(out, vec!["```", "| a | b |", "|---|---|", "```"]);
    }

    #[test]
    fn ragged_rows_keep_every_cell_and_end_where_their_content_does() {
        // More cells than the header still all show; fewer do not trail into padding.
        let out = document(&["| a | b |", "|---|---|", "| 1 |", "| 2 | 3 | 4 |"]);
        assert_eq!(out[2], "1");
        assert_eq!(out[3], "2  3  4");
    }
}
