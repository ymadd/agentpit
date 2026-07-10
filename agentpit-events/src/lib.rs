//! Shared run-event schema + capture for agentpit and its dashboard.
//!
//! agentpit is otherwise stateless: progress is printed to stderr and lost. To let
//! external tooling (the desktop dashboard) observe what agentpit is doing right now,
//! every dispatch appends newline-delimited JSON events to a state-dir log and streams
//! each backend's output to a per-run file.
//!
//! Both the agentpit CLI and the dashboard depend on this crate so the event schema and
//! the state-dir paths have a single source of truth — neither can silently drift from
//! the other.
//!
//! All writes are best-effort: a telemetry failure must never break a dispatch, so every
//! fallible step is swallowed. Set `AGENTPIT_NO_EVENTS=1` to disable emission.

use std::collections::HashSet;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::process;
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// Backend agents agentpit can route to. Lives here so the event schema and the CLI
/// share one definition. The CLI re-exports this as `crate::types::BackendId`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, PartialOrd, Ord)]
#[serde(rename_all = "lowercase")]
pub enum BackendId {
    Claude,
    Codex,
    Gemini,
    Antigravity,
    Opencode,
    Goose,
    Copilot,
}

impl BackendId {
    pub const ALL: &'static [BackendId] = &[
        BackendId::Claude,
        BackendId::Codex,
        BackendId::Gemini,
        BackendId::Antigravity,
        BackendId::Opencode,
        BackendId::Goose,
        BackendId::Copilot,
    ];

    pub fn as_str(&self) -> &'static str {
        match self {
            BackendId::Claude => "claude",
            BackendId::Codex => "codex",
            BackendId::Gemini => "gemini",
            BackendId::Antigravity => "antigravity",
            BackendId::Opencode => "opencode",
            BackendId::Goose => "goose",
            BackendId::Copilot => "copilot",
        }
    }
}

impl fmt::Display for BackendId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for BackendId {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "claude" => Ok(BackendId::Claude),
            "codex" => Ok(BackendId::Codex),
            "gemini" => Ok(BackendId::Gemini),
            "antigravity" | "agy" => Ok(BackendId::Antigravity),
            "opencode" => Ok(BackendId::Opencode),
            "goose" => Ok(BackendId::Goose),
            "copilot" => Ok(BackendId::Copilot),
            other => Err(format!("unknown backend: {other}")),
        }
    }
}

/// Which command kicked off a run. Used by the dashboard to label the run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunKind {
    Rescue,
    Review,
    SecurityReview,
    AdversarialReview,
    Explain,
    Refactor,
    Ensemble,
    Workflow,
    /// A `profile run` gold-bench sweep: one backend graded across the suite. Single-member,
    /// sequential — it shows in the dashboard swarm like any other run rather than staying invisible.
    Bench,
}

impl RunKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            RunKind::Rescue => "rescue",
            RunKind::Review => "review",
            RunKind::SecurityReview => "security_review",
            RunKind::AdversarialReview => "adversarial_review",
            RunKind::Explain => "explain",
            RunKind::Refactor => "refactor",
            RunKind::Ensemble => "ensemble",
            RunKind::Workflow => "workflow",
            RunKind::Bench => "bench",
        }
    }
}

/// Terminal status of a member or aggregator leg.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LegStatus {
    Ok,
    Error,
    Skipped,
}

impl LegStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            LegStatus::Ok => "ok",
            LegStatus::Error => "error",
            LegStatus::Skipped => "skipped",
        }
    }
}

