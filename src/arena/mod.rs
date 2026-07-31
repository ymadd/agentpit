//! The arena: several backends build the same thing, a human picks the better work blind.
//!
//! Every other signal agentpit learns from is a proxy. An exit code says the process finished; an
//! aggregator's grade is one model's opinion of another's; the gold bench measures a fixed suite
//! that no real task resembles. The arena asks the question those proxies stand in for — *which
//! of these did the better job?* — and puts a human on it.
//!
//! Three decisions carry the design, all borrowed from what makes LMArena's numbers mean
//! something:
//!
//! 1. **Blind.** Submissions are shown as A and B. Knowing which agent wrote a patch is a bias
//!    the judge cannot correct for by trying harder, so the names are withheld until the vote is
//!    in ([`store::Round::blind_order`]).
//! 2. **Pairwise.** People rank two things far more reliably than they score one thing, and
//!    pairwise judgements are what Bradley–Terry consumes ([`rating`]).
//! 3. **Isolated.** Each contender edits files in its own git worktree, so the human judges the
//!    work rather than the interleaving ([`worktree`]).
//!
//! **What the arena is not.** A public leaderboard rests on millions of votes; one person
//! dogfooding will produce dozens. So arena results are NOT a new top-priority profile source
//! that overrides the gold bench. Each vote is emitted as a `MemberGraded` — the same
//! high-weight, human-origin label `agentpit outcome` already produces — and flows through the
//! ordinary learning fold, where the sample gate and the `benchmarked > learned` rule still
//! apply. The leaderboard reports its bootstrap interval so a thin record reads as thin.

pub mod rating;
pub mod store;
pub mod templates;
pub mod worktree;

use std::path::Path;
use std::sync::Arc;

use anyhow::{Result, bail};
use tokio_util::sync::CancellationToken;

use crate::dispatch::{Registries, dispatch};
use crate::effort::Effort;
use crate::types::BackendId;
use store::{Round, Submission};

/// One contender: a backend at the model/effort it will actually run.
#[derive(Debug, Clone)]
pub struct Contender {
    pub backend: BackendId,
    pub model: Option<String>,
    pub effort: Option<Effort>,
}

/// Run `task` once per contender, each in its own worktree, and collect their patches.
///
/// Sequential on purpose for the MVP: N coding agents in parallel is N times the token spend and
/// N times the CPU, and the arena is already the most expensive thing agentpit can do — one round
/// is a full agentic run per contender. Failures do not abort the round; a contender that errored
/// is recorded with its error and simply is not offered for judging.
#[allow(clippy::too_many_arguments)]
pub async fn run_round(
    round_id: &str,
    run_id: &str,
    task: &str,
    cwd: &Path,
    contenders: &[Contender],
    regs: &Registries,
    cancel: CancellationToken,
    on_progress: impl Fn(&Contender, usize, usize),
) -> Result<Round> {
    if contenders.len() < 2 {
        bail!("an arena needs at least two contenders to compare");
    }
    let repo = worktree::repo_root(cwd)?;

    let mut submissions = Vec::with_capacity(contenders.len());
    for (i, contender) in contenders.iter().enumerate() {
        on_progress(contender, i + 1, contenders.len());
        submissions.push(run_one(round_id, task, &repo, contender, regs, &cancel).await);
    }

    Ok(Round {
        round_id: round_id.to_string(),
        run_id: run_id.to_string(),
        task: task.to_string(),
        cwd: cwd.display().to_string(),
        submissions,
    })
}

/// Dispatch one contender inside a fresh worktree and reduce its work to a patch. Every failure
/// mode — worktree creation, dispatch, diff capture — becomes a recorded error on the submission
/// rather than an aborted round, so one broken backend cannot cost the others their runs.
async fn run_one(
    round_id: &str,
    task: &str,
    repo: &Path,
    contender: &Contender,
    regs: &Registries,
    cancel: &CancellationToken,
) -> Submission {
    let mut submission = Submission {
        backend: contender.backend,
        model: contender.model.clone(),
        effort: contender.effort,
        patch: String::new(),
        binary_files: Vec::new(),
        summary: String::new(),
        error: None,
    };

    let tree = match worktree::create(repo, &format!("{round_id}-{}", contender.backend)) {
        Ok(t) => t,
        Err(e) => {
            submission.error = Some(format!("{e:#}"));
            return submission;
        }
    };

    let sink: Arc<dyn Fn(&str) + Send + Sync> = Arc::new(|_: &str| {});
    let outcome = dispatch(
        contender.backend,
        task,
        tree.path(),
        cancel.clone(),
        sink,
        regs,
        contender.model.as_deref(),
        contender.effort,
    )
    .await;

    match outcome {
        Ok(res) if res.auth_failed => {
            submission.error = Some("auth failure during execution".into());
        }
        Ok(res) => {
            submission.summary = res.output.trim().to_string();
            match worktree::capture_patch(&tree) {
                Ok(capture) => {
                    submission.patch = capture.patch;
                    submission.binary_files = capture.binary;
                }
                Err(e) => submission.error = Some(format!("{e:#}")),
            }
        }
        Err(e) => submission.error = Some(format!("{e:#}")),
    }
    submission
}

/// Turn the stored votes into the pairwise record Bradley–Terry consumes. Ties carry no ordering
/// signal, so they are counted by the caller but never fitted.
pub fn pairs(votes: &[store::Vote]) -> Vec<rating::Pair> {
    votes
        .iter()
        .filter(|v| !v.tie)
        .filter_map(|v| {
            Some(rating::Pair {
                winner: v.winner?,
                loser: v.loser?,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vote(winner: Option<BackendId>, loser: Option<BackendId>, tie: bool) -> store::Vote {
        store::Vote {
            round_id: "r".into(),
            ts: 0,
            winner,
            loser,
            tie,
        }
    }

    #[test]
    fn ties_and_half_recorded_votes_never_reach_the_fit() {
        let votes = vec![
            vote(Some(BackendId::Codex), Some(BackendId::Claude), false),
            vote(None, None, true),
            vote(Some(BackendId::Codex), None, false),
        ];
        let fitted = pairs(&votes);
        assert_eq!(
            fitted.len(),
            1,
            "only the decisive, complete vote is fitted"
        );
        assert_eq!(fitted[0].winner, BackendId::Codex);
    }
}
