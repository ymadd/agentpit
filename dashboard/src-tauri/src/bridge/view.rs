//! One open loop: fold its journal with the shared code and keep the webview's
//! `LoopView` current.
//!
//! The first view comes from disk, at once. After that the task attaches to the loop's
//! runner when one is live (or should be: a runner that died mid-run is restarted, which is
//! recovery) and applies each streamed record, resuming from its cursor after a
//! reconnect. A loop with no runner (parked, finished, read-only) is followed by polling
//! the journal file, so looking at it never wakes it. Whenever a runner connection ends,
//! the journal on disk is re-read: it is the truth, and the runner may have written its
//! last records (`writer_closed`) after the connection stopped being read.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agentpit_events::loops::{
    decode_line, loop_dir_in, loop_view, read_loop, Cursor, LoopState, LoopStatus, LoopView,
};
use agentpit_events::wire::{Event, Frame, RequestBody, ResponseData};
use serde::Serialize;
use tokio::sync::mpsc;
use tokio::time::Instant;

use super::conn::{response_data, CONTROL_TIMEOUT};
use super::{lock, Bridge, BridgeError};

/// At most one `loops:view` per loop per this long (the first change after a quiet spell
/// goes out at once, so a node transition is never held back by more than this).
const VIEW_EVERY: Duration = Duration::from_millis(40);
/// Live output is batched per this long.
const CHUNK_EVERY: Duration = Duration::from_millis(50);
/// Output buffered between flushes beyond this is dropped (the webview fills gaps with
/// `step_output`).
const CHUNK_BUFFER_MAX: usize = 512 * 1024;
/// How often a loop without a runner has its journal checked for growth.
const DISK_POLL: Duration = Duration::from_millis(500);
const RETRY_MIN: Duration = Duration::from_millis(250);
const RETRY_MAX: Duration = Duration::from_secs(5);
/// A runner connection shorter than this counts as a failed attempt (backoff applies).
const STABLE_CONNECTION: Duration = Duration::from_secs(2);

pub(super) struct ViewEntry {
    kick: mpsc::UnboundedSender<Kick>,
    task: tokio::task::JoinHandle<()>,
    pub(super) state: Arc<Mutex<LoopState>>,
}

impl Drop for ViewEntry {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[derive(Debug)]
pub(super) enum Kick {
    /// Something about the loop may have changed (its board row, an operation).
    Recheck,
    /// An operation just ensured this runner: attach to it.
    Runner(PathBuf),
}

#[derive(Serialize)]
struct ViewPayload<'a> {
    loop_id: &'a str,
    view: LoopView,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ChunkOut {
    pub step_id: String,
    pub offset: u64,
    pub text: String,
}

#[derive(Serialize)]
struct ChunksPayload<'a> {
    loop_id: &'a str,
    chunks: Vec<ChunkOut>,
}

/// Should a runner be running this loop right now? (Running and not only waiting for a
/// person, or draining a stop.)
fn wants_runner(state: &LoopState) -> bool {
    match state.status {
        LoopStatus::Running => !state.is_waiting(),
        LoopStatus::Stopping => true,
        _ => false,
    }
}

/// What the task knows about the journal file since it last read it.
#[derive(Debug, Clone, Copy, Default)]
pub(super) struct DiskMark {
    /// File length when last read.
    len: u64,
    /// The last record is `writer_closed`: its runner exited on purpose (parked, stopped,
    /// finished), so a stale "live" row must not bring it back.
    closed: bool,
}

impl Bridge {
    /// Open (or re-open) a loop view: returns the view now and keeps it current through
    /// `loops:view` / `loops:chunks` until [`Bridge::close_loop`].
    pub async fn open_loop(self: &Arc<Self>, loop_id: &str) -> Result<LoopView, BridgeError> {
        let dir = loop_dir_in(&self.paths.loops_root, loop_id)
            .ok_or_else(|| BridgeError::bad_request(format!("{loop_id:?} is not a loop id")))?;
        let existing = {
            let views = lock(&self.views);
            views
                .get(loop_id)
                .filter(|v| !v.task.is_finished())
                .map(|v| {
                    let _ = v.kick.send(Kick::Recheck);
                    Arc::clone(&v.state)
                })
        };
        if let Some(state) = existing {
            return project(&lock(&state));
        }
        let (state, mark) = read_disk(&dir).await?;
        let view = project(&state)?;
        let shared = Arc::new(Mutex::new(state));
        let (tx, rx) = mpsc::unbounded_channel();
        let task = tokio::spawn(run_view(
            Arc::clone(self),
            loop_id.to_string(),
            dir,
            Arc::clone(&shared),
            mark,
            rx,
        ));
        // A concurrent open of the same loop may have won: the replaced entry's task is
        // aborted when it drops.
        lock(&self.views).insert(
            loop_id.to_string(),
            ViewEntry {
                kick: tx,
                task,
                state: shared,
            },
        );
        Ok(view)
    }

