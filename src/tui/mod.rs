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
mod completion;
mod input;
mod markdown;
mod scroll;
mod slash;
pub mod theme;
mod views;

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::event::{
    DisableMouseCapture, EnableMouseCapture, Event as TermEvent, EventStream, KeyCode, KeyEvent,
    KeyModifiers, MouseEvent, MouseEventKind,
};
use futures::StreamExt;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, List, ListItem, ListState, Paragraph};

use crate::daemon::client::{Conn, connect_daemon, create_session, open_session};
use crate::daemon::protocol::{Event, Frame, RequestBody, ResponseData};
use crate::types::BackendId;
use chunks::{LineAssembler, is_progress_line};
use completion::{Edit, FileMenu, SlashMenu};
use input::InputState;
use markdown::MdRenderer;
use scroll::{clamp_offset, tail_window};
use slash::{Local, Protocol, Route, Suspend, route};
use views::{ListCursor, RosterRow, help_lines, tree_line_id};

/// Transcript memory cap: beyond this many lines the oldest are dropped (the session
/// JSONL remains the durable history; this is only the on-screen buffer).
const TRANSCRIPT_CAP: usize = 5000;

pub async fn run(session: Option<String>) -> Result<()> {
    let cwd = crate::cli::resolve_cwd(None)?;
    // Fill the slash registry's second layer before the popup or the router reads it: the
    // project scope is relative to this cwd. Both sources read files and nothing else — no
    // MCP server is started by opening the TUI.
    crate::cli::slash::install(crate::cli::runtime_slash_entries(&cwd));
    let (session_id, conn) = match &session {
        Some(id) => open_session(id, true).await?,
        None => create_session(&cwd, true).await?,
    };

    let picker_cwd = if session.is_some() {
        session_cwd(&session_id)
            .await
            .unwrap_or_else(|| cwd.clone())
    } else {
        cwd.clone()
    };
    let mut app = App::new(session_id, conn, &picker_cwd);
    app.cwd_label = picker_cwd.display().to_string();
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
    crossterm::execute!(
        std::io::stdout(),
        DisableMouseCapture,
        crossterm::terminal::LeaveAlternateScreen
    )?;
    crossterm::terminal::disable_raw_mode()
}

/// Hold the normal screen until the user has read a suspended subcommand's output.
///
/// Raw mode is enabled only for the length of the read, and disabled again on every path
/// — including a read error — so a terminal that dies here is left cooked, not raw. If raw
/// mode cannot be entered at all (no tty behind the suspend), fall back to a line read so
/// the pause still exists instead of flashing past.
fn wait_for_key() {
    eprint!("[press any key to return to the TUI]");
    let _ = std::io::stderr().flush();
    tokio::task::block_in_place(|| {
        if crossterm::terminal::enable_raw_mode().is_ok() {
            loop {
                match crossterm::event::read() {
                    Ok(TermEvent::Key(_)) | Err(_) => break,
                    Ok(_) => continue,
                }
            }
            let _ = crossterm::terminal::disable_raw_mode();
        } else {
            let mut line = String::new();
            let _ = std::io::stdin().read_line(&mut line);
        }
    });
    eprintln!();
}

/// Which surface owns the keys right now.
enum Mode {
    Chat,
    Overlay(Overlay),
}

/// What the main loop must do after a key was handled.
enum KeyOutcome {
    /// Keep looping.
    Continue,
    /// Leave the TUI (Ctrl-D, /quit, /exit, /detach).
    Exit,
    /// Hand the terminal back and run a CLI subcommand (D2 SUSPEND). Performed by the
    /// main loop, not here: it owns the crossterm `EventStream`, whose reader thread must
    /// let go of stdin before an interactive subcommand can read it.
    Suspend(Suspend),
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
    /// The slash-command dropdown (D4) — pure state; this loop only feeds it keys and
    /// draws it above the input line.
    menu: SlashMenu,
    /// Project paths offered for the `@` token at the input cursor.
    file_menu: FileMenu,
    assembler: LineAssembler,
    /// Styles the backend's answer lines (headings, code fences, emphasis) as they land.
    md: MdRenderer,
    busy: bool,
    turn_started: Instant,
    last_ctrl_c: Option<Instant>,
    /// Request id of OUR in-flight send (its response ends the busy state).
    pending_send: Option<u64>,
    backend_line: String,
    /// Backend of the most recent turn — what `/login` with no argument means here.
    active_backend: Option<String>,
    /// 100ms app-timer count driving the working pulse.
    tick: usize,
    /// Shown in the header; the session's cwd.
    cwd_label: String,
}

