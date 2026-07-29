//! Task diagnosis: turn a free-text task into a `(TaskCategory, confidence)` verdict that
//! the router can consult against the capability profiles.
//!
//! Phase A1 ships the **heuristic layer only** (pure, no model call). A future LLM-assisted
//! layer is intended to refine low-confidence verdicts; the hook for it is documented in
//! [`diagnose`] but not implemented here.

pub mod features;
pub mod heuristic;

pub use features::{TaskFeatures, extract};
pub use heuristic::classify;

use serde::{Deserialize, Serialize};

use crate::profile::TaskCategory;

/// How a diagnosis was reached.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiagnoseMethod {
    /// Pure heuristic classification (no model call).
    Heuristic,
    /// Heuristic refined by an LLM assist pass (not implemented in A1).
    LlmAssisted,
    /// The caller declared the category outright via a first-line [`CATEGORY_MARKER`].
    Declared,
}

/// The outcome of diagnosing a task.
#[derive(Debug, Clone, Serialize)]
pub struct Diagnosis {
    /// The most likely category for the task.
    pub primary: TaskCategory,
    /// Confidence in `primary`, in `[0, 1]` (softmax-normalized).
    pub confidence: f32,
    /// The features the verdict was derived from (kept for observability / `diagnose --json`).
    pub features: TaskFeatures,
    /// Which layer produced the verdict.
    pub method: DiagnoseMethod,
}

/// Confidence below which a future LLM-assisted layer would be consulted to refine the
/// heuristic verdict. In A1 this only gates a (currently no-op) delegation branch.
pub const LLM_ASSIST_CONFIDENCE_THRESHOLD: f32 = 0.55;

/// First-line marker declaring the task's category outright: `CATEGORY: <name>` (name in
/// [`TaskCategory::from_str`]'s grammar, e.g. `review`, `security-review`). Written by a
/// caller that already knows what kind of work it is dispatching — canonically the workflow
/// manager, which decomposed the goal itself: over its long multi-instruction sub-task
/// prompts the keyword heuristic splits across categories and lands under the confidence
/// gate (observed 0.37/0.39 on real manager prompts, 2026-07-30), so a declared category is
/// strictly better evidence than re-guessing from the same text.
pub const CATEGORY_MARKER: &str = "CATEGORY:";

/// Confidence assigned to a declared category — above every routing gate, but not 1.0 so a
/// future assist layer could still be tuned to distrust obviously-wrong declarations.
pub const DECLARED_CONFIDENCE: f32 = 0.95;

/// Parse a first-line `CATEGORY: <name>` declaration. Case-insensitive on the marker;
/// leading blank lines are skipped; an unknown name yields `None` (the heuristic then runs
/// as usual — a typo must degrade to normal routing, never block the dispatch).
fn declared_category(task: &str) -> Option<TaskCategory> {
    let first = task.trim_start().lines().next()?.trim();
    if first.len() < CATEGORY_MARKER.len()
        || !first.as_bytes()[..CATEGORY_MARKER.len()]
            .eq_ignore_ascii_case(CATEGORY_MARKER.as_bytes())
    {
        return None;
    }
    first[CATEGORY_MARKER.len()..].parse::<TaskCategory>().ok()
}

