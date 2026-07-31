//! Score aggregation → `profiles.toml` merge.
//!
//! Two layers, deliberately split:
//!
//! - **Pure aggregation** ([`aggregate`]): a backend's per-task gold scores — each a fraction
//!   in `0..=1` tagged with its [`TaskCategory`] — collapse *per category* into one
//!   [`Score`]: `value` is the mean scaled to `0..=100`, `samples` is the bucket size, and
//!   `confidence` rises with the sample count and falls as the per-task scores spread out (see
//!   [`confidence`]). These per-category scores plus a **caller-supplied** `measured_at` build a
//!   [`BenchmarkResult`]. This function never reads the clock — the timestamp is an argument so
//!   the same inputs always produce the same result.
//!
//! - **Thin I/O wrapper** ([`merge_into_profiles`]): load the existing [`ProfileSet`], fold the
//!   result into the target backend's profile via [`apply_benchmark`] (which promotes the
//!   profile's source to [`Benchmarked`](crate::profile::ProfileSource::Benchmarked) and
//!   overwrites the seeded prior), then save the rebuilt set back.
//!
//! Immutable throughout: aggregation borrows its input and returns a fresh result; the merge
//! never mutates the loaded set — it constructs a brand-new one with the target backend's
//! profile replaced and writes that.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::Result;

use crate::effort::Effort;
use crate::profile::category::TaskCategory;
use crate::profile::model::{
    BenchmarkResult, CapabilityProfile, ProfileSource, Score, apply_benchmark,
};
use crate::profile::store::{load_profiles, save_profiles};
use crate::profile::{ProfileKey, ProfileSet};
use crate::types::BackendId;

/// Largest variance achievable by fractions in `[0, 1]` (a 50/50 split between 0 and 1 gives
/// mean 0.5, variance 0.25). Used to normalize a bucket's spread into `[0, 1]`.
const MAX_FRACTION_VARIANCE: f64 = 0.25;

/// One graded gold task: which category it exercised and the candidate's `0..=1` score on it.
/// The aggregation buckets these by [`category`](Self::category).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GradedTask {
    /// The category this gold task measures.
    pub category: TaskCategory,
    /// Candidate score on this single task; clamped to `0..=1` during aggregation.
    pub score: f64,
}

impl GradedTask {
    /// Convenience constructor.
    pub fn new(category: TaskCategory, score: f64) -> Self {
        Self { category, score }
    }
}

/// Aggregate a backend's graded gold-task scores into a [`BenchmarkResult`].
///
/// Tasks are bucketed by category; each bucket becomes one [`Score`] (see [`score_for_bucket`]).
/// `measured_at` is carried through verbatim onto the result — this function never generates a
/// timestamp itself. So are `measured_model` / `measured_effort`, the provenance of WHAT was
/// measured: a gold-bench score belongs to a (backend, model, effort) triple, not to the backend
/// name alone. Pure: borrows `graded`, returns a fresh result.
pub fn aggregate(
    graded: &[GradedTask],
    measured_at: Option<String>,
    measured_model: Option<String>,
    measured_effort: Option<Effort>,
) -> BenchmarkResult {
    let mut buckets: BTreeMap<TaskCategory, Vec<f64>> = BTreeMap::new();
    for task in graded {
        buckets
            .entry(task.category)
            .or_default()
            .push(task.score.clamp(0.0, 1.0));
    }

    let scores = buckets
        .into_iter()
        .map(|(category, fractions)| (category, score_for_bucket(&fractions)))
        .collect();

    BenchmarkResult {
        scores,
        measured_at,
        measured_model,
        measured_effort,
    }
}

