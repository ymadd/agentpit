//! Learning status: what the routing layer has actually learned, and what it is still
//! waiting on.
//!
//! `profile show` renders the matrix as it stands; this module answers the questions the
//! matrix alone cannot: how much of it is still a hand-seeded guess, which cells are
//! accruing evidence but have not reached `min_samples` yet, how good that evidence is
//! (a human verdict is worth six exit codes), and whether learning has changed any actual
//! routing decision versus the seeded priors.
//!
//! Everything here is a pure function over already-loaded inputs. The IO (reading
//! `events.jsonl`, `profiles.toml`, the similarity store) and the assembly of
//! `LearningStatus` live in `cli::learning`, so these can be tested on synthetic data.

use std::collections::{BTreeMap, HashMap, HashSet};

use serde::Serialize;

use super::category::TaskCategory;
use super::learn::{Label, LabelSource, fold_scores};
use super::model::ProfileSource;
use super::{ProfileKey, ProfileSet};
use crate::types::BackendId;

const DAY_MS: u64 = 24 * 60 * 60 * 1000;

/// How many cells of the matrix rest on each provenance. `total` is every cell present in
/// the loaded profiles, so `seeded` is literally "still a guess".
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Coverage {
    pub total: usize,
    pub seeded: usize,
    pub learned: usize,
    pub benchmarked: usize,
}

/// Label counts split by evidence source. Named fields rather than a map so the frontend
/// reads a fixed shape and every source always renders, including at zero.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct SourceMix {
    pub outcome: u16,
    pub relative: u16,
    pub grade: u16,
    pub rerun: u16,
    pub exit: u16,
}

impl SourceMix {
    fn add(&mut self, source: LabelSource) {
        let slot = match source {
            LabelSource::Outcome => &mut self.outcome,
            LabelSource::Relative => &mut self.relative,
            LabelSource::Grade => &mut self.grade,
            LabelSource::Rerun => &mut self.rerun,
            LabelSource::Exit => &mut self.exit,
        };
        *slot = slot.saturating_add(1);
    }
}

/// The telemetry standing behind one `(backend, category)` cell: how many labels, how they
/// split, and what the fold *would* write if the sample gate let it through.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Evidence {
    pub labels: u16,
    pub good: u16,
    pub bad: u16,
    pub mix: SourceMix,
    /// The value `fold_scores` computes from these labels, gate ignored.
    pub projected: u8,
    pub projected_confidence: f32,
    /// `labels >= min_samples` — the fold would write this cell.
    pub promoted: bool,
    /// The cell is Benchmarked, so the `benchmarked > learned` gate discards this evidence
    /// however much of it accrues. Without this the progress bar would promise a promotion
    /// that can never land.
    pub outranked: bool,
    /// Most recent label timestamp (ms epoch), 0 when unknown.
    pub last_ts: u64,
}

/// One cell of the rendered matrix: the stored score plus the telemetry behind it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Cell {
    pub category: TaskCategory,
    pub value: u8,
    pub confidence: f32,
    pub samples: u16,
    pub source: ProfileSource,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub evidence: Option<Evidence>,
}

/// One backend's row of the matrix.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Row {
    /// Stable identity of this ROW, not of the backend: `codex` for the unpinned row,
    /// `codex@gpt-5.4-codex/xhigh` for a measured variant. The frontend keys and selects on
    /// this, since one backend can now occupy several rows.
    pub id: String,
    pub backend: BackendId,
    /// What this row is about beyond the backend: the pinned model / effort it was measured
    /// for. Both `None` = the unpinned row (the backend on its CLI's own defaults).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    /// The profile-level summary provenance (highest-priority cell present).
    pub summary_source: ProfileSource,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub measured_at: Option<String>,
    pub cells: Vec<Cell>,
}

/// Labels landing in one UTC day. `start_ms` is the day's boundary so the frontend can
/// format the date in the viewer's locale.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct DayBucket {
    pub start_ms: u64,
    pub labels: u16,
    pub good: u16,
    pub bad: u16,
}

/// Where one category routes now, and whether learning moved it off the seeded prior.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Pick {
    pub category: TaskCategory,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backend: Option<BackendId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub score: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<ProfileSource>,
    /// The winner was the cheaper backend within `quality_margin`, not the top scorer.
    pub cost_tiebreak: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seeded_backend: Option<BackendId>,
    /// Measured capability routes this category somewhere the seeded priors would not.
    pub changed: bool,
}

