//! The board: every loop's row from the daemon's `loop_watch`, the inbox derived from
//! those rows, and the daemon status.
//!
//! Rows come from `head.json`, so a parked loop's gates are in the inbox without waking
//! it. When a gate leaves a row, the journal on disk says who answered it and how; the
//! inbox keeps a short list of those so a second window sees what happened (design §12).

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use agentpit_events::loops::{loop_dir_in, read_loop, Actor, GateHead, GateStatus, LoopSummary};
use agentpit_events::wire::{Event, Frame, LoopRow, RequestBody, ResponseData};
use serde::Serialize;

use super::{lock, Bridge, BridgeError};

/// Rows arrive in bursts (one daemon poll can change many loops): emit once the burst
/// is over, but never later than this after the first change.
const BOARD_QUIET: Duration = Duration::from_millis(15);
const BOARD_MAX_DELAY: Duration = Duration::from_millis(60);
const RECONNECT_MIN: Duration = Duration::from_millis(250);
const RECONNECT_MAX: Duration = Duration::from_secs(5);
/// Answered gates the inbox keeps showing.
const ANSWERED_KEEP: usize = 20;
const ANSWERED_MAX_AGE_MS: u64 = 30 * 60 * 1000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum StatusState {
    #[default]
    Idle,
    Connecting,
    Connected,
    Down,
}

#[derive(Debug, Clone, PartialEq, Serialize, Default)]
pub struct DaemonStatus {
    pub state: StatusState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    pub since_ms: u64,
}

/// One open gate in the inbox.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct InboxGate {
    pub loop_id: String,
    pub loop_title: String,
    pub blueprint: String,
    /// No runner is live: answering wakes the loop.
    pub parked: bool,
    #[serde(flatten)]
    pub gate: GateHead,
}

