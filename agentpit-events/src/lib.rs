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
}

fn now_ms() -> u64 {
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
}
