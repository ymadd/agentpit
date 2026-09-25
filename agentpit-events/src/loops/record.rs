//! Journal records (design §6): one line of `journal.jsonl` == one `rec` on the wire.
//!
//! Every line is an envelope `{"v","seq","ts","kind","op"?,"anc"?,"data"}` whose `data`
//! is the kind's payload. Decoding is two-level — envelope first, then the payload by
//! `kind` — so a kind this build does not know is kept as [`Body::Unknown`] instead of
//! making the line (and the cursor) disappear, which is what `events.jsonl` readers do
//! today (design §13).
//!
//! `anc` marks an *ancillary* kind: detail an older reader or writer may ignore. For a
//! kind this build knows, the table ([`KINDS`]) decides, never the line; for an unknown
//! kind the line's own flag decides. Any other unknown kind is *critical*: this build
//! cannot tell what state it drives, so the journal becomes read-only for it.

use std::collections::BTreeMap;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{
    string_enum, Actor, Assignee, BlobRef, Budget, FeedbackItem, NodeKind, Outcome, Verdict,
    WorkspaceMode,
};

/// Envelope major version. A reader treats `v` above this as an unknown record.
pub const RECORD_V: u16 = 1;

/// The envelope exactly as it sits on disk.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Record {
    pub v: u16,
    /// 1-based, contiguous per journal, assigned by the single writer. Order is `seq`.
    pub seq: u64,
    /// Epoch ms, clamped non-decreasing per journal. Display only.
    pub ts: u64,
    pub kind: String,
    /// The client operation that caused this record (its idempotency key).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub op: Option<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub anc: bool,
    #[serde(default)]
    pub data: Value,
}

fn is_false(b: &bool) -> bool {
    !*b
}

pub struct KindInfo {
    pub name: &'static str,
    pub ancillary: bool,
}

const fn critical(name: &'static str) -> KindInfo {
    KindInfo {
        name,
        ancillary: false,
    }
}

const fn ancillary(name: &'static str) -> KindInfo {
    KindInfo {
        name,
        ancillary: true,
    }
}

/// Every kind this build reads and writes.
pub const KINDS: &[KindInfo] = &[
    critical("loop_created"),
    critical("writer_opened"),
    ancillary("writer_closed"),
    critical("loop_started"),
    critical("loop_paused"),
    critical("loop_resumed"),
    critical("loop_stop_requested"),
    critical("loop_finished"),
    critical("budget_changed"),
    critical("iteration_started"),
    critical("iteration_finished"),
    critical("node_skipped"),
    critical("step_started"),
    ancillary("step_spawned"),
    critical("step_finished"),
    critical("step_interrupted"),
    critical("gate_opened"),
    critical("gate_resolved"),
    critical("gate_cancelled"),
    critical("instruction_received"),
    ancillary("warning"),
];

/// Names later phases will use (design §6). This build decodes them as unknown.
pub const RESERVED_KINDS: &[&str] = &[
    "blueprint_revised",
    "workspace_ready",
    "workspace_failed",
    "proposal_created",
    "proposal_decided",
    "proposal_landing",
    "proposal_landed",
    "proposal_closed",
    "outcome_labeled",
    "artifact_created",
    "artifact_preview",
    "step_tool",
];

pub fn kind_info(name: &str) -> Option<&'static KindInfo> {
    KINDS.iter().find(|k| k.name == name)
}

// ---------------------------------------------------------------------------------------
// Payload enums.

string_enum! {
    pub enum BlueprintScope {
        Project = "project",
        User = "user",
        Inline = "inline",
        Builtin = "builtin",
    }
}

string_enum! {
    /// Where a loop was started from.
    pub enum Surface {
        Cli = "cli",
        Tui = "tui",
        Dashboard = "dashboard",
        Mcp = "mcp",
        Acp = "acp",
    }
}

string_enum! {
    pub enum CloseReason {
        Terminal = "terminal",
        Idle = "idle",
        Shutdown = "shutdown",
    }
}

string_enum! {
    pub enum PauseMode {
        /// Running steps finish; nothing new starts.
        Drain = "drain",
        /// Running compute steps are cancelled (and retried on resume).
        Cancel = "cancel",
    }
}

