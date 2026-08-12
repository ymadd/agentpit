//! The TUI's mini design system (§11.3, modeled on prime-agent's two-layer theme:
//! a raw palette → semantic tokens → components; opencode's bordered-input layout).
//!
//! ## Tokens
//!
//! | Layer | Token | Value | Used by |
//! |-------|-------|-------|---------|
//! | palette | `ACCENT` | #8abeb7 | labels, titles, selection symbol |
//! | palette | `CYAN` | #00d7ff | running state, border accent |
//! | palette | `BLUE` | #5f87ff | input border (focused) |
//! | palette | `GREEN` | #b5bd68 | success |
//! | palette | `RED` | #cc6666 | errors |
//! | palette | `YELLOW` | #f0c674 | warnings, idle state |
//! | palette | `MUTED` | #808080 | secondary text |
//! | palette | `DIM` | #666666 | progress lines, hints |
//! | palette | `USER_BG` | #343541 | the user-message card |
//! | palette | `SELECTED_BG` | #3a3a4a | overlay selection row |
//! | palette | `BORDER_MUTED` | #505050 | input border while busy |
//!
//! ## Components and their states
//!
//! - **Input box**: rounded border; `focused` (BLUE) / `busy` (BORDER_MUTED — typing is
//!   still allowed, the dimming just signals a turn is running).
//! - **Status bar**: `idle` (session id + key hints, muted) / `busy` (working pulse
//!   ◇◈◆◈ + elapsed + backend + cancel hint) / `busy+hint` (after ~15s a rotating
//!   "Hint:" line, prime's rotator).
//! - **Command dropdown**: the slash menu anchored above the input box — rounded
//!   BORDER_MUTED frame, label in ACCENT, description in DIM, highlighted row in
//!   `style_selected` (the same selection vocabulary as the overlays).
//! - **Transcript line**: `user` (USER_BG card, "› " lead) / `backend-turn-start`
//!   (dim "◈ backend" marker) / `answer` (plain text) / `progress` (DIM, "◈" lead) /
//!   `notice` (YELLOW) / `error` (RED) / `summary` (ACCENT rule).
//!
//! Every visual choice in the TUI must come from here — hardcoded colors in the app
//! loop are a bug (the design-system rule this module exists to enforce).

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

// ── palette ─────────────────────────────────────────────────────────────────
pub const ACCENT: Color = Color::Rgb(0x8a, 0xbe, 0xb7);
pub const CYAN: Color = Color::Rgb(0x00, 0xd7, 0xff);
pub const BLUE: Color = Color::Rgb(0x5f, 0x87, 0xff);
pub const GREEN: Color = Color::Rgb(0xb5, 0xbd, 0x68);
pub const RED: Color = Color::Rgb(0xcc, 0x66, 0x66);
pub const YELLOW: Color = Color::Rgb(0xf0, 0xc6, 0x74);
pub const MUTED: Color = Color::Rgb(0x80, 0x80, 0x80);
pub const DIM: Color = Color::Rgb(0x66, 0x66, 0x66);
pub const USER_BG: Color = Color::Rgb(0x34, 0x35, 0x41);
pub const SELECTED_BG: Color = Color::Rgb(0x3a, 0x3a, 0x4a);
pub const BORDER_MUTED: Color = Color::Rgb(0x50, 0x50, 0x50);

// ── motion ──────────────────────────────────────────────────────────────────
/// prime's working pulse (250ms) — shown while a turn runs.
pub const WORKING_PULSE: [&str; 4] = ["◇", "◈", "◆", "◈"];
/// Frame period for the pulse, in ticks of the 100ms app timer.
pub const PULSE_TICKS: usize = 2;
/// Seconds of busy time before the hint rotator joins the status bar.
pub const HINT_AFTER_SECS: u64 = 15;
/// Seconds each hint stays before rotating.
pub const HINT_ROTATE_SECS: u64 = 10;

/// The hint rotator's pool (prime's feature-discovery idea, agentpit's facts).
pub const HINTS: &[&str] = &[
    "you can close this terminal — the session keeps running (agentpit attach brings it back)",
    "← on an empty line lists every session with its live state",
    "/tree shows branches; Enter moves the leaf, f forks at the cursor",
    "!codex …  routes a single turn to codex without switching the default",
    "agentpit orchestrate runs TypeScript cells that fan work out to backends",
];

