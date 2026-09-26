//! A loop as a screen shows it (design §5 `LoopView`, §12 canvas and board).
//!
//! A pure projection of the fold, so the dashboard bridge never re-implements the state
//! machine: for every blueprint node, the instance the canvas should show (the one in the
//! current iteration of each enclosing repeat), its phase, who runs it, how it ended; plus
//! the recent steps, the gates and the instructions. The webview renders this and nothing
//! else.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::{
    instance_key, string_enum, Assignee, GateRun, GateStatus, InstanceStatus, InstructionRun,
    IterationHead, LoopState, LoopSummary, NodeKind, Outcome, StartCause, StateWarning, StepRun,
    StepStatus,
};

/// Steps kept in a view (newest first).
pub const VIEW_RECENT_STEPS: usize = 50;
/// Gates and instructions kept in a view (newest first).
pub const VIEW_RECENT_ITEMS: usize = 100;
/// Fold warnings kept in a view (newest last).
pub const VIEW_WARNINGS: usize = 20;

string_enum! {
    /// Where a node stands in the iteration the canvas shows.
    pub enum NodePhase {
        /// Not decided yet in the shown iteration.
        Idle = "idle",
        Running = "running",
        /// A person has to answer a gate about it (approval, step error, recovery).
        Waiting = "waiting",
        Done = "done",
        Skipped = "skipped",
        /// Its runner died while it ran; recovery decides what happens.
        Interrupted = "interrupted",
    }
}

/// One blueprint node, as the canvas draws it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NodeView {
    pub node: String,
    pub kind: NodeKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    pub phase: NodePhase,
    /// The shown instance's iteration path (`[2]` = second iteration of its repeat).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub iter: Vec<u32>,
    /// The shown instance's effective outcome, once decided.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<Outcome>,
    #[serde(default)]
    pub attempts: u32,
    /// The shown instance's latest step.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assignee: Option<Assignee>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub excerpt: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_ts: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_ts: Option<u64>,
    /// An open gate about this node (answer it from the inbox).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gate_id: Option<String>,
    /// Instances decided so far, over every iteration.
    #[serde(default)]
    pub runs: u32,
    /// The most recent decided outcome of any instance (what happened last time).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_outcome: Option<Outcome>,
    /// Repeat nodes: the shown instance's iteration badge.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub iteration: Option<IterationHead>,
}

/// One attempt, as the step list shows it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StepView {
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
    pub assignee: Option<Assignee>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    pub started_ts: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_ts: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub elapsed_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub excerpt: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl From<&StepRun> for StepView {
    fn from(s: &StepRun) -> Self {
        StepView {
            step_id: s.step_id.clone(),
            node: s.node.clone(),
            iter: s.iter.clone(),
            attempt: s.attempt,
            kind: s.kind,
            cause: s.cause,
            status: s.status,
            outcome: s.outcome,
            assignee: s.assignee.clone(),
            run_id: s.run_id.clone(),
            started_ts: s.started_ts,
            finished_ts: s.finished_ts,
            elapsed_ms: s.elapsed_ms,
            excerpt: s.excerpt.clone(),
            error: s.error.clone(),
        }
    }
}

/// Everything a loop's screen shows (design §5).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LoopView {
    pub summary: LoopSummary,
    /// The frozen blueprint document (nodes, edges, layout). Present only when asked for:
    /// it never changes, so a client fetches it once per loop.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blueprint: Option<serde_json::Value>,
    /// Every blueprint node, in declaration order.
    pub nodes: Vec<NodeView>,
    /// Newest first, at most [`VIEW_RECENT_STEPS`].
    pub steps: Vec<StepView>,
    /// Newest first, at most [`VIEW_RECENT_ITEMS`].
    pub gates: Vec<GateRun>,
    /// Newest first, at most [`VIEW_RECENT_ITEMS`].
    pub instructions: Vec<InstructionRun>,
    /// Oldest first, at most [`VIEW_WARNINGS`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<StateWarning>,
    /// Why this build may not change the loop (it can still be shown).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_only: Option<String>,
}