string_enum! {
    pub enum StopMode {
        /// Running steps finish, then the loop ends cancelled.
        Graceful = "graceful",
        /// Running steps are cancelled.
        Cancel = "cancel",
    }
}

string_enum! {
    pub enum FinishStatus {
        Succeeded = "succeeded",
        Failed = "failed",
        Cancelled = "cancelled",
    }
}

string_enum! {
    pub enum FinishReason {
        /// The top scope settled with every non-ok outcome handled.
        Completed = "completed",
        /// A top-level node ended non-ok and no edge handled it.
        UnhandledOutcome = "unhandled_outcome",
        Budget = "budget",
        Stopped = "stopped",
        /// The runner itself failed (not an agent).
        Error = "error",
    }
}

string_enum! {
    pub enum IterationDecision {
        /// The body settled cleanly: leave the repeat with outcome ok.
        Break = "break",
        /// The body left an unhandled non-ok outcome: run another iteration.
        Continue = "continue",
        /// As continue, but this was the last allowed iteration.
        Exhausted = "exhausted",
        Cancelled = "cancelled",
    }
}

string_enum! {
    pub enum SkipReason {
        /// Every in-edge resolved and none fired.
        DeadPath = "dead_path",
        /// The scope ended (an unhandled non-ok outcome) before this node could run.
        ScopeEnded = "scope_ended",
        Stopped = "stopped",
    }
}

string_enum! {
    pub enum StartCause {
        Ready = "ready",
        /// An automatic retry after error/timeout, or a step_error gate's retry.
        Retry = "retry",
        /// A recovery gate's retry after the runner crashed mid-step.
        Recovery = "recovery",
        /// Restarted to pick up a new instruction.
        Instruction = "instruction",
        /// Re-run after being cancelled by a pause.
        Resume = "resume",
    }
}

string_enum! {
    /// Why a step ended `cancelled` — needed after a restart to know what to re-run.
    pub enum CancelCause {
        Pause = "pause",
        Stop = "stop",
        CancelStep = "cancel_step",
        Instruction = "instruction",
        ScopeEnded = "scope_ended",
    }
}

string_enum! {
    pub enum GateKind {
        /// A blueprint gate node.
        Approval = "approval",
        /// An agent/check errored past its retries (`policy.on_error = gate`).
        StepError = "step_error",
        /// The budget ran out (`policy.on_budget = gate`).
        Budget = "budget",
        /// The runner crashed while a step edited the tree in place.
        Recovery = "recovery",
    }
}

string_enum! {
    pub enum GateCancelReason {
        Stopped = "stopped",
        /// The deadline passed and the gate has no `on_timeout` option.
        TimedOut = "timed_out",
        Superseded = "superseded",
    }
}

// ---------------------------------------------------------------------------------------
// Payloads.

/// seq 1 of every journal: everything needed to run the loop without re-reading anything.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LoopCreated {
    pub loop_id: String,
    /// Incarnation id ([`super::new_uid`]); part of every cursor.
    pub uid: String,
    pub title: String,
    pub blueprint: FrozenBlueprint,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub inputs: BTreeMap<String, String>,
    pub cwd: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo_root: Option<String>,
    pub workspace: WorkspaceMode,
    /// The effective budget (the blueprint's, possibly overridden at start).
    pub budget: Budget,
    pub origin: Origin,
    /// The loop's root run in events.jsonl (`RunKind::Workflow`), so the existing
    /// dashboard shows the loop's dispatches as one tree.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root_run_id: Option<String>,
}

/// The exact document the loop runs. Never re-read from disk.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FrozenBlueprint {
    pub name: String,
    pub scope: BlueprintScope,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    pub rev: String,
    pub doc: Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Origin {
    pub surface: Surface,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
}

/// A writer took the journal (epoch 1 at creation, +1 on every reopen).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WriterOpened {
    pub epoch: u32,
    pub pid: u32,
    pub start_id: String,
    pub build: String,
    pub schema_minor: u16,
    /// Bytes of an unterminated tail discarded on open.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub truncated_tail_bytes: u64,
}

