//! Append-only JSONL session log — one file per session, branches included.
//!
//! This is the durable conversation layer designed in `docs/session-persistence-design.md`
//! (§2–§3, §6). Unlike `events.jsonl` (run-scoped telemetry, compacted and pruned), a
//! session file is the source of truth for a conversation and is never rewritten: every
//! mutation is an appended line, and branching moves an in-memory leaf pointer instead of
//! copying files.
//!
//! Schema principles:
//! - **Agent-agnostic**: `backend` is a free string and per-backend raw output stays out of
//!   this file (referenced via `raw_ref` into `runs/`), so new backends need no schema change.
//! - **`exchange`/`result` split**: the request is appended when a dispatch starts and the
//!   result when it ends. An exchange without a result child is crash evidence — the JSONL
//!   itself is the recovery journal (§6.3).
//! - **Forward compatible**: unknown `type`s and all `ext` entries are preserved and carried
//!   through the chain, but never interpreted.
//! - **Torn-line tolerant**: an unparsable line (crash mid-append) is skipped with a warning;
//!   the file is never repaired in place (§6.1).

use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::state_dir;

/// Directory holding one `<session_id>.jsonl` per session.
pub fn sessions_dir() -> PathBuf {
    state_dir().join("sessions")
}

/// Schema version written into every session header. Bump only with a load-time migration.
pub const SESSION_SCHEMA_VERSION: u32 = 1;

/// Terminal status of one exchange with a backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExchangeStatus {
    Ok,
    Error,
    Timeout,
    Cancelled,
    Auth,
}

/// Lifecycle status recorded by a `state` entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    Active,
    Archived,
    Crash,
}

/// Why a summary was written.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SummaryReason {
    Manual,
    Auto,
}

/// One line of a session file. Every variant carries the envelope (`id`, `parent_id`, `ts`);
/// the header (`session`) has no parent. `ts` is RFC3339 with millisecond precision.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionEntry {
    /// Header — always the first line of the file.
    Session {
        id: String,
        ts: String,
        v: u32,
        session_id: String,
        cwd: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        /// Source file path when this session was forked/cloned from another.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent_session: Option<String>,
    },
    User {
        id: String,
        parent_id: String,
        ts: String,
        text: String,
    },
    /// Routing decision for the following exchange. Transparent for context building.
    Route {
        id: String,
        parent_id: String,
        ts: String,
        tool: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        category: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        confidence: Option<f32>,
        backend: String,
        reason: String,
    },
    /// A request sent to a backend, appended when the dispatch STARTS.
    Exchange {
        id: String,
        parent_id: String,
        ts: String,
        backend: String,
        transport: String,
        /// Correlation key into `events.jsonl` / `runs/<run_id>/`.
        run_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        effort: Option<String>,
        prompt: String,
        /// The `backend_session_ref` this dispatch resumed from, when native continuation ran.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        continue_from: Option<String>,
    },
    /// The outcome of an exchange, appended when the dispatch ENDS. `parent_id` is always
    /// the exchange entry — a missing result child marks an interrupted exchange.
    #[serde(rename = "result")]
    ExchangeResult {
        id: String,
        parent_id: String,
        ts: String,
        status: ExchangeStatus,
        answer: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        exit_code: Option<i32>,
        duration_ms: u64,
        /// Opaque continuation token (claude session id, codex thread id, …). Only the
        /// owning adapter interprets it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        backend_session_ref: Option<String>,
        /// Relative path (under the state dir) of the captured raw stream. Best-effort:
        /// `runs/` is pruned, so this may dangle later while `answer`/`status` survive here.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        raw_ref: Option<String>,
    },
    /// Active-backend switch. Transparent for context building.
    Switch {
        id: String,
        parent_id: String,
        ts: String,
        from: String,
        to: String,
    },
    /// Compaction point: replay keeps `text` plus everything from `first_kept_id` onward.
    Summary {
        id: String,
        parent_id: String,
        ts: String,
        text: String,
        first_kept_id: String,
        reason: SummaryReason,
    },
    State {
        id: String,
        parent_id: String,
        ts: String,
        status: SessionStatus,
    },
    /// Names a node (`target_id`, defaulting to this entry's parent). Transparent.
    Label {
        id: String,
        parent_id: String,
        ts: String,
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        target_id: Option<String>,
    },
    /// Extension escape hatch — preserved and chained, never interpreted by replay.
    Ext {
        id: String,
        parent_id: String,
        ts: String,
        ext_type: String,
        data: serde_json::Value,
    },
}

impl SessionEntry {
    pub fn id(&self) -> &str {
        match self {
            SessionEntry::Session { id, .. }
            | SessionEntry::User { id, .. }
            | SessionEntry::Route { id, .. }
            | SessionEntry::Exchange { id, .. }
            | SessionEntry::ExchangeResult { id, .. }
            | SessionEntry::Switch { id, .. }
            | SessionEntry::Summary { id, .. }
            | SessionEntry::State { id, .. }
            | SessionEntry::Label { id, .. }
            | SessionEntry::Ext { id, .. } => id,
        }
    }

    pub fn parent_id(&self) -> Option<&str> {
        match self {
            SessionEntry::Session { .. } => None,
            SessionEntry::User { parent_id, .. }
            | SessionEntry::Route { parent_id, .. }
            | SessionEntry::Exchange { parent_id, .. }
            | SessionEntry::ExchangeResult { parent_id, .. }
            | SessionEntry::Switch { parent_id, .. }
            | SessionEntry::Summary { parent_id, .. }
            | SessionEntry::State { parent_id, .. }
            | SessionEntry::Label { parent_id, .. }
            | SessionEntry::Ext { parent_id, .. } => Some(parent_id),
        }
    }
}

/// A loaded line: either a schema-known entry or a forward-compat unknown one (an object
/// this build can't type but which still carries `id`/`parent_id` and chains normally).
// Not boxing `Known`: the size gap only wastes memory on `Unknown` entries, which exist
// solely for forward compatibility (normally zero per file), while boxing would cost a
// nested match at every one of the many `Known(SessionEntry::…)` pattern sites.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone)]
pub enum LoadedEntry {
    Known(SessionEntry),
    Unknown(serde_json::Value),
}