/// Count the matrix cells by provenance.
pub fn coverage(set: &ProfileSet) -> Coverage {
    let mut coverage = Coverage::default();
    for (_, profile) in set.iter() {
        for score in profile.scores.values() {
            coverage.total += 1;
            match score.source {
                ProfileSource::Seeded => coverage.seeded += 1,
                ProfileSource::Learned => coverage.learned += 1,
                ProfileSource::Benchmarked => coverage.benchmarked += 1,
            }
        }
    }
    coverage
}

/// The whole log's labels split by source — the headline "how good is this evidence".
pub fn label_mix(labels: &[Label]) -> SourceMix {
    let mut mix = SourceMix::default();
    for label in labels {
        mix.add(label.source);
    }
    mix
}

/// Per-cell evidence for every `(variant, category)` pair that has at least one label.
///
/// Keyed by [`ProfileKey`] rather than by backend so the evidence bar under a `high`-effort row
/// counts only the runs that actually ran at `high`.
///
/// `projected` reuses the real fold (`fold_scores` with the gate opened to 1) rather than
/// re-deriving the posterior here, so the number shown is the number that would be written.
pub fn evidence(
    labels: &[Label],
    min_samples: u16,
    set: &ProfileSet,
) -> BTreeMap<(ProfileKey, TaskCategory), Evidence> {
    let projected = fold_scores(labels, 1);
    let mut out: BTreeMap<(ProfileKey, TaskCategory), Evidence> = BTreeMap::new();

    for label in labels {
        let key = (
            ProfileKey::new(label.backend, label.model.clone(), label.effort),
            label.category,
        );
        let entry = out.entry(key).or_insert_with(|| Evidence {
            labels: 0,
            good: 0,
            bad: 0,
            mix: SourceMix::default(),
            projected: 0,
            projected_confidence: 0.0,
            promoted: false,
            outranked: false,
            last_ts: 0,
        });
        entry.labels = entry.labels.saturating_add(1);
        if label.success {
            entry.good = entry.good.saturating_add(1);
        } else {
            entry.bad = entry.bad.saturating_add(1);
        }
        entry.mix.add(label.source);
        entry.last_ts = entry.last_ts.max(label.ts);
    }

    for ((key, category), entry) in out.iter_mut() {
        if let Some(score) = projected.get(key).and_then(|c| c.get(category)) {
            entry.projected = score.value;
            entry.projected_confidence = score.confidence;
        }
        entry.promoted = entry.labels >= min_samples;
        entry.outranked = set
            .resolve(key.backend, key.model.as_deref(), key.effort)
            .and_then(|p| p.score(*category))
            .is_some_and(|s| s.source == ProfileSource::Benchmarked);
    }
    out
}

/// The matrix as rows, each cell carrying the telemetry behind it.
pub fn rows(
    set: &ProfileSet,
    evidence: &BTreeMap<(ProfileKey, TaskCategory), Evidence>,
) -> Vec<Row> {
    set.iter()
        .map(|(key, profile)| Row {
            id: match key.is_unpinned() {
                true => key.backend.to_string(),
                false => format!("{}@{}", key.backend, key.variant_label()),
            },
            backend: key.backend,
            model: profile.model.clone(),
            effort: profile.effort.map(|e| e.to_string()),
            summary_source: profile.source,
            measured_at: profile.measured_at.clone(),
            cells: profile
                .scores
                .iter()
                .map(|(category, score)| Cell {
                    category: *category,
                    value: score.value,
                    confidence: score.confidence,
                    samples: score.samples,
                    source: score.source,
                    evidence: evidence.get(&(key.clone(), *category)).cloned(),
                })
                .collect(),
        })
        .collect()
}

/// Labels bucketed into the last `days` UTC days, oldest first, zero-filled.
///
/// Buckets are calendar-aligned (`ts / DAY_MS`) rather than measured back from `now`, so
/// the same label always lands in the same bucket no matter when the view is opened.
pub fn timeline(labels: &[Label], now_ms: u64, days: usize) -> Vec<DayBucket> {
    let today = now_ms / DAY_MS;
    let first = today.saturating_sub(days.saturating_sub(1) as u64);
    let mut buckets: Vec<DayBucket> = (first..=today)
        .map(|day| DayBucket {
            start_ms: day * DAY_MS,
            labels: 0,
            good: 0,
            bad: 0,
        })
        .collect();

    for label in labels {
        let day = label.ts / DAY_MS;
        if day < first || day > today {
            continue; // outside the window (or ts == 0, i.e. unknown)
        }
        let bucket = &mut buckets[(day - first) as usize];
        bucket.labels = bucket.labels.saturating_add(1);
        if label.success {
            bucket.good = bucket.good.saturating_add(1);
        } else {
            bucket.bad = bucket.bad.saturating_add(1);
        }
    }
    buckets
}

