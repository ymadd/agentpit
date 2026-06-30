//! Gold benchmark suite — the static, deterministic task definitions that the harness scores
//! a backend against to fill its capability profile with *measured* numbers.
//!
//! This file is **data and types only**: it neither executes candidates nor grades them. It
//! defines one `GoldTask` per probe across the seven *complete-gold* categories (design §2.2)
//! and the category-specific grading metadata each carries. The judge/merge stages that
//! consume this metadata live in sibling modules.
//!
//! The three resolved disagreements from design §2.3 are encoded directly in the types so the
//! grader cannot drift from the design:
//!
//! 1. **Refactor** — behavioural equivalence is a *hard gate* ([`RefactorGrading::behavior_test`]
//!    must pass before any complexity/LOC baseline contributes), not a weighted sum.
//! 2. **SecurityReview** — a defect is identified by **CWE-id**, so [`SecurityDefect::cwe`] is a
//!    required `String` (not `Option`), unlike the looser [`Defect::cwe`] used for plain review.
//! 3. **AdversarialReview** — reporting a decoy is a *hard* false positive at **weight 2**
//!    ([`AdversarialKind::DECOY_FP_WEIGHT`]).
//!
//! Everything here is immutable: [`all_tasks`] builds and returns a fresh `Vec` from literals;
//! nothing is mutated in place.

use serde::{Deserialize, Serialize};

use crate::profile::category::TaskCategory;

/// Language of a sandbox-executed grading fixture. Python fixtures run under `pytest`, Rust
/// fixtures under `cargo test`, both network-isolated with a timeout (design §2.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FixtureLang {
    Python,
    Rust,
}

/// A hidden-test fixture: test source that is run against the candidate's extracted solution.
/// The score is `passed / total` of the assertions inside `source`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HiddenTests {
    pub lang: FixtureLang,
    /// Test source executed against the candidate output (imports the candidate as `solution`).
    pub source: String,
}

/// Refactor grading. Behavioural equivalence is a **hard gate** (design §2.3-1): the score is
/// `behavior_pass ? metric_norm : 0`, so a refactor that changes behaviour earns zero even if
/// it looks tidier. The optional baselines bound the metric term only when the gate passes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RefactorGrading {
    /// Behaviour-equivalence reference test. MUST pass or the task scores 0.
    pub behavior_test: HiddenTests,
    /// Optional cyclomatic-complexity ceiling the refactor should come in under.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub complexity_baseline: Option<u32>,
    /// Optional line-count ceiling the refactor should come in under.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub loc_baseline: Option<u32>,
}

/// One embedded defect a plain-review task expects the reviewer to find. A report matches when
/// its line is within ±2 of `line` and the kind (and CWE, when present) agree (design §2.1).
/// For plain `Review`, `cwe` is optional — kind matching suffices.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Defect {
    /// 1-based line of the defect in the task prompt's code listing.
    pub line: u32,
    /// Defect-class token, e.g. `"missing-auth-check"`.
    pub kind: String,
    /// CWE identifier, e.g. `"CWE-285"`. Optional for plain review.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwe: Option<String>,
}

/// One embedded defect a security-review task expects. Unlike [`Defect`], `cwe` is **required**:
/// security findings are matched on CWE-id, not keywords (design §2.3-2), so the type forbids a
/// security defect without one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SecurityDefect {
    /// 1-based line of the defect in the task prompt's code listing.
    pub line: u32,
    /// Defect-class token, e.g. `"sql-injection"`.
    pub kind: String,
    /// CWE identifier — required (e.g. `"CWE-89"`).
    pub cwe: String,
    /// Optional secondary severity bonus tag (design §2.3-2: severity is a side bonus).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub severity: Option<String>,
}

/// Whether an adversarial-review item is a genuine defect or a planted decoy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AdversarialKind {
    /// A genuine defect: reporting it is a true positive.
    Real,
    /// A plausible-looking non-defect: reporting it is a hard false positive.
    Decoy,
}