impl LoadedEntry {
    pub fn id(&self) -> Option<&str> {
        match self {
            LoadedEntry::Known(e) => Some(e.id()),
            LoadedEntry::Unknown(v) => v.get("id").and_then(|x| x.as_str()),
        }
    }

    pub fn parent_id(&self) -> Option<&str> {
        match self {
            LoadedEntry::Known(e) => e.parent_id(),
            LoadedEntry::Unknown(v) => v.get("parent_id").and_then(|x| x.as_str()),
        }
    }
}

/// Fields for [`SessionLog::append_exchange`].
pub struct NewExchange<'a> {
    pub backend: &'a str,
    pub transport: &'a str,
    pub run_id: &'a str,
    pub model: Option<&'a str>,
    pub effort: Option<&'a str>,
    pub prompt: &'a str,
    pub continue_from: Option<&'a str>,
}

/// Fields for [`SessionLog::append_result`].
pub struct NewResult<'a> {
    pub status: ExchangeStatus,
    pub answer: &'a str,
    pub exit_code: Option<i32>,
    pub duration_ms: u64,
    pub backend_session_ref: Option<&'a str>,
    pub raw_ref: Option<&'a str>,
}

/// Context item produced by replay (§3.3): what a session "said" so far, for transcript
/// seeding and for composed continuation of backends without native resume.
#[derive(Debug, Clone, PartialEq)]
pub enum ContextItem<'a> {
    Summary(&'a str),
    User(&'a str),
    Answer { backend: &'a str, text: &'a str },
}

/// `ext_type` of the marker appended when an interrupted exchange (no result child) is
/// discovered on load; carrying `data.exchange_id`. Written once per interrupted exchange.
pub const RECOVERY_EXT_TYPE: &str = "agentpit.recovery";

/// One session file held open for appending. The single-writer discipline is enforced one
/// level up by [`crate::session_lease`]; this type itself assumes it is the only writer.
pub struct SessionLog {
    path: PathBuf,
    entries: Vec<LoadedEntry>,
    /// id → index into `entries`. First occurrence wins on (never-expected) duplicates.
    index: HashMap<String, usize>,
    /// Load-time repairs for dangling parents: child id → substitute parent id (§6.1).
    /// In-memory only; the file is never rewritten.
    repaired_parents: HashMap<String, String>,
    /// The current leaf — where the next append attaches. In-memory only: after a plain
    /// load it is the file's last entry, so an un-followed `branch()` does not survive a
    /// process restart (accepted, §3.1).
    leaf_id: String,
    session_id: String,
    warnings: Vec<String>,
}

fn now_rfc3339() -> String {
    humantime::format_rfc3339_millis(SystemTime::now()).to_string()
}

impl SessionLog {
    /// Create a new session file under `dir` and write its header. The file is named
    /// `<session_id>.jsonl` with a UUIDv7 session id (time-sortable listings for free).
    pub fn create(
        dir: &Path,
        cwd: &str,
        title: Option<&str>,
        parent_session: Option<&str>,
    ) -> std::io::Result<SessionLog> {
        fs::create_dir_all(dir)?;
        let session_id = uuid::Uuid::now_v7().to_string();
        let path = dir.join(format!("{session_id}.jsonl"));
        let header_id = random_entry_id(&HashMap::new());
        let header = SessionEntry::Session {
            id: header_id.clone(),
            ts: now_rfc3339(),
            v: SESSION_SCHEMA_VERSION,
            session_id: session_id.clone(),
            cwd: cwd.to_string(),
            title: title.map(str::to_string),
            parent_session: parent_session.map(str::to_string),
        };
        let mut log = SessionLog {
            path,
            entries: Vec::new(),
            index: HashMap::new(),
            repaired_parents: HashMap::new(),
            leaf_id: header_id,
            session_id,
            warnings: Vec::new(),
        };
        log.write_line(&header, false)?;
        log.push_known(header);
        Ok(log)
    }

    /// Load an existing session file. Unparsable lines are skipped with a warning
    /// (torn-line tolerance); the file itself is never modified by loading.
    pub fn open(path: &Path) -> std::io::Result<SessionLog> {
        let content = fs::read_to_string(path)?;
        let mut entries: Vec<LoadedEntry> = Vec::new();
        let mut warnings: Vec<String> = Vec::new();
        for (lineno, line) in content.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<SessionEntry>(line) {
                Ok(e) => entries.push(LoadedEntry::Known(e)),
                Err(_) => match serde_json::from_str::<serde_json::Value>(line) {
                    Ok(v) if v.get("id").and_then(|x| x.as_str()).is_some() => {
                        entries.push(LoadedEntry::Unknown(v));
                    }
                    _ => warnings.push(format!(
                        "line {}: unparsable (torn or corrupt), skipped",
                        lineno + 1
                    )),
                },
            }
        }

        // The header must exist and be typed — without it there is no session identity.
        let session_id = entries
            .iter()
            .find_map(|e| match e {
                LoadedEntry::Known(SessionEntry::Session { session_id, .. }) => {
                    Some(session_id.clone())
                }
                _ => None,
            })
            .ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("{}: no session header entry", path.display()),
                )
            })?;

        let mut index: HashMap<String, usize> = HashMap::new();
        for (i, e) in entries.iter().enumerate() {
            if let Some(id) = e.id() {
                if index.contains_key(id) {
                    warnings.push(format!("duplicate entry id {id}, keeping the first"));
                } else {
                    index.insert(id.to_string(), i);
                }
            }
        }

        // Repair dangling parents in memory: reattach to the closest preceding entry (§6.1).
        let mut repaired_parents: HashMap<String, String> = HashMap::new();
        let mut prev_id: Option<String> = None;
        for e in &entries {
            let Some(id) = e.id() else { continue };
            if let Some(parent) = e.parent_id() {
                if !index.contains_key(parent) {
                    if let Some(prev) = &prev_id {
                        warnings.push(format!(
                            "entry {id}: parent {parent} missing, reattached to {prev}"
                        ));
                        repaired_parents.insert(id.to_string(), prev.clone());
                    } else {
                        warnings.push(format!(
                            "entry {id}: parent {parent} missing before any entry"
                        ));
                    }
                }
            }
            prev_id = Some(id.to_string());
        }

        let leaf_id = entries
            .iter()
            .rev()
            .find_map(|e| e.id().map(str::to_string))
            .expect("header exists, so at least one entry has an id");

        Ok(SessionLog {
            path: path.to_path_buf(),
            entries,
            index,
            repaired_parents,
            leaf_id,
            session_id,
            warnings,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    pub fn leaf_id(&self) -> &str {
        &self.leaf_id
    }

    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    pub fn entries(&self) -> &[LoadedEntry] {
        &self.entries
    }

    pub fn entry(&self, id: &str) -> Option<&LoadedEntry> {
        self.index.get(id).map(|&i| &self.entries[i])
    }

    /// Move the leaf to an existing entry. The next append will branch there. No file I/O.
    pub fn branch(&mut self, target_id: &str) -> Result<(), String> {
        if !self.index.contains_key(target_id) {
            return Err(format!("no entry {target_id} in this session"));
        }
        self.leaf_id = target_id.to_string();
        Ok(())
    }

    pub fn append_user(&mut self, text: &str) -> std::io::Result<String> {
        let (id, parent_id, ts) = self.envelope();
        let e = SessionEntry::User {
            id: id.clone(),
            parent_id,
            ts,
            text: text.to_string(),
        };
        self.append_entry(e, false)?;
        Ok(id)
    }

    pub fn append_route(
        &mut self,
        tool: &str,
        category: Option<&str>,
        confidence: Option<f32>,
        backend: &str,
        reason: &str,
    ) -> std::io::Result<String> {
        let (id, parent_id, ts) = self.envelope();
        let e = SessionEntry::Route {
            id: id.clone(),
            parent_id,
            ts,
            tool: tool.to_string(),
            category: category.map(str::to_string),
            confidence,
            backend: backend.to_string(),
            reason: reason.to_string(),
        };
        self.append_entry(e, false)?;
        Ok(id)
    }

    pub fn append_exchange(&mut self, x: NewExchange<'_>) -> std::io::Result<String> {
        let (id, parent_id, ts) = self.envelope();
        let e = SessionEntry::Exchange {
            id: id.clone(),
            parent_id,
            ts,
            backend: x.backend.to_string(),
            transport: x.transport.to_string(),
            run_id: x.run_id.to_string(),
            model: x.model.map(str::to_string),
            effort: x.effort.map(str::to_string),
            prompt: x.prompt.to_string(),
            continue_from: x.continue_from.map(str::to_string),
        };
        self.append_entry(e, false)?;
        Ok(id)
    }

    /// Append the result of `exchange_id`. Its parent is the exchange itself (not the
    /// current leaf), and the write is fsynced — this is the durability checkpoint.
    pub fn append_result(
        &mut self,
        exchange_id: &str,
        r: NewResult<'_>,
    ) -> std::io::Result<String> {
        if !self.index.contains_key(exchange_id) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("no exchange {exchange_id} in this session"),
            ));
        }
        let id = random_entry_id(&self.index);
        let e = SessionEntry::ExchangeResult {
            id: id.clone(),
            parent_id: exchange_id.to_string(),
            ts: now_rfc3339(),
            status: r.status,
            answer: r.answer.to_string(),
            exit_code: r.exit_code,
            duration_ms: r.duration_ms,
            backend_session_ref: r.backend_session_ref.map(str::to_string),
            raw_ref: r.raw_ref.map(str::to_string),
        };
        self.append_entry(e, true)?;
        Ok(id)
    }

    pub fn append_switch(&mut self, from: &str, to: &str) -> std::io::Result<String> {
        let (id, parent_id, ts) = self.envelope();
        let e = SessionEntry::Switch {
            id: id.clone(),
            parent_id,
            ts,
            from: from.to_string(),
            to: to.to_string(),
        };
        self.append_entry(e, false)?;
        Ok(id)
    }

    /// Append a compaction summary (fsynced — the other durability checkpoint).
    pub fn append_summary(
        &mut self,
        text: &str,
        first_kept_id: &str,
        reason: SummaryReason,
    ) -> std::io::Result<String> {
        let (id, parent_id, ts) = self.envelope();
        let e = SessionEntry::Summary {
            id: id.clone(),
            parent_id,
            ts,
            text: text.to_string(),
            first_kept_id: first_kept_id.to_string(),
            reason,
        };
        self.append_entry(e, true)?;
        Ok(id)
    }

    pub fn append_state(&mut self, status: SessionStatus) -> std::io::Result<String> {
        let (id, parent_id, ts) = self.envelope();
        let e = SessionEntry::State {
            id: id.clone(),
            parent_id,
            ts,
            status,
        };
        self.append_entry(e, false)?;
        Ok(id)
    }

    pub fn append_label(&mut self, text: &str, target_id: Option<&str>) -> std::io::Result<String> {
        let (id, parent_id, ts) = self.envelope();
        let e = SessionEntry::Label {
            id: id.clone(),
            parent_id,
            ts,
            text: text.to_string(),
            target_id: target_id.map(str::to_string),
        };
        self.append_entry(e, false)?;
        Ok(id)
    }

    pub fn append_ext(
        &mut self,
        ext_type: &str,
        data: serde_json::Value,
    ) -> std::io::Result<String> {
        let (id, parent_id, ts) = self.envelope();
        let e = SessionEntry::Ext {
            id: id.clone(),
            parent_id,
            ts,
            ext_type: ext_type.to_string(),
            data,
        };
        self.append_entry(e, false)?;
        Ok(id)
    }

    /// The root→leaf chain of the CURRENT branch (repaired parents applied).
    pub fn path_from_root(&self) -> Vec<&LoadedEntry> {
        let mut chain: Vec<&LoadedEntry> = Vec::new();
        let mut cursor: Option<&str> = Some(self.leaf_id.as_str());
        // Cycle guard: repairs and hand-edited files could theoretically loop.
        let mut hops = 0usize;
        while let Some(id) = cursor {
            if hops > self.entries.len() {
                break;
            }
            hops += 1;
            let Some(entry) = self.entry(id) else { break };
            chain.push(entry);
            cursor = self.effective_parent(id);
        }
        chain.reverse();
        chain
    }

    fn effective_parent(&self, id: &str) -> Option<&str> {
        if let Some(sub) = self.repaired_parents.get(id) {
            return Some(sub.as_str());
        }
        self.entry(id).and_then(|e| e.parent_id())
    }

    /// Replay the current branch into context items (§3.3): the latest summary (if any)
    /// followed by kept user/answer items. Transparent entries are dropped.
    pub fn context(&self) -> Vec<ContextItem<'_>> {
        let path = self.path_from_root();
        // Latest summary on the path wins.
        let summary = path.iter().rev().find_map(|e| match e {
            LoadedEntry::Known(SessionEntry::Summary {
                text,
                first_kept_id,
                ..
            }) => Some((text.as_str(), first_kept_id.as_str())),
            _ => None,
        });

        let mut items: Vec<ContextItem<'_>> = Vec::new();
        let mut keeping = summary.is_none();
        if let Some((text, _)) = summary {
            items.push(ContextItem::Summary(text));
        }
        for e in &path {
            if let (false, Some((_, first_kept))) = (keeping, summary) {
                if e.id() == Some(first_kept) {
                    keeping = true;
                }
            }
            if !keeping {
                continue;
            }
            match e {
                LoadedEntry::Known(SessionEntry::User { text, .. }) => {
                    items.push(ContextItem::User(text));
                }
                LoadedEntry::Known(SessionEntry::ExchangeResult {
                    parent_id,
                    answer,
                    status,
                    ..
                }) => {
                    // Only successful answers become context. An error dump or an empty
                    // cancellation must never be replayed to the next backend as if the
                    // assistant had said it (M3).
                    if *status != ExchangeStatus::Ok {
                        continue;
                    }
                    let backend = match self.entry(parent_id) {
                        Some(LoadedEntry::Known(SessionEntry::Exchange { backend, .. })) => {
                            backend.as_str()
                        }
                        _ => "",
                    };
                    items.push(ContextItem::Answer {
                        backend,
                        text: answer,
                    });
                }
                _ => {}
            }
        }
        items
    }

    /// The most recent `backend_session_ref` on the current branch for `backend`, for
    /// native continuation. A `switch` does not reset it — refs are per-backend.
    pub fn last_backend_ref(&self, backend: &str) -> Option<&str> {
        let path = self.path_from_root();
        path.iter().rev().find_map(|e| match e {
            LoadedEntry::Known(SessionEntry::ExchangeResult {
                parent_id,
                backend_session_ref: Some(r),
                ..
            }) => match self.entry(parent_id) {
                Some(LoadedEntry::Known(SessionEntry::Exchange { backend: b, .. }))
                    if b == backend =>
                {
                    Some(r.as_str())
                }
                _ => None,
            },
            _ => None,
        })
    }

    /// True when the most recent exchange for `backend` on the current branch resumed
    /// natively (`continue_from` set) AND its result errored — the ref is stale (the
    /// backend expired the session), so the next turn must compose rather than resume the
    /// same dead ref again and fail identically forever (H1).
    pub fn last_native_failed(&self, backend: &str) -> bool {
        for e in self.path_from_root().iter().rev() {
            let LoadedEntry::Known(SessionEntry::ExchangeResult {
                parent_id, status, ..
            }) = e
            else {
                continue;
            };
            if let Some(LoadedEntry::Known(SessionEntry::Exchange {
                backend: b,
                continue_from,
                ..
            })) = self.entry(parent_id)
            {
                if b == backend {
                    // The most recent exchange for this backend decides.
                    return continue_from.is_some() && *status == ExchangeStatus::Error;
                }
            }
        }
        false
    }

    /// Exchanges (anywhere in the tree) with no result child and no recovery marker yet —
    /// evidence of a writer that died mid-dispatch (§5.4). Callers append the
    /// [`RECOVERY_EXT_TYPE`] marker so the interruption is reported exactly once.
    pub fn interrupted_exchanges(&self) -> Vec<String> {
        let mut has_result: HashMap<&str, bool> = HashMap::new();
        let mut marked: Vec<&str> = Vec::new();
        for e in &self.entries {
            match e {
                LoadedEntry::Known(SessionEntry::ExchangeResult { parent_id, .. }) => {
                    has_result.insert(parent_id.as_str(), true);
                }
                LoadedEntry::Known(SessionEntry::Ext { ext_type, data, .. })
                    if ext_type == RECOVERY_EXT_TYPE =>
                {
                    if let Some(x) = data.get("exchange_id").and_then(|v| v.as_str()) {
                        marked.push(x);
                    }
                }
                _ => {}
            }
        }
        self.entries
            .iter()
            .filter_map(|e| match e {
                LoadedEntry::Known(SessionEntry::Exchange { id, .. })
                    if !has_result.contains_key(id.as_str()) && !marked.contains(&id.as_str()) =>
                {
                    Some(id.clone())
                }
                _ => None,
            })
            .collect()
    }

    /// Fork the chain root→`up_to_id` into a NEW session file under `dest_dir` (§3.2).
    /// Entry ids and timestamps are preserved (they stay unique — the copy is a subset of
    /// one file), only the first copied entry is re-parented onto the new header, and
    /// `parent_session` records the source path. Cloning = forking at the current leaf.
    pub fn fork(&self, up_to_id: &str, dest_dir: &Path) -> std::io::Result<SessionLog> {
        if !self.index.contains_key(up_to_id) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("no entry {up_to_id} in this session"),
            ));
        }
        // Chain root→target, minus the source header (the fork gets its own).
        let mut chain: Vec<&LoadedEntry> = Vec::new();
        let mut cursor: Option<&str> = Some(up_to_id);
        let mut hops = 0usize;
        while let Some(id) = cursor {
            if hops > self.entries.len() {
                break;
            }
            hops += 1;
            let Some(entry) = self.entry(id) else { break };
            if !matches!(entry, LoadedEntry::Known(SessionEntry::Session { .. })) {
                chain.push(entry);
            }
            cursor = self.effective_parent(id);
        }
        chain.reverse();

        fs::create_dir_all(dest_dir)?;
        let session_id = uuid::Uuid::now_v7().to_string();
        let path = dest_dir.join(format!("{session_id}.jsonl"));
        // The header id must not collide with any preserved entry id.
        let copied_ids: HashMap<String, usize> = chain
            .iter()
            .enumerate()
            .filter_map(|(i, e)| e.id().map(|id| (id.to_string(), i)))
            .collect();
        let header_id = random_entry_id(&copied_ids);
        let (cwd, title) = (self.cwd().to_string(), self.title().map(str::to_string));
        let header = SessionEntry::Session {
            id: header_id.clone(),
            ts: now_rfc3339(),
            v: SESSION_SCHEMA_VERSION,
            session_id: session_id.clone(),
            cwd,
            title,
            parent_session: Some(self.path.display().to_string()),
        };
        let mut forked = SessionLog {
            path,
            entries: Vec::new(),
            index: HashMap::new(),
            repaired_parents: HashMap::new(),
            leaf_id: header_id.clone(),
            session_id,
            warnings: Vec::new(),
        };
        forked.write_line(&header, false)?;
        forked.push_known(header);

        for (i, entry) in chain.iter().enumerate() {
            match entry {
                LoadedEntry::Known(e) => {
                    let mut copy = (*e).clone();
                    if i == 0 {
                        set_parent(&mut copy, &header_id);
                    }
                    forked.write_line(&copy, false)?;
                    forked.push_known(copy);
                }
                LoadedEntry::Unknown(v) => {
                    let mut copy = v.clone();
                    if i == 0 {
                        if let Some(obj) = copy.as_object_mut() {
                            obj.insert(
                                "parent_id".to_string(),
                                serde_json::Value::String(header_id.clone()),
                            );
                        }
                    }
                    let line = serde_json::to_string(&copy).map_err(std::io::Error::other)?;
                    forked.write_raw_line(&line, false)?;
                    let id = copy
                        .get("id")
                        .and_then(|x| x.as_str())
                        .expect("chain entries have ids")
                        .to_string();
                    forked.entries.push(LoadedEntry::Unknown(copy));
                    forked.index.insert(id.clone(), forked.entries.len() - 1);
                    forked.leaf_id = id;
                }
            }
        }
        // Durability checkpoint: a fork that vanishes on power loss would surprise more
        // than a lost turn.
        forked.sync()?;
        Ok(forked)
    }

    /// The header's working directory.
    pub fn cwd(&self) -> &str {
        match self.entries.first() {
            Some(LoadedEntry::Known(SessionEntry::Session { cwd, .. })) => cwd,
            _ => "",
        }
    }

    /// The header's title, when one was set at creation/fork time.
    pub fn title(&self) -> Option<&str> {
        match self.entries.first() {
            Some(LoadedEntry::Known(SessionEntry::Session { title, .. })) => title.as_deref(),
            _ => None,
        }
    }

    fn sync(&self) -> std::io::Result<()> {
        OpenOptions::new()
            .append(true)
            .open(&self.path)
            .and_then(|f| f.sync_all())
    }

    fn envelope(&self) -> (String, String, String) {
        (
            random_entry_id(&self.index),
            self.leaf_id.clone(),
            now_rfc3339(),
        )
    }

    fn append_entry(&mut self, entry: SessionEntry, durable: bool) -> std::io::Result<()> {
        self.write_line(&entry, durable)?;
        self.push_known(entry);
        Ok(())
    }

    fn push_known(&mut self, entry: SessionEntry) {
        let id = entry.id().to_string();
        self.entries.push(LoadedEntry::Known(entry));
        self.index.insert(id.clone(), self.entries.len() - 1);
        self.leaf_id = id;
    }

    fn write_line(&self, entry: &SessionEntry, durable: bool) -> std::io::Result<()> {
        let line = serde_json::to_string(entry).map_err(std::io::Error::other)?;
        self.write_raw_line(&line, durable)
    }

    /// One line = one `write_all` on an append-mode handle: effectively atomic for normal
    /// line sizes, and a torn tail on crash is tolerated by [`SessionLog::open`]. `durable`
    /// adds an fsync (results and summaries — the checkpoints worth surviving power loss).
    fn write_raw_line(&self, line: &str, durable: bool) -> std::io::Result<()> {
        let mut f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        f.write_all(line.as_bytes())?;
        f.write_all(b"\n")?;
        f.flush()?;
        if durable {
            f.sync_all()?;
        }
        Ok(())
    }
}