/// One line in the event log. The `event` tag distinguishes variants.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    RunStarted {
        ts: u64,
        run_id: String,
        pid: u32,
        kind: RunKind,
        members: Vec<BackendId>,
        cwd: String,
    },
    MemberStarted {
        ts: u64,
        run_id: String,
        backend: BackendId,
        /// True when this leg is the aggregator pass rather than a parallel member.
        #[serde(default)]
        aggregator: bool,
    },
    MemberFinished {
        ts: u64,
        run_id: String,
        backend: BackendId,
        #[serde(default)]
        aggregator: bool,
        status: LegStatus,
        elapsed_ms: u64,
        #[serde(skip_serializing_if = "Option::is_none")]
        chars: Option<usize>,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    RunFinished {
        ts: u64,
        run_id: String,
        status: LegStatus,
    },
    /// The manager is asking the supervising human a question and is now blocked on it.
    /// The durable request/response lives in `asks/<ask_id>.json`; this event is the
    /// dashboard's notification that a new ask exists (and a best-effort audit line — the
    /// `asks/` files, not this log entry, are the source of truth, since compaction may
    /// drop old runs' lines).
    Ask {
        ts: u64,
        run_id: String,
        ask_id: String,
        prompt: String,
        /// "blocking" | "review" (the src side passes `AskKind::as_str()`).
        kind: String,
        #[serde(default)]
        option_count: usize,
        timeout_secs: u64,
        /// Pid of the process blocked on the ask, so the dashboard can reap a card whose
        /// asker has died.
        #[serde(default)]
        pid: u32,
    },
    /// An ask was resolved — either the human answered or it timed out.
    AskAnswered {
        ts: u64,
        run_id: String,
        ask_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        answer: Option<String>,
        #[serde(default)]
        timed_out: bool,
    },
    /// A durable note appended to the run transcript — the substrate for ① handoff and ③ the
    /// shared board (design §4.5 "conversation layer M1"). Unlike [`Event::Ask`], a note has no
    /// recipient field and no per-consumer cursor: the only long-lived consumer is the workflow
    /// manager, which reads the transcript in order. Notes are append-only, ordered, fire-and-forget
    /// (no claim/ack), and compaction-bounded exactly like every other event. The manager posts one
    /// to record a worker→manager handoff or a shared-board entry before it re-seeds or discards
    /// context. Best-effort and audit-only: the dashboard renders it for context but keeps no
    /// run-view state for it, and compaction may drop old runs' notes just like any other line.
    Note {
        ts: u64,
        run_id: String,
        /// Who authored the note: the dispatching worker's backend for a handoff, or the
        /// manager's own backend for a board entry. Omitted when the poster names no backend.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        from: Option<BackendId>,
        /// "handoff" (a 1→1 context pass to the next leg) or "board" (a shared scratch entry).
        /// Free-form; any other value is treated as a generic note.
        kind: String,
        /// The note body — the handed-off context or board entry. Clamped by the caller.
        body: String,
    },
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// `$XDG_STATE_HOME/agentpit` or `~/.local/state/agentpit`.
pub fn state_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("XDG_STATE_HOME") {
        if !dir.is_empty() {
            return PathBuf::from(dir).join("agentpit");
        }
    }
    dirs::home_dir()
        .map(|h| h.join(".local").join("state").join("agentpit"))
        .unwrap_or_else(|| PathBuf::from(".local/state/agentpit"))
}

/// Path to the append-only event log the dashboard tails.
pub fn events_path() -> PathBuf {
    state_dir().join("events.jsonl")
}

/// Directory holding per-run captured output, one subdir per run id.
pub fn runs_dir() -> PathBuf {
    state_dir().join("runs")
}

/// Per-ask request/response mailbox, a sibling of `runs/`. Files here are deliberately kept
/// OFF the run-output pruner and OFF the `events.jsonl` compactor so an in-flight ask record
/// can never vanish mid-poll.
pub fn asks_dir() -> PathBuf {
    state_dir().join("asks")
}

/// Path to an ask's request sidecar (`asks/<ask_id>.json`). `None` unless `ask_id` is a safe
/// single path component — mirrors [`backend_log_path`]'s validate-before-join discipline.
pub fn ask_request_path(ask_id: &str) -> Option<PathBuf> {
    is_safe_log_component(ask_id).then(|| asks_dir().join(format!("{ask_id}.json")))
}

/// Path to an ask's response sidecar (`asks/<ask_id>.response.json`). `None` unless `ask_id`
/// is a safe single path component.
pub fn ask_response_path(ask_id: &str) -> Option<PathBuf> {
    is_safe_log_component(ask_id).then(|| asks_dir().join(format!("{ask_id}.response.json")))
}

/// Log file capturing a single backend leg's streamed output within a run. The
/// aggregator pass is kept separate so it doesn't clobber the same backend's member log.
pub fn backend_log_path(run_id: &str, backend: &str, aggregator: bool) -> PathBuf {
    let name = if aggregator {
        format!("{backend}.agg.log")
    } else {
        format!("{backend}.log")
    };
    runs_dir().join(run_id).join(name)
}