/// Where each category routes under `current` versus the seeded priors.
///
/// Reproduces the deployed profile stage — the full candidate set, then the cost tiebreak
/// with the live `quality_margin` — so `changed` reflects a decision the router would
/// really make differently, not an argmax that dispatch never consults.
pub fn picks(
    current: &ProfileSet,
    seeded: &ProfileSet,
    available: &HashSet<BackendId>,
    margin: u8,
    costs: &HashMap<BackendId, u8>,
) -> Vec<Pick> {
    let cost_of = |backend: BackendId| costs.get(&backend).copied().unwrap_or(50);
    TaskCategory::ALL
        .iter()
        .map(|category| {
            let now = crate::router::pick_with_cost_tiebreak(
                &current.candidates_for(*category, available),
                margin,
                cost_of,
            );
            let before = crate::router::pick_with_cost_tiebreak(
                &seeded.candidates_for(*category, available),
                margin,
                cost_of,
            );
            Pick {
                category: *category,
                backend: now.map(|(backend, _, _)| backend),
                score: now.map(|(_, score, _)| score.value),
                source: now.map(|(_, score, _)| score.source),
                cost_tiebreak: now.is_some_and(|(_, _, tiebreak)| tiebreak),
                seeded_backend: before.map(|(backend, _, _)| backend),
                changed: match (now, before) {
                    (Some((a, _, _)), Some((b, _, _))) => a != b,
                    _ => false,
                },
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile::model::{CapabilityProfile, Score};

    fn label(
        backend: BackendId,
        category: TaskCategory,
        success: bool,
        source: LabelSource,
        ts: u64,
    ) -> Label {
        Label {
            backend,
            model: None,
            effort: None,
            category,
            success,
            source,
            task_hash: None,
            ts,
        }
    }

    fn cell(value: u8, source: ProfileSource) -> Score {
        Score {
            value,
            samples: if source == ProfileSource::Seeded {
                0
            } else {
                8
            },
            confidence: 0.6,
            source,
        }
    }

    #[test]
    fn coverage_counts_every_cell_by_its_own_provenance() {
        let mut claude = CapabilityProfile::seeded(BackendId::Claude);
        claude
            .scores
            .insert(TaskCategory::Coding, cell(80, ProfileSource::Seeded));
        claude
            .scores
            .insert(TaskCategory::Review, cell(90, ProfileSource::Learned));
        let mut codex = CapabilityProfile::seeded(BackendId::Codex);
        codex
            .scores
            .insert(TaskCategory::Debug, cell(95, ProfileSource::Benchmarked));
        let set = ProfileSet::from_profiles([claude, codex]);

        assert_eq!(
            coverage(&set),
            Coverage {
                total: 3,
                seeded: 1,
                learned: 1,
                benchmarked: 1,
            }
        );
    }

    #[test]
    fn evidence_reports_the_gate_the_mix_and_the_value_that_would_land() {
        let labels = vec![
            label(
                BackendId::Codex,
                TaskCategory::Coding,
                true,
                LabelSource::Outcome,
                10,
            ),
            label(
                BackendId::Codex,
                TaskCategory::Coding,
                true,
                LabelSource::Grade,
                20,
            ),
            label(
                BackendId::Codex,
                TaskCategory::Coding,
                false,
                LabelSource::Exit,
                30,
            ),
        ];
        let set = ProfileSet::default();
        let found = evidence(&labels, 5, &set);
        let cell = &found[&(ProfileKey::unpinned(BackendId::Codex), TaskCategory::Coding)];

        assert_eq!(cell.labels, 3);
        assert_eq!((cell.good, cell.bad), (2, 1));
        assert_eq!(
            cell.mix,
            SourceMix {
                outcome: 1,
                relative: 0,
                grade: 1,
                rerun: 0,
                exit: 1
            }
        );
        assert!(!cell.promoted, "3 labels is under min_samples=5");
        assert_eq!(cell.last_ts, 30);
        // α = 3+2+1 = 6, β = 0.5+1 = 1.5 → 100·6/7.5 = 80.
        assert_eq!(cell.projected, 80);
        // The same labels with the gate at 3 promote.
        assert!(
            evidence(&labels, 3, &set)
                [&(ProfileKey::unpinned(BackendId::Codex), TaskCategory::Coding)]
                .promoted
        );
    }

    #[test]
    fn evidence_marks_a_benchmarked_cell_as_outranked() {
        let labels = vec![label(
            BackendId::Codex,
            TaskCategory::Debug,
            true,
            LabelSource::Outcome,
            1,
        )];
        let mut codex = CapabilityProfile::seeded(BackendId::Codex);
        codex
            .scores
            .insert(TaskCategory::Debug, cell(95, ProfileSource::Benchmarked));
        let benched = ProfileSet::from_profiles([codex]);
        assert!(
            evidence(&labels, 1, &benched)
                [&(ProfileKey::unpinned(BackendId::Codex), TaskCategory::Debug)]
                .outranked
        );

        let mut codex = CapabilityProfile::seeded(BackendId::Codex);
        codex
            .scores
            .insert(TaskCategory::Debug, cell(70, ProfileSource::Learned));
        let learned = ProfileSet::from_profiles([codex]);
        assert!(
            !evidence(&labels, 1, &learned)
                [&(ProfileKey::unpinned(BackendId::Codex), TaskCategory::Debug)]
                .outranked
        );
    }

    #[test]
    fn timeline_is_calendar_aligned_zero_filled_and_windowed() {
        let now = 10 * DAY_MS + 5_000; // day 10, a few seconds in
        let labels = vec![
            label(
                BackendId::Claude,
                TaskCategory::Coding,
                true,
                LabelSource::Exit,
                10 * DAY_MS + 1,
            ),
            label(
                BackendId::Claude,
                TaskCategory::Coding,
                false,
                LabelSource::Exit,
                10 * DAY_MS + 2,
            ),
            label(
                BackendId::Claude,
                TaskCategory::Coding,
                true,
                LabelSource::Exit,
                8 * DAY_MS,
            ),
            // Older than the window, and an unknown (zero) timestamp: both dropped.
            label(
                BackendId::Claude,
                TaskCategory::Coding,
                true,
                LabelSource::Exit,
                2 * DAY_MS,
            ),
            label(
                BackendId::Claude,
                TaskCategory::Coding,
                true,
                LabelSource::Exit,
                0,
            ),
        ];
        let buckets = timeline(&labels, now, 4);

        assert_eq!(buckets.len(), 4);
        assert_eq!(buckets[0].start_ms, 7 * DAY_MS);
        assert_eq!(buckets[3].start_ms, 10 * DAY_MS);
        assert_eq!(buckets[0].labels, 0, "empty days are still rendered");
        assert_eq!(buckets[1].labels, 1);
        assert_eq!(buckets[3].labels, 2);
        assert_eq!((buckets[3].good, buckets[3].bad), (1, 1));
    }

    #[test]
    fn picks_report_where_learning_moved_a_decision() {
        let mut seeded_claude = CapabilityProfile::seeded(BackendId::Claude);
        seeded_claude
            .scores
            .insert(TaskCategory::Debug, cell(82, ProfileSource::Seeded));
        let mut seeded_codex = CapabilityProfile::seeded(BackendId::Codex);
        seeded_codex
            .scores
            .insert(TaskCategory::Debug, cell(70, ProfileSource::Seeded));
        let seeded = ProfileSet::from_profiles([seeded_claude.clone(), seeded_codex.clone()]);

        // Telemetry lifted codex above claude for Debug.
        let mut learned_codex = seeded_codex.clone();
        learned_codex
            .scores
            .insert(TaskCategory::Debug, cell(97, ProfileSource::Learned));
        let current = ProfileSet::from_profiles([seeded_claude, learned_codex]);

        let available: HashSet<BackendId> = [BackendId::Claude, BackendId::Codex].into();
        let costs: HashMap<BackendId, u8> = HashMap::new();
        let picks = picks(&current, &seeded, &available, 0, &costs);
        let debug = picks
            .iter()
            .find(|p| p.category == TaskCategory::Debug)
            .unwrap();

        assert_eq!(debug.backend, Some(BackendId::Codex));
        assert_eq!(debug.source, Some(ProfileSource::Learned));
        assert_eq!(debug.seeded_backend, Some(BackendId::Claude));
        assert!(debug.changed);

        // A category nobody scored is reported as unrouted, not as a change.
        let coding = picks
            .iter()
            .find(|p| p.category == TaskCategory::Coding)
            .unwrap();
        assert_eq!(coding.backend, None);
        assert!(!coding.changed);
    }
}
