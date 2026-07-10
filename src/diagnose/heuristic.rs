//! Heuristic task classifier.
//!
//! [`classify`] is a pure function over [`TaskFeatures`]: it builds a per-category weighted
//! score, picks the argmax, and turns the score vector into a `confidence` via a
//! (temperature-scaled) softmax. `LongContext` is not keyword-driven — it is promoted only
//! when the token estimate clears a threshold *and* no other category shows a real signal.

use std::collections::BTreeMap;

use crate::profile::TaskCategory;

use super::features::{TaskFeatures, verb_category};

/// Weight a single matched category keyword contributes.
const KEYWORD_WEIGHT: f32 = 1.0;
/// Weight a single matched command verb contributes.
const VERB_WEIGHT: f32 = 0.5;
/// Bonus toward `Coding` when the task carries a code fence.
const CODE_BLOCK_WEIGHT: f32 = 0.5;

/// Token estimate above which a task may be promoted to `LongContext`.
/// Self-contained to the diagnose layer (independent of the router's config threshold).
pub const LONG_CONTEXT_TOKEN_THRESHOLD: u64 = 4_000;
/// A categorical signal at or above this is "real" and blocks `LongContext` promotion.
const WEAK_SIGNAL_CEILING: f32 = 1.0;

/// Temperature scale applied before softmax. >1 sharpens a clear winner so a single solid
/// keyword match lands above the LLM-assist confidence floor.
const SOFTMAX_SCALE: f32 = 2.0;

/// Confidence returned for a task with no categorical signal at all (kept below the
/// LLM-assist threshold so a future assist layer would take over).
const NEUTRAL_CONFIDENCE: f32 = 0.1;

/// Classify a task from its features, returning `(primary_category, confidence)`.
/// Pure: reads `features`, mutates nothing, returns owned values.
pub fn classify(features: &TaskFeatures) -> (TaskCategory, f32) {
    let scores = category_scores(features);
    let (best_cat, best_score) = argmax(&scores);

    // LongContext is a promoted feature, not a keyword category: only when the task is long
    // AND no other category shows a real signal (a long "refactor this huge file" stays
    // Refactor rather than being swallowed by LongContext).
    if features.token_estimate > LONG_CONTEXT_TOKEN_THRESHOLD && best_score < WEAK_SIGNAL_CEILING {
        return (
            TaskCategory::LongContext,
            long_context_confidence(features.token_estimate),
        );
    }

    if best_score <= 0.0 {
        // No signal — neutral default at low confidence.
        return (TaskCategory::Coding, NEUTRAL_CONFIDENCE);
    }

    (best_cat, softmax_confidence(&scores, best_cat))
}

/// Build the per-category weighted score map (all categories present, zero-filled).
fn category_scores(features: &TaskFeatures) -> BTreeMap<TaskCategory, f32> {
    let mut scores: BTreeMap<TaskCategory, f32> =
        TaskCategory::ALL.iter().map(|c| (*c, 0.0)).collect();

    for (cat, _) in &features.matched_keywords {
        *scores.entry(*cat).or_insert(0.0) += KEYWORD_WEIGHT;
    }
    for verb in &features.verbs {
        if let Some(cat) = verb_category(verb) {
            *scores.entry(cat).or_insert(0.0) += VERB_WEIGHT;
        }
    }
    if features.has_code_block {
        *scores.entry(TaskCategory::Coding).or_insert(0.0) += CODE_BLOCK_WEIGHT;
    }

    scores
}

/// Argmax over the score map. Ties break by category declaration order in
/// `TaskCategory::ALL`, never by map iteration quirks, so the result is deterministic.
fn argmax(scores: &BTreeMap<TaskCategory, f32>) -> (TaskCategory, f32) {
    TaskCategory::ALL
        .iter()
        .map(|cat| (*cat, scores.get(cat).copied().unwrap_or(0.0)))
        .fold(
            (TaskCategory::Coding, f32::NEG_INFINITY),
            |best, (cat, score)| {
                if score > best.1 { (cat, score) } else { best }
            },
        )
}

/// Softmax probability of `best` over the (temperature-scaled) score vector.
/// Numerically stable (subtracts the max before exponentiating).
fn softmax_confidence(scores: &BTreeMap<TaskCategory, f32>, best: TaskCategory) -> f32 {
    let scaled: Vec<(TaskCategory, f32)> = TaskCategory::ALL
        .iter()
        .map(|cat| {
            (
                *cat,
                scores.get(cat).copied().unwrap_or(0.0) * SOFTMAX_SCALE,
            )
        })
        .collect();

    let max = scaled
        .iter()
        .map(|(_, s)| *s)
        .fold(f32::NEG_INFINITY, f32::max);

    let exps: Vec<(TaskCategory, f32)> = scaled
        .iter()
        .map(|(cat, s)| (*cat, (s - max).exp()))
        .collect();

    let sum: f32 = exps.iter().map(|(_, e)| *e).sum();
    let best_exp = exps
        .iter()
        .find(|(cat, _)| *cat == best)
        .map(|(_, e)| *e)
        .unwrap_or(0.0);

    if sum > 0.0 { best_exp / sum } else { 0.0 }
}

/// Confidence for a `LongContext` promotion, scaling with how far the task clears the
/// threshold and saturating below 1.0.
fn long_context_confidence(token_estimate: u64) -> f32 {
    let over = token_estimate as f32 / LONG_CONTEXT_TOKEN_THRESHOLD as f32;
    (0.5 + 0.1 * over).min(0.95)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diagnose::features::extract;

    #[test]
    fn refactor_task_classifies_as_refactor() {
        let (cat, conf) = classify(&extract(
            "refactor the payment module to remove duplication",
        ));
        assert_eq!(cat, TaskCategory::Refactor);
        assert!(conf > 0.5, "confidence too low: {conf}");
    }

    #[test]
    fn security_audit_classifies_as_security_review() {
        let (cat, _) = classify(&extract("audit security of the login handler"));
        assert_eq!(cat, TaskCategory::SecurityReview);
    }

    #[test]
    fn long_weak_task_promotes_to_long_context() {
        let long = "alpha beta gamma delta ".repeat(2_000);
        let f = extract(&long);
        assert!(f.token_estimate > LONG_CONTEXT_TOKEN_THRESHOLD);
        let (cat, _) = classify(&f);
        assert_eq!(cat, TaskCategory::LongContext);
    }

    #[test]
    fn long_but_strong_signal_does_not_promote() {
        // A long task that still clearly asks for a refactor stays Refactor.
        let long = format!("refactor this: {}", "alpha beta gamma ".repeat(2_000));
        let f = extract(&long);
        assert!(f.token_estimate > LONG_CONTEXT_TOKEN_THRESHOLD);
        let (cat, _) = classify(&f);
        assert_eq!(cat, TaskCategory::Refactor);
    }

    #[test]
    fn no_signal_short_task_is_low_confidence() {
        let (_, conf) = classify(&extract("alpha beta gamma"));
        assert!(conf < 0.55, "expected low confidence, got {conf}");
    }

    #[test]
    fn softmax_confidence_is_a_probability() {
        let conf = softmax_confidence(
            &category_scores(&extract("refactor this")),
            TaskCategory::Refactor,
        );
        assert!((0.0..=1.0).contains(&conf));
    }
}
