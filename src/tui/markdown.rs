//! Line-oriented markdown styling for the transcript (§11.3, prime-agent's rendered
//! answers). Chunks arrive line-by-line, so this is a streaming renderer: each completed
//! line is styled on its own, and the ONLY state carried between lines is whether a code
//! fence is open. No layout is changed — a styled line wraps and scrolls exactly like the
//! plain text did, so the scroll math in `scroll.rs` is untouched.

use ratatui::text::{Line, Span};

use super::theme;

/// Styles one markdown line at a time; `in_code` survives across lines so fenced blocks
/// render as code even though the renderer never sees the whole document.
#[derive(Debug, Default)]
pub struct MdRenderer {
    in_code: bool,
}

impl MdRenderer {
    /// Forget an unclosed fence (a new turn starts a new document).
    pub fn reset(&mut self) {
        self.in_code = false;
    }

    pub fn render(&mut self, line: &str) -> Line<'static> {
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

    #[test]
    fn headings_lose_their_hashes_and_gain_the_lead() {
        let mut md = MdRenderer::default();
        let line = md.render("## Results");
        assert_eq!(flat(&line), "▍ Results");
        assert_eq!(line.spans[1].style, theme::style_md_heading());
        // Not a heading without the space, and #7 is too deep.
        assert_eq!(flat(&md.render("#nope")), "#nope");
        assert_eq!(flat(&md.render("####### deep")), "####### deep");
    }

    #[test]
    fn code_fences_toggle_block_styling_across_lines() {
        let mut md = MdRenderer::default();
        md.render("```rust");
        let inside = md.render("let x = **not bold**;");
        assert_eq!(flat(&inside), "let x = **not bold**;");
        assert_eq!(inside.spans[0].style, theme::style_md_code());
        md.render("```");
        let outside = md.render("plain");
        assert_eq!(outside.spans[0].style, ratatui::style::Style::default());
        // reset() closes a dangling fence.
        md.render("```");
        md.reset();
        assert_eq!(
            md.render("plain").spans[0].style,
            ratatui::style::Style::default()
        );
    }

    #[test]
    fn inline_bold_code_and_italic_split_into_spans() {
        let mut md = MdRenderer::default();
        let line = md.render("run `cargo test` for **all** of *it*");
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
        assert_eq!(md.render("2 * 3 * 4").spans.len(), 1);
        assert_eq!(flat(&md.render("2 * 3 * 4")), "2 * 3 * 4");
        assert_eq!(flat(&md.render("a **dangling")), "a **dangling");
        assert_eq!(flat(&md.render("`open")), "`open");
    }

    #[test]
    fn lists_quotes_and_rules_get_their_glyphs() {
        let mut md = MdRenderer::default();
        let bullet = md.render("  - item with `code`");
        assert_eq!(flat(&bullet), "  • item with code");
        let numbered = md.render("2. second");
        assert_eq!(flat(&numbered), "2. second");
        assert_eq!(numbered.spans[1].style, theme::style_accent());
        assert_eq!(flat(&md.render("> quoted")), "▏ quoted");
        assert_eq!(flat(&md.render("---")), "─".repeat(40));
        // A negative number is not a list; a word before the dot is not either.
        assert_eq!(flat(&md.render("-1. nope")), "-1. nope");
        assert_eq!(flat(&md.render("v2. nope")), "v2. nope");
    }
}
