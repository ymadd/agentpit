//! Bradley–Terry ratings over the arena's pairwise votes.
//!
//! A vote says only "A beat B on this task". Bradley–Terry turns a pile of those into one
//! strength per contender by fitting `P(A beats B) = s_A / (s_A + s_B)`, which is the same model
//! behind chess Elo and behind LMArena's leaderboard. The MM (minorization–maximization) update
//! below converges to the maximum-likelihood fit and needs no learning rate:
//!
//! ```text
//! s_i  ←  wins_i / Σ_j  n_ij / (s_i + s_j)
//! ```
//!
//! **Why the interval matters more here than on a public leaderboard.** LMArena's ratings are
//! trustworthy because they rest on millions of votes; one person dogfooding their own tools will
//! produce dozens. At that size the point estimate is mostly noise, and reporting it bare would
//! invite exactly the false confidence the arena is supposed to remove. So every rating carries a
//! bootstrap interval — resample the votes with replacement, refit, take percentiles — and the
//! caller is expected to show it. A contender whose interval spans half the scale has not been
//! measured yet, however tidy its point estimate looks.

use std::collections::BTreeMap;

use crate::types::BackendId;

/// One recorded pairwise judgement. Ties are kept out of the fit (they carry no ordering signal
/// for Bradley–Terry) but are still counted so the interval reflects the real effort spent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pair {
    pub winner: BackendId,
    pub loser: BackendId,
}

/// A contender's standing after the fit.
#[derive(Debug, Clone, PartialEq)]
pub struct Rating {
    pub backend: BackendId,
    /// Bradley–Terry strength rescaled so the field averages 50 — a leaderboard number, not a
    /// probability. Differences are what mean something.
    pub score: u8,
    /// 5th/95th percentile of the bootstrap distribution, on the same scale.
    pub low: u8,
    pub high: u8,
    pub wins: u32,
    pub losses: u32,
}

impl Rating {
    /// Comparisons this contender has actually been in.
    pub fn comparisons(&self) -> u32 {
        self.wins + self.losses
    }

    /// Whether this standing is not yet evidence. The caller marks these rather than hiding
    /// them — "we do not know yet" is the finding, and a leaderboard that omits it is worse
    /// than no leaderboard.
    ///
    /// Two independent ways to be unsettled, and either is enough: too few comparisons, or an
    /// interval wide enough that the ordering could flip. The count check is not redundant with
    /// the interval — a handful of *unanimous* votes produces a narrow interval while still
    /// being a handful of votes, and that is precisely the case most likely to be over-read.
    pub fn provisional(&self) -> bool {
        self.comparisons() < MIN_COMPARISONS
            || self.high.saturating_sub(self.low) >= PROVISIONAL_SPREAD
    }
}

/// A bootstrap interval at least this wide (on the 0–100 scale) means the comparison has not
/// separated the contenders yet.
pub const PROVISIONAL_SPREAD: u8 = 25;

/// Below this many comparisons a contender's standing is provisional however clean it looks.
pub const MIN_COMPARISONS: u32 = 10;

const MM_ITERATIONS: usize = 200;
const BOOTSTRAP_ROUNDS: usize = 400;

