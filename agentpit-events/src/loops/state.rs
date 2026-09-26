//! Loop state = fold(journal) (design §5, §8).
//!
//! Two functions over the same state, deliberately separate (design §9.3):
//!
//! - [`LoopState::apply`] is the **lenient** fold. It is total, never panics, and applies
//!   recorded facts only (no clock, no I/O, no evaluation), so every reader — the runner
//!   after a crash, the daemon, the dashboard bridge, a build older or newer than the
//!   writer — reconstructs the same state from the same lines. Anything that does not fit
//!   becomes a warning instead of a failure.
//! - [`LoopState::admit`] is the **strict** transition table the single writer checks
//!   *before* a record is appended. It is the backstop that keeps a buggy scheduler from
//!   journaling an impossible history; it is not the scheduler (phase 2 decides *what* to
//!   write, `admit` only refuses what can never be right).

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use super::record::*;
use super::{
    blueprint_rev, clamp_text, gate_id, instance_key, instruction_id, is_valid_loop_id,
    is_valid_option_id, step_id, string_enum, validate, Access, Assignee, Blueprint,
    BlueprintIndex, Budget, FeedbackItem, NodeKind, NodeSpec, Outcome, Usage, ValidateEnv,
    MAX_DETAIL_BYTES, MAX_FEEDBACK_ITEMS, MAX_GATE_OPTIONS, MAX_SHORT_BYTES, MAX_TEXT_BYTES,
    MAX_WARNINGS, SCHEMA_MINOR, SUMMARY_OPEN_GATES, SUMMARY_PROMPT_BYTES,
};

string_enum! {
    /// Loop lifecycle (design §8.1). `waiting` is derived, never stored.
    #[derive(Default)]
    pub enum LoopStatus {
        #[default]
        Created = "created",
        Running = "running",
        Paused = "paused",
        Stopping = "stopping",
        Succeeded = "succeeded",
        Failed = "failed",
        Cancelled = "cancelled",
    }
}

impl LoopStatus {
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            LoopStatus::Succeeded | LoopStatus::Failed | LoopStatus::Cancelled
        )
    }
}

string_enum! {
    /// One attempt (design §8.2). Terminal states never change; a retry is a new step.
    pub enum StepStatus {
        Running = "running",
        Done = "done",
        /// Was running when its writer died.
        Interrupted = "interrupted",
    }
}

string_enum! {
    /// A node instance — "段階" (design §8.3).
    pub enum InstanceStatus {
        Running = "running",
        Done = "done",
        Interrupted = "interrupted",
        Skipped = "skipped",
    }
}

string_enum! {
    pub enum GateStatus {
        Open = "open",
        Resolved = "resolved",
        Cancelled = "cancelled",
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StepRun {
    pub step_id: String,
    pub node: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub iter: Vec<u32>,
    pub attempt: u32,
    pub kind: NodeKind,
    pub cause: StartCause,
    pub status: StepStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<Outcome>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub access: Option<Access>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assignee: Option<Assignee>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_of: Option<String>,
    /// The writer epoch the step was started in.
    pub epoch: u32,
    pub started_seq: u64,
    pub started_ts: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_ts: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub elapsed_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub excerpt: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cancel: Option<CancelCause>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid_start_id: Option<String>,
    /// The full answer (agents), for `{{nodes.<id>.output}}`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<super::BlobRef>,
    /// What this attempt left for the next iteration.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub feedback: Vec<FeedbackItem>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InstanceRun {
    pub key: String,
    pub node: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub iter: Vec<u32>,
    pub status: InstanceStatus,
    pub attempts: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest_step: Option<String>,
    /// The effective outcome: the latest attempt's, unless a resolved step_error or
    /// recovery gate chose an option that carries an outcome (design §8.3).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<Outcome>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skip_reason: Option<SkipReason>,
}

/// One repeat instance and its iterations.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RepeatRun {
    /// Instance key of the repeat node.
    pub key: String,
    pub node: String,
    /// The repeat's own enclosing iterations.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub outer: Vec<u32>,
    pub max_iterations: u32,
    /// The latest iteration number (0 before the first).
    pub current: u32,
    pub open: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub decisions: Vec<IterationDecision>,
    /// Feedback handed to the current iteration.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub feedback: Vec<FeedbackItem>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GateResolution {
    pub option: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comment: Option<String>,
    pub by: super::Actor,
    pub seq: u64,
    pub ts: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GateRun {
    pub gate_id: String,
    pub kind: GateKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub iter: Vec<u32>,
    pub prompt: String,
    pub options: Vec<GateOption>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deadline_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_timeout: Option<String>,
    pub status: GateStatus,
    pub opened_seq: u64,
    pub opened_ts: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolution: Option<GateResolution>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cancel_reason: Option<GateCancelReason>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InstructionRun {
    pub instruction_id: String,
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    pub received_seq: u64,
    /// The step that consumed it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub consumed_by: Option<String>,
}

/// A fold anomaly (duplicate seq, gap, an impossible transition in a foreign journal).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StateWarning {
    pub seq: u64,
    pub message: String,
}

/// Why this build must not append to a journal (design §13).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadOnlyReason {
    /// No `loop_created` yet.
    NoHeader,
    /// A record this build cannot interpret and may not ignore.
    Blocking { seq: u64, kind: String },
    /// A writer with a newer schema minor has written here.
    NewerWriter { schema_minor: u16 },
    /// The frozen blueprint uses something this build cannot run.
    Blueprint(String),
    /// The journal file is damaged (reported by the journal layer).
    Corrupt(String),
}

impl std::fmt::Display for ReadOnlyReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReadOnlyReason::NoHeader => write!(f, "journal has no loop_created record"),
            ReadOnlyReason::Blocking { seq, kind } => write!(
                f,
                "record {seq} ({kind}) was written by a newer agentpit; upgrade to modify this loop"
            ),
            ReadOnlyReason::NewerWriter { schema_minor } => write!(
                f,
                "a newer agentpit (schema minor {schema_minor}) wrote this loop; upgrade to modify it"
            ),
            ReadOnlyReason::Blueprint(why) => write!(f, "frozen blueprint cannot run here: {why}"),
            ReadOnlyReason::Corrupt(why) => write!(f, "journal is corrupt: {why}"),
        }
    }
}

string_enum! {
    /// Why [`LoopState::admit`] refused a record. Stable snake_case codes.
    pub enum RejectCode {
        MissingHeader = "missing_header",
        DuplicateHeader = "duplicate_header",
        InvalidHeader = "invalid_header",
        BadEpoch = "bad_epoch",
        BadStatus = "bad_status",
        UnknownNode = "unknown_node",
        KindMismatch = "kind_mismatch",
        BadStepId = "bad_step_id",
        BadIter = "bad_iter",
        InstanceDecided = "instance_decided",
        BadRetry = "bad_retry",
        BudgetExhausted = "budget_exhausted",
        Concurrency = "concurrency",
        IncompleteEffect = "incomplete_effect",
        StepNotFound = "step_not_found",
        StepNotRunning = "step_not_running",
        OutcomeNotAllowed = "outcome_not_allowed",
        GatePending = "gate_pending",
        IterationOpen = "iteration_open",
        IterationNotOpen = "iteration_not_open",
        BadIteration = "bad_iteration",
        ScopeBusy = "scope_busy",
        GateNotFound = "gate_not_found",
        GateNotOpen = "gate_not_open",
        BadOption = "bad_option",
        BadGateSubject = "bad_gate_subject",
        BadId = "bad_id",
        EmptyText = "empty_text",
        TooLarge = "too_large",
        BudgetOutOfRange = "budget_out_of_range",
        UnknownValue = "unknown_value",
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rejection {
    pub code: RejectCode,
    pub detail: String,
}

impl std::fmt::Display for Rejection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.detail)
    }
}

