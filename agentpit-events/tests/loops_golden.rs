//! Golden tests for the loop schema (docs/workspace-loop-design.md): every wire shape is
//! pinned by a fixture line, and the fixture journals replay to the documented states.

use agentpit_events::loops::*;
use serde_json::{json, Value};

const KINDS_V1: &str = include_str!("fixtures/loops/kinds_v1.jsonl");
const JOURNAL: &str = include_str!("fixtures/loops/journal_fix_until_green.jsonl");
const RECOVERY: &str = include_str!("fixtures/loops/journal_recovery.jsonl");
const OPS_V1: &str = include_str!("fixtures/loops/ops_v1.jsonl");
const BLUEPRINT: &str = include_str!("fixtures/loops/blueprint_fix_until_green.json");

fn records(text: &str) -> Vec<LoadedRecord> {
    let scan = scan_bytes(text.as_bytes());
    assert!(scan.is_clean(), "{:?}", scan.issues);
    scan.records
}

fn state_after(text: &str, seq: u64) -> LoopState {
    let recs = records(text);
    replay(recs.iter().filter(|r| r.seq <= seq))
}

fn event(line: &str) -> LoopEvent {
    decode_line(line).unwrap().event().unwrap().clone()
}

fn code(result: Result<(), Rejection>) -> RejectCode {
    result.expect_err("expected a rejection").code
}

#[test]
fn every_kind_has_exactly_one_fixture_line_that_round_trips() {
    let lines: Vec<&str> = KINDS_V1.lines().collect();
    assert_eq!(lines.len(), KINDS.len());
    for info in KINDS {
        let hits: Vec<&&str> = lines
            .iter()
            .filter(|l| serde_json::from_str::<Value>(l).unwrap()["kind"] == info.name)
            .collect();
        assert_eq!(hits.len(), 1, "{}", info.name);
    }
    for line in lines {
        let rec = decode_line(line).unwrap();
        let ev = rec.event().unwrap_or_else(|| panic!("unknown: {line}"));
        assert!(!rec.blocks_writing(), "{line}");
        assert_eq!(rec.anc, ev.is_ancillary());
        let again = encode_record(rec.seq, rec.ts, rec.op.as_deref(), ev);
        assert_eq!(
            serde_json::from_str::<Value>(&again).unwrap(),
            serde_json::from_str::<Value>(line).unwrap(),
            "{line}"
        );
    }
}

#[test]
fn fixture_rev_is_pinned() {
    let doc: Value = serde_json::from_str(BLUEPRINT).unwrap();
    assert_eq!(blueprint_rev(&doc), "b1-6c29e826fc3c2a74");
}

#[test]
fn fix_until_green_replays_to_succeeded() {
    let state = replay(&records(JOURNAL));
    assert_eq!(state.status, LoopStatus::Succeeded);
    assert_eq!(state.head_seq, 26);
    assert!(state.warnings.is_empty(), "{:?}", state.warnings);
    assert_eq!(state.usage.steps, 5);
    // Wall-clock with a compute step running: 49013 + 81230 + 14543 + 66306 + 14789.
    assert_eq!(state.usage.active_ms, 225_881);
    assert_eq!(
        state.instance("test", &[1]).unwrap().outcome,
        Some(Outcome::Fail)
    );
    assert_eq!(
        state.instance("test", &[2]).unwrap().outcome,
        Some(Outcome::Ok)
    );
    assert_eq!(
        state.repeats["fix"].decisions,
        vec![IterationDecision::Continue, IterationDecision::Break]
    );
    assert_eq!(state.ops["0199a1c0-7b1e-7f00-9c11-3e5f6a7b8c9d"], 23);
    assert_eq!(state.ops["0199a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5b"], 1);
    assert_eq!(state.epoch, 1);
    assert!(state.writable().is_ok());
    assert_eq!(
        state
            .gate("g1")
            .unwrap()
            .resolution
            .as_ref()
            .unwrap()
            .option,
        "approve"
    );
}

