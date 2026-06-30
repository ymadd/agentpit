//! Gold-bench harness: the deterministic scoring suite that fills capability profiles with
//! *measured* numbers (design §2). [`suite`] holds the static task definitions; [`replay`] is
//! the offline scoring path that folds a recorded fixture into a benchmark result; [`merge`] is
//! the pure score aggregation + thin `profiles.toml` merge; [`judge`] is the grade-dispatch
//! facade over its two scorer halves — [`score`] (pure Review/Sec/Adversarial/LongContext/Refactor
//! metrics) and [`sandbox`] (the network-isolated code-execution jail). [`refute_tasks`] /
//! [`refute_run`] are the standalone ④ refute-quality gate (design §5.1) — excluded from
//! [`suite::all_tasks`], scored separately.

pub mod judge;
pub mod merge;
pub mod refute_run;
pub mod refute_tasks;
pub mod replay;
pub mod run;
mod sandbox;
mod score;
pub mod suite;
mod tasks;

pub use judge::{
    GradeOutcome, SandboxOutcome, extract_last_fence, grade, grade_refute_inner, run_hidden_tests,
    score_adversarial, score_long_context, score_refactor, score_review, score_security_review,
};
pub use merge::{GradedTask, aggregate, merge_into_profiles};
pub use refute_run::{
    DELTA_PASS_MARGIN, RefuteProbeResult, gate_passes, run_refute_bench, score_refute_bundle,
};
pub use refute_tasks::refute_probe_tasks;
pub use replay::{ReplayFixture, TaskOutcome, score_fixture};
pub use run::{RawFixture, RawOutput, RawScored, run_live, score_raw};
pub use suite::{
    AdversarialItem, AdversarialKind, Defect, FixtureLang, GoldTask, Grading, HiddenTests, Needle,
    RefactorGrading, SecurityDefect, all_tasks,
};