/// Project a fold. `None` before `loop_created`.
pub fn loop_view(state: &LoopState, with_blueprint: bool) -> Option<LoopView> {
    let summary = state.summary()?;
    let created = state.created.as_deref()?;
    let nodes = match &state.blueprint {
        Some(bp) => {
            let index = bp.index();
            bp.nodes
                .iter()
                .map(|n| node_view(state, &index.repeat_chain(&n.id), n))
                .collect()
        }
        None => vec![],
    };
    let mut steps: Vec<&StepRun> = state.steps.values().collect();
    steps.sort_by(|a, b| b.started_seq.cmp(&a.started_seq));
    Some(LoopView {
        summary,
        blueprint: with_blueprint.then(|| created.blueprint.doc.clone()),
        nodes,
        steps: steps
            .into_iter()
            .take(VIEW_RECENT_STEPS)
            .map(StepView::from)
            .collect(),
        gates: state
            .gates
            .iter()
            .rev()
            .take(VIEW_RECENT_ITEMS)
            .cloned()
            .collect(),
        instructions: state
            .instructions
            .iter()
            .rev()
            .take(VIEW_RECENT_ITEMS)
            .cloned()
            .collect(),
        warnings: state
            .warnings
            .iter()
            .skip(state.warnings.len().saturating_sub(VIEW_WARNINGS))
            .cloned()
            .collect(),
        read_only: state.writable().err().map(|r| r.to_string()),
    })
}

/// The iteration path of the current iteration of every repeat enclosing a node (`chain`
/// is outer → inner). Shorter than `chain` when an enclosing iteration has not started.
fn current_path(state: &LoopState, chain: &[String]) -> Vec<u32> {
    let mut path = Vec::with_capacity(chain.len());
    for repeat in chain {
        match state.repeats.get(&instance_key(repeat, &path)) {
            Some(run) if run.current > 0 => path.push(run.current),
            _ => break,
        }
    }
    path
}