// ── semantic styles ─────────────────────────────────────────────────────────
pub fn style_dim() -> Style {
    Style::default().fg(DIM)
}

pub fn style_muted() -> Style {
    Style::default().fg(MUTED)
}

pub fn style_error() -> Style {
    Style::default().fg(RED)
}

pub fn style_notice() -> Style {
    Style::default().fg(YELLOW)
}

pub fn style_accent() -> Style {
    Style::default().fg(ACCENT)
}

// Markdown tokens (§11.3, prime-agent's rendered answers): headings in the accent,
// code in green, quotes muted-italic — all drawn from the palette above.
pub fn style_md_heading() -> Style {
    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
}

pub fn style_md_code() -> Style {
    Style::default().fg(GREEN)
}

pub fn style_md_quote() -> Style {
    Style::default().fg(MUTED).add_modifier(Modifier::ITALIC)
}

pub fn style_md_bold() -> Style {
    Style::default().add_modifier(Modifier::BOLD)
}

pub fn style_md_italic() -> Style {
    Style::default().add_modifier(Modifier::ITALIC)
}

/// Overlay selection row (SELECTED_BG + accent, prime's tree-selector cursor).
pub fn style_selected() -> Style {
    Style::default()
        .bg(SELECTED_BG)
        .fg(CYAN)
        .add_modifier(Modifier::BOLD)
}

// ── component builders (pure — unit-tested) ────────────────────────────────

/// The one-time header block inserted into scrollback on attach (prime renders its
/// butterfly + version/model/cwd exactly once; this is agentpit's equivalent).
pub fn header_lines(version: &str, session_short: &str, cwd: &str) -> Vec<Line<'static>> {
    vec![
        Line::from(Span::styled(
            "▐ agentpit ▌".to_string(),
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        )),
        Line::from(vec![
            Span::styled("  version ".to_string(), style_dim()),
            Span::styled(version.to_string(), style_muted()),
            Span::styled("   session ".to_string(), style_dim()),
            Span::styled(session_short.to_string(), style_muted()),
        ]),
        Line::from(vec![
            Span::styled("  cwd ".to_string(), style_dim()),
            Span::styled(cwd.to_string(), style_muted()),
        ]),
        Line::from(""),
    ]
}

/// A user turn as a background card with a lead glyph (prime's userMsgBg).
pub fn user_line(text: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            " › ".to_string(),
            Style::default()
                .bg(USER_BG)
                .fg(ACCENT)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("{text} "),
            Style::default().bg(USER_BG).fg(Color::Reset),
        ),
    ])
}

/// The dim marker line opening a backend's turn (`◈ codex`).
pub fn turn_start_line(backend: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled("◈ ".to_string(), Style::default().fg(ACCENT)),
        Span::styled(backend.to_string(), style_muted()),
    ])
}

/// A tool/progress line: dim with the pulse glyph lead (folded-tool de-emphasis).
pub fn progress_line(text: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled("  ◈ ".to_string(), style_dim()),
        Span::styled(text.to_string(), style_dim()),
    ])
}

/// The idle status line: session identity left, key hints right of it.
pub fn status_idle(session_short: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled(session_short.to_string(), style_accent()),
        Span::styled(
            "  ← agents · /tree branches · ? keys · Ctrl-D detach".to_string(),
            style_dim(),
        ),
    ])
}

/// The busy status line: pulse + elapsed + backend + cancel hint (+ rotating hint).
/// `tick` is the 100ms app-timer count driving the pulse.
pub fn status_busy(
    tick: usize,
    elapsed_secs: u64,
    backend: &str,
    hint: Option<&str>,
) -> Line<'static> {
    let pulse = WORKING_PULSE[(tick / PULSE_TICKS) % WORKING_PULSE.len()];
    let mut spans = vec![
        Span::styled(
            format!("{pulse} "),
            Style::default().fg(CYAN).add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!("Working {elapsed_secs}s"), style_muted()),
        Span::styled(" · ".to_string(), style_dim()),
        Span::styled(
            if backend.is_empty() {
                "routing…".to_string()
            } else {
                backend.to_string()
            },
            style_accent(),
        ),
        Span::styled(" · Ctrl-C cancels".to_string(), style_dim()),
    ];
    if let Some(h) = hint {
        spans.push(Span::styled("   Hint: ".to_string(), style_notice()));
        spans.push(Span::styled(h.to_string(), style_dim()));
    }
    Line::from(spans)
}

