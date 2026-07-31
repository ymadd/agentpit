//! On-disk record of arena rounds and the votes cast on them.
//!
//! A round is written when the contenders finish and read back later when the human sits down to
//! judge — the two are deliberately separate steps, because a round takes minutes and nobody
//! should have to watch it. Rounds live under `<state>/arena/<round_id>.json`; votes append to
//! `<state>/arena/votes.jsonl`, the same append-only shape as the event log.
//!
//! Submissions are stored **anonymised at read time, not at write time**: the file records which
//! backend produced which patch (that is the whole point of rating them), and
//! [`Round::blind_order`] is what hides the names while a comparison is on screen.

use std::fs;
use std::io::Write;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::effort::Effort;
use crate::types::BackendId;

/// One contender's attempt at the round's task.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Submission {
    pub backend: BackendId,
    /// What the contender was actually running — an arena result belongs to a
    /// `(backend, model, effort)` triple exactly like a benchmark score does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<Effort>,
    /// The unified diff of everything it changed. Empty = it changed nothing.
    pub patch: String,
    /// Binary paths left out of the patch (build artifacts, mostly). Shown to the judge so a
    /// submission never looks smaller than it was without saying why.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub binary_files: Vec<String>,
    /// Its final message, kept for context when a patch alone is hard to read.
    pub summary: String,
    /// Set when the dispatch itself failed; such a contender is excluded from voting.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl Submission {
    /// Only a contender that ran AND produced changes can be judged. An empty patch is not a
    /// weak entry to be voted down, it is a missing entry: pairing it against real work would
    /// hand the opponent a free win and corrupt the rating.
    pub fn judgeable(&self) -> bool {
        self.error.is_none() && !self.patch.trim().is_empty()
    }
}

/// One arena round: a task and every contender's attempt at it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Round {
    pub round_id: String,
    /// The run id the dispatches were logged under, so a round joins up with the event log.
    pub run_id: String,
    pub task: String,
    pub cwd: String,
    pub submissions: Vec<Submission>,
}

impl Round {
    /// The judgeable submissions in a deterministic but name-free order, as `(label, index)`.
    ///
    /// The label is a letter, never the backend id: knowing which agent wrote a patch is exactly
    /// the bias a blind comparison exists to remove, and it is the one bias the judge cannot
    /// correct for by trying harder. The order is derived from the round id so the same round
    /// always presents the same way, without the letters tracking the backend order in the file.
    pub fn blind_order(&self) -> Vec<(char, usize)> {
        let mut idx: Vec<usize> = self
            .submissions
            .iter()
            .enumerate()
            .filter(|(_, s)| s.judgeable())
            .map(|(i, _)| i)
            .collect();
        let seed: u32 = self.round_id.bytes().map(u32::from).sum();
        // A rotation is enough to decouple letters from file order while staying reproducible.
        if !idx.is_empty() {
            let by = seed as usize % idx.len();
            idx.rotate_left(by);
        }
        idx.into_iter()
            .enumerate()
            .map(|(n, i)| ((b'A' + (n as u8 % 26)) as char, i))
            .collect()
    }

    /// Every unordered pair of judgeable submissions — the comparisons a human is asked for.
    pub fn matchups(&self) -> Vec<(usize, usize)> {
        let order = self.blind_order();
        let mut out = Vec::new();
        for a in 0..order.len() {
            for b in (a + 1)..order.len() {
                out.push((order[a].1, order[b].1));
            }
        }
        out
    }
}

/// One human judgement. `tie` records that the judge saw both and could not separate them —
/// dropping those silently would make the record look more decisive than the judge was.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Vote {
    pub round_id: String,
    pub ts: u64,
    pub winner: Option<BackendId>,
    pub loser: Option<BackendId>,
    #[serde(default)]
    pub tie: bool,
}

pub fn arena_dir() -> PathBuf {
    crate::events::state_dir().join("arena")
}

fn votes_path() -> PathBuf {
    arena_dir().join("votes.jsonl")
}

fn round_path(round_id: &str) -> PathBuf {
    arena_dir().join(format!("{round_id}.json"))
}