impl std::error::Error for Rejection {}

fn reject<T>(code: RejectCode, detail: impl Into<String>) -> Result<T, Rejection> {
    Err(Rejection {
        code,
        detail: detail.into(),
    })
}

/// The fold of one loop journal.
#[derive(Debug, Clone, Default)]
pub struct LoopState {
    pub created: Option<Box<LoopCreated>>,
    /// Typed view of the frozen blueprint (`None` if it does not parse).
    pub blueprint: Option<Blueprint>,
    index: BlueprintIndex,
    pub status: LoopStatus,
    pub pause: Option<PauseMode>,
    pub stop: Option<StopMode>,
    /// The reason given with the first stop request (`"budget"` when the budget ran out
    /// under `on_budget: fail`, which makes the drain end `failed` instead of `cancelled`).
    pub stop_reason: Option<String>,
    pub finish: Option<LoopFinished>,
    pub budget: Budget,
    pub usage: Usage,
    /// The current writer epoch (0 before the first `writer_opened`).
    pub epoch: u32,
    /// The highest `schema_minor` any writer of this journal declared.
    pub newest_schema_minor: u16,
    pub head_seq: u64,
    pub last_ts: u64,
    pub steps: BTreeMap<String, StepRun>,
    pub instances: BTreeMap<String, InstanceRun>,
    pub repeats: BTreeMap<String, RepeatRun>,
    /// In gate-id order (`g1` first).
    pub gates: Vec<GateRun>,
    pub instructions: Vec<InstructionRun>,
    /// op_id → the first seq it caused (idempotency, design §7).
    pub ops: BTreeMap<String, u64>,
    /// Records this build cannot interpret and may not ignore.
    pub blocking: Vec<(u64, String)>,
    pub warnings: Vec<StateWarning>,
    /// Compute steps of the current epoch still running.
    compute_running: u32,
    active_since: Option<u64>,
}

/// Fold a sequence of records from scratch.
pub fn replay<'a>(records: impl IntoIterator<Item = &'a LoadedRecord>) -> LoopState {
    let mut state = LoopState::default();
    for r in records {
        state.apply(r);
    }
    state
}

impl LoopState {
    pub fn loop_id(&self) -> Option<&str> {
        self.created.as_deref().map(|c| c.loop_id.as_str())
    }

    pub fn uid(&self) -> Option<&str> {
        self.created.as_deref().map(|c| c.uid.as_str())
    }

    pub fn step(&self, step_id: &str) -> Option<&StepRun> {
        self.steps.get(step_id)
    }

    pub fn instance(&self, node: &str, iter: &[u32]) -> Option<&InstanceRun> {
        self.instances.get(&instance_key(node, iter))
    }

    pub fn gate(&self, gate_id: &str) -> Option<&GateRun> {
        self.gates.iter().find(|g| g.gate_id == gate_id)
    }

    pub fn open_gates(&self) -> impl Iterator<Item = &GateRun> {
        self.gates.iter().filter(|g| g.status == GateStatus::Open)
    }

    /// Running steps, oldest first.
    pub fn running_steps(&self) -> Vec<&StepRun> {
        let mut v: Vec<&StepRun> = self
            .steps
            .values()
            .filter(|s| s.status == StepStatus::Running)
            .collect();
        v.sort_by_key(|s| s.started_seq);
        v
    }

    /// Compute steps a dead writer left running: the next writer must kill their
    /// processes and record `step_interrupted` for each (design §9.4).
    pub fn orphaned_steps(&self) -> Vec<&StepRun> {
        self.running_steps()
            .into_iter()
            .filter(|s| s.kind.is_compute() && s.epoch < self.epoch)
            .collect()
    }

    /// Unconsumed instructions, oldest first.
    pub fn pending_instructions(&self) -> impl Iterator<Item = &InstructionRun> {
        self.instructions.iter().filter(|i| i.consumed_by.is_none())
    }

    /// Compute time used as of `now_ms`: the folded total plus the span still open while
    /// compute steps of this writer run (the fold only adds a span when the last one ends).
    pub fn active_ms_at(&self, now_ms: u64) -> u64 {
        self.usage.active_ms + self.active_since.map_or(0, |s| now_ms.saturating_sub(s))
    }

    /// Running, no compute step running, and at least one gate waiting for a person.
    pub fn is_waiting(&self) -> bool {
        self.status == LoopStatus::Running
            && !self.running_steps().iter().any(|s| s.kind.is_compute())
            && self.open_gates().next().is_some()
    }

    /// Whether this build may append to the journal.
    pub fn writable(&self) -> Result<(), ReadOnlyReason> {
        if self.created.is_none() {
            return Err(ReadOnlyReason::NoHeader);
        }
        if let Some((seq, kind)) = self.blocking.first() {
            return Err(ReadOnlyReason::Blocking {
                seq: *seq,
                kind: kind.clone(),
            });
        }
        if self.newest_schema_minor > SCHEMA_MINOR {
            return Err(ReadOnlyReason::NewerWriter {
                schema_minor: self.newest_schema_minor,
            });
        }
        match &self.blueprint {
            None => Err(ReadOnlyReason::Blueprint(
                "the frozen document does not parse".into(),
            )),
            Some(bp) if !bp.is_understood() => Err(ReadOnlyReason::Blueprint(
                "it uses a node kind, value or feature this agentpit does not know".into(),
            )),
            Some(_) => Ok(()),
        }
    }

    fn warn(&mut self, seq: u64, message: impl Into<String>) {
        self.warnings.push(StateWarning {
            seq,
            message: message.into(),
        });
        if self.warnings.len() > MAX_WARNINGS {
            let excess = self.warnings.len() - MAX_WARNINGS;
            self.warnings.drain(..excess);
        }
    }

    // -----------------------------------------------------------------------------------
    // Lenient fold.

    /// Apply one decoded record. Duplicates (seq ≤ head) are ignored; a gap is warned
    /// about and applied anyway.
    pub fn apply(&mut self, rec: &LoadedRecord) {
        if self.head_seq > 0 && rec.seq <= self.head_seq {
            self.warn(rec.seq, format!("duplicate seq {} ignored", rec.seq));
            return;
        }
        if self.head_seq.checked_add(1) != Some(rec.seq) {
            let expected = self.head_seq.saturating_add(1);
            self.warn(
                rec.seq,
                format!("seq gap: expected {expected}, got {}", rec.seq),
            );
        }
        let prev_ts = self.last_ts;
        self.head_seq = rec.seq;
        self.last_ts = self.last_ts.max(rec.ts);
        if let Some(op) = &rec.op {
            self.ops.entry(op.clone()).or_insert(rec.seq);
        }
        if rec.blocks_writing() {
            self.blocking.push((rec.seq, rec.kind.clone()));
        }
        if let Body::Known(ev) = &rec.body {
            self.apply_event(rec.seq, rec.ts.max(prev_ts), prev_ts, ev);
        }
    }

    fn compute_started(&mut self, ts: u64) {
        if self.compute_running == 0 {
            self.active_since = Some(ts);
        }
        self.compute_running += 1;
    }

    fn compute_stopped(&mut self, ts: u64) {
        if self.compute_running == 0 {
            return;
        }
        self.compute_running -= 1;
        if self.compute_running == 0 {
            if let Some(since) = self.active_since.take() {
                self.usage.active_ms += ts.saturating_sub(since);
            }
        }
    }

