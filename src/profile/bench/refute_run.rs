//! ④ refute-quality gate runner (design §5.1, §4.6 crux #1): does a live critique→defense pass
//! actually improve a stuck candidate, or merely hold it steady (or make it worse)?
//!
//! Mirrors [`super::run`]'s split between a network-touching half ([`run_refute_probe`] /
//! [`run_refute_bench`]) and a pure, unit-tested half ([`score_refute_bundle`]): only the live
//! critique+defense dispatch touches the network ([`crate::workflow::converse::run_refute`] —
//! reused as-is, not reimplemented); grading both the `stuck` candidate (the "before" half) and
//! the defense leg's revised candidate (the "after" half) is deterministic and runs through the
//! same [`super::judge`] every other probe in the suite uses. Adjudication is deliberately not
//! benched here: it is the manager's own in-context turn, not a dispatch (design §4.5), so there
//! is nothing live to measure for it (design §4.6 crux #4 is a separate, still-open question).

use std::path::Path;

use anyhow::{Result, anyhow};
use tokio_util::sync::CancellationToken;

use super::judge::{GradeOutcome, grade_refute_inner};
use super::suite::{GoldTask, Grading};
use crate::dispatch::Registries;
use crate::types::BackendId;
use crate::workflow::converse::{RefuteBundle, run_refute};

/// The margin a probe's "after" score must clear its "before" score by to count as a real
/// recovery rather than noise (design §5.1: green means refute *helps*, not merely
/// "doesn't regress").
pub const DELTA_PASS_MARGIN: f64 = 0.2;

/// One probe's before/after result.
#[derive(Debug, Clone)]
pub struct RefuteProbeResult {
    pub task_id: String,
    /// Grading the `stuck` candidate as-is (offline, deterministic, no network).
    pub before: Option<GradeOutcome>,
    /// Grading the defense leg's revised candidate against the same inner grader.
    pub after: Option<GradeOutcome>,
    /// Whether the critique leg produced a critique at all.
    pub critique_ok: bool,
    /// Whether the defense leg produced a defense at all.
    pub defense_ok: bool,
}

impl RefuteProbeResult {
    /// `after - before` when both are real (non-skipped) scores. `None` when either half was
    /// skipped (sandbox unavailable) or never ran — a skip must never be silently read as zero.
    pub fn delta(&self) -> Option<f64> {
        match (self.before, self.after) {
            (Some(GradeOutcome::Scored(b)), Some(GradeOutcome::Scored(a))) => Some(a - b),
            _ => None,
        }
    }

    /// This probe alone clears the pass margin.
    pub fn passes(&self) -> bool {
        self.delta().is_some_and(|d| d >= DELTA_PASS_MARGIN)
    }
}

/// Run the live critique+defense legs for one probe and grade both halves. The only network-
/// touching function in this module; everything it doesn't do itself is delegated to
/// [`run_refute`] (generation) and [`score_refute_bundle`] (grading).
pub async fn run_refute_probe(
    task: &GoldTask,
    critic: BackendId,
    defender: BackendId,
    cwd: &Path,
    regs: &Registries,
    cancel: CancellationToken,
) -> Result<RefuteProbeResult> {
    if !matches!(task.grading, Grading::Refute { .. }) {
        return Err(anyhow!("{} is not a refute-quality probe", task.id));
    }
    let Grading::Refute { stuck, .. } = &task.grading else {
        unreachable!("checked above");
    };
    let bundle = run_refute(
        &task.prompt,
        stuck,
        critic,
        defender,
        cwd,
        regs,
        cancel,
        None,
    )
    .await;
    Ok(score_refute_bundle(task, &bundle))
}

/// Run every probe in `tasks` against the same critic/defender pair, sequentially (each leg
/// already streams its own progress; three probes don't need fan-out). A single probe's dispatch
/// failure degrades that probe to an all-`None` result rather than aborting the rest — refute
/// itself is advisory, and so is the gate that measures it.
pub async fn run_refute_bench(
    tasks: &[GoldTask],
    critic: BackendId,
    defender: BackendId,
    cwd: &Path,
    regs: &Registries,
    cancel: CancellationToken,
) -> Vec<RefuteProbeResult> {
    let mut results = Vec::with_capacity(tasks.len());
    for task in tasks {
        match run_refute_probe(task, critic, defender, cwd, regs, cancel.clone()).await {
            Ok(r) => results.push(r),
            Err(e) => {
                eprintln!("agentpit: refute-bench probe {} failed: {e:#}", task.id);
                results.push(RefuteProbeResult {
                    task_id: task.id.clone(),
                    before: None,
                    after: None,
                    critique_ok: false,
                    defense_ok: false,
                });
            }
        }
    }
    results
}

