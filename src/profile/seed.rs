//! Hand-seeded capability priors.
//!
//! These are the `source = Seeded` numbers the router falls back to before any benchmark or
//! telemetry has run. They encode each backend's well-known strengths as a low-confidence
//! starting point: a benchmarked or learned reading (higher `ProfileSource` priority) always
//! supersedes them once measured.
//!
//! Confidence is deliberately low (0.4) — high enough to break ties between backends that
//! have never been measured, low enough that the first real benchmark overrides cleanly.

use super::ProfileSet;
use super::category::TaskCategory;
use super::model::{CapabilityProfile, ProfileSource, Score};
use crate::types::BackendId;

/// Confidence attached to every seeded score: a soft prior, not a measurement.
const SEED_CONFIDENCE: f32 = 0.4;

/// A seeded score: a value with no samples behind it and a low fixed confidence.
fn seed(value: u8) -> Score {
    Score {
        value,
        samples: 0,
        confidence: SEED_CONFIDENCE,
        source: ProfileSource::Seeded,
    }
}

/// Build one backend's seeded profile from a fixed (category, value) table. The order of
/// `rows` is irrelevant — scores live in a `BTreeMap` keyed by category.
fn seeded_profile(backend: BackendId, rows: &[(TaskCategory, u8)]) -> CapabilityProfile {
    let scores = rows
        .iter()
        .map(|(category, value)| (*category, seed(*value)))
        .collect();
    CapabilityProfile {
        backend,
        scores,
        telemetry: Default::default(),
        source: ProfileSource::Seeded,
        measured_at: None,
        model: None,
        effort: None,
    }
}

/// The hand-seeded capability matrix for the backends with known strengths.
///
/// Strengths encoded (per design §1.4):
/// - **claude** — coding / refactor.
/// - **codex** — review / adversarial review.
/// - **antigravity** — long context / docs.
/// - **opencode** — middling all-rounder (no standout column).
///
/// Backends without a row here (goose, copilot) simply have no seeded scores and are picked
/// up only once benchmarked.
pub fn seeded_profiles() -> ProfileSet {
    use TaskCategory::*;

    let claude = seeded_profile(
        BackendId::Claude,
        &[
            (Coding, 88),
            (Refactor, 86),
            (Review, 78),
            (AdversarialReview, 72),
            (SecurityReview, 75),
            (Debug, 82),
            (Explain, 80),
            (Docs, 78),
            (Planning, 80),
            (LongContext, 70),
        ],
    );

    let codex = seeded_profile(
        BackendId::Codex,
        &[
            (Coding, 82),
            (Refactor, 78),
            (Review, 86),
            (AdversarialReview, 88),
            (SecurityReview, 84),
            (Debug, 80),
            (Explain, 72),
            (Docs, 68),
            (Planning, 72),
            (LongContext, 64),
        ],
    );

    let antigravity = seeded_profile(
        BackendId::Antigravity,
        &[
            (Coding, 74),
            (Refactor, 72),
            (Review, 74),
            (AdversarialReview, 70),
            (SecurityReview, 72),
            (Debug, 72),
            (Explain, 80),
            (Docs, 86),
            (Planning, 78),
            (LongContext, 88),
        ],
    );

    let opencode = seeded_profile(
        BackendId::Opencode,
        &[
            (Coding, 66),
            (Refactor, 64),
            (Review, 64),
            (AdversarialReview, 62),
            (SecurityReview, 62),
            (Debug, 64),
            (Explain, 66),
            (Docs, 64),
            (Planning, 64),
            (LongContext, 64),
        ],
    );

    ProfileSet::from_profiles([claude, codex, antigravity, opencode])
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn all_backends() -> HashSet<BackendId> {
        BackendId::ALL.iter().copied().collect()
    }

    #[test]
    fn seeds_the_four_known_backends() {
        let set = seeded_profiles();
        assert_eq!(set.len(), 4);
        for b in [
            BackendId::Claude,
            BackendId::Codex,
            BackendId::Antigravity,
            BackendId::Opencode,
        ] {
            assert!(set.get(b).is_some(), "missing seed for {b:?}");
        }
        // Backends without a known prior stay unseeded.
        assert!(set.get(BackendId::Goose).is_none());
        assert!(set.get(BackendId::Copilot).is_none());
    }

    #[test]
    fn every_seeded_score_is_low_confidence_seeded_source() {
        let set = seeded_profiles();
        for (_, profile) in set.iter() {
            assert_eq!(profile.source, ProfileSource::Seeded);
            for category in TaskCategory::ALL {
                let score = profile.score(*category).expect("full row seeded");
                assert_eq!(score.samples, 0);
                assert!((score.confidence - SEED_CONFIDENCE).abs() < f32::EPSILON);
            }
        }
    }

    #[test]
    fn encodes_known_strengths_as_argmax_winners() {
        let set = seeded_profiles();
        let available = all_backends();

        // claude leads coding and refactor.
        assert_eq!(
            set.best_for(TaskCategory::Coding, &available).unwrap().0,
            BackendId::Claude
        );
        assert_eq!(
            set.best_for(TaskCategory::Refactor, &available).unwrap().0,
            BackendId::Claude
        );

        // codex leads review and adversarial review.
        assert_eq!(
            set.best_for(TaskCategory::Review, &available).unwrap().0,
            BackendId::Codex
        );
        assert_eq!(
            set.best_for(TaskCategory::AdversarialReview, &available)
                .unwrap()
                .0,
            BackendId::Codex
        );

        // antigravity leads docs; long context is led by the long-context specialists.
        assert_eq!(
            set.best_for(TaskCategory::Docs, &available).unwrap().0,
            BackendId::Antigravity
        );
        assert_eq!(
            set.best_for(TaskCategory::LongContext, &available)
                .unwrap()
                .0,
            BackendId::Antigravity
        );
    }
}
