//! The TUI front end (design §11, revised 2026-08-08): FULLSCREEN in every state, by
//! user decision. The transcript scrolls internally (PageUp/PageDown, End follows the
//! newest) with the editor + status docked at the bottom — prime-agent's fullscreen
//! layout. The Agents View, Tree View, and help render as overlays inside the same
//! alternate screen; Esc returns to the conversation.
//!
//! Detach is exit (§11.4 T1): closing the TUI never touches the worker or its in-flight
//! turn; `agentpit tui --session <id>` comes back to it, and the attach snapshot rebuilds
//! the transcript.

mod chunks;
mod input;
mod scroll;
pub mod theme;
mod views;

use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::event::{Event as TermEvent, EventStream, KeyCode, KeyEvent, KeyModifiers};
use futures::StreamExt;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, List, ListItem, ListState, Paragraph};

use crate::daemon::client::{Conn, connect_daemon, create_session, open_session};
use crate::daemon::protocol::{Event, Frame, RequestBody, ResponseData};
use chunks::{LineAssembler, is_progress_line};
use input::InputState;
use scroll::{clamp_offset, tail_window};
use views::{ListCursor, RosterRow, help_lines, tree_line_id};

/// Transcript memory cap: beyond this many lines the oldest are dropped (the session
/// JSONL remains the durable history; this is only the on-screen buffer).
const TRANSCRIPT_CAP: usize = 5000;

pub async fn run(session: Option<String>) -> Result<()> {
    let cwd = crate::cli::resolve_cwd(None)?;
    let (session_id, conn) = match &session {
        Some(id) => open_session(id, true).await?,
        None => create_session(&cwd, true).await?,
    };

    let mut app = App::new(session_id, conn);
    app.cwd_label = cwd.display().to_string();
    app.enter_terminal().map_err(|e| {
        let _ = leave_screen();
        anyhow::anyhow!(
            "{e:#}. This terminal cannot host the TUI — use `agentpit repl` \
             or `agentpit attach` instead."
        )
    })?;
    let result = app.main_loop().await;
    app.leave_terminal();
    eprintln!(
        "[detached — {}]",
        crate::cli::guidance::detach_hint(crate::cli::guidance::short_id(&app.session_id))
    );
    result
}

fn leave_screen() -> std::io::Result<()> {
    crossterm::execute!(std::io::stdout(), crossterm::terminal::LeaveAlternateScreen)?;
    crossterm::terminal::disable_raw_mode()
}

/// Which surface owns the keys right now.
enum Mode {
    Chat,
    Overlay(Overlay),
}

/// A fullscreen list overlay (Agents / Tree / Help) drawn over the conversation.
struct Overlay {
    kind: OverlayKind,
    title: &'static str,
    items: Vec<Line<'static>>,
    /// `keys[i]` is what Enter/f act on for row i (session id / entry id; empty = none).
    keys: Vec<String>,
    cursor: ListCursor,
}

#[derive(PartialEq)]
enum OverlayKind {
    Agents,
    Tree,
    Help,
}

struct App {
    session_id: String,
    conn: Conn,
    terminal: Option<Terminal<CrosstermBackend<std::io::Stdout>>>,
    mode: Mode,
    transcript: Vec<Line<'static>>,
    /// Rows from the bottom the view is scrolled up by; 0 = follow the newest.
    scroll_offset: usize,
    input: InputState,
    assembler: LineAssembler,
    busy: bool,
    turn_started: Instant,
    last_ctrl_c: Option<Instant>,
    /// Request id of OUR in-flight send (its response ends the busy state).
    pending_send: Option<u64>,
    backend_line: String,
    /// 100ms app-timer count driving the working pulse.
    tick: usize,
    /// Shown in the header; the session's cwd.
    cwd_label: String,
}

impl App {
    fn new(session_id: String, conn: Conn) -> App {
        App {
            session_id,
            conn,
            terminal: None,
            mode: Mode::Chat,
            transcript: Vec::new(),
            scroll_offset: 0,
            input: InputState::default(),
            assembler: LineAssembler::default(),
            busy: false,
            turn_started: Instant::now(),
            last_ctrl_c: None,
            pending_send: None,
            backend_line: String::new(),
            tick: 0,
            cwd_label: String::new(),
        }
    }