    pub fn close_loop(&self, loop_id: &str) {
        let removed = lock(&self.views).remove(loop_id);
        drop(removed);
    }

    /// The loop's board row changed, or an operation touched it.
    pub(super) fn kick_view(&self, loop_id: &str) {
        if let Some(v) = lock(&self.views).get(loop_id) {
            let _ = v.kick.send(Kick::Recheck);
        }
    }

    pub(super) fn kick_view_runner(&self, loop_id: &str, socket: PathBuf) {
        if let Some(v) = lock(&self.views).get(loop_id) {
            let _ = v.kick.send(Kick::Runner(socket));
        }
    }

    /// The folded state of an open loop.
    pub(super) fn open_state(&self, loop_id: &str) -> Option<Arc<Mutex<LoopState>>> {
        lock(&self.views).get(loop_id).map(|v| Arc::clone(&v.state))
    }
}

fn project(state: &LoopState) -> Result<LoopView, BridgeError> {
    loop_view(state, true)
        .ok_or_else(|| BridgeError::new("not_found", "the loop journal has no loop_created record"))
}

/// Fold the journal from disk (off the async threads).
pub(super) async fn read_disk(dir: &Path) -> Result<(LoopState, DiskMark), BridgeError> {
    let dir = dir.to_path_buf();
    let read = tokio::task::spawn_blocking(move || {
        let len = std::fs::metadata(agentpit_events::loops::journal_path(&dir))
            .map(|m| m.len())
            .unwrap_or(0);
        read_loop(&dir).map(|(state, scan)| {
            let closed = scan
                .records
                .last()
                .is_some_and(|r| r.kind == "writer_closed");
            (state, DiskMark { len, closed })
        })
    })
    .await
    .map_err(|e| BridgeError::internal(format!("read the loop journal: {e}")))?;
    read.map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => BridgeError::new("not_found", "no such loop"),
        _ => BridgeError::new("io", format!("read the loop journal: {e}")),
    })
}

fn journal_len(dir: &Path) -> u64 {
    std::fs::metadata(agentpit_events::loops::journal_path(dir))
        .map(|m| m.len())
        .unwrap_or(0)
}

/// Throttled `loops:view` / `loops:chunks` for one loop.
struct Outbox {
    bridge: Arc<Bridge>,
    loop_id: String,
    last_view: Option<Instant>,
    view_dirty: bool,
    chunks: Vec<ChunkOut>,
    chunk_bytes: usize,
    last_chunks: Option<Instant>,
}

impl Outbox {
    fn new(bridge: Arc<Bridge>, loop_id: String) -> Self {
        Outbox {
            bridge,
            loop_id,
            last_view: None,
            view_dirty: false,
            chunks: Vec::new(),
            chunk_bytes: 0,
            last_chunks: None,
        }
    }

    fn view_changed(&mut self, state: &Mutex<LoopState>) {
        if self.last_view.is_none_or(|t| t.elapsed() >= VIEW_EVERY) {
            self.emit_view(state);
        } else {
            self.view_dirty = true;
        }
    }

    fn emit_view(&mut self, state: &Mutex<LoopState>) {
        self.view_dirty = false;
        self.last_view = Some(Instant::now());
        let view = loop_view(&lock(state), true);
        if let Some(view) = view {
            self.bridge.emit(
                "loops:view",
                &ViewPayload {
                    loop_id: &self.loop_id,
                    view,
                },
            );
        }
    }