#[test]
fn every_fixture_record_is_admissible_in_order() {
    for text in [JOURNAL, RECOVERY] {
        let mut state = LoopState::default();
        for rec in records(text) {
            if let Some(ev) = rec.event() {
                state
                    .admit(ev)
                    .unwrap_or_else(|r| panic!("seq {} {}: {r}", rec.seq, rec.kind));
            }
            state.apply(&rec);
        }
    }
}

#[test]
fn summary_answers_which_loop_which_stage_which_agent() {
    let s = state_after(JOURNAL, 9).summary().unwrap();
    assert_eq!(s.loop_id, "lp-0199a1b2c3d47e5f8a9b0c1d2e3f4a5b");
    assert_eq!(s.status, LoopStatus::Running);
    assert!(!s.waiting);
    assert_eq!(s.active.len(), 1);
    assert_eq!(s.active[0].step_id, "implement.i1.a1");
    assert_eq!(s.active[0].assignee.as_ref().unwrap().backend, "codex");
    assert_eq!(
        s.iterations,
        vec![IterationHead {
            repeat: "fix".into(),
            outer: vec![],
            n: 1,
            max: 4,
            open: true
        }]
    );

    let s = state_after(JOURNAL, 22).summary().unwrap();
    assert!(s.waiting, "only the sign-off gate remains");
    assert_eq!(s.open_gate_count, 1);
    assert_eq!(s.open_gates[0].gate_id, "g1");
    assert_eq!(s.active[0].kind, NodeKind::Gate);

    // The summary is also head.json: it must round-trip.
    let back: LoopSummary = serde_json::from_value(serde_json::to_value(&s).unwrap()).unwrap();
    assert_eq!(back, s);
}

#[test]
fn recovery_fixture_leaves_an_interrupted_step_and_an_open_recovery_gate() {
    let before = state_after(RECOVERY, 11);
    let orphans: Vec<&str> = before
        .orphaned_steps()
        .iter()
        .map(|s| s.step_id.as_str())
        .collect();
    assert_eq!(orphans, vec!["implement.i1.a1"]);
    // Active time stops at the dead writer's last record, not at the reopen.
    assert_eq!(before.usage.active_ms, 49_400 - 49_216 + 49_013);

    let state = state_after(RECOVERY, 13);
    assert!(state.orphaned_steps().is_empty());
    assert_eq!(state.epoch, 2);
    assert_eq!(
        state.instance("implement", &[1]).unwrap().status,
        InstanceStatus::Interrupted
    );
    assert!(state.is_waiting());

    // Resolving "retry" lets attempt 2 start with cause recovery.
    let mut next = state.clone();
    let resolved = LoopEvent::GateResolved(GateResolved {
        gate_id: "g1".into(),
        option: "retry".into(),
        comment: None,
        by: Actor::human("cli"),
    });
    next.admit(&resolved).unwrap();
    next.apply(&loaded(14, resolved));
    let retry = step_started(
        "implement",
        &[1],
        2,
        NodeKind::Agent,
        Some("implement.i1.a1"),
    );
    next.admit(&retry).unwrap();

    // "Keep edits, continue" instead marks the instance done/ok and forbids a retry.
    let mut kept = state;
    kept.apply(&loaded(
        14,
        LoopEvent::GateResolved(GateResolved {
            gate_id: "g1".into(),
            option: "mark_done".into(),
            comment: None,
            by: Actor::human("cli"),
        }),
    ));
    let inst = kept.instance("implement", &[1]).unwrap();
    assert_eq!(
        (inst.status, inst.outcome),
        (InstanceStatus::Done, Some(Outcome::Ok))
    );
    assert_eq!(code(kept.admit(&retry)), RejectCode::BadRetry);
}

