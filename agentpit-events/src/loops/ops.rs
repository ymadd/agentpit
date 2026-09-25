//! Operations a client sends to a running loop (design §7).
//!
//! Every operation carries a client-chosen `op_id`: the loop journal records it on every
//! record the operation causes, so a retried operation is answered `duplicate` with the
//! original seq instead of being applied twice. A rejected operation is never journaled.
//!
//! This module is the wire shape only. Deciding what an accepted operation writes is the
//! runner's job (phase 2).

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::record::{PauseMode, StopMode};
use super::string_enum;

/// The operation, tagged by `"op"`. An operation from a newer client parses as
/// [`LoopOp::Unknown`] and is answered `unsupported`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum LoopOp {
    /// Created → Running.
    Start,
    Pause {
        mode: PauseMode,
    },
    Resume,
    Stop {
        mode: StopMode,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    /// Cancel one running agent/check step.
    CancelStep {
        step_id: String,
    },
    /// Answer a gate (approval, step_error, budget, recovery).
    ResolveGate {
        gate_id: String,
        option: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        comment: Option<String>,
    },
    /// Give the next agent step (of `target`, or any) an instruction.
    Instruct {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        target: Option<String>,
    },
    /// Change the budget; absent fields keep their value.
    SetBudget {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max_steps: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max_active_secs: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max_parallel: Option<u32>,
    },
    #[serde(other)]
    Unknown,
}

impl LoopOp {
    pub fn name(&self) -> &'static str {
        match self {
            LoopOp::Start => "start",
            LoopOp::Pause { .. } => "pause",
            LoopOp::Resume => "resume",
            LoopOp::Stop { .. } => "stop",
            LoopOp::CancelStep { .. } => "cancel_step",
            LoopOp::ResolveGate { .. } => "resolve_gate",
            LoopOp::Instruct { .. } => "instruct",
            LoopOp::SetBudget { .. } => "set_budget",
            LoopOp::Unknown => "unknown",
        }
    }
}

/// `{"loop_id","op_id","op":…, …op fields, "expect_seq"?}` — flat on the wire.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpRequest {
    pub loop_id: String,
    pub op_id: String,
    /// Optimistic concurrency: refuse with `conflict` unless the journal head is this seq.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expect_seq: Option<u64>,
    #[serde(flatten)]
    pub op: LoopOp,
}

string_enum! {
    pub enum OpOutcome {
        /// Journaled; `seq` is the first record it caused.
        Applied = "applied",
        /// This op_id was already applied; `seq` is the original first record.
        Duplicate = "duplicate",
        /// Already in the requested state (e.g. pause while paused); nothing journaled.
        Noop = "noop",
    }
}

/// Success. Arrives on the requesting connection AFTER the records it caused
/// (read-your-writes).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OpResult {
    pub op_id: String,
    pub outcome: OpOutcome,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seq: Option<u64>,
    pub head_seq: u64,
    /// Op-specific extras, e.g. `{"instruction_id":"in2"}`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
}

string_enum! {
    pub enum ErrorCode {
        BadRequest = "bad_request",
        /// Unknown op, or a feature this runner does not have.
        Unsupported = "unsupported",
        NotFound = "not_found",
        /// Not possible in the loop's (or the gate's, step's) current state.
        InvalidState = "invalid_state",
        /// `expect_seq` did not match.
        Conflict = "conflict",
        Validation = "validation",
        /// The journal is read-only for this build.
        ReadOnly = "read_only",
        Busy = "busy",
        Gone = "gone",
        Unavailable = "unavailable",
        Internal = "internal",
    }
}

/// Failure: a stable code, one sentence ending with the next step, optional details
/// (e.g. who already answered a gate).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OpError {
    pub code: ErrorCode,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn requests_are_flat_and_round_trip() {
        let wire = json!({
            "loop_id": "lp-0199a1b2c3d47e5f8a9b0c1d2e3f4a5b",
            "op_id": "0199a1c0-7b1e-7f00-9c11-3e5f6a7b8c9d",
            "op": "resolve_gate",
            "gate_id": "g1",
            "option": "approve"
        });
        let req: OpRequest = serde_json::from_value(wire.clone()).unwrap();
        assert_eq!(
            req.op,
            LoopOp::ResolveGate {
                gate_id: "g1".into(),
                option: "approve".into(),
                comment: None
            }
        );
        assert_eq!(serde_json::to_value(&req).unwrap(), wire);
    }

    #[test]
    fn unknown_ops_parse_with_their_extra_fields() {
        let req: OpRequest = serde_json::from_value(json!({
            "loop_id": "lp-0199a1b2c3d47e5f8a9b0c1d2e3f4a5b",
            "op_id": "0199a1c2-0000-7000-8000-000000000009",
            "op": "teleport",
            "to": "x"
        }))
        .unwrap();
        assert_eq!(req.op, LoopOp::Unknown);
        assert_eq!(req.op.name(), "unknown");
    }

    #[test]
    fn unit_ops_need_no_fields() {
        let req: OpRequest = serde_json::from_value(json!({
            "loop_id": "lp-0199a1b2c3d47e5f8a9b0c1d2e3f4a5b",
            "op_id": "0199a1c2-0000-7000-8000-000000000001",
            "op": "start",
            "expect_seq": 3
        }))
        .unwrap();
        assert_eq!(req.op, LoopOp::Start);
        assert_eq!(req.expect_seq, Some(3));
    }

    #[test]
    fn results_and_errors_omit_absent_fields() {
        let ok = OpResult {
            op_id: "x".repeat(8),
            outcome: OpOutcome::Noop,
            seq: None,
            head_seq: 12,
            result: None,
        };
        assert_eq!(
            serde_json::to_value(&ok).unwrap(),
            json!({"op_id": "xxxxxxxx", "outcome": "noop", "head_seq": 12})
        );
        let err: OpError = serde_json::from_value(json!({
            "code": "invalid_state",
            "message": "g1 is already resolved; refresh the inbox.",
            "details": {"status": "resolved", "option": "approve", "by": {"kind": "human"}}
        }))
        .unwrap();
        assert_eq!(err.code, ErrorCode::InvalidState);
        let future: OpError =
            serde_json::from_value(json!({"code": "rate_limited", "message": "slow down."}))
                .unwrap();
        assert!(future.code.is_unknown());
    }
}