    fn enter_terminal(&mut self) -> Result<()> {
        crossterm::terminal::enable_raw_mode()?;
        crossterm::execute!(std::io::stdout(), crossterm::terminal::EnterAlternateScreen)?;
        let backend = CrosstermBackend::new(std::io::stdout());
        self.terminal = Some(Terminal::new(backend)?);
        Ok(())
    }

    fn leave_terminal(&mut self) {
        self.terminal = None;
        let _ = leave_screen();
    }

    /// Append a transcript line. Follow mode sticks to the bottom; a scrolled-up reader
    /// keeps their place.
    fn push_line(&mut self, line: Line<'static>) {
        self.transcript.push(line);
        if self.transcript.len() > TRANSCRIPT_CAP {
            let drop = self.transcript.len() - TRANSCRIPT_CAP;
            self.transcript.drain(..drop);
            self.scroll_offset = self.scroll_offset.saturating_sub(drop);
        }
    }

    fn push_text(&mut self, text: &str, style: Style) {
        self.push_line(Line::from(Span::styled(text.to_string(), style)));
    }

    /// Attach + rebuild the transcript from the snapshot.
    async fn attach(&mut self) -> Result<()> {
        let short = crate::cli::guidance::short_id(&self.session_id).to_string();
        for line in theme::header_lines(env!("CARGO_PKG_VERSION"), &short, &self.cwd_label) {
            self.push_line(line);
        }
        let tail = crate::config::load_config(None)
            .map(|l| l.config.session.transcript_tail)
            .unwrap_or(400);
        let data = self.conn.request(RequestBody::Attach { tail }).await?;
        if let ResponseData::Snapshot {
            transcript,
            total_entries,
            shown,
            ..
        } = data
        {
            let lines: Vec<Line<'static>> = transcript
                .iter()
                .map(|(who, text)| transcript_line(who, text))
                .collect();
            for line in lines {
                self.push_line(line);
            }
            if total_entries > shown {
                self.push_text(
                    &format!("Showing latest {shown} of {total_entries} messages for faster open."),
                    theme::style_dim(),
                );
            }
        }
        self.push_text(
            &format!(
                "[attached to {short} — ? for keys; Ctrl-D detaches, the session keeps running]"
            ),
            theme::style_dim(),
        );
        self.scroll_offset = 0;
        Ok(())
    }

    async fn main_loop(&mut self) -> Result<()> {
        self.attach().await?;
        let mut events = EventStream::new();
        let mut tick = tokio::time::interval(Duration::from_millis(100));
        loop {
            self.draw()?;
            tokio::select! {
                frame = self.conn.recv_frame() => {
                    match frame {
                        Err(e) => {
                            let short = crate::cli::guidance::short_id(&self.session_id).to_string();
                            self.push_text(
                                &format!("connection lost ({e:#}) — reopen with `agentpit tui --session {short}`"),
                                theme::style_error(),
                            );
                            self.draw()?;
                            tokio::time::sleep(Duration::from_millis(1500)).await;
                            return Ok(());
                        }
                        Ok(frame) => { self.on_frame(frame); }
                    }
                }
                ev = events.next() => {
                    match ev {
                        Some(Ok(TermEvent::Key(key))) => {
                            if self.on_key(key).await? {
                                return Ok(());
                            }
                        }
                        Some(Ok(_)) => {}
                        Some(Err(_)) | None => return Ok(()),
                    }
                }
                _ = tick.tick() => {
                    self.tick = self.tick.wrapping_add(1);
                }
            }
        }
    }