    fn apply_event(&mut self, seq: u64, ts: u64, prev_ts: u64, ev: &LoopEvent) {
        match ev {
            LoopEvent::LoopCreated(c) => {
                if self.created.is_some() {
                    self.warn(seq, "second loop_created ignored");
                    return;
                }
                self.blueprint = serde_json::from_value(c.blueprint.doc.clone()).ok();
                self.index = self
                    .blueprint
                    .as_ref()
                    .map(Blueprint::index)
                    .unwrap_or_default();
                self.budget = c.budget;
                self.status = LoopStatus::Created;
                self.created = Some(c.clone());
            }
            LoopEvent::WriterOpened(w) => {
                if w.epoch > 1 && self.compute_running > 0 {
                    // The previous writer died with compute running: count active time up
                    // to its last record, not across the crash gap. Its steps no longer
                    // accrue (they are orphans until interrupted).
                    if let Some(since) = self.active_since.take() {
                        self.usage.active_ms += prev_ts.saturating_sub(since);
                    }
                    self.compute_running = 0;
                }
                if w.epoch <= self.epoch {
                    self.warn(seq, format!("writer epoch {} did not advance", w.epoch));
                }
                self.epoch = self.epoch.max(w.epoch);
                self.newest_schema_minor = self.newest_schema_minor.max(w.schema_minor);
            }
            LoopEvent::WriterClosed(_) | LoopEvent::Warning(_) => {}
            LoopEvent::LoopStarted => {
                if self.status == LoopStatus::Created {
                    self.status = LoopStatus::Running;
                } else {
                    self.warn(seq, format!("loop_started while {}", self.status));
                }
            }
            LoopEvent::LoopPaused(p) => {
                if self.status == LoopStatus::Running {
                    self.status = LoopStatus::Paused;
                    self.pause = Some(p.mode);
                } else {
                    self.warn(seq, format!("loop_paused while {}", self.status));
                }
            }
            LoopEvent::LoopResumed => {
                if self.status == LoopStatus::Paused {
                    self.status = LoopStatus::Running;
                    self.pause = None;
                } else {
                    self.warn(seq, format!("loop_resumed while {}", self.status));
                }
            }
            LoopEvent::LoopStopRequested(s) => {
                if self.status.is_terminal() {
                    self.warn(seq, format!("loop_stop_requested while {}", self.status));
                } else {
                    self.status = LoopStatus::Stopping;
                    self.pause = None;
                    if self.stop.is_none() {
                        self.stop_reason = s.reason.clone();
                    }
                    // Graceful → cancel is an escalation; cancel is never downgraded.
                    if self.stop != Some(StopMode::Cancel) {
                        self.stop = Some(s.mode);
                    }
                }
            }
            LoopEvent::LoopFinished(f) => {
                self.status = match f.status {
                    FinishStatus::Succeeded => LoopStatus::Succeeded,
                    FinishStatus::Failed => LoopStatus::Failed,
                    FinishStatus::Cancelled => LoopStatus::Cancelled,
                    FinishStatus::Unknown => LoopStatus::Unknown,
                };
                self.pause = None;
                self.finish = Some(f.clone());
            }
            LoopEvent::BudgetChanged(b) => self.budget = b.budget,
            LoopEvent::IterationStarted(i) => {
                let Some((&n, outer)) = i.iter.split_last() else {
                    self.warn(seq, "iteration_started without an iteration number");
                    return;
                };
                let key = instance_key(&i.repeat, outer);
                let max = self.repeat_max(&i.repeat);
                let run = self
                    .repeats
                    .entry(key.clone())
                    .or_insert_with(|| RepeatRun {
                        key,
                        node: i.repeat.clone(),
                        outer: outer.to_vec(),
                        max_iterations: max,
                        current: 0,
                        open: false,
                        decisions: vec![],
                        feedback: vec![],
                    });
                run.current = n;
                run.open = true;
                run.feedback = i.feedback.clone();
            }
            LoopEvent::IterationFinished(i) => {
                let Some((_, outer)) = i.iter.split_last() else {
                    self.warn(seq, "iteration_finished without an iteration number");
                    return;
                };
                match self.repeats.get_mut(&instance_key(&i.repeat, outer)) {
                    Some(run) => {
                        run.open = false;
                        run.decisions.push(i.decision);
                    }
                    None => self.warn(seq, format!("iteration_finished for unknown {}", i.repeat)),
                }
            }
            LoopEvent::NodeSkipped(n) => {
                let key = instance_key(&n.node, &n.iter);
                if self.instances.contains_key(&key) {
                    self.warn(seq, format!("node_skipped for decided {key}"));
                    return;
                }
                self.instances.insert(
                    key.clone(),
                    InstanceRun {
                        key,
                        node: n.node.clone(),
                        iter: n.iter.clone(),
                        status: InstanceStatus::Skipped,
                        attempts: 0,
                        latest_step: None,
                        outcome: None,
                        skip_reason: Some(n.reason),
                    },
                );
            }
            LoopEvent::StepStarted(s) => self.apply_step_started(seq, ts, s),
            LoopEvent::StepSpawned(s) => match self.steps.get_mut(&s.step_id) {
                Some(step) => {
                    step.pid = Some(s.pid);
                    step.pid_start_id = Some(s.start_id.clone());
                }
                None => self.warn(seq, format!("step_spawned for unknown {}", s.step_id)),
            },
            LoopEvent::StepFinished(f) => self.apply_step_finished(seq, ts, f),
            LoopEvent::StepInterrupted(i) => {
                let Some(step) = self.steps.get_mut(&i.step_id) else {
                    self.warn(seq, format!("step_interrupted for unknown {}", i.step_id));
                    return;
                };
                if step.status != StepStatus::Running {
                    let msg = format!("step_interrupted for {} while {}", i.step_id, step.status);
                    self.warn(seq, msg);
                    return;
                }
                step.status = StepStatus::Interrupted;
                step.finished_ts = Some(ts);
                let (node, iter, epoch, compute) = (
                    step.node.clone(),
                    step.iter.clone(),
                    step.epoch,
                    step.kind.is_compute(),
                );
                if compute && epoch == self.epoch {
                    self.compute_stopped(ts);
                }
                if let Some(inst) = self.instances.get_mut(&instance_key(&node, &iter)) {
                    if inst.latest_step.as_deref() == Some(i.step_id.as_str()) {
                        inst.status = InstanceStatus::Interrupted;
                        inst.outcome = None;
                    }
                }
            }
            LoopEvent::GateOpened(g) => {
                if g.gate_id != gate_id(self.gates.len() + 1) {
                    self.warn(seq, format!("gate id {} out of order", g.gate_id));
                }
                self.gates.push(GateRun {
                    gate_id: g.gate_id.clone(),
                    kind: g.kind,
                    step_id: g.step_id.clone(),
                    node: g.node.clone(),
                    iter: g.iter.clone(),
                    prompt: g.prompt.clone(),
                    options: g.options.clone(),
                    deadline_ms: g.deadline_ms,
                    on_timeout: g.on_timeout.clone(),
                    status: GateStatus::Open,
                    opened_seq: seq,
                    opened_ts: ts,
                    resolution: None,
                    cancel_reason: None,
                });
            }
            LoopEvent::GateResolved(r) => self.apply_gate_resolved(seq, ts, r),
            LoopEvent::GateCancelled(c) => {
                match self.gates.iter_mut().find(|g| g.gate_id == c.gate_id) {
                    Some(g) if g.status == GateStatus::Open => {
                        g.status = GateStatus::Cancelled;
                        g.cancel_reason = Some(c.reason);
                    }
                    _ => self.warn(seq, format!("gate_cancelled for non-open {}", c.gate_id)),
                }
            }
            LoopEvent::InstructionReceived(i) => self.instructions.push(InstructionRun {
                instruction_id: i.instruction_id.clone(),
                text: i.text.clone(),
                target: i.target.clone(),
                received_seq: seq,
                consumed_by: None,
            }),
        }
    }