/// Validate that `s` is safe to use as a single path component (a run_id or backend name).
///
/// Returns `true` only when `s` is non-empty, is neither `"."` nor `".."`, contains no
/// path-separator characters (`'/'` or `'\\'`), no NUL byte, and every character is ASCII
/// alphanumeric or one of `'.'`, `'_'`, `'-'`.  This guards every place a caller-supplied
/// run_id or backend name is joined onto an on-disk path with
/// [`PathBuf::join`], which does **not** collapse `..` and will replace the entire prefix
/// when given an absolute component.
pub fn is_safe_log_component(s: &str) -> bool {
    if s.is_empty() || s == "." || s == ".." {
        return false;
    }
    s.bytes().all(|b| {
        b != b'/' && b != b'\\' && b != b'\0' && {
            let c = b as char;
            c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-'
        }
    })
}

/// Whether event/output capture is on. Set `AGENTPIT_NO_EVENTS=1` to disable.
fn events_enabled() -> bool {
    std::env::var("AGENTPIT_NO_EVENTS")
        .map(|v| v.is_empty() || v == "0")
        .unwrap_or(true)
}

/// A streaming sink that appends every chunk to a backend's per-run log file, so the
/// dashboard can tail the agent's live output. Returns a no-op sink when capture is
/// disabled or the file can't be opened — streaming must never break a dispatch.
pub fn output_streamer(
    run_id: &str,
    backend: BackendId,
    aggregator: bool,
) -> Arc<dyn Fn(&str) + Send + Sync> {
    if !events_enabled() {
        return Arc::new(|_chunk: &str| {});
    }
    // Reject any run_id or backend name that could escape the runs directory.
    if !is_safe_log_component(run_id) || !is_safe_log_component(backend.as_str()) {
        return Arc::new(|_chunk: &str| {});
    }
    let path = backend_log_path(run_id, backend.as_str(), aggregator);
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let file: Option<File> = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .ok();
    let shared = Arc::new(Mutex::new(file));
    Arc::new(move |chunk: &str| {
        if let Ok(mut guard) = shared.lock() {
            if let Some(f) = guard.as_mut() {
                let _ = f.write_all(chunk.as_bytes());
                let _ = f.flush();
            }
        }
    })
}

/// Keep only the `keep` most-recently-modified run output dirs; drop older ones so a
/// long-lived state dir doesn't grow without bound. Best-effort.
pub fn prune_run_outputs(keep: usize) {
    let dir = runs_dir();
    let Ok(entries) = fs::read_dir(&dir) else {
        return;
    };
    let mut dirs: Vec<(SystemTime, PathBuf)> = entries
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_dir())
        .map(|e| {
            let mtime = e
                .metadata()
                .and_then(|m| m.modified())
                .unwrap_or(UNIX_EPOCH);
            (mtime, e.path())
        })
        .collect();
    if dirs.len() <= keep {
        return;
    }
    dirs.sort_by_key(|d| std::cmp::Reverse(d.0)); // newest first
    for (_, path) in dirs.into_iter().skip(keep) {
        let _ = fs::remove_dir_all(path);
    }
}

const COMPACT_THRESHOLD_BYTES: u64 = 4 * 1024 * 1024;
const COMPACT_KEEP_RUNS: usize = 500;

fn run_id_of(line: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(line)
        .ok()?
        .get("run_id")?
        .as_str()
        .map(|s| s.to_string())
}

