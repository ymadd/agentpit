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

use std::num::NonZeroUsize;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Result, bail};
use futures_util::{StreamExt, stream};
use tokio_util::sync::CancellationToken;

use crate::dispatch::{Registries, dispatch};
use crate::effort::Effort;
use crate::types::BackendId;
use store::{Round, Submission};

/// Conservative default because every concurrent contender is a full agentic run, multiplying
/// both token spend and local CPU use.
pub const DEFAULT_CONCURRENCY: NonZeroUsize = NonZeroUsize::new(2).unwrap();

/// One contender: a backend at the model/effort it will actually run.
#[derive(Debug, Clone)]
pub struct Contender {
    pub backend: BackendId,
    pub model: Option<String>,
    pub effort: Option<Effort>,
}

/// A contender's visible lifecycle within a concurrent round.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Progress {
    Started,
    Finished { failed: bool },
}

/// Run `task` once per contender, each in its own worktree, and collect their patches.
///
/// At most `concurrency` contenders run at once. Completion order does not affect storage order:
/// submissions are restored to the order of `contenders`, keeping blind labels deterministic.
/// Failures do not abort the round; a contender that errored is recorded with its error and simply
/// is not offered for judging.
#[allow(clippy::too_many_arguments)]
pub async fn run_round(
    round_id: &str,
    run_id: &str,
    task: &str,
    cwd: &Path,
    contenders: &[Contender],
    regs: &Registries,
    cancel: CancellationToken,
    concurrency: usize,
    on_progress: impl Fn(&Contender, usize, usize, Progress),
) -> Result<Round> {
    if contenders.len() < 2 {
        bail!("an arena needs at least two contenders to compare");
    }
    if concurrency == 0 {
        bail!("arena concurrency must be at least 1");
    }
    let repo = worktree::repo_root(cwd)?;

    let total = contenders.len();
    let on_progress = &on_progress;
    let mut indexed = stream::iter(contenders.iter().enumerate())
        .map(|(index, contender)| {
            let repo = &repo;
            let cancel = &cancel;
            async move {
                on_progress(contender, index + 1, total, Progress::Started);
                let submission =
                    run_one(round_id, index, task, repo, contender, regs, cancel).await;
                on_progress(
                    contender,
                    index + 1,
                    total,
                    Progress::Finished {
                        failed: submission.error.is_some(),
                    },
                );
                (index, submission)
            }
        })
        .buffer_unordered(concurrency.min(total))
        .collect::<Vec<_>>()
        .await;

    // `buffer_unordered` yields in completion order. Restore input order so persisted submissions
    // — and therefore their blind labels — never depend on which backend happened to finish first.
    indexed.sort_unstable_by_key(|(index, _)| *index);
    let submissions = indexed
        .into_iter()
        .map(|(_, submission)| submission)
        .collect();

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
    contender_index: usize,
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

    // A cancelled round must not start queued contenders after the in-flight dispatches unwind.
    if cancel.is_cancelled() {
        submission.error = Some("cancelled before execution".into());
        return submission;
    }

    // Include the stable slice index as well as the backend. Besides making the tag genuinely
    // per-contender, this prevents duplicate backend entries from racing for the same checkout.
    let tree = match worktree::create(
        repo,
        &format!("{round_id}-{}-{}", contender_index + 1, contender.backend),
    ) {
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
    use std::process::Command;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Duration;

    use super::*;
    use crate::exec::{ExecAdapter, ExecSpec};

    static NEXT_ROUND: AtomicU64 = AtomicU64::new(0);

    struct ScriptedExec {
        backend: BackendId,
        script: String,
        env: Vec<(String, String)>,
    }

    impl ScriptedExec {
        fn new(backend: BackendId, script: impl Into<String>) -> Self {
            Self {
                backend,
                script: script.into(),
                env: Vec::new(),
            }
        }

        fn with_env(mut self, key: &str, value: String) -> Self {
            self.env.push((key.to_string(), value));
            self
        }
    }

    impl ExecAdapter for ScriptedExec {
        fn id(&self) -> BackendId {
            self.backend
        }

        fn build_spec(
            &self,
            _task: &str,
            _model: Option<&str>,
            _effort: Option<Effort>,
        ) -> ExecSpec {
            ExecSpec {
                command: "sh".into(),
                args: vec!["-c".into(), self.script.clone()],
                env: self.env.clone(),
                stdin_input: None,
            }
        }
    }

    fn init_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for args in [
            &["init", "-q"][..],
            &["config", "user.email", "arena@example.com"][..],
            &["config", "user.name", "Arena Test"][..],
        ] {
            git(dir.path(), args);
        }
        std::fs::write(dir.path().join("base.txt"), "base\n").unwrap();
        git(dir.path(), &["add", "."]);
        git(dir.path(), &["commit", "-qm", "base"]);
        dir
    }

    fn git(cwd: &Path, args: &[&str]) {
        let output = Command::new("git")
            .args(args)
            .current_dir(cwd)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn contender(backend: BackendId) -> Contender {
        Contender {
            backend,
            model: None,
            effort: None,
        }
    }

    fn round_id(label: &str) -> String {
        format!(
            "test-{label}-{}-{}",
            std::process::id(),
            NEXT_ROUND.fetch_add(1, Ordering::Relaxed)
        )
    }

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

    #[tokio::test]
    async fn submissions_keep_contender_order_when_completion_order_differs() {
        let repo = init_repo();
        let release = repo.path().join("release-slow-contender");
        let mut regs = Registries::empty();
        regs.execs.insert(
            BackendId::Claude,
            Box::new(
                ScriptedExec::new(
                    BackendId::Claude,
                    "while [ ! -f \"$ARENA_TEST_RELEASE\" ]; do sleep 0.01; done; \
                     printf slow > result.txt",
                )
                .with_env("ARENA_TEST_RELEASE", release.display().to_string()),
            ),
        );
        regs.execs.insert(
            BackendId::Codex,
            Box::new(ScriptedExec::new(
                BackendId::Codex,
                "printf fast > result.txt",
            )),
        );
        let contenders = vec![contender(BackendId::Claude), contender(BackendId::Codex)];
        let finishes = Arc::new(Mutex::new(Vec::new()));
        let finishes_for_progress = finishes.clone();
        let release_for_progress = release.clone();
        let id = round_id("order");

        let round = tokio::time::timeout(
            Duration::from_secs(10),
            run_round(
                &id,
                "test-run",
                "make a result",
                repo.path(),
                &contenders,
                &regs,
                CancellationToken::new(),
                2,
                move |contender, _, _, progress| {
                    if matches!(progress, Progress::Finished { .. }) {
                        finishes_for_progress
                            .lock()
                            .unwrap()
                            .push(contender.backend);
                        if contender.backend == BackendId::Codex {
                            std::fs::write(&release_for_progress, "go").unwrap();
                        }
                    }
                },
            ),
        )
        .await
        .expect("the concurrently released round should finish")
        .unwrap();

        assert_eq!(
            *finishes.lock().unwrap(),
            vec![BackendId::Codex, BackendId::Claude],
            "the test must actually complete in reverse order"
        );
        assert_eq!(
            round
                .submissions
                .iter()
                .map(|submission| submission.backend)
                .collect::<Vec<_>>(),
            vec![BackendId::Claude, BackendId::Codex],
            "stored order follows the contender slice, not completion order"
        );
        assert!(round.submissions[0].patch.contains("slow"));
        assert!(round.submissions[1].patch.contains("fast"));
    }

    #[tokio::test]
    async fn one_contender_failure_does_not_prevent_siblings_being_recorded() {
        let repo = init_repo();
        let mut regs = Registries::empty();
        regs.execs.insert(
            BackendId::Claude,
            Box::new(ScriptedExec::new(
                BackendId::Claude,
                "printf boom >&2; exit 7",
            )),
        );
        regs.execs.insert(
            BackendId::Codex,
            Box::new(ScriptedExec::new(
                BackendId::Codex,
                "printf success > result.txt",
            )),
        );
        let contenders = vec![contender(BackendId::Claude), contender(BackendId::Codex)];

        let round = run_round(
            &round_id("failure"),
            "test-run",
            "make a result",
            repo.path(),
            &contenders,
            &regs,
            CancellationToken::new(),
            2,
            |_, _, _, _| {},
        )
        .await
        .unwrap();

        assert_eq!(round.submissions.len(), 2);
        assert!(
            round.submissions[0]
                .error
                .as_deref()
                .is_some_and(|error| error.contains("exited with code 7")),
            "the failed contender is retained with its error: {:?}",
            round.submissions[0]
        );
        assert!(round.submissions[1].error.is_none());
        assert!(round.submissions[1].patch.contains("success"));
    }

    #[tokio::test]
    async fn cancelling_a_round_stops_every_in_flight_contender() {
        let repo = init_repo();
        let started_a = repo.path().join("started-a");
        let started_b = repo.path().join("started-b");
        let mut regs = Registries::empty();
        for (backend, marker) in [
            (BackendId::Claude, started_a.clone()),
            (BackendId::Codex, started_b.clone()),
        ] {
            regs.execs.insert(
                backend,
                Box::new(
                    ScriptedExec::new(backend, "touch \"$ARENA_TEST_STARTED\"; exec sleep 30")
                        .with_env("ARENA_TEST_STARTED", marker.display().to_string()),
                ),
            );
        }
        let contenders = vec![contender(BackendId::Claude), contender(BackendId::Codex)];
        let cancel = CancellationToken::new();
        let cancel_when_started = cancel.clone();
        let monitor = tokio::spawn(async move {
            loop {
                if started_a.exists() && started_b.exists() {
                    cancel_when_started.cancel();
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        });

        let round = tokio::time::timeout(
            Duration::from_secs(10),
            run_round(
                &round_id("cancel"),
                "test-run",
                "wait forever",
                repo.path(),
                &contenders,
                &regs,
                cancel,
                2,
                |_, _, _, _| {},
            ),
        )
        .await
        .expect("cancellation should stop both long-running contenders")
        .unwrap();
        monitor.await.unwrap();

        assert!(
            round
                .submissions
                .iter()
                .all(|submission| submission.error.is_some()),
            "both cancelled contenders should be retained as failures: {:?}",
            round.submissions
        );
    }
}
