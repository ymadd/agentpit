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

use serde::{Deserialize, Serialize};

use crate::effort::Effort;
use crate::events::{Event, LegStatus, OutcomeLabel, RunKind};
use crate::profile::model::Score;
use crate::profile::{ProfileKey, ProfileSource, TaskCategory};
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

/// Which of the four evidence sources produced a label. Carried on the label rather than
/// inferred back from its weight, so anything reporting the *quality* of the evidence
/// (`agentpit learning`) reads the fact instead of reverse-engineering a float.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LabelSource {
    /// The human's explicit verdict (`OutcomeNoted`).
    Outcome,
    /// An aggregator grade (`MemberGraded`).
    Grade,
    /// The task was re-dispatched elsewhere shortly after, failing this attempt.
    Rerun,
    /// Exit status alone, discounted.
    Exit,
}

impl LabelSource {
    /// This source's fold weight. The single place the constants are read.
    pub fn weight(&self) -> f32 {
        match self {
            LabelSource::Outcome => WEIGHT_OUTCOME,
            LabelSource::Grade => WEIGHT_GRADE,
            LabelSource::Rerun => WEIGHT_RERUN,
            LabelSource::Exit => WEIGHT_EXIT,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            LabelSource::Outcome => "outcome",
            LabelSource::Grade => "grade",
            LabelSource::Rerun => "rerun",
            LabelSource::Exit => "exit",
        }
    }
}

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
    /// The model / effort `RouteDecided` recorded for this run, which is what its labels are
    /// ABOUT. For a fan-out run these are the run-wide `--model` / `--effort` (they applied to
    /// every member); both `None` means the run left each backend on its configured default and
    /// the log does not say which — so the labels belong to the UNPINNED row, whose meaning is
    /// exactly "this backend on unspecified settings".
    pub model: Option<String>,
    pub effort: Option<Effort>,
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

/// One weighted outcome observation for a `(backend, model, effort, category)` cell.
/// `task_hash`/`ts` carry the source run's identity through to the similarity sample store
/// (routes.jsonl).
#[derive(Debug, Clone, PartialEq)]
pub struct Label {
    pub backend: BackendId,
    /// The variant this observation is about — see [`RunRecord::model`]. Folded into the
    /// matching `profiles.toml` row so a `high`-effort run's evidence never lands on the `low`
    /// row's score.
    pub model: Option<String>,
    pub effort: Option<Effort>,
    pub category: TaskCategory,
    pub success: bool,
    pub source: LabelSource,
    pub task_hash: Option<String>,
    pub ts: u64,
}

