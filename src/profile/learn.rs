//! Learn fold: turn runtime telemetry (`events.jsonl`) into `ProfileSource::Learned` scores.
//!
//! The fold reads the event log, derives at most one outcome label per run (per-member for
//! graded ensemble runs), aggregates weighted success/failure counts per
//! `(BackendId, TaskCategory)` cell as a Beta posterior, and returns the cells with enough
//! samples to trust. `agentpit profile learn` merges them into `profiles.toml` under the
//! standing `benchmarked > learned > seeded` gate, so a gold-bench measurement is never
//! overwritten by noisy telemetry.
//!
//! Label sources, strongest first — the first available source decides a run's label:
//! 1. `OutcomeNoted` — the human's explicit verdict (single-backend runs).
//! 2. `MemberGraded` — the aggregator's structured grades (per member; ≥70 pass, <40 fail).
//! 3. Re-dispatch — the same task re-run on a different backend shortly after counts the
//!    earlier attempt as a failure.
//! 4. `RunFinished` — exit status, discounted (exit ok ≠ quality ok).

use std::collections::BTreeMap;

use crate::events::{Event, LegStatus, OutcomeLabel, RunKind};
use crate::profile::TaskCategory;
use crate::profile::model::Score;
use crate::types::BackendId;

/// Cells with fewer labels than this are not written back (too little evidence).
pub const DEFAULT_MIN_SAMPLES: u16 = 5;
/// A same-task re-dispatch on a different backend within this window fails the earlier run.
pub const DEFAULT_RERUN_WINDOW_MS: u64 = 6 * 60 * 60 * 1000;

/// Learned confidence: `0.3 + 0.05·samples`, capped below Benchmarked territory.
const CONFIDENCE_BASE: f32 = 0.3;
const CONFIDENCE_PER_SAMPLE: f32 = 0.05;
const CONFIDENCE_CAP: f32 = 0.85;

/// Label weights by source (design: outcome strongest, exit status weakest).
const WEIGHT_OUTCOME: f32 = 3.0;
const WEIGHT_GRADE: f32 = 2.0;
const WEIGHT_RERUN: f32 = 1.0;
const WEIGHT_EXIT: f32 = 0.5;

/// Aggregator grade thresholds: ≥ pass succeeds, < fail fails, in between is ignored.
const GRADE_PASS: u8 = 70;
const GRADE_FAIL: u8 = 40;

/// Everything the fold needs about one run, assembled from its event lines.
#[derive(Debug, Clone, Default)]
pub struct RunRecord {
    pub run_id: String,
    /// The routed backend from `RouteDecided`. `None` = pre-telemetry run; no labels.
    pub backend: Option<BackendId>,
    /// Category from `RouteDecided`, else resolved later from the saved task text.
    pub category: Option<TaskCategory>,
    pub task_hash: Option<String>,
    /// `RouteDecided` timestamp; orders same-task runs for re-dispatch detection.
    pub route_ts: u64,
    pub members: Vec<BackendId>,
    pub kind: Option<RunKind>,
    pub finished: Option<LegStatus>,
    pub grades: Vec<(BackendId, u8)>,
    pub outcome: Option<OutcomeLabel>,
}

impl RunRecord {
    /// A run the router (or a fan-out path) dispatched to exactly one backend — the only
    /// shape whose run-level outcome is attributable to a single backend.
    fn single_backend(&self) -> Option<BackendId> {
        (self.members.len() <= 1).then_some(self.backend).flatten()
    }
}

/// One weighted outcome observation for a `(backend, category)` cell.
#[derive(Debug, Clone, PartialEq)]
pub struct Label {
    pub backend: BackendId,
    pub category: TaskCategory,
    pub success: bool,
    pub weight: f32,
}

