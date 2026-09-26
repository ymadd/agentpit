//! Scheduler simulations: a fake runner drives `sched::next` → `records` → `admit` → `apply`
//! to the end of a scripted loop. Every record the scheduler asks for must pass the
//! journal's transition table, so these tests pin the scheduler and the schema together.

use std::collections::BTreeSet;

use agentpit_events::loops::*;
use serde_json::{Value, json};

use super::records::{GatePrep, Prep, decision_events, gate_follow_up, start_events};
use super::sched::*;

const FIX_UNTIL_GREEN: &str =
    include_str!("../../agentpit-events/tests/fixtures/loops/blueprint_fix_until_green.json");

/// What a scripted compute step does.
enum Effect {
    Finish(Outcome, Vec<FeedbackItem>),
    /// Keep running until the test finishes or cancels it.
    Hang,
}

type Script = Box<dyn FnMut(&StartPlan) -> Effect>;
type Answer = Box<dyn FnMut(&GateRun) -> Option<(String, Option<String>)>>;

struct Sim {
    state: LoopState,
    seq: u64,
    now: u64,
    kinds: Vec<String>,
    script: Script,
    answer: Answer,
    started: Vec<StartPlan>,
}

fn loaded(seq: u64, ts: u64, ev: LoopEvent) -> LoadedRecord {
    LoadedRecord {
        seq,
        ts,
        kind: ev.kind().to_string(),
        op: None,
        anc: ev.is_ancillary(),
        raw: String::new(),
        body: Body::Known(ev),
    }
}

fn ok_script() -> Script {
    Box::new(|_| Effect::Finish(Outcome::Ok, vec![]))
}

fn approve_all() -> Answer {
    Box::new(|g| {
        let id = match g.kind {
            GateKind::Budget => OPT_EXTEND,
            GateKind::StepError | GateKind::Recovery => OPT_RETRY,
            _ => "approve",
        };
        Some((id.to_string(), None))
    })
}

fn check_feedback(node: &str, step: &str) -> FeedbackItem {
    FeedbackItem {
        source: FeedbackSource::Check,
        node: node.into(),
        step_id: Some(step.into()),
        summary: "exited 1".into(),
        detail: Some("assertion failed".into()),
    }
}

impl Sim {
    fn new(doc: Value, script: Script, answer: Answer) -> Sim {
        let v = validate(&doc, &ValidateEnv::default());
        assert!(v.is_runnable(), "{:?}", v.diagnostics);
        let bp = v.blueprint.unwrap();
        let created = LoopCreated {
            loop_id: "lp-0199a1b2c3d47e5f8a9b0c1d2e3f4a5b".into(),
            uid: "9c41e07a2b5d4f18".into(),
            title: "sim".into(),
            blueprint: FrozenBlueprint {
                name: bp.name.clone(),
                scope: BlueprintScope::Inline,
                path: None,
                rev: v.rev.clone(),
                doc,
            },
            inputs: [("goal".to_string(), "g".to_string())].into(),
            cwd: "/w".into(),
            repo_root: None,
            workspace: bp.workspace.mode,
            budget: bp.budget,
            origin: Origin {
                surface: Surface::Cli,
                client: None,
                session_id: None,
            },
            root_run_id: None,
        };
        let mut sim = Sim {
            state: LoopState::default(),
            seq: 0,
            now: 1_000,
            kinds: vec![],
            script,
            answer,
            started: vec![],
        };
        sim.commit(vec![
            LoopEvent::LoopCreated(Box::new(created)),
            LoopEvent::WriterOpened(WriterOpened {
                epoch: 1,
                pid: 1,
                start_id: String::new(),
                build: "sim".into(),
                schema_minor: SCHEMA_MINOR,
                truncated_tail_bytes: 0,
            }),
            LoopEvent::LoopStarted,
        ]);
        sim
    }

    fn commit(&mut self, events: Vec<LoopEvent>) {
        for ev in events {
            if let Err(r) = self.state.admit(&ev) {
                panic!(
                    "admit refused {} ({r}) after {:?}",
                    ev.kind(),
                    self.kinds.iter().rev().take(6).collect::<Vec<_>>()
                );
            }
            self.seq += 1;
            self.now += 10;
            self.kinds.push(ev.kind().to_string());
            self.state.apply(&loaded(self.seq, self.now, ev));
        }
    }

