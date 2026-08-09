//! Transcript wrapping + windowing for the fullscreen TUI (§11.2, revised: the user
//! chose fullscreen for every state, so the transcript scrolls INTERNALLY instead of
//! living in terminal scrollback).
//!
//! Wrapping is done here, cell-accurately (CJK double-width via unicode-width), instead
//! of trusting a widget's own word-wrap: the scroll math and the painted rows must agree
//! exactly, or "follow the bottom" drifts. Pure functions, unit-tested.

use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthChar;

/// Wrap one styled line into display rows of at most `width` cells (character wrap, the
/// terminal's native behavior). Styles survive the split. `width == 0` yields the line
/// unchanged (degenerate area; nothing sane to do).
pub fn wrap_line(line: &Line<'static>, width: usize) -> Vec<Line<'static>> {
    if width == 0 {
        return vec![line.clone()];
    }
    let mut rows: Vec<Line<'static>> = Vec::new();
    let mut current: Vec<Span<'static>> = Vec::new();
    let mut used = 0usize;
    for span in &line.spans {
        let mut buf = String::new();
        for ch in span.content.chars() {
            let w = ch.width().unwrap_or(0);
            if used + w > width && used > 0 {
                if !buf.is_empty() {
                    current.push(Span::styled(std::mem::take(&mut buf), span.style));
                }
                rows.push(Line::from(std::mem::take(&mut current)));
                used = 0;
            }
            buf.push(ch);
            used += w;
        }
        if !buf.is_empty() {
            current.push(Span::styled(buf, span.style));
        }
    }
    rows.push(Line::from(current));
    rows
}

/// The visible window: the last `height` wrapped rows, skipping `offset_from_bottom`
/// rows of the tail (0 = follow the newest). Walks the transcript BACKWARDS and stops as
/// soon as the window is filled, so cost is O(window), not O(history).
/// Returns `(rows, hit_top)` — `hit_top` = the window reached the first line (scrolling
/// further up would show nothing new).
pub fn tail_window(
    lines: &[Line<'static>],
    width: usize,
    height: usize,
    offset_from_bottom: usize,
) -> (Vec<Line<'static>>, bool) {
    let needed = height + offset_from_bottom;
    let mut collected: Vec<Line<'static>> = Vec::new(); // newest-last, built backwards
    let mut hit_top = true;
    for line in lines.iter().rev() {
        let mut rows = wrap_line(line, width);
        rows.reverse();
        for row in rows {
            collected.push(row);
            if collected.len() >= needed {
                break;
            }
        }
        if collected.len() >= needed {
            hit_top = false;
            break;
        }
    }
    collected.reverse();
    // Drop the `offset` newest rows, keep at most `height` above them.
    let end = collected.len().saturating_sub(offset_from_bottom);
    let start = end.saturating_sub(height);
    (collected[start..end].to_vec(), hit_top)
}

/// Clamp a scroll offset so it never runs past the top of the history.
pub fn clamp_offset(lines: &[Line<'static>], width: usize, height: usize, wanted: usize) -> usize {
    if wanted == 0 {
        return 0;
    }
    let total: usize = lines.iter().map(|l| wrap_line(l, width).len()).sum();
    wanted.min(total.saturating_sub(height))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(s: &str) -> Line<'static> {
        Line::raw(s.to_string())
    }

    fn flat(rows: &[Line<'static>]) -> Vec<String> {
        rows.iter()
            .map(|l| l.spans.iter().map(|s| s.content.to_string()).collect())
            .collect()
    }

    #[test]
    fn wraps_at_cell_width_including_cjk() {
        let rows = wrap_line(&plain("abcdef"), 4);
        assert_eq!(flat(&rows), vec!["abcd", "ef"]);
        // 日本語 is double-width: 2 chars = 4 cells per row.
        let rows = wrap_line(&plain("日本語です"), 4);
        assert_eq!(flat(&rows), vec!["日本", "語で", "す"]);
        // A style split survives across the boundary.
        let styled = Line::from(vec![
            Span::styled("abc".to_string(), crate::tui::theme::style_dim()),
            Span::raw("def".to_string()),
        ]);
        let rows = wrap_line(&styled, 4);
        assert_eq!(flat(&rows), vec!["abcd", "ef"]);
        assert_eq!(rows[0].spans[0].style, crate::tui::theme::style_dim());
    }

    #[test]
    fn tail_window_follows_the_bottom_and_scrolls_back() {
        let lines: Vec<Line<'static>> = (1..=5).map(|i| plain(&format!("line{i}"))).collect();
        // Follow mode: the last `height` rows.
        let (rows, hit_top) = tail_window(&lines, 80, 2, 0);
        assert_eq!(flat(&rows), vec!["line4", "line5"]);
        assert!(!hit_top);
        // Scrolled up by 2: the window shifts.
        let (rows, _) = tail_window(&lines, 80, 2, 2);
        assert_eq!(flat(&rows), vec!["line2", "line3"]);
        // Window taller than history: everything, hit_top.
        let (rows, hit_top) = tail_window(&lines, 80, 10, 0);
        assert_eq!(rows.len(), 5);
        assert!(hit_top);
    }

    #[test]
    fn tail_window_counts_wrapped_rows_not_lines() {
        // "abcdefghij" wraps at 4 into abcd|efgh|ij; "xy" adds one more row.
        let lines = vec![plain("abcdefghij"), plain("xy")];
        let (rows, _) = tail_window(&lines, 4, 2, 0);
        assert_eq!(flat(&rows), vec!["ij", "xy"]);
        let (rows, _) = tail_window(&lines, 4, 2, 1);
        assert_eq!(flat(&rows), vec!["efgh", "ij"]);
    }

    #[test]
    fn clamp_stops_at_the_top() {
        let lines: Vec<Line<'static>> = (0..4).map(|i| plain(&format!("l{i}"))).collect();
        assert_eq!(clamp_offset(&lines, 80, 2, 0), 0);
        assert_eq!(clamp_offset(&lines, 80, 2, 1), 1);
        assert_eq!(
            clamp_offset(&lines, 80, 2, 99),
            2,
            "4 rows - 2 visible = max 2"
        );
    }
}