/// Fit strengths and bootstrap intervals over `pairs`. Deterministic: the bootstrap resamples
/// with a fixed-seed LCG rather than the thread RNG, so the same votes always produce the same
/// leaderboard (a rating that shifted on re-render would be indistinguishable from a real change).
pub fn rate(pairs: &[Pair]) -> Vec<Rating> {
    let contenders = field(pairs);
    if contenders.is_empty() {
        return Vec::new();
    }

    let point = fit(pairs, &contenders);
    let mut samples: BTreeMap<BackendId, Vec<f64>> =
        contenders.iter().map(|b| (*b, Vec::new())).collect();
    let meetings = distinct_meetings(pairs);
    let mut rng = Lcg::new(0x5EED_1234_ABCD_0001);
    for _ in 0..BOOTSTRAP_ROUNDS {
        let mut resampled: Vec<Pair> = (0..pairs.len())
            .map(|_| pairs[rng.below(pairs.len())])
            .collect();
        // A plain resample of unanimous votes is the same every round, so the interval would
        // collapse to a point and three votes would read as settled fact. One coin-flip
        // pseudo-vote per pairing is the missing prior: it dominates a thin record (3 real votes
        // vs 1 pseudo moves the fit a lot) and vanishes into a thick one (60 vs 1 moves nothing),
        // which is exactly how confidence should scale with evidence.
        for (a, b) in &meetings {
            resampled.push(match rng.below(2) {
                0 => Pair {
                    winner: *a,
                    loser: *b,
                },
                _ => Pair {
                    winner: *b,
                    loser: *a,
                },
            });
        }
        // The resample can drop a contender entirely; it keeps its point estimate for that round
        // rather than being scored 0, which would fabricate a loss it never took.
        let fitted = fit(&resampled, &contenders);
        for backend in &contenders {
            let v = fitted
                .get(backend)
                .copied()
                .or_else(|| point.get(backend).copied())
                .unwrap_or(50.0);
            samples.get_mut(backend).expect("seeded above").push(v);
        }
    }

    let mut out: Vec<Rating> = contenders
        .iter()
        .map(|backend| {
            let mut s = samples.remove(backend).unwrap_or_default();
            s.sort_by(f64::total_cmp);
            Rating {
                backend: *backend,
                score: clamp_u8(point.get(backend).copied().unwrap_or(50.0)),
                low: clamp_u8(percentile(&s, 0.05)),
                high: clamp_u8(percentile(&s, 0.95)),
                wins: pairs.iter().filter(|p| p.winner == *backend).count() as u32,
                losses: pairs.iter().filter(|p| p.loser == *backend).count() as u32,
            }
        })
        .collect();
    out.sort_by(|a, b| {
        b.score
            .cmp(&a.score)
            .then(b.wins.cmp(&a.wins))
            .then(a.backend.cmp(&b.backend))
    });
    out
}