#[test]
fn cursors_resume_or_reset() {
    let state = replay(&records(JOURNAL));
    let resume = Cursor {
        uid: "9c41e07a2b5d4f18".into(),
        seq: 20,
    };
    assert_eq!(
        state.check_cursor(&resume),
        CursorCheck::Resume { after: 20 }
    );
    let other_incarnation = Cursor {
        uid: "0000000000000000".into(),
        seq: 20,
    };
    assert_eq!(state.check_cursor(&other_incarnation), CursorCheck::Reset);
    let ahead = Cursor {
        uid: "9c41e07a2b5d4f18".into(),
        seq: 99,
    };
    assert_eq!(state.check_cursor(&ahead), CursorCheck::Reset);
    let scan = scan_bytes(JOURNAL.as_bytes());
    assert_eq!(scan.after(24).len(), 2);
    assert_eq!(scan.after(0).len(), 26);
}

#[test]
fn fold_ignores_duplicates_and_warns_on_gaps() {
    let recs = records(JOURNAL);
    let mut state = replay(&recs[..6]);
    state.apply(&recs[5]); // duplicate seq 6
    assert_eq!(state.head_seq, 6);
    state.apply(&recs[7]); // seq 8 without 7
    assert_eq!(state.head_seq, 8);
    assert_eq!(state.warnings.len(), 2);
}

#[test]
fn unknown_critical_records_make_the_state_read_only_but_ancillary_ones_do_not() {
    let mut text = JOURNAL.lines().take(3).collect::<Vec<_>>().join("\n");
    text.push_str(
        "\n{\"v\":1,\"seq\":4,\"ts\":1,\"kind\":\"future_hint\",\"anc\":true,\"data\":{}}\n",
    );
    let state = replay(&records(&text));
    assert!(state.writable().is_ok());

    text.push_str("{\"v\":1,\"seq\":5,\"ts\":1,\"kind\":\"proposal_created\",\"data\":{}}\n");
    let state = replay(&records(&text));
    assert!(matches!(
        state.writable(),
        Err(ReadOnlyReason::Blocking { seq: 5, .. })
    ));
    assert!(state.summary().is_some_and(|s| !s.writable));
}

#[test]
fn a_newer_writer_makes_the_state_read_only() {
    let mut text = JOURNAL.lines().take(3).collect::<Vec<_>>().join("\n");
    text.push_str("\n{\"v\":1,\"seq\":4,\"ts\":1,\"kind\":\"writer_opened\",\"data\":{\"epoch\":2,\"pid\":1,\"start_id\":\"\",\"build\":\"9.9.9\",\"schema_minor\":7}}\n");
    let state = replay(&records(&text));
    assert_eq!(
        state.writable(),
        Err(ReadOnlyReason::NewerWriter { schema_minor: 7 })
    );
}

#[test]
fn ops_fixture_round_trips() {
    for line in OPS_V1.lines() {
        let req: OpRequest = serde_json::from_str(line).unwrap();
        assert_ne!(req.op, LoopOp::Unknown, "{line}");
        assert_eq!(
            serde_json::to_value(&req).unwrap(),
            serde_json::from_str::<Value>(line).unwrap()
        );
    }
}

// ---------------------------------------------------------------------------------------
// admit: at least one case per rejection code.

fn loaded(seq: u64, ev: LoopEvent) -> LoadedRecord {
    LoadedRecord {
        seq,
        ts: 1_790_381_000_000 + seq,
        kind: ev.kind().to_string(),
        op: None,
        anc: ev.is_ancillary(),
        body: Body::Known(ev),
    }
}

fn step_started(
    node: &str,
    iter: &[u32],
    attempt: u32,
    kind: NodeKind,
    retry_of: Option<&str>,
) -> LoopEvent {
    let compute = kind.is_compute();
    LoopEvent::StepStarted(Box::new(StepStarted {
        step_id: step_id(node, iter, attempt),
        node: node.into(),
        iter: iter.to_vec(),
        attempt,
        kind,
        cause: if attempt > 1 {
            StartCause::Recovery
        } else {
            StartCause::Ready
        },
        retry_of: retry_of.map(str::to_string),
        access: match kind {
            NodeKind::Agent => Some(Access::Write),
            NodeKind::Check => Some(Access::Read),
            _ => None,
        },
        assignee: (kind == NodeKind::Agent).then(|| Assignee {
            backend: "claude".into(),
            ..Assignee::default()
        }),
        run_id: None,
        prompt: (kind == NodeKind::Agent).then(|| BlobRef {
            path: "prompts/x.md".into(),
            bytes: 1,
        }),
        command: (kind == NodeKind::Check).then(|| "true".to_string()),
        deadline_ms: compute.then_some(1),
        instructions: vec![],
    }))
}