impl AdversarialKind {
    /// False-positive weight applied when a decoy is (wrongly) reported. Decoys are *hard* FPs
    /// at weight 2 (design §2.3-3); reporting a real defect is a true positive, not an FP.
    pub const DECOY_FP_WEIGHT: u32 = 2;

    /// The FP weight this kind contributes when reported: 2 for a decoy, 0 for a real defect.
    pub fn fp_weight(self) -> u32 {
        match self {
            AdversarialKind::Real => 0,
            AdversarialKind::Decoy => Self::DECOY_FP_WEIGHT,
        }
    }
}

/// One item in an adversarial-review task: a real defect to catch or a decoy to resist. Scoring
/// is a weighted F1 where flagging a [`AdversarialKind::Decoy`] costs [`AdversarialKind::DECOY_FP_WEIGHT`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdversarialItem {
    /// 1-based line of the item in the task prompt's code listing.
    pub line: u32,
    /// Defect-class token (for a real defect) or the trap it imitates (for a decoy).
    pub kind: String,
    /// CWE identifier when applicable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwe: Option<String>,
    /// Real defect or decoy.
    pub item: AdversarialKind,
}

/// A long-context exact-match probe: surface `expected` verbatim in answer to `needle`. Scored
/// by exact string match, so no LLM judge is needed (design §2.2 — `correct / N`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Needle {
    /// The question identifying what to retrieve from the haystack.
    pub needle: String,
    /// The exact expected answer; compared by exact string equality.
    pub expected: String,
}

/// Category-specific grading metadata. Each variant carries exactly what its grader needs and
/// nothing more, so the grader for a category is a total function over its variant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Grading {
    /// Coding & Debug: hidden tests, score = `passed / total`.
    HiddenTests(HiddenTests),
    /// Refactor: behaviour-equivalence hard gate plus optional metric baselines.
    Refactor(RefactorGrading),
    /// Review: embedded defects (CWE optional); F1, with `1/(1+FP)` for over-detection. An
    /// empty `defects` list is the noise-resistance case — the only correct answer is `[]`.
    Review { defects: Vec<Defect> },
    /// SecurityReview: embedded defects matched on required CWE-id; F1 / `1/(1+FP)`.
    SecurityReview { defects: Vec<SecurityDefect> },
    /// AdversarialReview: real defects mixed with decoys; weighted F1.
    Adversarial { items: Vec<AdversarialItem> },
    /// LongContext: needle/expected exact-match probes.
    LongContext { needles: Vec<Needle> },
}

/// One gold task: a category, a stable unique id, the prompt shown to the candidate backend,
/// and the category-specific grading metadata used to score the response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GoldTask {
    /// Stable, unique id, namespaced by category, e.g. `"coding/parse_duration"`.
    pub id: String,
    /// Which capability column this task measures.
    pub category: TaskCategory,
    /// The instruction shown to the candidate backend.
    pub prompt: String,
    /// How to grade the candidate's response.
    pub grading: Grading,
}

impl GoldTask {
    /// Construct a gold task from string slices, owning copies of each. Visible to the sibling
    /// [`tasks`](super::tasks) module, which holds the per-probe builders.
    pub(super) fn new(id: &str, category: TaskCategory, prompt: &str, grading: Grading) -> Self {
        Self {
            id: id.to_string(),
            category,
            prompt: prompt.to_string(),
            grading,
        }
    }
}