/// Rewrite `entry`'s parent pointer (used when re-rooting the first forked entry).
fn set_parent(entry: &mut SessionEntry, new_parent: &str) {
    match entry {
        SessionEntry::Session { .. } => {}
        SessionEntry::User { parent_id, .. }
        | SessionEntry::Route { parent_id, .. }
        | SessionEntry::Exchange { parent_id, .. }
        | SessionEntry::ExchangeResult { parent_id, .. }
        | SessionEntry::Switch { parent_id, .. }
        | SessionEntry::Summary { parent_id, .. }
        | SessionEntry::State { parent_id, .. }
        | SessionEntry::Label { parent_id, .. }
        | SessionEntry::Ext { parent_id, .. } => *parent_id = new_parent.to_string(),
    }
}

/// A session file's identity, read from its header line only (cheap listing).
#[derive(Debug, Clone)]
pub struct SessionMeta {
    pub session_id: String,
    pub path: PathBuf,
    pub title: Option<String>,
    pub cwd: String,
    /// File mtime — "last activity" for listings.
    pub updated_at: SystemTime,
    pub size_bytes: u64,
}

/// List sessions under `dir`, newest first. Files without a readable header are skipped.
pub fn list_sessions(dir: &Path) -> Vec<SessionMeta> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<SessionMeta> = Vec::new();
    for entry in entries.filter_map(|e| e.ok()) {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        let Some(meta) = read_meta(&path) else {
            continue;
        };
        out.push(meta);
    }
    out.sort_by_key(|m| std::cmp::Reverse(m.updated_at));
    out
}