fn finished(step: &str, outcome: Outcome) -> LoopEvent {
    LoopEvent::StepFinished(Box::new(StepFinished {
        step_id: step.into(),
        outcome,
        elapsed_ms: 1,
        exit_code: None,
        verdict: None,
        output: None,
        excerpt: None,
        log_tail: None,
        error: None,
        feedback: vec![],
        cancel: (outcome == Outcome::Cancelled).then_some(CancelCause::Stop),
    }))
}

fn gate_opened(id: &str, kind: GateKind, step: Option<&str>) -> LoopEvent {
    LoopEvent::GateOpened(Box::new(GateOpened {
        gate_id: id.into(),
        kind,
        step_id: step.map(str::to_string),
        node: None,
        iter: vec![],
        prompt: "?".into(),
        options: vec![GateOption {
            id: "ok".into(),
            label: None,
            outcome: Some(Outcome::Ok),
        }],
        deadline_ms: None,
        on_timeout: None,
    }))
}

fn created_with(rev: &str) -> LoopEvent {
    let mut v: Value = serde_json::from_str(JOURNAL.lines().next().unwrap()).unwrap();
    v["data"]["blueprint"]["rev"] = json!(rev);
    event(&v.to_string())
}

#[test]
fn admit_rejects_each_documented_violation() {
    let empty = LoopState::default();
    let at = |seq| state_after(JOURNAL, seq);
    let big = "x".repeat(MAX_DETAIL_BYTES + 1);
    let resolved = |gate: &str, option: &str| {
        LoopEvent::GateResolved(GateResolved {
            gate_id: gate.into(),
            option: option.into(),
            comment: None,
            by: Actor::human("cli"),
        })
    };
    let instruction = |id: &str, text: &str| {
        LoopEvent::InstructionReceived(InstructionReceived {
            instruction_id: id.into(),
            text: text.into(),
            target: None,
            by: Actor::human("cli"),
        })
    };

    // Budget exhaustion needs a state whose usage has reached max_steps.
    let mut tight = at(8);
    tight.apply(&loaded(
        9,
        LoopEvent::BudgetChanged(BudgetChanged {
            budget: Budget {
                max_steps: 1,
                ..Budget::default()
            },
            by: Actor::system(),
        }),
    ));

    let mut no_prompt = step_started("implement", &[1], 1, NodeKind::Agent, None);
    if let LoopEvent::StepStarted(s) = &mut no_prompt {
        s.prompt = None;
    }

    let cases: Vec<(RejectCode, Result<(), Rejection>)> = vec![
        (
            RejectCode::MissingHeader,
            empty.admit(&LoopEvent::LoopStarted),
        ),
        (
            RejectCode::DuplicateHeader,
            at(1).admit(&created_with("b1-6c29e826fc3c2a74")),
        ),
        (
            RejectCode::InvalidHeader,
            empty.admit(&created_with("b1-0000000000000000")),
        ),
        (
            RejectCode::BadEpoch,
            at(3).admit(&event(JOURNAL.lines().nth(1).unwrap())),
        ),
        (RejectCode::BadStatus, at(3).admit(&LoopEvent::LoopStarted)),
        (
            RejectCode::UnknownNode,
            at(3).admit(&step_started("ghost", &[], 1, NodeKind::Agent, None)),
        ),
        (
            RejectCode::KindMismatch,
            at(3).admit(&step_started("plan", &[], 1, NodeKind::Check, None)),
        ),
        (
            RejectCode::BadIter,
            at(3).admit(&step_started("implement", &[], 1, NodeKind::Agent, None)),
        ),
        (
            RejectCode::BadStepId,
            at(3).admit(&{
                let mut e = step_started("plan", &[], 1, NodeKind::Agent, None);
                if let LoopEvent::StepStarted(s) = &mut e {
                    s.step_id = "plan.a2".into();
                }
                e
            }),
        ),
        (
            RejectCode::InstanceDecided,
            at(6).admit(&step_started("plan", &[], 1, NodeKind::Agent, None)),
        ),
        (
            RejectCode::BadRetry,
            at(6).admit(&step_started(
                "plan",
                &[],
                2,
                NodeKind::Agent,
                Some("plan.a1"),
            )),
        ),
        (
            RejectCode::BudgetExhausted,
            tight.admit(&step_started("implement", &[1], 1, NodeKind::Agent, None)),
        ),
        (
            RejectCode::Concurrency,
            at(9).admit(&step_started("test", &[1], 1, NodeKind::Check, None)),
        ),
        (RejectCode::IncompleteEffect, at(8).admit(&no_prompt)),
        (
            RejectCode::StepNotFound,
            at(6).admit(&finished("nope.a1", Outcome::Ok)),
        ),
        (
            RejectCode::StepNotRunning,
            at(6).admit(&finished("plan.a1", Outcome::Ok)),
        ),
        (
            RejectCode::OutcomeNotAllowed,
            at(4).admit(&finished("plan.a1", Outcome::Exhausted)),
        ),
        (
            RejectCode::GatePending,
            at(22).admit(&finished("signoff.a1", Outcome::Ok)),
        ),
        (
            RejectCode::IterationOpen,
            at(8).admit(&finished("fix.a1", Outcome::Cancelled)),
        ),
        (
            RejectCode::IterationNotOpen,
            at(13).admit(&LoopEvent::IterationFinished(IterationFinished {
                repeat: "fix".into(),
                iter: vec![1],
                decision: IterationDecision::Continue,
            })),
        ),
        (
            RejectCode::BadIteration,
            at(13).admit(&LoopEvent::IterationStarted(IterationStarted {
                repeat: "fix".into(),
                iter: vec![3],
                feedback: vec![],
            })),
        ),
        (
            RejectCode::ScopeBusy,
            at(9).admit(&LoopEvent::IterationFinished(IterationFinished {
                repeat: "fix".into(),
                iter: vec![1],
                decision: IterationDecision::Continue,
            })),
        ),
        (
            RejectCode::GateNotFound,
            at(22).admit(&resolved("g9", "approve")),
        ),
        (
            RejectCode::GateNotOpen,
            at(23).admit(&resolved("g1", "approve")),
        ),
        (
            RejectCode::BadOption,
            at(22).admit(&resolved("g1", "maybe")),
        ),
        (
            RejectCode::BadGateSubject,
            at(6).admit(&gate_opened("g1", GateKind::StepError, Some("plan.a1"))),
        ),
        (
            RejectCode::BadId,
            at(6).admit(&gate_opened("g2", GateKind::Budget, None)),
        ),
        (
            RejectCode::EmptyText,
            at(6).admit(&instruction("in1", "  ")),
        ),
        (
            RejectCode::TooLarge,
            at(6).admit(&LoopEvent::Warning(Warning {
                code: "x".into(),
                message: big,
                step_id: None,
            })),
        ),
        (
            RejectCode::BudgetOutOfRange,
            at(6).admit(&LoopEvent::BudgetChanged(BudgetChanged {
                budget: Budget {
                    max_parallel: 9,
                    ..Budget::default()
                },
                by: Actor::system(),
            })),
        ),
        (
            RejectCode::UnknownValue,
            at(4).admit(&finished("plan.a1", Outcome::Unknown)),
        ),
    ];
    let mut covered: Vec<RejectCode> = Vec::new();
    for (expected, result) in cases {
        assert_eq!(code(result), expected);
        covered.push(expected);
    }
    for c in RejectCode::KNOWN {
        assert!(covered.contains(c), "no case for {c}");
    }
}

