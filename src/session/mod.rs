//! REPL-facing session layer over `agentpit_events::session` (design §3–§4).
//!
//! The events crate owns the durable log (schema, appends, branching, leases); this module
//! owns policy: when to continue natively vs compose context, what a composed prompt looks
//! like, and how a session tree is rendered for `/tree`.

use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, anyhow};

use crate::types::BackendId;
use agentpit_events::session::{
    ContextItem, LoadedEntry, SessionEntry, SessionLog, list_sessions, resolve_session,
    sessions_dir,
};
use agentpit_events::session_lease::{LeaseError, SessionLease};

pub use agentpit_events::session::{
    CWD_EXT_TYPE, ExchangeStatus, NewExchange, NewResult, RECOVERY_EXT_TYPE, SessionMeta,
    SummaryReason,
};

/// `ext_type` recording a summary of a branch that was just left (B5). Transparent to the
/// events crate's replay; folded into composed prompts by [`compose_prompt`]'s caller.
pub const BRANCH_SUMMARY_EXT_TYPE: &str = "agentpit.branch_summary";
/// Durable no-op node appended after every leaf move. Without it a branch with no summary
/// existed only in memory and reopening the session snapped back to the abandoned file tail.
pub const BRANCH_MOVE_EXT_TYPE: &str = "agentpit.branch_move";

/// A live, lease-held session. Shared through the REPL's cloneable state as
/// `Arc<Mutex<SessionRecorder>>` — lock per operation, never across an await.
pub struct SessionRecorder {
    log: SessionLog,
    _lease: SessionLease,
}

pub mod turn_engine;

pub type SharedRecorder = Arc<Mutex<SessionRecorder>>;

/// How the next exchange will carry the conversation (§4.3).
#[derive(Debug, Clone, PartialEq)]
pub enum TurnPlan {
    /// First turn of a fresh session: send the task as-is.
    Fresh,
    /// The backend resumes natively: send the task as-is plus `continue_from`.
    Native { continue_from: String },
    /// No native resume: send a composed prompt carrying summary + recent turns.
    Composed { prompt: String },
}

impl SessionRecorder {
    /// Assemble from an already-opened log and an already-held lease (worker startup and
    /// tests, which manage their own lease roots).
    pub fn from_parts(log: SessionLog, lease: SessionLease) -> SessionRecorder {
        SessionRecorder { log, _lease: lease }
    }

    /// Create a fresh session under the default sessions dir, lease held.
    pub fn create(cwd: &Path) -> Result<SessionRecorder> {
        let dir = sessions_dir();
        let log = SessionLog::create(&dir, &cwd.display().to_string(), None, None)
            .context("create session file")?;
        let lease = acquire_lease(log.path())?;
        Ok(SessionRecorder { log, _lease: lease })
    }

    /// Resume an existing session by id (exact, then unique prefix/suffix).
    pub fn resume(needle: &str) -> Result<SessionRecorder> {
        let dir = sessions_dir();
        let meta = resolve_session(&dir, needle).map_err(|e| match e {
            agentpit_events::session::ResolveError::NotFound => {
                anyhow!(
                    "no session matches \"{needle}\". Run `agentpit sessions` to list resumable ids."
                )
            }
            agentpit_events::session::ResolveError::Ambiguous(c) => {
                let ids: Vec<_> = c.iter().map(|m| m.session_id.clone()).collect();
                anyhow!(
                    "\"{needle}\" matches {} sessions ({}). Use more characters of the id.",
                    ids.len(),
                    ids.join(", ")
                )
            }
        })?;
        let lease = acquire_lease(&meta.path)?;
        let log = SessionLog::open(&meta.path).context("open session file")?;
        Ok(SessionRecorder { log, _lease: lease })
    }

    /// Torn-line / repair warnings collected at load time.
    pub fn warnings(&self) -> &[String] {
        self.log.warnings()
    }

    pub fn session_id(&self) -> &str {
        self.log.session_id()
    }

    /// The short id shown in prompts and hints (uuidv7 tail — unique enough to resolve).
    pub fn short_id(&self) -> String {
        let id = self.log.session_id();
        id[id.len().saturating_sub(12)..].to_string()
    }

    pub fn path(&self) -> &Path {
        self.log.path()
    }