/// Collapse one category's pass-fractions into a [`Score`]: `value = round(mean * 100)` clamped
/// to `0..=100`, `samples` = bucket size, `confidence` from [`confidence`]. An empty bucket
/// yields a zeroed, zero-confidence score (defensive — `aggregate` never builds empty buckets).
fn score_for_bucket(fractions: &[f64]) -> Score {
    let samples = u16::try_from(fractions.len()).unwrap_or(u16::MAX);
    if samples == 0 {
        return Score {
            value: 0,
            samples: 0,
            confidence: 0.0,
            source: ProfileSource::Benchmarked,
        };
    }

    let n = f64::from(samples);
    let mean = fractions.iter().sum::<f64>() / n;
    let variance = fractions.iter().map(|f| (f - mean).powi(2)).sum::<f64>() / n;

    Score {
        value: (mean * 100.0).round().clamp(0.0, 100.0) as u8,
        samples,
        confidence: confidence(samples, variance),
        source: ProfileSource::Benchmarked,
    }
}

/// Confidence in a category's value: a saturating base term in `samples` (0.55 → 0.95) scaled
/// down by how much the per-task scores spread. `variance` (of fractions in `[0, 1]`, so at most
/// [`MAX_FRACTION_VARIANCE`]) is normalized to `[0, 1]`; a maximally split bucket keeps only half
/// its base confidence. Returns 0.0 for an empty bucket.
fn confidence(samples: u16, variance: f64) -> f32 {
    if samples == 0 {
        return 0.0;
    }
    let base = (0.5 + 0.05 * f64::from(samples)).min(0.95);
    let spread = (variance / MAX_FRACTION_VARIANCE).clamp(0.0, 1.0);
    (base * (1.0 - 0.5 * spread)).clamp(0.0, 1.0) as f32
}

/// Thin I/O wrapper: load `profiles.toml` at `profiles_path`, fold `result` into `backend`'s
/// profile via [`apply_benchmark`], and save the rebuilt set back. Returns the merged profile so
/// the caller can report it.
///
/// A missing file is not an error — [`load_profiles`] returns the seeded matrix, so a first-run
/// merge promotes a seeded profile to benchmarked. Immutable: the loaded set is never mutated; a
/// fresh set with the target row replaced is constructed and written.
///
/// WHICH row is folded into comes from the result's own `measured_model` / `measured_effort`: a
/// bench of codex at `xhigh` updates the `codex@…/xhigh` row and leaves every other rung alone.
/// A variant measured for the first time starts from the backend's UNPINNED row (the seeded
/// priors) so the categories the suite did not cover still have sensible values, rather than
/// appearing as a hole.
pub fn merge_into_profiles(
    backend: BackendId,
    result: &BenchmarkResult,
    profiles_path: &Path,
) -> Result<CapabilityProfile> {
    let set = load_profiles(Some(profiles_path))?;
    let key = ProfileKey::new(
        backend,
        result.measured_model.clone(),
        result.measured_effort,
    );
    let base = set
        .resolve(backend, key.model.as_deref(), key.effort)
        .cloned()
        // `resolve` may have fallen back to the unpinned row; re-stamp the identity so the merge
        // writes the variant rather than overwriting the fallback it borrowed values from.
        .map(|p| CapabilityProfile {
            model: key.model.clone(),
            effort: key.effort,
            ..p
        })
        .unwrap_or_else(|| CapabilityProfile::for_variant(backend, key.model.clone(), key.effort));

    let merged = apply_benchmark(&base, result);

    let next = with_profile(&set, merged.clone());
    save_profiles(&next, profiles_path)?;
    Ok(merged)
}

