//! Offline replay scoring for `agentpit profile run`.
//!
//! The gold-bench harness's *live* path runs each [`GoldTask`](super::suite::GoldTask) against
//! a backend and grades it in a network-isolated sandbox (design §2.1) — that is a manual step.
//! This module is the *offline* path: it folds a recorded fixture of per-task outcomes into a
//! [`BenchmarkResult`] that the standard merge ([`apply_benchmark`](crate::profile::apply_benchmark))
//! writes into `profiles.toml`. So a backend is measured live once, its per-task `passed/total`
//! counts are recorded, and every later run just replays and merges them deterministically.
//!
//! Pure and immutable: scoring borrows the suite and the fixture and returns a fresh
//! [`BenchmarkResult`]; nothing is mutated in place.

use std::collections::BTreeMap;

use anyhow::{Result, anyhow, bail};
use serde::{Deserialize, Serialize};

use crate::profile::ProfileSource;

use super::suite::GoldTask;
use crate::profile::category::TaskCategory;
use crate::profile::model::{BenchmarkResult, Score};
use crate::types::BackendId;

/// One gold task's recorded grading outcome — the raw counts a grader produced: how many
/// hidden-test assertions / needles / findings matched out of the total. Score = `passed/total`,
/// matching the per-category formulas in design §2.2.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskOutcome {
    /// Must equal a [`GoldTask::id`](super::suite::GoldTask::id) in the suite.
    pub task_id: String,
    /// Matched assertions / needles / findings.
    pub passed: u32,
    /// Total assertions / needles / findings (must be > 0).
    pub total: u32,
}

/// A recorded bench fixture for one backend: the per-task outcomes from a prior live run. The
/// offline path replays these instead of re-executing the candidate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplayFixture {
    /// Backend these outcomes were measured for.
    pub backend: BackendId,
    /// Per-task recorded outcomes.
    pub outcomes: Vec<TaskOutcome>,
    /// Optional ISO-8601 measurement time, carried through onto the merged profile.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub measured_at: Option<String>,
}

/// Fold a fixture's per-task outcomes into a [`BenchmarkResult`].
///
/// Each task contributes `passed/total` ∈ [0,1]; tasks of the same category are averaged and
/// scaled to a 0–100 score whose confidence rises with the sample count. Validates that every
/// `task_id` names a suite task and that `0 < passed <= total`. Pure: borrows `tasks` and
/// `fixture`, returns a brand-new result.
pub fn score_fixture(tasks: &[GoldTask], fixture: &ReplayFixture) -> Result<BenchmarkResult> {
    // Validate + reduce every outcome to (category, fraction) first, so a bad fixture errors
    // before any aggregation runs.
    let graded: Vec<(TaskCategory, f64)> = fixture
        .outcomes
        .iter()
        .map(|outcome| resolve_outcome(tasks, outcome))
        .collect::<Result<_>>()?;

    // Sum fractions per category alongside a sample count.
    let mut sums: BTreeMap<TaskCategory, (f64, u16)> = BTreeMap::new();
    for (category, fraction) in graded {
        let entry = sums.entry(category).or_insert((0.0, 0));
        entry.0 += fraction;
        entry.1 = entry.1.saturating_add(1);
    }

    let scores = sums
        .into_iter()
        .map(|(category, (sum, samples))| (category, aggregate_score(sum, samples)))
        .collect();

    Ok(BenchmarkResult {
        scores,
        measured_at: fixture.measured_at.clone(),
        // A graded-counts fixture records no model/effort: it is `passed/total` numbers only,
        // with nothing left to say which model produced them.
        measured_model: None,
        measured_effort: None,
    })
}

/// Validate one outcome and reduce it to `(category, passed/total)`. Errors on an unknown
/// task id, `total == 0`, or `passed > total`.
fn resolve_outcome(tasks: &[GoldTask], outcome: &TaskOutcome) -> Result<(TaskCategory, f64)> {
    let task = tasks
        .iter()
        .find(|t| t.id == outcome.task_id)
        .ok_or_else(|| anyhow!("fixture references unknown task id: {}", outcome.task_id))?;
    if outcome.total == 0 {
        bail!("task {} has total = 0 (cannot score)", outcome.task_id);
    }
    if outcome.passed > outcome.total {
        bail!(
            "task {} has passed ({}) > total ({})",
            outcome.task_id,
            outcome.passed,
            outcome.total
        );
    }
    Ok((
        task.category,
        f64::from(outcome.passed) / f64::from(outcome.total),
    ))
}

