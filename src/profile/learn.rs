//! Learn fold: turn runtime telemetry (`events.jsonl`) into `ProfileSource::Learned` scores.
//!
//! The fold reads the event log, derives at most one outcome label per run (per-member for
//! graded ensemble runs), aggregates weighted success/failure counts per
//! `(backend, model, effort, category)` cell as a Beta posterior, and returns the cells with enough
//! samples to trust. `agentpit profile learn` merges them into `profiles.toml` under the
//! standing `benchmarked > learned > seeded` gate, so a gold-bench measurement is never
//! overwritten by noisy telemetry.
//!
//! Label sources, strongest first — the first available source decides a run's label:
//! 1. `OutcomeNoted` — the human's explicit verdict (single-backend runs).
//! 2. `MemberGraded` — read *within the run*: the best-graded member wins and the worst loses,
//!    provided the run separated them at all. A lone graded member has nobody to compare
//!    against and falls back to the absolute reading (≥70 pass, <40 fail).
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
/// A within-run comparison is *cleaner* evidence than an absolute grade — both members saw the
/// same task and the same judge, so the judge's scale bias and the task's difficulty cancel —
/// yet it is still one judge's opinion about one task, never a statement that the winning work
/// was good, so it stays well below the human's own verdict. It is deliberately not raised
/// above [`WEIGHT_GRADE`] either: relative reading already emits ~2 labels where the absolute
/// rule emitted ~1, so the channel's total mass rose on its own, and paying more per label on
/// top of that would let a fan-out-heavy week drown out the `OutcomeNoted` verdicts.
const WEIGHT_RELATIVE: f32 = 2.0;
const WEIGHT_RERUN: f32 = 1.0;
const WEIGHT_EXIT: f32 = 0.5;

/// Which evidence source produced a label. Carried on the label rather than
/// inferred back from its weight, so anything reporting the *quality* of the evidence
/// (`agentpit learning`) reads the fact instead of reverse-engineering a float.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LabelSource {
    /// The human's explicit verdict (`OutcomeNoted`).
    Outcome,
    /// A within-run comparison of `MemberGraded` grades: this member came out top (or bottom)
    /// of a run that actually separated its members.
    Relative,
    /// An absolute aggregator grade (`MemberGraded`), used only when the run graded exactly one
    /// member, so there was nothing to compare it against.
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
            LabelSource::Relative => WEIGHT_RELATIVE,
            LabelSource::Grade => WEIGHT_GRADE,
            LabelSource::Rerun => WEIGHT_RERUN,
            LabelSource::Exit => WEIGHT_EXIT,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            LabelSource::Outcome => "outcome",
            LabelSource::Relative => "relative",
            LabelSource::Grade => "grade",
            LabelSource::Rerun => "rerun",
            LabelSource::Exit => "exit",
        }
    }
}

/// Absolute grade thresholds, the fallback for a run that graded exactly one member:
/// ≥ pass succeeds, < fail fails, in between is ignored.
const GRADE_PASS: u8 = 70;
const GRADE_FAIL: u8 = 40;

/// How far apart the best and worst grade of a run must be before the run counts as having
/// separated its members. Judges score coarsely and inconsistently at the margin, so a few
/// points is a rounding difference, not a finding: under this gap the run is flat and says
/// nothing about anybody. This is what keeps "80 vs 78" from manufacturing a loser.
const RELATIVE_MARGIN: u8 = 10;