/// Pick the rotating hint for a busy turn, or `None` before [`HINT_AFTER_SECS`].
pub fn current_hint(elapsed_secs: u64) -> Option<&'static str> {
    if elapsed_secs < HINT_AFTER_SECS {
        return None;
    }
    let idx = ((elapsed_secs - HINT_AFTER_SECS) / HINT_ROTATE_SECS) as usize % HINTS.len();
    Some(HINTS[idx])
}

/// Roster row styling for the Agents View: running=cyan bold, idle=warning,
/// inactive=dim (prime's agents-view glyph language, §7.2 B1).
pub fn roster_line(glyph: &str, state: &str, rest: &str) -> Line<'static> {
    let style = match state {
        "running" => Style::default().fg(CYAN).add_modifier(Modifier::BOLD),
        "idle" => Style::default().fg(YELLOW),
        _ => style_dim(),
    };
    Line::from(vec![
        Span::styled(format!("{glyph} {state:9}"), style),
        Span::styled(rest.to_string(), Style::default().fg(Color::Reset)),
    ])
}

/// One row of the slash-command dropdown: the label in the label colour, its description
/// muted behind it. Column width is fixed so the descriptions line up (the widest label
/// the registry offers is `/login [backend]`).
pub fn menu_row(label: &str, description: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{label:MENU_LABEL_WIDTH$}"), style_accent()),
        Span::styled(description.to_string(), style_dim()),
    ])
}

/// Column width for [`menu_row`]'s label.
pub const MENU_LABEL_WIDTH: usize = 18;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn menu_rows_align_their_descriptions() {
        let row = menu_row("/tree", "Show the session tree");
        assert_eq!(row.spans[0].content.chars().count(), MENU_LABEL_WIDTH);
        assert_eq!(row.spans[0].style.fg, Some(ACCENT));
        assert_eq!(row.spans[1].style.fg, Some(DIM));
    }

    #[test]
    fn header_carries_identity_once() {
        let lines = header_lines("0.2.12", "abc123", "/work");
        let flat: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
            .collect();
        assert!(flat.contains("agentpit"));
        assert!(flat.contains("0.2.12"));
        assert!(flat.contains("abc123"));
        assert!(flat.contains("/work"));
    }

    #[test]
    fn user_lines_are_background_cards() {
        let line = user_line("hello");
        assert!(line.spans.iter().all(|s| s.style.bg == Some(USER_BG)));
        assert!(line.spans.iter().any(|s| s.content.contains("hello")));
    }

    #[test]
    fn busy_status_pulses_and_rotates_hints() {
        // The pulse cycles through prime's ◇◈◆◈ as ticks advance.
        let frames: Vec<String> = (0..8)
            .map(|t| {
                status_busy(t, 3, "codex", None).spans[0]
                    .content
                    .to_string()
            })
            .collect();
        assert!(frames.contains(&"◇ ".to_string()));
        assert!(frames.contains(&"◆ ".to_string()));

        // No hint early; hints appear after the threshold and rotate over time.
        assert_eq!(current_hint(5), None);
        let first = current_hint(HINT_AFTER_SECS).unwrap();
        let later = current_hint(HINT_AFTER_SECS + HINT_ROTATE_SECS).unwrap();
        assert_ne!(first, later, "hints must rotate");
        let flat: String = status_busy(0, 20, "codex", Some(first))
            .spans
            .iter()
            .map(|s| s.content.to_string())
            .collect();
        assert!(flat.contains("Hint:"));
        assert!(flat.contains("Working 20s"));
        assert!(flat.contains("codex"));
    }

    #[test]
    fn roster_states_map_to_their_colors() {
        let running = roster_line("◇", "running", " x");
        assert_eq!(running.spans[0].style.fg, Some(CYAN));
        let idle = roster_line("●", "idle", " x");
        assert_eq!(idle.spans[0].style.fg, Some(YELLOW));
        let inactive = roster_line("✓", "inactive", " x");
        assert_eq!(inactive.spans[0].style.fg, Some(DIM));
    }
}