    fn repeat_max(&self, repeat: &str) -> u32 {
        match self.blueprint.as_ref().and_then(|bp| bp.node(repeat)) {
            Some(n) => match &n.spec {
                NodeSpec::Repeat(r) => r.max_iterations,
                _ => 0,
            },
            None => 0,
        }
    }

    fn apply_step_started(&mut self, seq: u64, ts: u64, s: &StepStarted) {
        if self.steps.contains_key(&s.step_id) {
            self.warn(seq, format!("duplicate step_started {}", s.step_id));
            return;
        }
        if s.kind.is_compute() {
            self.usage.steps += 1;
            self.compute_started(ts);
        }
        let key = instance_key(&s.node, &s.iter);
        let inst = self
            .instances
            .entry(key.clone())
            .or_insert_with(|| InstanceRun {
                key: key.clone(),
                node: s.node.clone(),
                iter: s.iter.clone(),
                status: InstanceStatus::Running,
                attempts: 0,
                latest_step: None,
                outcome: None,
                skip_reason: None,
            });
        inst.status = InstanceStatus::Running;
        inst.attempts = inst.attempts.max(s.attempt);
        inst.latest_step = Some(s.step_id.clone());
        inst.outcome = None;
        if s.kind == NodeKind::Repeat {
            let max = self.repeat_max(&s.node);
            self.repeats.insert(
                key.clone(),
                RepeatRun {
                    key,
                    node: s.node.clone(),
                    outer: s.iter.clone(),
                    max_iterations: max,
                    current: 0,
                    open: false,
                    decisions: vec![],
                    feedback: vec![],
                },
            );
        }
        for id in &s.instructions {
            if let Some(ins) = self
                .instructions
                .iter_mut()
                .find(|i| &i.instruction_id == id)
            {
                ins.consumed_by.get_or_insert_with(|| s.step_id.clone());
            }
        }
        self.steps.insert(
            s.step_id.clone(),
            StepRun {
                step_id: s.step_id.clone(),
                node: s.node.clone(),
                iter: s.iter.clone(),
                attempt: s.attempt,
                kind: s.kind,
                cause: s.cause,
                status: StepStatus::Running,
                outcome: None,
                access: s.access,
                assignee: s.assignee.clone(),
                run_id: s.run_id.clone(),
                retry_of: s.retry_of.clone(),
                epoch: self.epoch,
                started_seq: seq,
                started_ts: ts,
                finished_ts: None,
                elapsed_ms: None,
                excerpt: None,
                error: None,
                exit_code: None,
                cancel: None,
                pid: None,
                pid_start_id: None,
                output: None,
                feedback: vec![],
            },
        );
    }

    fn apply_step_finished(&mut self, seq: u64, ts: u64, f: &StepFinished) {
        let current_epoch = self.epoch;
        let Some(step) = self.steps.get_mut(&f.step_id) else {
            self.warn(seq, format!("step_finished for unknown {}", f.step_id));
            return;
        };
        if step.status != StepStatus::Running {
            let msg = format!("step_finished for {} while {}", f.step_id, step.status);
            self.warn(seq, msg);
            return;
        }
        step.status = StepStatus::Done;
        step.outcome = Some(f.outcome);
        step.finished_ts = Some(ts);
        step.elapsed_ms = Some(f.elapsed_ms);
        step.excerpt = f.excerpt.clone();
        step.error = f.error.clone();
        step.exit_code = f.exit_code;
        step.cancel = f.cancel;
        step.output = f.output.clone();
        step.feedback = f.feedback.clone();
        let (node, iter, compute, epoch) = (
            step.node.clone(),
            step.iter.clone(),
            step.kind.is_compute(),
            step.epoch,
        );
        if compute && epoch == current_epoch {
            self.compute_stopped(ts);
        }
        if let Some(inst) = self.instances.get_mut(&instance_key(&node, &iter)) {
            if inst.latest_step.as_deref() == Some(f.step_id.as_str()) {
                inst.status = InstanceStatus::Done;
                inst.outcome = Some(f.outcome);
            }
        }
    }

    fn apply_gate_resolved(&mut self, seq: u64, ts: u64, r: &GateResolved) {
        let Some(gate) = self.gates.iter_mut().find(|g| g.gate_id == r.gate_id) else {
            self.warn(seq, format!("gate_resolved for unknown {}", r.gate_id));
            return;
        };
        if gate.status != GateStatus::Open {
            let msg = format!("gate_resolved for {} while {}", r.gate_id, gate.status);
            self.warn(seq, msg);
            return;
        }
        gate.status = GateStatus::Resolved;
        gate.resolution = Some(GateResolution {
            option: r.option.clone(),
            comment: r.comment.clone(),
            by: r.by.clone(),
            seq,
            ts,
        });
        // step_error / recovery choices that carry an outcome override the instance's
        // effective outcome ("treat as failed" → fail, "keep edits, continue" → ok).
        let chosen = gate
            .options
            .iter()
            .find(|o| o.id == r.option)
            .and_then(|o| o.outcome);
        let (kind, step_id) = (gate.kind, gate.step_id.clone());
        if !matches!(kind, GateKind::StepError | GateKind::Recovery) {
            return;
        }
        let (Some(outcome), Some(step_id)) = (chosen, step_id) else {
            return;
        };
        let Some(step) = self.steps.get(&step_id) else {
            return;
        };
        let key = instance_key(&step.node, &step.iter);
        if let Some(inst) = self.instances.get_mut(&key) {
            if inst.latest_step.as_deref() == Some(step_id.as_str()) {
                inst.status = InstanceStatus::Done;
                inst.outcome = Some(outcome);
            }
        }
    }

    // -----------------------------------------------------------------------------------
    // Strict admission.