/// Everything the fold needs about one run, assembled from its event lines.
#[derive(Debug, Clone, Default)]
pub struct RunRecord {
    pub run_id: String,
    /// The routed backend from `RouteDecided`. `None` = pre-telemetry run; no labels.
    pub backend: Option<BackendId>,
    /// Category from `RouteDecided`, else resolved later from the saved task text.
    pub category: Option<TaskCategory>,
    /// The run-level model / effort from `RouteDecided`. Single-backend labels use this variant;
    /// old fan-out logs also fall back to it when their `MemberStarted` lines lack variant data.
    pub model: Option<String>,
    pub effort: Option<Effort>,
    pub task_hash: Option<String>,
    /// `RouteDecided` timestamp; orders same-task runs for re-dispatch detection.
    pub route_ts: u64,
    pub members: Vec<BackendId>,
    pub kind: Option<RunKind>,
    pub finished: Option<LegStatus>,
    /// Resolved variants for non-aggregator fan-out members, keyed by backend.
    member_variants: BTreeMap<BackendId, (Option<String>, Option<Effort>)>,
    pub grades: Vec<(BackendId, u8)>,
    pub outcome: Option<OutcomeLabel>,
    /// The backend that ran as aggregator (`MemberStarted{aggregator:true}`), when the log says
    /// so. `None` covers both "no aggregator in this run" and "old log, the line predates the
    /// field" — either way there is nothing to exclude, so grading falls back to trusting every
    /// grade line as before.
    pub aggregator: Option<BackendId>,
}

impl RunRecord {
    /// A run the router (or a fan-out path) dispatched to exactly one backend — the only
    /// shape whose run-level outcome is attributable to a single backend.
    fn single_backend(&self) -> Option<BackendId> {
        (self.members.len() <= 1).then_some(self.backend).flatten()
    }