fn node_view(state: &LoopState, chain: &[String], node: &super::Node) -> NodeView {
    let id = node.id.as_str();
    let mut view = NodeView {
        node: id.to_string(),
        kind: node.spec.kind(),
        parent: node.parent.clone(),
        phase: NodePhase::Idle,
        iter: vec![],
        outcome: None,
        attempts: 0,
        step_id: None,
        assignee: None,
        excerpt: None,
        error: None,
        started_ts: None,
        finished_ts: None,
        gate_id: None,
        runs: 0,
        last_outcome: None,
        iteration: None,
    };

    // Every instance of the node, and the latest step of each.
    let latest_step = |key: &str| {
        state
            .instances
            .get(key)
            .and_then(|i| i.latest_step.as_deref())
            .and_then(|s| state.steps.get(s))
    };
    let mut decided: BTreeMap<u64, Outcome> = BTreeMap::new();
    for inst in state.instances.values().filter(|i| i.node == id) {
        if inst.status != InstanceStatus::Running {
            view.runs += 1;
        }
        if let Some(outcome) = inst.outcome {
            let seq = latest_step(&inst.key).map_or(0, |s| s.started_seq);
            decided.insert(seq, outcome);
        }
    }
    view.last_outcome = decided.values().next_back().copied();

    // The instance the canvas shows: the one in the current iteration of every enclosing
    // repeat; failing that (an enclosing iteration has not started yet), the latest one.
    let path = current_path(state, chain);
    let shown = if path.len() == chain.len() {
        view.iter = path.clone();
        state.instance(id, &path)
    } else {
        state
            .instances
            .values()
            .filter(|i| i.node == id)
            .max_by_key(|i| latest_step(&i.key).map_or(0, |s| s.started_seq))
    };
    let Some(inst) = shown else {
        return view;
    };
    view.iter = inst.iter.clone();
    view.outcome = inst.outcome;
    view.attempts = inst.attempts;
    view.phase = match inst.status {
        InstanceStatus::Running => NodePhase::Running,
        InstanceStatus::Done => NodePhase::Done,
        InstanceStatus::Skipped => NodePhase::Skipped,
        InstanceStatus::Interrupted => NodePhase::Interrupted,
        InstanceStatus::Unknown => NodePhase::Unknown,
    };
    if let Some(step) = latest_step(&inst.key) {
        view.step_id = Some(step.step_id.clone());
        view.assignee = step.assignee.clone();
        view.excerpt = step.excerpt.clone();
        view.error = step.error.clone();
        view.started_ts = Some(step.started_ts);
        view.finished_ts = step.finished_ts;
    }
    if let Some(gate) = state.gates.iter().rev().find(|g| {
        g.status == GateStatus::Open && g.node.as_deref() == Some(id) && g.iter == inst.iter
    }) {
        view.gate_id = Some(gate.gate_id.clone());
        view.phase = NodePhase::Waiting;
    }
    if node.spec.kind() == NodeKind::Repeat {
        if let Some(run) = state.repeats.get(&inst.key).filter(|r| r.current > 0) {
            view.iteration = Some(IterationHead {
                repeat: run.node.clone(),
                outer: run.outer.clone(),
                n: run.current,
                max: run.max_iterations,
                open: run.open,
            });
        }
    }
    view
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::loops::{decode_line, replay, LoadedRecord};

    const JOURNAL: &str = include_str!("../../tests/fixtures/loops/journal_fix_until_green.jsonl");

    fn records() -> Vec<LoadedRecord> {
        JOURNAL
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| decode_line(l).unwrap())
            .collect()
    }

    fn node<'a>(v: &'a LoopView, id: &str) -> &'a NodeView {
        v.nodes.iter().find(|n| n.node == id).unwrap()
    }

    #[test]
    fn a_finished_loop_shows_every_node_as_it_last_stood() {
        let state = replay(&records());
        let v = loop_view(&state, true).unwrap();
        assert!(v.blueprint.is_some());
        let ids: Vec<&str> = v.nodes.iter().map(|n| n.node.as_str()).collect();
        assert_eq!(ids, ["plan", "fix", "implement", "test", "signoff"]);
        assert!(
            v.nodes.iter().all(|n| n.phase == NodePhase::Done),
            "{:?}",
            v.nodes
        );
        let fix = node(&v, "fix");
        let badge = fix.iteration.as_ref().unwrap();
        assert_eq!((badge.n, badge.max, badge.open), (2, 4, false));
        // The body shows the last iteration, and remembers there were two.
        let test = node(&v, "test");
        assert_eq!(test.iter, [2]);
        assert_eq!(test.outcome, Some(Outcome::Ok));
        assert_eq!(test.runs, 2);
        assert!(v
            .steps
            .windows(2)
            .all(|w| w[0].started_ts >= w[1].started_ts));
        assert_eq!(v.read_only, None);
        assert!(loop_view(&state, false).unwrap().blueprint.is_none());
    }

    #[test]
    fn mid_run_the_canvas_follows_the_current_iteration() {
        let all = records();
        // Replay up to (and including) the second iteration's start.
        let cut = all
            .iter()
            .position(|r| r.kind == "iteration_started" && r.raw.contains("\"iter\":[2]"))
            .unwrap();
        let state = replay(&all[..=cut]);
        let v = loop_view(&state, false).unwrap();
        let implement = node(&v, "implement");
        // Iteration 2 has started but implement has not run in it yet.
        assert_eq!(implement.iter, [2]);
        assert_eq!(implement.phase, NodePhase::Idle);
        assert_eq!(implement.last_outcome, Some(Outcome::Ok));
        let test = node(&v, "test");
        assert_eq!(test.phase, NodePhase::Idle);
        assert_eq!(test.last_outcome, Some(Outcome::Fail));
        let fix = node(&v, "fix");
        assert_eq!(fix.phase, NodePhase::Running);
        assert_eq!(fix.iteration.as_ref().unwrap().n, 2);
    }

    #[test]
    fn an_open_approval_gate_marks_its_node_waiting() {
        let all = records();
        let cut = all.iter().position(|r| r.kind == "gate_opened").unwrap();
        let state = replay(&all[..=cut]);
        let v = loop_view(&state, false).unwrap();
        let signoff = node(&v, "signoff");
        assert_eq!(signoff.phase, NodePhase::Waiting);
        assert_eq!(signoff.gate_id.as_deref(), Some("g1"));
        assert!(v.summary.waiting);
    }
}