#[test]
fn approval_gate_step_outcome_must_match_the_chosen_option() {
    let state = state_after(JOURNAL, 23);
    assert_eq!(
        code(state.admit(&finished("signoff.a1", Outcome::Fail))),
        RejectCode::OutcomeNotAllowed
    );
    state.admit(&finished("signoff.a1", Outcome::Ok)).unwrap();
}

#[test]
fn repeat_outcome_follows_the_last_iteration_decision() {
    let state = state_after(JOURNAL, 19);
    assert_eq!(
        code(state.admit(&finished("fix.a1", Outcome::Exhausted))),
        RejectCode::OutcomeNotAllowed
    );
    state.admit(&finished("fix.a1", Outcome::Ok)).unwrap();

    // exhausted is only possible on the last iteration.
    let state = state_after(JOURNAL, 12);
    assert_eq!(
        code(
            state.admit(&LoopEvent::IterationFinished(IterationFinished {
                repeat: "fix".into(),
                iter: vec![1],
                decision: IterationDecision::Exhausted,
            }))
        ),
        RejectCode::BadIteration
    );
}

#[test]
fn a_loop_cannot_finish_with_work_in_flight() {
    let state = state_after(JOURNAL, 22);
    let done = LoopEvent::LoopFinished(LoopFinished {
        status: FinishStatus::Succeeded,
        reason: FinishReason::Completed,
        node: None,
        detail: None,
    });
    assert_eq!(code(state.admit(&done)), RejectCode::ScopeBusy);
    state_after(JOURNAL, 24).admit(&done).unwrap();
    // Terminal loops only accept writer bookkeeping and warnings.
    let terminal = state_after(JOURNAL, 25);
    assert_eq!(
        code(terminal.admit(&LoopEvent::LoopResumed)),
        RejectCode::BadStatus
    );
}