/// Turn an averaged pass-fraction into a [`Score`]: value = `round(mean*100)` clamped to
/// `0..=100`, `samples` tasks behind it, confidence climbing with the sample count (0.55 → 0.95).
fn aggregate_score(sum: f64, samples: u16) -> Score {
    let mean = if samples == 0 {
        0.0
    } else {
        sum / f64::from(samples)
    };
    let value = (mean * 100.0).round().clamp(0.0, 100.0) as u8;
    let confidence = (0.5 + 0.05 * f32::from(samples)).min(0.95);
    Score {
        value,
        samples,
        confidence,
        source: ProfileSource::Benchmarked,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile::bench::all_tasks;

    fn outcome(task_id: &str, passed: u32, total: u32) -> TaskOutcome {
        TaskOutcome {
            task_id: task_id.to_string(),
            passed,
            total,
        }
    }

    #[test]
    fn scores_perfect_outcome_as_full_marks() {
        let tasks = all_tasks();
        let fixture = ReplayFixture {
            backend: BackendId::Codex,
            outcomes: vec![outcome("coding/parse_duration", 4, 4)],
            measured_at: Some("2026-06-30T00:00:00Z".into()),
        };
        let result = score_fixture(&tasks, &fixture).unwrap();
        let coding = result.scores.get(&TaskCategory::Coding).unwrap();
        assert_eq!(coding.value, 100);
        assert_eq!(coding.samples, 1);
        assert_eq!(result.measured_at.as_deref(), Some("2026-06-30T00:00:00Z"));
    }

    #[test]
    fn averages_outcomes_within_a_category() {
        let tasks = all_tasks();
        // 100% and 50% in the same category → 75.
        let fixture = ReplayFixture {
            backend: BackendId::Claude,
            outcomes: vec![
                outcome("coding/parse_duration", 4, 4),
                outcome("coding/top_k_frequent", 1, 2),
            ],
            measured_at: None,
        };
        let result = score_fixture(&tasks, &fixture).unwrap();
        let coding = result.scores.get(&TaskCategory::Coding).unwrap();
        assert_eq!(coding.value, 75);
        assert_eq!(coding.samples, 2);
    }

    #[test]
    fn fixture_round_trips_through_json() {
        let fixture = ReplayFixture {
            backend: BackendId::Codex,
            outcomes: vec![outcome("review/api_handler_bug", 1, 1)],
            measured_at: Some("2026-06-30T00:00:00Z".into()),
        };
        let json = serde_json::to_string(&fixture).unwrap();
        let back: ReplayFixture = serde_json::from_str(&json).unwrap();
        assert_eq!(back, fixture);
    }

    #[test]
    fn unknown_task_id_is_an_error() {
        let tasks = all_tasks();
        let fixture = ReplayFixture {
            backend: BackendId::Codex,
            outcomes: vec![outcome("coding/does_not_exist", 1, 1)],
            measured_at: None,
        };
        let err = score_fixture(&tasks, &fixture).unwrap_err();
        assert!(
            format!("{err:#}").contains("unknown task id"),
            "got: {err:#}"
        );
    }

    #[test]
    fn passed_over_total_is_an_error() {
        let tasks = all_tasks();
        let fixture = ReplayFixture {
            backend: BackendId::Codex,
            outcomes: vec![outcome("coding/parse_duration", 5, 4)],
            measured_at: None,
        };
        assert!(score_fixture(&tasks, &fixture).is_err());
    }

    #[test]
    fn zero_total_is_an_error() {
        let tasks = all_tasks();
        let fixture = ReplayFixture {
            backend: BackendId::Codex,
            outcomes: vec![outcome("coding/parse_duration", 0, 0)],
            measured_at: None,
        };
        assert!(score_fixture(&tasks, &fixture).is_err());
    }
}
