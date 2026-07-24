//! Capability profile data model.
//!
//! A `CapabilityProfile` is one backend's row in the backend×`TaskCategory` score matrix.
//! Profiles are machine-generated (seeded heuristics, then benchmark-measured, eventually
//! telemetry-learned) and live in `profiles.toml`, separate from the hand-written
//! `config.toml`, so a benchmark run can never clobber a user's `[routes]` table.
//!
//! Everything here is immutable by contract: corrections are pure functions that return a
//! brand-new `CapabilityProfile` rather than mutating in place.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::category::TaskCategory;
use crate::types::BackendId;

/// A single backend×category competency reading on a 0–100 scale.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Score {
    /// 0–100 competency.
    pub value: u8,
    /// How many graded samples backed this value.
    pub samples: u16,
    /// 0.0–1.0 confidence in `value` (low when `samples` is small or variance is high).
    pub confidence: f32,
    /// Where this cell's value came from. Provenance is per-cell, not per-profile: a
    /// partial benchmark (say, Review only) must not freeze learned updates to the other
    /// categories. The merge gates in [`apply_benchmark`]/[`apply_learned`] compare against
    /// this field.
    pub source: ProfileSource,
}

impl Score {
    /// A seeded guess with no measured samples behind it.
    pub fn seeded(value: u8) -> Self {
        Self {
            value,
            samples: 0,
            confidence: 0.2,
            source: ProfileSource::Seeded,
        }
    }
}

/// Where a score came from. Priority for merges: benchmarked > learned > seeded.
/// Carried per cell on [`Score`]; the profile-level copy on [`CapabilityProfile`] is a
/// display/legacy summary (highest-priority cell) and no longer gates merges.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProfileSource {
    /// Hand-seeded prior, lowest trust.
    Seeded,
    /// Measured by the gold-bench harness, highest trust.
    Benchmarked,
    /// Adjusted from runtime telemetry (events.jsonl), middle trust.
    Learned,
}

impl ProfileSource {
    /// Higher wins when two sources contend for the same category. Encodes the
    /// `benchmarked > learned > seeded` rule that the merge logic is built on.
    pub fn priority(&self) -> u8 {
        match self {
            ProfileSource::Seeded => 0,
            ProfileSource::Learned => 1,
            ProfileSource::Benchmarked => 2,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            ProfileSource::Seeded => "seeded",
            ProfileSource::Benchmarked => "benchmarked",
            ProfileSource::Learned => "learned",
        }
    }
}

impl std::fmt::Display for ProfileSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Placeholder for future events-derived correction. Only a frame for now: every field is
/// optional and defaults to empty, so a profile without telemetry deserializes cleanly.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TelemetryStats {
    /// Successful runs observed for this backend.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub success: Option<u32>,
    /// Total runs observed for this backend.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total: Option<u32>,
    /// Median wall-clock duration in milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub p50_ms: Option<u64>,
    /// 95th-percentile wall-clock duration in milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub p95_ms: Option<u64>,
}

/// One backend's full capability row: per-category scores plus provenance and telemetry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CapabilityProfile {
    pub backend: BackendId,
    #[serde(default)]
    pub scores: BTreeMap<TaskCategory, Score>,
    #[serde(default)]
    pub telemetry: TelemetryStats,
    pub source: ProfileSource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub measured_at: Option<String>,
}

impl CapabilityProfile {
    /// An empty seeded profile for one backend (no scores yet).
    pub fn seeded(backend: BackendId) -> Self {
        Self {
            backend,
            scores: BTreeMap::new(),
            telemetry: TelemetryStats::default(),
            source: ProfileSource::Seeded,
            measured_at: None,
        }
    }

    /// The score for one category, if this profile has measured it.
    pub fn score(&self, category: TaskCategory) -> Option<Score> {
        self.scores.get(&category).copied()
    }
}

/// Aggregated output from the gold-bench harness: per-category scores plus the optional
/// measurement timestamp persisted by the merge layer.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct BenchmarkResult {
    #[serde(default)]
    pub scores: BTreeMap<TaskCategory, Score>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub measured_at: Option<String>,
}