    fn on_frame(&mut self, frame: Frame) {
        match frame {
            Frame::Event(Event::TurnStarted { backend }) => {
                self.busy = true;
                self.turn_started = Instant::now();
                self.push_line(theme::turn_start_line(&backend));
                self.backend_line = backend;
            }
            Frame::Event(Event::Chunk { text }) => {
                self.busy = true;
                for line in self.assembler.push(&text) {
                    if is_progress_line(&line) {
                        self.push_line(theme::progress_line(&line));
                    } else {
                        self.push_text(&line, Style::default());
                    }
                }
            }
            Frame::Event(Event::TurnFinished { status }) => {
                if let Some(rest) = self.assembler.finish() {
                    self.push_text(&rest, Style::default());
                }
                if status != "ok" {
                    self.push_text(&format!("[turn ended: {status}]"), theme::style_notice());
                }
                self.busy = false;
                self.backend_line.clear();
            }
            Frame::Event(Event::Notice { text }) => {
                self.push_text(&format!("session: {text}"), theme::style_notice());
            }
            Frame::Response(resp) => {
                if Some(resp.id) == self.pending_send {
                    self.pending_send = None;
                    self.busy = false;
                    if !resp.ok {
                        self.push_text(
                            &format!("error: {}", resp.error.unwrap_or_default()),
                            theme::style_error(),
                        );
                    }
                }
            }
        }
    }

    /// Handle a key; `true` = exit the TUI.
    async fn on_key(&mut self, key: KeyEvent) -> Result<bool> {
        if let Mode::Overlay(_) = self.mode {
            self.on_overlay_key(key).await?;
            return Ok(false);
        }
        match (key.code, key.modifiers) {
            (KeyCode::Char('c'), KeyModifiers::CONTROL) => {
                if self.busy {
                    let _ = self.conn.send_request(RequestBody::Cancel).await;
                    self.push_text("[cancelling…]", theme::style_notice());
                } else if self
                    .last_ctrl_c
                    .map(|t| t.elapsed() <= Duration::from_secs(2))
                    .unwrap_or(false)
                {
                    return Ok(true);
                } else {
                    self.last_ctrl_c = Some(Instant::now());
                    self.push_text("(press Ctrl-C again within 2s to exit)", theme::style_dim());
                }
            }
            (KeyCode::Char('d'), KeyModifiers::CONTROL) if self.input.is_empty() => {
                return Ok(true);
            }
            (KeyCode::PageUp, _) => self.scroll_by(1),
            (KeyCode::PageDown, _) => self.scroll_by(-1),
            (KeyCode::End, _) if self.input.is_empty() => self.scroll_offset = 0,
            (KeyCode::Char('?'), _) if self.input.is_empty() => {
                let lines = help_lines();
                self.mode = Mode::Overlay(Overlay {
                    kind: OverlayKind::Help,
                    title: "Keys — Esc returns",
                    keys: vec![String::new(); lines.len()],
                    items: lines.into_iter().map(Line::raw).collect(),
                    cursor: ListCursor::default(),
                });
            }
            (KeyCode::Left, _) if self.input.is_empty() => {
                let rows = fetch_roster().await;
                let items: Vec<Line<'static>> = rows
                    .iter()
                    .map(|r| {
                        theme::roster_line(
                            r.glyph(),
                            &r.state,
                            &format!(
                                " {}  {}  {}",
                                crate::cli::guidance::short_id(&r.session_id),
                                r.title.as_deref().unwrap_or("-"),
                                r.cwd
                            ),
                        )
                    })
                    .collect();
                self.mode = Mode::Overlay(Overlay {
                    kind: OverlayKind::Agents,
                    title: "Agents — Enter attaches, Esc returns",
                    keys: rows.into_iter().map(|r| r.session_id).collect(),
                    items,
                    cursor: ListCursor::default(),
                });
            }
            (KeyCode::Enter, _) => {
                let line = self.input.submit();
                if line.is_empty() {
                } else if line == "/tree" {
                    let lines = match self.conn.request(RequestBody::Tree).await {
                        Ok(ResponseData::Lines { lines }) => lines,
                        _ => return Ok(false),
                    };
                    self.mode = Mode::Overlay(Overlay {
                        kind: OverlayKind::Tree,
                        title: "Tree — Enter moves the leaf, f forks at the cursor, Esc returns",
                        keys: lines
                            .iter()
                            .map(|l| tree_line_id(l).unwrap_or("").to_string())
                            .collect(),
                        items: lines.into_iter().map(Line::raw).collect(),
                        cursor: ListCursor::default(),
                    });
                } else if matches!(line.as_str(), "/quit" | "/exit" | "/detach") {
                    return Ok(true);
                } else {
                    self.push_line(theme::user_line(&line));
                    self.scroll_offset = 0; // sending re-follows the bottom
                    let id = self
                        .conn
                        .send_request(RequestBody::Send {
                            text: line,
                            backend: None,
                        })
                        .await?;
                    self.pending_send = Some(id);
                    self.busy = true;
                    self.turn_started = Instant::now();
                }
            }
            (KeyCode::Backspace, _) => self.input.backspace(),
            (KeyCode::Left, _) => self.input.left(),
            (KeyCode::Right, _) => self.input.right(),
            (KeyCode::Home, _) => self.input.home(),
            (KeyCode::End, _) => self.input.end(),
            (KeyCode::Up, _) => self.input.history_prev(),
            (KeyCode::Down, _) => self.input.history_next(),
            (KeyCode::Char(c), m) if m.is_empty() || m == KeyModifiers::SHIFT => {
                self.input.insert(c);
            }
            _ => {}
        }
        Ok(false)
    }