    fn chunk(&mut self, step_id: String, offset: u64, text: String) {
        self.chunk_bytes += text.len();
        match self.chunks.last_mut() {
            Some(last)
                if last.step_id == step_id && last.offset + last.text.len() as u64 == offset =>
            {
                last.text.push_str(&text);
            }
            _ => self.chunks.push(ChunkOut {
                step_id,
                offset,
                text,
            }),
        }
        while self.chunk_bytes > CHUNK_BUFFER_MAX && self.chunks.len() > 1 {
            let dropped = self.chunks.remove(0);
            self.chunk_bytes -= dropped.text.len();
        }
        if self.last_chunks.is_none_or(|t| t.elapsed() >= CHUNK_EVERY) {
            self.emit_chunks();
        }
    }

    fn emit_chunks(&mut self) {
        self.last_chunks = Some(Instant::now());
        if self.chunks.is_empty() {
            return;
        }
        self.chunk_bytes = 0;
        let chunks = std::mem::take(&mut self.chunks);
        self.bridge.emit(
            "loops:chunks",
            &ChunksPayload {
                loop_id: &self.loop_id,
                chunks,
            },
        );
    }

    /// When the next throttled emit is due, if one is pending.
    fn due(&self) -> Option<Instant> {
        let view = self
            .view_dirty
            .then(|| self.last_view.map_or_else(Instant::now, |t| t + VIEW_EVERY));
        let chunks = (!self.chunks.is_empty()).then(|| {
            self.last_chunks
                .map_or_else(Instant::now, |t| t + CHUNK_EVERY)
        });
        match (view, chunks) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    fn flush_due(&mut self, state: &Mutex<LoopState>) {
        let now = Instant::now();
        if self.view_dirty && self.last_view.is_none_or(|t| t + VIEW_EVERY <= now) {
            self.emit_view(state);
        }
        if !self.chunks.is_empty() && self.last_chunks.is_none_or(|t| t + CHUNK_EVERY <= now) {
            self.emit_chunks();
        }
    }

    fn flush_all(&mut self, state: &Mutex<LoopState>) {
        if self.view_dirty {
            self.emit_view(state);
        }
        self.emit_chunks();
    }
}

async fn sleep_until(at: Option<Instant>) {
    match at {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending().await,
    }
}

/// Replace the state with the journal on disk; emit when it moved.
async fn resync(dir: &Path, shared: &Mutex<LoopState>, out: &mut Outbox, mark: &mut DiskMark) {
    let Ok((state, new_mark)) = read_disk(dir).await else {
        return;
    };
    *mark = new_mark;
    let changed = {
        let mut s = lock(shared);
        let changed = s.head_seq != state.head_seq || s.uid() != state.uid();
        *s = state;
        changed
    };
    if changed {
        out.view_changed(shared);
    }
}

enum Followed {
    /// The runner connection ended.
    Ended,
    /// The view was closed.
    Closed,
}

async fn run_view(
    bridge: Arc<Bridge>,
    loop_id: String,
    dir: PathBuf,
    shared: Arc<Mutex<LoopState>>,
    mut mark: DiskMark,
    mut kicks: mpsc::UnboundedReceiver<Kick>,
) {
    let mut out = Outbox::new(Arc::clone(&bridge), loop_id.clone());
    let mut retry = RETRY_MIN;
    let mut hint: Option<PathBuf> = None;
    // The head at which the daemon said no runner will serve this loop (finished or
    // read-only): not asked again until the journal moves or an operation names a runner.
    let mut unservable_at: Option<u64> = None;
    loop {
        let (terminal, wants, head) = {
            let s = lock(&shared);
            (s.status.is_terminal(), wants_runner(&s), s.head_seq)
        };
        let blocked = unservable_at == Some(head);
        let attach = hint.is_some()
            || (!terminal
                && !blocked
                && !mark.closed
                && (wants || bridge.row_runner_live(&loop_id)));
        if attach {
            let socket = match hint.take() {
                Some(socket) => Ok(socket),
                None => bridge.ensure_runner(&loop_id).await,
            };
            let started = Instant::now();
            let result = match socket {
                Ok(socket) => follow(&bridge, &socket, &shared, &mut out, &mut kicks).await,
                Err(e) => Err(e),
            };
            let pause = match result {
                Ok(Followed::Closed) => return,
                // A connection that lasted is a normal end (parked, finished, restarted).
                Ok(Followed::Ended) if started.elapsed() >= STABLE_CONNECTION => {
                    retry = RETRY_MIN;
                    false
                }
                Err(e) if e.code == "gone" || e.code == "read_only" => {
                    unservable_at = Some(head);
                    false
                }
                // Refused, or dropped at once (a runner crashing on start): back off.
                Ok(Followed::Ended) | Err(_) => true,
            };
            // The journal is the truth, whatever the connection saw.
            resync(&dir, &shared, &mut out, &mut mark).await;
            if pause {
                tokio::select! {
                    kick = kicks.recv() => match kick {
                        None => return,
                        Some(Kick::Runner(socket)) => hint = Some(socket),
                        Some(Kick::Recheck) => {}
                    },
                    _ = tokio::time::sleep(retry) => {}
                }
                retry = (retry * 2).min(RETRY_MAX);
            }
            continue;
        }

        // No runner to follow: watch the journal file.
        tokio::select! {
            kick = kicks.recv() => match kick {
                None => return,
                Some(Kick::Runner(socket)) => hint = Some(socket),
                Some(Kick::Recheck) => {}
            },
            _ = tokio::time::sleep(DISK_POLL) => {}
            _ = sleep_until(out.due()) => out.flush_due(&shared),
        }
        let len = {
            let dir = dir.clone();
            tokio::task::spawn_blocking(move || journal_len(&dir))
                .await
                .unwrap_or(mark.len)
        };
        if len != mark.len {
            resync(&dir, &shared, &mut out, &mut mark).await;
        }
    }
}

/// Attach to a runner and apply what it streams until the connection ends.
async fn follow(
    bridge: &Arc<Bridge>,
    socket: &Path,
    shared: &Mutex<LoopState>,
    out: &mut Outbox,
    kicks: &mut mpsc::UnboundedReceiver<Kick>,
) -> Result<Followed, BridgeError> {
    let mut conn = bridge.runner(socket).await?;
    let since = {
        let s = lock(shared);
        s.uid().map(|uid| Cursor {
            uid: uid.to_string(),
            seq: s.head_seq,
        })
    };
    let id = conn
        .send(RequestBody::LoopAttach {
            since,
            chunks: true,
        })
        .await?;
    let attached = tokio::time::timeout(CONTROL_TIMEOUT, async {
        loop {
            match conn.recv().await? {
                Frame::Response(r) if r.id == id => return response_data(r),
                _ => continue,
            }
        }
    })
    .await
    .map_err(|_| BridgeError::new("timeout", "the runner did not answer loop_attach"))??;
    match attached {
        ResponseData::LoopAttached { reset: true, .. } => {
            // Another incarnation, or a cursor ahead of the journal: replay from seq 1.
            *lock(shared) = LoopState::default();
        }
        ResponseData::LoopAttached { .. } => {}
        other => {
            return Err(BridgeError::protocol(format!(
                "unexpected answer to loop_attach: {other:?}"
            )))
        }
    }
    loop {
        tokio::select! {
            frame = conn.recv() => {
                let frame = match frame {
                    Ok(frame) => frame,
                    Err(_) => {
                        out.flush_all(shared);
                        return Ok(Followed::Ended);
                    }
                };
                match frame {
                    Frame::Event(Event::LoopRecord { rec, .. }) => {
                        let Ok(line) = serde_json::to_string(&rec) else { continue };
                        let Ok(record) = decode_line(&line) else { continue };
                        {
                            let mut s = lock(shared);
                            if record.seq <= s.head_seq {
                                continue;
                            }
                            s.apply(&record);
                        }
                        out.view_changed(shared);
                    }
                    Frame::Event(Event::LoopChunk { step_id, offset, text, .. }) => {
                        out.chunk(step_id, offset, text);
                    }
                    _ => {}
                }
            }
            kick = kicks.recv() => {
                if kick.is_none() {
                    return Ok(Followed::Closed);
                }
                // Already following the runner: nothing to do.
            }
            _ = sleep_until(out.due()) => out.flush_due(shared),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_loop_waiting_for_a_person_does_not_want_a_runner() {
        let mut s = LoopState::default();
        assert!(!wants_runner(&s), "created");
        s.status = LoopStatus::Running;
        assert!(wants_runner(&s));
        s.status = LoopStatus::Stopping;
        assert!(wants_runner(&s));
        s.status = LoopStatus::Paused;
        assert!(!wants_runner(&s));
        s.status = LoopStatus::Succeeded;
        assert!(!wants_runner(&s));
    }
}
