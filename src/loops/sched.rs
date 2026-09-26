//! The loop scheduler (docs/workspace-loop-design.md §4.3): given a folded loop, decide the
//! next record to write.
//!
//! Pure and level-triggered: every decision is derived from the journal's fold alone, so a
//! runner that crashed and replayed its journal decides exactly what it would have decided
//! before. The runner asks for ONE decision at a time, commits it (the journal's `admit`
//! double-checks it), re-folds, and asks again — so the scheduler never has to simulate the
//! effect of its own earlier decisions, and every concurrency or budget check sees the state
//! as it really is.
//!
//! Cancellations are the one thing that is not a record: stopping a running process is an
//! effect, reported later as `step_finished{cancelled}`. They come back as a separate list
//! the runner applies idempotently on every round.

use agentpit_events::loops::*;

/// Standard option ids of the gates the runner opens itself.
pub const OPT_RETRY: &str = "retry";
pub const OPT_FAIL: &str = "fail";
pub const OPT_STOP: &str = "stop";
pub const OPT_MARK_DONE: &str = "mark_done";
pub const OPT_EXTEND: &str = "extend";

/// `loop_stop_requested.reason` for a budget exhausted under `on_budget: fail`: the drain
/// then ends the loop `failed` instead of `cancelled`.
pub const STOP_REASON_BUDGET: &str = "budget";

/// One attempt the runner should start. The runner prepares the effect (assignee, prompt,
/// command, deadline) and turns it into `step_started` (plus `gate_opened` for a gate).
#[derive(Debug, Clone, PartialEq)]
pub struct StartPlan {
    pub node: String,
    pub iter: Vec<u32>,
    pub attempt: u32,
    pub kind: NodeKind,
    pub cause: StartCause,
    pub retry_of: Option<String>,
    /// Pending instructions this attempt consumes (agents only).
    pub instructions: Vec<String>,
}

