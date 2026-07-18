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
}

impl Score {
    /// A seeded guess with no measured samples behind it.
    pub fn seeded(value: u8) -> Self {
        Self {
            value,
            samples: 0,
            confidence: 0.2,
        }
    }
}

/// Where a profile's numbers came from. Priority for merges: benchmarked > learned > seeded.
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

/// Fold a benchmark result into a profile, returning a brand-new profile (the input is
/// never mutated).
///
/// Merge foundation: a benchmark reading overwrites a category only when its source
/// (`Benchmarked`) has priority >= the profile's current source. Benchmarked is the
/// highest priority, so in practice it always wins here — but the rule is written out so
/// the later `learned`/`seeded` merges reuse the same gate. Categories the benchmark did
/// not cover keep their existing scores.
pub fn apply_benchmark(base: &CapabilityProfile, result: &BenchmarkResult) -> CapabilityProfile {
    let incoming = ProfileSource::Benchmarked;
    let overwrite = incoming.priority() >= base.source.priority();

    let mut scores = base.scores.clone();
    if overwrite {
        for (category, score) in &result.scores {
            scores.insert(*category, *score);
        }
    }

    CapabilityProfile {
        backend: base.backend,
        scores,
        telemetry: base.telemetry.clone(),
        source: if overwrite { incoming } else { base.source },
        measured_at: result
            .measured_at
            .clone()
            .or_else(|| base.measured_at.clone()),
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
            },
        );
        result.measured_at = Some("2026-06-30T00:00:00Z".into());

        let updated = apply_benchmark(&base, &result);

        // input untouched
        assert_eq!(base.source, ProfileSource::Seeded);
        assert_eq!(base.score(TaskCategory::Coding).unwrap().value, 40);

        // benchmark overwrote the measured category, preserved the untouched one
        assert_eq!(updated.source, ProfileSource::Benchmarked);
        assert_eq!(updated.score(TaskCategory::Coding).unwrap().value, 90);
        assert_eq!(updated.score(TaskCategory::Docs).unwrap().value, 55);
        assert_eq!(updated.measured_at.as_deref(), Some("2026-06-30T00:00:00Z"));
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