    fn prep(&self, plan: &StartPlan) -> Prep {
        let spec = &self
            .state
            .blueprint
            .as_ref()
            .unwrap()
            .node(&plan.node)
            .unwrap()
            .spec;
        let deadline = Some(self.now + 60_000);
        match spec {
            NodeSpec::Agent(a) => Prep {
                access: Some(a.access),
                assignee: Some(Assignee {
                    backend: "fake".into(),
                    ..Assignee::default()
                }),
                prompt: Some(BlobRef {
                    path: format!("prompts/{}.md", plan.step_id()),
                    bytes: 1,
                }),
                deadline_ms: deadline,
                ..Prep::default()
            },
            NodeSpec::Check(c) => Prep {
                access: Some(Access::Read),
                command: Some(c.command.clone()),
                deadline_ms: deadline,
                ..Prep::default()
            },
            NodeSpec::Gate(g) => Prep {
                gate: Some(GatePrep {
                    prompt: g.prompt.clone(),
                    options: if g.options.is_empty() {
                        default_gate_options()
                    } else {
                        g.options.clone()
                    }
                    .into_iter()
                    .map(|o| GateOption {
                        id: o.id,
                        label: o.label,
                        outcome: Some(o.outcome),
                    })
                    .collect(),
                    deadline_ms: g.timeout_secs.map(|s| self.now + s * 1000),
                    on_timeout: g.on_timeout.clone(),
                }),
                ..Prep::default()
            },
            _ => Prep::default(),
        }
    }

    fn finish(&mut self, step_id: &str, outcome: Outcome, cancel: Option<CancelCause>) {
        self.finish_with(step_id, outcome, cancel, vec![]);
    }

    fn finish_with(
        &mut self,
        step_id: &str,
        outcome: Outcome,
        cancel: Option<CancelCause>,
        feedback: Vec<FeedbackItem>,
    ) {
        self.commit(vec![LoopEvent::StepFinished(Box::new(StepFinished {
            step_id: step_id.into(),
            outcome,
            elapsed_ms: 5,
            exit_code: None,
            verdict: None,
            output: None,
            excerpt: None,
            log_tail: None,
            error: (outcome == Outcome::Error).then(|| "boom".into()),
            feedback,
            cancel,
        }))]);
    }

    /// One round. `false` = nothing more happens without outside help.
    fn step(&mut self) -> bool {
        let plan = next(&self.state, self.now);
        let mut progressed = false;
        for (step_id, cause) in plan.cancels {
            if self
                .state
                .step(&step_id)
                .is_some_and(|s| s.status == StepStatus::Running)
            {
                self.finish(&step_id, Outcome::Cancelled, Some(cause));
                progressed = true;
            }
        }
        if progressed {
            return true;
        }
        if let Some(d) = plan.decision {
            match &d {
                Decision::Start(p) => {
                    let prep = self.prep(p);
                    let events = start_events(&self.state, p, prep);
                    self.commit(events);
                    self.started.push(p.clone());
                    if p.kind.is_compute() {
                        match (self.script)(p) {
                            Effect::Finish(o, fb) => {
                                let cancel =
                                    (o == Outcome::Cancelled).then_some(CancelCause::CancelStep);
                                self.finish_with(&p.step_id(), o, cancel, fb);
                            }
                            Effect::Hang => {}
                        }
                    }
                }
                other => {
                    let events = decision_events(&self.state, other);
                    self.commit(events);
                }
            }
            return true;
        }
        let open: Option<GateRun> = self.state.open_gates().next().cloned();
        if let Some(g) = open
            && let Some((option, comment)) = (self.answer)(&g)
        {
            let by = Actor::human("sim");
            let mut events = vec![LoopEvent::GateResolved(GateResolved {
                gate_id: g.gate_id.clone(),
                option: option.clone(),
                comment,
                by: by.clone(),
            })];
            events.extend(gate_follow_up(&self.state, &g, &option, &by));
            self.commit(events);
            return true;
        }
        if let Some(w) = plan.wake_at
            && w > self.now
        {
            self.now = w;
            return true;
        }
        false
    }