/// Each distinct pairing that has met at least once, in a deterministic order.
fn distinct_meetings(pairs: &[Pair]) -> Vec<(BackendId, BackendId)> {
    pairs
        .iter()
        .map(|p| ordered(p.winner, p.loser))
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// Every contender that appears in at least one vote, in a deterministic order.
fn field(pairs: &[Pair]) -> Vec<BackendId> {
    let mut set: Vec<BackendId> = pairs
        .iter()
        .flat_map(|p| [p.winner, p.loser])
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    set.dedup();
    set
}

/// The MM fit, rescaled to average 50. Contenders absent from `pairs` are omitted.
fn fit(pairs: &[Pair], contenders: &[BackendId]) -> BTreeMap<BackendId, f64> {
    let mut wins: BTreeMap<BackendId, f64> = BTreeMap::new();
    let mut meetings: BTreeMap<(BackendId, BackendId), f64> = BTreeMap::new();
    for p in pairs {
        *wins.entry(p.winner).or_insert(0.0) += 1.0;
        wins.entry(p.loser).or_insert(0.0);
        *meetings.entry(ordered(p.winner, p.loser)).or_insert(0.0) += 1.0;
    }
    let present: Vec<BackendId> = contenders
        .iter()
        .copied()
        .filter(|b| wins.contains_key(b))
        .collect();
    if present.len() < 2 {
        return present.into_iter().map(|b| (b, 50.0)).collect();
    }

    let mut strength: BTreeMap<BackendId, f64> = present.iter().map(|b| (*b, 1.0)).collect();
    for _ in 0..MM_ITERATIONS {
        let prev = strength.clone();
        for i in &present {
            let denom: f64 = present
                .iter()
                .filter(|j| *j != i)
                .map(|j| {
                    let n = meetings.get(&ordered(*i, *j)).copied().unwrap_or(0.0);
                    match n {
                        0.0 => 0.0,
                        n => n / (prev[i] + prev[j]),
                    }
                })
                .sum();
            // An undefeated contender has unbounded likelihood under Bradley–Terry. Adding a
            // half-win/half-loss prior (the standard smoothing) keeps the fit finite so the
            // leaderboard stays readable instead of showing one contender at infinity.
            let w = wins.get(i).copied().unwrap_or(0.0) + 0.5;
            let d = denom + 0.5 / (prev[i] + 1.0);
            if d > 0.0 {
                strength.insert(*i, w / d);
            }
        }
        let mean: f64 = strength.values().sum::<f64>() / strength.len() as f64;
        if mean > 0.0 {
            for v in strength.values_mut() {
                *v /= mean;
            }
        }
    }

    // Map strength (mean 1.0, unbounded above) onto a bounded 0–100 display scale. The logistic
    // keeps a 10× strength advantage from running off the end of the bar.
    strength
        .into_iter()
        .map(|(b, s)| (b, 100.0 / (1.0 + (-s.max(1e-9).ln()).exp())))
        .collect()
}

fn ordered(a: BackendId, b: BackendId) -> (BackendId, BackendId) {
    match a <= b {
        true => (a, b),
        false => (b, a),
    }
}

fn percentile(sorted: &[f64], q: f64) -> f64 {
    if sorted.is_empty() {
        return 50.0;
    }
    let idx = ((sorted.len() - 1) as f64 * q).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

fn clamp_u8(v: f64) -> u8 {
    v.round().clamp(0.0, 100.0) as u8
}

/// A tiny linear congruential generator. The bootstrap needs randomness, not cryptography, and a
/// fixed seed is what makes the leaderboard reproducible.
struct Lcg(u64);

impl Lcg {
    fn new(seed: u64) -> Self {
        Self(seed)
    }

    fn below(&mut self, n: usize) -> usize {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        match n {
            0 => 0,
            n => (self.0 >> 33) as usize % n,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn beat(winner: BackendId, loser: BackendId, times: usize) -> Vec<Pair> {
        vec![Pair { winner, loser }; times]
    }

    #[test]
    fn no_votes_means_no_leaderboard() {
        assert!(rate(&[]).is_empty());
    }

    #[test]
    fn a_consistent_winner_outranks_the_field() {
        let mut pairs = beat(BackendId::Codex, BackendId::Claude, 12);
        pairs.extend(beat(BackendId::Codex, BackendId::Opencode, 10));
        pairs.extend(beat(BackendId::Claude, BackendId::Opencode, 8));

        let table = rate(&pairs);
        let order: Vec<BackendId> = table.iter().map(|r| r.backend).collect();
        assert_eq!(
            order,
            vec![BackendId::Codex, BackendId::Claude, BackendId::Opencode]
        );
        assert_eq!(table[0].wins, 22);
        assert_eq!(table[0].losses, 0);
        assert!(table[0].score > table[2].score);
    }

    #[test]
    fn a_thin_record_is_marked_provisional_and_a_thick_one_is_not() {
        // Two votes cannot separate anyone, however one-sided they were.
        let thin = rate(&beat(BackendId::Codex, BackendId::Claude, 2));
        assert!(
            thin.iter().all(|r| r.provisional()),
            "2 votes must not read as settled: {thin:?}"
        );

        // The same lopsided result, sixty times over, is evidence.
        let thick = rate(&beat(BackendId::Codex, BackendId::Claude, 60));
        assert!(
            !thick[0].provisional(),
            "60 consistent votes should settle: {:?}",
            thick[0]
        );
    }

    /// The interval must reflect how much was seen, not just how much the judges disagreed.
    /// A unanimous handful is the case most likely to be over-read, so it has to come out wider
    /// than a unanimous many.
    #[test]
    fn the_interval_narrows_as_votes_accumulate() {
        let spread = |n| {
            let r = rate(&beat(BackendId::Codex, BackendId::Claude, n));
            r[0].high.saturating_sub(r[0].low)
        };
        let (few, many) = (spread(3), spread(60));
        assert!(few > 0, "a 3-vote record cannot have a point-like interval");
        assert!(
            few > many,
            "interval should narrow with evidence: 3 votes -> {few}, 60 -> {many}"
        );
    }

    #[test]
    fn an_even_split_leaves_the_contenders_level() {
        let mut pairs = beat(BackendId::Codex, BackendId::Claude, 20);
        pairs.extend(beat(BackendId::Claude, BackendId::Codex, 20));
        let table = rate(&pairs);
        let spread = table[0].score.abs_diff(table[1].score);
        assert!(spread <= 2, "even record should tie: {table:?}");
    }

    #[test]
    fn the_leaderboard_is_reproducible_for_the_same_votes() {
        let pairs = beat(BackendId::Codex, BackendId::Claude, 7);
        assert_eq!(rate(&pairs), rate(&pairs));
    }
}