    /// Check that `ev`, appended next, is a legal transition. Pure.
    pub fn admit(&self, ev: &LoopEvent) -> Result<(), Rejection> {
        use RejectCode as C;
        match (self.created.is_some(), ev) {
            (false, LoopEvent::LoopCreated(c)) => return self.admit_created(c),
            (false, _) => return reject(C::MissingHeader, "the first record must be loop_created"),
            (true, LoopEvent::LoopCreated(_)) => {
                return reject(C::DuplicateHeader, "loop_created is only ever seq 1")
            }
            _ => {}
        }
        if ev.has_unknown_values() {
            return reject(C::UnknownValue, "writers never serialize unknown values");
        }
        if self.status.is_terminal()
            && !matches!(
                ev,
                LoopEvent::WriterOpened(_) | LoopEvent::WriterClosed(_) | LoopEvent::Warning(_)
            )
        {
            return reject(C::BadStatus, format!("the loop is {}", self.status));
        }
        let status = self.status;
        match ev {
            LoopEvent::LoopCreated(_) => unreachable!("handled above"),
            LoopEvent::WriterOpened(w) => {
                if w.epoch != self.epoch + 1 {
                    return reject(C::BadEpoch, format!("next epoch is {}", self.epoch + 1));
                }
                Ok(())
            }
            LoopEvent::WriterClosed(w) => {
                if w.epoch != self.epoch {
                    return reject(C::BadEpoch, format!("current epoch is {}", self.epoch));
                }
                Ok(())
            }
            LoopEvent::LoopStarted => expect_status(status, &[LoopStatus::Created]),
            LoopEvent::LoopPaused(_) => expect_status(status, &[LoopStatus::Running]),
            LoopEvent::LoopResumed => expect_status(status, &[LoopStatus::Paused]),
            LoopEvent::LoopStopRequested(s) => match status {
                LoopStatus::Created | LoopStatus::Running | LoopStatus::Paused => Ok(()),
                LoopStatus::Stopping
                    if self.stop == Some(StopMode::Graceful) && s.mode == StopMode::Cancel =>
                {
                    Ok(())
                }
                _ => reject(
                    C::BadStatus,
                    format!("cannot request a {} stop while {status}", s.mode),
                ),
            },
            LoopEvent::LoopFinished(f) => {
                match f.status {
                    FinishStatus::Cancelled => expect_status(status, &[LoopStatus::Stopping])?,
                    // A stop drain caused by budget exhaustion (`on_budget: fail`) ends failed.
                    FinishStatus::Failed => {
                        expect_status(status, &[LoopStatus::Running, LoopStatus::Stopping])?
                    }
                    _ => expect_status(status, &[LoopStatus::Running])?,
                }
                if let Some(s) = self.running_steps().first() {
                    return reject(C::ScopeBusy, format!("step {} is still running", s.step_id));
                }
                if let Some(g) = self.open_gates().next() {
                    return reject(C::GatePending, format!("gate {} is still open", g.gate_id));
                }
                Ok(())
            }
            LoopEvent::BudgetChanged(b) => {
                if let Some(field) = b.budget.out_of_range() {
                    return reject(
                        C::BudgetOutOfRange,
                        format!("{field} is outside the hard caps"),
                    );
                }
                if b.budget.max_steps < self.usage.steps {
                    return reject(
                        C::BudgetOutOfRange,
                        format!(
                            "max_steps below the {} steps already used",
                            self.usage.steps
                        ),
                    );
                }
                Ok(())
            }
            LoopEvent::IterationStarted(i) => self.admit_iteration_started(i),
            LoopEvent::IterationFinished(i) => self.admit_iteration_finished(i),
            LoopEvent::NodeSkipped(n) => {
                expect_status(
                    status,
                    &[
                        LoopStatus::Running,
                        LoopStatus::Paused,
                        LoopStatus::Stopping,
                    ],
                )?;
                self.node_kind(&n.node)?;
                self.check_context(&n.node, &n.iter)?;
                if self.instances.contains_key(&instance_key(&n.node, &n.iter)) {
                    return reject(
                        C::InstanceDecided,
                        format!(
                            "{} already ran or was skipped",
                            instance_key(&n.node, &n.iter)
                        ),
                    );
                }
                Ok(())
            }
            LoopEvent::StepStarted(s) => self.admit_step_started(s),
            LoopEvent::StepSpawned(s) => {
                let step = self.running_step(&s.step_id)?;
                if !step.kind.is_compute() {
                    return reject(
                        C::KindMismatch,
                        "only agent and check steps spawn processes",
                    );
                }
                Ok(())
            }
            LoopEvent::StepFinished(f) => self.admit_step_finished(f),
            LoopEvent::StepInterrupted(i) => {
                let step = self.running_step(&i.step_id)?;
                if !step.kind.is_compute() {
                    return reject(
                        C::KindMismatch,
                        "only agent and check steps are interrupted",
                    );
                }
                if step.epoch >= self.epoch || i.epoch != step.epoch {
                    return reject(
                        C::BadEpoch,
                        "only a step from an earlier writer epoch is interrupted",
                    );
                }
                Ok(())
            }
            LoopEvent::GateOpened(g) => self.admit_gate_opened(g),
            LoopEvent::GateResolved(r) => {
                let gate = self.open_gate(&r.gate_id)?;
                if !gate.options.iter().any(|o| o.id == r.option) {
                    return reject(
                        C::BadOption,
                        format!("{} is not an option of {}", r.option, r.gate_id),
                    );
                }
                if r.comment
                    .as_ref()
                    .is_some_and(|c| c.len() > MAX_DETAIL_BYTES)
                {
                    return reject(C::TooLarge, "comment is too long");
                }
                Ok(())
            }
            LoopEvent::GateCancelled(c) => self.open_gate(&c.gate_id).map(|_| ()),
            LoopEvent::InstructionReceived(i) => {
                if i.instruction_id != instruction_id(self.instructions.len() + 1) {
                    return reject(
                        C::BadId,
                        format!(
                            "next instruction id is {}",
                            instruction_id(self.instructions.len() + 1)
                        ),
                    );
                }
                if i.text.trim().is_empty() {
                    return reject(C::EmptyText, "instruction text is empty");
                }
                if i.text.len() > MAX_TEXT_BYTES {
                    return reject(C::TooLarge, "instruction text is too long");
                }
                if let Some(target) = &i.target {
                    if self.node_kind(target)? != NodeKind::Agent {
                        return reject(C::KindMismatch, "instructions target agent nodes");
                    }
                }
                Ok(())
            }
            LoopEvent::Warning(w) => {
                if w.message.len() > MAX_DETAIL_BYTES || w.code.len() > MAX_SHORT_BYTES {
                    return reject(C::TooLarge, "warning is too long");
                }
                Ok(())
            }
        }
    }

    fn admit_created(&self, c: &LoopCreated) -> Result<(), Rejection> {
        let bad = |detail: String| reject(RejectCode::InvalidHeader, detail);
        if !is_valid_loop_id(&c.loop_id) {
            return bad(format!("{:?} is not a loop id", c.loop_id));
        }
        if c.uid.len() != 16 || !c.uid.bytes().all(|b| b.is_ascii_hexdigit()) {
            return bad("uid must be 16 hex digits".into());
        }
        if c.title.len() > MAX_SHORT_BYTES {
            return bad("title is too long".into());
        }
        if let Some(field) = c.budget.out_of_range() {
            return bad(format!("budget {field} is outside the hard caps"));
        }
        if c.workspace.is_unknown()
            || c.blueprint.scope.is_unknown()
            || c.origin.surface.is_unknown()
        {
            return bad("header carries an unknown value".into());
        }
        if blueprint_rev(&c.blueprint.doc) != c.blueprint.rev {
            return bad("blueprint rev does not match the frozen document".into());
        }
        // Validated here, at creation only: a later, stricter validator must never strand
        // a loop that is already running (design §13).
        let v = validate(&c.blueprint.doc, &ValidateEnv::default());
        if !v.is_runnable() {
            let codes: BTreeSet<&str> = v
                .diagnostics
                .iter()
                .filter(|d| d.severity == super::Severity::Error)
                .map(|d| d.code.as_str())
                .collect();
            return bad(format!(
                "frozen blueprint does not validate: {}",
                codes.into_iter().collect::<Vec<_>>().join(", ")
            ));
        }
        let bp = v.blueprint.expect("runnable implies parsed");
        if bp.workspace.mode != c.workspace {
            return bad("workspace differs from the blueprint's".into());
        }
        if bp.name != c.blueprint.name {
            return bad("blueprint name differs from the document's".into());
        }
        Ok(())
    }

    fn node_kind(&self, node: &str) -> Result<NodeKind, Rejection> {
        match self.blueprint.as_ref().and_then(|bp| bp.node(node)) {
            Some(n) => Ok(n.spec.kind()),
            None => reject(
                RejectCode::UnknownNode,
                format!("{node} is not in the blueprint"),
            ),
        }
    }

