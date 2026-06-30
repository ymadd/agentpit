//! Grade dispatch for the gold-bench suite (design §2.1).
//!
//! This is the single point that maps each [`Grading`] variant onto its scorer and folds the two
//! flavours of result — a `0.0..=1.0` number, or a sandbox-skip — into one [`GradeOutcome`]. The
//! actual scoring lives in two siblings, kept here behind a re-export so the bench's public API is
//! one module: [`score`](super::score) holds the pure (no-I/O) scorers, [`sandbox`](super::sandbox)
//! the network-isolated code-execution jail. The only logic that genuinely belongs here is the
//! glue: the variant dispatch, the sandbox→grade mapping, and the refactor behaviour-gate (which
//! straddles both siblings — it runs the gate in the sandbox, then defers to the pure metric).

use super::sandbox::{lang_tag, run_in_sandbox, sandbox_exec_available};
use super::score::refactor_metric_norm;
use super::suite::{GoldTask, Grading, RefactorGrading};

pub use super::sandbox::{SandboxOutcome, run_hidden_tests};
pub use super::score::{
    extract_last_fence, score_adversarial, score_long_context, score_review, score_security_review,
};

/// The result of a grade: skipped (jail unavailable) or a `0.0..=1.0` score.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum GradeOutcome {
    /// `sandbox-exec` was unavailable; this task is not counted as a pass *or* a fail.
    Skipped,
    /// A normalised `0.0..=1.0` score.
    Scored(f64),
}

/// Grade one gold task against a candidate's raw output. The single dispatch point that maps
/// each [`Grading`] variant onto its scorer. Pure for every variant except the two
/// sandbox-backed ones, which run hidden tests in an isolated jail.
pub fn grade(task: &GoldTask, output: &str) -> GradeOutcome {
    match &task.grading {
        Grading::HiddenTests(tests) => outcome_to_grade(run_hidden_tests(tests, output)),
        Grading::Refactor(grading) => score_refactor(grading, output),
        Grading::Review { defects } => GradeOutcome::Scored(score_review(defects, output)),
        Grading::SecurityReview { defects } => {
            GradeOutcome::Scored(score_security_review(defects, output))
        }
        Grading::Adversarial { items } => GradeOutcome::Scored(score_adversarial(items, output)),
        Grading::LongContext { needles } => {
            GradeOutcome::Scored(score_long_context(needles, output))
        }
    }
}

/// Map a raw sandbox outcome to a grade: `passed/total`, or `Skipped` when the jail was absent.
fn outcome_to_grade(outcome: SandboxOutcome) -> GradeOutcome {
    match outcome {
        SandboxOutcome::Skipped => GradeOutcome::Skipped,
        SandboxOutcome::Ran { passed, total } => GradeOutcome::Scored(if total == 0 {
            0.0
        } else {
            f64::from(passed) / f64::from(total)
        }),
    }
}

/// Score a refactor: behaviour equivalence is a hard gate (design §2.3-1). The reference test
/// must fully pass before complexity/LOC metrics contribute; a failing gate scores 0.
pub fn score_refactor(grading: &RefactorGrading, output: &str) -> GradeOutcome {
    if !sandbox_exec_available() {
        super::sandbox::log_skip("refactor grade");
        return GradeOutcome::Skipped;
    }
    let tag = lang_tag(grading.behavior_test.lang);
    let Some(code) = extract_last_fence(output, tag) else {
        return GradeOutcome::Scored(0.0);
    };
    let gate = match run_in_sandbox(
        grading.behavior_test.lang,
        &code,
        &grading.behavior_test.source,
    ) {
        Ok(outcome) => outcome,
        Err(e) => {
            eprintln!("agentpit: sandbox execution error: {e}");
            SandboxOutcome::Ran {
                passed: 0,
                total: 1,
            }
        }
    };
    refactor_score_from_gate(gate, &code, grading)
}

/// Pure half of [`score_refactor`]: given the behaviour-gate outcome and the candidate code,
/// emit the grade. Only a fully-passing gate unlocks the metric term.
fn refactor_score_from_gate(
    gate: SandboxOutcome,
    code: &str,
    grading: &RefactorGrading,
) -> GradeOutcome {
    match gate {
        SandboxOutcome::Skipped => GradeOutcome::Skipped,
        SandboxOutcome::Ran { passed, total } if total > 0 && passed >= total => {
            GradeOutcome::Scored(refactor_metric_norm(code, grading))
        }
        SandboxOutcome::Ran { .. } => GradeOutcome::Scored(0.0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile::bench::suite::{FixtureLang, HiddenTests, Needle};
    use crate::profile::category::TaskCategory;

    #[test]
    fn outcome_mapping_and_dispatch() {
        assert_eq!(
            outcome_to_grade(SandboxOutcome::Ran {
                passed: 2,
                total: 4
            }),
            GradeOutcome::Scored(0.5)
        );
        assert_eq!(
            outcome_to_grade(SandboxOutcome::Ran {
                passed: 0,
                total: 0
            }),
            GradeOutcome::Scored(0.0)
        );
        assert_eq!(
            outcome_to_grade(SandboxOutcome::Skipped),
            GradeOutcome::Skipped
        );

        // grade() routes a LongContext task through the exact-match scorer.
        let task = GoldTask {
            id: "x".to_string(),
            category: TaskCategory::LongContext,
            prompt: String::new(),
            grading: Grading::LongContext {
                needles: vec![Needle {
                    needle: "n".to_string(),
                    expected: "42".to_string(),
                }],
            },
        };
        assert_eq!(grade(&task, "42"), GradeOutcome::Scored(1.0));
    }

    #[test]
    fn refactor_gate_is_a_hard_gate() {
        let grading = RefactorGrading {
            behavior_test: HiddenTests {
                lang: FixtureLang::Python,
                source: String::new(),
            },
            complexity_baseline: Some(4),
            loc_baseline: None,
        };
        let code = "def f():\n    return 1\n";

        // Passing gate ⇒ metric score.
        let pass = refactor_score_from_gate(
            SandboxOutcome::Ran {
                passed: 3,
                total: 3,
            },
            code,
            &grading,
        );
        assert_eq!(pass, GradeOutcome::Scored(1.0));

        // Failing gate ⇒ hard zero regardless of how tidy the code is.
        let fail = refactor_score_from_gate(
            SandboxOutcome::Ran {
                passed: 2,
                total: 3,
            },
            code,
            &grading,
        );
        assert_eq!(fail, GradeOutcome::Scored(0.0));

        // Skipped jail ⇒ skipped grade.
        let skip = refactor_score_from_gate(SandboxOutcome::Skipped, code, &grading);
        assert_eq!(skip, GradeOutcome::Skipped);
    }
}