/// Group the log's event lines into per-run records, in first-seen order. Unparseable lines
/// are skipped (the log is best-effort by design); gold-bench runs are excluded — they feed
/// the Benchmarked source, not this one.
pub fn parse_runs(log: &str) -> Vec<RunRecord> {
    fn record<'a>(
        order: &mut Vec<String>,
        runs: &'a mut BTreeMap<String, RunRecord>,
        run_id: &str,
    ) -> &'a mut RunRecord {
        if !runs.contains_key(run_id) {
            order.push(run_id.to_string());
            runs.insert(
                run_id.to_string(),
                RunRecord {
                    run_id: run_id.to_string(),
                    ..RunRecord::default()
                },
            );
        }
        runs.get_mut(run_id).expect("just inserted")
    }
    let mut order: Vec<String> = Vec::new();
    let mut runs: BTreeMap<String, RunRecord> = BTreeMap::new();

    for line in log.lines() {
        let Ok(event) = serde_json::from_str::<Event>(line) else {
            continue;
        };
        match event {
            Event::RunStarted {
                run_id,
                kind,
                members,
                ..
            } => {
                let r = record(&mut order, &mut runs, &run_id);
                r.kind = Some(kind);
                r.members = members;
            }
            Event::RouteDecided {
                run_id,
                backend,
                category,
                task_hash,
                ts,
                ..
            } => {
                let r = record(&mut order, &mut runs, &run_id);
                r.backend = Some(backend);
                r.category = category.and_then(|c| c.parse().ok());
                r.task_hash = Some(task_hash);
                r.route_ts = ts;
            }
            Event::MemberGraded {
                run_id,
                backend,
                grade,
                ..
            } => record(&mut order, &mut runs, &run_id)
                .grades
                .push((backend, grade)),
            Event::OutcomeNoted {
                run_id, outcome, ..
            } => record(&mut order, &mut runs, &run_id).outcome = Some(outcome),
            Event::RunFinished { run_id, status, .. } => {
                record(&mut order, &mut runs, &run_id).finished = Some(status)
            }
            _ => {}
        }
    }

    order
        .into_iter()
        .filter_map(|id| runs.remove(&id))
        .filter(|r| r.kind != Some(RunKind::Bench))
        .collect()
}

/// Fill in missing categories via `resolve` (production: read `tasks/<hash>.txt` and re-run
/// the diagnose heuristic). Runs whose category cannot be resolved yield no labels later.
pub fn resolve_categories(runs: &mut [RunRecord], resolve: impl Fn(&str) -> Option<TaskCategory>) {
    for run in runs.iter_mut() {
        if run.category.is_none()
            && let Some(hash) = run.task_hash.as_deref()
        {
            run.category = resolve(hash);
        }
    }
}

/// Derive at most one label per run (per graded member for ensemble runs), strongest
/// available source first.
pub fn derive_labels(runs: &[RunRecord], rerun_window_ms: u64) -> Vec<Label> {
    // Re-dispatch detection: order each task's single-backend runs by route time; a quick
    // follow-up on a DIFFERENT backend marks the earlier run superseded (= failed).
    let mut by_task: BTreeMap<&str, Vec<&RunRecord>> = BTreeMap::new();
    for run in runs {
        if let (Some(hash), Some(_)) = (run.task_hash.as_deref(), run.single_backend()) {
            by_task.entry(hash).or_default().push(run);
        }
    }
    let mut superseded: Vec<&str> = Vec::new();
    for same_task in by_task.values_mut() {
        same_task.sort_by_key(|r| r.route_ts);
        for pair in same_task.windows(2) {
            let (earlier, later) = (pair[0], pair[1]);
            if later.route_ts.saturating_sub(earlier.route_ts) <= rerun_window_ms
                && earlier.backend != later.backend
            {
                superseded.push(earlier.run_id.as_str());
            }
        }
    }

    let mut labels = Vec::new();
    for run in runs {
        let Some(category) = run.category else {
            continue;
        };

        // Source 1: the human's explicit verdict — single-backend runs only, since a verdict
        // on a fan-out run doesn't say which member earned it.
        if let (Some(outcome), Some(backend)) = (run.outcome, run.single_backend()) {
            labels.push(Label {
                backend,
                category,
                success: outcome == OutcomeLabel::Good,
                weight: WEIGHT_OUTCOME,
            });
            continue;
        }

        // Source 2: aggregator grades — one label per decisively-graded member.
        if !run.grades.is_empty() {
            for (backend, grade) in &run.grades {
                let success = match *grade {
                    g if g >= GRADE_PASS => true,
                    g if g < GRADE_FAIL => false,
                    _ => continue, // middling grade: no signal
                };
                labels.push(Label {
                    backend: *backend,
                    category,
                    success,
                    weight: WEIGHT_GRADE,
                });
            }
            continue;
        }

        let Some(backend) = run.single_backend() else {
            continue; // ungraded fan-out run: nothing attributable
        };

        // Source 3: the task was quickly re-dispatched elsewhere — count this attempt failed.
        if superseded.contains(&run.run_id.as_str()) {
            labels.push(Label {
                backend,
                category,
                success: false,
                weight: WEIGHT_RERUN,
            });
            continue;
        }

        // Source 4: exit status, discounted (exit ok ≠ quality ok).
        match run.finished {
            Some(LegStatus::Ok) => labels.push(Label {
                backend,
                category,
                success: true,
                weight: WEIGHT_EXIT,
            }),
            Some(LegStatus::Error) => labels.push(Label {
                backend,
                category,
                success: false,
                weight: WEIGHT_EXIT,
            }),
            Some(LegStatus::Skipped) | None => {}
        }
    }
    labels
}

