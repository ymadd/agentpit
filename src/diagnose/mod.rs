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

/// Diagnose a task. Pure in A1: runs the heuristic layer and returns its verdict.
///
/// When `confidence < LLM_ASSIST_CONFIDENCE_THRESHOLD` a future LLM-assisted layer
/// (`src/diagnose/llm.rs`) is meant to take over and return a `LlmAssisted` diagnosis;
/// that layer is intentionally not implemented in A1, so we return the heuristic verdict
/// unchanged here.
pub fn diagnose(task: &str) -> Diagnosis {
    let features = extract(task);
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
}