pub fn save_round(round: &Round) -> Result<PathBuf> {
    let dir = arena_dir();
    fs::create_dir_all(&dir).with_context(|| format!("failed to create {}", dir.display()))?;
    let path = round_path(&round.round_id);
    let body =
        serde_json::to_string_pretty(round).context("failed to serialize the arena round")?;
    fs::write(&path, body).with_context(|| format!("failed to write {}", path.display()))?;
    Ok(path)
}

pub fn load_round(round_id: &str) -> Result<Round> {
    let path = round_path(round_id);
    let body = fs::read_to_string(&path)
        .with_context(|| format!("no arena round at {}", path.display()))?;
    serde_json::from_str(&body).with_context(|| format!("failed to parse {}", path.display()))
}

/// Round ids newest-first, by file modification time.
pub fn list_rounds() -> Vec<String> {
    let Ok(entries) = fs::read_dir(arena_dir()) else {
        return Vec::new();
    };
    let mut rounds: Vec<(std::time::SystemTime, String)> = entries
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
        .filter_map(|e| {
            let id = e.path().file_stem()?.to_string_lossy().into_owned();
            let ts = e.metadata().ok()?.modified().ok()?;
            Some((ts, id))
        })
        .collect();
    rounds.sort_by_key(|(ts, _)| std::cmp::Reverse(*ts));
    rounds.into_iter().map(|(_, id)| id).collect()
}

/// Append one vote. Best-effort like the event log: a telemetry write must never be the thing
/// that loses a judgement the human already made, so the caller reports the error and moves on.
pub fn append_vote(vote: &Vote) -> Result<()> {
    let dir = arena_dir();
    fs::create_dir_all(&dir).with_context(|| format!("failed to create {}", dir.display()))?;
    let line = serde_json::to_string(vote).context("failed to serialize the vote")?;
    let mut f = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(votes_path())
        .with_context(|| format!("failed to open {}", votes_path().display()))?;
    writeln!(f, "{line}").context("failed to append the vote")
}

/// Every recorded vote. Unparseable lines are skipped, matching the event log's tolerance for a
/// half-written final line.
pub fn load_votes() -> Vec<Vote> {
    let Ok(body) = fs::read_to_string(votes_path()) else {
        return Vec::new();
    };
    body.lines()
        .filter_map(|l| serde_json::from_str::<Vote>(l).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn submission(backend: BackendId, patch: &str) -> Submission {
        Submission {
            backend,
            model: None,
            effort: None,
            patch: patch.into(),
            binary_files: Vec::new(),
            summary: String::new(),
            error: None,
        }
    }

    fn round(subs: Vec<Submission>) -> Round {
        Round {
            round_id: "r-abc".into(),
            run_id: "run-1".into(),
            task: "build it".into(),
            cwd: "/tmp".into(),
            submissions: subs,
        }
    }

    #[test]
    fn an_empty_or_failed_submission_is_not_judgeable() {
        assert!(submission(BackendId::Codex, "+ real work").judgeable());
        assert!(!submission(BackendId::Codex, "   \n").judgeable());
        let failed = Submission {
            error: Some("timed out".into()),
            ..submission(BackendId::Codex, "+ work")
        };
        assert!(!failed.judgeable());
    }

    #[test]
    fn blind_order_covers_only_judgeable_entries_and_hides_nothing_else() {
        let r = round(vec![
            submission(BackendId::Claude, "+a"),
            submission(BackendId::Codex, ""), // produced nothing
            submission(BackendId::Opencode, "+c"),
        ]);
        let order = r.blind_order();
        assert_eq!(
            order.len(),
            2,
            "the empty submission is not offered for judging"
        );
        let labels: Vec<char> = order.iter().map(|(l, _)| *l).collect();
        assert_eq!(labels, vec!['A', 'B']);
        // Same round, same presentation — a reshuffle between renders would make the judge
        // think they were looking at something new.
        assert_eq!(r.blind_order(), order);
    }

    #[test]
    fn matchups_are_every_unordered_pair_of_judgeable_entries() {
        let r = round(vec![
            submission(BackendId::Claude, "+a"),
            submission(BackendId::Codex, "+b"),
            submission(BackendId::Opencode, "+c"),
        ]);
        assert_eq!(r.matchups().len(), 3);
        let single = round(vec![submission(BackendId::Claude, "+a")]);
        assert!(
            single.matchups().is_empty(),
            "one contender is nothing to compare"
        );
    }
}