/// Rewrite `events.jsonl` keeping only the newest `keep_runs` runs' lines once the file
/// crosses `max_bytes`. Bounds the log a long-lived setup would otherwise grow forever.
/// Writes via a temp file + atomic rename so a concurrent reader never sees a half file;
/// a concurrent appender re-opens the path per write, so at most an in-flight line races.
/// Best-effort — any failure leaves the existing log untouched.
fn compact_events_log(max_bytes: u64, keep_runs: usize) {
    let path = events_path();
    let Ok(meta) = fs::metadata(&path) else {
        return;
    };
    if meta.len() <= max_bytes {
        return;
    }
    let Ok(content) = fs::read_to_string(&path) else {
        return;
    };

    let mut order: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for line in content.lines() {
        if let Some(id) = run_id_of(line) {
            if seen.insert(id.clone()) {
                order.push(id);
            }
        }
    }
    if order.len() <= keep_runs {
        // Oversized but few runs (e.g. one enormous run) — nothing safe to drop.
        return;
    }

    let keep: HashSet<&str> = order
        .iter()
        .rev()
        .take(keep_runs)
        .map(|s| s.as_str())
        .collect();
    let mut out = String::with_capacity(content.len() / 2);
    for line in content.lines() {
        match run_id_of(line) {
            Some(id) if keep.contains(id.as_str()) => {
                out.push_str(line);
                out.push('\n');
            }
            _ => {}
        }
    }

    let tmp = path.with_extension("jsonl.compact");
    if fs::write(&tmp, out).is_ok() {
        let _ = fs::rename(&tmp, &path);
    } else {
        let _ = fs::remove_file(&tmp);
    }
}

/// A per-process nonce (nanoseconds at first use) so run ids stay unique even after the
/// OS recycles our pid for a later agentpit process.
fn process_nonce() -> u128 {
    static NONCE: OnceLock<u128> = OnceLock::new();
    *NONCE.get_or_init(|| {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    })
}

/// Generate a globally-unique run id: `<pid>-<process-nonce>-<monotonic-counter>`.
fn next_run_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{}-{}-{}", process::id(), process_nonce(), n)
}

/// Generate a globally-unique ask id: `ask-<pid>-<process-nonce>-<counter>`. The `ask-`
/// prefix keeps it from ever colliding with a run id and guarantees it passes
/// [`is_safe_log_component`]. Reuses the same pid+nonce uniqueness scheme as [`next_run_id`].
pub fn next_ask_token() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("ask-{}-{}-{}", process::id(), process_nonce(), n)
}

/// Best-effort emitter scoped to a single run. Cheap to clone (shares the run id).
#[derive(Debug, Clone)]
pub struct RunLogger {
    run_id: String,
    enabled: bool,
}

impl RunLogger {
    /// Start a run and emit `RunStarted`. Returns a logger carrying the run id.
    pub fn start(kind: RunKind, members: &[BackendId], cwd: &std::path::Path) -> Self {
        let enabled = events_enabled();
        if enabled {
            // Bound on-disk state before this run adds to it.
            prune_run_outputs(50);
            compact_events_log(COMPACT_THRESHOLD_BYTES, COMPACT_KEEP_RUNS);
        }
        let logger = RunLogger {
            run_id: next_run_id(),
            enabled,
        };
        logger.emit(Event::RunStarted {
            ts: now_ms(),
            run_id: logger.run_id.clone(),
            pid: process::id(),
            kind,
            members: members.to_vec(),
            cwd: cwd.display().to_string(),
        });
        logger
    }

    pub fn run_id(&self) -> &str {
        &self.run_id
    }

    /// Attach to an existing run id WITHOUT emitting `RunStarted`. Used by `agentpit ask` and
    /// the MCP `ask_human` tool, which join the manager's run to emit ask events rather than
    /// starting a fresh run.
    pub fn adopt(run_id: String) -> Self {
        RunLogger {
            run_id,
            enabled: events_enabled(),
        }
    }

    pub fn member_started(&self, backend: BackendId, aggregator: bool) {
        self.emit(Event::MemberStarted {
            ts: now_ms(),
            run_id: self.run_id.clone(),
            backend,
            aggregator,
        });
    }

    pub fn member_finished(
        &self,
        backend: BackendId,
        aggregator: bool,
        status: LegStatus,
        elapsed_ms: u64,
        chars: Option<usize>,
        error: Option<String>,
    ) {
        self.emit(Event::MemberFinished {
            ts: now_ms(),
            run_id: self.run_id.clone(),
            backend,
            aggregator,
            status,
            elapsed_ms,
            chars,
            error,
        });
    }

    pub fn finished(&self, status: LegStatus) {
        self.emit(Event::RunFinished {
            ts: now_ms(),
            run_id: self.run_id.clone(),
            status,
        });
    }