/// Diagnose a task. Pure in A1: a first-line `CATEGORY:` declaration wins outright;
/// otherwise the heuristic layer runs and returns its verdict.
///
/// When `confidence < LLM_ASSIST_CONFIDENCE_THRESHOLD` a future LLM-assisted layer
/// (`src/diagnose/llm.rs`) is meant to take over and return a `LlmAssisted` diagnosis;
/// that layer is intentionally not implemented in A1, so we return the heuristic verdict
/// unchanged here.
pub fn diagnose(task: &str) -> Diagnosis {
    let features = extract(task);
    if let Some(primary) = declared_category(task) {
        return Diagnosis {
            primary,
            confidence: DECLARED_CONFIDENCE,
            features,
            method: DiagnoseMethod::Declared,
        };
    }
    let (primary, confidence) = classify(&features);

    // TODO(A1+): if confidence < LLM_ASSIST_CONFIDENCE_THRESHOLD, delegate to the
    // LLM-assisted layer and (on success) return a `DiagnoseMethod::LlmAssisted` diagnosis.
    // Failures/timeouts must fall back to this heuristic result so diagnosis never blocks.

    Diagnosis {
        primary,
        confidence,
        features,
        method: DiagnoseMethod::Heuristic,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diagnose_refactor_task() {
        let d = diagnose("refactor the auth module to flatten the nesting");
        assert_eq!(d.primary, TaskCategory::Refactor);
        assert_eq!(d.method, DiagnoseMethod::Heuristic);
        assert!((0.0..=1.0).contains(&d.confidence));
    }

    #[test]
    fn diagnose_security_audit_task() {
        let d = diagnose("audit security of the file upload endpoint for injection bugs");
        assert_eq!(d.primary, TaskCategory::SecurityReview);
    }

    #[test]
    fn diagnose_long_task_promotes_to_long_context() {
        let long = "alpha beta gamma delta ".repeat(2_000);
        let d = diagnose(&long);
        assert_eq!(d.primary, TaskCategory::LongContext);
    }

    #[test]
    fn diagnosis_is_serializable() {
        let d = diagnose("refactor this");
        let json = serde_json::to_string(&d).unwrap();
        assert!(json.contains("\"primary\":\"refactor\""));
        assert!(json.contains("\"method\":\"heuristic\""));
    }

    #[test]
    fn declared_category_wins_over_the_heuristic() {
        // The fire-check failure mode (2026-07-30): a real manager-style prompt whose keywords
        // split across categories ("create" → Coding, "fix" → Debug, "document" → Docs, …)
        // diagnosed at ~0.39, under the 0.55 gate. With a declaration the category is exact
        // and the confidence clears every gate.
        let manager_style = "CATEGORY: review\n\
             READ-ONLY REVIEW: do not modify, create, or delete any file. Review guard.rs for \
             defects; report the concrete failure scenario and the suggested fix (describe it, \
             do NOT apply it), then document test gaps.";
        let d = diagnose(manager_style);
        assert_eq!(d.primary, TaskCategory::Review);
        assert_eq!(d.confidence, DECLARED_CONFIDENCE);
        assert!(d.confidence >= LLM_ASSIST_CONFIDENCE_THRESHOLD);
        assert_eq!(d.method, DiagnoseMethod::Declared);
        // The same prompt WITHOUT the declaration stays under the gate — the reason the
        // marker exists. If this half ever fails, the heuristic got better and the marker's
        // justification should be revisited, not the assert loosened silently.
        let undeclared = diagnose(manager_style.split_once('\n').unwrap().1);
        assert!(
            undeclared.confidence < LLM_ASSIST_CONFIDENCE_THRESHOLD,
            "heuristic now confident ({}) — revisit CATEGORY_MARKER's rationale",
            undeclared.confidence
        );
    }

    #[test]
    fn declared_marker_is_case_insensitive_and_accepts_aliases() {
        let d = diagnose("category: security-review\naudit the upload endpoint");
        assert_eq!(d.primary, TaskCategory::SecurityReview);
        assert_eq!(d.method, DiagnoseMethod::Declared);
        // Leading blank lines are tolerated.
        let d = diagnose("\n\n  Category: docs\nwrite the readme");
        assert_eq!(d.primary, TaskCategory::Docs);
    }

    #[test]
    fn unknown_or_misplaced_declaration_falls_back_to_the_heuristic() {
        // A typo'd category must degrade to normal routing, never error.
        let d = diagnose("CATEGORY: ghost\nrefactor the auth module");
        assert_eq!(d.method, DiagnoseMethod::Heuristic);
        assert_eq!(d.primary, TaskCategory::Refactor);
        // A marker that is not on the first line is plain text, not a declaration.
        let d = diagnose("refactor the auth module\nCATEGORY: docs");
        assert_eq!(d.method, DiagnoseMethod::Heuristic);
        assert_eq!(d.primary, TaskCategory::Refactor);
    }
}
