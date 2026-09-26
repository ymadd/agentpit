//! Deciding what an operation writes (design §7's table). Pure: the runner commits the
//! records, `admit` double-checks them, and a rejected operation is never journaled.

use agentpit_events::loops::*;
use serde_json::json;

use super::records::gate_follow_up;

/// What an accepted operation does.
#[derive(Debug, Clone, PartialEq)]
pub enum OpPlan {
    /// Already in the requested state; nothing is written.
    Noop,
    /// Write these records (all tagged with the op id). `result` rides on the answer.
    Commit {
        events: Vec<LoopEvent>,
        result: Option<serde_json::Value>,
    },
    /// Cancel a running compute step: its `step_finished{cancelled}` comes when the
    /// process has ended.
    CancelStep { step_id: String },
}

#[derive(Debug, Clone, PartialEq)]
pub enum Admission {
    /// This op id was applied before: the original first seq.
    Duplicate {
        seq: u64,
    },
    Plan(OpPlan),
}

pub fn op_error(code: ErrorCode, message: impl Into<String>) -> OpError {
    OpError {
        code,
        message: message.into(),
        details: None,
    }
}

fn invalid_state(state: &LoopState, what: &str) -> OpError {
    op_error(
        ErrorCode::InvalidState,
        format!(
            "cannot {what} a loop that is {}; refresh its status and pick an action it allows",
            state.status
        ),
    )
}

/// The checks every operation passes before its own rules (design §7 "共通").
pub fn admit_op(
    state: &LoopState,
    loop_id: &str,
    req: &OpRequest,
    by: &Actor,
) -> Result<Admission, OpError> {
    if req.loop_id != loop_id {
        return Err(op_error(
            ErrorCode::NotFound,
            format!(
                "this runner serves {loop_id}, not {}; ask the daemon for that loop's socket",
                req.loop_id
            ),
        ));
    }
    if !is_valid_op_id(&req.op_id) {
        return Err(op_error(
            ErrorCode::BadRequest,
            "op_id must be 8-64 characters of [A-Za-z0-9._-]; send a fresh UUID",
        ));
    }
    if let Some(&seq) = state.ops.get(&req.op_id) {
        return Ok(Admission::Duplicate { seq });
    }
    if let Err(reason) = state.writable() {
        return Err(op_error(ErrorCode::ReadOnly, format!("{reason}")));
    }
    if let Some(expected) = req.expect_seq
        && expected != state.head_seq
    {
        return Err(OpError {
            code: ErrorCode::Conflict,
            message: format!(
                "the loop moved on (head is {}, you expected {expected}); refresh and retry",
                state.head_seq
            ),
            details: Some(json!({"head_seq": state.head_seq})),
        });
    }
    classify(state, &req.op, by).map(Admission::Plan)
}

