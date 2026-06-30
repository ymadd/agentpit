//! Gold-bench harness: the deterministic scoring suite that fills capability profiles with
//! *measured* numbers (design §2). [`suite`] holds the static task definitions; [`replay`] is
//! the offline scoring path that folds a recorded fixture into a benchmark result; [`merge`] is
//! the pure score aggregation + thin `profiles.toml` merge; [`judge`] is the grade-dispatch
//! facade over its two scorer halves — [`score`] (pure Review/Sec/Adversarial/LongContext/Refactor
//! metrics) and [`sandbox`] (the network-isolated code-execution jail).

pub mod judge;
pub mod merge;
pub mod replay;
pub mod run;
mod sandbox;
mod score;
pub mod suite;
mod tasks;

pub use judge::{
    GradeOutcome, SandboxOutcome, extract_last_fence, grade, run_hidden_tests, score_adversarial,
    score_long_context, score_refactor, score_review, score_security_review,
};
pub use merge::{GradedTask, aggregate, merge_into_profiles};
pub use replay::{ReplayFixture, TaskOutcome, score_fixture};
pub use run::{RawFixture, RawOutput, RawScored, run_live, score_raw};
pub use suite::{
    AdversarialItem, AdversarialKind, Defect, FixtureLang, GoldTask, Grading, HiddenTests, Needle,
    RefactorGrading, SecurityDefect, all_tasks,
};