/// Rebuild a [`ProfileSet`] with `profile`'s ROW replaced by `profile` — the row being its
/// `(backend, model, effort)` key, so sibling rungs of the same backend survive untouched.
/// Pure: reads `set`, returns a brand-new set; the input is untouched.
fn with_profile(set: &ProfileSet, profile: CapabilityProfile) -> ProfileSet {
    let key = profile.key();
    let others = set
        .iter()
        .filter(|(k, _)| **k != key)
        .map(|(_, p)| p.clone());
    ProfileSet::from_profiles(others.chain(std::iter::once(profile)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile::ProfileSource;
    use tempfile::tempdir;

    fn task(category: TaskCategory, score: f64) -> GradedTask {
        GradedTask::new(category, score)
    }

    #[test]
    fn aggregate_buckets_by_category_with_expected_value_and_samples() {
        // Two coding tasks (100% and 50% → mean 75) and one perfect review task.
        let graded = vec![
            task(TaskCategory::Coding, 1.0),
            task(TaskCategory::Coding, 0.5),
            task(TaskCategory::Review, 1.0),
        ];

        let result = aggregate(&graded, Some("2026-06-30T00:00:00Z".into()), None, None);

        let coding = result.scores.get(&TaskCategory::Coding).unwrap();
        assert_eq!(coding.value, 75);
        assert_eq!(coding.samples, 2);

        let review = result.scores.get(&TaskCategory::Review).unwrap();
        assert_eq!(review.value, 100);
        assert_eq!(review.samples, 1);

        // measured_at is carried through verbatim from the caller.
        assert_eq!(result.measured_at.as_deref(), Some("2026-06-30T00:00:00Z"));
    }

    #[test]
    fn aggregate_never_generates_a_timestamp() {
        // With no measured_at supplied the result carries none — the function does not invent one.
        let result = aggregate(&[task(TaskCategory::Docs, 1.0)], None, None, None);
        assert!(result.measured_at.is_none());
    }

    #[test]
    fn aggregate_clamps_out_of_range_fractions() {
        // Scores outside [0,1] are clamped before averaging: clamp(1.5)=1, clamp(-0.5)=0 → mean .5.
        let graded = vec![
            task(TaskCategory::Planning, 1.5),
            task(TaskCategory::Planning, -0.5),
        ];
        let result = aggregate(&graded, None, None, None);
        assert_eq!(
            result.scores.get(&TaskCategory::Planning).unwrap().value,
            50
        );
    }

    #[test]
    fn confidence_drops_as_scores_spread_out() {
        // Same sample count, different spread: a tight bucket (both 1.0, zero variance) should be
        // more confident than a maximally split bucket (1.0 and 0.0, variance 0.25).
        let tight = aggregate(
            &[
                task(TaskCategory::Coding, 1.0),
                task(TaskCategory::Coding, 1.0),
            ],
            None,
            None,
            None,
        );
        let split = aggregate(
            &[
                task(TaskCategory::Coding, 1.0),
                task(TaskCategory::Coding, 0.0),
            ],
            None,
            None,
            None,
        );

        let tight_conf = tight.scores.get(&TaskCategory::Coding).unwrap().confidence;
        let split_conf = split.scores.get(&TaskCategory::Coding).unwrap().confidence;
        assert!(
            split_conf < tight_conf,
            "spread bucket conf {split_conf} should be below tight bucket conf {tight_conf}"
        );
        // A maximally split bucket keeps exactly half its base confidence.
        assert!((split_conf - tight_conf * 0.5).abs() < 1e-6);
    }

    #[test]
    fn confidence_climbs_with_more_samples() {
        let one = aggregate(&[task(TaskCategory::Debug, 1.0)], None, None, None);
        let many = aggregate(
            &[
                task(TaskCategory::Debug, 1.0),
                task(TaskCategory::Debug, 1.0),
                task(TaskCategory::Debug, 1.0),
            ],
            None,
            None,
            None,
        );
        let c1 = one.scores.get(&TaskCategory::Debug).unwrap().confidence;
        let c3 = many.scores.get(&TaskCategory::Debug).unwrap().confidence;
        assert!(
            c3 > c1,
            "more samples should raise confidence: {c1} -> {c3}"
        );
    }

    #[test]
    fn merge_promotes_seeded_to_benchmarked_overwrites_and_persists() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("profiles.toml");

        // No file yet → load returns the seeded matrix. Claude's seeded coding prior is 88.
        let seeded = load_profiles(Some(&path)).unwrap();
        assert_eq!(
            seeded.get(BackendId::Claude).unwrap().source,
            ProfileSource::Seeded
        );
        assert_eq!(
            seeded
                .get(BackendId::Claude)
                .unwrap()
                .score(TaskCategory::Coding)
                .unwrap()
                .value,
            88
        );
        let backend_count = seeded.len();

        let result = aggregate(
            &[
                task(TaskCategory::Coding, 1.0),
                task(TaskCategory::Coding, 1.0),
            ],
            Some("2026-06-30T00:00:00Z".into()),
            Some("opus".into()),
            Some(Effort::XHigh),
        );
        let merged = merge_into_profiles(BackendId::Claude, &result, &path).unwrap();

        // Returned profile reflects the promotion + overwrite.
        assert_eq!(merged.source, ProfileSource::Benchmarked);
        assert_eq!(merged.score(TaskCategory::Coding).unwrap().value, 100);
        assert_eq!(merged.measured_at.as_deref(), Some("2026-06-30T00:00:00Z"));
        // The merged row records WHAT was measured, not just when.
        assert_eq!(merged.model.as_deref(), Some("opus"));
        assert_eq!(merged.effort, Some(Effort::XHigh));

        // save → load round-trips the merged row to the isolated path — and lands on the
        // VARIANT, addressed by the triple it was measured for.
        let reloaded = load_profiles(Some(&path)).unwrap();
        let claude = reloaded
            .resolve(BackendId::Claude, Some("opus"), Some(Effort::XHigh))
            .unwrap();
        assert_eq!(claude.source, ProfileSource::Benchmarked);
        assert_eq!(claude.score(TaskCategory::Coding).unwrap().value, 100);
        assert_eq!(claude.score(TaskCategory::Coding).unwrap().samples, 2);
        assert_eq!(claude.measured_at.as_deref(), Some("2026-06-30T00:00:00Z"));

        // The unpinned row is UNTOUCHED: measuring opus at xhigh says nothing about claude on
        // its CLI defaults, so that row keeps its seeded prior of 88.
        let unpinned = reloaded.get(BackendId::Claude).unwrap();
        assert_eq!(unpinned.source, ProfileSource::Seeded);
        assert_eq!(unpinned.score(TaskCategory::Coding).unwrap().value, 88);

        // Nor does a run at one rung answer for another: `low` has never been measured, so it
        // falls back to the unpinned prior rather than borrowing the xhigh number.
        let at_low = reloaded
            .resolve(BackendId::Claude, Some("opus"), Some(Effort::Low))
            .unwrap();
        assert_eq!(at_low.score(TaskCategory::Coding).unwrap().value, 88);

        // The merge is immutable over the rest of the set: every other backend survives, seeded,
        // and the new variant is an ADDITIONAL row.
        assert_eq!(reloaded.len(), backend_count + 1);
        let codex = reloaded.get(BackendId::Codex).unwrap();
        assert_eq!(codex.source, ProfileSource::Seeded);
    }

    #[test]
    fn merge_leaves_uncovered_categories_intact() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("profiles.toml");

        // Seeded Claude has a Docs prior; merging only a Coding score must not drop it.
        let seeded = load_profiles(Some(&path)).unwrap();
        let docs_prior = seeded
            .get(BackendId::Claude)
            .unwrap()
            .score(TaskCategory::Docs);

        let result = aggregate(&[task(TaskCategory::Coding, 1.0)], None, None, None);
        merge_into_profiles(BackendId::Claude, &result, &path).unwrap();

        let reloaded = load_profiles(Some(&path)).unwrap();
        assert_eq!(
            reloaded
                .get(BackendId::Claude)
                .unwrap()
                .score(TaskCategory::Docs),
            docs_prior
        );
    }
}
