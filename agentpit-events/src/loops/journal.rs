//! The append-only loop journal: `loops/<loop_id>/journal.jsonl` (design §9).
//!
//! Invariants:
//!
//! - **I1 single writer.** Every append happens under a lease on the loop directory
//!   (the session lease mechanism, keyed on the directory so the key is stable before the
//!   file exists).
//! - **I2 contiguity.** seq runs 1, 2, 3… with no gaps or duplicates; line 1 is
//!   `loop_created`.
//! - **I3 durable before visible.** A batch is one `write_all` + `sync_data`; only after
//!   it returns may the caller apply, publish, acknowledge, or start an effect.
//! - **I4 truncate on open.** Bytes after the last `'\n'` were never acknowledged, so a
//!   writer drops them before its first append — even if they parse. A reader just
//!   ignores them.
//! - **I5 no repair.** Mid-file damage or a seq discontinuity makes the journal read-only;
//!   nothing rewrites history.
//!
//! Readers need no lease: [`read_loop`] scans and folds. Note that I3 holds for what the
//! writer *publishes*; a concurrent file reader can see a line in the page cache before
//! its `sync_data` returns, so tailers should trust only seqs the writer announced.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use super::record::*;
use super::state::{replay, LoopState, ReadOnlyReason, Rejection};
use super::{journal_path, JOURNAL_FILE, MAX_LINE_BYTES, SCHEMA_MINOR};
use crate::session_lease::{process_start_id, LeaseError, SessionLease};

/// What a scan found wrong. Any issue makes the journal read-only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScanIssue {
    Unparseable {
        line: usize,
        offset: u64,
        error: String,
    },
    BadFirstRecord {
        detail: String,
    },
    SeqOutOfOrder {
        line: usize,
        expected: u64,
        found: u64,
    },
}

impl std::fmt::Display for ScanIssue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ScanIssue::Unparseable {
                line,
                offset,
                error,
            } => {
                write!(f, "line {line} (byte {offset}) is unreadable: {error}")
            }
            ScanIssue::BadFirstRecord { detail } => write!(f, "bad first record: {detail}"),
            ScanIssue::SeqOutOfOrder {
                line,
                expected,
                found,
            } => write!(f, "line {line} has seq {found}, expected {expected}"),
        }
    }
}

/// The complete lines of a journal, decoded.
#[derive(Debug, Clone, Default)]
pub struct Scan {
    pub records: Vec<LoadedRecord>,
    /// Bytes up to and including the last `'\n'`.
    pub complete_len: u64,
    /// Bytes after it (an unacknowledged, possibly torn, write).
    pub torn_tail_bytes: u64,
    pub issues: Vec<ScanIssue>,
}

impl Scan {
    pub fn is_clean(&self) -> bool {
        self.issues.is_empty()
    }

    /// The seq of the last record (0 when there is none).
    pub fn head_seq(&self) -> u64 {
        self.records.last().map_or(0, |r| r.seq)
    }

    /// Records after a cursor position.
    pub fn after(&self, seq: u64) -> &[LoadedRecord] {
        let start = self.records.partition_point(|r| r.seq <= seq);
        &self.records[start..]
    }
}

/// Decode a journal's bytes. Pure.
pub fn scan_bytes(bytes: &[u8]) -> Scan {
    let complete_len = bytes.iter().rposition(|b| *b == b'\n').map_or(0, |i| i + 1);
    let mut scan = Scan {
        complete_len: complete_len as u64,
        torn_tail_bytes: (bytes.len() - complete_len) as u64,
        ..Scan::default()
    };
    let mut offset = 0u64;
    for (i, raw) in bytes[..complete_len].split(|b| *b == b'\n').enumerate() {
        let line_no = i + 1;
        let line_offset = offset;
        offset += raw.len() as u64 + 1;
        if raw.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        let decoded = std::str::from_utf8(raw)
            .map_err(|e| e.to_string())
            .and_then(decode_line);
        let rec = match decoded {
            Ok(rec) => rec,
            Err(error) => {
                scan.issues.push(ScanIssue::Unparseable {
                    line: line_no,
                    offset: line_offset,
                    error,
                });
                continue;
            }
        };
        match scan.records.last() {
            None if rec.seq != 1 || rec.kind != "loop_created" => {
                scan.issues.push(ScanIssue::BadFirstRecord {
                    detail: format!("seq {} kind {}", rec.seq, rec.kind),
                })
            }
            Some(prev) if prev.seq.checked_add(1) != Some(rec.seq) => {
                scan.issues.push(ScanIssue::SeqOutOfOrder {
                    line: line_no,
                    expected: prev.seq.saturating_add(1),
                    found: rec.seq,
                })
            }
            _ => {}
        }
        scan.records.push(rec);
    }
    if scan.records.is_empty() && scan.issues.is_empty() {
        scan.issues.push(ScanIssue::BadFirstRecord {
            detail: "the journal has no complete record".into(),
        });
    }
    scan
}