/// The per-op rules.
pub fn classify(state: &LoopState, op: &LoopOp, by: &Actor) -> Result<OpPlan, OpError> {
    use LoopStatus as S;
    let status = state.status;
    let commit = |events: Vec<LoopEvent>| {
        Ok(OpPlan::Commit {
            events,
            result: None,
        })
    };
    match op {
        LoopOp::Start => match status {
            S::Created => commit(vec![LoopEvent::LoopStarted]),
            S::Running | S::Paused => Ok(OpPlan::Noop),
            _ => Err(invalid_state(state, "start")),
        },
        LoopOp::Pause { mode } => {
            if mode.is_unknown() {
                return Err(op_error(
                    ErrorCode::Validation,
                    "pause mode must be drain or cancel",
                ));
            }
            match status {
                S::Running => commit(vec![LoopEvent::LoopPaused(LoopPaused {
                    mode: *mode,
                    note: None,
                })]),
                S::Paused => Ok(OpPlan::Noop),
                _ => Err(invalid_state(state, "pause")),
            }
        }
        LoopOp::Resume => match status {
            S::Paused => commit(vec![LoopEvent::LoopResumed]),
            S::Running => Ok(OpPlan::Noop),
            _ => Err(invalid_state(state, "resume")),
        },
        LoopOp::Stop { mode, reason } => {
            if mode.is_unknown() {
                return Err(op_error(
                    ErrorCode::Validation,
                    "stop mode must be graceful or cancel",
                ));
            }
            let event = LoopEvent::LoopStopRequested(LoopStopRequested {
                mode: *mode,
                reason: reason
                    .as_deref()
                    .map(str::trim)
                    .filter(|r| !r.is_empty())
                    .map(|r| clamp_text(r, MAX_SHORT_BYTES)),
            });
            match status {
                S::Created | S::Running | S::Paused => commit(vec![event]),
                // Escalating a graceful stop to a cancelling one is allowed.
                S::Stopping
                    if state.stop == Some(StopMode::Graceful) && *mode == StopMode::Cancel =>
                {
                    commit(vec![event])
                }
                S::Stopping => Ok(OpPlan::Noop),
                _ => Err(invalid_state(state, "stop")),
            }
        }
        LoopOp::CancelStep { step_id } => {
            let Some(step) = state.step(step_id) else {
                return Err(op_error(
                    ErrorCode::NotFound,
                    format!("{step_id} is not a step of this loop; check the step id"),
                ));
            };
            if !step.kind.is_compute() || step.status != StepStatus::Running {
                return Err(op_error(
                    ErrorCode::InvalidState,
                    format!(
                        "{step_id} is not a running agent or check step; only those can be cancelled"
                    ),
                ));
            }
            Ok(OpPlan::CancelStep {
                step_id: step_id.clone(),
            })
        }
        LoopOp::ResolveGate {
            gate_id,
            option,
            comment,
        } => {
            let Some(gate) = state.gate(gate_id) else {
                return Err(op_error(
                    ErrorCode::NotFound,
                    format!("{gate_id} is not a gate of this loop; refresh the inbox"),
                ));
            };
            if gate.status != GateStatus::Open {
                let details = match &gate.resolution {
                    Some(r) => json!({
                        "status": gate.status.as_str(),
                        "option": r.option,
                        "by": r.by,
                        "seq": r.seq,
                    }),
                    None => json!({
                        "status": gate.status.as_str(),
                        "reason": gate.cancel_reason.map(|r| r.as_str()),
                    }),
                };
                let who = match &gate.resolution {
                    Some(r) => format!(
                        "was already answered {} by {}",
                        r.option,
                        r.by.client.as_deref().unwrap_or(r.by.kind.as_str())
                    ),
                    None => "was closed".to_string(),
                };
                return Err(OpError {
                    code: ErrorCode::InvalidState,
                    message: format!("{gate_id} {who}; refresh the inbox"),
                    details: Some(details),
                });
            }
            if !gate.options.iter().any(|o| &o.id == option) {
                let ids: Vec<&str> = gate.options.iter().map(|o| o.id.as_str()).collect();
                return Err(OpError {
                    code: ErrorCode::Validation,
                    message: format!(
                        "{gate_id} has no option {option:?}; answer one of {}",
                        ids.join(", ")
                    ),
                    details: Some(json!({"options": ids})),
                });
            }
            let comment = comment
                .as_deref()
                .map(str::trim)
                .filter(|c| !c.is_empty())
                .map(str::to_string);
            if comment.as_ref().is_some_and(|c| c.len() > MAX_TEXT_BYTES) {
                return Err(op_error(
                    ErrorCode::Validation,
                    format!("the comment is longer than {MAX_TEXT_BYTES} bytes; shorten it"),
                ));
            }
            let mut events = vec![LoopEvent::GateResolved(GateResolved {
                gate_id: gate_id.clone(),
                option: option.clone(),
                comment,
                by: by.clone(),
            })];
            events.extend(gate_follow_up(state, gate, option, by));
            commit(events)
        }
        LoopOp::Instruct { text, target } => {
            if status.is_terminal() {
                return Err(invalid_state(state, "instruct"));
            }
            let text = text.trim();
            if text.is_empty() {
                return Err(op_error(
                    ErrorCode::Validation,
                    "the instruction is empty; write what the next agent step should do",
                ));
            }
            if text.len() > MAX_TEXT_BYTES {
                return Err(op_error(
                    ErrorCode::Validation,
                    format!("the instruction is longer than {MAX_TEXT_BYTES} bytes; shorten it"),
                ));
            }
            if let Some(t) = target {
                let is_agent = state
                    .blueprint
                    .as_ref()
                    .and_then(|bp| bp.node(t))
                    .is_some_and(|n| matches!(n.spec, NodeSpec::Agent(_)));
                if !is_agent {
                    return Err(op_error(
                        ErrorCode::Validation,
                        format!(
                            "{t} is not an agent node of this loop; target an agent node or none"
                        ),
                    ));
                }
            }
            let id = instruction_id(state.instructions.len() + 1);
            Ok(OpPlan::Commit {
                events: vec![LoopEvent::InstructionReceived(InstructionReceived {
                    instruction_id: id.clone(),
                    text: text.to_string(),
                    target: target.clone(),
                    by: by.clone(),
                })],
                result: Some(json!({"instruction_id": id})),
            })
        }
        LoopOp::SetBudget {
            max_steps,
            max_active_secs,
            max_parallel,
        } => {
            if status.is_terminal() {
                return Err(invalid_state(state, "change the budget of"));
            }
            let budget = Budget {
                max_steps: max_steps.unwrap_or(state.budget.max_steps),
                max_active_secs: max_active_secs.unwrap_or(state.budget.max_active_secs),
                max_parallel: max_parallel.unwrap_or(state.budget.max_parallel),
            };
            if let Some(why) = budget.out_of_range() {
                return Err(op_error(
                    ErrorCode::Validation,
                    format!("{why}; pick a value inside the limits"),
                ));
            }
            if budget.max_steps < state.usage.steps {
                return Err(op_error(
                    ErrorCode::Validation,
                    format!(
                        "the loop already used {} steps; max_steps cannot go below that",
                        state.usage.steps
                    ),
                ));
            }
            if budget == state.budget {
                return Ok(OpPlan::Noop);
            }
            commit(vec![LoopEvent::BudgetChanged(BudgetChanged {
                budget,
                by: by.clone(),
            })])
        }
        LoopOp::Unknown => Err(op_error(
            ErrorCode::Unsupported,
            "this runner does not know that operation (it predates your agentpit); \
             stop the runner with `agentpit loop` after upgrading",
        )),
    }
}