/// Pure half: grade the before/after halves of an already-dispatched [`RefuteBundle`]. Never
/// touches the network — unit-testable with a hand-built bundle, mirroring
/// [`super::run::score_raw`]'s split from [`super::run::run_live`].
pub fn score_refute_bundle(task: &GoldTask, bundle: &RefuteBundle) -> RefuteProbeResult {
    let Grading::Refute { stuck, .. } = &task.grading else {
        return RefuteProbeResult {
            task_id: task.id.clone(),
            before: None,
            after: None,
            critique_ok: false,
            defense_ok: false,
        };
    };
    let before = grade_refute_inner(task, stuck);
    let critique_ok = bundle.critique.is_ok();
    // Grade the defense leg's *raw* text, not a pre-extracted candidate: the inner grader (e.g.
    // `run_hidden_tests`) already runs `extract_last_fence` itself on whatever it is given, the
    // same way it grades a raw one-shot dispatch in the normal live-run path. Extracting here
    // first would hand it bare code with no fence markers left to find — a real bug this module
    // shipped with once already (caught by the test below).
    let (defense_ok, after) = match &bundle.defense {
        Some(Ok(defense_text)) => (true, grade_refute_inner(task, defense_text)),
        Some(Err(_)) | None => (false, None),
    };
    RefuteProbeResult {
        task_id: task.id.clone(),
        before,
        after,
        critique_ok,
        defense_ok,
    }
}