    fn run(&mut self) {
        for _ in 0..2_000 {
            if !self.step() {
                return;
            }
        }
        panic!("simulation did not settle: {:?}", self.kinds);
    }

    fn running(&self) -> BTreeSet<String> {
        self.state
            .running_steps()
            .into_iter()
            .filter(|s| s.kind.is_compute())
            .map(|s| s.step_id.clone())
            .collect()
    }

    fn count(&self, kind: &str) -> usize {
        self.kinds.iter().filter(|k| *k == kind).count()
    }
}

fn doc(nodes: Value, edges: Value) -> Value {
    json!({
        "schema": "agentpit.blueprint/1",
        "name": "sim",
        "inputs": {"goal": {"required": true}},
        "nodes": nodes,
        "edges": edges
    })
}

#[test]
fn fix_until_green_breaks_on_the_second_iteration_and_threads_feedback() {
    let mut sim = Sim::new(
        serde_json::from_str(FIX_UNTIL_GREEN).unwrap(),
        Box::new(|p| match (p.node.as_str(), p.iter.as_slice()) {
            ("test", [1]) => {
                Effect::Finish(Outcome::Fail, vec![check_feedback("test", &p.step_id())])
            }
            _ => Effect::Finish(Outcome::Ok, vec![]),
        }),
        approve_all(),
    );
    sim.run();
    assert_eq!(sim.state.status, LoopStatus::Succeeded, "{:?}", sim.kinds);
    assert_eq!(
        sim.state.repeats["fix"].decisions,
        vec![IterationDecision::Continue, IterationDecision::Break]
    );
    // The second iteration was told why the first failed.
    let fb = &sim.state.repeats["fix"].feedback;
    assert_eq!(fb.len(), 1);
    assert_eq!(fb[0].step_id.as_deref(), Some("test.i1.a1"));
    assert_eq!(sim.state.usage.steps, 5);
    assert_eq!(
        sim.state.instance("signoff", &[]).unwrap().outcome,
        Some(Outcome::Ok)
    );
}

#[test]
fn a_check_that_never_passes_exhausts_the_repeat_and_still_reaches_signoff() {
    let fail_tests: Script = Box::new(|p| {
        if p.node == "test" {
            Effect::Finish(Outcome::Fail, vec![check_feedback("test", &p.step_id())])
        } else {
            Effect::Finish(Outcome::Ok, vec![])
        }
    });
    let mut sim = Sim::new(
        serde_json::from_str(FIX_UNTIL_GREEN).unwrap(),
        fail_tests,
        approve_all(),
    );
    sim.run();
    assert_eq!(sim.state.status, LoopStatus::Succeeded);
    assert_eq!(
        sim.state.repeats["fix"].decisions.last(),
        Some(&IterationDecision::Exhausted)
    );
    assert_eq!(sim.state.repeats["fix"].decisions.len(), 4);
    assert_eq!(
        sim.state.instance("fix", &[]).unwrap().outcome,
        Some(Outcome::Exhausted)
    );

    // Rejecting the sign-off is an unhandled fail at the top: the loop fails.
    let mut sim = Sim::new(
        serde_json::from_str(FIX_UNTIL_GREEN).unwrap(),
        ok_script(),
        Box::new(|_| Some(("reject".into(), Some("not good enough".into())))),
    );
    sim.run();
    assert_eq!(sim.state.status, LoopStatus::Failed);
    assert_eq!(
        sim.state.finish.as_ref().unwrap().node.as_deref(),
        Some("signoff")
    );
}

#[test]
fn errors_retry_automatically_then_ask_a_person() {
    let d = doc(
        json!([{"id": "a", "kind": "agent", "task": "do {{goal}}", "retries": 1}]),
        json!([]),
    );
    let mut sim = Sim::new(
        d,
        Box::new(|p| {
            if p.attempt < 3 {
                Effect::Finish(Outcome::Error, vec![])
            } else {
                Effect::Finish(Outcome::Ok, vec![])
            }
        }),
        approve_all(),
    );
    sim.run();
    assert_eq!(sim.state.status, LoopStatus::Succeeded);
    let inst = sim.state.instance("a", &[]).unwrap();
    assert_eq!(inst.attempts, 3);
    assert_eq!(sim.state.gates.len(), 1);
    assert_eq!(sim.state.gates[0].kind, GateKind::StepError);
    assert_eq!(sim.started[1].cause, StartCause::Retry);
}