impl App {
    fn new(session_id: String, conn: Conn, cwd: &Path) -> App {
        App {
            session_id,
            conn,
            terminal: None,
            mode: Mode::Chat,
            transcript: Vec::new(),
            scroll_offset: 0,
            input: InputState::default(),
            menu: SlashMenu::default(),
            file_menu: FileMenu::from_cwd(cwd),
            assembler: LineAssembler::default(),
            md: MdRenderer::default(),
            busy: false,
            turn_started: Instant::now(),
            last_ctrl_c: None,
            pending_send: None,
            backend_line: String::new(),
            active_backend: None,
            tick: 0,
            cwd_label: String::new(),
        }
    }

    fn enter_terminal(&mut self) -> Result<()> {
        crossterm::terminal::enable_raw_mode()?;
        // Mouse capture is what makes the wheel scroll the transcript instead of the
        // terminal's (empty) alternate-screen scrollback. Text selection still works via
        // the terminal's usual override (Shift/Option-drag).
        crossterm::execute!(
            std::io::stdout(),
            crossterm::terminal::EnterAlternateScreen,
            EnableMouseCapture
        )?;
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
            // One renderer for the whole snapshot so a fence stays open across its lines;
            // the live turns' renderer (`self.md`) is untouched.
            let mut md = MdRenderer::default();
            let lines: Vec<Line<'static>> = transcript
                .iter()
                .map(|(who, text)| transcript_line(who, text, &mut md))
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
        // `Option` so the stream can be DROPPED for the duration of a suspended
        // subcommand: crossterm's EventStream parks a thread inside `poll_internal`,
        // which consumes stdin into its own buffer, so a live stream would swallow every
        // keystroke an interactive subcommand (e.g. `/login`) is waiting for. Dropping it
        // wakes that thread and hands stdin back; a fresh stream is built on return.
        let mut events = Some(EventStream::new());
        let mut tick = tokio::time::interval(Duration::from_millis(100));
        loop {
            self.draw()?;
            let mut suspend: Option<Suspend> = None;
            let stream = events
                .as_mut()
                .expect("event stream is restored before the next iteration");
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
                ev = stream.next() => {
                    match ev {
                        Some(Ok(TermEvent::Key(key))) => {
                            match self.on_key(key).await? {
                                KeyOutcome::Continue => {}
                                KeyOutcome::Exit => return Ok(()),
                                KeyOutcome::Suspend(cmd) => suspend = Some(cmd),
                            }
                        }
                        // Re-clamp the scroll offset on resize (M7): a wider terminal
                        // rewraps to fewer rows, so a stale large offset would compute an
                        // empty window and blank the transcript.
                        Some(Ok(TermEvent::Resize(w, h))) => {
                            let height = (h.saturating_sub(5)) as usize;
                            self.scroll_offset = scroll::clamp_offset(
                                &self.transcript,
                                w as usize,
                                height,
                                self.scroll_offset,
                            );
                        }
                        Some(Ok(TermEvent::Mouse(m))) => self.on_mouse(m),
                        Some(Ok(_)) => {}
                        Some(Err(_)) | None => return Ok(()),
                    }
                }
                _ = tick.tick() => {
                    self.tick = self.tick.wrapping_add(1);
                }
            }
            if let Some(cmd) = suspend {
                drop(events.take()); // hands stdin back to the subcommand
                if !self.suspend_and_run(cmd).await {
                    return Ok(()); // terminal lost — do not start another reader on it
                }
                events = Some(EventStream::new());
            }
        }
    }

    /// Run a CLI subcommand with the real terminal handed back to it (D2 SUSPEND), then
    /// come back to the conversation. `false` = the alternate screen could not be
    /// re-entered, so the caller must stop instead of drawing into a dead terminal.
    ///
    /// Order matters for safety: the screen is restored to its normal, cooked-mode state
    /// FIRST and re-entered LAST, so a subcommand that panics or a terminal that dies
    /// mid-command leaves the user in a usable shell rather than in raw mode.
    async fn suspend_and_run(&mut self, cmd: Suspend) -> bool {
        let label = cmd.label();
        self.leave_terminal();
        eprintln!("── {label} ──");
        if let Err(e) = self.run_suspended(&cmd).await {
            eprintln!("error: {e:#}");
        }
        wait_for_key();
        match self.enter_terminal() {
            Ok(()) => {
                // The transcript and `scroll_offset` are untouched by the round trip, so
                // the redraw comes back to exactly the rows the reader was looking at.
                self.push_text(
                    &format!("[{label} ran on the normal screen]"),
                    theme::style_dim(),
                );
                true
            }
            Err(e) => {
                eprintln!(
                    "cannot return to the TUI ({e:#}) — reopen with `agentpit tui --session {}`",
                    crate::cli::guidance::short_id(&self.session_id)
                );
                false
            }
        }
    }

    /// The CLI implementation behind a suspended command — the same code the subcommand
    /// runs, so the TUI cannot drift from `agentpit status` / `agentpit login`.
    async fn run_suspended(&self, cmd: &Suspend) -> Result<()> {
        match cmd {
            Suspend::Config => crate::cli::menu::run_config().await,
            Suspend::Status => crate::cli::status::run(None).await,
            Suspend::Login(requested) => {
                let backend = self.login_target(requested.as_deref())?;
                crate::cli::login::run(backend, false).await
            }
            Suspend::Learning => crate::cli::learning::run(false),
            Suspend::Arena(words) => crate::cli::arena::run_words(words.clone()).await,
            Suspend::Profile(words) => crate::cli::profile::run_words(words.clone()).await,
            #[cfg(feature = "similarity")]
            Suspend::Similarity(words) => {
                crate::cli::similarity_cmd::run_words(words.clone()).await
            }
            Suspend::Outcome { verdict, run_id } => {
                crate::cli::outcome::run(verdict.clone(), run_id.clone()).await
            }
            Suspend::Doctor { fix } => crate::cli::doctor::run(*fix).await,
            Suspend::Diagnose(task) => crate::cli::diagnose::run(task.clone(), false).await,
            Suspend::Sessions(words) => crate::cli::sessions::run_words(words.clone()).await,
            Suspend::Mcp(words) => crate::cli::mcp_cmd::run_words(words.clone()).await,
        }
    }

    /// Which backend `/login` means: the one named, else the one that ran the last turn,
    /// else the configured default.
    fn login_target(&self, requested: Option<&str>) -> Result<BackendId> {
        if let Some(id) = requested {
            return id.parse::<BackendId>().map_err(|e| {
                anyhow::anyhow!(
                    "{e}. Valid backends: {}",
                    BackendId::ALL
                        .iter()
                        .map(|b| b.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            });
        }
        if let Some(active) = self.active_backend.as_deref()
            && let Ok(id) = active.parse::<BackendId>()
        {
            return Ok(id);
        }
        Ok(crate::config::load_config(None)?.config.default.backend)
    }

    fn on_frame(&mut self, frame: Frame) {
        match frame {
            Frame::Event(Event::TurnStarted { backend }) => {
                self.busy = true;
                self.md.reset(); // a new turn is a new document — no fence carries over
                self.turn_started = Instant::now();
                self.push_line(theme::turn_start_line(&backend));
                self.active_backend = Some(backend.clone());
                self.backend_line = backend;
            }
            Frame::Event(Event::Chunk { text }) => {
                self.busy = true;
                for line in self.assembler.push(&text) {
                    if is_progress_line(&line) {
                        self.push_line(theme::progress_line(&line));
                    } else {
                        let rendered = self.md.render(&line);
                        self.push_line(rendered);
                    }
                }
            }
            Frame::Event(Event::TurnFinished { status }) => {
                if let Some(rest) = self.assembler.finish() {
                    let rendered = self.md.render(&rest);
                    self.push_line(rendered);
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

    /// Send a request and wait for its response, routing EVERY other frame that arrives
    /// meanwhile through `on_frame` (H2). Using `Conn::request` here instead would discard
    /// the in-flight turn's Chunk/TurnFinished events and even the pending send's own
    /// Response — leaving `busy` stuck true and the spinner running forever.
    async fn request_data(&mut self, body: RequestBody) -> Result<ResponseData> {
        let want = self.conn.send_request(body).await?;
        loop {
            let frame = self.conn.recv_frame().await?;
            match frame {
                Frame::Response(resp) if resp.id == want => {
                    return if resp.ok {
                        Ok(resp.data.unwrap_or(ResponseData::Unit))
                    } else {
                        Err(anyhow::anyhow!(resp.error.unwrap_or_default()))
                    };
                }
                // Turn events and the pending send's Response keep flowing to on_frame,
                // so busy state and the transcript stay correct during the round-trip.
                other => self.on_frame(other),
            }
        }
    }

    /// Handle a key, telling the main loop what to do next.
    async fn on_key(&mut self, key: KeyEvent) -> Result<KeyOutcome> {
        if let Mode::Overlay(_) = self.mode {
            self.on_overlay_key(key).await?;
            return Ok(KeyOutcome::Continue);
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
                    return Ok(KeyOutcome::Exit);
                } else {
                    self.last_ctrl_c = Some(Instant::now());
                    self.push_text("(press Ctrl-C again within 2s to exit)", theme::style_dim());
                }
            }
            (KeyCode::Char('d'), KeyModifiers::CONTROL) if self.input.is_empty() => {
                return Ok(KeyOutcome::Exit);
            }
            (KeyCode::PageUp, _) => self.scroll_by(1),
            (KeyCode::PageDown, _) => self.scroll_by(-1),
            (KeyCode::End, _) if self.input.is_empty() => self.scroll_offset = 0,
            (KeyCode::Char('?'), _) if self.input.is_empty() => self.open_help(),
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
            // Everything else belongs to the editor: the input line and the slash menu
            // that shares its keys. `completion::handle_key` owns that split (D4) — this
            // loop only learns whether a line is ready to route.
            _ => {
                if completion::handle_key_with_files(
                    &mut self.input,
                    &mut self.menu,
                    &mut self.file_menu,
                    key,
                ) == Edit::Submit
                {
                    // Registry-driven (D2): only `Route::FreeText` may become a Send, so
                    // an unknown /word is refused in-screen instead of billing a turn.
                    match route(&self.input.submit()) {
                        Route::Ignore => {}
                        Route::Local(Local::Exit) => return Ok(KeyOutcome::Exit),
                        Route::Local(Local::Help) => self.open_help(),
                        Route::Protocol(cmd) => self.run_protocol(cmd).await?,
                        Route::Suspend(cmd) => return Ok(KeyOutcome::Suspend(cmd)),
                        Route::Unknown(message) => self.notify(&message, theme::style_notice()),
                        Route::FreeText(text) => match split_backend_modifier(&text) {
                            Ok((_, task)) if task.is_empty() => self.notify(
                                "A backend override needs task text after it.",
                                theme::style_notice(),
                            ),
                            Ok((backend, task)) => self.send_turn(text, task, backend).await?,
                            Err(message) => self.notify(&message, theme::style_notice()),
                        },
                        // A skill's turn is sent exactly like a typed one; only what the
                        // transcript shows differs — the provenance line and the command,
                        // rather than the composed body. The note goes in before the send
                        // so the size and the source file are on screen by the time the
                        // turn starts, not after it.
                        Route::Compose {
                            provenance,
                            label,
                            text,
                        } => {
                            self.push_text(&provenance, theme::style_dim());
                            self.send_turn(label, text, None).await?;
                        }
                        // An MCP prompt is the same turn one step later: its body is on the
                        // server, so it is fetched here and only then sent. A server that
                        // cannot be reached prints its refusal into the transcript and sends
                        // nothing — a failed fetch is never a turn.
                        Route::McpPrompt(invocation) => {
                            match crate::mcp::prompts::invoke(&invocation).await {
                                Ok(composed) => {
                                    self.push_text(&composed.provenance, theme::style_dim());
                                    self.send_turn(
                                        format!("/{}", invocation.name),
                                        composed.turn,
                                        None,
                                    )
                                    .await?;
                                }
                                Err(e) => {
                                    self.notify(&format!("error: {e:#}"), theme::style_error())
                                }
                            }
                        }
                    }
                }
            }
        }
        Ok(KeyOutcome::Continue)
    }

    /// Start one turn: echo `shown` into the transcript and send `text` to the worker.
    ///
    /// The two are the same string for a line the user typed, and differ for a composed
    /// turn (a skill), where `text` is the whole skill body.
    async fn send_turn(
        &mut self,
        shown: String,
        text: String,
        backend: Option<String>,
    ) -> Result<()> {
        self.push_line(theme::user_line(&shown));
        self.scroll_offset = 0; // sending re-follows the bottom
        let id = self
            .conn
            .send_request(RequestBody::Send { text, backend })
            .await?;
        self.pending_send = Some(id);
        self.busy = true;
        self.turn_started = Instant::now();
        Ok(())
    }

    /// Print a result line and re-follow the bottom, so the reader sees it even when
    /// scrolled up.
    fn notify(&mut self, text: &str, style: Style) {
        self.push_text(text, style);
        self.scroll_offset = 0;
    }

    /// The `?` / `/help` overlay: keys and the commands this screen serves.
    fn open_help(&mut self) {
        let lines = help_lines();
        self.mode = Mode::Overlay(Overlay {
            kind: OverlayKind::Help,
            title: "Keys & commands — Esc returns",
            keys: vec![String::new(); lines.len()],
            items: lines.into_iter().map(Line::raw).collect(),
            cursor: ListCursor::default(),
        });
    }

    /// Run a worker-served command and render its result in-screen.
    async fn run_protocol(&mut self, cmd: Protocol) -> Result<()> {
        let request = cmd.request();
        match cmd {
            Protocol::Tree => {
                let lines = match self.request_data(request).await {
                    Ok(ResponseData::Lines { lines }) => lines,
                    _ => return Ok(()),
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
            }
            Protocol::Branch(target) => match self.request_data(request).await {
                Ok(_) => self.notify(
                    &format!("moved to {target} — the next turn continues from there"),
                    theme::style_accent(),
                ),
                Err(e) => self.notify(&format!("error: {e:#}"), theme::style_error()),
            },
            Protocol::Fork(_) => match self.request_data(request).await {
                Ok(ResponseData::Forked { session_id }) => {
                    let tail = crate::cli::guidance::short_id(&session_id).to_string();
                    self.notify(
                        &format!("forked — open it with `agentpit tui --session {tail}`"),
                        theme::style_accent(),
                    );
                }
                Ok(_) => {}
                Err(e) => self.notify(&format!("error: {e:#}"), theme::style_error()),
            },
            Protocol::Compact => match self.request_data(request).await {
                Ok(_) => self.notify(
                    "compacted — future turns replay from the summary",
                    theme::style_accent(),
                ),
                Err(e) => self.notify(&format!("error: {e:#}"), theme::style_error()),
            },
        }
        Ok(())
    }

    /// Scroll by half a screen; +1 = up (older), -1 = down (newer).
    fn scroll_by(&mut self, direction: i32) {
        let (_, height) = self.viewport();
        self.scroll_rows(direction, (height / 2).max(1));
    }

    /// Scroll by `step` wrapped rows; +1 = up (older), -1 = down (newer).
    fn scroll_rows(&mut self, direction: i32, step: usize) {
        let (width, height) = self.viewport();
        let wanted = if direction > 0 {
            self.scroll_offset + step
        } else {
            self.scroll_offset.saturating_sub(step)
        };
        self.scroll_offset = clamp_offset(&self.transcript, width, height, wanted);
    }

    /// The transcript area's (width, height) in cells.
    fn viewport(&self) -> (usize, usize) {
        self.terminal
            .as_ref()
            .and_then(|t| t.size().ok())
            .map(|s| (s.width as usize, s.height.saturating_sub(5) as usize))
            .unwrap_or((80, 20))
    }

    /// The wheel scrolls whatever surface is up: the transcript, or an overlay's cursor.
    fn on_mouse(&mut self, m: MouseEvent) {
        const WHEEL_STEP: usize = 3;
        match (m.kind, &mut self.mode) {
            (MouseEventKind::ScrollUp, Mode::Overlay(o)) => o.cursor.up(),
            (MouseEventKind::ScrollDown, Mode::Overlay(o)) => {
                let len = o.items.len();
                o.cursor.down(len);
            }
            (MouseEventKind::ScrollUp, Mode::Chat) => self.scroll_rows(1, WHEEL_STEP),
            (MouseEventKind::ScrollDown, Mode::Chat) => self.scroll_rows(-1, WHEEL_STEP),
            _ => {}
        }
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
                    // Same command as `/branch <id>`, same rendering — one path.
                    (OverlayKind::Tree, Some(target)) if !target.is_empty() => {
                        self.run_protocol(Protocol::Branch(target)).await?;
                    }
                    _ => {}
                }
            }
            KeyCode::Char('f') if overlay.kind == OverlayKind::Tree => {
                let picked = overlay.keys.get(overlay.cursor.index).cloned();
                self.mode = Mode::Chat;
                if let Some(at) = picked.filter(|a| !a.is_empty()) {
                    // Same command as `/fork <id>`, same rendering — one path.
                    self.run_protocol(Protocol::Fork(Some(at))).await?;
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
        // Cursor x in display CELLS, not char count (M8): a CJK char before the cursor
        // occupies two columns, so a char-index cursor lands left of the true position.
        let cursor: u16 = self
            .input
            .text()
            .chars()
            .take(self.input.cursor())
            .map(|c| unicode_width::UnicodeWidthChar::width(c).unwrap_or(0) as u16)
            .sum();
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
        let menu = &self.menu;
        let file_menu = &self.file_menu;
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
                // Both completion popups share the same anchored geometry. The file picker
                // has priority while an `@token` is active; otherwise `/` shows commands.
                if file_menu.is_open() {
                    if let Some(area) = completion::popup_area(box_area, file_menu.matches().len())
                    {
                        f.render_widget(Clear, area);
                        let mut state = ListState::default();
                        state.select(Some(file_menu.index()));
                        let rows = file_menu.matches().iter().map(|path| {
                            ListItem::new(theme::menu_row(&format!("@{path}"), "project file"))
                        });
                        let list = List::new(rows)
                            .block(
                                Block::default()
                                    .borders(Borders::ALL)
                                    .border_type(BorderType::Rounded)
                                    .border_style(Style::default().fg(theme::BORDER_MUTED))
                                    .title(Span::styled(
                                        " files — Tab/Enter selects · Esc dismisses ",
                                        theme::style_dim(),
                                    )),
                            )
                            .highlight_style(theme::style_selected())
                            .highlight_symbol("› ");
                        f.render_stateful_widget(list, area, &mut state);
                    }
                } else if let Some(area) = completion::popup_area(box_area, menu.matches().len()) {
                    f.render_widget(Clear, area);
                    let mut state = ListState::default();
                    state.select(Some(menu.index()));
                    let rows = menu
                        .matches()
                        .iter()
                        .map(|c| ListItem::new(theme::menu_row(&c.label, c.description)));
                    let list = List::new(rows)
                        .block(
                            Block::default()
                                .borders(Borders::ALL)
                                .border_type(BorderType::Rounded)
                                .border_style(Style::default().fg(theme::BORDER_MUTED))
                                .title(Span::styled(
                                    " commands — Tab completes · Esc dismisses ",
                                    theme::style_dim(),
                                )),
                        )
                        .highlight_style(theme::style_selected())
                        .highlight_symbol("› ");
                    f.render_stateful_widget(list, area, &mut state);
                }
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
        if let Some(cwd) = session_cwd(&self.session_id).await {
            self.cwd_label = cwd.display().to_string();
            self.file_menu = FileMenu::from_cwd(&cwd);
        } else {
            // Never keep offering paths from the previously attached session.
            self.file_menu = FileMenu::default();
        }
        self.transcript.clear();
        self.scroll_offset = 0;
        self.attach().await
    }
}

/// Split a leading, known `!backend` from an ordinary TUI turn.
///
/// `@` remains exclusively available to the project-file picker and is never interpreted
/// here. Email addresses and mentions later in the line also remain untouched.
fn split_backend_modifier(text: &str) -> Result<(Option<String>, String), String> {
    let trimmed = text.trim();
    let Some(rest) = trimmed.strip_prefix('!') else {
        return Ok((None, trimmed.to_string()));
    };
    let (name, task) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
    match name.parse::<BackendId>() {
        Ok(backend) => Ok((Some(backend.to_string()), task.trim().to_string())),
        Err(error) => Err(format!("Unknown backend !{name}: {error}")),
    }
}

/// One transcript line, styled by speaker via the theme (§11.3). Backend lines pass
/// through the markdown renderer, same as when they streamed in live.
fn transcript_line(who: &str, text: &str, md: &mut MdRenderer) -> Line<'static> {
    match who {
        "user" => {
            md.reset(); // a user turn ends the previous answer's document
            theme::user_line(text)
        }
        "summary" => Line::from(vec![
            Span::styled("── summary ── ".to_string(), theme::style_accent()),
            Span::styled(text.to_string(), theme::style_muted()),
        ]),
        backend => {
            let mut line = md.render(text);
            line.spans.insert(
                0,
                Span::styled(
                    format!("{backend}  "),
                    theme::style_muted().add_modifier(ratatui::style::Modifier::BOLD),
                ),
            );
            line.spans
                .insert(0, Span::styled("◈ ".to_string(), theme::style_accent()));
            line
        }
    }
}

async fn session_cwd(session_id: &str) -> Option<PathBuf> {
    fetch_roster()
        .await
        .into_iter()
        .find(|row| row.session_id == session_id)
        .map(|row| PathBuf::from(row.cwd))
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

#[cfg(test)]
mod tests {
    use super::split_backend_modifier;

    #[test]
    fn known_leading_bang_backend_is_sent_out_of_band() {
        assert_eq!(
            split_backend_modifier("  !codex  review @src/lib.rs  "),
            Ok((Some("codex".to_string()), "review @src/lib.rs".to_string()))
        );
        assert_eq!(
            split_backend_modifier("!claude"),
            Ok((Some("claude".to_string()), String::new()))
        );
        assert!(split_backend_modifier("!unknown task").is_err());
    }

    #[test]
    fn file_mentions_and_email_are_not_backend_modifiers() {
        assert_eq!(
            split_backend_modifier("@src/lib.rs explain this"),
            Ok((None, "@src/lib.rs explain this".to_string()))
        );
        assert_eq!(
            split_backend_modifier("email me@example.com"),
            Ok((None, "email me@example.com".to_string()))
        );
    }
}