pub fn scan_file(path: &Path) -> std::io::Result<Scan> {
    let mut bytes = Vec::new();
    File::open(path)?.read_to_end(&mut bytes)?;
    Ok(scan_bytes(&bytes))
}

/// The seq of a journal's last complete line, read from the end of the file (a few KB,
/// not the whole journal). `None` for an empty journal or a last line that does not
/// decode. Readers use it to tell whether a cached `head.json` is current: a runner that
/// dies between appending and rewriting `head.json` leaves the cache one commit behind.
pub fn last_seq(path: &Path) -> std::io::Result<Option<u64>> {
    use std::io::{Seek, SeekFrom};
    const STEP: u64 = 8 * 1024;
    let mut f = File::open(path)?;
    let len = f.metadata()?.len();
    let mut take = STEP.min(len);
    loop {
        f.seek(SeekFrom::Start(len - take))?;
        let mut buf = vec![0; take as usize];
        f.read_exact(&mut buf)?;
        // Ignore a torn tail: only lines ended by '\n' were acknowledged.
        let Some(end) = buf.iter().rposition(|b| *b == b'\n') else {
            if take == len || take > MAX_LINE_BYTES as u64 {
                return Ok(None);
            }
            take = (take + STEP).min(len);
            continue;
        };
        let body = &buf[..end];
        match body.iter().rposition(|b| *b == b'\n') {
            Some(start) => return Ok(line_seq(&body[start + 1..])),
            // The whole buffer is inside the last line: it starts the file, or read more.
            None if take == len => return Ok(line_seq(body)),
            None if take > MAX_LINE_BYTES as u64 => return Ok(None),
            None => take = (take + STEP).min(len),
        }
    }
}

fn line_seq(line: &[u8]) -> Option<u64> {
    #[derive(serde::Deserialize)]
    struct Seq {
        seq: u64,
    }
    serde_json::from_slice::<Seq>(line).ok().map(|s| s.seq)
}

/// Read a loop without taking the lease (dashboards, `agentpit loop show`).
pub fn read_loop(loop_dir: &Path) -> std::io::Result<(LoopState, Scan)> {
    let scan = scan_file(&journal_path(loop_dir))?;
    let state = replay(&scan.records);
    Ok((state, scan))
}

/// One record to append; seq and ts are assigned by the writer.
#[derive(Debug, Clone, PartialEq)]
pub struct Draft {
    pub op: Option<String>,
    pub event: LoopEvent,
}

impl Draft {
    pub fn new(event: LoopEvent) -> Self {
        Draft { op: None, event }
    }

    pub fn by_op(op: &str, event: LoopEvent) -> Self {
        Draft {
            op: Some(op.to_string()),
            event,
        }
    }
}

#[derive(Debug)]
pub enum JournalError {
    /// Another live process holds the loop's lease (`pid` 0 = owner mid-acquisition).
    Busy {
        pid: u32,
    },
    Io(std::io::Error),
    NotFound,
    AlreadyExists,
    InvalidFirst(String),
    /// The journal is damaged; the scan is kept so it can still be displayed.
    Corrupt(Box<Scan>),
    LineTooLarge {
        index: usize,
        bytes: usize,
    },
    /// A previous write or fsync failed; only a fresh open may continue.
    Poisoned,
}

impl std::fmt::Display for JournalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            JournalError::Busy { pid } if *pid > 0 => {
                write!(f, "loop journal is held by another process (pid {pid})")
            }
            JournalError::Busy { .. } => write!(f, "loop journal is held by another process"),
            JournalError::Io(e) => write!(f, "loop journal I/O error: {e}"),
            JournalError::NotFound => write!(f, "loop journal not found"),
            JournalError::AlreadyExists => write!(f, "loop journal already exists"),
            JournalError::InvalidFirst(why) => write!(f, "invalid first record: {why}"),
            JournalError::Corrupt(scan) => {
                let first = scan
                    .issues
                    .first()
                    .map(ToString::to_string)
                    .unwrap_or_default();
                write!(f, "loop journal is corrupt: {first}")
            }
            JournalError::LineTooLarge { index, bytes } => write!(
                f,
                "record {index} encodes to {bytes} bytes (limit {MAX_LINE_BYTES})"
            ),
            JournalError::Poisoned => {
                write!(f, "an earlier write failed; reopen the loop journal")
            }
        }
    }
}