#[test]
fn a_step_error_answered_fail_is_an_unhandled_failure() {
    let d = doc(
        json!([{"id": "a", "kind": "agent", "task": "x"}]),
        json!([]),
    );
    let mut sim = Sim::new(
        d,
        Box::new(|_| Effect::Finish(Outcome::Error, vec![])),
        Box::new(|_| Some((OPT_FAIL.into(), None))),
    );
    sim.run();
    assert_eq!(sim.state.status, LoopStatus::Failed);
    assert_eq!(
        sim.state.instance("a", &[]).unwrap().outcome,
        Some(Outcome::Fail)
    );
}

#[test]
fn on_error_fail_routes_the_error_along_an_error_edge() {
    let mut d = doc(
        json!([
            {"id": "a", "kind": "agent", "task": "x"},
            {"id": "b", "kind": "agent", "task": "clean up"}
        ]),
        json!([{"from": "a", "to": "b", "on": "error"}]),
    );
    d["policy"] = json!({"on_error": "fail"});
    let mut sim = Sim::new(
        d,
        Box::new(|p| {
            if p.node == "a" {
                Effect::Finish(Outcome::Error, vec![])
            } else {
                Effect::Finish(Outcome::Ok, vec![])
            }
        }),
        approve_all(),
    );
    sim.run();
    assert_eq!(sim.state.status, LoopStatus::Succeeded);
    assert!(sim.state.gates.is_empty());
    assert_eq!(
        sim.state.instance("b", &[]).unwrap().outcome,
        Some(Outcome::Ok)
    );
}

#[test]
fn edges_that_do_not_fire_skip_their_targets() {
    let d = doc(
        json!([
            {"id": "a", "kind": "check", "command": "true"},
            {"id": "b", "kind": "agent", "task": "repair"},
            {"id": "c", "kind": "agent", "task": "ship"}
        ]),
        json!([
            {"from": "a", "to": "b", "on": "fail"},
            {"from": "a", "to": "c"}
        ]),
    );
    let mut sim = Sim::new(d, ok_script(), approve_all());
    sim.run();
    assert_eq!(sim.state.status, LoopStatus::Succeeded);
    let b = sim.state.instance("b", &[]).unwrap();
    assert_eq!(
        (b.status, b.skip_reason),
        (InstanceStatus::Skipped, Some(SkipReason::DeadPath))
    );
    assert_eq!(
        sim.state.instance("c", &[]).unwrap().outcome,
        Some(Outcome::Ok)
    );
}

#[test]
fn an_exhausted_budget_asks_to_extend_or_fails_by_policy() {
    let nodes = json!([
        {"id": "a", "kind": "agent", "task": "x"},
        {"id": "b", "kind": "agent", "task": "y"}
    ]);
    let edges = json!([{"from": "a", "to": "b"}]);
    let mut d = doc(nodes.clone(), edges.clone());
    d["budget"] = json!({"max_steps": 1});
    let mut sim = Sim::new(d, ok_script(), approve_all());
    sim.run();
    assert_eq!(sim.state.status, LoopStatus::Succeeded);
    assert_eq!(sim.state.gates[0].kind, GateKind::Budget);
    assert_eq!(sim.count("budget_changed"), 1);
    assert_eq!(sim.state.budget.max_steps, 11);

    let mut d = doc(nodes, edges);
    d["budget"] = json!({"max_steps": 1});
    d["policy"] = json!({"on_budget": "fail"});
    let mut sim = Sim::new(d, ok_script(), approve_all());
    sim.run();
    assert_eq!(sim.state.status, LoopStatus::Failed);
    let finish = sim.state.finish.as_ref().unwrap();
    assert_eq!(finish.reason, FinishReason::Budget);
    assert!(sim.state.instance("b", &[]).is_none(), "b never started");
}