    /// The session's CURRENT working directory: the latest journaled `/cwd` change on the
    /// active branch, falling back to the header entry.
    pub fn cwd_string(&self) -> String {
        self.log.effective_cwd().to_string()
    }

    /// Journal a `/cwd` change (an `ext` entry — transparent to replay) so resume and
    /// daemon attach land in the directory the user last chose, not the original one.
    pub fn record_cwd_change(&mut self, cwd: &str) -> Result<()> {
        self.log
            .append_ext(CWD_EXT_TYPE, serde_json::json!({ "cwd": cwd }))?;
        Ok(())
    }

    /// Mark exchanges that never got a result (a previous writer died mid-dispatch) and
    /// return one human-readable note per marked exchange. Idempotent across resumes.
    pub fn mark_interrupted(&mut self) -> Vec<String> {
        let interrupted = self.log.interrupted_exchanges();
        let mut notes = Vec::new();
        for exchange_id in interrupted {
            let _ = self.log.append_ext(
                RECOVERY_EXT_TYPE,
                serde_json::json!({
                    "exchange_id": exchange_id,
                    "note": "previous writer died mid-exchange; outcome uncertain",
                }),
            );
            notes.push(format!(
                "exchange {exchange_id} was interrupted in a previous run; its side effects may be incomplete"
            ));
        }
        notes
    }

    /// Decide how the next turn for `backend` travels (§4.3): native when the adapter can
    /// resume and a ref exists on this branch; composed when there is history but no native
    /// path; fresh otherwise.
    pub fn plan_turn(
        &self,
        backend: BackendId,
        supports_native: bool,
        task: &str,
        compose_window: usize,
    ) -> TurnPlan {
        // Native resume, UNLESS the last native attempt for this backend already errored:
        // a stale ref (expired session) would fail identically forever, so fall through to
        // a composed retry once, which records a fresh ref and lets native resume again (H1).
        if supports_native
            && !self.log.last_native_failed(backend.as_str())
            && let Some(r) = self.log.last_backend_ref(backend.as_str())
        {
            return TurnPlan::Native {
                continue_from: r.to_string(),
            };
        }
        let items = self.log.context();
        if items.is_empty() {
            return TurnPlan::Fresh;
        }
        TurnPlan::Composed {
            prompt: compose_prompt(&items, self.branch_notes(), compose_window, task),
        }
    }