/// Merge incoming cells into a copy of `base.scores`, gating **per cell**: an incoming
/// reading overwrites a category only when `incoming` priority >= that cell's own source
/// priority (missing cells always accept). The stored cell's `source` is forced to
/// `incoming` regardless of what the caller put in the `Score`, so provenance can never
/// be spoofed by a constructor site.
fn merge_cells(
    base: &CapabilityProfile,
    incoming_scores: &BTreeMap<TaskCategory, Score>,
    incoming: ProfileSource,
) -> BTreeMap<TaskCategory, Score> {
    let mut scores = base.scores.clone();
    for (category, score) in incoming_scores {
        let allowed = scores
            .get(category)
            .is_none_or(|existing| incoming.priority() >= existing.source.priority());
        if allowed {
            scores.insert(
                *category,
                Score {
                    source: incoming,
                    ..*score
                },
            );
        }
    }
    scores
}

/// The profile-level summary source: the highest-priority provenance present in any cell.
/// Kept for display and so an older binary reading the file stays conservative; merges
/// gate on the per-cell source instead.
fn summary_source(scores: &BTreeMap<TaskCategory, Score>) -> ProfileSource {
    scores
        .values()
        .map(|s| s.source)
        .max_by_key(ProfileSource::priority)
        .unwrap_or(ProfileSource::Seeded)
}

/// Fold a benchmark result into a profile, returning a brand-new profile (the input is
/// never mutated).
///
/// Gating is per cell (see [`merge_cells`]): `Benchmarked` is the highest priority, so in
/// practice every measured category wins here. Categories the benchmark did not cover keep
/// their existing scores *and* their existing provenance — a partial bench must not freeze
/// learned updates to the categories it never measured.
pub fn apply_benchmark(base: &CapabilityProfile, result: &BenchmarkResult) -> CapabilityProfile {
    let scores = merge_cells(base, &result.scores, ProfileSource::Benchmarked);
    CapabilityProfile {
        backend: base.backend,
        source: summary_source(&scores),
        scores,
        telemetry: base.telemetry.clone(),
        measured_at: result
            .measured_at
            .clone()
            .or_else(|| base.measured_at.clone()),
    }
}