/// An `admit` refusal of an operation's records, as the error the client sees.
pub fn rejection_error(r: &Rejection) -> OpError {
    let code = match r.code {
        RejectCode::BadOption
        | RejectCode::EmptyText
        | RejectCode::TooLarge
        | RejectCode::BudgetOutOfRange
        | RejectCode::UnknownValue => ErrorCode::Validation,
        RejectCode::GateNotFound | RejectCode::StepNotFound => ErrorCode::NotFound,
        _ => ErrorCode::InvalidState,
    };
    OpError {
        code,
        message: format!("the loop refused it ({r}); refresh its status and retry"),
        details: Some(json!({"reject": r.code.as_str()})),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOOP: &str = "lp-0199a1b2c3d47e5f8a9b0c1d2e3f4a5b";

    fn state_after(extra: Vec<LoopEvent>) -> LoopState {
        let doc = json!({
            "schema": "agentpit.blueprint/1",
            "name": "ops",
            "nodes": [
                {"id": "work", "kind": "agent", "backend": "codex", "task": "do it"},
                {"id": "ok", "kind": "gate", "prompt": "Ship?"}
            ],
            "edges": [{"from": "work", "to": "ok"}]
        });
        let v = validate(&doc, &ValidateEnv::default());
        let bp = v.blueprint.unwrap();
        let mut events = vec![
            LoopEvent::LoopCreated(Box::new(LoopCreated {
                loop_id: LOOP.into(),
                uid: "9c41e07a2b5d4f18".into(),
                title: "t".into(),
                blueprint: FrozenBlueprint {
                    name: bp.name,
                    scope: BlueprintScope::Inline,
                    path: None,
                    rev: v.rev,
                    doc,
                },
                inputs: Default::default(),
                cwd: "/w".into(),
                repo_root: None,
                workspace: WorkspaceMode::InPlace,
                budget: bp.budget,
                origin: Origin {
                    surface: Surface::Cli,
                    client: None,
                    session_id: None,
                },
                root_run_id: None,
            })),
            LoopEvent::WriterOpened(WriterOpened {
                epoch: 1,
                pid: 1,
                start_id: String::new(),
                build: "t".into(),
                schema_minor: SCHEMA_MINOR,
                truncated_tail_bytes: 0,
            }),
        ];
        events.extend(extra);
        let mut state = LoopState::default();
        for (i, ev) in events.into_iter().enumerate() {
            state.admit(&ev).unwrap();
            state.apply(&LoadedRecord {
                seq: i as u64 + 1,
                ts: 1,
                kind: ev.kind().into(),
                op: (i == 2).then(|| "op-first-0001".into()),
                anc: ev.is_ancillary(),
                raw: String::new(),
                body: Body::Known(ev),
            });
        }
        state
    }

    fn by() -> Actor {
        Actor::human("test")
    }

    fn req(op: LoopOp) -> OpRequest {
        OpRequest {
            loop_id: LOOP.into(),
            op_id: "op-0000-0001".into(),
            expect_seq: None,
            op,
        }
    }

    fn code(r: Result<Admission, OpError>) -> ErrorCode {
        r.unwrap_err().code
    }

    #[test]
    fn lifecycle_ops_follow_the_table() {
        let created = state_after(vec![]);
        assert_eq!(
            classify(&created, &LoopOp::Start, &by()).unwrap(),
            OpPlan::Commit {
                events: vec![LoopEvent::LoopStarted],
                result: None
            }
        );
        let pause = LoopOp::Pause {
            mode: PauseMode::Drain,
        };
        assert_eq!(
            classify(&created, &pause, &by()).unwrap_err().code,
            ErrorCode::InvalidState
        );
        let running = state_after(vec![LoopEvent::LoopStarted]);
        assert_eq!(
            classify(&running, &LoopOp::Start, &by()).unwrap(),
            OpPlan::Noop
        );
        assert_eq!(
            classify(&running, &LoopOp::Resume, &by()).unwrap(),
            OpPlan::Noop
        );
        let graceful = state_after(vec![
            LoopEvent::LoopStarted,
            LoopEvent::LoopStopRequested(LoopStopRequested {
                mode: StopMode::Graceful,
                reason: None,
            }),
        ]);
        let stop = |mode| LoopOp::Stop { mode, reason: None };
        assert_eq!(
            classify(&graceful, &stop(StopMode::Graceful), &by()).unwrap(),
            OpPlan::Noop
        );
        assert!(matches!(
            classify(&graceful, &stop(StopMode::Cancel), &by()).unwrap(),
            OpPlan::Commit { .. }
        ));
    }

    #[test]
    fn the_common_checks_come_first() {
        let running = state_after(vec![LoopEvent::LoopStarted]);
        let mut r = req(LoopOp::Resume);
        r.loop_id = "lp-00000000000000000000000000000001".into();
        assert_eq!(
            code(admit_op(&running, LOOP, &r, &by())),
            ErrorCode::NotFound
        );
        let mut r = req(LoopOp::Resume);
        r.op_id = "bad id".into();
        assert_eq!(
            code(admit_op(&running, LOOP, &r, &by())),
            ErrorCode::BadRequest
        );
        // The op that started the loop is recognised as a duplicate.
        let mut r = req(LoopOp::Start);
        r.op_id = "op-first-0001".into();
        assert_eq!(
            admit_op(&running, LOOP, &r, &by()).unwrap(),
            Admission::Duplicate { seq: 3 }
        );
        let mut r = req(LoopOp::Resume);
        r.expect_seq = Some(1);
        assert_eq!(
            code(admit_op(&running, LOOP, &r, &by())),
            ErrorCode::Conflict
        );
        assert_eq!(
            code(admit_op(&running, LOOP, &req(LoopOp::Unknown), &by())),
            ErrorCode::Unsupported
        );
    }

    #[test]
    fn instructions_and_budgets_are_validated() {
        let running = state_after(vec![LoopEvent::LoopStarted]);
        let instruct = |text: &str, target: Option<&str>| LoopOp::Instruct {
            text: text.into(),
            target: target.map(str::to_string),
        };
        assert_eq!(
            classify(&running, &instruct("  ", None), &by())
                .unwrap_err()
                .code,
            ErrorCode::Validation
        );
        assert_eq!(
            classify(&running, &instruct("x", Some("ok")), &by())
                .unwrap_err()
                .code,
            ErrorCode::Validation,
            "a gate is not an agent"
        );
        match classify(&running, &instruct(" keep it small ", Some("work")), &by()).unwrap() {
            OpPlan::Commit { events, result } => {
                assert_eq!(result, Some(json!({"instruction_id": "in1"})));
                assert!(matches!(
                    &events[0],
                    LoopEvent::InstructionReceived(i) if i.text == "keep it small"
                ));
            }
            other => panic!("{other:?}"),
        }
        let budget = |steps| LoopOp::SetBudget {
            max_steps: Some(steps),
            max_active_secs: None,
            max_parallel: None,
        };
        assert_eq!(
            classify(&running, &budget(running.budget.max_steps), &by()).unwrap(),
            OpPlan::Noop
        );
        assert_eq!(
            classify(&running, &budget(0), &by()).unwrap_err().code,
            ErrorCode::Validation
        );
        assert!(matches!(
            classify(&running, &budget(99), &by()).unwrap(),
            OpPlan::Commit { .. }
        ));
    }

    #[test]
    fn cancel_step_needs_a_running_compute_step() {
        let running = state_after(vec![LoopEvent::LoopStarted]);
        let cancel = |id: &str| LoopOp::CancelStep { step_id: id.into() };
        assert_eq!(
            classify(&running, &cancel("work.a1"), &by())
                .unwrap_err()
                .code,
            ErrorCode::NotFound
        );
    }
}