    /// Emit an `Ask` — the manager is now blocked waiting on the human.
    pub fn ask(
        &self,
        ask_id: &str,
        prompt: &str,
        kind: &str,
        option_count: usize,
        timeout_secs: u64,
        pid: u32,
    ) {
        self.emit(Event::Ask {
            ts: now_ms(),
            run_id: self.run_id.clone(),
            ask_id: ask_id.to_string(),
            prompt: prompt.to_string(),
            kind: kind.to_string(),
            option_count,
            timeout_secs,
            pid,
        });
    }

    /// Emit an `AskAnswered` — the human answered (`Some`) or the ask timed out (`timed_out`).
    pub fn ask_answered(&self, ask_id: &str, answer: Option<&str>, timed_out: bool) {
        self.emit(Event::AskAnswered {
            ts: now_ms(),
            run_id: self.run_id.clone(),
            ask_id: ask_id.to_string(),
            answer: answer.map(|s| s.to_string()),
            timed_out,
        });
    }

    /// Emit a `Note` — a durable transcript entry for ① handoff or ③ the shared board. `from`
    /// is the authoring backend (the handed-off worker, or the manager); `kind` is "handoff" or
    /// "board". Fire-and-forget: it reuses the same best-effort append path as every other event.
    pub fn note(&self, from: Option<BackendId>, kind: &str, body: &str) {
        self.emit(Event::Note {
            ts: now_ms(),
            run_id: self.run_id.clone(),
            from,
            kind: kind.to_string(),
            body: body.to_string(),
        });
    }

    /// Append one event as a JSON line. Silently ignores every failure.
    fn emit(&self, event: Event) {
        if !self.enabled {
            return;
        }
        let _ = append_line(&event);
    }
}