impl std::error::Error for JournalError {}

impl From<std::io::Error> for JournalError {
    fn from(e: std::io::Error) -> Self {
        JournalError::Io(e)
    }
}

impl From<LeaseError> for JournalError {
    fn from(e: LeaseError) -> Self {
        match e {
            LeaseError::Busy { pid } => JournalError::Busy { pid },
            LeaseError::Io(e) => JournalError::Io(e),
        }
    }
}

fn encode_batch(
    first_seq: u64,
    ts: u64,
    drafts: &[Draft],
) -> Result<(String, Vec<LoadedRecord>), JournalError> {
    let mut batch = String::new();
    let mut records = Vec::with_capacity(drafts.len());
    for (i, d) in drafts.iter().enumerate() {
        let seq = first_seq + i as u64;
        let line = encode_record(seq, ts, d.op.as_deref(), &d.event);
        if line.len() + 1 > MAX_LINE_BYTES {
            return Err(JournalError::LineTooLarge {
                index: i,
                bytes: line.len() + 1,
            });
        }
        batch.push_str(&line);
        batch.push('\n');
        records.push(LoadedRecord {
            seq,
            ts,
            kind: d.event.kind().to_string(),
            op: d.op.clone(),
            anc: d.event.is_ancillary(),
            body: Body::Known(d.event.clone()),
            raw: line,
        });
    }
    Ok((batch, records))
}

/// The dumb seq/fsync layer: no semantics, just I1–I4.
pub struct JournalWriter {
    path: PathBuf,
    file: File,
    _lease: SessionLease,
    head_seq: u64,
    last_ts: u64,
    /// Set by `open` when a torn tail exists; applied right before the first append, so a
    /// caller that decides the journal is read-only leaves the file untouched.
    pending_truncate: Option<u64>,
    poisoned: bool,
}

impl JournalWriter {
    /// Create `dir/journal.jsonl` holding exactly `drafts` (the first must be
    /// `loop_created`). Atomic: the batch is written to a temporary file, fsynced, and
    /// renamed into place, so a crash leaves either no journal or the whole first batch.
    pub fn create(
        dir: &Path,
        leases_root: &Path,
        now_ms: u64,
        drafts: &[Draft],
    ) -> Result<(JournalWriter, Vec<LoadedRecord>), JournalError> {
        match drafts.first().map(|d| &d.event) {
            Some(LoopEvent::LoopCreated(_)) => {}
            _ => {
                return Err(JournalError::InvalidFirst(
                    "the first record must be loop_created".into(),
                ))
            }
        }
        if !dir.is_dir() {
            return Err(JournalError::NotFound);
        }
        let (batch, records) = encode_batch(1, now_ms, drafts)?;
        let lease = SessionLease::acquire_at(leases_root, dir)?;
        let path = journal_path(dir);
        if path.exists() {
            return Err(JournalError::AlreadyExists);
        }
        let tmp = dir.join(format!("{JOURNAL_FILE}.tmp"));
        {
            let mut f = File::create(&tmp)?;
            f.write_all(batch.as_bytes())?;
            f.sync_all()?;
        }
        fs::rename(&tmp, &path)?;
        sync_dir(dir);
        let file = OpenOptions::new().append(true).open(&path)?;
        let writer = JournalWriter {
            path,
            file,
            _lease: lease,
            head_seq: records.last().map_or(0, |r| r.seq),
            last_ts: now_ms,
            pending_truncate: None,
            poisoned: false,
        };
        Ok((writer, records))
    }

    /// Take the lease and scan. Does not modify the file (see `pending_truncate`).
    pub fn open(dir: &Path, leases_root: &Path) -> Result<(JournalWriter, Scan), JournalError> {
        let path = journal_path(dir);
        if !path.is_file() {
            return Err(JournalError::NotFound);
        }
        let lease = SessionLease::acquire_at(leases_root, dir)?;
        let scan = scan_file(&path)?;
        if !scan.is_clean() {
            return Err(JournalError::Corrupt(Box::new(scan)));
        }
        let file = OpenOptions::new().append(true).open(&path)?;
        let writer = JournalWriter {
            path,
            file,
            _lease: lease,
            head_seq: scan.head_seq(),
            last_ts: scan.records.last().map_or(0, |r| r.ts),
            pending_truncate: (scan.torn_tail_bytes > 0).then_some(scan.complete_len),
            poisoned: false,
        };
        Ok((writer, scan))
    }