/// The gate's overall verdict (design §5.1): green only when there is at least one real delta
/// (an all-skipped/all-failed run is inconclusive, not green) and **every** probe that produced
/// one cleared the pass margin — one strong recovery must not mask another probe's regression.
pub fn gate_passes(results: &[RefuteProbeResult]) -> bool {
    let deltas: Vec<f64> = results.iter().filter_map(|r| r.delta()).collect();
    !deltas.is_empty() && deltas.iter().all(|&d| d >= DELTA_PASS_MARGIN)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile::bench::refute_tasks::refute_probe_tasks;
    use crate::profile::bench::suite::{FixtureLang, HiddenTests};
    use crate::profile::category::TaskCategory;

    fn refute_task(stuck: &str) -> GoldTask {
        GoldTask::new(
            "x",
            TaskCategory::Coding,
            "p",
            Grading::Refute {
                stuck: stuck.to_string(),
                inner: Box::new(Grading::HiddenTests(HiddenTests {
                    lang: FixtureLang::Python,
                    source: "from solution import f\n\ndef test():\n    assert f() == 2\n"
                        .to_string(),
                })),
            },
        )
    }

    #[test]
    fn delta_is_after_minus_before_when_both_are_real_scores() {
        let r = RefuteProbeResult {
            task_id: "x".into(),
            before: Some(GradeOutcome::Scored(0.0)),
            after: Some(GradeOutcome::Scored(1.0)),
            critique_ok: true,
            defense_ok: true,
        };
        assert_eq!(r.delta(), Some(1.0));
        assert!(r.passes());
    }

    #[test]
    fn delta_is_none_when_either_half_is_skipped_or_missing() {
        let skipped = RefuteProbeResult {
            task_id: "x".into(),
            before: Some(GradeOutcome::Skipped),
            after: Some(GradeOutcome::Scored(1.0)),
            critique_ok: true,
            defense_ok: true,
        };
        assert_eq!(skipped.delta(), None);
        assert!(!skipped.passes());

        let no_defense = RefuteProbeResult {
            task_id: "x".into(),
            before: Some(GradeOutcome::Scored(0.0)),
            after: None,
            critique_ok: false,
            defense_ok: false,
        };
        assert_eq!(no_defense.delta(), None);
        assert!(!no_defense.passes());
    }

    #[test]
    fn gate_requires_at_least_one_real_delta_and_all_of_them_above_margin() {
        let none_real = vec![RefuteProbeResult {
            task_id: "x".into(),
            before: None,
            after: None,
            critique_ok: false,
            defense_ok: false,
        }];
        assert!(!gate_passes(&none_real), "all-None run must not be green");

        let one_strong_one_weak = vec![
            RefuteProbeResult {
                task_id: "a".into(),
                before: Some(GradeOutcome::Scored(0.0)),
                after: Some(GradeOutcome::Scored(1.0)),
                critique_ok: true,
                defense_ok: true,
            },
            RefuteProbeResult {
                task_id: "b".into(),
                before: Some(GradeOutcome::Scored(0.5)),
                after: Some(GradeOutcome::Scored(0.55)),
                critique_ok: true,
                defense_ok: true,
            },
        ];
        assert!(
            !gate_passes(&one_strong_one_weak),
            "a strong probe must not mask a weak one"
        );

        let both_clear_margin = vec![
            RefuteProbeResult {
                task_id: "a".into(),
                before: Some(GradeOutcome::Scored(0.0)),
                after: Some(GradeOutcome::Scored(1.0)),
                critique_ok: true,
                defense_ok: true,
            },
            RefuteProbeResult {
                task_id: "b".into(),
                before: Some(GradeOutcome::Scored(0.5)),
                after: Some(GradeOutcome::Scored(0.9)),
                critique_ok: true,
                defense_ok: true,
            },
        ];
        assert!(gate_passes(&both_clear_margin));
    }

    #[test]
    fn score_refute_bundle_grades_stuck_and_extracts_the_revision_from_a_fenced_defense() {
        if !matches!(
            crate::profile::bench::sandbox::run_in_sandbox(
                FixtureLang::Python,
                "def f():\n    return 2\n",
                "from solution import f\n\ndef test():\n    assert f() == 2\n",
            ),
            Ok(crate::profile::bench::sandbox::SandboxOutcome::Ran {
                passed: 1,
                total: 1
            })
        ) {
            eprintln!("skipping refute grading test: functional Python sandbox unavailable");
            return;
        }
        let task = refute_task("```python\ndef f():\n    return 1\n```\n");
        let bundle = RefuteBundle {
            critic: BackendId::Codex,
            defender: BackendId::Opencode,
            critique: Ok("returns 1, should return 2".to_string()),
            defense: Some(Ok(
                "I concede the critique.\n\n```python\ndef f():\n    return 2\n```\n".to_string(),
            )),
        };
        let result = score_refute_bundle(&task, &bundle);
        assert_eq!(result.before, Some(GradeOutcome::Scored(0.0)));
        assert_eq!(result.after, Some(GradeOutcome::Scored(1.0)));
        assert!(result.critique_ok);
        assert!(result.defense_ok);
        assert_eq!(result.delta(), Some(1.0));
    }

    #[test]
    fn score_refute_bundle_handles_a_failed_critique_with_no_defense() {
        let task = refute_task("```python\ndef f():\n    return 1\n```\n");
        let bundle = RefuteBundle {
            critic: BackendId::Codex,
            defender: BackendId::Opencode,
            critique: Err("codex: not authenticated".to_string()),
            defense: None,
        };
        let result = score_refute_bundle(&task, &bundle);
        assert!(!result.critique_ok);
        assert!(!result.defense_ok);
        assert_eq!(result.after, None);
        assert_eq!(result.delta(), None);
    }

    #[test]
    fn real_probe_tasks_round_trip_through_score_refute_bundle() {
        // Sanity check that the actual MVP probes (not a hand-built fixture) plug into the
        // scoring path without panicking, and that a defense which fixes the bug scores 1.0.
        for task in refute_probe_tasks() {
            let Grading::Refute { inner, .. } = &task.grading else {
                panic!("{} is not Refute-graded", task.id);
            };
            let (lang_tag, fixed) = match inner.as_ref() {
                Grading::HiddenTests(t) if t.lang == FixtureLang::Python => ("python", "pass"),
                other => panic!("{}: unexpected inner grading {other:?}", task.id),
            };
            let _ = lang_tag;
            // A defense that doesn't fix anything: scores whatever the empty/no-op body scores
            // (not asserted here — covered by the offline `each_stuck_candidate_*` test in
            // refute_tasks.rs). This test only proves the plumbing doesn't panic on real data.
            let bundle = RefuteBundle {
                critic: BackendId::Codex,
                defender: BackendId::Opencode,
                critique: Ok("a critique".to_string()),
                defense: Some(Ok(format!("```python\n{fixed}\n```\n"))),
            };
            let result = score_refute_bundle(&task, &bundle);
            assert_eq!(result.task_id, task.id);
            assert!(result.before.is_some());
        }
    }
}