    pub fn record_user(&mut self, text: &str) -> Result<String> {
        Ok(self.log.append_user(text)?)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn record_route(
        &mut self,
        tool: &str,
        category: Option<&str>,
        confidence: Option<f32>,
        backend: BackendId,
        reason: &str,
    ) -> Result<()> {
        self.log
            .append_route(tool, category, confidence, backend.as_str(), reason)?;
        Ok(())
    }

    pub fn record_exchange(&mut self, x: NewExchange<'_>) -> Result<String> {
        Ok(self.log.append_exchange(x)?)
    }

    pub fn record_result(&mut self, exchange_id: &str, r: NewResult<'_>) -> Result<()> {
        self.log.append_result(exchange_id, r)?;
        Ok(())
    }

    pub fn record_switch(&mut self, from: BackendId, to: BackendId) -> Result<()> {
        self.log.append_switch(from.as_str(), to.as_str())?;
        Ok(())
    }

    /// Log one orchestration-REPL cell (§10.7): the code and its outcome — never the
    /// deno heap, which is rebuildable only through `store` by design.
    pub fn append_repl_cell(
        &mut self,
        code: &str,
        ok: bool,
        detail: &str,
        duration_ms: u64,
    ) -> Result<()> {
        self.log.append_ext(
            crate::orchestrate::REPL_CELL_EXT_TYPE,
            serde_json::json!({
                "code": code,
                "ok": ok,
                "detail": detail,
                "duration_ms": duration_ms,
            }),
        )?;
        Ok(())
    }

    /// Compact: fold history into `summary_text`, keeping the last complete exchange.
    /// Replay afterwards = summary + the latest user turn onward — anchoring at the raw
    /// leaf (normally the assistant's result) would strand that answer without the
    /// question that produced it.
    pub fn record_summary(&mut self, summary_text: &str, reason: SummaryReason) -> Result<()> {
        let first_kept = self
            .log
            .last_user_id()
            .unwrap_or_else(|| self.log.leaf_id().to_string());
        self.log.append_summary(summary_text, &first_kept, reason)?;
        Ok(())
    }

    /// Whether `id` names an entry in this session — a cheap pre-check so callers can
    /// validate a `/branch` target BEFORE paying for a summarization LLM call (L4).
    pub fn has_entry(&self, id: &str) -> bool {
        self.log.entry(id).is_some()
    }

    /// Move the leaf to `target_id`, optionally leaving a summary of the abandoned branch
    /// (B5). The summary is an `ext` on the NEW path — transparent to replay, folded into
    /// composed prompts as "notes".
    pub fn branch(&mut self, target_id: &str, left_branch_summary: Option<&str>) -> Result<()> {
        let from = self.log.leaf_id().to_string();
        self.log.branch(target_id).map_err(|e| anyhow!(e))?;
        // Persist the selected path even when the user chooses "no summary". The marker is
        // transparent to context replay but becomes the file tail, so reopen keeps this leaf.
        self.log.append_ext(
            BRANCH_MOVE_EXT_TYPE,
            serde_json::json!({ "from": from, "target": target_id }),
        )?;
        if let Some(text) = left_branch_summary {
            self.log.append_ext(
                BRANCH_SUMMARY_EXT_TYPE,
                serde_json::json!({ "from": from, "text": text }),
            )?;
        }
        Ok(())
    }

    /// Fork at `target_id` (or the current leaf) into a new session file; returns the new
    /// session's id. The live session stays on the current file.
    pub fn fork(&self, target_id: Option<&str>) -> Result<String> {
        let at = target_id.unwrap_or(self.log.leaf_id()).to_string();
        let forked = self.log.fork(&at, &sessions_dir())?;
        Ok(forked.session_id().to_string())
    }

    /// The current branch's conversation, for transcript display and summarization.
    pub fn context_items(&self) -> Vec<(String, String)> {
        self.log
            .context()
            .into_iter()
            .map(|item| match item {
                ContextItem::Summary(t) => ("summary".to_string(), t.to_string()),
                ContextItem::User(t) => ("user".to_string(), t.to_string()),
                ContextItem::Answer { backend, text } => (backend.to_string(), text.to_string()),
            })
            .collect()
    }

    /// Branch-summary notes on the current path (newest last), for prompt composition.
    fn branch_notes(&self) -> Vec<String> {
        self.log
            .path_from_root()
            .iter()
            .filter_map(|e| match e {
                LoadedEntry::Known(SessionEntry::Ext { ext_type, data, .. })
                    if ext_type == BRANCH_SUMMARY_EXT_TYPE =>
                {
                    data.get("text")
                        .and_then(|v| v.as_str())
                        .map(str::to_string)
                }
                _ => None,
            })
            .collect()
    }

    /// Render the whole tree as indented text lines for `/tree` (P1 data-layer UI; the TUI
    /// Tree View replaces this in T2). Shows conversation-shaped nodes only; `•` marks the
    /// current path and `←` the leaf.
    pub fn tree_lines(&self) -> Vec<String> {
        let entries = self.log.entries();
        // children in file order, root(s) = entries whose parent is absent/none.
        let mut children: Vec<(Option<&str>, &LoadedEntry)> = Vec::new();
        for e in entries {
            children.push((e.parent_id(), e));
        }
        let path = self.log.path_from_root();
        let on_path: std::collections::HashSet<&str> = path.iter().filter_map(|e| e.id()).collect();
        // The durable branch-move marker is deliberately hidden. Mark the latest visible
        // ancestor as the current point so `/tree` still has one `←` after reopen.
        let leaf = path
            .iter()
            .rev()
            .find(|entry| describe(entry).is_some())
            .and_then(|entry| entry.id())
            .unwrap_or(self.log.leaf_id());

        let mut lines = Vec::new();
        fn walk(
            parent: Option<&str>,
            depth: usize,
            children: &[(Option<&str>, &LoadedEntry)],
            on_path: &std::collections::HashSet<&str>,
            leaf: &str,
            lines: &mut Vec<String>,
        ) {
            for (p, e) in children {
                if *p != parent {
                    continue;
                }
                let Some(id) = e.id() else { continue };
                if let Some(line) = describe(e) {
                    let marker = if id == leaf {
                        "←"
                    } else if on_path.contains(id) {
                        "•"
                    } else {
                        " "
                    };
                    lines.push(format!("{} {}{} {}", marker, "  ".repeat(depth), id, line));
                }
                // Hidden nodes (route/exchange/…) keep their depth so the tree stays flat
                // where nothing visible branches.
                let next_depth = if describe(e).is_some() {
                    depth + 1
                } else {
                    depth
                };
                walk(Some(id), next_depth, children, on_path, leaf, lines);
            }
        }
        walk(None, 0, &children, &on_path, leaf, &mut lines);
        lines
    }
}

/// One display line for tree-visible entries; `None` hides the entry from `/tree`.
fn describe(e: &LoadedEntry) -> Option<String> {
    let trunc = |s: &str| {
        let one_line = s.split_whitespace().collect::<Vec<_>>().join(" ");
        let mut out: String = one_line.chars().take(56).collect();
        if one_line.chars().count() > 56 {
            out.push('…');
        }
        out
    };
    match e {
        LoadedEntry::Known(SessionEntry::Session { .. }) => Some("[session]".to_string()),
        LoadedEntry::Known(SessionEntry::User { text, .. }) => {
            Some(format!("[user] {}", trunc(text)))
        }
        LoadedEntry::Known(SessionEntry::ExchangeResult { answer, status, .. }) => Some(format!(
            "[{}] {}",
            serde_json::to_value(status)
                .ok()
                .and_then(|v| v.as_str().map(str::to_string))
                .unwrap_or_else(|| "result".into()),
            trunc(answer)
        )),
        LoadedEntry::Known(SessionEntry::Summary { text, .. }) => {
            Some(format!("[summary] {}", trunc(text)))
        }
        LoadedEntry::Known(SessionEntry::Label { text, .. }) => {
            Some(format!("[label] {}", trunc(text)))
        }
        _ => None,
    }
}

/// Compose the continuation prompt for backends without native resume (§4.3): latest
/// summary (already folded into `items` by replay) + the last `window` turns + branch
/// notes + the new request. Deterministic; bounded by `window`.
pub fn compose_prompt(
    items: &[ContextItem<'_>],
    branch_notes: Vec<String>,
    window: usize,
    task: &str,
) -> String {
    let mut out = String::from(
        "You are continuing an ongoing session. Prior context follows; respond to the new request at the end.\n",
    );
    if let Some(ContextItem::Summary(text)) = items.first() {
        out.push_str("\n## Session summary\n");
        out.push_str(text);
        out.push('\n');
    }
    // Keep the last `window` user↔answer pairs (2×window items of the non-summary tail).
    let turns: Vec<&ContextItem> = items
        .iter()
        .filter(|i| !matches!(i, ContextItem::Summary(_)))
        .collect();
    let keep = window.saturating_mul(2);
    let tail = &turns[turns.len().saturating_sub(keep)..];
    if !tail.is_empty() {
        out.push_str("\n## Recent turns\n");
        for item in tail {
            match item {
                ContextItem::User(t) => {
                    out.push_str("User: ");
                    out.push_str(t);
                    out.push('\n');
                }
                ContextItem::Answer { backend, text } => {
                    out.push_str(&format!("Assistant ({backend}): "));
                    out.push_str(text);
                    out.push('\n');
                }
                ContextItem::Summary(_) => {}
            }
        }
    }
    if !branch_notes.is_empty() {
        out.push_str("\n## Notes from abandoned branches\n");
        for note in &branch_notes {
            out.push_str("- ");
            out.push_str(note);
            out.push('\n');
        }
    }
    out.push_str("\n## New request\n");
    out.push_str(task);
    out
}

fn acquire_lease(path: &Path) -> Result<SessionLease> {
    SessionLease::acquire(path).map_err(|e| match e {
        LeaseError::Busy { pid } if pid > 0 => anyhow!(
            "this session is already open in another process (pid {pid}). \
             Close it there, or run `agentpit sessions` to pick a different session."
        ),
        LeaseError::Busy { .. } => anyhow!(
            "this session is already open in another process. \
             Close it there, or run `agentpit sessions` to pick a different session."
        ),
        LeaseError::Io(err) => anyhow!("session lease failed: {err}"),
    })
}

/// Newest-first metadata for `agentpit sessions`.
pub fn list_all() -> Vec<SessionMeta> {
    list_sessions(&sessions_dir())
}

/// Resolve a session reference for read-only commands (`sessions show/export`).
pub fn resolve(needle: &str) -> Result<SessionMeta> {
    resolve_session(&sessions_dir(), needle).map_err(|e| anyhow!("{e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn recorder_in(dir: &Path) -> SessionRecorder {
        let log = SessionLog::create(dir, "/w", None, None).unwrap();
        let lease = SessionLease::acquire_at(&dir.join("leases"), log.path()).unwrap();
        SessionRecorder { log, _lease: lease }
    }

    fn turn(rec: &mut SessionRecorder, q: &str, backend: &str, a: &str, r#ref: Option<&str>) {
        rec.record_user(q).unwrap();
        let e = rec
            .record_exchange(NewExchange {
                backend,
                transport: "exec",
                run_id: "r",
                model: None,
                effort: None,
                prompt: q,
                continue_from: None,
            })
            .unwrap();
        rec.record_result(
            &e,
            NewResult {
                status: ExchangeStatus::Ok,
                answer: a,
                exit_code: Some(0),
                duration_ms: 1,
                backend_session_ref: r#ref,
                raw_ref: None,
            },
        )
        .unwrap();
    }

    #[test]
    fn plan_fresh_native_and_composed() {
        let tmp = tempfile::tempdir().unwrap();
        let mut rec = recorder_in(tmp.path());

        // Empty session → fresh.
        assert_eq!(
            rec.plan_turn(BackendId::Claude, true, "hi", 4),
            TurnPlan::Fresh
        );

        turn(&mut rec, "q1", "claude", "a1", Some("sid-1"));
        // Native: adapter supports resume and a ref exists for this backend.
        assert_eq!(
            rec.plan_turn(BackendId::Claude, true, "q2", 4),
            TurnPlan::Native {
                continue_from: "sid-1".into()
            }
        );
        // Same history, but a backend with no ref on this branch → composed.
        match rec.plan_turn(BackendId::Codex, true, "q2", 4) {
            TurnPlan::Composed { prompt } => {
                assert!(prompt.contains("q1"));
                assert!(prompt.contains("a1"));
                assert!(prompt.contains("## New request\nq2"));
            }
            other => panic!("expected Composed, got {other:?}"),
        }
        // No native support → composed even with a ref.
        assert!(matches!(
            rec.plan_turn(BackendId::Claude, false, "q2", 4),
            TurnPlan::Composed { .. }
        ));
    }

    #[test]
    fn compose_window_bounds_included_turns() {
        let tmp = tempfile::tempdir().unwrap();
        let mut rec = recorder_in(tmp.path());
        for i in 0..6 {
            turn(&mut rec, &format!("q{i}"), "agy", &format!("a{i}"), None);
        }
        let TurnPlan::Composed { prompt } = rec.plan_turn(BackendId::Antigravity, false, "next", 2)
        else {
            panic!("expected Composed");
        };
        // Window 2 → last two turns only.
        assert!(prompt.contains("q5") && prompt.contains("a5"));
        assert!(prompt.contains("q4") && prompt.contains("a4"));
        assert!(!prompt.contains("q3"));
        assert!(!prompt.contains("q0"));
    }

    #[test]
    fn branch_with_summary_feeds_composition_notes() {
        let tmp = tempfile::tempdir().unwrap();
        let mut rec = recorder_in(tmp.path());
        let anchor = rec.record_user("base").unwrap();
        turn(&mut rec, "try A", "codex", "A failed", None);
        rec.branch(&anchor, Some("approach A hit a dead end"))
            .unwrap();

        let TurnPlan::Composed { prompt } = rec.plan_turn(BackendId::Codex, false, "try B", 4)
        else {
            panic!("expected Composed");
        };
        assert!(prompt.contains("abandoned branches"));
        assert!(prompt.contains("approach A hit a dead end"));
        // The abandoned branch's content is NOT replayed as turns.
        assert!(!prompt.contains("A failed"));
    }

    #[test]
    fn branch_without_summary_survives_reopen() {
        let tmp = tempfile::tempdir().unwrap();
        let mut rec = recorder_in(tmp.path());
        let anchor = rec.record_user("base").unwrap();
        turn(&mut rec, "abandoned", "codex", "old answer", None);
        rec.branch(&anchor, None).unwrap();

        let path = rec.path().to_path_buf();
        drop(rec);
        let log = SessionLog::open(&path).unwrap();
        let lease = SessionLease::acquire_at(&tmp.path().join("leases"), &path).unwrap();
        let reopened = SessionRecorder::from_parts(log, lease);
        let context = reopened.context_items();
        assert_eq!(context, vec![("user".to_string(), "base".to_string())]);
        assert!(!context.iter().any(|(_, text)| text == "old answer"));
        let leaves: Vec<_> = reopened
            .tree_lines()
            .into_iter()
            .filter(|line| line.starts_with('←'))
            .collect();
        assert_eq!(leaves.len(), 1);
        assert!(leaves[0].contains("base"));
    }

    #[test]
    fn record_summary_folds_history_at_the_leaf() {
        let tmp = tempfile::tempdir().unwrap();
        let mut rec = recorder_in(tmp.path());
        turn(&mut rec, "old", "codex", "old answer", None);
        turn(&mut rec, "recent", "codex", "recent answer", None);
        rec.record_summary("compressed history", SummaryReason::Manual)
            .unwrap();

        let items = rec.context_items();
        assert_eq!(items[0].0, "summary");
        assert!(items.iter().any(|(_, t)| t == "recent answer"));
        assert!(
            items.iter().any(|(_, t)| t == "recent"),
            "the kept answer must keep its QUESTION too — an orphan assistant answer \
             reads as context noise: {items:?}"
        );
        assert!(!items.iter().any(|(_, t)| t == "old answer"));
    }

    #[test]
    fn cwd_changes_are_journaled_and_survive_resume() {
        let tmp = tempfile::tempdir().unwrap();
        let mut rec = recorder_in(tmp.path());
        assert_eq!(rec.cwd_string(), "/w");
        rec.record_cwd_change("/elsewhere").unwrap();
        assert_eq!(rec.cwd_string(), "/elsewhere");

        // A fresh open (what resume and daemon attach do) sees the journaled directory.
        let path = rec.path().to_path_buf();
        drop(rec); // releases the lease
        let log = SessionLog::open(&path).unwrap();
        let lease = SessionLease::acquire_at(&tmp.path().join("leases"), &path).unwrap();
        let reopened = SessionRecorder::from_parts(log, lease);
        assert_eq!(reopened.cwd_string(), "/elsewhere");
    }

    #[test]
    fn tree_lines_mark_path_and_leaf_and_hide_plumbing() {
        let tmp = tempfile::tempdir().unwrap();
        let mut rec = recorder_in(tmp.path());
        let u1 = rec.record_user("first").unwrap();
        turn(&mut rec, "second", "codex", "answer two", None);
        rec.branch(&u1, None).unwrap();
        rec.record_user("alternative").unwrap();

        let lines = rec.tree_lines();
        let joined = lines.join("\n");
        assert!(joined.contains("[session]"));
        assert!(joined.contains("[user] first"));
        assert!(joined.contains("[user] alternative"));
        assert!(joined.contains("[ok] answer two"));
        // Plumbing entries are hidden.
        assert!(!joined.contains("[exchange]"));
        // Exactly one leaf marker, on the alternative branch.
        let leaf_lines: Vec<_> = lines.iter().filter(|l| l.starts_with('←')).collect();
        assert_eq!(leaf_lines.len(), 1);
        assert!(leaf_lines[0].contains("alternative"));
        // The abandoned tip is neither leaf nor on the current path.
        assert!(
            lines
                .iter()
                .any(|l| l.contains("answer two") && l.starts_with(' '))
        );
    }

    #[test]
    fn fork_returns_new_resumable_id() {
        let tmp = tempfile::tempdir().unwrap();
        // Route sessions_dir writes into a temp state dir? fork() writes to the REAL
        // sessions_dir — so exercise SessionLog::fork through the recorder against an
        // explicit dir instead.
        let mut rec = recorder_in(tmp.path());
        turn(&mut rec, "q", "codex", "a", None);
        let forked = rec.log.fork(rec.log.leaf_id(), tmp.path()).unwrap();
        assert_ne!(forked.session_id(), rec.session_id());
    }
}