fn read_meta(path: &Path) -> Option<SessionMeta> {
    use std::io::BufRead;
    let f = fs::File::open(path).ok()?;
    let mut first = String::new();
    std::io::BufReader::new(f).read_line(&mut first).ok()?;
    let SessionEntry::Session {
        session_id,
        cwd,
        title,
        ..
    } = serde_json::from_str::<SessionEntry>(&first).ok()?
    else {
        return None;
    };
    let md = fs::metadata(path).ok()?;
    Some(SessionMeta {
        session_id,
        path: path.to_path_buf(),
        title,
        cwd,
        updated_at: md.modified().unwrap_or(UNIX_EPOCH),
        size_bytes: md.len(),
    })
}

#[derive(Debug)]
pub enum ResolveError {
    NotFound,
    /// Multiple candidates — never silently pick one (prime's discipline).
    Ambiguous(Vec<SessionMeta>),
}

impl std::fmt::Display for ResolveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ResolveError::NotFound => write!(f, "no session matches"),
            ResolveError::Ambiguous(c) => write!(f, "{} sessions match", c.len()),
        }
    }
}

impl std::error::Error for ResolveError {}

/// Resolve a user-supplied session reference: exact id first, then unique prefix/suffix.
pub fn resolve_session(dir: &Path, needle: &str) -> Result<SessionMeta, ResolveError> {
    let all = list_sessions(dir);
    if let Some(exact) = all.iter().find(|m| m.session_id == needle) {
        return Ok(exact.clone());
    }
    let partial: Vec<SessionMeta> = all
        .into_iter()
        .filter(|m| m.session_id.starts_with(needle) || m.session_id.ends_with(needle))
        .collect();
    match partial.len() {
        0 => Err(ResolveError::NotFound),
        1 => Ok(partial.into_iter().next().expect("len checked")),
        _ => Err(ResolveError::Ambiguous(partial)),
    }
}