    /// Append one batch durably. Returns the records as written.
    pub fn append(
        &mut self,
        now_ms: u64,
        drafts: &[Draft],
    ) -> Result<Vec<LoadedRecord>, JournalError> {
        if self.poisoned {
            return Err(JournalError::Poisoned);
        }
        if drafts.is_empty() {
            return Ok(vec![]);
        }
        let ts = now_ms.max(self.last_ts);
        let (batch, records) = encode_batch(self.head_seq + 1, ts, drafts)?;
        let result = (|| -> std::io::Result<()> {
            if let Some(len) = self.pending_truncate {
                self.file.set_len(len)?;
                self.file.sync_all()?;
            }
            self.file.write_all(batch.as_bytes())?;
            self.file.sync_data()
        })();
        if let Err(e) = result {
            // After a failed fsync the page cache cannot be trusted: stop here.
            self.poisoned = true;
            return Err(JournalError::Io(e));
        }
        self.pending_truncate = None;
        self.head_seq += records.len() as u64;
        self.last_ts = ts;
        Ok(records)
    }

    pub fn head_seq(&self) -> u64 {
        self.head_seq
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

fn sync_dir(dir: &Path) {
    // Makes the rename durable on Linux; best-effort elsewhere.
    if let Ok(d) = File::open(dir) {
        let _ = d.sync_all();
    }
}

/// Who is writing (recorded in `writer_opened`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriterInfo {
    pub pid: u32,
    pub start_id: String,
    pub build: String,
}

impl WriterInfo {
    /// This process.
    pub fn current(build: &str) -> Self {
        let pid = std::process::id();
        WriterInfo {
            pid,
            start_id: process_start_id(pid),
            build: build.to_string(),
        }
    }

