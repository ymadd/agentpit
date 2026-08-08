//! The TUI front end (design §11): another client of the worker protocol.
//!
//! The conversation stays INLINE — transcript lines go to the terminal's own scrollback
//! via `insert_before`, and only a small viewport (live line + input + status) is ever
//! repainted — prime's best property, realized with ratatui's inline viewport instead of
//! a bespoke renderer (§11.2). The Agents View, Tree View, and help are fullscreen
//! overlays on the alternate screen, entered and left per use.
//!
//! Detach is exit (§11.4 T1): closing the TUI never touches the worker or its in-flight
//! turn; `agentpit tui --session <id>` comes back to it.

mod chunks;
mod input;
pub mod theme;
mod views;

use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::event::{Event as TermEvent, EventStream, KeyCode, KeyEvent, KeyModifiers};
use futures::StreamExt;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, List, ListItem, ListState, Paragraph, Widget};

use crate::daemon::client::{Conn, connect_daemon, create_session, open_session};
use crate::daemon::protocol::{Event, Frame, RequestBody, ResponseData};
use chunks::{LineAssembler, is_progress_line};
use input::InputState;
use views::{ListCursor, RosterRow, help_lines, tree_line_id};

/// Inline viewport height: live line + bordered input (3) + status line.
const VIEWPORT_LINES: u16 = 5;