/// Fold telemetry-learned scores into a profile, returning a brand-new profile. Same
/// per-cell gate as [`apply_benchmark`] with `Learned` as the incoming source: it refreshes
/// seeded or learned cells but never overwrites a benchmarked cell, and categories the fold
/// did not cover keep their existing scores.
pub fn apply_learned(
    base: &CapabilityProfile,
    learned: &BTreeMap<TaskCategory, Score>,
) -> CapabilityProfile {
    let scores = merge_cells(base, learned, ProfileSource::Learned);
    CapabilityProfile {
        backend: base.backend,
        source: summary_source(&scores),
        scores,
        telemetry: base.telemetry.clone(),
        measured_at: base.measured_at.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_priority_orders_benchmarked_over_learned_over_seeded() {
        assert!(ProfileSource::Benchmarked.priority() > ProfileSource::Learned.priority());
        assert!(ProfileSource::Learned.priority() > ProfileSource::Seeded.priority());
    }

    #[test]
    fn apply_benchmark_is_immutable_and_overwrites_seeded() {
        let mut base = CapabilityProfile::seeded(BackendId::Claude);
        base.scores.insert(TaskCategory::Coding, Score::seeded(40));
        base.scores.insert(TaskCategory::Docs, Score::seeded(55));

        let mut result = BenchmarkResult::default();
        result.scores.insert(
            TaskCategory::Coding,
            Score {
                value: 90,
                samples: 12,
                confidence: 0.8,
                source: ProfileSource::Seeded, // spoof attempt: merge must force Benchmarked
            },
        );
        result.measured_at = Some("2026-06-30T00:00:00Z".into());

        let updated = apply_benchmark(&base, &result);

        // input untouched
        assert_eq!(base.source, ProfileSource::Seeded);
        assert_eq!(base.score(TaskCategory::Coding).unwrap().value, 40);

        // benchmark overwrote the measured category, preserved the untouched one
        assert_eq!(updated.source, ProfileSource::Benchmarked);
        let coding = updated.score(TaskCategory::Coding).unwrap();
        assert_eq!(coding.value, 90);
        assert_eq!(coding.source, ProfileSource::Benchmarked);
        let docs = updated.score(TaskCategory::Docs).unwrap();
        assert_eq!(docs.value, 55);
        assert_eq!(docs.source, ProfileSource::Seeded);
        assert_eq!(updated.measured_at.as_deref(), Some("2026-06-30T00:00:00Z"));
    }

    #[test]
    fn apply_learned_refreshes_seeded_but_never_a_benchmarked_cell() {
        let learned_cell = Score {
            value: 85,
            samples: 12,
            confidence: 0.85,
            source: ProfileSource::Learned,
        };
        let learned: BTreeMap<TaskCategory, Score> = [(TaskCategory::Coding, learned_cell)].into();

        // Seeded cells: the learned cell wins, untouched cells survive, summary flips.
        let mut seeded = CapabilityProfile::seeded(BackendId::Gemini);
        seeded
            .scores
            .insert(TaskCategory::Coding, Score::seeded(60));
        seeded.scores.insert(TaskCategory::Docs, Score::seeded(70));
        let updated = apply_learned(&seeded, &learned);
        assert_eq!(updated.source, ProfileSource::Learned);
        assert_eq!(updated.score(TaskCategory::Coding).unwrap().value, 85);
        assert_eq!(
            updated.score(TaskCategory::Coding).unwrap().source,
            ProfileSource::Learned
        );
        assert_eq!(updated.score(TaskCategory::Docs).unwrap().value, 70);

        // Benchmarked cell: it alone stays frozen.
        let mut benched = CapabilityProfile::seeded(BackendId::Gemini);
        benched.source = ProfileSource::Benchmarked;
        benched.scores.insert(
            TaskCategory::Coding,
            Score {
                value: 90,
                samples: 24,
                confidence: 0.8,
                source: ProfileSource::Benchmarked,
            },
        );
        let untouched = apply_learned(&benched, &learned);
        assert_eq!(untouched.source, ProfileSource::Benchmarked);
        assert_eq!(untouched.score(TaskCategory::Coding).unwrap().value, 90);
    }

    /// Review finding M8: a partial benchmark (one category measured) must not freeze
    /// learned updates to the categories it never touched.
    #[test]
    fn partial_benchmark_does_not_freeze_learned_updates_to_other_cells() {
        let mut base = CapabilityProfile::seeded(BackendId::Codex);
        base.scores.insert(TaskCategory::Coding, Score::seeded(60));

        // Bench only Review.
        let mut result = BenchmarkResult::default();
        result.scores.insert(
            TaskCategory::Review,
            Score {
                value: 92,
                samples: 24,
                confidence: 0.8,
                source: ProfileSource::Benchmarked,
            },
        );
        let benched = apply_benchmark(&base, &result);
        assert_eq!(benched.source, ProfileSource::Benchmarked);

        // Learned still updates Coding (seeded cell), while Review stays benchmarked.
        let learned: BTreeMap<TaskCategory, Score> = [
            (
                TaskCategory::Coding,
                Score {
                    value: 81,
                    samples: 10,
                    confidence: 0.7,
                    source: ProfileSource::Learned,
                },
            ),
            (
                TaskCategory::Review,
                Score {
                    value: 10,
                    samples: 10,
                    confidence: 0.7,
                    source: ProfileSource::Learned,
                },
            ),
        ]
        .into();
        let merged = apply_learned(&benched, &learned);
        let coding = merged.score(TaskCategory::Coding).unwrap();
        assert_eq!(coding.value, 81);
        assert_eq!(coding.source, ProfileSource::Learned);
        let review = merged.score(TaskCategory::Review).unwrap();
        assert_eq!(review.value, 92);
        assert_eq!(review.source, ProfileSource::Benchmarked);
        // Summary stays at the highest-priority cell present.
        assert_eq!(merged.source, ProfileSource::Benchmarked);
    }

    #[test]
    fn telemetry_defaults_to_all_none() {
        let t = TelemetryStats::default();
        assert!(t.success.is_none());
        assert!(t.total.is_none());
        assert!(t.p50_ms.is_none());
        assert!(t.p95_ms.is_none());
    }
}