fn is_zero(n: &u64) -> bool {
    *n == 0
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WriterClosed {
    pub epoch: u32,
    pub reason: CloseReason,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoopPaused {
    pub mode: PauseMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoopStopRequested {
    pub mode: StopMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoopFinished {
    pub status: FinishStatus,
    pub reason: FinishReason,
    /// The node whose outcome decided a failure.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BudgetChanged {
    pub budget: Budget,
    pub by: Actor,
}

/// A repeat instance opens iteration `iter.last()`. `iter` is the full path: the
/// enclosing iterations of the repeat, then this one (`[2]`, or `[1, 3]` nested).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IterationStarted {
    pub repeat: String,
    pub iter: Vec<u32>,
    /// What the previous iteration(s) left for this one's prompts.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub feedback: Vec<FeedbackItem>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IterationFinished {
    pub repeat: String,
    pub iter: Vec<u32>,
    pub decision: IterationDecision,
}

/// A node instance decided without running.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeSkipped {
    pub node: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub iter: Vec<u32>,
    pub reason: SkipReason,
}

/// Write-ahead authorization (design §4.3 rule 8): it fully describes the effect, and the
/// runner spawns nothing until this line is durable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StepStarted {
    /// `step_id(node, iter, attempt)`.
    pub step_id: String,
    pub node: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub iter: Vec<u32>,
    pub attempt: u32,
    pub kind: NodeKind,
    pub cause: StartCause,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_of: Option<String>,
    /// Agent and check steps only (a check is always `read`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub access: Option<super::Access>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assignee: Option<Assignee>,
    /// The step's child run in events.jsonl.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    /// The rendered prompt (agents).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<BlobRef>,
    /// The shell command (checks).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deadline_ms: Option<u64>,
    /// Instructions this attempt consumed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub instructions: Vec<String>,
}

/// The step's process, for killing orphans after a runner crash.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StepSpawned {
    pub step_id: String,
    pub pid: u32,
    pub start_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StepFinished {
    pub step_id: String,
    pub outcome: Outcome,
    pub elapsed_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verdict: Option<Verdict>,
    /// The full answer (agents).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<BlobRef>,
    /// First line(s) of the answer for the board.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub excerpt: Option<String>,
    /// Last bytes of a check's output.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log_tail: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub feedback: Vec<FeedbackItem>,
    /// Set exactly when `outcome` is `cancelled`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cancel: Option<CancelCause>,
}

/// A compute step that was running when its writer died (recorded by the next writer).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StepInterrupted {
    pub step_id: String,
    /// The epoch the step had been started in.
    pub epoch: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GateOption {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// For approval gates: the gate step's outcome. For step_error/recovery gates: the
    /// outcome the choice gives the node instance (e.g. "treat as failed" → fail).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<Outcome>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GateOpened {
    /// `g<n>`, numbered per loop.
    pub gate_id: String,
    pub kind: GateKind,
    /// The step the gate is about (approval: the running gate step; step_error: the
    /// errored attempt; recovery: the interrupted attempt; budget: none).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub iter: Vec<u32>,
    /// Rendered prompt.
    pub prompt: String,
    pub options: Vec<GateOption>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deadline_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_timeout: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GateResolved {
    pub gate_id: String,
    pub option: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comment: Option<String>,
    pub by: Actor,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GateCancelled {
    pub gate_id: String,
    pub reason: GateCancelReason,
}

/// Human text for a running loop, consumed by the next matching agent step's
/// `{{instructions}}` (phase 4 adds editor context: selection / file / range).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstructionReceived {
    /// `in<n>`, numbered per loop.
    pub instruction_id: String,
    pub text: String,
    /// An agent node id; `None` = the next agent step of any node.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    pub by: Actor,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Warning {
    pub code: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step_id: Option<String>,
}

// ---------------------------------------------------------------------------------------
// The typed record body.

/// A known record's payload. Large payloads are boxed to keep the enum small.
#[derive(Debug, Clone, PartialEq)]
pub enum LoopEvent {
    LoopCreated(Box<LoopCreated>),
    WriterOpened(WriterOpened),
    WriterClosed(WriterClosed),
    LoopStarted,
    LoopPaused(LoopPaused),
    LoopResumed,
    LoopStopRequested(LoopStopRequested),
    LoopFinished(LoopFinished),
    BudgetChanged(BudgetChanged),
    IterationStarted(IterationStarted),
    IterationFinished(IterationFinished),
    NodeSkipped(NodeSkipped),
    StepStarted(Box<StepStarted>),
    StepSpawned(StepSpawned),
    StepFinished(Box<StepFinished>),
    StepInterrupted(StepInterrupted),
    GateOpened(Box<GateOpened>),
    GateResolved(GateResolved),
    GateCancelled(GateCancelled),
    InstructionReceived(InstructionReceived),
    Warning(Warning),
}

fn payload<T: DeserializeOwned>(data: Value) -> Result<T, String> {
    serde_json::from_value(data).map_err(|e| e.to_string())
}

/// Payload-less kinds accept `{}`, `null` or an absent `data`.
fn empty(data: &Value) -> Result<(), String> {
    match data {
        Value::Null => Ok(()),
        Value::Object(_) => Ok(()),
        other => Err(format!("expected an object, got {other}")),
    }
}

impl LoopEvent {
    pub fn kind(&self) -> &'static str {
        match self {
            LoopEvent::LoopCreated(_) => "loop_created",
            LoopEvent::WriterOpened(_) => "writer_opened",
            LoopEvent::WriterClosed(_) => "writer_closed",
            LoopEvent::LoopStarted => "loop_started",
            LoopEvent::LoopPaused(_) => "loop_paused",
            LoopEvent::LoopResumed => "loop_resumed",
            LoopEvent::LoopStopRequested(_) => "loop_stop_requested",
            LoopEvent::LoopFinished(_) => "loop_finished",
            LoopEvent::BudgetChanged(_) => "budget_changed",
            LoopEvent::IterationStarted(_) => "iteration_started",
            LoopEvent::IterationFinished(_) => "iteration_finished",
            LoopEvent::NodeSkipped(_) => "node_skipped",
            LoopEvent::StepStarted(_) => "step_started",
            LoopEvent::StepSpawned(_) => "step_spawned",
            LoopEvent::StepFinished(_) => "step_finished",
            LoopEvent::StepInterrupted(_) => "step_interrupted",
            LoopEvent::GateOpened(_) => "gate_opened",
            LoopEvent::GateResolved(_) => "gate_resolved",
            LoopEvent::GateCancelled(_) => "gate_cancelled",
            LoopEvent::InstructionReceived(_) => "instruction_received",
            LoopEvent::Warning(_) => "warning",
        }
    }

    pub fn is_ancillary(&self) -> bool {
        kind_info(self.kind()).is_some_and(|k| k.ancillary)
    }

    /// The payload as JSON (`{}` for payload-less kinds).
    pub fn to_data(&self) -> Value {
        let v = match self {
            LoopEvent::LoopCreated(p) => serde_json::to_value(p),
            LoopEvent::WriterOpened(p) => serde_json::to_value(p),
            LoopEvent::WriterClosed(p) => serde_json::to_value(p),
            LoopEvent::LoopStarted | LoopEvent::LoopResumed => {
                Ok(Value::Object(Default::default()))
            }
            LoopEvent::LoopPaused(p) => serde_json::to_value(p),
            LoopEvent::LoopStopRequested(p) => serde_json::to_value(p),
            LoopEvent::LoopFinished(p) => serde_json::to_value(p),
            LoopEvent::BudgetChanged(p) => serde_json::to_value(p),
            LoopEvent::IterationStarted(p) => serde_json::to_value(p),
            LoopEvent::IterationFinished(p) => serde_json::to_value(p),
            LoopEvent::NodeSkipped(p) => serde_json::to_value(p),
            LoopEvent::StepStarted(p) => serde_json::to_value(p),
            LoopEvent::StepSpawned(p) => serde_json::to_value(p),
            LoopEvent::StepFinished(p) => serde_json::to_value(p),
            LoopEvent::StepInterrupted(p) => serde_json::to_value(p),
            LoopEvent::GateOpened(p) => serde_json::to_value(p),
            LoopEvent::GateResolved(p) => serde_json::to_value(p),
            LoopEvent::GateCancelled(p) => serde_json::to_value(p),
            LoopEvent::InstructionReceived(p) => serde_json::to_value(p),
            LoopEvent::Warning(p) => serde_json::to_value(p),
        };
        // Plain structs of strings/numbers/Values always serialize.
        v.unwrap_or(Value::Null)
    }

    /// Decode a known kind's payload. `Ok(None)` for a kind this build does not know.
    pub fn from_data(kind: &str, data: Value) -> Result<Option<LoopEvent>, String> {
        let ev = match kind {
            "loop_created" => LoopEvent::LoopCreated(Box::new(payload(data)?)),
            "writer_opened" => LoopEvent::WriterOpened(payload(data)?),
            "writer_closed" => LoopEvent::WriterClosed(payload(data)?),
            "loop_started" => {
                empty(&data)?;
                LoopEvent::LoopStarted
            }
            "loop_paused" => LoopEvent::LoopPaused(payload(data)?),
            "loop_resumed" => {
                empty(&data)?;
                LoopEvent::LoopResumed
            }
            "loop_stop_requested" => LoopEvent::LoopStopRequested(payload(data)?),
            "loop_finished" => LoopEvent::LoopFinished(payload(data)?),
            "budget_changed" => LoopEvent::BudgetChanged(payload(data)?),
            "iteration_started" => LoopEvent::IterationStarted(payload(data)?),
            "iteration_finished" => LoopEvent::IterationFinished(payload(data)?),
            "node_skipped" => LoopEvent::NodeSkipped(payload(data)?),
            "step_started" => LoopEvent::StepStarted(Box::new(payload(data)?)),
            "step_spawned" => LoopEvent::StepSpawned(payload(data)?),
            "step_finished" => LoopEvent::StepFinished(Box::new(payload(data)?)),
            "step_interrupted" => LoopEvent::StepInterrupted(payload(data)?),
            "gate_opened" => LoopEvent::GateOpened(Box::new(payload(data)?)),
            "gate_resolved" => LoopEvent::GateResolved(payload(data)?),
            "gate_cancelled" => LoopEvent::GateCancelled(payload(data)?),
            "instruction_received" => LoopEvent::InstructionReceived(payload(data)?),
            "warning" => LoopEvent::Warning(payload(data)?),
            _ => return Ok(None),
        };
        Ok(Some(ev))
    }

    /// Whether a state-driving enum holds a value this build does not know. Such a
    /// record parses (so it can be displayed) but makes the journal read-only here,
    /// because this build cannot tell what transition it records. Display-only enums
    /// (skip/cancel/close reasons, surfaces, actors) never count.
    pub fn has_unknown_values(&self) -> bool {
        match self {
            LoopEvent::LoopCreated(p) => p.workspace.is_unknown(),
            LoopEvent::LoopPaused(p) => p.mode.is_unknown(),
            LoopEvent::LoopStopRequested(p) => p.mode.is_unknown(),
            LoopEvent::LoopFinished(p) => p.status.is_unknown(),
            LoopEvent::IterationFinished(p) => p.decision.is_unknown(),
            LoopEvent::StepStarted(p) => {
                p.kind.is_unknown() || p.access.is_some_and(|a| a.is_unknown())
            }
            LoopEvent::StepFinished(p) => p.outcome.is_unknown(),
            LoopEvent::GateOpened(p) => {
                p.kind.is_unknown()
                    || p.options
                        .iter()
                        .any(|o| o.outcome.is_some_and(|x| x.is_unknown()))
            }
            _ => false,
        }
    }
}

/// Why a line could not be read as a known record.
#[derive(Debug, Clone, PartialEq)]
pub enum UnknownReason {
    /// The envelope's `v` is newer than [`RECORD_V`] (or 0).
    Version(u16),
    /// A kind this build does not know.
    Kind,
    /// A known kind whose payload does not decode.
    Invalid(String),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Body {
    Known(LoopEvent),
    /// Kept (with its payload) so it can be displayed and so the cursor still advances.
    Unknown {
        reason: UnknownReason,
        data: Value,
    },
}

/// One decoded journal line.
#[derive(Debug, Clone, PartialEq)]
pub struct LoadedRecord {
    pub seq: u64,
    pub ts: u64,
    pub kind: String,
    pub op: Option<String>,
    /// For a known kind, from [`KINDS`]; for an unknown kind, the line's own flag.
    pub anc: bool,
    pub body: Body,
}

impl LoadedRecord {
    pub fn event(&self) -> Option<&LoopEvent> {
        match &self.body {
            Body::Known(ev) => Some(ev),
            Body::Unknown { .. } => None,
        }
    }

    /// A record this build cannot interpret and may not ignore.
    pub fn blocks_writing(&self) -> bool {
        match &self.body {
            Body::Known(ev) => ev.has_unknown_values(),
            Body::Unknown { .. } => !self.anc,
        }
    }

    /// Re-encode as a journal line (no trailing newline). Unknown bodies keep their data.
    pub fn to_record(&self) -> Record {
        let data = match &self.body {
            Body::Known(ev) => ev.to_data(),
            Body::Unknown { data, .. } => data.clone(),
        };
        let v = match &self.body {
            Body::Unknown {
                reason: UnknownReason::Version(v),
                ..
            } => *v,
            _ => RECORD_V,
        };
        Record {
            v,
            seq: self.seq,
            ts: self.ts,
            kind: self.kind.clone(),
            op: self.op.clone(),
            anc: self.anc,
            data,
        }
    }
}

/// Decode one line. `Err` only when the envelope itself is unusable (not JSON, or missing
/// `v`/`seq`/`ts`/`kind`); everything else becomes a [`LoadedRecord`].
pub fn decode_line(line: &str) -> Result<LoadedRecord, String> {
    let rec: Record = serde_json::from_str(line).map_err(|e| e.to_string())?;
    let unknown = |reason, anc, data| LoadedRecord {
        seq: rec.seq,
        ts: rec.ts,
        kind: rec.kind.clone(),
        op: rec.op.clone(),
        anc,
        body: Body::Unknown { reason, data },
    };
    if rec.v != RECORD_V {
        return Ok(unknown(UnknownReason::Version(rec.v), rec.anc, rec.data));
    }
    let Some(info) = kind_info(&rec.kind) else {
        return Ok(unknown(UnknownReason::Kind, rec.anc, rec.data));
    };
    match LoopEvent::from_data(&rec.kind, rec.data.clone()) {
        Ok(Some(ev)) => Ok(LoadedRecord {
            seq: rec.seq,
            ts: rec.ts,
            kind: rec.kind.clone(),
            op: rec.op.clone(),
            anc: info.ancillary,
            body: Body::Known(ev),
        }),
        Ok(None) => Ok(unknown(UnknownReason::Kind, rec.anc, rec.data)),
        Err(e) => Ok(unknown(UnknownReason::Invalid(e), info.ancillary, rec.data)),
    }
}

/// Encode one record as a journal line (no trailing newline).
pub fn encode_record(seq: u64, ts: u64, op: Option<&str>, event: &LoopEvent) -> String {
    debug_assert!(
        !event.has_unknown_values(),
        "writers never serialize an Unknown value"
    );
    let rec = Record {
        v: RECORD_V,
        seq,
        ts,
        kind: event.kind().to_string(),
        op: op.map(str::to_string),
        anc: event.is_ancillary(),
        data: event.to_data(),
    };
    serde_json::to_string(&rec).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn kinds_and_reserved_kinds_are_disjoint_and_unique() {
        let mut names: Vec<&str> = KINDS.iter().map(|k| k.name).collect();
        names.sort();
        let len = names.len();
        names.dedup();
        assert_eq!(names.len(), len, "duplicate kind");
        for r in RESERVED_KINDS {
            assert!(kind_info(r).is_none(), "{r} is both known and reserved");
        }
    }

    #[test]
    fn anc_comes_from_the_table_for_known_kinds() {
        // A known critical kind claiming anc on the line is still critical.
        let r = decode_line(r#"{"v":1,"seq":3,"ts":1,"kind":"loop_started","anc":true,"data":{}}"#)
            .unwrap();
        assert!(!r.anc);
        let r = decode_line(
            r#"{"v":1,"seq":3,"ts":1,"kind":"warning","data":{"code":"x","message":"y"}}"#,
        )
        .unwrap();
        assert!(r.anc);
    }

    #[test]
    fn payloadless_kinds_accept_missing_or_null_data() {
        for line in [
            r#"{"v":1,"seq":3,"ts":1,"kind":"loop_started"}"#,
            r#"{"v":1,"seq":3,"ts":1,"kind":"loop_started","data":null}"#,
            r#"{"v":1,"seq":3,"ts":1,"kind":"loop_resumed","data":{}}"#,
        ] {
            let r = decode_line(line).unwrap();
            assert!(matches!(r.body, Body::Known(_)), "{line}");
        }
    }

    #[test]
    fn future_lines_decode_as_documented() {
        let critical = decode_line(
            r#"{"v":1,"seq":9,"ts":1,"kind":"proposal_created","data":{"proposal_id":"p1"}}"#,
        )
        .unwrap();
        assert!(matches!(
            critical.body,
            Body::Unknown {
                reason: UnknownReason::Kind,
                ..
            }
        ));
        assert!(critical.blocks_writing());

        let hint =
            decode_line(r#"{"v":1,"seq":9,"ts":1,"kind":"future_hint","anc":true,"data":{"x":1}}"#)
                .unwrap();
        assert!(hint.anc && !hint.blocks_writing());

        let v2 = decode_line(r#"{"v":2,"seq":9,"ts":1,"kind":"step_finished","data":{}}"#).unwrap();
        assert!(matches!(
            v2.body,
            Body::Unknown {
                reason: UnknownReason::Version(2),
                ..
            }
        ));
        assert!(v2.blocks_writing());

        let new_value = decode_line(r#"{"v":1,"seq":9,"ts":1,"kind":"step_finished","data":{"step_id":"x.a1","outcome":"partial","elapsed_ms":1}}"#).unwrap();
        assert!(matches!(new_value.body, Body::Known(_)));
        assert!(
            new_value.blocks_writing(),
            "an unknown outcome drives state"
        );

        let new_field = decode_line(r#"{"v":1,"seq":9,"ts":1,"kind":"step_finished","data":{"step_id":"x.a1","outcome":"ok","elapsed_ms":1,"future_field":{"a":1}}}"#).unwrap();
        assert!(matches!(new_field.body, Body::Known(_)));
        assert!(!new_field.blocks_writing());

        let broken =
            decode_line(r#"{"v":1,"seq":9,"ts":1,"kind":"step_finished","data":{"outcome":"ok"}}"#)
                .unwrap();
        assert!(matches!(
            broken.body,
            Body::Unknown {
                reason: UnknownReason::Invalid(_),
                ..
            }
        ));
        assert!(
            broken.blocks_writing(),
            "an undecodable critical record is critical"
        );

        assert!(decode_line("not json").is_err());
        assert!(decode_line(r#"{"seq":1,"ts":1,"kind":"loop_started"}"#).is_err());
    }

    #[test]
    fn display_only_unknown_values_do_not_block_writing() {
        let r = decode_line(r#"{"v":1,"seq":9,"ts":1,"kind":"node_skipped","data":{"node":"a","reason":"teleported"}}"#).unwrap();
        assert!(matches!(r.body, Body::Known(_)));
        assert!(!r.blocks_writing());
    }

    #[test]
    fn encode_omits_absent_options_and_empty_vectors() {
        let ev = LoopEvent::StepFinished(Box::new(StepFinished {
            step_id: "plan.a1".into(),
            outcome: Outcome::Ok,
            elapsed_ms: 5,
            exit_code: None,
            verdict: None,
            output: None,
            excerpt: None,
            log_tail: None,
            error: None,
            feedback: vec![],
            cancel: None,
        }));
        let line = encode_record(4, 10, None, &ev);
        assert_eq!(
            serde_json::from_str::<Value>(&line).unwrap(),
            json!({"v":1,"seq":4,"ts":10,"kind":"step_finished","data":{"step_id":"plan.a1","outcome":"ok","elapsed_ms":5}})
        );
        let back = decode_line(&line).unwrap();
        assert_eq!(back.event(), Some(&ev));
    }

    #[test]
    fn unknown_records_re_encode_with_their_payload() {
        let line = r#"{"v":1,"seq":9,"ts":1,"kind":"future_hint","anc":true,"data":{"x":1}}"#;
        let r = decode_line(line).unwrap();
        assert_eq!(
            serde_json::to_value(r.to_record()).unwrap(),
            serde_json::from_str::<Value>(line).unwrap()
        );
    }
}
