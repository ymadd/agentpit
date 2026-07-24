//! kNN similarity routing: route a task to the backend that won similar past tasks.
//!
//! This module is split so the store and selection logic stay dependency-free and always
//! compiled/tested; only the embedding model (`embed.rs`, fastembed/ONNX) sits behind the
//! `similarity` cargo feature. Samples live in `<state>/routes.jsonl`, one JSON object per
//! line, written by `agentpit profile learn` (which also computes the embeddings in bulk —
//! the dispatch path only ever embeds the single query).

#[cfg(feature = "similarity")]
pub mod embed;

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::config::SimilaritySection;
use crate::types::BackendId;

/// Samples older than this are dropped on the next `profile learn` rewrite.
pub const SAMPLE_TTL_MS: u64 = 180 * 24 * 60 * 60 * 1000;

/// One labelled routing outcome with its task embedding.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouteSample {
    pub task_hash: String,
    pub embedding: Vec<f32>,
    pub backend: BackendId,
    /// "good" | "bad" (mirrors `OutcomeLabel` wire values).
    pub label: String,
    /// The source label's fold weight (human verdict 3.0 … exit status 0.5), so weak
    /// evidence stays weak in the kNN vote too. Older stores default to 1.0.
    #[serde(default = "default_sample_weight")]
    pub weight: f32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    pub ts: u64,
}

fn default_sample_weight() -> f32 {
    1.0
}

impl RouteSample {
    pub fn is_good(&self) -> bool {
        self.label == "good"
    }
}

/// Path to the sample store (`<state>/routes.jsonl`).
pub fn routes_path() -> PathBuf {
    crate::events::state_dir().join("routes.jsonl")
}

/// Parse the jsonl store; unparseable lines are skipped (best-effort, like events.jsonl).
pub fn parse_samples(raw: &str) -> Vec<RouteSample> {
    raw.lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

pub fn serialize_samples(samples: &[RouteSample]) -> String {
    let mut out = String::new();
    for sample in samples {
        if let Ok(line) = serde_json::to_string(sample) {
            out.push_str(&line);
            out.push('\n');
        }
    }
    out
}

/// Cosine similarity; 0.0 when either vector is degenerate (zero norm / length mismatch).
pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let (mut dot, mut norm_a, mut norm_b) = (0.0f32, 0.0f32, 0.0f32);
    for (x, y) in a.iter().zip(b) {
        dot += x * y;
        norm_a += x * x;
        norm_b += y * y;
    }
    if norm_a == 0.0 || norm_b == 0.0 {
        return 0.0;
    }
    dot / (norm_a.sqrt() * norm_b.sqrt())
}

/// The similarity route's verdict for one query embedding.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SimilarityPick {
    pub backend: BackendId,
    /// Best neighbour similarity for the winning backend, in `[0, 1]`.
    pub sim: f32,
    /// Similar samples backing the winner.
    pub samples: usize,
}