    /// A graded member's own variant, with per-field fallback to the run variant for old logs.
    fn member_variant(&self, backend: BackendId) -> (Option<String>, Option<Effort>) {
        match self.member_variants.get(&backend) {
            Some((model, effort)) => (
                model.clone().or_else(|| self.model.clone()),
                (*effort).or(self.effort),
            ),
            None => (self.model.clone(), self.effort),
        }
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
            Event::MemberStarted {
                run_id,
                backend,
                aggregator,
                model,
                effort,
                ..
            } => {
                let r = record(&mut order, &mut runs, &run_id);
                if aggregator {
                    r.aggregator = Some(backend);
                } else {
                    r.member_variants
                        .insert(backend, (model, effort.and_then(|e| e.parse().ok())));
                }
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

/// Turn one run's `MemberGraded` lines into labels, reading each grade *against the others in
/// the same run* rather than against a fixed bar.
///
/// An absolute threshold cannot work here. The graders are language models (and, in the arena,
/// one person) scoring free-form work: their scales drift between runs, and a hard task drags
/// every member's number down without saying anything about the backends. The old ≥70/<40 rule
/// turned that into a dead band that swallowed the most common real result — the observed
/// `82 vs 66` ensemble scored a win for the leader and *nothing* for the member that plainly
/// lost, so the loser's row never learned anything and the fold only ever moved upward.
///
/// Within one run those distortions cancel: the members answered the same task and were read by
/// the same judge, so the ordering is the part worth believing. The best-graded member wins, the
/// worst loses, and anyone in between is left alone — being neither best nor worst is not a
/// finding. Two guards keep the rule from inventing evidence:
///
/// * a run whose grades span less than [`RELATIVE_MARGIN`] did not separate anybody, so it
///   yields nothing (this is the `80 vs 78` case);
/// * a run that graded a single member has no comparison at all, so it falls back to the
///   absolute reading — the only reading available — with its conservative dead band intact.
///
/// The emitter (`parse_member_grades`) already validates range and duplicates, but the log is
/// plain text on disk, so re-verify here: out-of-range grades drop and duplicates keep the first
/// entry (defense in depth against hand-edited or corrupted lines).
///
/// One more exclusion, unrelated to range/duplicates: every `MemberGraded` line in a run was
/// written by that run's aggregator, so a grade whose *subject* is the aggregator itself is not
/// a judgment at all — it is the aggregator grading its own work, with every incentive to score
/// itself well and no independent judge to catch it. Real telemetry (run `70658`) shows exactly
/// this: `claude` was dispatched as both a fan-out member and the aggregator, and duly graded
/// itself 86 with rank 1. Dropped here, before best/worst is computed, so it can neither win a
/// label for itself nor drag a peer's grade down into a false "loss" by comparison against an
/// inflated self-score. The peer's own grade is untouched and still scores normally — as a
/// lone grade (absolute fallback) if it was the aggregator's only other subject, or relative to
/// other peers otherwise. Old logs without a `MemberStarted{aggregator:true}` line leave
/// `run.aggregator` at `None`, so this filter is a no-op for them — behavior is unchanged.
fn grade_labels(run: &RunRecord, category: TaskCategory) -> Vec<Label> {
    let mut seen: std::collections::HashSet<BackendId> = Default::default();
    let graded: Vec<(BackendId, u8)> = run
        .grades
        .iter()
        .copied()
        .filter(|(backend, grade)| {
            *grade <= 100 && seen.insert(*backend) && Some(*backend) != run.aggregator
        })
        .collect();

    let label = |backend: BackendId, success: bool, source: LabelSource| {
        let (model, effort) = run.member_variant(backend);
        Label {
            backend,
            model,
            effort,
            category,
            success,
            source,
            task_hash: run.task_hash.clone(),
            ts: run.route_ts,
        }
    };

    match graded.as_slice() {
        [] => Vec::new(),
        [(backend, grade)] => match *grade {
            g if g >= GRADE_PASS => vec![label(*backend, true, LabelSource::Grade)],
            g if g < GRADE_FAIL => vec![label(*backend, false, LabelSource::Grade)],
            _ => Vec::new(), // middling lone grade: no signal
        },
        members => {
            let best = members.iter().map(|(_, g)| *g).max().unwrap_or_default();
            let worst = members.iter().map(|(_, g)| *g).min().unwrap_or_default();
            if best.saturating_sub(worst) < RELATIVE_MARGIN {
                return Vec::new(); // flat run: the judge did not separate them
            }
            members
                .iter()
                .filter_map(|(backend, grade)| match *grade {
                    g if g == best => Some(label(*backend, true, LabelSource::Relative)),
                    g if g == worst => Some(label(*backend, false, LabelSource::Relative)),
                    _ => None, // neither best nor worst: no finding
                })
                .collect()
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

        // Source 2: aggregator grades, read relative to the rest of the run (see
        // `grade_labels`). A graded run never falls through to the weaker sources, even when
        // the comparison came out flat — "the judge could not separate them" is an answer.
        if !run.grades.is_empty() {
            labels.extend(grade_labels(run, category));
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
            // r-2: graded ensemble — best and worst are labelled; the one in between is silent.
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
                    source: LabelSource::Relative,
                    task_hash: Some("bb".into()),
                    ts: 6,
                },
                Label {
                    backend: BackendId::Codex,
                    model: None,
                    effort: None,
                    category: TaskCategory::Review,
                    success: false,
                    source: LabelSource::Relative,
                    task_hash: Some("bb".into()),
                    ts: 6,
                },
            ]
        );
    }

    fn graded(run: &str, backend: &str, grade: u8) -> String {
        format!(
            r#"{{"event":"member_graded","ts":11,"run_id":"{run}","backend":"{backend}","grade":{grade}}}"#
        )
    }

    /// The case that motivated relative labelling, taken from the real telemetry: an ensemble
    /// judge scored 82 against 66. The old absolute rule banked a win for the leader and said
    /// nothing at all about the member that plainly lost — every such run pushed the fold
    /// upward and nobody ever accrued a failure. The run separated them, so both are labelled.
    #[test]
    fn a_run_that_separated_its_members_labels_the_winner_and_the_loser() {
        let log = [
            started("r-1", "review", &["claude", "codex"]),
            route("r-1", "claude", Some("review"), "aa", 5),
            graded("r-1", "claude", 82),
            graded("r-1", "codex", 66),
            finished("r-1", "ok"),
        ]
        .join("\n");

        let labels = derive_labels(&parse_runs(&log), DEFAULT_RERUN_WINDOW_MS);
        assert_eq!(labels.len(), 2, "both members are labelled: {labels:?}");
        assert_eq!(
            (labels[0].backend, labels[0].success, labels[0].source),
            (BackendId::Claude, true, LabelSource::Relative),
        );
        assert_eq!(
            (labels[1].backend, labels[1].success, labels[1].source),
            (BackendId::Codex, false, LabelSource::Relative),
            "66 lost the run and must record a loss, not silence",
        );
        // A comparison is one judge's opinion about one task: heavier than an exit code, never
        // heavier than the human's own verdict.
        assert_eq!(labels[0].weight(), WEIGHT_RELATIVE);
        const { assert!(WEIGHT_RELATIVE < WEIGHT_OUTCOME) };
    }

    /// The other half of the same rule: a run whose grades sit on top of each other did not
    /// separate anybody, so it must invent neither a winner nor a loser.
    #[test]
    fn a_flat_run_labels_nobody() {
        let log = [
            started("r-1", "review", &["claude", "codex"]),
            route("r-1", "claude", Some("review"), "aa", 5),
            graded("r-1", "claude", 80),
            graded("r-1", "codex", 78),
            finished("r-1", "ok"),
        ]
        .join("\n");
        assert!(
            derive_labels(&parse_runs(&log), DEFAULT_RERUN_WINDOW_MS).is_empty(),
            "a 2-point spread is judge rounding, not a finding",
        );

        // Nor does a flat run fall through to its exit status: it was graded, and "the judge
        // could not separate them" is the answer, not an absence of evidence.
        let unanimous = [
            started("r-2", "review", &["claude", "codex", "opencode"]),
            route("r-2", "claude", Some("review"), "bb", 5),
            graded("r-2", "claude", 90),
            graded("r-2", "codex", 90),
            graded("r-2", "opencode", 90),
            finished("r-2", "ok"),
        ]
        .join("\n");
        assert!(
            derive_labels(&parse_runs(&unanimous), DEFAULT_RERUN_WINDOW_MS).is_empty(),
            "three identical grades separate nobody",
        );
    }

    /// A run that graded exactly one member has nothing to compare against, so it keeps the
    /// absolute reading — and that reading stays conservative: only a decisive grade speaks.
    #[test]
    fn a_lone_grade_keeps_the_absolute_reading() {
        let lone = |grade: u8| {
            let log = [
                started("r-1", "rescue", &["claude"]),
                route("r-1", "claude", Some("coding"), "aa", 5),
                graded("r-1", "claude", grade),
            ]
            .join("\n");
            derive_labels(&parse_runs(&log), DEFAULT_RERUN_WINDOW_MS)
        };

        let pass = lone(82);
        assert_eq!(pass.len(), 1);
        assert_eq!(
            (pass[0].success, pass[0].source),
            (true, LabelSource::Grade)
        );
        let fail = lone(20);
        assert_eq!(
            (fail[0].success, fail[0].source),
            (false, LabelSource::Grade)
        );
        assert!(lone(55).is_empty(), "a middling lone grade says nothing");
    }

    /// The arena feeds its human head-to-head votes through this same channel as Bradley–Terry
    /// scores (`cli::arena::emit_grades`), so they land as relative labels — and deliberately at
    /// the same weight as a model judge's, since the log cannot tell the two apart and the arena
    /// is designed to move the learned scores rather than outrank them.
    #[test]
    fn arena_votes_land_as_relative_labels_at_the_judge_weight() {
        let log = [
            started("r-1", "arena", &["claude", "codex"]),
            route("r-1", "claude", Some("coding"), "aa", 5),
            r#"{"event":"member_graded","ts":11,"run_id":"r-1","backend":"claude","grade":73,"rank":1}"#.into(),
            r#"{"event":"member_graded","ts":11,"run_id":"r-1","backend":"codex","grade":27,"rank":2}"#.into(),
        ]
        .join("\n");
        let labels = derive_labels(&parse_runs(&log), DEFAULT_RERUN_WINDOW_MS);
        assert_eq!(
            labels
                .iter()
                .map(|l| (l.backend, l.success, l.source, l.weight()))
                .collect::<Vec<_>>(),
            vec![
                (BackendId::Claude, true, LabelSource::Relative, 2.0),
                (BackendId::Codex, false, LabelSource::Relative, 2.0),
            ],
        );

        // A round with a single contender has no head-to-head; Bradley–Terry scores it 50, which
        // the absolute fallback correctly refuses to read as anything.
        let alone = [
            started("r-2", "arena", &["claude"]),
            route("r-2", "claude", Some("coding"), "bb", 5),
            r#"{"event":"member_graded","ts":11,"run_id":"r-2","backend":"claude","grade":50,"rank":1}"#.into(),
        ]
        .join("\n");
        assert!(derive_labels(&parse_runs(&alone), DEFAULT_RERUN_WINDOW_MS).is_empty());
    }

    /// Precedence is unchanged: a graded run reads its grades even when it also exited ok and
    /// was re-dispatched elsewhere inside the window.
    #[test]
    fn grades_still_outrank_rerun_and_exit_status() {
        let log = [
            started("r-1", "rescue", &["claude"]),
            route("r-1", "claude", Some("coding"), "aa", 1_000),
            graded("r-1", "claude", 85),
            finished("r-1", "ok"),
            // Same task, different backend, one second later: r-1 would otherwise be superseded.
            started("r-2", "rescue", &["codex"]),
            route("r-2", "codex", Some("coding"), "aa", 2_000),
            finished("r-2", "ok"),
        ]
        .join("\n");
        let labels = derive_labels(&parse_runs(&log), DEFAULT_RERUN_WINDOW_MS);
        assert_eq!(
            labels
                .iter()
                .map(|l| (l.backend, l.success, l.source))
                .collect::<Vec<_>>(),
            vec![
                (BackendId::Claude, true, LabelSource::Grade),
                (BackendId::Codex, true, LabelSource::Exit),
            ],
            "the grade wins over both rerun and exit for r-1",
        );
    }

    #[test]
    fn fan_out_grades_fold_into_each_members_resolved_effort_row() {
        let log = [
            started("r-1", "review", &["claude", "codex"]),
            route("r-1", "claude", Some("review"), "aa", 5),
            r#"{"event":"member_started","ts":6,"run_id":"r-1","backend":"claude","aggregator":false,"effort":"low"}"#.into(),
            r#"{"event":"member_started","ts":6,"run_id":"r-1","backend":"codex","aggregator":false,"effort":"high"}"#.into(),
            r#"{"event":"member_graded","ts":7,"run_id":"r-1","backend":"claude","grade":90}"#.into(),
            r#"{"event":"member_graded","ts":7,"run_id":"r-1","backend":"codex","grade":20}"#.into(),
            finished("r-1", "ok"),
        ]
        .join("\n");

        let labels = derive_labels(&parse_runs(&log), DEFAULT_RERUN_WINDOW_MS);
        assert_eq!(labels.len(), 2);
        assert_eq!(labels[0].effort, Some(Effort::Low));
        assert_eq!(labels[1].effort, Some(Effort::High));

        let scores = fold_scores(&labels, 1);
        let claude_low = ProfileKey::new(BackendId::Claude, None, Some(Effort::Low));
        let codex_high = ProfileKey::new(BackendId::Codex, None, Some(Effort::High));
        assert_eq!(scores.len(), 2);
        assert!(scores.contains_key(&claude_low));
        assert!(scores.contains_key(&codex_high));
        assert!(!scores.contains_key(&ProfileKey::unpinned(BackendId::Claude)));
        assert!(!scores.contains_key(&ProfileKey::unpinned(BackendId::Codex)));
    }

    /// The case that motivated this exclusion, taken from real telemetry: run `70658` dispatched
    /// `claude` as both a fan-out member and the aggregator, and `claude` graded itself 86/rank1
    /// — a self-grade is not evidence for the grader. The peer it also graded must still land.
    #[test]
    fn an_aggregator_grading_itself_produces_no_evidence_for_itself() {
        let log = [
            started("r-1", "review", &["claude", "codex"]),
            route("r-1", "claude", Some("review"), "aa", 5),
            r#"{"event":"member_started","ts":6,"run_id":"r-1","backend":"claude","aggregator":true}"#.into(),
            graded("r-1", "claude", 86),
            graded("r-1", "codex", 20),
            finished("r-1", "ok"),
        ]
        .join("\n");

        let labels = derive_labels(&parse_runs(&log), DEFAULT_RERUN_WINDOW_MS);
        assert_eq!(
            labels
                .iter()
                .map(|l| (l.backend, l.success, l.source))
                .collect::<Vec<_>>(),
            vec![(BackendId::Codex, false, LabelSource::Grade)],
            "the self-grade must vanish and the peer's grade must land as a lone (absolute) reading: {labels:?}",
        );
    }

    /// Same shape, but with a second peer: the aggregator's self-grade must not enter the
    /// best/worst comparison either, so it cannot manufacture a loser out of a peer that only
    /// looks bad next to the aggregator's inflated score of itself.
    #[test]
    fn an_aggregators_self_grade_is_excluded_from_the_relative_comparison_too() {
        let log = [
            started("r-1", "review", &["claude", "codex", "opencode"]),
            route("r-1", "claude", Some("review"), "aa", 5),
            r#"{"event":"member_started","ts":6,"run_id":"r-1","backend":"claude","aggregator":true}"#.into(),
            graded("r-1", "claude", 99), // self-grade: would otherwise dominate as "best"
            graded("r-1", "codex", 80),
            graded("r-1", "opencode", 60),
            finished("r-1", "ok"),
        ]
        .join("\n");

        let labels = derive_labels(&parse_runs(&log), DEFAULT_RERUN_WINDOW_MS);
        assert_eq!(
            labels
                .iter()
                .map(|l| (l.backend, l.success, l.source))
                .collect::<Vec<_>>(),
            vec![
                (BackendId::Codex, true, LabelSource::Relative),
                (BackendId::Opencode, false, LabelSource::Relative),
            ],
            "claude never appears; codex (80) and opencode (60) are compared against each \
             other, not against claude's excluded self-grade of 99: {labels:?}",
        );
    }

    #[test]
    fn old_member_started_lines_fall_back_to_the_run_variant() {
        let route = r#"{"event":"route_decided","ts":5,"run_id":"r-1","backend":"claude","reason":"ensemble","category":"review","model":"shared-model","effort":"medium","task_hash":"aa"}"#;
        let grade =
            r#"{"event":"member_graded","ts":7,"run_id":"r-1","backend":"claude","grade":90}"#;
        let without_member_started = [
            started("r-1", "review", &["claude"]),
            route.into(),
            grade.into(),
        ]
        .join("\n");
        let with_old_member_started = [
            started("r-1", "review", &["claude"]),
            route.into(),
            r#"{"event":"member_started","ts":6,"run_id":"r-1","backend":"claude","aggregator":false}"#.into(),
            grade.into(),
        ]
        .join("\n");

        let expected = derive_labels(
            &parse_runs(&without_member_started),
            DEFAULT_RERUN_WINDOW_MS,
        );
        let actual = derive_labels(
            &parse_runs(&with_old_member_started),
            DEFAULT_RERUN_WINDOW_MS,
        );
        assert_eq!(actual, expected);
        assert_eq!(actual[0].model.as_deref(), Some("shared-model"));
        assert_eq!(actual[0].effort, Some(Effort::Medium));
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