fn nested() -> Value {
    doc(
        json!([
            {"id": "outer", "kind": "repeat", "max_iterations": 2},
            {"id": "inner", "kind": "repeat", "max_iterations": 2, "parent": "outer"},
            {"id": "work", "kind": "agent", "task": "x", "parent": "inner"},
            {"id": "ok", "kind": "gate", "prompt": "fine?", "parent": "outer"}
        ]),
        json!([{"from": "inner", "to": "ok"}]),
    )
}

#[test]
fn stop_cancel_drains_nested_repeats_innermost_first() {
    let mut sim = Sim::new(nested(), Box::new(|_| Effect::Hang), approve_all());
    sim.run();
    assert_eq!(sim.running(), BTreeSet::from(["work.i1-1.a1".to_string()]));
    sim.commit(vec![LoopEvent::LoopStopRequested(LoopStopRequested {
        mode: StopMode::Cancel,
        reason: None,
    })]);
    sim.run();
    assert_eq!(sim.state.status, LoopStatus::Cancelled, "{:?}", sim.kinds);
    let work = sim.state.step("work.i1-1.a1").unwrap();
    assert_eq!(work.cancel, Some(CancelCause::Stop));
    assert_eq!(
        sim.state.repeats["inner.i1"].decisions,
        vec![IterationDecision::Cancelled]
    );
    assert_eq!(
        sim.state.repeats["outer"].decisions,
        vec![IterationDecision::Cancelled]
    );
}

#[test]
fn a_graceful_stop_waits_for_running_steps() {
    let mut sim = Sim::new(nested(), Box::new(|_| Effect::Hang), approve_all());
    sim.run();
    sim.commit(vec![LoopEvent::LoopStopRequested(LoopStopRequested {
        mode: StopMode::Graceful,
        reason: None,
    })]);
    sim.run();
    assert_eq!(
        sim.state.status,
        LoopStatus::Stopping,
        "the step still runs"
    );
    sim.finish("work.i1-1.a1", Outcome::Ok, None);
    sim.run();
    assert_eq!(sim.state.status, LoopStatus::Cancelled);
    assert!(
        sim.state.instance("ok", &[1]).is_none(),
        "nothing new started"
    );
}

#[test]
fn pause_cancel_then_resume_reruns_the_cancelled_step() {
    let d = doc(
        json!([{"id": "a", "kind": "agent", "task": "x"}]),
        json!([]),
    );
    let mut first = true;
    let mut sim = Sim::new(
        d,
        Box::new(move |_| {
            if std::mem::take(&mut first) {
                Effect::Hang
            } else {
                Effect::Finish(Outcome::Ok, vec![])
            }
        }),
        approve_all(),
    );
    sim.run();
    sim.commit(vec![LoopEvent::LoopPaused(LoopPaused {
        mode: PauseMode::Cancel,
        note: None,
    })]);
    sim.run();
    assert_eq!(sim.state.status, LoopStatus::Paused);
    assert_eq!(
        sim.state.step("a.a1").unwrap().cancel,
        Some(CancelCause::Pause)
    );
    sim.commit(vec![LoopEvent::LoopResumed]);
    sim.run();
    assert_eq!(sim.state.status, LoopStatus::Succeeded);
    assert_eq!(sim.started.last().unwrap().cause, StartCause::Resume);
}

fn crash(sim: &mut Sim, step: &str) {
    let epoch = sim.state.epoch;
    sim.commit(vec![
        LoopEvent::WriterOpened(WriterOpened {
            epoch: epoch + 1,
            pid: 2,
            start_id: String::new(),
            build: "sim".into(),
            schema_minor: SCHEMA_MINOR,
            truncated_tail_bytes: 0,
        }),
        LoopEvent::StepInterrupted(StepInterrupted {
            step_id: step.into(),
            epoch,
        }),
    ]);
}