/// An 8-hex random entry id, collision-checked against the in-memory index (same scheme
/// as prime-agent). Falls back to a full uuid on pathological collision streaks.
fn random_entry_id(index: &HashMap<String, usize>) -> String {
    for _ in 0..100 {
        let full = uuid::Uuid::new_v4().simple().to_string();
        let short = full[..8].to_string();
        if !index.contains_key(&short) {
            return short;
        }
    }
    uuid::Uuid::new_v4().simple().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seed(dir: &Path) -> SessionLog {
        SessionLog::create(dir, "/work", Some("t"), None).unwrap()
    }

    fn run_turn(log: &mut SessionLog, text: &str, answer: &str) -> (String, String) {
        let _u = log.append_user(text).unwrap();
        let e = log
            .append_exchange(NewExchange {
                backend: "codex",
                transport: "exec",
                run_id: "r-1",
                model: None,
                effort: None,
                prompt: text,
                continue_from: None,
            })
            .unwrap();
        let r = log
            .append_result(
                &e,
                NewResult {
                    status: ExchangeStatus::Ok,
                    answer,
                    exit_code: Some(0),
                    duration_ms: 5,
                    backend_session_ref: Some("thread-1"),
                    raw_ref: None,
                },
            )
            .unwrap();
        (e, r)
    }

    /// Append one native-continuation exchange for `backend` that ends in `status`.
    fn native_turn(log: &mut SessionLog, backend: &str, status: ExchangeStatus, answer: &str) {
        log.append_user("q").unwrap();
        let e = log
            .append_exchange(NewExchange {
                backend,
                transport: "exec",
                run_id: "r",
                model: None,
                effort: None,
                prompt: "q",
                continue_from: Some("old-ref"),
            })
            .unwrap();
        log.append_result(
            &e,
            NewResult {
                status,
                answer,
                exit_code: None,
                duration_ms: 1,
                backend_session_ref: (status == ExchangeStatus::Ok).then_some("fresh-ref"),
                raw_ref: None,
            },
        )
        .unwrap();
    }

    #[test]
    fn last_native_failed_detects_a_stale_ref_and_clears_after_success() {
        // H1: a native resume that errored must be visible so the planner falls back to
        // composed, and a subsequent success must clear the flag.
        let tmp = tempfile::tempdir().unwrap();
        let mut log = seed(tmp.path());
        assert!(
            !log.last_native_failed("codex"),
            "fresh session: no failure"
        );

        native_turn(&mut log, "codex", ExchangeStatus::Error, "boom");
        assert!(log.last_native_failed("codex"), "native error must be seen");
        // Only the failing backend is affected.
        assert!(!log.last_native_failed("claude"));

        native_turn(&mut log, "codex", ExchangeStatus::Ok, "recovered");
        assert!(
            !log.last_native_failed("codex"),
            "a later success clears the stale-ref flag"
        );
    }

    #[test]
    fn context_excludes_error_and_cancelled_results() {
        // M3: only successful answers replay into composed context.
        let tmp = tempfile::tempdir().unwrap();
        let mut log = seed(tmp.path());
        native_turn(&mut log, "codex", ExchangeStatus::Error, "error dump here");
        native_turn(&mut log, "codex", ExchangeStatus::Ok, "the real answer");
        let items = log.context();
        assert!(
            items.iter().any(
                |i| matches!(i, ContextItem::Answer { text, .. } if *text == "the real answer")
            ),
            "the successful answer is kept"
        );
        assert!(
            !items.iter().any(
                |i| matches!(i, ContextItem::Answer { text, .. } if text.contains("error dump"))
            ),
            "the error dump must not become an answer"
        );
    }

    #[test]
    fn create_open_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let log = seed(tmp.path());
        let sid = log.session_id().to_string();
        assert!(log.path().ends_with(format!("{sid}.jsonl")));

        let reopened = SessionLog::open(log.path()).unwrap();
        assert_eq!(reopened.session_id(), sid);
        assert!(reopened.warnings().is_empty());
        // Leaf after a fresh open = the last entry (the header).
        assert_eq!(reopened.leaf_id(), log.leaf_id());
    }

    #[test]
    fn chain_appends_and_replays_in_order() {
        let tmp = tempfile::tempdir().unwrap();
        let mut log = seed(tmp.path());
        log.append_route("rescue", Some("coding"), Some(0.9), "codex", "profile")
            .unwrap();
        let (_e, r) = run_turn(&mut log, "hello", "world");
        assert_eq!(log.leaf_id(), r);

        let reopened = SessionLog::open(log.path()).unwrap();
        assert_eq!(reopened.leaf_id(), r);
        let items = reopened.context();
        assert_eq!(
            items,
            vec![
                ContextItem::User("hello"),
                ContextItem::Answer {
                    backend: "codex",
                    text: "world"
                },
            ]
        );
        assert_eq!(reopened.last_backend_ref("codex"), Some("thread-1"));
        assert_eq!(reopened.last_backend_ref("claude"), None);
    }

    #[test]
    fn branch_moves_leaf_and_grows_a_second_child() {
        let tmp = tempfile::tempdir().unwrap();
        let mut log = seed(tmp.path());
        let u1 = log.append_user("first").unwrap();
        run_turn(&mut log, "unused", "old answer");

        log.branch(&u1).unwrap();
        let u2 = log.append_user("second try").unwrap();

        // In-memory: the new path is header → first → second try.
        let ids: Vec<_> = log.path_from_root().iter().filter_map(|e| e.id()).collect();
        assert_eq!(ids.last().copied(), Some(u2.as_str()));
        assert!(ids.contains(&u1.as_str()));
        let items = log.context();
        assert_eq!(
            items,
            vec![ContextItem::User("first"), ContextItem::User("second try")]
        );

        // On disk: all four+ lines still present — nothing was copied or deleted.
        let raw = std::fs::read_to_string(log.path()).unwrap();
        assert!(raw.contains("old answer"));
        assert!(raw.contains("second try"));

        // Reopen: leaf snaps to the file's LAST entry (u2 — it was appended last).
        let reopened = SessionLog::open(log.path()).unwrap();
        assert_eq!(reopened.leaf_id(), u2);
    }

    #[test]
    fn branch_to_unknown_id_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let mut log = seed(tmp.path());
        assert!(log.branch("zzzzzzzz").is_err());
    }

    #[test]
    fn torn_last_line_is_skipped_with_warning() {
        let tmp = tempfile::tempdir().unwrap();
        let mut log = seed(tmp.path());
        let u1 = log.append_user("kept").unwrap();
        // Simulate a crash mid-append: a truncated JSON tail.
        let mut raw = std::fs::read_to_string(log.path()).unwrap();
        raw.push_str("{\"type\":\"user\",\"id\":\"abcd");
        std::fs::write(log.path(), raw).unwrap();

        let reopened = SessionLog::open(log.path()).unwrap();
        assert_eq!(reopened.warnings().len(), 1);
        assert!(reopened.warnings()[0].contains("torn"));
        assert_eq!(reopened.leaf_id(), u1);
    }

    #[test]
    fn unknown_entry_type_is_preserved_and_chains() {
        let tmp = tempfile::tempdir().unwrap();
        let mut log = seed(tmp.path());
        let u1 = log.append_user("hi").unwrap();
        let mut raw = std::fs::read_to_string(log.path()).unwrap();
        raw.push_str(&format!(
            "{{\"type\":\"future_thing\",\"id\":\"ffff0001\",\"parent_id\":\"{u1}\",\"ts\":\"2026-01-01T00:00:00.000Z\",\"payload\":42}}\n"
        ));
        std::fs::write(log.path(), raw).unwrap();

        let mut reopened = SessionLog::open(log.path()).unwrap();
        assert!(reopened.warnings().is_empty(), "{:?}", reopened.warnings());
        // The unknown entry became the leaf and the next append chains through it.
        assert_eq!(reopened.leaf_id(), "ffff0001");
        let u2 = reopened.append_user("after unknown").unwrap();
        let ids: Vec<_> = reopened
            .path_from_root()
            .iter()
            .filter_map(|e| e.id())
            .collect();
        assert_eq!(ids.last().copied(), Some(u2.as_str()));
        assert!(ids.contains(&"ffff0001"));
        // Replay treats it as transparent.
        let items = reopened.context();
        assert_eq!(
            items,
            vec![ContextItem::User("hi"), ContextItem::User("after unknown")]
        );
    }

    #[test]
    fn summary_cuts_replay_at_first_kept_id() {
        let tmp = tempfile::tempdir().unwrap();
        let mut log = seed(tmp.path());
        run_turn(&mut log, "old question", "old answer");
        let u2 = log.append_user("new question").unwrap();
        let e2 = log
            .append_exchange(NewExchange {
                backend: "claude",
                transport: "exec",
                run_id: "r-2",
                model: None,
                effort: None,
                prompt: "new question",
                continue_from: None,
            })
            .unwrap();
        log.append_result(
            &e2,
            NewResult {
                status: ExchangeStatus::Ok,
                answer: "new answer",
                exit_code: Some(0),
                duration_ms: 3,
                backend_session_ref: None,
                raw_ref: None,
            },
        )
        .unwrap();
        log.append_summary("## Goal\ncompressed", &u2, SummaryReason::Manual)
            .unwrap();

        let items = log.context();
        assert_eq!(
            items,
            vec![
                ContextItem::Summary("## Goal\ncompressed"),
                ContextItem::User("new question"),
                ContextItem::Answer {
                    backend: "claude",
                    text: "new answer"
                },
            ]
        );
    }

    #[test]
    fn interrupted_exchange_detection_and_marker() {
        let tmp = tempfile::tempdir().unwrap();
        let mut log = seed(tmp.path());
        log.append_user("q").unwrap();
        let e1 = log
            .append_exchange(NewExchange {
                backend: "codex",
                transport: "exec",
                run_id: "r-3",
                model: None,
                effort: None,
                prompt: "q",
                continue_from: None,
            })
            .unwrap();
        // No result appended — the writer "crashed" here.
        let reopened = SessionLog::open(log.path()).unwrap();
        assert_eq!(reopened.interrupted_exchanges(), vec![e1.clone()]);

        let mut log = reopened;
        log.append_ext(
            RECOVERY_EXT_TYPE,
            serde_json::json!({ "exchange_id": e1, "note": "writer died mid-exchange" }),
        )
        .unwrap();
        assert!(log.interrupted_exchanges().is_empty());
        // And the marker survives a reload.
        let again = SessionLog::open(log.path()).unwrap();
        assert!(again.interrupted_exchanges().is_empty());
    }

    #[test]
    fn dangling_parent_is_repaired_in_memory() {
        let tmp = tempfile::tempdir().unwrap();
        let mut log = seed(tmp.path());
        let u1 = log.append_user("anchor").unwrap();
        let mut raw = std::fs::read_to_string(log.path()).unwrap();
        raw.push_str(
            "{\"type\":\"user\",\"id\":\"beefbeef\",\"parent_id\":\"gone0000\",\"ts\":\"2026-01-01T00:00:00.000Z\",\"text\":\"orphan\"}\n",
        );
        std::fs::write(log.path(), raw).unwrap();

        let reopened = SessionLog::open(log.path()).unwrap();
        assert!(
            reopened.warnings().iter().any(|w| w.contains("reattached")),
            "{:?}",
            reopened.warnings()
        );
        // The orphan is the leaf; its repaired chain reaches the anchor and the root.
        assert_eq!(reopened.leaf_id(), "beefbeef");
        let ids: Vec<_> = reopened
            .path_from_root()
            .iter()
            .filter_map(|e| e.id())
            .collect();
        assert!(ids.contains(&u1.as_str()));
        assert_eq!(ids.last().copied(), Some("beefbeef"));
    }

    #[test]
    fn result_for_unknown_exchange_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let mut log = seed(tmp.path());
        let err = log
            .append_result(
                "nope0000",
                NewResult {
                    status: ExchangeStatus::Ok,
                    answer: "x",
                    exit_code: None,
                    duration_ms: 1,
                    backend_session_ref: None,
                    raw_ref: None,
                },
            )
            .unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    }

    #[test]
    fn fork_copies_only_the_chain_and_rerooots_it() {
        let tmp = tempfile::tempdir().unwrap();
        let mut log = seed(tmp.path());
        let u1 = log.append_user("keep me").unwrap();
        let (_e1, r1) = run_turn(&mut log, "keep q", "keep a");
        // A second branch that must NOT be copied.
        log.branch(&u1).unwrap();
        log.append_user("abandoned branch").unwrap();

        let dest = tmp.path().join("forks");
        let forked = log.fork(&r1, &dest).unwrap();
        assert_ne!(forked.session_id(), log.session_id());
        assert_eq!(forked.title(), Some("t"));
        assert_eq!(forked.cwd(), "/work");

        let raw = std::fs::read_to_string(forked.path()).unwrap();
        assert!(raw.contains("keep q"));
        assert!(raw.contains("\"parent_session\""));
        assert!(!raw.contains("abandoned branch"));

        // The fork replays exactly the copied conversation and stays appendable.
        let mut reopened = SessionLog::open(forked.path()).unwrap();
        assert!(reopened.warnings().is_empty(), "{:?}", reopened.warnings());
        assert_eq!(
            reopened.context(),
            vec![
                ContextItem::User("keep me"),
                ContextItem::User("keep q"),
                ContextItem::Answer {
                    backend: "codex",
                    text: "keep a"
                },
            ]
        );
        reopened.append_user("continue in fork").unwrap();
        assert_eq!(reopened.context().len(), 4);
        // Continuation refs survive the copy.
        assert_eq!(reopened.last_backend_ref("codex"), Some("thread-1"));
    }

    #[test]
    fn fork_of_unknown_id_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let log = seed(tmp.path());
        assert!(log.fork("nope0000", tmp.path()).is_err());
    }

    #[test]
    fn list_and_resolve_sessions() {
        let tmp = tempfile::tempdir().unwrap();
        let a = SessionLog::create(tmp.path(), "/a", Some("first"), None).unwrap();
        let b = SessionLog::create(tmp.path(), "/b", None, None).unwrap();
        // Junk that must be skipped, not crash the listing.
        std::fs::write(tmp.path().join("junk.jsonl"), "not json\n").unwrap();
        std::fs::write(tmp.path().join("readme.txt"), "ignore").unwrap();

        let listed = list_sessions(tmp.path());
        assert_eq!(listed.len(), 2);
        let ids: Vec<_> = listed.iter().map(|m| m.session_id.as_str()).collect();
        assert!(ids.contains(&a.session_id()));
        assert!(ids.contains(&b.session_id()));

        // Exact match wins.
        let hit = resolve_session(tmp.path(), a.session_id()).unwrap();
        assert_eq!(hit.session_id, a.session_id());
        // Unique suffix resolves (uuidv7 tails differ).
        let tail = &b.session_id()[b.session_id().len() - 12..];
        let hit = resolve_session(tmp.path(), tail).unwrap();
        assert_eq!(hit.session_id, b.session_id());
        // A shared prefix is ambiguous, not a silent pick: uuidv7 ids created in the same
        // millisecond share a long prefix, so probe with a 4-char prefix common to both if
        // it exists, else assert NotFound behavior on garbage.
        let p4a = &a.session_id()[..4];
        if b.session_id().starts_with(p4a) {
            assert!(matches!(
                resolve_session(tmp.path(), p4a),
                Err(ResolveError::Ambiguous(_))
            ));
        }
        assert!(matches!(
            resolve_session(tmp.path(), "zzzz-not-a-session"),
            Err(ResolveError::NotFound)
        ));
    }

    #[test]
    fn wire_format_matches_design_examples() {
        let tmp = tempfile::tempdir().unwrap();
        let mut log = seed(tmp.path());
        run_turn(&mut log, "hello", "world");
        let raw = std::fs::read_to_string(log.path()).unwrap();
        let lines: Vec<&str> = raw.lines().collect();
        assert!(lines[0].contains("\"type\":\"session\""), "{}", lines[0]);
        assert!(lines[0].contains("\"v\":1"));
        assert!(lines[1].contains("\"type\":\"user\""));
        assert!(lines[2].contains("\"type\":\"exchange\""));
        assert!(lines[2].contains("\"backend\":\"codex\""));
        assert!(lines[2].contains("\"transport\":\"exec\""));
        assert!(lines[3].contains("\"type\":\"result\""));
        assert!(lines[3].contains("\"status\":\"ok\""));
        assert!(lines[3].contains("\"backend_session_ref\":\"thread-1\""));
        // Absent options are omitted from the wire, not serialized as null.
        assert!(!lines[3].contains("\"raw_ref\""));
    }
}