/// A gate answered while the app watched: who answered, and how.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AnsweredGate {
    pub loop_id: String,
    pub loop_title: String,
    pub gate_id: String,
    pub prompt: String,
    pub option: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub option_label: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub comment: Option<String>,
    pub by: Actor,
    /// When it was answered (journal time).
    pub ts: u64,
    /// When this app noticed (the inbox keeps answers for a while from here).
    #[serde(skip)]
    seen_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct InboxPayload {
    pub gates: Vec<InboxGate>,
    /// Gates beyond the few each row lists (the row's count is exact).
    pub hidden: usize,
    pub answered: Vec<AnsweredGate>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct BoardPayload {
    /// Most recently updated first.
    pub rows: Vec<LoopRow>,
    pub status: DaemonStatus,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct BoardSnapshot {
    pub rows: Vec<LoopRow>,
    pub status: DaemonStatus,
    pub inbox: InboxPayload,
}

#[derive(Default)]
pub struct Board {
    watching: bool,
    status: DaemonStatus,
    rows: BTreeMap<String, LoopRow>,
    answered: VecDeque<AnsweredGate>,
}

impl Board {
    fn payload(&self) -> BoardPayload {
        let mut rows: Vec<LoopRow> = self.rows.values().cloned().collect();
        rows.sort_by(|a, b| {
            b.summary
                .updated_ts
                .cmp(&a.summary.updated_ts)
                .then_with(|| a.summary.loop_id.cmp(&b.summary.loop_id))
        });
        BoardPayload {
            rows,
            status: self.status.clone(),
        }
    }

    fn inbox(&self, now_ms: u64) -> InboxPayload {
        let mut gates = Vec::new();
        let mut hidden = 0;
        for row in self.rows.values() {
            let s = &row.summary;
            if s.status.is_terminal() {
                continue;
            }
            hidden += s.open_gate_count.saturating_sub(s.open_gates.len());
            for g in &s.open_gates {
                gates.push(InboxGate {
                    loop_id: s.loop_id.clone(),
                    loop_title: s.title.clone(),
                    blueprint: s.blueprint.name.clone(),
                    parked: row.runner != "live",
                    gate: g.clone(),
                });
            }
        }
        // Soonest deadline first, then by loop and gate number (stable across emits).
        gates.sort_by(|a, b| {
            let da = a.gate.deadline_ms.unwrap_or(u64::MAX);
            let db = b.gate.deadline_ms.unwrap_or(u64::MAX);
            da.cmp(&db)
                .then_with(|| a.loop_id.cmp(&b.loop_id))
                .then_with(|| gate_number(&a.gate.gate_id).cmp(&gate_number(&b.gate.gate_id)))
        });
        let answered = self
            .answered
            .iter()
            .filter(|a| now_ms.saturating_sub(a.seen_ms) <= ANSWERED_MAX_AGE_MS)
            .cloned()
            .collect();
        InboxPayload {
            gates,
            hidden,
            answered,
        }
    }

    fn remember(&mut self, answered: AnsweredGate) {
        if self
            .answered
            .iter()
            .any(|a| a.loop_id == answered.loop_id && a.gate_id == answered.gate_id)
        {
            return;
        }
        self.answered.push_front(answered);
        self.answered.truncate(ANSWERED_KEEP);
    }
}

fn gate_number(gate_id: &str) -> u64 {
    gate_id
        .strip_prefix('g')
        .and_then(|n| n.parse().ok())
        .unwrap_or(u64::MAX)
}

/// Gate ids a row listed before and no longer lists.
fn left_gates(old: Option<&LoopSummary>, new: Option<&LoopSummary>) -> Vec<String> {
    let Some(old) = old else {
        return Vec::new();
    };
    let now: BTreeSet<&str> = new
        .map(|n| n.open_gates.iter().map(|g| g.gate_id.as_str()).collect())
        .unwrap_or_default();
    old.open_gates
        .iter()
        .filter(|g| !now.contains(g.gate_id.as_str()))
        .map(|g| g.gate_id.clone())
        .collect()
}

impl Bridge {
    /// Start following the board (once), and return what is known now.
    pub fn board_snapshot(self: &Arc<Self>) -> BoardSnapshot {
        self.ensure_board();
        let board = lock(&self.board);
        BoardSnapshot {
            rows: board.payload().rows,
            status: board.status.clone(),
            inbox: board.inbox(agentpit_events::now_ms()),
        }
    }

    pub fn daemon_status(&self) -> DaemonStatus {
        lock(&self.board).status.clone()
    }

    /// The runner of a loop as the board last saw it (`"live"`/`"absent"`).
    pub(super) fn row_runner_live(&self, loop_id: &str) -> bool {
        lock(&self.board)
            .rows
            .get(loop_id)
            .is_some_and(|r| r.runner == "live")
    }

    fn ensure_board(self: &Arc<Self>) {
        {
            let mut board = lock(&self.board);
            if board.watching {
                return;
            }
            board.watching = true;
        }
        let me = Arc::clone(self);
        tokio::spawn(async move { me.watch_board().await });
    }

    pub(super) fn set_status(&self, state: StatusState, message: Option<&str>) {
        let status = {
            let mut board = lock(&self.board);
            let message = message.map(str::to_string);
            if board.status.state == state && board.status.message == message {
                return;
            }
            board.status = DaemonStatus {
                state,
                message,
                since_ms: agentpit_events::now_ms(),
            };
            board.status.clone()
        };
        self.emit("daemon:status", &status);
    }

    fn emit_board(&self) {
        let (board, inbox) = {
            let b = lock(&self.board);
            (b.payload(), b.inbox(agentpit_events::now_ms()))
        };
        self.emit("loops:board", &board);
        self.emit("loops:inbox", &inbox);
    }

    async fn watch_board(self: Arc<Self>) {
        let mut backoff = RECONNECT_MIN;
        loop {
            self.set_status(StatusState::Connecting, None);
            match self.follow_board().await {
                Ok(()) => backoff = RECONNECT_MIN,
                Err(e) => {
                    self.set_status(StatusState::Down, Some(&e.message));
                }
            }
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(RECONNECT_MAX);
        }
    }

    /// One `loop_watch` connection, until it ends. `Ok` once it was established (so the
    /// reconnect starts from the short backoff).
    async fn follow_board(self: &Arc<Self>) -> Result<(), BridgeError> {
        let mut conn = self.daemon(true).await?;
        let rows = match conn
            .request(RequestBody::LoopWatch {
                include_terminal: true,
            })
            .await?
        {
            ResponseData::Loops { loops } => loops,
            other => {
                return Err(BridgeError::protocol(format!(
                    "unexpected answer to loop_watch: {other:?}"
                )))
            }
        };
        self.replace_rows(rows);
        self.set_status(StatusState::Connected, None);
        self.emit_board();

        let mut dirty_since: Option<tokio::time::Instant> = None;
        loop {
            let flush_at = dirty_since.map(|first| {
                (tokio::time::Instant::now() + BOARD_QUIET).min(first + BOARD_MAX_DELAY)
            });
            tokio::select! {
                frame = conn.recv() => {
                    let frame = match frame {
                        Ok(f) => f,
                        Err(e) => {
                            if dirty_since.is_some() {
                                self.emit_board();
                            }
                            self.set_status(StatusState::Connecting, Some(&e.message));
                            return Ok(());
                        }
                    };
                    let changed = match frame {
                        Frame::Event(Event::LoopRow { row }) => {
                            self.upsert_row(*row);
                            true
                        }
                        Frame::Event(Event::LoopGone { loop_id }) => {
                            lock(&self.board).rows.remove(&loop_id).is_some()
                        }
                        _ => false,
                    };
                    if changed && dirty_since.is_none() {
                        dirty_since = Some(tokio::time::Instant::now());
                    }
                }
                _ = sleep_until(flush_at) => {
                    dirty_since = None;
                    self.emit_board();
                }
            }
        }
    }

    fn replace_rows(self: &Arc<Self>, rows: Vec<LoopRow>) {
        let mut board = lock(&self.board);
        board.rows = rows
            .into_iter()
            .map(|r| (r.summary.loop_id.clone(), r))
            .collect();
    }

    fn upsert_row(self: &Arc<Self>, row: LoopRow) {
        let loop_id = row.summary.loop_id.clone();
        let left = {
            let mut board = lock(&self.board);
            let left = left_gates(
                board.rows.get(&loop_id).map(|r| &r.summary),
                Some(&row.summary),
            );
            board.rows.insert(loop_id.clone(), row);
            left
        };
        self.kick_view(&loop_id);
        if !left.is_empty() {
            let me = Arc::clone(self);
            tokio::spawn(async move { me.record_answers(loop_id, left).await });
        }
    }

    /// Look up how gates that left a row were closed, and add the answered ones to the
    /// inbox's history.
    async fn record_answers(self: Arc<Self>, loop_id: String, gate_ids: Vec<String>) {
        let Some(dir) = loop_dir_in(&self.paths.loops_root, &loop_id) else {
            return;
        };
        let read = tokio::task::spawn_blocking(move || read_loop(&dir)).await;
        let Ok(Ok((state, _))) = read else {
            return;
        };
        let title = state
            .created
            .as_ref()
            .map(|c| c.title.clone())
            .unwrap_or_default();
        let mut added = false;
        {
            let mut board = lock(&self.board);
            for gate in state.gates.iter().filter(|g| gate_ids.contains(&g.gate_id)) {
                if gate.status != GateStatus::Resolved {
                    continue;
                }
                let Some(res) = &gate.resolution else {
                    continue;
                };
                board.remember(AnsweredGate {
                    loop_id: loop_id.clone(),
                    loop_title: title.clone(),
                    gate_id: gate.gate_id.clone(),
                    prompt: agentpit_events::loops::clamp_text(&gate.prompt, 280),
                    option: res.option.clone(),
                    option_label: gate
                        .options
                        .iter()
                        .find(|o| o.id == res.option)
                        .and_then(|o| o.label.clone()),
                    comment: res.comment.clone(),
                    by: res.by.clone(),
                    ts: res.ts,
                    seen_ms: agentpit_events::now_ms(),
                });
                added = true;
            }
        }
        if added {
            let inbox = lock(&self.board).inbox(agentpit_events::now_ms());
            self.emit("loops:inbox", &inbox);
        }
    }
}

/// Sleep until `at`, or forever when there is nothing to wait for.
async fn sleep_until(at: Option<tokio::time::Instant>) {
    match at {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending().await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn summary(loop_id: &str, gates: &[&str]) -> LoopSummary {
        let mut s: LoopSummary = serde_json::from_value(serde_json::json!({
            "loop_id": loop_id,
            "uid": "9c41e07a2b5d4f18",
            "title": "t",
            "blueprint": {"name": "bp", "rev": "b1-0"},
            "status": "running",
            "usage": {"steps": 0, "active_ms": 0},
            "budget": {"max_steps": 1, "max_active_secs": 1, "max_parallel": 1},
            "head_seq": 1,
            "updated_ts": 1,
            "writable": true
        }))
        .unwrap();
        s.open_gates = gates
            .iter()
            .map(|id| {
                serde_json::from_value(serde_json::json!({
                    "gate_id": id, "kind": "approval", "prompt": "ok?", "options": []
                }))
                .unwrap()
            })
            .collect();
        s.open_gate_count = gates.len();
        s
    }

    #[test]
    fn a_gate_leaves_a_row_only_when_the_row_stops_listing_it() {
        let before = summary("lp-a", &["g1", "g2"]);
        assert_eq!(
            left_gates(Some(&before), Some(&summary("lp-a", &["g2", "g3"]))),
            vec!["g1".to_string()]
        );
        assert!(left_gates(None, Some(&before)).is_empty());
        assert_eq!(left_gates(Some(&before), None).len(), 2);
    }

    #[test]
    fn the_inbox_lists_open_gates_of_unfinished_loops_and_marks_parked_ones() {
        let mut board = Board::default();
        let mut a = summary("lp-a", &["g2", "g10"]);
        a.open_gate_count = 7;
        board.rows.insert(
            "lp-a".into(),
            LoopRow {
                summary: a,
                runner: "absent".into(),
            },
        );
        let mut done = summary("lp-b", &["g1"]);
        done.status = agentpit_events::loops::LoopStatus::Succeeded;
        board.rows.insert(
            "lp-b".into(),
            LoopRow {
                summary: done,
                runner: "absent".into(),
            },
        );
        let inbox = board.inbox(0);
        let ids: Vec<&str> = inbox
            .gates
            .iter()
            .map(|g| g.gate.gate_id.as_str())
            .collect();
        assert_eq!(ids, ["g2", "g10"]);
        assert!(inbox.gates.iter().all(|g| g.parked));
        assert_eq!(inbox.hidden, 5);
    }
}
