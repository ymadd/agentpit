//! Live + raw-replay runner: the bridge that makes the deterministic graders reachable.
//!
//! [`replay`](super::replay) folds *already-graded* `passed/total` counts into a result. This
//! module instead works at the *raw output* level: it takes each [`GoldTask`]'s verbatim
//! candidate output — captured live ([`run_live`]) or recorded ([`RawFixture`]) — and runs it
//! through [`judge::grade`](super::judge::grade), so the real scorers actually execute. Tasks the
//! sandbox jail could not run ([`GradeOutcome::Skipped`]) are excluded (never silently passed) and
//! their ids surfaced; the rest aggregate via [`merge::aggregate`](super::merge::aggregate).
//!
//! Pure where it can be: [`score_raw`] borrows its inputs and returns a fresh result. Only
//! [`run_live`] touches the network, dispatching each task as an isolated one-shot.

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use super::judge::{GradeOutcome, grade};
use super::merge::{GradedTask, aggregate};
use super::suite::GoldTask;
use crate::dispatch::{Registries, dispatch};
use crate::profile::model::BenchmarkResult;
use crate::types::BackendId;

/// One gold task's verbatim output from a candidate backend.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RawOutput {
    /// Must equal a [`GoldTask::id`] in the suite.
    pub task_id: String,
    /// The candidate backend's raw response, graded as-is.
    pub output: String,
}

/// A recorded *raw-output* fixture for one backend: every gold task's verbatim response, ready to
/// be re-graded offline. Distinct from [`ReplayFixture`](super::replay::ReplayFixture), which
/// records already-graded `passed/total` counts — this records the raw text so the graders run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RawFixture {
    /// Backend these outputs were produced by.
    pub backend: BackendId,
    /// Per-task verbatim outputs.
    pub outputs: Vec<RawOutput>,
    /// Optional ISO-8601 measurement time, carried through onto the merged profile.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub measured_at: Option<String>,
}

/// The result of grading a set of raw outputs: the aggregated benchmark result, plus the ids of
/// tasks excluded because the sandbox jail was unavailable (logged by the caller, never passed).
#[derive(Debug, Clone, PartialEq)]
pub struct RawScored {
    /// Aggregated per-category scores from the tasks that were actually graded.
    pub result: BenchmarkResult,
    /// Ids of tasks skipped (sandbox unavailable) — neither pass nor fail.
    pub skipped: Vec<String>,
    /// How many tasks contributed a real score.
    pub graded: usize,
}

/// Grade every raw output through [`judge::grade`](super::judge::grade) and aggregate into a
/// [`BenchmarkResult`]. Validates that each `task_id` names a suite task. Skipped tasks are
/// excluded and their ids returned. Pure: borrows `tasks` and `fixture`, returns a fresh result.
pub fn score_raw(tasks: &[GoldTask], fixture: &RawFixture) -> Result<RawScored> {
    let mut graded: Vec<GradedTask> = Vec::new();
    let mut skipped: Vec<String> = Vec::new();

    for raw in &fixture.outputs {
        let task = tasks
            .iter()
            .find(|t| t.id == raw.task_id)
            .ok_or_else(|| anyhow!("fixture references unknown task id: {}", raw.task_id))?;
        match grade(task, &raw.output) {
            GradeOutcome::Scored(score) => graded.push(GradedTask::new(task.category, score)),
            GradeOutcome::Skipped => skipped.push(raw.task_id.clone()),
        }
    }

    let result = aggregate(&graded, fixture.measured_at.clone());
    Ok(RawScored {
        result,
        skipped,
        graded: graded.len(),
    })
}

/// Dispatch every gold task to `backend` once, capturing its raw output into a [`RawFixture`].
///
/// Each task runs as an independent one-shot exec; its chunks are streamed to `on_chunk` (so the
/// dashboard can tail the sweep live) while the full captured output is what gets graded. This is
/// the live, network-touching path — auth is the caller's responsibility (checked before this
/// runs). A `cancel` trip aborts the in-flight dispatch and propagates the error.
pub async fn run_live(
    backend: BackendId,
    tasks: &[GoldTask],
    cwd: &Path,
    regs: &Registries,
    measured_at: Option<String>,
    cancel: CancellationToken,
    on_chunk: Arc<dyn Fn(&str) + Send + Sync>,
) -> Result<RawFixture> {
    let mut outputs = Vec::with_capacity(tasks.len());
    for task in tasks {
        let res = dispatch(backend, &task.prompt, cwd, cancel.clone(), on_chunk.clone(), regs)
            .await
            .with_context(|| format!("dispatch failed for gold task {}", task.id))?;
        outputs.push(RawOutput {
            task_id: task.id.clone(),
            output: res.output,
        });
    }
    Ok(RawFixture {
        backend,
        outputs,
        measured_at,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile::bench::all_tasks;
    use crate::profile::category::TaskCategory;

    fn raw(task_id: &str, output: &str) -> RawOutput {
        RawOutput {
            task_id: task_id.to_string(),
            output: output.to_string(),
        }
    }

    #[test]
    fn grades_review_output_through_the_real_scorer() {
        // A raw review output is graded by judge::score_review (not pre-counted), proving the
        // grader is actually reached from the runner. The api_handler_bug review task carries
        // embedded defects; an empty json array reports nothing → F1 0 for a defect-bearing task.
        let tasks = all_tasks();
        let fixture = RawFixture {
            backend: BackendId::Codex,
            outputs: vec![raw(
                "review/api_handler_bug",
                "no issues found\n```json\n[]\n```",
            )],
            measured_at: Some("2026-06-30T00:00:00Z".into()),
        };
        let scored = score_raw(&tasks, &fixture).unwrap();
        assert_eq!(scored.graded, 1);
        assert!(scored.skipped.is_empty());
        // Missing every real defect scores 0 for this category.
        let review = scored.result.scores.get(&TaskCategory::Review).unwrap();
        assert_eq!(review.value, 0);
    }

    #[test]
    fn unknown_task_id_is_an_error() {
        let tasks = all_tasks();
        let fixture = RawFixture {
            backend: BackendId::Codex,
            outputs: vec![raw("coding/does_not_exist", "```python\n```")],
            measured_at: None,
        };
        let err = score_raw(&tasks, &fixture).unwrap_err();
        assert!(
            format!("{err:#}").contains("unknown task id"),
            "got: {err:#}"
        );
    }

    #[test]
    fn raw_fixture_round_trips_through_json() {
        let fixture = RawFixture {
            backend: BackendId::Claude,
            outputs: vec![raw("review/api_handler_bug", "```json\n[]\n```")],
            measured_at: Some("2026-06-30T00:00:00Z".into()),
        };
        let json = serde_json::to_string(&fixture).unwrap();
        let back: RawFixture = serde_json::from_str(&json).unwrap();
        assert_eq!(back, fixture);
    }
}