fn append_line(event: &Event) -> std::io::Result<()> {
    let dir = state_dir();
    fs::create_dir_all(&dir)?;
    let mut line = serde_json::to_string(event).map_err(std::io::Error::other)?;
    line.push('\n');
    let mut f = OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("events.jsonl"))?;
    f.write_all(line.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_log_component_accepts_valid_ids() {
        assert!(is_safe_log_component("1234-5-6"));
        assert!(is_safe_log_component("claude"));
        assert!(is_safe_log_component("opencode.agg"));
        assert!(is_safe_log_component("my_backend-1"));
    }

    #[test]
    fn safe_log_component_rejects_traversal_and_separators() {
        assert!(!is_safe_log_component(".."));
        assert!(!is_safe_log_component("."));
        assert!(!is_safe_log_component("../etc"));
        assert!(!is_safe_log_component("a/b"));
        assert!(!is_safe_log_component("a\\b"));
        assert!(!is_safe_log_component("/abs"));
        assert!(!is_safe_log_component(""));
    }

    #[test]
    fn safe_log_component_rejects_nul_byte() {
        let with_nul = "bad\x00name";
        assert!(!is_safe_log_component(with_nul));
    }

    // Tests that mutate the process-wide XDG_STATE_HOME must not run concurrently.
    static ENV_LOCK: Mutex<()> = Mutex::new(());
    fn lock_env() -> std::sync::MutexGuard<'static, ()> {
        ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn parses_known_backends() {
        assert_eq!("gemini".parse::<BackendId>().unwrap(), BackendId::Gemini);
        assert_eq!("AGY".parse::<BackendId>().unwrap(), BackendId::Antigravity);
    }

    #[test]
    fn backend_display_round_trips() {
        for id in BackendId::ALL {
            assert_eq!(id.as_str().parse::<BackendId>().unwrap(), *id);
        }
    }

    #[test]
    fn run_ids_are_unique_and_carry_pid_and_nonce() {
        let a = next_run_id();
        let b = next_run_id();
        assert_ne!(a, b);
        assert!(a.starts_with(&process::id().to_string()));
        // pid-nonce-counter → three dash-separated parts.
        assert_eq!(a.split('-').count(), 3);
    }

    #[test]
    fn event_round_trips_through_json() {
        let ev = Event::MemberFinished {
            ts: 123,
            run_id: "1-0".into(),
            backend: BackendId::Gemini,
            aggregator: false,
            status: LegStatus::Ok,
            elapsed_ms: 3200,
            chars: Some(1024),
            error: None,
        };
        let json = serde_json::to_string(&ev).unwrap();
        assert!(json.contains("\"event\":\"member_finished\""));
        assert!(json.contains("\"backend\":\"gemini\""));
        assert!(json.contains("\"status\":\"ok\""));
        let back: Event = serde_json::from_str(&json).unwrap();
        match back {
            Event::MemberFinished { chars, .. } => assert_eq!(chars, Some(1024)),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn bench_run_kind_serializes_as_bench_and_round_trips() {
        // The dashboard deserializes RunStarted via this crate, so the `bench` wire tag is a
        // contract: a gold-bench sweep must label its swarm row "bench", not silently fail to parse.
        assert_eq!(RunKind::Bench.as_str(), "bench");
        let ev = Event::RunStarted {
            ts: 1,
            run_id: "1-0".into(),
            pid: 7,
            kind: RunKind::Bench,
            members: vec![BackendId::Codex],
            cwd: "/x".into(),
        };
        let json = serde_json::to_string(&ev).unwrap();
        assert!(json.contains("\"kind\":\"bench\""), "got: {json}");
        match serde_json::from_str::<Event>(&json).unwrap() {
            Event::RunStarted { kind, .. } => assert_eq!(kind, RunKind::Bench),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn writes_to_temp_state_dir() {
        let _env = lock_env();
        let tmp = tempfile::tempdir().unwrap();
        // SAFETY: single-threaded test; sets the env only for this process.
        unsafe {
            std::env::set_var("XDG_STATE_HOME", tmp.path());
        }
        let logger = RunLogger::start(RunKind::Rescue, &[BackendId::Gemini], tmp.path());
        logger.member_started(BackendId::Gemini, false);
        logger.member_finished(BackendId::Gemini, false, LegStatus::Ok, 10, Some(5), None);
        logger.finished(LegStatus::Ok);

        let contents = std::fs::read_to_string(tmp.path().join("agentpit/events.jsonl")).unwrap();
        let lines: Vec<&str> = contents.lines().collect();
        assert_eq!(lines.len(), 4);
        assert!(lines[0].contains("run_started"));
        assert!(lines[3].contains("run_finished"));
        unsafe {
            std::env::remove_var("XDG_STATE_HOME");
        }
    }

    #[test]
    fn compaction_keeps_only_newest_runs() {
        let _env = lock_env();
        let tmp = tempfile::tempdir().unwrap();
        let log = tmp.path().join("agentpit").join("events.jsonl");
        fs::create_dir_all(log.parent().unwrap()).unwrap();
        // 5 runs, 2 lines each.
        let mut content = String::new();
        for i in 0..5 {
            content.push_str(&format!(
                "{{\"event\":\"run_started\",\"ts\":{i},\"run_id\":\"r{i}\",\"pid\":1,\"kind\":\"rescue\",\"members\":[\"gemini\"],\"cwd\":\"/\"}}\n"
            ));
            content.push_str(&format!(
                "{{\"event\":\"run_finished\",\"ts\":{i},\"run_id\":\"r{i}\",\"status\":\"ok\"}}\n"
            ));
        }
        fs::write(&log, &content).unwrap();
        unsafe {
            std::env::set_var("XDG_STATE_HOME", tmp.path());
        }
        // Force compaction regardless of size, keeping the newest 2 runs.
        compact_events_log(0, 2);
        let out = fs::read_to_string(&log).unwrap();
        assert!(out.contains("\"r4\""));
        assert!(out.contains("\"r3\""));
        assert!(!out.contains("\"r0\""));
        assert!(!out.contains("\"r2\""));
        unsafe {
            std::env::remove_var("XDG_STATE_HOME");
        }
    }

    #[test]
    fn ask_events_round_trip_through_json() {
        let ask = Event::Ask {
            ts: 1,
            run_id: "r1".into(),
            ask_id: "ask-1-2-3".into(),
            prompt: "Proceed?".into(),
            kind: "blocking".into(),
            option_count: 2,
            timeout_secs: 180,
            pid: 4321,
        };
        let json = serde_json::to_string(&ask).unwrap();
        assert!(json.contains("\"event\":\"ask\""), "got: {json}");
        assert!(json.contains("\"kind\":\"blocking\""));
        assert!(matches!(
            serde_json::from_str::<Event>(&json).unwrap(),
            Event::Ask {
                option_count: 2,
                pid: 4321,
                ..
            }
        ));

        let answered = Event::AskAnswered {
            ts: 2,
            run_id: "r1".into(),
            ask_id: "ask-1-2-3".into(),
            answer: Some("yes".into()),
            timed_out: false,
        };
        let json = serde_json::to_string(&answered).unwrap();
        assert!(json.contains("\"event\":\"ask_answered\""), "got: {json}");
        assert!(json.contains("\"answer\":\"yes\""));

        // A timed-out answer omits the `answer` field entirely.
        let timed_out = Event::AskAnswered {
            ts: 3,
            run_id: "r1".into(),
            ask_id: "ask-1-2-3".into(),
            answer: None,
            timed_out: true,
        };
        let json = serde_json::to_string(&timed_out).unwrap();
        assert!(
            !json.contains("\"answer\":"),
            "timed-out answer must omit the answer field: {json}"
        );
        assert!(json.contains("\"timed_out\":true"));
    }

    #[test]
    fn note_event_round_trips_and_omits_absent_author() {
        let handoff = Event::Note {
            ts: 7,
            run_id: "r1".into(),
            from: Some(BackendId::Codex),
            kind: "handoff".into(),
            body: "auth module done; wire the CLI next".into(),
        };
        let json = serde_json::to_string(&handoff).unwrap();
        assert!(json.contains("\"event\":\"note\""), "got: {json}");
        assert!(json.contains("\"from\":\"codex\""));
        assert!(json.contains("\"kind\":\"handoff\""));
        assert!(matches!(
            serde_json::from_str::<Event>(&json).unwrap(),
            Event::Note {
                from: Some(BackendId::Codex),
                ..
            }
        ));

        // A note with no named author omits the `from` field entirely.
        let board = Event::Note {
            ts: 8,
            run_id: "r1".into(),
            from: None,
            kind: "board".into(),
            body: "shared constraint: keep files < 800 lines".into(),
        };
        let json = serde_json::to_string(&board).unwrap();
        assert!(
            !json.contains("\"from\":"),
            "absent author must be omitted: {json}"
        );
        // A legacy/absent `from` deserializes back to None via #[serde(default)].
        assert!(matches!(
            serde_json::from_str::<Event>(&json).unwrap(),
            Event::Note { from: None, .. }
        ));
    }

    #[test]
    fn logger_note_appends_to_event_log() {
        let _env = lock_env();
        let tmp = tempfile::tempdir().unwrap();
        // SAFETY: single-threaded under ENV_LOCK.
        unsafe {
            std::env::set_var("XDG_STATE_HOME", tmp.path());
        }
        let logger = RunLogger::adopt("run-note".to_string());
        logger.note(
            Some(BackendId::Claude),
            "handoff",
            "context for the next leg",
        );
        let contents = std::fs::read_to_string(tmp.path().join("agentpit/events.jsonl")).unwrap();
        assert!(contents.contains("\"event\":\"note\""), "got: {contents}");
        assert!(contents.contains("\"run_id\":\"run-note\""));
        assert!(contents.contains("context for the next leg"));
        unsafe {
            std::env::remove_var("XDG_STATE_HOME");
        }
    }

    #[test]
    fn ask_paths_reject_traversal_and_live_under_asks_dir() {
        assert!(ask_request_path("..").is_none());
        assert!(ask_request_path("a/b").is_none());
        assert!(ask_response_path("/abs").is_none());
        let req = ask_request_path("ask-1-2-3").unwrap();
        assert!(
            req.ends_with("asks/ask-1-2-3.json"),
            "got: {}",
            req.display()
        );
        let resp = ask_response_path("ask-1-2-3").unwrap();
        assert!(
            resp.ends_with("asks/ask-1-2-3.response.json"),
            "got: {}",
            resp.display()
        );
    }

    #[test]
    fn ask_tokens_are_unique_safe_and_prefixed() {
        let a = next_ask_token();
        let b = next_ask_token();
        assert_ne!(a, b);
        assert!(a.starts_with("ask-"));
        assert!(
            is_safe_log_component(&a),
            "ask token must be a safe component: {a}"
        );
    }
}