impl StartPlan {
    pub fn step_id(&self) -> String {
        step_id(&self.node, &self.iter, self.attempt)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Decision {
    Start(StartPlan),
    StartIteration(IterationStarted),
    FinishIteration(IterationFinished),
    /// The bookkeeping end of a gate or repeat step (no process involved).
    FinishStep {
        step_id: String,
        outcome: Outcome,
        cancel: Option<CancelCause>,
        feedback: Vec<FeedbackItem>,
    },
    Skip(NodeSkipped),
    /// A gate the runner opens on its own: step_error, recovery (with a subject step) or
    /// budget (without).
    OpenGate {
        kind: GateKind,
        step_id: Option<String>,
    },
    /// A gate deadline passed: resolve with `on_timeout`, or cancel it `timed_out`.
    TimeoutGate {
        gate_id: String,
        option: Option<String>,
    },
    CancelGate {
        gate_id: String,
        reason: GateCancelReason,
    },
    Stop {
        mode: StopMode,
        reason: String,
    },
    Finish(LoopFinished),
}

/// What the runner should do now.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Plan {
    /// Running agent/check steps to cancel (idempotent: repeated every round until they end).
    pub cancels: Vec<(String, CancelCause)>,
    /// The next record-producing decision, if any.
    pub decision: Option<Decision>,
    /// Wake up at this epoch-ms even without input (the next gate deadline).
    pub wake_at: Option<u64>,
}

/// The standard options of a runner-opened gate (design §8.5).
pub fn standard_gate_options(kind: GateKind) -> Vec<GateOption> {
    let opt = |id: &str, label: &str, outcome: Option<Outcome>| GateOption {
        id: id.to_string(),
        label: Some(label.to_string()),
        outcome,
    };
    match kind {
        GateKind::StepError => vec![
            opt(OPT_RETRY, "Retry the step", None),
            opt(OPT_FAIL, "Treat it as failed", Some(Outcome::Fail)),
            opt(OPT_STOP, "Stop the loop", None),
        ],
        GateKind::Recovery => vec![
            opt(OPT_RETRY, "Retry the step", None),
            opt(
                OPT_MARK_DONE,
                "Keep its edits and continue",
                Some(Outcome::Ok),
            ),
            opt(OPT_STOP, "Stop the loop", None),
        ],
        GateKind::Budget => vec![
            opt(OPT_EXTEND, "Extend the budget", None),
            opt(OPT_STOP, "Stop the loop", None),
        ],
        GateKind::Approval | GateKind::Unknown => blueprint_default_options(),
    }
}

fn blueprint_default_options() -> Vec<GateOption> {
    default_gate_options()
        .into_iter()
        .map(|o| GateOption {
            id: o.id,
            label: o.label,
            outcome: Some(o.outcome),
        })
        .collect()
}

/// The budget after an `extend` answer: +50% steps (at least 10) and +50% compute time,
/// within the hard caps.
pub fn extended_budget(b: Budget) -> Budget {
    Budget {
        max_steps: (b.max_steps + (b.max_steps / 2).max(10)).min(HARD_MAX_STEPS),
        max_active_secs: (b.max_active_secs + b.max_active_secs / 2).min(HARD_MAX_ACTIVE_SECS),
        max_parallel: b.max_parallel,
    }
}

/// Whether the budget no longer allows another compute step.
pub fn budget_exhausted(state: &LoopState) -> bool {
    state.usage.steps >= state.budget.max_steps
        || state.usage.active_ms >= state.budget.max_active_secs.saturating_mul(1000)
}

/// Decide what to do next. Pure.
pub fn next(state: &LoopState, now_ms: u64) -> Plan {
    let mut plan = Plan::default();
    let Some(bp) = state.blueprint.as_ref() else {
        return plan;
    };
    if state.created.is_none() || state.status.is_terminal() || state.status == LoopStatus::Created
    {
        return plan;
    }
    let ctx = Ctx::new(state, bp);

    // Gate deadlines come first in every non-terminal state.
    let mut deadline_decision = None;
    for g in state.open_gates() {
        match g.deadline_ms {
            Some(d) if d <= now_ms => {
                deadline_decision.get_or_insert_with(|| match &g.on_timeout {
                    Some(opt) => Decision::TimeoutGate {
                        gate_id: g.gate_id.clone(),
                        option: Some(opt.clone()),
                    },
                    None => Decision::CancelGate {
                        gate_id: g.gate_id.clone(),
                        reason: GateCancelReason::TimedOut,
                    },
                });
            }
            Some(d) => plan.wake_at = Some(plan.wake_at.map_or(d, |w| w.min(d))),
            None => {}
        }
    }

    match state.status {
        LoopStatus::Paused => {
            if state.pause == Some(PauseMode::Cancel) {
                for s in ctx.running_compute() {
                    plan.cancels.push((s.step_id.clone(), CancelCause::Pause));
                }
            }
            plan.decision = deadline_decision.or_else(|| ctx.finish_closed_gate_steps());
        }
        LoopStatus::Stopping => {
            if state.stop == Some(StopMode::Cancel) {
                for s in ctx.running_compute() {
                    plan.cancels.push((s.step_id.clone(), CancelCause::Stop));
                }
            }
            plan.decision = deadline_decision.or_else(|| ctx.stop_drain());
        }
        LoopStatus::Running => {
            let mut out = Walk::default();
            ctx.walk(&mut out);
            plan.cancels = out.cancels;
            plan.decision = deadline_decision.or(out.decision);
        }
        _ => {}
    }
    plan
}

/// How a scope is being run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Normal,
    /// A member ended with an outcome no edge handles: finish what runs, skip the rest.
    Failing,
    /// An enclosing scope is failing: end this iteration `cancelled`.
    Drained,
}

/// One scope: the top level, or one open iteration of a repeat instance.
#[derive(Debug, Clone)]
struct Scope {
    repeat: Option<String>,
    /// The iteration path of every member instance.
    path: Vec<u32>,
    members: Vec<String>,
}

