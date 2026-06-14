//! Reconstruct run state from agentpit's JSONL event log, incrementally.
//!
//! The event schema comes from the shared `agentpit-events` crate, so the dashboard can't
//! drift from what the CLI writes. `Tracker` keeps a running view and ingests only the
//! bytes appended since the last read (resetting if the log was compacted/truncated), so
//! per-poll cost is proportional to new events rather than total history.

use std::collections::BTreeMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use agentpit_events::Event;
use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct MemberView {
    pub backend: String,
    pub aggregator: bool,
    /// "running" | "ok" | "error" | "skipped" | "interrupted" | "pending"
    pub status: String,
    pub started_ts: Option<u64>,
    pub elapsed_ms: Option<u64>,
    pub chars: Option<usize>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RunView {
    pub run_id: String,
    pub pid: u32,
    pub kind: String,
    pub cwd: String,
    pub started_ts: u64,
    pub finished: bool,
    /// Run-level outcome once finished, or "interrupted" if the process died mid-run.
    pub status: Option<String>,
    pub members: Vec<MemberView>,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct Snapshot {
    /// Runs whose process is still alive and which have not emitted run_finished.
    pub live: Vec<RunView>,
    /// Finished (or interrupted) runs, newest first.
    pub recent: Vec<RunView>,
}

/// Upper bound on how many runs the tracker keeps in memory. The event log grows for the
/// life of the machine, so without a cap a long-lived dashboard session would accumulate
/// every run ever recorded. Far above `RECENT_CAP` (what the UI shows) so scrollback isn't
/// affected; only the unbounded tail is reclaimed.
const MAX_TRACKED_RUNS: usize = 500;

/// Incremental reader over the event log. Holds the reconstructed run map and the byte
/// offset read so far.
#[derive(Default)]
pub struct Tracker {
    runs: BTreeMap<String, RunView>,
    order: Vec<String>,
    offset: u64,
}

impl Tracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Read and apply events appended since the last call. If the file shrank (the CLI
    /// compacted it) we can't trust the offset, so rebuild from the start.
    pub fn ingest(&mut self, path: &Path) {
        let mut file = match std::fs::File::open(path) {
            Ok(f) => f,
            Err(_) => return, // no log yet
        };
        let len = match file.metadata() {
            Ok(m) => m.len(),
            Err(_) => return,
        };
        if len < self.offset {
            self.reset();
        }
        if len == self.offset {
            return;
        }
        if file.seek(SeekFrom::Start(self.offset)).is_err() {
            return;
        }
        let mut buf = Vec::with_capacity((len - self.offset) as usize);
        if file.read_to_end(&mut buf).is_err() {
            return;
        }
        // Consume only up to the last newline; a partial trailing line is re-read next time.
        // The offset always lands on a newline boundary, so we never split a UTF-8 char.
        let last_nl = match buf.iter().rposition(|&b| b == b'\n') {
            Some(i) => i,
            None => return, // no complete line yet
        };
        let consumed = &buf[..=last_nl];
        if let Ok(text) = std::str::from_utf8(consumed) {
            for line in text.lines() {
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                if let Ok(ev) = serde_json::from_str::<Event>(line) {
                    self.apply(ev);
                }
            }
        }
        self.offset += (last_nl + 1) as u64;
    }

    fn reset(&mut self) {
        self.runs.clear();
        self.order.clear();
        self.offset = 0;
    }

    fn ensure(&mut self, id: &str) -> &mut RunView {
        if !self.runs.contains_key(id) {
            self.order.push(id.to_string());
            self.runs.insert(
                id.to_string(),
                RunView {
                    run_id: id.to_string(),
                    pid: 0,
                    kind: "?".into(),
                    cwd: String::new(),
                    started_ts: 0,
                    finished: false,
                    status: None,
                    members: Vec::new(),
                },
            );
            self.prune();
        }
        self.runs.get_mut(id).unwrap()
    }

    /// Evict the oldest *finished* runs once we exceed `MAX_TRACKED_RUNS` so memory stays
    /// bounded over a long-lived session. Live (unfinished) runs are never evicted, and the
    /// just-inserted run (at the back of `order`) is safe because eviction walks from the
    /// front (oldest first).
    fn prune(&mut self) {
        if self.order.len() <= MAX_TRACKED_RUNS {
            return;
        }
        let mut excess = self.order.len() - MAX_TRACKED_RUNS;
        let mut kept = Vec::with_capacity(self.order.len());
        for id in std::mem::take(&mut self.order) {
            let finished = self.runs.get(&id).map(|r| r.finished).unwrap_or(true);
            if excess > 0 && finished {
                self.runs.remove(&id);
                excess -= 1;
            } else {
                kept.push(id);
            }
        }
        self.order = kept;
    }

    fn apply(&mut self, ev: Event) {
        match ev {
            Event::RunStarted {
                ts,
                run_id,
                pid,
                kind,
                members,
                cwd,
            } => {
                let run = self.ensure(&run_id);
                run.pid = pid;
                run.kind = kind.as_str().to_string();
                run.cwd = cwd;
                run.started_ts = ts;
                for b in members {
                    let name = b.as_str().to_string();
                    if !run
                        .members
                        .iter()
                        .any(|m| m.backend == name && !m.aggregator)
                    {
                        run.members.push(MemberView {
                            backend: name,
                            aggregator: false,
                            status: "pending".into(),
                            started_ts: None,
                            elapsed_ms: None,
                            chars: None,
                            error: None,
                        });
                    }
                }
            }
            Event::MemberStarted {
                ts,
                run_id,
                backend,
                aggregator,
            } => {
                let run = self.ensure(&run_id);
                let m = find_or_push(&mut run.members, backend.as_str(), aggregator);
                m.status = "running".into();
                m.started_ts = Some(ts);
            }
            Event::MemberFinished {
                run_id,
                backend,
                aggregator,
                status,
                elapsed_ms,
                chars,
                error,
                ..
            } => {
                let run = self.ensure(&run_id);
                let m = find_or_push(&mut run.members, backend.as_str(), aggregator);
                m.status = status.as_str().to_string();
                m.elapsed_ms = Some(elapsed_ms);
                m.chars = chars;
                m.error = error;
            }
            Event::RunFinished { run_id, status, .. } => {
                let run = self.ensure(&run_id);
                run.finished = true;
                run.status = Some(status.as_str().to_string());
            }
        }
    }

    /// Split runs into live vs. recent. `alive` reports whether a pid is still running, so
    /// an aborted run (no run_finished) isn't shown live forever.
    pub fn snapshot(&self, alive: impl Fn(u32) -> bool, recent_cap: usize) -> Snapshot {
        let mut live: Vec<RunView> = Vec::new();
        let mut recent: Vec<RunView> = Vec::new();
        for id in &self.order {
            let mut run = self.runs.get(id).unwrap().clone();
            if !run.finished && alive(run.pid) {
                live.push(run);
            } else {
                if !run.finished {
                    run.status = Some("interrupted".into());
                    for m in run.members.iter_mut() {
                        if m.status == "running" || m.status == "pending" {
                            m.status = "interrupted".into();
                        }
                    }
                }
                recent.push(run);
            }
        }
        live.sort_by_key(|r| std::cmp::Reverse(r.started_ts));
        recent.sort_by_key(|r| std::cmp::Reverse(r.started_ts));
        recent.truncate(recent_cap);
        Snapshot { live, recent }
    }
}

fn find_or_push<'a>(
    members: &'a mut Vec<MemberView>,
    backend: &str,
    aggregator: bool,
) -> &'a mut MemberView {
    if let Some(idx) = members
        .iter()
        .position(|m| m.backend == backend && m.aggregator == aggregator)
    {
        return &mut members[idx];
    }
    members.push(MemberView {
        backend: backend.to_string(),
        aggregator,
        status: "pending".into(),
        started_ts: None,
        elapsed_ms: None,
        chars: None,
        error: None,
    });
    let last = members.len() - 1;
    &mut members[last]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    const LOG: &str = r#"
{"event":"run_started","ts":100,"run_id":"1-0","pid":4242,"kind":"ensemble","members":["gemini","claude"],"cwd":"/tmp"}
{"event":"member_started","ts":101,"run_id":"1-0","backend":"gemini","aggregator":false}
{"event":"member_started","ts":101,"run_id":"1-0","backend":"claude","aggregator":false}
{"event":"member_finished","ts":300,"run_id":"1-0","backend":"claude","aggregator":false,"status":"ok","elapsed_ms":199,"chars":42}
"#;

    fn tracker_from(text: &str) -> (Tracker, tempfile::NamedTempFile) {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(text.as_bytes()).unwrap();
        f.flush().unwrap();
        let mut t = Tracker::new();
        t.ingest(f.path());
        (t, f)
    }

    #[test]
    fn unfinished_run_with_live_pid_is_live() {
        let (t, _f) = tracker_from(LOG);
        let snap = t.snapshot(|_| true, 20);
        assert_eq!(snap.live.len(), 1);
        assert_eq!(snap.recent.len(), 0);
        let run = &snap.live[0];
        assert_eq!(run.kind, "ensemble");
        assert_eq!(run.members.len(), 2);
        assert_eq!(
            run.members
                .iter()
                .find(|m| m.backend == "gemini")
                .unwrap()
                .status,
            "running"
        );
        let claude = run.members.iter().find(|m| m.backend == "claude").unwrap();
        assert_eq!(claude.status, "ok");
        assert_eq!(claude.chars, Some(42));
    }

    #[test]
    fn unfinished_run_with_dead_pid_is_interrupted_recent() {
        let (t, _f) = tracker_from(LOG);
        let snap = t.snapshot(|_| false, 20);
        assert_eq!(snap.live.len(), 0);
        assert_eq!(snap.recent.len(), 1);
        assert_eq!(snap.recent[0].status.as_deref(), Some("interrupted"));
        assert_eq!(
            snap.recent[0]
                .members
                .iter()
                .find(|m| m.backend == "gemini")
                .unwrap()
                .status,
            "interrupted"
        );
    }

    #[test]
    fn finished_run_goes_to_recent() {
        let log = format!(
            "{LOG}{}\n",
            r#"{"event":"run_finished","ts":400,"run_id":"1-0","status":"ok"}"#
        );
        let (t, _f) = tracker_from(&log);
        let snap = t.snapshot(|_| true, 20);
        assert_eq!(snap.live.len(), 0);
        assert_eq!(snap.recent.len(), 1);
        assert_eq!(snap.recent[0].status.as_deref(), Some("ok"));
    }

    #[test]
    fn tolerates_garbage_lines() {
        let (t, _f) = tracker_from("not json\n{\"event\":\"bogus\"}\n");
        let snap = t.snapshot(|_| true, 20);
        assert_eq!(snap.live.len(), 0);
        assert_eq!(snap.recent.len(), 0);
    }

    #[test]
    fn incremental_ingest_appends_without_reparsing() {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(b"{\"event\":\"run_started\",\"ts\":1,\"run_id\":\"r\",\"pid\":1,\"kind\":\"rescue\",\"members\":[\"gemini\"],\"cwd\":\"/\"}\n").unwrap();
        f.flush().unwrap();
        let mut t = Tracker::new();
        t.ingest(f.path());
        let off1 = t.offset;
        assert!(off1 > 0);
        // Append one more line; ingest should advance from the prior offset.
        f.write_all(b"{\"event\":\"member_finished\",\"run_id\":\"r\",\"backend\":\"gemini\",\"status\":\"ok\",\"elapsed_ms\":5,\"ts\":2}\n").unwrap();
        f.flush().unwrap();
        t.ingest(f.path());
        assert!(t.offset > off1);
        let snap = t.snapshot(|_| true, 20);
        let run = &snap.live[0];
        assert_eq!(run.members[0].status, "ok");
    }

    #[test]
    fn partial_trailing_line_is_not_consumed_until_complete() {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        // Write a complete line plus a partial (no trailing newline).
        f.write_all(b"{\"event\":\"run_started\",\"ts\":1,\"run_id\":\"r\",\"pid\":1,\"kind\":\"rescue\",\"members\":[\"gemini\"],\"cwd\":\"/\"}\n{\"event\":\"member_st").unwrap();
        f.flush().unwrap();
        let mut t = Tracker::new();
        t.ingest(f.path());
        assert_eq!(t.snapshot(|_| true, 20).live.len(), 1);
        // Complete the partial line.
        f.write_all(b"arted\",\"ts\":2,\"run_id\":\"r\",\"backend\":\"gemini\"}\n")
            .unwrap();
        f.flush().unwrap();
        t.ingest(f.path());
        let snap = t.snapshot(|_| true, 20);
        assert_eq!(snap.live[0].members[0].status, "running");
    }

    #[test]
    fn finished_runs_are_evicted_past_the_cap() {
        let n = MAX_TRACKED_RUNS + 50;
        let mut log = String::new();
        for i in 0..n {
            log.push_str(&format!(
                "{{\"event\":\"run_started\",\"ts\":{i},\"run_id\":\"r{i}\",\"pid\":1,\"kind\":\"rescue\",\"members\":[\"gemini\"],\"cwd\":\"/\"}}\n"
            ));
            log.push_str(&format!(
                "{{\"event\":\"run_finished\",\"ts\":{i},\"run_id\":\"r{i}\",\"status\":\"ok\"}}\n"
            ));
        }
        let (t, _f) = tracker_from(&log);
        assert!(
            t.runs.len() <= MAX_TRACKED_RUNS,
            "runs map must stay bounded"
        );
        assert_eq!(t.order.len(), t.runs.len(), "order and map stay in sync");
        // Oldest evicted, newest retained.
        assert!(!t.runs.contains_key("r0"));
        assert!(t.runs.contains_key(&format!("r{}", n - 1)));
    }

    #[test]
    fn live_runs_are_not_evicted() {
        // One live run, then enough finished runs to exceed the cap. The live one survives.
        let mut log = String::from(
            "{\"event\":\"run_started\",\"ts\":0,\"run_id\":\"live\",\"pid\":1,\"kind\":\"rescue\",\"members\":[\"gemini\"],\"cwd\":\"/\"}\n",
        );
        for i in 1..(MAX_TRACKED_RUNS + 50) {
            log.push_str(&format!(
                "{{\"event\":\"run_started\",\"ts\":{i},\"run_id\":\"r{i}\",\"pid\":1,\"kind\":\"rescue\",\"members\":[\"gemini\"],\"cwd\":\"/\"}}\n"
            ));
            log.push_str(&format!(
                "{{\"event\":\"run_finished\",\"ts\":{i},\"run_id\":\"r{i}\",\"status\":\"ok\"}}\n"
            ));
        }
        let (t, _f) = tracker_from(&log);
        assert!(
            t.runs.contains_key("live"),
            "unfinished run must not be evicted"
        );
    }

    #[test]
    fn truncation_triggers_rebuild() {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(LOG.as_bytes()).unwrap();
        f.flush().unwrap();
        let mut t = Tracker::new();
        t.ingest(f.path());
        assert_eq!(t.snapshot(|_| true, 20).live.len(), 1);
        // Compaction rewrites the file shorter, with a different run.
        let smaller = "{\"event\":\"run_started\",\"ts\":9,\"run_id\":\"z\",\"pid\":7,\"kind\":\"review\",\"members\":[\"codex\"],\"cwd\":\"/x\"}\n";
        std::fs::write(f.path(), smaller).unwrap();
        t.ingest(f.path());
        let snap = t.snapshot(|_| true, 20);
        assert_eq!(snap.live.len(), 1);
        assert_eq!(snap.live[0].run_id, "z");
    }
}