/// Re-export the suite's task builders so `all_tasks` stays addressable at this module's path
/// (the data definitions live in the sibling [`tasks`](super::tasks) module).
pub use super::tasks::all_tasks;

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    /// The seven complete-gold categories this suite must cover (design §2.2).
    const COMPLETE_GOLD: [TaskCategory; 7] = [
        TaskCategory::Coding,
        TaskCategory::Debug,
        TaskCategory::Refactor,
        TaskCategory::Review,
        TaskCategory::SecurityReview,
        TaskCategory::AdversarialReview,
        TaskCategory::LongContext,
    ];

    #[test]
    fn covers_all_seven_complete_gold_categories() {
        let present: BTreeSet<_> = all_tasks().into_iter().map(|t| t.category).collect();
        let expected: BTreeSet<_> = COMPLETE_GOLD.into_iter().collect();
        assert_eq!(present, expected);
    }

    #[test]
    fn every_category_has_at_least_two_tasks() {
        let tasks = all_tasks();
        for category in COMPLETE_GOLD {
            let count = tasks.iter().filter(|t| t.category == category).count();
            assert!(count >= 2, "{category} has only {count} task(s)");
        }
    }

    #[test]
    fn task_ids_are_unique() {
        let tasks = all_tasks();
        let unique: BTreeSet<_> = tasks.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(unique.len(), tasks.len(), "duplicate task id present");
    }

    #[test]
    fn grading_metadata_round_trips_through_json() {
        for task in all_tasks() {
            let json = serde_json::to_string(&task.grading).expect("serialize grading");
            let back: Grading = serde_json::from_str(&json).expect("deserialize grading");
            assert_eq!(back, task.grading, "round-trip mismatch for {}", task.id);
        }
    }

    #[test]
    fn whole_task_round_trips_through_json() {
        let tasks = all_tasks();
        let json = serde_json::to_string(&tasks).expect("serialize tasks");
        let back: Vec<GoldTask> = serde_json::from_str(&json).expect("deserialize tasks");
        assert_eq!(back, tasks);
    }

    #[test]
    fn refactor_tasks_all_carry_a_behaviour_gate() {
        for task in all_tasks()
            .into_iter()
            .filter(|t| t.category == TaskCategory::Refactor)
        {
            match task.grading {
                Grading::Refactor(g) => {
                    assert!(!g.behavior_test.source.is_empty(), "{} lacks gate", task.id);
                }
                other => panic!("refactor task {} graded as {other:?}", task.id),
            }
        }
    }

    #[test]
    fn security_defects_require_cwe() {
        // The type already forbids a missing CWE (it is a `String`, not `Option`); this asserts
        // the data fills it in non-empty for every security defect.
        for task in all_tasks()
            .into_iter()
            .filter(|t| t.category == TaskCategory::SecurityReview)
        {
            if let Grading::SecurityReview { defects } = task.grading {
                for defect in defects {
                    assert!(
                        defect.cwe.starts_with("CWE-"),
                        "{} has a non-CWE id: {}",
                        task.id,
                        defect.cwe
                    );
                }
            }
        }
    }

    #[test]
    fn decoy_false_positive_weight_is_two() {
        assert_eq!(AdversarialKind::DECOY_FP_WEIGHT, 2);
        assert_eq!(AdversarialKind::Decoy.fp_weight(), 2);
        assert_eq!(AdversarialKind::Real.fp_weight(), 0);
    }

    #[test]
    fn adversarial_tasks_mix_real_and_decoy() {
        for task in all_tasks()
            .into_iter()
            .filter(|t| t.category == TaskCategory::AdversarialReview)
        {
            if let Grading::Adversarial { items } = task.grading {
                assert!(
                    items.iter().any(|i| i.item == AdversarialKind::Real),
                    "{} has no real defect",
                    task.id
                );
                assert!(
                    items.iter().any(|i| i.item == AdversarialKind::Decoy),
                    "{} has no decoy",
                    task.id
                );
            }
        }
    }

    #[test]
    fn coding_and_debug_use_hidden_tests() {
        for task in all_tasks()
            .into_iter()
            .filter(|t| matches!(t.category, TaskCategory::Coding | TaskCategory::Debug))
        {
            assert!(
                matches!(task.grading, Grading::HiddenTests(_)),
                "{} is not graded by hidden tests",
                task.id
            );
        }
    }
}