    fn opened(&self, epoch: u32, truncated_tail_bytes: u64) -> LoopEvent {
        LoopEvent::WriterOpened(WriterOpened {
            epoch,
            pid: self.pid,
            start_id: self.start_id.clone(),
            build: self.build.clone(),
            schema_minor: SCHEMA_MINOR,
            truncated_tail_bytes,
        })
    }
}

#[derive(Debug)]
pub enum CommitError {
    ReadOnly(ReadOnlyReason),
    /// Draft `index` is not a legal transition; nothing was written.
    Rejected {
        index: usize,
        rejection: Rejection,
    },
    Journal(JournalError),
}

impl std::fmt::Display for CommitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CommitError::ReadOnly(r) => write!(f, "read-only: {r}"),
            CommitError::Rejected { index, rejection } => {
                write!(f, "record {index} rejected: {rejection}")
            }
            CommitError::Journal(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for CommitError {}

impl From<JournalError> for CommitError {
    fn from(e: JournalError) -> Self {
        CommitError::Journal(e)
    }
}

/// How [`LoopJournal::open`] found the loop.
pub enum Opened {
    Writable(LoopJournal),
    /// This build may not append; the state is still readable. The lease is released and
    /// the file untouched.
    ReadOnly {
        state: LoopState,
        reason: ReadOnlyReason,
    },
}

/// The single commit path: admit → append (fsync) → apply.
pub struct LoopJournal {
    writer: JournalWriter,
    state: LoopState,
}

/// A draft as the fold will see it once written (only ever applied to a scratch state).
fn provisional(seq: u64, ts: u64, d: &Draft) -> LoadedRecord {
    LoadedRecord {
        seq,
        ts,
        kind: d.event.kind().to_string(),
        op: d.op.clone(),
        anc: d.event.is_ancillary(),
        body: Body::Known(d.event.clone()),
        raw: String::new(),
    }
}

/// Admit every draft against a provisional copy of `state`, applying each in turn so a
/// batch may build on itself (e.g. `step_started` then `gate_opened` for that step).
fn admit_batch(state: &LoopState, now_ms: u64, drafts: &[Draft]) -> Result<(), CommitError> {
    let mut prov = state.clone();
    let ts = now_ms.max(state.last_ts);
    for (index, d) in drafts.iter().enumerate() {
        prov.admit(&d.event)
            .map_err(|rejection| CommitError::Rejected { index, rejection })?;
        prov.apply(&provisional(prov.head_seq.saturating_add(1), ts, d));
    }
    Ok(())
}

impl LoopJournal {
    /// Create a loop: `loop_created`, `writer_opened{epoch 1}`, and `loop_started` when
    /// `start` is set — one atomic first batch.
    pub fn create(
        dir: &Path,
        leases_root: &Path,
        now_ms: u64,
        created: LoopCreated,
        op: Option<&str>,
        writer: &WriterInfo,
        start: bool,
    ) -> Result<LoopJournal, CommitError> {
        let mut drafts = vec![
            Draft {
                op: op.map(str::to_string),
                event: LoopEvent::LoopCreated(Box::new(created)),
            },
            Draft::new(writer.opened(1, 0)),
        ];
        if start {
            drafts.push(Draft {
                op: op.map(str::to_string),
                event: LoopEvent::LoopStarted,
            });
        }
        admit_batch(&LoopState::default(), now_ms, &drafts)?;
        let (writer, records) = JournalWriter::create(dir, leases_root, now_ms, &drafts)?;
        Ok(LoopJournal {
            writer,
            state: replay(&records),
        })
    }

    /// Reopen as the loop's writer: epoch + 1, recorded with the truncated tail size. The
    /// caller then handles [`LoopState::orphaned_steps`] (design §9.4).
    pub fn open(
        dir: &Path,
        leases_root: &Path,
        now_ms: u64,
        writer: &WriterInfo,
    ) -> Result<Opened, JournalError> {
        let (file, scan) = match JournalWriter::open(dir, leases_root) {
            Ok(x) => x,
            Err(JournalError::Corrupt(scan)) => {
                let reason = ReadOnlyReason::Corrupt(
                    scan.issues
                        .first()
                        .map(ToString::to_string)
                        .unwrap_or_default(),
                );
                return Ok(Opened::ReadOnly {
                    state: replay(&scan.records),
                    reason,
                });
            }
            Err(e) => return Err(e),
        };
        let state = replay(&scan.records);
        if let Err(reason) = state.writable() {
            drop(file);
            return Ok(Opened::ReadOnly { state, reason });
        }
        let epoch = state.epoch + 1;
        let mut journal = LoopJournal {
            writer: file,
            state,
        };
        let opened = Draft::new(writer.opened(epoch, scan.torn_tail_bytes));
        match journal.commit(now_ms, &[opened]) {
            Ok(_) => Ok(Opened::Writable(journal)),
            Err(CommitError::Journal(e)) => Err(e),
            Err(other) => Err(JournalError::Io(std::io::Error::other(other.to_string()))),
        }
    }

    /// Append a batch if every record is a legal transition; all or nothing.
    pub fn commit(
        &mut self,
        now_ms: u64,
        drafts: &[Draft],
    ) -> Result<Vec<LoadedRecord>, CommitError> {
        self.state.writable().map_err(CommitError::ReadOnly)?;
        admit_batch(&self.state, now_ms, drafts)?;
        let records = self.writer.append(now_ms, drafts)?;
        for r in &records {
            self.state.apply(r);
        }
        Ok(records)
    }

    pub fn state(&self) -> &LoopState {
        &self.state
    }

    pub fn head_seq(&self) -> u64 {
        self.writer.head_seq()
    }

    pub fn path(&self) -> &Path {
        self.writer.path()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::loops::{
        blueprint_rev, step_id, Access, Actor, Assignee, BlobRef, Budget, LoopStatus, NodeKind,
        Outcome, WorkspaceMode,
    };
    use serde_json::Value;
    use std::path::PathBuf;

    const BLUEPRINT: &str =
        include_str!("../../tests/fixtures/loops/blueprint_fix_until_green.json");
    const LOOP_ID: &str = "lp-0199a1b2c3d47e5f8a9b0c1d2e3f4a5b";

    struct Fixture {
        _tmp: tempfile::TempDir,
        dir: PathBuf,
        leases: PathBuf,
    }

    fn fixture() -> Fixture {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("loops").join(LOOP_ID);
        fs::create_dir_all(&dir).unwrap();
        let leases = tmp.path().join("loop-leases");
        Fixture {
            _tmp: tmp,
            dir,
            leases,
        }
    }

    #[test]
    fn last_seq_reads_the_last_acknowledged_line_only() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("journal.jsonl");
        assert!(last_seq(&path).is_err(), "no journal");
        fs::write(&path, "").unwrap();
        assert_eq!(last_seq(&path).unwrap(), None);
        fs::write(&path, "{\"seq\":1}\n").unwrap();
        assert_eq!(last_seq(&path).unwrap(), Some(1));
        // A torn tail is not a record yet.
        fs::write(&path, "{\"seq\":1}\n{\"seq\":2,\"data\":\"x").unwrap();
        assert_eq!(last_seq(&path).unwrap(), Some(1));
        // Lines longer than one read step.
        let big = "y".repeat(20_000);
        let text = format!(
            "{{\"seq\":1}}\n{{\"seq\":2,\"pad\":\"{big}\"}}\n{{\"seq\":3,\"pad\":\"{big}\"}}\n"
        );
        fs::write(&path, text).unwrap();
        assert_eq!(last_seq(&path).unwrap(), Some(3));
        fs::write(&path, "not json\n").unwrap();
        assert_eq!(last_seq(&path).unwrap(), None);
    }

    fn created() -> LoopCreated {
        let doc: Value = serde_json::from_str(BLUEPRINT).unwrap();
        LoopCreated {
            loop_id: LOOP_ID.into(),
            uid: "9c41e07a2b5d4f18".into(),
            title: "t".into(),
            blueprint: FrozenBlueprint {
                name: "fix-until-green".into(),
                scope: BlueprintScope::Project,
                path: None,
                rev: blueprint_rev(&doc),
                doc,
            },
            inputs: [("goal".to_string(), "g".to_string())].into(),
            cwd: "/tmp".into(),
            repo_root: None,
            workspace: WorkspaceMode::InPlace,
            budget: Budget::default(),
            origin: Origin {
                surface: Surface::Cli,
                client: None,
                session_id: None,
            },
            root_run_id: None,
        }
    }

    fn writer() -> WriterInfo {
        WriterInfo::current("test")
    }

    fn start_plan() -> Draft {
        Draft::new(LoopEvent::StepStarted(Box::new(StepStarted {
            step_id: step_id("plan", &[], 1),
            node: "plan".into(),
            iter: vec![],
            attempt: 1,
            kind: NodeKind::Agent,
            cause: StartCause::Ready,
            retry_of: None,
            access: Some(Access::Read),
            assignee: Some(Assignee {
                backend: "claude".into(),
                ..Assignee::default()
            }),
            run_id: None,
            prompt: Some(BlobRef {
                path: "prompts/plan.a1.md".into(),
                bytes: 3,
            }),
            command: None,
            deadline_ms: Some(9_999_999),
            instructions: vec![],
        })))
    }

    fn file_text(f: &Fixture) -> String {
        fs::read_to_string(journal_path(&f.dir)).unwrap()
    }

    fn reopen(f: &Fixture, now: u64) -> LoopJournal {
        match LoopJournal::open(&f.dir, &f.leases, now, &writer()).unwrap() {
            Opened::Writable(j) => j,
            Opened::ReadOnly { reason, .. } => panic!("read-only: {reason}"),
        }
    }

    #[test]
    fn create_writes_the_first_batch_atomically_and_contiguously() {
        let f = fixture();
        let j = LoopJournal::create(
            &f.dir,
            &f.leases,
            1000,
            created(),
            Some("op-00001"),
            &writer(),
            true,
        )
        .unwrap();
        assert_eq!(j.head_seq(), 3);
        assert_eq!(j.state().status, LoopStatus::Running);
        let scan = scan_file(&journal_path(&f.dir)).unwrap();
        assert!(scan.is_clean());
        let kinds: Vec<&str> = scan.records.iter().map(|r| r.kind.as_str()).collect();
        assert_eq!(kinds, vec!["loop_created", "writer_opened", "loop_started"]);
        assert_eq!(scan.records[2].op.as_deref(), Some("op-00001"));
        assert!(!f.dir.join("journal.jsonl.tmp").exists());
    }

    #[test]
    fn create_twice_is_already_exists_and_a_live_writer_is_busy() {
        let f = fixture();
        let j = LoopJournal::create(&f.dir, &f.leases, 1000, created(), None, &writer(), false)
            .unwrap();
        // Same process, lease held: the second create sees the lease first.
        let again = LoopJournal::create(&f.dir, &f.leases, 1000, created(), None, &writer(), false);
        assert!(matches!(
            again,
            Err(CommitError::Journal(JournalError::Busy { .. }))
        ));
        drop(j);
        let again = LoopJournal::create(&f.dir, &f.leases, 1000, created(), None, &writer(), false);
        assert!(matches!(
            again,
            Err(CommitError::Journal(JournalError::AlreadyExists))
        ));
    }

    #[test]
    fn an_inadmissible_or_oversized_first_batch_leaves_no_file() {
        let f = fixture();
        let mut bad = created();
        bad.blueprint.rev = "b1-0000000000000000".into();
        let r = LoopJournal::create(&f.dir, &f.leases, 1000, bad, None, &writer(), true);
        assert!(matches!(r, Err(CommitError::Rejected { index: 0, .. })));

        let mut huge = created();
        huge.title = String::new();
        huge.inputs
            .insert("goal".into(), "x".repeat(MAX_LINE_BYTES));
        let r = LoopJournal::create(&f.dir, &f.leases, 1000, huge, None, &writer(), true);
        assert!(matches!(
            r,
            Err(CommitError::Journal(JournalError::LineTooLarge {
                index: 0,
                ..
            }))
        ));
        assert!(!journal_path(&f.dir).exists());
    }

    #[test]
    fn commit_rejects_inadmissible_batches_without_writing() {
        let f = fixture();
        let mut j =
            LoopJournal::create(&f.dir, &f.leases, 1000, created(), None, &writer(), true).unwrap();
        let before = file_text(&f);
        // A valid first record followed by an impossible one: all or nothing.
        let r = j.commit(2000, &[start_plan(), Draft::new(LoopEvent::LoopStarted)]);
        assert!(matches!(r, Err(CommitError::Rejected { index: 1, .. })));
        assert_eq!(file_text(&f), before);
        assert_eq!(j.head_seq(), 3);
        j.commit(2000, &[start_plan()]).unwrap();
        assert_eq!(j.head_seq(), 4);
    }

    #[test]
    fn ts_is_clamped_non_decreasing() {
        let f = fixture();
        let mut j =
            LoopJournal::create(&f.dir, &f.leases, 5000, created(), None, &writer(), true).unwrap();
        let recs = j.commit(10, &[start_plan()]).unwrap();
        assert_eq!(recs[0].ts, 5000);
    }

    #[test]
    fn reopen_truncates_a_torn_tail_before_writing_and_records_it() {
        let f = fixture();
        drop(
            LoopJournal::create(&f.dir, &f.leases, 1000, created(), None, &writer(), true).unwrap(),
        );
        let complete = file_text(&f);
        // A complete-looking but unterminated line is still a torn tail.
        let torn = r#"{"v":1,"seq":4,"ts":1,"kind":"loop_paused","data":{"mode":"drain"}}"#;
        fs::write(journal_path(&f.dir), format!("{complete}{torn}")).unwrap();

        let j = reopen(&f, 9000);
        assert_eq!(
            j.head_seq(),
            4,
            "writer_opened took seq 4, not the torn line"
        );
        assert_eq!(j.state().epoch, 2);
        assert_eq!(j.state().status, LoopStatus::Running);
        let text = file_text(&f);
        assert!(text.starts_with(&complete));
        assert!(!text.contains("loop_paused"));
        let last = scan_bytes(text.as_bytes()).records.pop().unwrap();
        match last.body {
            Body::Known(LoopEvent::WriterOpened(w)) => {
                assert_eq!((w.epoch, w.truncated_tail_bytes), (2, torn.len() as u64))
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn mid_file_damage_and_seq_gaps_are_read_only_and_untouched() {
        for damage in [
            "garbage\n",
            "{\"v\":1,\"seq\":9,\"ts\":1,\"kind\":\"loop_resumed\",\"data\":{}}\n",
        ] {
            let f = fixture();
            drop(
                LoopJournal::create(&f.dir, &f.leases, 1000, created(), None, &writer(), true)
                    .unwrap(),
            );
            let text = format!("{}{damage}", file_text(&f));
            fs::write(journal_path(&f.dir), &text).unwrap();
            match LoopJournal::open(&f.dir, &f.leases, 2000, &writer()).unwrap() {
                Opened::ReadOnly { state, reason } => {
                    assert!(matches!(reason, ReadOnlyReason::Corrupt(_)), "{reason}");
                    assert_eq!(state.status, LoopStatus::Running);
                }
                Opened::Writable(_) => panic!("damaged journal opened writable"),
            }
            assert_eq!(file_text(&f), text);
            // The lease was released: a second open is not Busy.
            assert!(LoopJournal::open(&f.dir, &f.leases, 2000, &writer()).is_ok());
        }
    }

    #[test]
    fn a_newer_journal_is_read_only_and_keeps_its_torn_tail() {
        let f = fixture();
        drop(
            LoopJournal::create(&f.dir, &f.leases, 1000, created(), None, &writer(), true).unwrap(),
        );
        let text = format!(
            "{}{}\n{}",
            file_text(&f),
            r#"{"v":1,"seq":4,"ts":1,"kind":"proposal_created","data":{}}"#,
            "{\"v\":1,\"seq\":5"
        );
        fs::write(journal_path(&f.dir), &text).unwrap();
        match LoopJournal::open(&f.dir, &f.leases, 2000, &writer()).unwrap() {
            Opened::ReadOnly { reason, .. } => {
                assert!(matches!(reason, ReadOnlyReason::Blocking { seq: 4, .. }))
            }
            Opened::Writable(_) => panic!("newer journal opened writable"),
        }
        assert_eq!(file_text(&f), text, "a read-only open never truncates");
    }

    #[test]
    fn crash_recovery_interrupts_orphans_and_excludes_the_gap_from_active_time() {
        let f = fixture();
        let mut j =
            LoopJournal::create(&f.dir, &f.leases, 1000, created(), None, &writer(), true).unwrap();
        j.commit(2000, &[start_plan()]).unwrap();
        drop(j); // the runner dies with plan.a1 running

        let mut j = reopen(&f, 100_000);
        let orphans: Vec<String> = j
            .state()
            .orphaned_steps()
            .iter()
            .map(|s| s.step_id.clone())
            .collect();
        assert_eq!(orphans, vec!["plan.a1"]);
        assert_eq!(
            j.state().usage.active_ms,
            0,
            "nothing ran after seq 4 was written"
        );
        j.commit(
            100_001,
            &[
                Draft::new(LoopEvent::StepInterrupted(StepInterrupted {
                    step_id: "plan.a1".into(),
                    epoch: 1,
                })),
                Draft::new(LoopEvent::GateOpened(Box::new(GateOpened {
                    gate_id: "g1".into(),
                    kind: GateKind::Recovery,
                    step_id: Some("plan.a1".into()),
                    node: Some("plan".into()),
                    iter: vec![],
                    prompt: "plan was interrupted".into(),
                    options: vec![
                        GateOption {
                            id: "retry".into(),
                            label: None,
                            outcome: None,
                        },
                        GateOption {
                            id: "mark_done".into(),
                            label: None,
                            outcome: Some(Outcome::Ok),
                        },
                    ],
                    deadline_ms: None,
                    on_timeout: None,
                }))),
            ],
        )
        .unwrap();
        assert!(j.state().is_waiting());
        assert!(j.state().orphaned_steps().is_empty());
        j.commit(
            100_002,
            &[Draft::by_op(
                "op-retry-1",
                LoopEvent::GateResolved(GateResolved {
                    gate_id: "g1".into(),
                    option: "retry".into(),
                    comment: None,
                    by: Actor::human("cli"),
                }),
            )],
        )
        .unwrap();
        let mut retry = start_plan();
        if let LoopEvent::StepStarted(s) = &mut retry.event {
            s.step_id = step_id("plan", &[], 2);
            s.attempt = 2;
            s.cause = StartCause::Recovery;
            s.retry_of = Some("plan.a1".into());
        }
        j.commit(100_003, &[retry]).unwrap();
        assert_eq!(j.state().usage.steps, 2);
        assert_eq!(j.state().ops["op-retry-1"], 8);
    }

    #[test]
    fn readers_need_no_lease() {
        let f = fixture();
        let _writer =
            LoopJournal::create(&f.dir, &f.leases, 1000, created(), None, &writer(), true).unwrap();
        let (state, scan) = read_loop(&f.dir).unwrap();
        assert_eq!(state.head_seq, 3);
        assert_eq!(scan.after(1).len(), 2);
    }

    #[test]
    fn an_empty_journal_is_corrupt() {
        let scan = scan_bytes(b"");
        assert!(matches!(
            scan.issues[..],
            [ScanIssue::BadFirstRecord { .. }]
        ));
        let torn = b"{\"v\":1,\"seq\":1,\"ts\":1";
        let scan = scan_bytes(torn);
        assert_eq!(scan.torn_tail_bytes, torn.len() as u64);
        assert!(!scan.is_clean());
    }
}