    /// `iter` must name exactly the open iterations of `node`'s enclosing repeats.
    fn check_context(&self, node: &str, iter: &[u32]) -> Result<(), Rejection> {
        let chain = self.index.repeat_chain(node);
        if chain.len() != iter.len() {
            return reject(
                RejectCode::BadIter,
                format!(
                    "{node} sits in {} repeat(s), got {} iteration number(s)",
                    chain.len(),
                    iter.len()
                ),
            );
        }
        for (k, repeat) in chain.iter().enumerate() {
            let key = instance_key(repeat, &iter[..k]);
            match self.repeats.get(&key) {
                Some(run) if run.open && run.current == iter[k] => {}
                _ => {
                    return reject(
                        RejectCode::BadIter,
                        format!("iteration {} of {key} is not open", iter[k]),
                    )
                }
            }
        }
        Ok(())
    }

    fn running_step(&self, step_id: &str) -> Result<&StepRun, Rejection> {
        match self.steps.get(step_id) {
            None => reject(RejectCode::StepNotFound, format!("no step {step_id}")),
            Some(s) if s.status != StepStatus::Running => reject(
                RejectCode::StepNotRunning,
                format!("{step_id} is {}", s.status),
            ),
            Some(s) => Ok(s),
        }
    }

    fn open_gate(&self, gate_id: &str) -> Result<&GateRun, Rejection> {
        match self.gate(gate_id) {
            None => reject(RejectCode::GateNotFound, format!("no gate {gate_id}")),
            Some(g) if g.status != GateStatus::Open => reject(
                RejectCode::GateNotOpen,
                format!("{gate_id} is {}", g.status),
            ),
            Some(g) => Ok(g),
        }
    }

    fn open_gate_about(&self, step_id: &str) -> Option<&GateRun> {
        self.open_gates()
            .find(|g| g.step_id.as_deref() == Some(step_id))
    }

    /// Whether a step lies inside iteration `iter` of `repeat` (at any depth).
    fn in_scope(&self, step: &StepRun, repeat: &str, iter: &[u32]) -> bool {
        self.index.is_descendant(&step.node, repeat) && step.iter.starts_with(iter)
    }

    fn admit_step_started(&self, s: &StepStarted) -> Result<(), Rejection> {
        use RejectCode as C;
        expect_status(self.status, &[LoopStatus::Running])?;
        let kind = self.node_kind(&s.node)?;
        if kind != s.kind {
            return reject(C::KindMismatch, format!("{} is a {kind} node", s.node));
        }
        self.check_context(&s.node, &s.iter)?;
        if s.attempt == 0 || s.step_id != step_id(&s.node, &s.iter, s.attempt) {
            return reject(
                C::BadStepId,
                format!("expected {}", step_id(&s.node, &s.iter, s.attempt.max(1))),
            );
        }
        let key = instance_key(&s.node, &s.iter);
        let instance = self.instances.get(&key);
        if s.attempt == 1 {
            if instance.is_some() {
                return reject(
                    C::InstanceDecided,
                    format!("{key} already ran or was skipped"),
                );
            }
        } else {
            if !kind.is_compute() {
                return reject(C::BadRetry, "only agent and check steps are retried");
            }
            let Some(inst) = instance else {
                return reject(C::BadRetry, format!("{key} has no earlier attempt"));
            };
            let latest = inst.latest_step.as_deref().unwrap_or_default();
            let retryable = inst.status == InstanceStatus::Interrupted
                || (inst.status == InstanceStatus::Done
                    && matches!(
                        inst.outcome,
                        Some(Outcome::Error | Outcome::Timeout | Outcome::Cancelled)
                    ));
            if inst.attempts + 1 != s.attempt || !retryable {
                return reject(
                    C::BadRetry,
                    format!(
                        "{key} is {} after {} attempt(s)",
                        inst.status, inst.attempts
                    ),
                );
            }
            if s.retry_of.as_deref() != Some(latest) {
                return reject(C::BadRetry, format!("retry_of must be {latest}"));
            }
            if let Some(g) = self.open_gate_about(latest) {
                return reject(C::GatePending, format!("answer {} first", g.gate_id));
            }
        }
        if kind.is_compute() {
            if self.usage.steps >= self.budget.max_steps {
                return reject(
                    C::BudgetExhausted,
                    format!(
                        "{} of {} steps used",
                        self.usage.steps, self.budget.max_steps
                    ),
                );
            }
            let Some(access) = s.access else {
                return reject(C::IncompleteEffect, "compute steps record their access");
            };
            // The blueprint's access is a floor: a write agent may not be recorded as read
            // (it would then run next to other compute).
            let declared =
                self.blueprint
                    .as_ref()
                    .and_then(|bp| bp.node(&s.node))
                    .map(|n| match &n.spec {
                        NodeSpec::Agent(a) => a.access,
                        _ => Access::Read,
                    });
            if declared == Some(Access::Write) && access != Access::Write {
                return reject(C::KindMismatch, format!("{} is a write agent", s.node));
            }
            if self.usage.active_ms >= self.budget.max_active_secs.saturating_mul(1000) {
                return reject(
                    C::BudgetExhausted,
                    format!("{}s of compute time used", self.budget.max_active_secs),
                );
            }
            if kind == NodeKind::Check && access != Access::Read {
                return reject(C::KindMismatch, "checks are read-only");
            }
            let running: Vec<&StepRun> = self
                .running_steps()
                .into_iter()
                .filter(|r| r.kind.is_compute())
                .collect();
            let writer_running = running.iter().any(|r| r.access == Some(Access::Write));
            if access == Access::Write && !running.is_empty() {
                return reject(C::Concurrency, "a write step runs alone");
            }
            if writer_running || running.len() as u32 >= self.budget.max_parallel {
                return reject(C::Concurrency, "no free compute slot");
            }
            if s.deadline_ms.is_none() {
                return reject(C::IncompleteEffect, "compute steps record their deadline");
            }
            match kind {
                NodeKind::Agent if s.assignee.is_none() || s.prompt.is_none() => {
                    return reject(
                        C::IncompleteEffect,
                        "agent steps record assignee and prompt",
                    );
                }
                NodeKind::Check if s.command.as_deref().is_none_or(str::is_empty) => {
                    return reject(C::IncompleteEffect, "check steps record their command");
                }
                _ => {}
            }
        }
        for id in &s.instructions {
            let Some(ins) = self.instructions.iter().find(|i| &i.instruction_id == id) else {
                return reject(C::BadId, format!("no instruction {id}"));
            };
            if kind != NodeKind::Agent
                || ins.consumed_by.is_some()
                || ins.target.as_deref().is_some_and(|t| t != s.node)
            {
                return reject(
                    C::BadId,
                    format!("{id} cannot be consumed by {}", s.step_id),
                );
            }
        }
        Ok(())
    }