/// How a member instance stands.
#[derive(Debug, Clone, PartialEq)]
enum Class {
    Undecided,
    Skipped,
    Running,
    /// Waits for a person (an open gate) or for a decision taken elsewhere.
    Waiting,
    /// Needs this decision (a retry, a gate, an iteration change, a bookkeeping finish).
    Act(Decision),
    Settled(Outcome, Option<CancelCause>),
}

#[derive(Default)]
struct Walk {
    cancels: Vec<(String, CancelCause)>,
    decision: Option<Decision>,
}

impl Walk {
    fn offer(&mut self, d: Decision) {
        if self.decision.is_none() {
            self.decision = Some(d);
        }
    }
}

struct Ctx<'a> {
    state: &'a LoopState,
    bp: &'a Blueprint,
    index: BlueprintIndex,
}

impl<'a> Ctx<'a> {
    fn new(state: &'a LoopState, bp: &'a Blueprint) -> Self {
        Ctx {
            state,
            bp,
            index: bp.index(),
        }
    }

    fn spec(&self, node: &str) -> Option<&'a NodeSpec> {
        self.bp.node(node).map(|n| &n.spec)
    }

    fn running_compute(&self) -> Vec<&'a StepRun> {
        self.state
            .running_steps()
            .into_iter()
            .filter(|s| s.kind.is_compute())
            .collect()
    }

    /// The latest gate (any status) about a step.
    fn gate_about(&self, step_id: &str) -> Option<&'a GateRun> {
        self.state
            .gates
            .iter()
            .rev()
            .find(|g| g.step_id.as_deref() == Some(step_id))
    }

    fn in_scope(&self, step: &StepRun, scope: &Scope) -> bool {
        match &scope.repeat {
            None => true,
            Some(r) => {
                self.index.is_descendant(&step.node, r) && step.iter.starts_with(&scope.path)
            }
        }
    }

    fn out_edges_match(&self, node: &str, outcome: Outcome) -> bool {
        self.bp
            .edges
            .iter()
            .any(|e| e.from == node && e.on.matches(outcome))
    }

    fn is_unhandled(&self, node: &str, outcome: Outcome, cause: Option<CancelCause>) -> bool {
        let failure = matches!(
            outcome,
            Outcome::Fail | Outcome::Error | Outcome::Timeout | Outcome::Exhausted
        ) || (outcome == Outcome::Cancelled
            && cause == Some(CancelCause::CancelStep));
        failure && !self.out_edges_match(node, outcome)
    }

    /// The open scopes, outer before inner.
    fn scopes(&self) -> Vec<Scope> {
        let mut scopes = vec![Scope {
            repeat: None,
            path: vec![],
            members: self.index.top_level(),
        }];
        let mut runs: Vec<&RepeatRun> = self
            .state
            .repeats
            .values()
            .filter(|r| r.open && self.repeat_step_running(r))
            .collect();
        runs.sort_by(|a, b| a.outer.len().cmp(&b.outer.len()).then(a.key.cmp(&b.key)));
        for r in runs {
            let mut path = r.outer.clone();
            path.push(r.current);
            scopes.push(Scope {
                repeat: Some(r.node.clone()),
                path,
                members: self.index.children(&r.node),
            });
        }
        scopes
    }

    fn repeat_step_running(&self, run: &RepeatRun) -> bool {
        self.state
            .instances
            .get(&run.key)
            .and_then(|i| i.latest_step.as_deref())
            .and_then(|id| self.state.steps.get(id))
            .is_some_and(|s| s.status == StepStatus::Running)
    }

    /// The key of the scope a repeat instance's iterations belong to: its parent scope.
    fn scope_key(scope: &Scope) -> (Option<String>, Vec<u32>) {
        (scope.repeat.clone(), scope.path.clone())
    }

    fn walk(&self, out: &mut Walk) {
        let mut failing: Vec<(Option<String>, Vec<u32>)> = Vec::new();
        for scope in self.scopes() {
            // A repeat scope is drained when the scope holding its repeat step is failing
            // (or drained itself).
            let outer_drained = scope.repeat.as_ref().is_some_and(|r| {
                let parent = self.index.repeat_chain(r).last().cloned();
                let parent_path = scope.path[..scope.path.len() - 1].to_vec();
                failing.contains(&(parent, parent_path))
            });
            let mode = if outer_drained {
                Mode::Drained
            } else if self.has_unhandled(&scope) {
                Mode::Failing
            } else {
                Mode::Normal
            };
            if mode != Mode::Normal {
                failing.push(Self::scope_key(&scope));
            }
            self.plan_scope(&scope, mode, out);
        }
    }

    fn has_unhandled(&self, scope: &Scope) -> bool {
        scope
            .members
            .iter()
            .any(|m| match self.classify(m, &scope.path, false) {
                Class::Settled(o, c) => self.is_unhandled(m, o, c),
                _ => false,
            })
    }

    fn first_unhandled(&self, scope: &Scope) -> Option<(String, Outcome)> {
        scope
            .members
            .iter()
            .find_map(|m| match self.classify(m, &scope.path, false) {
                Class::Settled(o, c) if self.is_unhandled(m, o, c) => Some((m.clone(), o)),
                _ => None,
            })
    }

    fn plan_scope(&self, scope: &Scope, mode: Mode, out: &mut Walk) {
        let draining = mode != Mode::Normal;
        let cause = CancelCause::ScopeEnded;
        let mut classes = Vec::with_capacity(scope.members.len());
        for m in &scope.members {
            classes.push((m.as_str(), self.classify(m, &scope.path, draining)));
        }

        if draining {
            // Stop what runs directly in this scope; nested scopes drain themselves.
            for s in self.running_compute() {
                if scope.members.contains(&s.node) && s.iter == scope.path {
                    out.cancels.push((s.step_id.clone(), cause));
                }
            }
            for g in self.state.open_gates() {
                let about_member = g
                    .step_id
                    .as_deref()
                    .and_then(|id| self.state.steps.get(id))
                    .is_some_and(|s| scope.members.contains(&s.node) && s.iter == scope.path);
                if about_member {
                    out.offer(Decision::CancelGate {
                        gate_id: g.gate_id.clone(),
                        reason: GateCancelReason::Superseded,
                    });
                }
            }
        }

        for (m, class) in &classes {
            match class {
                Class::Act(d) => {
                    if let Some(d) = self.admissible(d.clone()) {
                        out.offer(d);
                    }
                }
                Class::Undecided if draining => out.offer(Decision::Skip(NodeSkipped {
                    node: m.to_string(),
                    iter: scope.path.clone(),
                    reason: SkipReason::ScopeEnded,
                })),
                Class::Undecided => match self.readiness(m, &scope.members, &scope.path) {
                    Ready::Pending => {}
                    Ready::Dead => out.offer(Decision::Skip(NodeSkipped {
                        node: m.to_string(),
                        iter: scope.path.clone(),
                        reason: SkipReason::DeadPath,
                    })),
                    Ready::Go => {
                        let kind = self.spec(m).map_or(NodeKind::Unknown, NodeSpec::kind);
                        let plan = StartPlan {
                            node: m.to_string(),
                            iter: scope.path.clone(),
                            attempt: 1,
                            kind,
                            cause: StartCause::Ready,
                            retry_of: None,
                            instructions: self.instructions_for(m, kind),
                        };
                        if let Some(d) = self.admissible(Decision::Start(plan)) {
                            out.offer(d);
                        }
                    }
                },
                _ => {}
            }
        }

        // Scope completion.
        let busy = self
            .state
            .running_steps()
            .iter()
            .any(|s| self.in_scope(s, scope));
        let gated = self.state.open_gates().any(|g| {
            g.step_id
                .as_deref()
                .and_then(|id| self.state.steps.get(id))
                .is_some_and(|s| self.in_scope(s, scope))
        });
        if busy || gated {
            return;
        }
        let settled = classes.iter().all(|(_, c)| match c {
            Class::Settled(..) | Class::Skipped => true,
            // Draining: pending retries, recovery and error gates are abandoned.
            Class::Waiting | Class::Act(_) => draining,
            Class::Undecided | Class::Running => false,
        });
        if !settled {
            return;
        }
        let unhandled = self.first_unhandled(scope);
        match &scope.repeat {
            None => out.offer(Decision::Finish(match unhandled {
                None => LoopFinished {
                    status: FinishStatus::Succeeded,
                    reason: FinishReason::Completed,
                    node: None,
                    detail: None,
                },
                Some((node, outcome)) => LoopFinished {
                    status: FinishStatus::Failed,
                    reason: FinishReason::UnhandledOutcome,
                    node: Some(node),
                    detail: Some(format!("ended {outcome} and no edge handles it")),
                },
            })),
            Some(repeat) => {
                let n = *scope.path.last().unwrap_or(&0);
                let max = self.repeat_max(repeat);
                let decision = match (mode, unhandled.is_some()) {
                    (Mode::Drained, _) => IterationDecision::Cancelled,
                    (_, false) => IterationDecision::Break,
                    (_, true) if n < max => IterationDecision::Continue,
                    (_, true) => IterationDecision::Exhausted,
                };
                out.offer(Decision::FinishIteration(IterationFinished {
                    repeat: repeat.clone(),
                    iter: scope.path.clone(),
                    decision,
                }));
            }
        }
    }

    fn repeat_max(&self, repeat: &str) -> u32 {
        match self.spec(repeat) {
            Some(NodeSpec::Repeat(r)) => r.max_iterations,
            _ => 0,
        }
    }

    /// Classify one member instance. `draining` suppresses retries, new gates and new
    /// iterations.
    fn classify(&self, node: &str, path: &[u32], draining: bool) -> Class {
        let key = instance_key(node, path);
        let Some(inst) = self.state.instances.get(&key) else {
            return Class::Undecided;
        };
        let Some(spec) = self.spec(node) else {
            return Class::Waiting;
        };
        let latest = inst
            .latest_step
            .as_deref()
            .and_then(|id| self.state.steps.get(id));
        match inst.status {
            InstanceStatus::Skipped => Class::Skipped,
            InstanceStatus::Running => match spec {
                NodeSpec::Repeat(r) => self.classify_repeat(&key, latest, r, draining),
                NodeSpec::Gate(_) => match latest.and_then(|s| self.gate_closure(s)) {
                    Some(d) => Class::Act(d),
                    None => Class::Running,
                },
                _ => Class::Running,
            },
            InstanceStatus::Interrupted => {
                let Some(latest) = latest else {
                    return Class::Waiting;
                };
                if draining {
                    return Class::Settled(Outcome::Cancelled, Some(CancelCause::ScopeEnded));
                }
                match self.gate_about(&latest.step_id) {
                    Some(g) if g.status == GateStatus::Open => Class::Waiting,
                    Some(g) if g.status == GateStatus::Resolved => {
                        if resolved_option(g) == Some(OPT_RETRY) {
                            Class::Act(Decision::Start(
                                self.retry_plan(latest, StartCause::Recovery),
                            ))
                        } else {
                            // `stop` is carried out by the resolving op; overrides make the
                            // instance done in the fold.
                            Class::Waiting
                        }
                    }
                    Some(_) => Class::Settled(Outcome::Cancelled, Some(CancelCause::ScopeEnded)),
                    None => {
                        let writes = matches!(spec, NodeSpec::Agent(a) if a.access != Access::Read);
                        if writes {
                            Class::Act(Decision::OpenGate {
                                kind: GateKind::Recovery,
                                step_id: Some(latest.step_id.clone()),
                            })
                        } else {
                            Class::Act(Decision::Start(
                                self.retry_plan(latest, StartCause::Recovery),
                            ))
                        }
                    }
                }
            }
            InstanceStatus::Done => {
                let outcome = inst.outcome.unwrap_or(Outcome::Unknown);
                let cancel = latest.and_then(|s| s.cancel);
                let Some(latest) = latest.filter(|_| spec.kind().is_compute()) else {
                    return Class::Settled(outcome, cancel);
                };
                match outcome {
                    Outcome::Error | Outcome::Timeout => match self.gate_about(&latest.step_id) {
                        Some(g) if g.status == GateStatus::Open => Class::Waiting,
                        Some(g)
                            if g.status == GateStatus::Resolved
                                && resolved_option(g) == Some(OPT_RETRY)
                                && !draining =>
                        {
                            Class::Act(Decision::Start(self.retry_plan(latest, StartCause::Retry)))
                        }
                        Some(_) => Class::Settled(outcome, cancel),
                        None if draining => Class::Settled(outcome, cancel),
                        None if inst.attempts <= retries_of(spec) => {
                            Class::Act(Decision::Start(self.retry_plan(latest, StartCause::Retry)))
                        }
                        None if self.bp.policy.on_error == OnError::Gate => {
                            Class::Act(Decision::OpenGate {
                                kind: GateKind::StepError,
                                step_id: Some(latest.step_id.clone()),
                            })
                        }
                        None => Class::Settled(outcome, cancel),
                    },
                    Outcome::Cancelled if !draining => match cancel {
                        Some(CancelCause::Pause) => {
                            Class::Act(Decision::Start(self.retry_plan(latest, StartCause::Resume)))
                        }
                        Some(CancelCause::Instruction) => Class::Act(Decision::Start(
                            self.retry_plan(latest, StartCause::Instruction),
                        )),
                        _ => Class::Settled(outcome, cancel),
                    },
                    _ => Class::Settled(outcome, cancel),
                }
            }
            InstanceStatus::Unknown => Class::Waiting,
        }
    }

    fn classify_repeat(
        &self,
        key: &str,
        latest: Option<&StepRun>,
        spec: &RepeatSpec,
        draining: bool,
    ) -> Class {
        let Some(step) = latest else {
            return Class::Waiting;
        };
        let Some(run) = self.state.repeats.get(key) else {
            return Class::Waiting;
        };
        if run.open {
            return Class::Running;
        }
        let finish = |outcome: Outcome, cancel: Option<CancelCause>| {
            Class::Act(Decision::FinishStep {
                step_id: step.step_id.clone(),
                outcome,
                cancel,
                feedback: vec![],
            })
        };
        match run.decisions.last() {
            Some(IterationDecision::Break) => finish(Outcome::Ok, None),
            Some(IterationDecision::Exhausted) => finish(Outcome::Exhausted, None),
            None | Some(IterationDecision::Continue) if !draining => {
                let n = run.current + 1;
                if n > spec.max_iterations {
                    return finish(Outcome::Cancelled, Some(CancelCause::ScopeEnded));
                }
                let mut iter = run.outer.clone();
                iter.push(n);
                Class::Act(Decision::StartIteration(IterationStarted {
                    repeat: run.node.clone(),
                    iter,
                    feedback: self.next_feedback(run, spec.feedback),
                }))
            }
            _ => finish(Outcome::Cancelled, Some(CancelCause::ScopeEnded)),
        }
    }

    /// Feedback for the iteration after `run.current`.
    fn next_feedback(&self, run: &RepeatRun, mode: FeedbackMode) -> Vec<FeedbackItem> {
        if run.current == 0 || mode == FeedbackMode::None {
            return vec![];
        }
        let mut path = run.outer.clone();
        path.push(run.current);
        let scope = Scope {
            repeat: Some(run.node.clone()),
            path,
            members: vec![],
        };
        let mut steps: Vec<&StepRun> = self
            .state
            .steps
            .values()
            .filter(|s| self.in_scope(s, &scope))
            .collect();
        steps.sort_by_key(|s| s.started_seq);
        let fresh = steps.iter().flat_map(|s| s.feedback.iter().cloned());
        let mut items: Vec<FeedbackItem> = match mode {
            FeedbackMode::All => run.feedback.iter().cloned().chain(fresh).collect(),
            _ => fresh.collect(),
        };
        if items.len() > MAX_FEEDBACK_ITEMS {
            items.drain(..items.len() - MAX_FEEDBACK_ITEMS);
        }
        items
    }

    /// A gate step whose gate closed: the step ends with the gate's closure.
    fn gate_closure(&self, step: &StepRun) -> Option<Decision> {
        let gate = self.gate_about(&step.step_id)?;
        let (outcome, cancel, feedback) = match gate.status {
            GateStatus::Open => return None,
            GateStatus::Resolved => {
                let res = gate.resolution.as_ref()?;
                let outcome = gate
                    .options
                    .iter()
                    .find(|o| o.id == res.option)
                    .and_then(|o| o.outcome)
                    .unwrap_or(Outcome::Fail);
                let mut feedback = vec![];
                if res.comment.is_some() || outcome != Outcome::Ok {
                    let comment = res.comment.clone().unwrap_or_default();
                    let first = comment.lines().next().unwrap_or("").trim();
                    feedback.push(FeedbackItem {
                        source: FeedbackSource::Human,
                        node: step.node.clone(),
                        step_id: Some(step.step_id.clone()),
                        summary: clamp_text(
                            &if first.is_empty() {
                                format!("{} answered {}", step.node, res.option)
                            } else {
                                format!("{}: {first}", res.option)
                            },
                            MAX_SHORT_BYTES,
                        ),
                        detail: res
                            .comment
                            .as_ref()
                            .map(|c| clamp_text(c, MAX_DETAIL_BYTES)),
                    });
                }
                (outcome, None, feedback)
            }
            GateStatus::Cancelled if gate.cancel_reason == Some(GateCancelReason::TimedOut) => {
                (Outcome::Timeout, None, vec![])
            }
            GateStatus::Cancelled | GateStatus::Unknown => {
                let cause = if self.state.status == LoopStatus::Stopping {
                    CancelCause::Stop
                } else {
                    CancelCause::ScopeEnded
                };
                (Outcome::Cancelled, Some(cause), vec![])
            }
        };
        Some(Decision::FinishStep {
            step_id: step.step_id.clone(),
            outcome,
            cancel,
            feedback,
        })
    }

    fn retry_plan(&self, latest: &StepRun, cause: StartCause) -> StartPlan {
        StartPlan {
            node: latest.node.clone(),
            iter: latest.iter.clone(),
            attempt: latest.attempt + 1,
            kind: latest.kind,
            cause,
            retry_of: Some(latest.step_id.clone()),
            instructions: self.instructions_for(&latest.node, latest.kind),
        }
    }

    fn instructions_for(&self, node: &str, kind: NodeKind) -> Vec<String> {
        if kind != NodeKind::Agent {
            return vec![];
        }
        self.state
            .pending_instructions()
            .filter(|i| i.target.as_deref().is_none_or(|t| t == node))
            .map(|i| i.instruction_id.clone())
            .collect()
    }

    /// Whether an in-edge set lets `node` run.
    fn readiness(&self, node: &str, members: &[String], path: &[u32]) -> Ready {
        let mut any = false;
        let mut fired = false;
        for e in self.bp.edges.iter().filter(|e| e.to == node) {
            if !members.contains(&e.from) {
                continue;
            }
            any = true;
            match self.classify(&e.from, path, false) {
                Class::Settled(o, _) if e.on.matches(o) => fired = true,
                Class::Settled(..) | Class::Skipped => {}
                _ => return Ready::Pending,
            }
        }
        match (any, fired) {
            (false, _) | (true, true) => Ready::Go,
            (true, false) => Ready::Dead,
        }
    }

    /// Gate a start behind the budget, recovery and concurrency rules. `None` = not now.
    fn admissible(&self, d: Decision) -> Option<Decision> {
        let Decision::Start(plan) = &d else {
            return Some(d);
        };
        if !plan.kind.is_compute() {
            return Some(d);
        }
        if self
            .state
            .open_gates()
            .any(|g| g.kind == GateKind::Recovery)
        {
            return None;
        }
        if budget_exhausted(self.state) {
            if self.state.open_gates().any(|g| g.kind == GateKind::Budget) {
                return None;
            }
            return Some(match self.bp.policy.on_budget {
                OnBudget::Fail => Decision::Stop {
                    mode: StopMode::Graceful,
                    reason: STOP_REASON_BUDGET.into(),
                },
                _ => Decision::OpenGate {
                    kind: GateKind::Budget,
                    step_id: None,
                },
            });
        }
        let access = match self.spec(&plan.node) {
            Some(NodeSpec::Agent(a)) => a.access,
            _ => Access::Read,
        };
        let running = self.running_compute();
        if access == Access::Write && !running.is_empty() {
            return None;
        }
        let writer = running.iter().any(|s| s.access == Some(Access::Write));
        if writer || running.len() as u32 >= self.state.budget.max_parallel {
            return None;
        }
        Some(d)
    }

    /// Paused: only close gate steps whose gate already closed.
    fn finish_closed_gate_steps(&self) -> Option<Decision> {
        self.state
            .running_steps()
            .into_iter()
            .filter(|s| s.kind == NodeKind::Gate)
            .find_map(|s| self.gate_closure(s))
    }

    /// Stopping: cancel gates, then close gate steps, iterations (innermost first) and
    /// repeat steps, then finish.
    fn stop_drain(&self) -> Option<Decision> {
        if let Some(g) = self.state.open_gates().next() {
            return Some(Decision::CancelGate {
                gate_id: g.gate_id.clone(),
                reason: GateCancelReason::Stopped,
            });
        }
        if let Some(d) = self.finish_closed_gate_steps() {
            return Some(d);
        }
        let mut open: Vec<&RepeatRun> = self.state.repeats.values().filter(|r| r.open).collect();
        open.sort_by(|a, b| b.outer.len().cmp(&a.outer.len()));
        for run in open {
            let mut path = run.outer.clone();
            path.push(run.current);
            let scope = Scope {
                repeat: Some(run.node.clone()),
                path: path.clone(),
                members: vec![],
            };
            if !self
                .state
                .running_steps()
                .iter()
                .any(|s| self.in_scope(s, &scope))
            {
                return Some(Decision::FinishIteration(IterationFinished {
                    repeat: run.node.clone(),
                    iter: path,
                    decision: IterationDecision::Cancelled,
                }));
            }
        }
        for s in self.state.running_steps() {
            if s.kind != NodeKind::Repeat {
                continue;
            }
            let key = instance_key(&s.node, &s.iter);
            if let Some(run) = self.state.repeats.get(&key).filter(|r| !r.open) {
                let outcome = match run.decisions.last() {
                    Some(IterationDecision::Break) => Outcome::Ok,
                    Some(IterationDecision::Exhausted) => Outcome::Exhausted,
                    _ => Outcome::Cancelled,
                };
                return Some(Decision::FinishStep {
                    step_id: s.step_id.clone(),
                    outcome,
                    cancel: (outcome == Outcome::Cancelled).then_some(CancelCause::Stop),
                    feedback: vec![],
                });
            }
        }
        if self.state.running_steps().is_empty() {
            let budget = self.state.stop_reason.as_deref() == Some(STOP_REASON_BUDGET);
            return Some(Decision::Finish(LoopFinished {
                status: if budget {
                    FinishStatus::Failed
                } else {
                    FinishStatus::Cancelled
                },
                reason: if budget {
                    FinishReason::Budget
                } else {
                    FinishReason::Stopped
                },
                node: None,
                detail: None,
            }));
        }
        None
    }
}

enum Ready {
    Pending,
    Dead,
    Go,
}

fn resolved_option(g: &GateRun) -> Option<&str> {
    g.resolution.as_ref().map(|r| r.option.as_str())
}

fn retries_of(spec: &NodeSpec) -> u32 {
    match spec {
        NodeSpec::Agent(a) => a.retries,
        NodeSpec::Check(c) => c.retries,
        _ => 0,
    }
}
