//! Scheduler decisions → journal records.
//!
//! Shared by the runner and the scheduler's simulation tests, so the tests exercise exactly
//! the records production writes. Effects (resolving the assignee, rendering the prompt,
//! naming the run) are the runner's; they arrive here as a finished [`Prep`].

use agentpit_events::loops::*;

use super::sched::{Decision, StartPlan, extended_budget, standard_gate_options};

/// The approval gate a gate step opens in the same batch as its `step_started`.
#[derive(Debug, Clone, PartialEq)]
pub struct GatePrep {
    pub prompt: String,
    pub options: Vec<GateOption>,
    pub deadline_ms: Option<u64>,
    pub on_timeout: Option<String>,
}

/// Everything `step_started` must say about the effect before anything is spawned
/// (write-ahead authorization, design §4.3 rule 8).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Prep {
    pub access: Option<Access>,
    pub assignee: Option<Assignee>,
    pub run_id: Option<String>,
    pub prompt: Option<BlobRef>,
    pub command: Option<String>,
    pub deadline_ms: Option<u64>,
    pub gate: Option<GatePrep>,
}

/// The records that start one attempt.
pub fn start_events(state: &LoopState, plan: &StartPlan, prep: Prep) -> Vec<LoopEvent> {
    let step_id = plan.step_id();
    let mut events = vec![LoopEvent::StepStarted(Box::new(StepStarted {
        step_id: step_id.clone(),
        node: plan.node.clone(),
        iter: plan.iter.clone(),
        attempt: plan.attempt,
        kind: plan.kind,
        cause: plan.cause,
        retry_of: plan.retry_of.clone(),
        access: prep.access,
        assignee: prep.assignee,
        run_id: prep.run_id,
        prompt: prep.prompt,
        command: prep.command,
        deadline_ms: prep.deadline_ms,
        instructions: plan.instructions.clone(),
    }))];
    if let Some(gate) = prep.gate {
        events.push(LoopEvent::GateOpened(Box::new(GateOpened {
            gate_id: gate_id(state.gates.len() + 1),
            kind: GateKind::Approval,
            step_id: Some(step_id),
            node: Some(plan.node.clone()),
            iter: plan.iter.clone(),
            prompt: clamp_text(&gate.prompt, MAX_TEXT_BYTES),
            options: gate.options,
            deadline_ms: gate.deadline_ms,
            on_timeout: gate.on_timeout,
        })));
    }
    events
}

/// The records for every decision except `Start` (whose effect the runner prepares).
pub fn decision_events(state: &LoopState, decision: &Decision) -> Vec<LoopEvent> {
    match decision {
        Decision::Start(_) => vec![],
        Decision::StartIteration(i) => vec![LoopEvent::IterationStarted(i.clone())],
        Decision::FinishIteration(i) => vec![LoopEvent::IterationFinished(i.clone())],
        Decision::FinishStep {
            step_id,
            outcome,
            cancel,
            feedback,
        } => {
            let elapsed = state
                .steps
                .get(step_id)
                .map_or(0, |s| state.last_ts.saturating_sub(s.started_ts));
            vec![LoopEvent::StepFinished(Box::new(StepFinished {
                step_id: step_id.clone(),
                outcome: *outcome,
                elapsed_ms: elapsed,
                exit_code: None,
                verdict: None,
                output: None,
                excerpt: None,
                log_tail: None,
                error: None,
                feedback: feedback.clone(),
                cancel: *cancel,
            }))]
        }
        Decision::Skip(s) => vec![LoopEvent::NodeSkipped(s.clone())],
        Decision::OpenGate { kind, step_id } => {
            let step = step_id.as_deref().and_then(|id| state.steps.get(id));
            let prompt = match (kind, step) {
                (GateKind::StepError, Some(s)) => format!(
                    "{} ended {}{}. Retry it, treat it as failed, or stop the loop?",
                    s.step_id,
                    s.outcome.map_or("with an error".into(), |o| o.to_string()),
                    s.error
                        .as_deref()
                        .map(|e| format!(": {}", clamp_text(e, 400)))
                        .unwrap_or_default(),
                ),
                (GateKind::Recovery, Some(s)) => format!(
                    "{} was interrupted when its runner stopped. It may have left partial edits in \
                     the working tree, and its agent process may still be running. Retry it, keep \
                     its edits and continue, or stop the loop?",
                    s.step_id
                ),
                _ => format!(
                    "The loop used its budget ({} of {} steps, {}s of {}s compute). Extend it or \
                     stop the loop?",
                    state.usage.steps,
                    state.budget.max_steps,
                    state.usage.active_ms / 1000,
                    state.budget.max_active_secs
                ),
            };
            vec![LoopEvent::GateOpened(Box::new(GateOpened {
                gate_id: gate_id(state.gates.len() + 1),
                kind: *kind,
                step_id: step.map(|s| s.step_id.clone()),
                node: step.map(|s| s.node.clone()),
                iter: step.map(|s| s.iter.clone()).unwrap_or_default(),
                prompt,
                options: standard_gate_options(*kind),
                deadline_ms: None,
                on_timeout: None,
            }))]
        }
        Decision::TimeoutGate { gate_id, option } => match option {
            Some(option) => vec![LoopEvent::GateResolved(GateResolved {
                gate_id: gate_id.clone(),
                option: option.clone(),
                comment: None,
                by: Actor {
                    kind: ActorKind::Timeout,
                    client: None,
                },
            })],
            None => vec![LoopEvent::GateCancelled(GateCancelled {
                gate_id: gate_id.clone(),
                reason: GateCancelReason::TimedOut,
            })],
        },
        Decision::CancelGate { gate_id, reason } => vec![LoopEvent::GateCancelled(GateCancelled {
            gate_id: gate_id.clone(),
            reason: *reason,
        })],
        Decision::Stop { mode, reason } => vec![LoopEvent::LoopStopRequested(LoopStopRequested {
            mode: *mode,
            reason: Some(reason.clone()),
        })],
        Decision::Finish(f) => vec![LoopEvent::LoopFinished(f.clone())],
    }
}

/// Records that must accompany a gate answer (the gate's "follow-up", written in the same
/// batch so it can never be lost to a crash between the two).
pub fn gate_follow_up(
    state: &LoopState,
    gate: &GateRun,
    option: &str,
    by: &Actor,
) -> Vec<LoopEvent> {
    use super::sched::{OPT_EXTEND, OPT_STOP};
    match (gate.kind, option) {
        (GateKind::Budget, OPT_EXTEND) => vec![LoopEvent::BudgetChanged(BudgetChanged {
            budget: extended_budget(state.budget),
            by: by.clone(),
        })],
        (GateKind::Budget | GateKind::StepError | GateKind::Recovery, OPT_STOP) => {
            vec![LoopEvent::LoopStopRequested(LoopStopRequested {
                mode: StopMode::Graceful,
                reason: Some(format!("{} answered {OPT_STOP}", gate.gate_id)),
            })]
        }
        _ => vec![],
    }
}