#[test]
fn an_interrupted_agent_asks_a_person_and_an_interrupted_check_just_retries() {
    let d = doc(
        json!([
            {"id": "w", "kind": "agent", "task": "edit"},
            {"id": "r", "kind": "agent", "task": "read", "access": "read"},
            {"id": "c", "kind": "check", "command": "true"}
        ]),
        json!([{"from": "w", "to": "r"}, {"from": "r", "to": "c"}]),
    );
    // Each node's first attempt is the one "running" when the runner dies.
    let mut sim = Sim::new(
        d,
        Box::new(|p| {
            if p.attempt == 1 {
                Effect::Hang
            } else {
                Effect::Finish(Outcome::Ok, vec![])
            }
        }),
        approve_all(),
    );
    sim.run();
    crash(&mut sim, "w.a1");
    sim.run();
    assert_eq!(sim.state.gates[0].kind, GateKind::Recovery);
    assert_eq!(sim.started[1].cause, StartCause::Recovery);
    // A read-only agent may still have side effects: a person decides too.
    assert_eq!(sim.running(), BTreeSet::from(["r.a1".to_string()]));
    crash(&mut sim, "r.a1");
    sim.run();
    assert_eq!(sim.state.gates[1].kind, GateKind::Recovery);
    // A check is just run again.
    assert_eq!(sim.running(), BTreeSet::from(["c.a1".to_string()]));
    crash(&mut sim, "c.a1");
    sim.run();
    assert_eq!(sim.state.status, LoopStatus::Succeeded);
    assert_eq!(sim.state.gates.len(), 2);
    assert_eq!(sim.state.instance("c", &[]).unwrap().attempts, 2);
}

#[test]
fn a_raised_budget_closes_the_gate_it_made_obsolete() {
    let mut d = doc(
        json!([
            {"id": "a", "kind": "agent", "task": "x"},
            {"id": "b", "kind": "agent", "task": "y"}
        ]),
        json!([{"from": "a", "to": "b"}]),
    );
    d["budget"] = json!({"max_steps": 1});
    let mut sim = Sim::new(d, ok_script(), Box::new(|_| None));
    sim.run();
    assert_eq!(
        sim.state.open_gates().next().unwrap().kind,
        GateKind::Budget
    );
    // `agentpit loop budget --max-steps 5` instead of answering the gate.
    let mut budget = sim.state.budget;
    budget.max_steps = 5;
    sim.commit(vec![LoopEvent::BudgetChanged(BudgetChanged {
        budget,
        by: Actor::human("sim"),
    })]);
    sim.run();
    assert_eq!(sim.state.status, LoopStatus::Succeeded);
    assert_eq!(
        sim.state.gates[0].cancel_reason,
        Some(GateCancelReason::Superseded)
    );
}

#[test]
fn a_failure_while_the_budget_gate_is_open_still_ends_the_loop() {
    let mut d = doc(
        json!([
            {"id": "a", "kind": "agent", "task": "x", "access": "read"},
            {"id": "b", "kind": "check", "command": "false"},
            {"id": "c", "kind": "agent", "task": "y", "access": "read"}
        ]),
        json!([{"from": "a", "to": "c"}]),
    );
    d["budget"] = json!({"max_steps": 2, "max_parallel": 2});
    let mut sim = Sim::new(
        d,
        Box::new(|p| match p.node.as_str() {
            "b" => Effect::Hang,
            _ => Effect::Finish(Outcome::Ok, vec![]),
        }),
        Box::new(|_| None),
    );
    sim.run();
    assert_eq!(
        sim.state.open_gates().next().unwrap().kind,
        GateKind::Budget
    );
    sim.finish("b.a1", Outcome::Fail, None);
    sim.run();
    assert_eq!(sim.state.status, LoopStatus::Failed);
    let finish = sim.state.finish.as_ref().unwrap();
    assert_eq!(finish.reason, FinishReason::UnhandledOutcome);
    assert_eq!(finish.node.as_deref(), Some("b"));
}

#[test]
fn re_runs_after_a_crash_do_not_use_up_error_retries() {
    let mut d = doc(
        json!([{"id": "c", "kind": "check", "command": "x", "retries": 1}]),
        json!([]),
    );
    d["policy"] = json!({"on_error": "fail"});
    let mut sim = Sim::new(
        d,
        Box::new(|p| match p.attempt {
            1 => Effect::Hang,
            2 => Effect::Finish(Outcome::Error, vec![]),
            _ => Effect::Finish(Outcome::Ok, vec![]),
        }),
        approve_all(),
    );
    sim.run();
    crash(&mut sim, "c.a1");
    sim.run();
    // a1 interrupted, a2 (recovery) errored once, a3 is the one automatic retry.
    assert_eq!(sim.state.status, LoopStatus::Succeeded, "{:?}", sim.kinds);
    assert_eq!(sim.started.last().unwrap().cause, StartCause::Retry);
}