    /// Scroll by half a screen; +1 = up (older), -1 = down (newer).
    fn scroll_by(&mut self, direction: i32) {
        let (width, height) = self
            .terminal
            .as_ref()
            .and_then(|t| t.size().ok())
            .map(|s| (s.width as usize, s.height.saturating_sub(5) as usize))
            .unwrap_or((80, 20));
        let step = (height / 2).max(1);
        let wanted = if direction > 0 {
            self.scroll_offset + step
        } else {
            self.scroll_offset.saturating_sub(step)
        };
        self.scroll_offset = clamp_offset(&self.transcript, width, height, wanted);
    }

    async fn on_overlay_key(&mut self, key: KeyEvent) -> Result<()> {
        let Mode::Overlay(overlay) = &mut self.mode else {
            return Ok(());
        };
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                self.mode = Mode::Chat;
            }
            KeyCode::Up => overlay.cursor.up(),
            KeyCode::Down => overlay.cursor.down(overlay.items.len()),
            KeyCode::Enter => {
                let taken = std::mem::replace(&mut self.mode, Mode::Chat);
                let Mode::Overlay(overlay) = taken else {
                    return Ok(());
                };
                let picked = overlay.keys.get(overlay.cursor.index).cloned();
                match (overlay.kind, picked) {
                    (OverlayKind::Agents, Some(session)) if !session.is_empty() => {
                        self.switch_session(session).await?;
                    }
                    (OverlayKind::Tree, Some(target)) if !target.is_empty() => {
                        match self
                            .conn
                            .request(RequestBody::Branch {
                                target: target.clone(),
                                summary: None,
                            })
                            .await
                        {
                            Ok(_) => self.push_text(
                                &format!("moved to {target} — the next turn continues from there"),
                                theme::style_accent(),
                            ),
                            Err(e) => {
                                self.push_text(&format!("error: {e:#}"), theme::style_error())
                            }
                        }
                    }
                    _ => {}
                }
            }
            KeyCode::Char('f') if overlay.kind == OverlayKind::Tree => {
                let picked = overlay.keys.get(overlay.cursor.index).cloned();
                self.mode = Mode::Chat;
                if let Some(at) = picked.filter(|a| !a.is_empty()) {
                    match self.conn.request(RequestBody::Fork { at: Some(at) }).await {
                        Ok(ResponseData::Forked { session_id }) => {
                            let tail = crate::cli::guidance::short_id(&session_id).to_string();
                            self.push_text(
                                &format!("forked — open it with `agentpit tui --session {tail}`"),
                                theme::style_accent(),
                            );
                        }
                        Ok(_) => {}
                        Err(e) => self.push_text(&format!("error: {e:#}"), theme::style_error()),
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn draw(&mut self) -> Result<()> {
        let Some(terminal) = &mut self.terminal else {
            return Ok(());
        };
        let input_text = self.input.text().to_string();
        let cursor = self.input.cursor() as u16;
        let live = self.assembler.tail().to_string();
        let short = crate::cli::guidance::short_id(&self.session_id).to_string();
        let elapsed = self.turn_started.elapsed().as_secs();
        let mut status_line = if self.busy {
            theme::status_busy(
                self.tick,
                elapsed,
                &self.backend_line,
                theme::current_hint(elapsed),
            )
        } else {
            theme::status_idle(&short)
        };
        if self.scroll_offset > 0 {
            status_line.spans.push(Span::styled(
                "  ↑ scrolled · End follows",
                theme::style_notice(),
            ));
        }
        let border_style = if self.busy {
            Style::default().fg(theme::BORDER_MUTED)
        } else {
            Style::default().fg(theme::BLUE)
        };
        let transcript = &self.transcript;
        let scroll_offset = self.scroll_offset;
        let overlay = match &self.mode {
            Mode::Overlay(o) => Some(o),
            Mode::Chat => None,
        };
        terminal.draw(|f| {
            let [transcript_area, live_area, box_area, status_area] = Layout::vertical([
                Constraint::Min(1),
                Constraint::Length(1),
                Constraint::Length(3),
                Constraint::Length(1),
            ])
            .areas(f.area());

            let (rows, _) = tail_window(
                transcript,
                transcript_area.width as usize,
                transcript_area.height as usize,
                scroll_offset,
            );
            f.render_widget(Paragraph::new(rows), transcript_area);

            f.render_widget(
                Paragraph::new(live.as_str()).style(theme::style_dim()),
                live_area,
            );

            let input_box = Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(border_style);
            let inner = input_box.inner(box_area);
            f.render_widget(input_box, box_area);
            f.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::styled("› ", theme::style_accent()),
                    Span::raw(input_text.as_str()),
                ])),
                inner,
            );
            f.render_widget(Paragraph::new(status_line), status_area);

            if let Some(overlay) = overlay {
                let area = f.area();
                f.render_widget(Clear, area);
                let mut state = ListState::default();
                state.select((!overlay.items.is_empty()).then_some(overlay.cursor.index));
                let list = List::new(overlay.items.iter().map(|l| ListItem::new(l.clone())))
                    .block(
                        Block::default()
                            .borders(Borders::ALL)
                            .border_type(BorderType::Rounded)
                            .border_style(Style::default().fg(theme::BORDER_MUTED))
                            .title(Span::styled(overlay.title, theme::style_accent())),
                    )
                    .highlight_style(theme::style_selected())
                    .highlight_symbol("› ");
                f.render_stateful_widget(list, area, &mut state);
            } else {
                f.set_cursor_position((inner.x + 2 + cursor, inner.y));
            }
        })?;
        Ok(())
    }

    /// Detach from the current worker, ensure + attach the chosen session (§11.3).
    async fn switch_session(&mut self, session: String) -> Result<()> {
        let _ = self.conn.request(RequestBody::Detach).await;
        let (session_id, conn) = open_session(&session, true).await?;
        self.session_id = session_id;
        self.conn = conn;
        self.transcript.clear();
        self.scroll_offset = 0;
        self.attach().await
    }
}

/// One transcript line, styled by speaker via the theme (§11.3).
fn transcript_line(who: &str, text: &str) -> Line<'static> {
    match who {
        "user" => theme::user_line(text),
        "summary" => Line::from(vec![
            Span::styled("── summary ── ".to_string(), theme::style_accent()),
            Span::styled(text.to_string(), theme::style_muted()),
        ]),
        backend => Line::from(vec![
            Span::styled("◈ ".to_string(), theme::style_accent()),
            Span::styled(
                format!("{backend}  "),
                theme::style_muted().add_modifier(ratatui::style::Modifier::BOLD),
            ),
            Span::raw(text.to_string()),
        ]),
    }
}

async fn fetch_roster() -> Vec<RosterRow> {
    let Ok(mut conn) = connect_daemon(false).await else {
        return Vec::new();
    };
    match conn.request(RequestBody::List).await {
        Ok(ResponseData::Sessions { sessions }) => sessions
            .into_iter()
            .map(|r| RosterRow {
                session_id: r.session_id,
                state: r.state,
                title: r.title,
                cwd: r.cwd,
            })
            .collect(),
        _ => Vec::new(),
    }
}