#[test]
fn stop_can_escalate_from_graceful_to_cancel_but_not_back() {
    let mut state = state_after(JOURNAL, 9);
    let stop = |mode| LoopEvent::LoopStopRequested(LoopStopRequested { mode, reason: None });
    state.admit(&stop(StopMode::Graceful)).unwrap();
    state.apply(&loaded(10, stop(StopMode::Graceful)));
    assert_eq!(state.status, LoopStatus::Stopping);
    state.admit(&stop(StopMode::Cancel)).unwrap();
    state.apply(&loaded(11, stop(StopMode::Cancel)));
    assert_eq!(
        code(state.admit(&stop(StopMode::Graceful))),
        RejectCode::BadStatus
    );
    // No new work while stopping.
    assert_eq!(
        code(state.admit(&step_started("test", &[1], 1, NodeKind::Check, None))),
        RejectCode::BadStatus
    );
}

#[test]
fn instructions_are_consumed_once_by_a_matching_agent_step() {
    let mut state = state_after(JOURNAL, 8);
    let ins = LoopEvent::InstructionReceived(InstructionReceived {
        instruction_id: "in1".into(),
        text: "keep the error text".into(),
        target: Some("implement".into()),
        by: Actor::human("cli"),
    });
    state.admit(&ins).unwrap();
    state.apply(&loaded(9, ins));
    let mut start = step_started("implement", &[1], 1, NodeKind::Agent, None);
    if let LoopEvent::StepStarted(s) = &mut start {
        s.instructions = vec!["in1".into()];
    }
    state.admit(&start).unwrap();
    state.apply(&loaded(10, start));
    assert_eq!(state.pending_instructions().count(), 0);
    assert_eq!(
        state.instructions[0].consumed_by.as_deref(),
        Some("implement.i1.a1")
    );
    assert_eq!(
        state.summary().unwrap().pending_instructions,
        0,
        "the board counts only unconsumed instructions"
    );
}