#[test]
fn at_the_hard_cap_the_budget_stops_the_loop_instead_of_asking_again() {
    let mut d = doc(
        json!([
            {"id": "a", "kind": "agent", "task": "x"},
            {"id": "b", "kind": "agent", "task": "y"}
        ]),
        json!([{"from": "a", "to": "b"}]),
    );
    d["budget"] = json!({"max_active_secs": HARD_MAX_ACTIVE_SECS});
    let mut sim = Sim::new(
        d,
        Box::new(|p| match p.node.as_str() {
            "a" => Effect::Hang,
            _ => Effect::Finish(Outcome::Ok, vec![]),
        }),
        approve_all(),
    );
    sim.run();
    sim.now += HARD_MAX_ACTIVE_SECS * 1000;
    sim.finish("a.a1", Outcome::Ok, None);
    sim.run();
    assert!(sim.state.gates.is_empty(), "extend could not have helped");
    assert_eq!(sim.state.status, LoopStatus::Failed);
    assert_eq!(
        sim.state.finish.as_ref().unwrap().reason,
        FinishReason::Budget
    );
}

#[test]
fn overlapping_compute_counts_against_the_time_budget() {
    let mut d = doc(
        json!([
            {"id": "slow", "kind": "check", "command": "x"},
            {"id": "a", "kind": "agent", "task": "x", "access": "read"},
            {"id": "b", "kind": "agent", "task": "y", "access": "read"}
        ]),
        json!([{"from": "a", "to": "b"}]),
    );
    d["budget"] = json!({"max_active_secs": 600, "max_parallel": 2});
    let mut sim = Sim::new(d, Box::new(|_| Effect::Hang), Box::new(|_| None));
    sim.run();
    assert_eq!(sim.running().len(), 2);
    sim.now += 700_000;
    sim.finish("a.a1", Outcome::Ok, None);
    sim.run();
    // `slow` never stopped, so the folded total is still 0 — but 700s have been used.
    assert_eq!(sim.state.usage.active_ms, 0);
    assert!(sim.state.instance("b", &[]).is_none(), "b must not start");
    assert_eq!(
        sim.state.open_gates().next().unwrap().kind,
        GateKind::Budget
    );
}

#[test]
fn readers_share_the_parallel_budget_and_a_writer_waits_for_them() {
    let mut d = doc(
        json!([
            {"id": "a", "kind": "agent", "task": "x", "access": "read"},
            {"id": "b", "kind": "check", "command": "true"},
            {"id": "w", "kind": "agent", "task": "edit"}
        ]),
        json!([]),
    );
    d["budget"] = json!({"max_parallel": 2});
    let mut sim = Sim::new(d, Box::new(|_| Effect::Hang), approve_all());
    sim.run();
    assert_eq!(
        sim.running(),
        BTreeSet::from(["a.a1".to_string(), "b.a1".to_string()])
    );
    sim.finish("a.a1", Outcome::Ok, None);
    sim.run();
    assert!(
        !sim.running().contains("w.a1"),
        "the writer waits for every reader"
    );
    sim.finish("b.a1", Outcome::Ok, None);
    sim.run();
    assert_eq!(sim.running(), BTreeSet::from(["w.a1".to_string()]));
    sim.finish("w.a1", Outcome::Ok, None);
    sim.run();
    assert_eq!(sim.state.status, LoopStatus::Succeeded);
}