/// Aggregate labels into per-cell Learned scores: a Beta(1,1)-prior posterior mean over the
/// weighted counts, sample count = number of labels, confidence growing with samples but
/// capped below Benchmarked territory. Cells under `min_samples` are dropped.
pub fn fold_scores(
    labels: &[Label],
    min_samples: u16,
) -> BTreeMap<BackendId, BTreeMap<TaskCategory, Score>> {
    #[derive(Default)]
    struct Cell {
        success_weight: f32,
        failure_weight: f32,
        samples: u16,
    }
    let mut cells: BTreeMap<(BackendId, TaskCategory), Cell> = BTreeMap::new();
    for label in labels {
        let cell = cells.entry((label.backend, label.category)).or_default();
        if label.success {
            cell.success_weight += label.weight;
        } else {
            cell.failure_weight += label.weight;
        }
        cell.samples = cell.samples.saturating_add(1);
    }

    let mut scores: BTreeMap<BackendId, BTreeMap<TaskCategory, Score>> = BTreeMap::new();
    for ((backend, category), cell) in cells {
        if cell.samples < min_samples {
            continue;
        }
        let alpha = cell.success_weight + 1.0;
        let beta = cell.failure_weight + 1.0;
        let value = (100.0 * alpha / (alpha + beta)).round() as u8;
        let confidence =
            (CONFIDENCE_BASE + CONFIDENCE_PER_SAMPLE * f32::from(cell.samples)).min(CONFIDENCE_CAP);
        scores.entry(backend).or_default().insert(
            category,
            Score {
                value,
                samples: cell.samples,
                confidence,
            },
        );
    }
    scores
}

#[cfg(test)]
mod tests {
    use super::*;