impl Label {
    /// The fold weight this label contributes, derived from its source.
    pub fn weight(&self) -> f32 {
        self.source.weight()
    }
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
                model,
                effort,
                ..
            } => {
                let r = record(&mut order, &mut runs, &run_id);
                r.backend = Some(backend);
                r.category = category.and_then(|c| c.parse().ok());
                r.task_hash = Some(task_hash);
                r.route_ts = ts;
                r.model = model;
                r.effort = effort.and_then(|e| e.parse().ok());
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
                model: run.model.clone(),
                effort: run.effort,
                category,
                success: outcome == OutcomeLabel::Good,
                source: LabelSource::Outcome,
                task_hash: run.task_hash.clone(),
                ts: run.route_ts,
            });
            continue;
        }

        // Source 2: aggregator grades — one label per decisively-graded member. The emitter
        // (`parse_member_grades`) already validates range/duplicates, but the log is plain
        // text on disk, so re-verify here: out-of-range grades drop, duplicates keep the
        // first entry (defense in depth against hand-edited or corrupted lines).
        if !run.grades.is_empty() {
            let mut graded_backends: std::collections::HashSet<BackendId> = Default::default();
            for (backend, grade) in &run.grades {
                if *grade > 100 || !graded_backends.insert(*backend) {
                    continue;
                }
                let success = match *grade {
                    g if g >= GRADE_PASS => true,
                    g if g < GRADE_FAIL => false,
                    _ => continue, // middling grade: no signal
                };
                labels.push(Label {
                    backend: *backend,
                    model: run.model.clone(),
                    effort: run.effort,
                    category,
                    success,
                    source: LabelSource::Grade,
                    task_hash: run.task_hash.clone(),
                    ts: run.route_ts,
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
                model: run.model.clone(),
                effort: run.effort,
                category,
                success: false,
                source: LabelSource::Rerun,
                task_hash: run.task_hash.clone(),
                ts: run.route_ts,
            });
            continue;
        }

        // Source 4: exit status, discounted (exit ok ≠ quality ok).
        match run.finished {
            Some(LegStatus::Ok) => labels.push(Label {
                backend,
                model: run.model.clone(),
                effort: run.effort,
                category,
                success: true,
                source: LabelSource::Exit,
                task_hash: run.task_hash.clone(),
                ts: run.route_ts,
            }),
            Some(LegStatus::Error) => labels.push(Label {
                backend,
                model: run.model.clone(),
                effort: run.effort,
                category,
                success: false,
                source: LabelSource::Exit,
                task_hash: run.task_hash.clone(),
                ts: run.route_ts,
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
) -> BTreeMap<ProfileKey, BTreeMap<TaskCategory, Score>> {
    #[derive(Default)]
    struct Cell {
        success_weight: f32,
        failure_weight: f32,
        samples: u16,
    }
    // Bucketed by the VARIANT, not the backend: evidence from a `high`-effort run and a
    // `low`-effort run of the same backend are observations of two different things, and
    // averaging them together is the mistake this keying exists to prevent.
    let mut cells: BTreeMap<(ProfileKey, TaskCategory), Cell> = BTreeMap::new();
    for label in labels {
        let key = ProfileKey::new(label.backend, label.model.clone(), label.effort);
        let cell = cells.entry((key, label.category)).or_default();
        if label.success {
            cell.success_weight += label.weight();
        } else {
            cell.failure_weight += label.weight();
        }
        cell.samples = cell.samples.saturating_add(1);
    }

    let mut scores: BTreeMap<ProfileKey, BTreeMap<TaskCategory, Score>> = BTreeMap::new();
    for ((key, category), cell) in cells {
        if cell.samples < min_samples {
            continue;
        }
        let alpha = cell.success_weight + 1.0;
        let beta = cell.failure_weight + 1.0;
        let value = (100.0 * alpha / (alpha + beta)).round() as u8;
        let confidence =
            (CONFIDENCE_BASE + CONFIDENCE_PER_SAMPLE * f32::from(cell.samples)).min(CONFIDENCE_CAP);
        scores.entry(key).or_default().insert(
            category,
            Score {
                value,
                samples: cell.samples,
                confidence,
                source: ProfileSource::Learned,
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
            started("r-2", "review", &["claude", "codex", "opencode"]),
            route("r-2", "claude", Some("review"), "bb", 6),
            r#"{"event":"member_graded","ts":11,"run_id":"r-2","backend":"claude","grade":90}"#
                .into(),
            r#"{"event":"member_graded","ts":11,"run_id":"r-2","backend":"codex","grade":30}"#
                .into(),
            r#"{"event":"member_graded","ts":11,"run_id":"r-2","backend":"opencode","grade":55}"#
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
                    model: None,
                    effort: None,
                    category: TaskCategory::Coding,
                    success: false,
                    source: LabelSource::Outcome,
                    task_hash: Some("aa".into()),
                    ts: 5,
                },
                Label {
                    backend: BackendId::Claude,
                    model: None,
                    effort: None,
                    category: TaskCategory::Review,
                    success: true,
                    source: LabelSource::Grade,
                    task_hash: Some("bb".into()),
                    ts: 6,
                },
                Label {
                    backend: BackendId::Codex,
                    model: None,
                    effort: None,
                    category: TaskCategory::Review,
                    success: false,
                    source: LabelSource::Grade,
                    task_hash: Some("bb".into()),
                    ts: 6,
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
                    model: None,
                    effort: None,
                    category: TaskCategory::Coding,
                    success: false,
                    source: LabelSource::Rerun,
                    task_hash: Some("aa".into()),
                    ts: 1_000,
                },
                Label {
                    backend: BackendId::Codex,
                    model: None,
                    effort: None,
                    category: TaskCategory::Coding,
                    success: true,
                    source: LabelSource::Exit,
                    task_hash: Some("aa".into()),
                    ts: 2_000,
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
                    model: None,
                    effort: None,
                    category: TaskCategory::Coding,
                    success: true,
                    source: LabelSource::Outcome,
                    task_hash: None,
                    ts: 0,
                },
                n,
            )
        };
        let mut labels: Vec<Label> = win(BackendId::Codex, 10).collect();
        labels.push(Label {
            backend: BackendId::Codex,
            model: None,
            effort: None,
            category: TaskCategory::Coding,
            success: false,
            source: LabelSource::Outcome,
            task_hash: None,
            ts: 0,
        });
        // Claude has decent labels but too few of them.
        labels.extend(win(BackendId::Claude, 3));

        let scores = fold_scores(&labels, DEFAULT_MIN_SAMPLES);
        assert!(
            !scores.contains_key(&ProfileKey::unpinned(BackendId::Claude)),
            "under min_samples must not produce a cell"
        );
        let codex = &scores[&ProfileKey::unpinned(BackendId::Codex)][&TaskCategory::Coding];
        // α = 31, β = 4 → 100·31/35 ≈ 89.
        assert_eq!(codex.value, 89);
        assert_eq!(codex.samples, 11);
        assert_eq!(codex.confidence, CONFIDENCE_CAP);
    }
}