#[test]
fn a_gate_deadline_answers_with_its_timeout_option() {
    let d = doc(
        json!([{"id": "g", "kind": "gate", "prompt": "ok?", "timeout_secs": 60, "on_timeout": "approve"}]),
        json!([]),
    );
    let mut sim = Sim::new(d, ok_script(), Box::new(|_| None));
    sim.run();
    assert_eq!(sim.state.status, LoopStatus::Succeeded);
    let res = sim.state.gates[0].resolution.as_ref().unwrap();
    assert_eq!(res.by.kind, ActorKind::Timeout);

    // Without on_timeout the gate step times out, which nothing handles.
    let d = doc(
        json!([{"id": "g", "kind": "gate", "prompt": "ok?", "timeout_secs": 60}]),
        json!([]),
    );
    let mut sim = Sim::new(d, ok_script(), Box::new(|_| None));
    sim.run();
    assert_eq!(sim.state.status, LoopStatus::Failed);
    assert_eq!(
        sim.state.instance("g", &[]).unwrap().outcome,
        Some(Outcome::Timeout)
    );
}

#[test]
fn an_unhandled_failure_cancels_its_siblings_and_fails_the_loop() {
    let d = doc(
        json!([
            {"id": "slow", "kind": "agent", "task": "x", "access": "read"},
            {"id": "bad", "kind": "check", "command": "false"}
        ]),
        json!([]),
    );
    let mut d = d;
    d["budget"] = json!({"max_parallel": 2});
    let mut sim = Sim::new(
        d,
        Box::new(|p| {
            if p.node == "bad" {
                Effect::Finish(Outcome::Fail, vec![])
            } else {
                Effect::Hang
            }
        }),
        approve_all(),
    );
    sim.run();
    assert_eq!(sim.state.status, LoopStatus::Failed, "{:?}", sim.kinds);
    assert_eq!(
        sim.state.step("slow.a1").unwrap().cancel,
        Some(CancelCause::ScopeEnded)
    );
    assert_eq!(
        sim.state.finish.as_ref().unwrap().node.as_deref(),
        Some("bad")
    );
}

#[test]
fn instructions_go_to_the_next_matching_agent_step() {
    let mut sim = Sim::new(
        serde_json::from_str(FIX_UNTIL_GREEN).unwrap(),
        Box::new(|p| match (p.node.as_str(), p.iter.as_slice()) {
            ("test", [1]) => Effect::Finish(Outcome::Fail, vec![]),
            _ => Effect::Finish(Outcome::Ok, vec![]),
        }),
        approve_all(),
    );
    sim.commit(vec![LoopEvent::InstructionReceived(InstructionReceived {
        instruction_id: "in1".into(),
        text: "keep the error text".into(),
        target: Some("implement".into()),
        by: Actor::human("sim"),
    })]);
    sim.run();
    assert_eq!(sim.state.status, LoopStatus::Succeeded);
    let consumer = sim
        .started
        .iter()
        .find(|p| !p.instructions.is_empty())
        .unwrap();
    assert_eq!(consumer.step_id(), "implement.i1.a1");
    assert_eq!(
        sim.state.instructions[0].consumed_by.as_deref(),
        Some("implement.i1.a1")
    );
}

#[test]
fn an_inner_repeat_still_sees_what_the_outer_iteration_was_told() {
    let d = doc(
        json!([
            {"id": "outer", "kind": "repeat", "max_iterations": 2},
            {"id": "inner", "kind": "repeat", "max_iterations": 1, "parent": "outer"},
            {"id": "implement", "kind": "agent", "task": "{{feedback}}", "parent": "inner"},
            {"id": "review", "kind": "check", "command": "x", "parent": "outer"}
        ]),
        json!([{"from": "inner", "to": "review"}]),
    );
    let mut sim = Sim::new(
        d,
        Box::new(|p| match (p.node.as_str(), p.iter.as_slice()) {
            ("review", [1]) => {
                Effect::Finish(Outcome::Fail, vec![check_feedback("review", &p.step_id())])
            }
            _ => Effect::Finish(Outcome::Ok, vec![]),
        }),
        approve_all(),
    );
    sim.run();
    assert_eq!(sim.state.status, LoopStatus::Succeeded);
    assert!(
        sim.started
            .iter()
            .any(|p| p.node == "implement" && p.iter == [2, 1])
    );
    let fb = super::prompt::current_feedback(&sim.state, "implement", &[2, 1]);
    assert_eq!(fb.len(), 1, "{fb:?}");
    assert_eq!(fb[0].node, "review");
}