/// Pick a backend from the k nearest sufficiently-similar samples, or `None` when the
/// evidence is too thin (design: fall through to the profile route rather than guess):
/// the winner needs `min_samples` similar samples and a `margin` win-rate lead over the
/// runner-up backend. Wins/losses are similarity-weighted so a 0.95 neighbour counts for
/// more than a 0.80 one.
pub fn pick_backend(
    query: &[f32],
    samples: &[RouteSample],
    cfg: &SimilaritySection,
    available: impl Fn(BackendId) -> bool,
) -> Option<SimilarityPick> {
    let mut hits: Vec<(f32, &RouteSample)> = samples
        .iter()
        .filter(|s| available(s.backend))
        .map(|s| (cosine(query, &s.embedding), s))
        .filter(|(sim, _)| *sim >= cfg.min_sim)
        .collect();
    hits.sort_by(|(a, _), (b, _)| b.total_cmp(a));
    hits.truncate(cfg.k);
    if hits.is_empty() {
        return None;
    }

    #[derive(Default)]
    struct Stat {
        wins: f32,
        losses: f32,
        count: usize,
        best_sim: f32,
    }
    let mut stats: std::collections::BTreeMap<BackendId, Stat> = Default::default();
    for (sim, sample) in &hits {
        let stat = stats.entry(sample.backend).or_default();
        // Votes carry both closeness and label strength: a 0.95-similar human verdict
        // (weight 3.0) outweighs several 0.80-similar exit statuses (weight 0.5).
        let vote = sim * sample.weight.max(0.0);
        if sample.is_good() {
            stat.wins += vote;
        } else {
            stat.losses += vote;
        }
        stat.count += 1;
        stat.best_sim = stat.best_sim.max(*sim);
    }

    let win_rate = |s: &Stat| {
        let total = s.wins + s.losses;
        if total <= 0.0 { 0.0 } else { s.wins / total }
    };
    let (best_backend, best_stat) = stats
        .iter()
        .max_by(|(_, a), (_, b)| win_rate(a).total_cmp(&win_rate(b)))?;
    let runner_up_rate = stats
        .iter()
        .filter(|(backend, _)| *backend != best_backend)
        .map(|(_, s)| win_rate(s))
        .fold(0.0f32, f32::max);

    // Three gates: enough similar samples, a real lead over the runner-up, and a positive
    // track record in absolute terms — being the least-bad option is not a reason to route
    // here (fall through to the profile stage instead).
    if best_stat.count < cfg.min_samples
        || win_rate(best_stat) - runner_up_rate < cfg.margin
        || win_rate(best_stat) < 0.5
    {
        return None;
    }
    Some(SimilarityPick {
        backend: *best_backend,
        sim: best_stat.best_sim,
        samples: best_stat.count,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(backend: BackendId, label: &str, embedding: Vec<f32>) -> RouteSample {
        RouteSample {
            task_hash: "h".into(),
            embedding,
            backend,
            label: label.into(),
            weight: 1.0,
            category: None,
            ts: 1,
        }
    }

    fn cfg() -> SimilaritySection {
        SimilaritySection::default()
    }

    #[test]
    fn cosine_basics() {
        assert!((cosine(&[1.0, 0.0], &[1.0, 0.0]) - 1.0).abs() < 1e-6);
        assert!(cosine(&[1.0, 0.0], &[0.0, 1.0]).abs() < 1e-6);
        assert_eq!(cosine(&[1.0], &[1.0, 2.0]), 0.0, "length mismatch");
        assert_eq!(cosine(&[0.0, 0.0], &[1.0, 0.0]), 0.0, "zero norm");
    }

    #[test]
    fn five_good_samples_on_one_backend_win() {
        let samples: Vec<RouteSample> = (0..5)
            .map(|_| sample(BackendId::Codex, "good", vec![1.0, 0.0]))
            .collect();
        let pick = pick_backend(&[1.0, 0.0], &samples, &cfg(), |_| true).unwrap();
        assert_eq!(pick.backend, BackendId::Codex);
        assert_eq!(pick.samples, 5);
        assert!(pick.sim > 0.99);
    }

    #[test]
    fn thin_or_contested_evidence_falls_through() {
        // Too few samples.
        let samples = vec![
            sample(BackendId::Codex, "good", vec![1.0, 0.0]),
            sample(BackendId::Codex, "good", vec![1.0, 0.0]),
        ];
        assert!(pick_backend(&[1.0, 0.0], &samples, &cfg(), |_| true).is_none());

        // Enough samples but no win-rate lead: both backends all-good.
        let mut samples: Vec<RouteSample> = (0..3)
            .map(|_| sample(BackendId::Codex, "good", vec![1.0, 0.0]))
            .collect();
        samples.extend((0..3).map(|_| sample(BackendId::Gemini, "good", vec![1.0, 0.0])));
        assert!(pick_backend(&[1.0, 0.0], &samples, &cfg(), |_| true).is_none());

        // Dissimilar samples never count.
        let samples: Vec<RouteSample> = (0..5)
            .map(|_| sample(BackendId::Codex, "good", vec![0.0, 1.0]))
            .collect();
        assert!(pick_backend(&[1.0, 0.0], &samples, &cfg(), |_| true).is_none());
    }

    #[test]
    fn bad_labels_and_availability_steer_the_pick() {
        // Codex failed these tasks, Gemini succeeded them → Gemini wins.
        let mut samples: Vec<RouteSample> = (0..3)
            .map(|_| sample(BackendId::Codex, "bad", vec![1.0, 0.0]))
            .collect();
        samples.extend((0..3).map(|_| sample(BackendId::Gemini, "good", vec![1.0, 0.0])));
        let pick = pick_backend(&[1.0, 0.0], &samples, &cfg(), |_| true).unwrap();
        assert_eq!(pick.backend, BackendId::Gemini);

        // An offline winner is never picked.
        assert!(pick_backend(&[1.0, 0.0], &samples, &cfg(), |b| b != BackendId::Gemini).is_none());
    }

    #[test]
    fn samples_round_trip_and_junk_lines_skip() {
        let samples = vec![sample(BackendId::Claude, "good", vec![0.5, 0.5])];
        let raw = format!("{}junk line\n", serialize_samples(&samples));
        let parsed = parse_samples(&raw);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].backend, BackendId::Claude);
        assert!(parsed[0].is_good());

        // A pre-weight store line still parses, defaulting to weight 1.0.
        let legacy =
            r#"{"task_hash":"h","embedding":[1.0],"backend":"codex","label":"good","ts":1}"#;
        let parsed = parse_samples(legacy);
        assert_eq!(parsed[0].weight, 1.0);
    }

    #[test]
    fn label_weight_shifts_the_vote() {
        // One weight-3.0 human "bad" on Codex vs three weight-0.5 exit-ok "good"s: the
        // weighted loss (3.0) dominates the weighted wins (1.5) → win-rate 1.5/4.5 = 0.33,
        // so Codex must NOT be picked even though good samples outnumber bad ones.
        let mut samples: Vec<RouteSample> = (0..3)
            .map(|_| {
                let mut s = sample(BackendId::Codex, "good", vec![1.0, 0.0]);
                s.weight = 0.5;
                s
            })
            .collect();
        let mut human_bad = sample(BackendId::Codex, "bad", vec![1.0, 0.0]);
        human_bad.weight = 3.0;
        samples.push(human_bad);
        assert!(pick_backend(&[1.0, 0.0], &samples, &cfg(), |_| true).is_none());
    }
}