    fn route(run: &str, backend: &str, category: Option<&str>, hash: &str, ts: u64) -> String {
        let category = category
            .map(|c| format!(",\"category\":\"{c}\""))
            .unwrap_or_default();
        format!(
            r#"{{"event":"route_decided","ts":{ts},"run_id":"{run}","backend":"{backend}","reason":"profile"{category},"task_hash":"{hash}"}}"#
        )
    }
    fn started(run: &str, kind: &str, members: &[&str]) -> String {
        let members = members
            .iter()
            .map(|m| format!("\"{m}\""))
            .collect::<Vec<_>>()
            .join(",");
        format!(
            r#"{{"event":"run_started","ts":1,"run_id":"{run}","pid":1,"kind":"{kind}","members":[{members}],"cwd":"/x"}}"#
        )
    }
    fn finished(run: &str, status: &str) -> String {
        format!(r#"{{"event":"run_finished","ts":9,"run_id":"{run}","status":"{status}"}}"#)
    }

    #[test]
    fn parse_groups_lines_into_runs_and_skips_bench_and_junk() {
        let log = [
            started("r-1", "rescue", &["claude"]),
            route("r-1", "claude", Some("coding"), "aa", 5),
            "junk not json".into(),
            finished("r-1", "ok"),
            started("r-2", "bench", &["codex"]),
            finished("r-2", "ok"),
        ]
        .join("\n");
        let runs = parse_runs(&log);
        assert_eq!(runs.len(), 1, "bench runs are excluded");
        let r = &runs[0];
        assert_eq!(r.backend, Some(BackendId::Claude));
        assert_eq!(r.category, Some(TaskCategory::Coding));
        assert_eq!(r.task_hash.as_deref(), Some("aa"));
        assert_eq!(r.finished, Some(LegStatus::Ok));
    }

    #[test]
    fn outcome_beats_exit_status_and_grades_are_per_member() {
        let log = [
            // r-1: exit ok but the human said bad — the human wins.
            started("r-1", "rescue", &["claude"]),
            route("r-1", "claude", Some("coding"), "aa", 5),
            finished("r-1", "ok"),
            r#"{"event":"outcome_noted","ts":10,"run_id":"r-1","outcome":"bad"}"#.into(),
            // r-2: graded ensemble — decisive grades label members; the middling one is silent.
            started("r-2", "review", &["claude", "codex", "gemini"]),
            route("r-2", "claude", Some("review"), "bb", 6),
            r#"{"event":"member_graded","ts":11,"run_id":"r-2","backend":"claude","grade":90}"#
                .into(),
            r#"{"event":"member_graded","ts":11,"run_id":"r-2","backend":"codex","grade":30}"#
                .into(),
            r#"{"event":"member_graded","ts":11,"run_id":"r-2","backend":"gemini","grade":55}"#
                .into(),
            finished("r-2", "ok"),
        ]
        .join("\n");
        let labels = derive_labels(&parse_runs(&log), DEFAULT_RERUN_WINDOW_MS);
        assert_eq!(
            labels,
            vec![
                Label {
                    backend: BackendId::Claude,
                    category: TaskCategory::Coding,
                    success: false,
                    weight: WEIGHT_OUTCOME,
                },
                Label {
                    backend: BackendId::Claude,
                    category: TaskCategory::Review,
                    success: true,
                    weight: WEIGHT_GRADE,
                },
                Label {
                    backend: BackendId::Codex,
                    category: TaskCategory::Review,
                    success: false,
                    weight: WEIGHT_GRADE,
                },
            ]
        );
    }

    #[test]
    fn quick_rerun_on_another_backend_fails_the_earlier_attempt() {
        let log = [
            started("r-1", "rescue", &["claude"]),
            route("r-1", "claude", Some("coding"), "aa", 1_000),
            finished("r-1", "ok"),
            started("r-2", "rescue", &["codex"]),
            route("r-2", "codex", Some("coding"), "aa", 2_000),
            finished("r-2", "ok"),
        ]
        .join("\n");
        let labels = derive_labels(&parse_runs(&log), DEFAULT_RERUN_WINDOW_MS);
        // r-1 was superseded (failure at rerun weight); r-2 keeps its exit-ok label.
        assert_eq!(
            labels,
            vec![
                Label {
                    backend: BackendId::Claude,
                    category: TaskCategory::Coding,
                    success: false,
                    weight: WEIGHT_RERUN,
                },
                Label {
                    backend: BackendId::Codex,
                    category: TaskCategory::Coding,
                    success: true,
                    weight: WEIGHT_EXIT,
                },
            ]
        );

        // Outside the window the earlier run keeps its own exit label instead.
        let labels = derive_labels(&parse_runs(&log), 500);
        assert!(labels.iter().all(|l| l.success));
    }

    #[test]
    fn unresolved_category_yields_no_labels_and_resolver_fills_gaps() {
        let log = [
            started("r-1", "rescue", &["claude"]),
            route("r-1", "claude", None, "aa", 5),
            finished("r-1", "ok"),
        ]
        .join("\n");
        let mut runs = parse_runs(&log);
        assert!(derive_labels(&runs, DEFAULT_RERUN_WINDOW_MS).is_empty());

        resolve_categories(&mut runs, |hash| {
            assert_eq!(hash, "aa");
            Some(TaskCategory::Docs)
        });
        let labels = derive_labels(&runs, DEFAULT_RERUN_WINDOW_MS);
        assert_eq!(labels.len(), 1);
        assert_eq!(labels[0].category, TaskCategory::Docs);
    }

    #[test]
    fn fold_scores_applies_beta_posterior_and_min_samples() {
        let win = |backend, n: usize| {
            std::iter::repeat_n(
                Label {
                    backend,
                    category: TaskCategory::Coding,
                    success: true,
                    weight: WEIGHT_OUTCOME,
                },
                n,
            )
        };
        let mut labels: Vec<Label> = win(BackendId::Codex, 10).collect();
        labels.push(Label {
            backend: BackendId::Codex,
            category: TaskCategory::Coding,
            success: false,
            weight: WEIGHT_OUTCOME,
        });
        // Claude has decent labels but too few of them.
        labels.extend(win(BackendId::Claude, 3));

        let scores = fold_scores(&labels, DEFAULT_MIN_SAMPLES);
        assert!(
            !scores.contains_key(&BackendId::Claude),
            "under min_samples must not produce a cell"
        );
        let codex = &scores[&BackendId::Codex][&TaskCategory::Coding];
        // α = 31, β = 4 → 100·31/35 ≈ 89.
        assert_eq!(codex.value, 89);
        assert_eq!(codex.samples, 11);
        assert_eq!(codex.confidence, CONFIDENCE_CAP);
    }
}