pub async fn run(session: Option<String>) -> Result<()> {
    let cwd = crate::cli::resolve_cwd(None)?;
    let (session_id, conn) = match &session {
        Some(id) => open_session(id, true).await?,
        None => create_session(&cwd, true).await?,
    };

    let mut app = App::new(session_id, conn);
    app.cwd_label = cwd.display().to_string();
    app.enter_terminal().map_err(|e| {
        let _ = crossterm::terminal::disable_raw_mode();
        anyhow::anyhow!(
            "{e:#}. This terminal cannot host the inline TUI — use `agentpit repl` \
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

/// What a fullscreen overlay round returned.
enum OverlayPick {
    None,
    /// Enter on an item.
    Enter(String),
    /// `f` on an item (Tree View's fork).
    Fork(String),
}

struct App {
    session_id: String,
    conn: Conn,
    terminal: Option<Terminal<CrosstermBackend<std::io::Stdout>>>,
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
    /// Shown in the header; the session's cwd as the worker reports it.
    cwd_label: String,
}

impl App {
    fn new(session_id: String, conn: Conn) -> App {
        App {
            session_id,
            conn,
            terminal: None,
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
        let backend = CrosstermBackend::new(std::io::stdout());
        let terminal = Terminal::with_options(
            backend,
            ratatui::TerminalOptions {
                viewport: ratatui::Viewport::Inline(VIEWPORT_LINES),
            },
        )?;
        self.terminal = Some(terminal);
        Ok(())
    }

    fn leave_terminal(&mut self) {
        self.terminal = None;
        let _ = crossterm::terminal::disable_raw_mode();
        println!();
    }

    /// Insert one finished transcript line above the viewport (scrollback-preserving).
    fn scroll_line(&mut self, line: Line<'static>) {
        let Some(terminal) = &mut self.terminal else {
            return;
        };
        let width = terminal.size().map(|s| s.width).unwrap_or(80).max(10) as usize;
        let text_len: usize = line.spans.iter().map(|s| s.content.chars().count()).sum();
        let height = text_len.max(1).div_ceil(width).min(20) as u16;
        let _ = terminal.insert_before(height, |buf| {
            Paragraph::new(line)
                .wrap(ratatui::widgets::Wrap { trim: false })
                .render(buf.area, buf);
        });
    }

    fn scroll_text(&mut self, text: &str, style: Style) {
        self.scroll_line(Line::from(Span::styled(text.to_string(), style)));
    }

    /// Attach + render the transcript tail into scrollback.
    async fn attach(&mut self) -> Result<()> {
        let tail = crate::config::load_config(None)
            .map(|l| l.config.session.transcript_tail)
            .unwrap_or(400);
        // The one-time identity header (prime renders its logo + meta exactly once).
        let short = crate::cli::guidance::short_id(&self.session_id).to_string();
        for line in theme::header_lines(env!("CARGO_PKG_VERSION"), &short, &self.cwd_label) {
            self.scroll_line(line);
        }
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
                self.scroll_line(line);
            }
            if total_entries > shown {
                self.scroll_text(
                    &format!("Showing latest {shown} of {total_entries} messages for faster open."),
                    theme::style_dim(),
                );
            }
        }
        let short = crate::cli::guidance::short_id(&self.session_id).to_string();
        self.scroll_text(
            &format!(
                "[attached to {short} — ? for keys; Ctrl-D detaches, the session keeps running]"
            ),
            theme::style_dim(),
        );
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
                            self.scroll_text(
                                &format!("connection lost ({e:#}) — reopen with `agentpit tui --session {short}`"),
                                theme::style_error(),
                            );
                            return Ok(());
                        }
                        Ok(frame) => { self.on_frame(frame); }
                    }
                }
                ev = events.next() => {
                    match ev {
                        Some(Ok(TermEvent::Key(key))) => {
                            if self.on_key(key, &mut events).await? {
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
                self.scroll_line(theme::turn_start_line(&backend));
                self.backend_line = backend;
            }
            Frame::Event(Event::Chunk { text }) => {
                self.busy = true;
                for line in self.assembler.push(&text) {
                    if is_progress_line(&line) {
                        self.scroll_line(theme::progress_line(&line));
                    } else {
                        self.scroll_text(&line, Style::default());
                    }
                }
            }
            Frame::Event(Event::TurnFinished { status }) => {
                if let Some(rest) = self.assembler.finish() {
                    self.scroll_text(&rest, Style::default());
                }
                if status != "ok" {
                    self.scroll_text(&format!("[turn ended: {status}]"), theme::style_notice());
                }
                self.busy = false;
                self.backend_line.clear();
            }
            Frame::Event(Event::Notice { text }) => {
                self.scroll_text(&format!("session: {text}"), theme::style_notice());
            }
            Frame::Response(resp) => {
                if Some(resp.id) == self.pending_send {
                    self.pending_send = None;
                    self.busy = false;
                    if !resp.ok {
                        self.scroll_text(
                            &format!("error: {}", resp.error.unwrap_or_default()),
                            theme::style_error(),
                        );
                    }
                }
            }
        }
    }

    /// Handle a key; `true` = exit the TUI.
    async fn on_key(&mut self, key: KeyEvent, events: &mut EventStream) -> Result<bool> {
        match (key.code, key.modifiers) {
            (KeyCode::Char('c'), KeyModifiers::CONTROL) => {
                if self.busy {
                    let _ = self.conn.send_request(RequestBody::Cancel).await;
                    self.scroll_text("[cancelling…]", Style::default().fg(Color::Yellow));
                } else if self
                    .last_ctrl_c
                    .map(|t| t.elapsed() <= Duration::from_secs(2))
                    .unwrap_or(false)
                {
                    return Ok(true);
                } else {
                    self.last_ctrl_c = Some(Instant::now());
                    self.scroll_text("(press Ctrl-C again within 2s to exit)", theme::style_dim());
                }
            }
            (KeyCode::Char('d'), KeyModifiers::CONTROL) if self.input.is_empty() => {
                return Ok(true);
            }
            (KeyCode::Char('?'), _) if self.input.is_empty() => {
                self.help_overlay(events).await?;
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
                let keys: Vec<String> = rows.iter().map(|r| r.session_id.clone()).collect();
                if let OverlayPick::Enter(session) = self
                    .overlay(
                        "Agents — Enter attaches, Esc returns",
                        &items,
                        &keys,
                        events,
                    )
                    .await?
                {
                    self.switch_session(session).await?;
                }
            }
            (KeyCode::Enter, _) => {
                let line = self.input.submit();
                if line.is_empty() {
                } else if line == "/tree" {
                    self.tree_overlay(events).await?;
                } else if matches!(line.as_str(), "/quit" | "/exit" | "/detach") {
                    return Ok(true);
                } else {
                    self.scroll_line(transcript_line("user", &line));
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

    fn draw(&mut self) -> Result<()> {
        let Some(terminal) = &mut self.terminal else {
            return Ok(());
        };
        let input_text = self.input.text().to_string();
        let cursor = self.input.cursor() as u16;
        let live = self.assembler.tail().to_string();
        let short = crate::cli::guidance::short_id(&self.session_id).to_string();
        let elapsed = self.turn_started.elapsed().as_secs();
        let status_line = if self.busy {
            theme::status_busy(
                self.tick,
                elapsed,
                &self.backend_line,
                theme::current_hint(elapsed),
            )
        } else {
            theme::status_idle(&short)
        };
        // The input box dims while a turn runs (typing stays allowed; the border is the
        // state signal — see theme.rs "Input box" states).
        let border_style = if self.busy {
            Style::default().fg(theme::BORDER_MUTED)
        } else {
            Style::default().fg(theme::BLUE)
        };
        terminal.draw(|f| {
            let [live_area, box_area, status_area] = Layout::vertical([
                Constraint::Length(1),
                Constraint::Length(3),
                Constraint::Length(1),
            ])
            .areas(f.area());
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
            f.set_cursor_position((inner.x + 2 + cursor, inner.y));
        })?;
        Ok(())
    }

    /// Detach from the current worker, ensure + attach the chosen session (§11.3).
    async fn switch_session(&mut self, session: String) -> Result<()> {
        let _ = self.conn.request(RequestBody::Detach).await;
        let (session_id, conn) = open_session(&session, true).await?;
        self.session_id = session_id;
        self.conn = conn;
        self.scroll_text("── switched session ──", theme::style_accent());
        self.attach().await
    }

    async fn tree_overlay(&mut self, events: &mut EventStream) -> Result<()> {
        let lines = match self.conn.request(RequestBody::Tree).await {
            Ok(ResponseData::Lines { lines }) => lines,
            _ => return Ok(()),
        };
        let keys: Vec<String> = lines
            .iter()
            .map(|l| tree_line_id(l).unwrap_or("").to_string())
            .collect();
        let items: Vec<Line<'static>> = lines.iter().map(|l| Line::raw(l.clone())).collect();
        match self
            .overlay(
                "Tree — Enter moves the leaf, f forks at the cursor, Esc returns",
                &items,
                &keys,
                events,
            )
            .await?
        {
            OverlayPick::Enter(target) if !target.is_empty() => {
                match self
                    .conn
                    .request(RequestBody::Branch {
                        target: target.clone(),
                        summary: None,
                    })
                    .await
                {
                    Ok(_) => self.scroll_text(
                        &format!("moved to {target} — the next turn continues from there"),
                        theme::style_accent(),
                    ),
                    Err(e) => self.scroll_text(&format!("error: {e:#}"), theme::style_error()),
                }
            }
            OverlayPick::Fork(at) if !at.is_empty() => {
                match self.conn.request(RequestBody::Fork { at: Some(at) }).await {
                    Ok(ResponseData::Forked { session_id }) => {
                        let tail = crate::cli::guidance::short_id(&session_id).to_string();
                        self.scroll_text(
                            &format!("forked — open it with `agentpit tui --session {tail}`"),
                            theme::style_accent(),
                        );
                    }
                    Ok(_) => {}
                    Err(e) => self.scroll_text(&format!("error: {e:#}"), theme::style_error()),
                }
            }
            _ => {}
        }
        Ok(())
    }

    async fn help_overlay(&mut self, events: &mut EventStream) -> Result<()> {
        let lines = help_lines();
        let keys = vec![String::new(); lines.len()];
        let items: Vec<Line<'static>> = lines.iter().map(|l| Line::raw(l.clone())).collect();
        let _ = self
            .overlay("Keys — Esc returns", &items, &keys, events)
            .await?;
        Ok(())
    }

    /// One fullscreen list overlay on the alternate screen (§11.2). `keys[i]` is the
    /// value Enter/f return for row i.
    async fn overlay(
        &mut self,
        title: &str,
        items: &[Line<'static>],
        keys: &[String],
        events: &mut EventStream,
    ) -> Result<OverlayPick> {
        crossterm::execute!(std::io::stdout(), crossterm::terminal::EnterAlternateScreen)?;
        let backend = CrosstermBackend::new(std::io::stdout());
        let mut term = Terminal::new(backend)?;
        let mut cursor = ListCursor::default();
        let mut pick = OverlayPick::None;
        loop {
            cursor.clamp(items.len());
            let mut state = ListState::default();
            state.select((!items.is_empty()).then_some(cursor.index));
            term.draw(|f| {
                let list = List::new(items.iter().map(|l| ListItem::new(l.clone())))
                    .block(
                        Block::default()
                            .borders(Borders::ALL)
                            .border_type(BorderType::Rounded)
                            .border_style(Style::default().fg(theme::BORDER_MUTED))
                            .title(Span::styled(title.to_string(), theme::style_accent())),
                    )
                    .highlight_style(theme::style_selected())
                    .highlight_symbol("› ");
                f.render_stateful_widget(list, f.area(), &mut state);
            })?;
            match events.next().await {
                Some(Ok(TermEvent::Key(key))) => match key.code {
                    KeyCode::Esc | KeyCode::Char('q') => break,
                    KeyCode::Up => cursor.up(),
                    KeyCode::Down => cursor.down(items.len()),
                    KeyCode::Enter => {
                        if let Some(k) = keys.get(cursor.index) {
                            pick = OverlayPick::Enter(k.clone());
                        }
                        break;
                    }
                    KeyCode::Char('f') => {
                        if let Some(k) = keys.get(cursor.index) {
                            pick = OverlayPick::Fork(k.clone());
                        }
                        break;
                    }
                    _ => {}
                },
                Some(Ok(_)) => {}
                Some(Err(_)) | None => break,
            }
        }
        drop(term);
        crossterm::execute!(std::io::stdout(), crossterm::terminal::LeaveAlternateScreen)?;
        // Force the inline viewport to repaint cleanly after the excursion.
        if let Some(t) = &mut self.terminal {
            let _ = t.clear();
        }
        Ok(pick)
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
                theme::style_muted().add_modifier(Modifier::BOLD),
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