    fn admit_step_finished(&self, f: &StepFinished) -> Result<(), Rejection> {
        use RejectCode as C;
        let step = self.running_step(&f.step_id)?;
        let allowed: &[Outcome] = match step.kind {
            NodeKind::Agent | NodeKind::Check => &[
                Outcome::Ok,
                Outcome::Fail,
                Outcome::Error,
                Outcome::Timeout,
                Outcome::Cancelled,
            ],
            NodeKind::Gate => &[
                Outcome::Ok,
                Outcome::Fail,
                Outcome::Timeout,
                Outcome::Cancelled,
            ],
            NodeKind::Repeat => &[Outcome::Ok, Outcome::Exhausted, Outcome::Cancelled],
            NodeKind::Unknown => &[],
        };
        if !allowed.contains(&f.outcome) {
            return reject(
                C::OutcomeNotAllowed,
                format!("a {} step cannot end {}", step.kind, f.outcome),
            );
        }
        if (f.outcome == Outcome::Cancelled) != f.cancel.is_some() {
            return reject(
                C::OutcomeNotAllowed,
                "cancel is set exactly for cancelled steps",
            );
        }
        if f.feedback.len() > MAX_FEEDBACK_ITEMS {
            return reject(C::TooLarge, "too many feedback items");
        }
        match step.kind {
            NodeKind::Repeat => {
                let key = instance_key(&step.node, &step.iter);
                let run = self.repeats.get(&key);
                if run.is_some_and(|r| r.open) {
                    return reject(
                        C::IterationOpen,
                        format!("finish the open iteration of {key} first"),
                    );
                }
                // The outcome follows the last iteration decision (design §8.4).
                let expected = match run.and_then(|r| r.decisions.last()) {
                    Some(IterationDecision::Break) => Outcome::Ok,
                    Some(IterationDecision::Exhausted) => Outcome::Exhausted,
                    _ => Outcome::Cancelled,
                };
                if f.outcome != expected {
                    return reject(C::OutcomeNotAllowed, format!("{key} must end {expected}"));
                }
            }
            NodeKind::Gate => {
                if let Some(g) = self.open_gate_about(&f.step_id) {
                    return reject(C::GatePending, format!("close {} first", g.gate_id));
                }
                // The gate step's outcome is the gate's closure.
                let gate = self
                    .gates
                    .iter()
                    .rev()
                    .find(|g| g.step_id.as_deref() == Some(f.step_id.as_str()));
                let expected = match gate {
                    Some(g) if g.status == GateStatus::Resolved => g
                        .resolution
                        .as_ref()
                        .and_then(|r| g.options.iter().find(|o| o.id == r.option))
                        .and_then(|o| o.outcome)
                        .unwrap_or(Outcome::Unknown),
                    Some(g) if g.cancel_reason == Some(GateCancelReason::TimedOut) => {
                        Outcome::Timeout
                    }
                    _ => Outcome::Cancelled,
                };
                if f.outcome != expected {
                    return reject(
                        C::OutcomeNotAllowed,
                        format!("{} must end {expected}", f.step_id),
                    );
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn admit_iteration_started(&self, i: &IterationStarted) -> Result<(), Rejection> {
        use RejectCode as C;
        expect_status(self.status, &[LoopStatus::Running])?;
        if self.node_kind(&i.repeat)? != NodeKind::Repeat {
            return reject(C::KindMismatch, format!("{} is not a repeat", i.repeat));
        }
        let Some((&n, outer)) = i.iter.split_last() else {
            return reject(C::BadIter, "iter must end with the new iteration number");
        };
        self.check_context(&i.repeat, outer)?;
        let key = instance_key(&i.repeat, outer);
        let running = self
            .instances
            .get(&key)
            .and_then(|inst| inst.latest_step.as_deref())
            .and_then(|id| self.steps.get(id))
            .is_some_and(|s| s.status == StepStatus::Running);
        let Some(run) = self.repeats.get(&key).filter(|_| running) else {
            return reject(
                C::StepNotRunning,
                format!("the repeat step of {key} is not running"),
            );
        };
        if run.open {
            return reject(
                C::IterationOpen,
                format!("iteration {} of {key} is open", run.current),
            );
        }
        let continues =
            run.current == 0 || run.decisions.last() == Some(&IterationDecision::Continue);
        if n != run.current + 1 || n > run.max_iterations || !continues {
            return reject(
                C::BadIteration,
                format!(
                    "{key} cannot start iteration {n} (at {} of {})",
                    run.current, run.max_iterations
                ),
            );
        }
        if i.feedback.len() > MAX_FEEDBACK_ITEMS {
            return reject(C::TooLarge, "too many feedback items");
        }
        Ok(())
    }

    fn admit_iteration_finished(&self, i: &IterationFinished) -> Result<(), Rejection> {
        use RejectCode as C;
        let Some((&n, outer)) = i.iter.split_last() else {
            return reject(C::BadIter, "iter must end with the iteration number");
        };
        let key = instance_key(&i.repeat, outer);
        let Some(run) = self.repeats.get(&key).filter(|r| r.open && r.current == n) else {
            return reject(
                C::IterationNotOpen,
                format!("iteration {n} of {key} is not open"),
            );
        };
        let ok = match i.decision {
            IterationDecision::Continue => n < run.max_iterations,
            IterationDecision::Exhausted => n == run.max_iterations,
            _ => true,
        };
        if !ok {
            return reject(
                C::BadIteration,
                format!(
                    "{} is not possible at iteration {n} of {}",
                    i.decision, run.max_iterations
                ),
            );
        }
        if let Some(s) = self
            .running_steps()
            .into_iter()
            .find(|s| self.in_scope(s, &i.repeat, &i.iter))
        {
            return reject(
                C::ScopeBusy,
                format!("{} still runs in this iteration", s.step_id),
            );
        }
        let pending = self.open_gates().find(|g| {
            g.step_id
                .as_deref()
                .and_then(|id| self.steps.get(id))
                .is_some_and(|s| self.in_scope(s, &i.repeat, &i.iter))
        });
        if let Some(g) = pending {
            return reject(
                C::GatePending,
                format!("{} is still open in this iteration", g.gate_id),
            );
        }
        Ok(())
    }

    fn admit_gate_opened(&self, g: &GateOpened) -> Result<(), Rejection> {
        use RejectCode as C;
        let next = gate_id(self.gates.len() + 1);
        if g.gate_id != next {
            return reject(C::BadId, format!("next gate id is {next}"));
        }
        if g.options.is_empty() || g.options.len() > MAX_GATE_OPTIONS {
            return reject(C::BadOption, "a gate has 1..=8 options");
        }
        let mut ids = BTreeSet::new();
        for o in &g.options {
            if !is_valid_option_id(&o.id) || !ids.insert(o.id.as_str()) {
                return reject(
                    C::BadOption,
                    format!("option id {:?} is invalid or repeated", o.id),
                );
            }
        }
        if g.on_timeout
            .as_ref()
            .is_some_and(|t| !ids.contains(t.as_str()))
        {
            return reject(C::BadOption, "on_timeout must name an option");
        }
        if g.prompt.len() > MAX_TEXT_BYTES {
            return reject(C::TooLarge, "prompt is too long");
        }
        let subject = g.step_id.as_deref().and_then(|id| self.steps.get(id));
        let latest = |s: &StepRun| {
            self.instances
                .get(&instance_key(&s.node, &s.iter))
                .and_then(|i| i.latest_step.as_deref())
                == Some(s.step_id.as_str())
        };
        let about_open = g.step_id.as_deref().and_then(|id| self.open_gate_about(id));
        if let Some(open) = about_open {
            return reject(
                C::GatePending,
                format!("{} is already open for this step", open.gate_id),
            );
        }
        match g.kind {
            GateKind::Approval => {
                expect_status(self.status, &[LoopStatus::Running])?;
                let ok = subject
                    .is_some_and(|s| s.kind == NodeKind::Gate && s.status == StepStatus::Running)
                    && !self.gates.iter().any(|x| x.step_id == g.step_id);
                if !ok {
                    return reject(
                        C::BadGateSubject,
                        "an approval gate belongs to its running gate step",
                    );
                }
                if g.options
                    .iter()
                    .any(|o| !matches!(o.outcome, Some(Outcome::Ok | Outcome::Fail)))
                {
                    return reject(C::BadOption, "approval options map to ok or fail");
                }
            }
            GateKind::StepError | GateKind::Recovery => {
                expect_status(self.status, &[LoopStatus::Running, LoopStatus::Paused])?;
                let subject = subject.filter(|s| {
                    s.kind.is_compute()
                        && latest(s)
                        && match g.kind {
                            GateKind::StepError => {
                                s.status == StepStatus::Done
                                    && matches!(s.outcome, Some(Outcome::Error | Outcome::Timeout))
                            }
                            _ => s.status == StepStatus::Interrupted,
                        }
                });
                let Some(s) = subject else {
                    return reject(
                        C::BadGateSubject,
                        format!(
                            "a {} gate follows the latest {} agent/check attempt",
                            g.kind,
                            if g.kind == GateKind::StepError {
                                "errored"
                            } else {
                                "interrupted"
                            }
                        ),
                    );
                };
                // An instance is decided once: a resolved step_error/recovery gate already
                // chose what this attempt means.
                if let Some(done) = self.gates.iter().find(|x| {
                    x.step_id == g.step_id
                        && x.status == GateStatus::Resolved
                        && matches!(x.kind, GateKind::StepError | GateKind::Recovery)
                }) {
                    return reject(
                        C::BadGateSubject,
                        format!("{} already decided {}", done.gate_id, s.step_id),
                    );
                }
                // The answer may change the scope's outcome, so the scope must still be open.
                self.check_context(&s.node, &s.iter)?;
                if g.options.iter().any(|o| {
                    o.outcome
                        .is_some_and(|x| !matches!(x, Outcome::Ok | Outcome::Fail))
                }) {
                    return reject(C::BadOption, "an option may only override to ok or fail");
                }
            }
            GateKind::Budget => {
                expect_status(self.status, &[LoopStatus::Running, LoopStatus::Paused])?;
                if g.step_id.is_some() {
                    return reject(C::BadGateSubject, "a budget gate has no step");
                }
                if self.open_gates().any(|x| x.kind == GateKind::Budget) {
                    return reject(C::GatePending, "a budget gate is already open");
                }
            }
            GateKind::Unknown => return reject(C::UnknownValue, "unknown gate kind"),
        }
        if let (Some(s), Some(node)) = (subject, &g.node) {
            if &s.node != node || s.iter != g.iter {
                return reject(C::BadGateSubject, "node/iter must match the step");
            }
        }
        Ok(())
    }

    // -----------------------------------------------------------------------------------
    // Queries for clients.

    /// Resume a client cursor, or tell it to start over.
    pub fn check_cursor(&self, cursor: &Cursor) -> CursorCheck {
        match self.uid() {
            Some(uid) if uid == cursor.uid && cursor.seq <= self.head_seq => {
                CursorCheck::Resume { after: cursor.seq }
            }
            _ => CursorCheck::Reset,
        }
    }

    /// The board row: "which loop, at which stage, with which agent" (design §5).
    pub fn summary(&self) -> Option<LoopSummary> {
        let c = self.created.as_deref()?;
        let active = self
            .running_steps()
            .into_iter()
            .filter(|s| s.kind != NodeKind::Repeat)
            .map(|s| ActiveStep {
                step_id: s.step_id.clone(),
                node: s.node.clone(),
                iter: s.iter.clone(),
                kind: s.kind,
                attempt: s.attempt,
                assignee: s.assignee.clone(),
                run_id: s.run_id.clone(),
                started_ts: s.started_ts,
            })
            .collect();
        let iterations = self
            .repeats
            .values()
            .filter(|r| r.current > 0)
            .map(|r| IterationHead {
                repeat: r.node.clone(),
                outer: r.outer.clone(),
                n: r.current,
                max: r.max_iterations,
                open: r.open,
            })
            .collect();
        let open: Vec<&GateRun> = self.open_gates().collect();
        Some(LoopSummary {
            loop_id: c.loop_id.clone(),
            uid: c.uid.clone(),
            title: c.title.clone(),
            blueprint: BlueprintHead {
                name: c.blueprint.name.clone(),
                rev: c.blueprint.rev.clone(),
            },
            status: self.status,
            waiting: self.is_waiting(),
            pause: self.pause,
            stop: self.stop,
            finish: self.finish.clone(),
            active,
            iterations,
            open_gate_count: open.len(),
            open_gates: open
                .iter()
                .take(SUMMARY_OPEN_GATES)
                .map(|g| GateHead {
                    gate_id: g.gate_id.clone(),
                    kind: g.kind,
                    node: g.node.clone(),
                    step_id: g.step_id.clone(),
                    prompt: clamp_text(&g.prompt, SUMMARY_PROMPT_BYTES),
                    options: g.options.clone(),
                    deadline_ms: g.deadline_ms,
                })
                .collect(),
            pending_instructions: self.pending_instructions().count(),
            usage: self.usage,
            budget: self.budget,
            head_seq: self.head_seq,
            updated_ts: self.last_ts,
            writable: self.writable().is_ok(),
        })
    }
}

fn expect_status(status: LoopStatus, allowed: &[LoopStatus]) -> Result<(), Rejection> {
    if allowed.contains(&status) {
        return Ok(());
    }
    let names: Vec<&str> = allowed.iter().map(LoopStatus::as_str).collect();
    reject(
        RejectCode::BadStatus,
        format!("the loop is {status}, expected {}", names.join(" or ")),
    )
}

/// A client's position in one loop's journal. Held by the client only; the server keeps
/// no per-client state (design §9.5).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cursor {
    pub uid: String,
    pub seq: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorCheck {
    /// Replay records with `seq > after`.
    Resume { after: u64 },
    /// Different incarnation, or a cursor ahead of the journal: replay from the start.
    Reset,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlueprintHead {
    pub name: String,
    pub rev: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActiveStep {
    pub step_id: String,
    pub node: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub iter: Vec<u32>,
    pub kind: NodeKind,
    pub attempt: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assignee: Option<Assignee>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    pub started_ts: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IterationHead {
    pub repeat: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub outer: Vec<u32>,
    pub n: u32,
    pub max: u32,
    pub open: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GateHead {
    pub gate_id: String,
    pub kind: GateKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step_id: Option<String>,
    /// Clamped to [`SUMMARY_PROMPT_BYTES`].
    pub prompt: String,
    pub options: Vec<GateOption>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deadline_ms: Option<u64>,
}

/// One loop, one line: the board row, the inbox source, and (phase 2) `head.json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LoopSummary {
    pub loop_id: String,
    pub uid: String,
    pub title: String,
    pub blueprint: BlueprintHead,
    pub status: LoopStatus,
    /// Running, but only a person can move it forward.
    #[serde(default, skip_serializing_if = "is_false")]
    pub waiting: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pause: Option<PauseMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop: Option<StopMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finish: Option<LoopFinished>,
    /// "Which stage, which agent": running agent/check/gate steps.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub active: Vec<ActiveStep>,
    /// n/max badges for every repeat instance that has started.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub iterations: Vec<IterationHead>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub open_gates: Vec<GateHead>,
    #[serde(default)]
    pub open_gate_count: usize,
    #[serde(default)]
    pub pending_instructions: usize,
    pub usage: Usage,
    pub budget: Budget,
    pub head_seq: u64,
    pub updated_ts: u64,
    pub writable: bool,
}

fn is_false(b: &bool) -> bool {
    !*b
}
