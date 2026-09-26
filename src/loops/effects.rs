//! Step effects: the processes behind agent and check steps (design §4.2, §11).
//!
//! An effect never touches the journal. It runs only after its `step_started` is durable,
//! reports what happened as [`EffectMsg`]s, and the runner turns the report into records.
//! Every payload a record will name (the answer blob) is written and fsynced here, before
//! the report, so `step_finished` never points at a file that is not on disk.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use agentpit_events::loops::*;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::paths::{answer_rel, check_log_rel, output_log_rel};
use super::proc::{group_alive, signal_group};
use super::prompt::{excerpt, file_tail, parse_verdict};
use crate::dispatch::{Registries, dispatch_continuing};
use crate::effort::Effort;
use crate::events::{LegStatus, RunLogger, output_streamer};
use crate::types::BackendId;

/// How much of a check's output goes into the journal (`log_tail`) and into feedback.
pub const LOG_TAIL_BYTES: usize = 4 * 1024;

/// A step's final report.
#[derive(Debug, Clone, PartialEq)]
pub struct StepDone {
    pub step_id: String,
    pub outcome: Outcome,
    pub elapsed_ms: u64,
    pub exit_code: Option<i32>,
    pub verdict: Option<Verdict>,
    pub output: Option<BlobRef>,
    pub excerpt: Option<String>,
    pub log_tail: Option<String>,
    pub error: Option<String>,
    pub feedback: Vec<FeedbackItem>,
}

impl StepDone {
    fn new(step_id: &str, outcome: Outcome, started: Instant) -> StepDone {
        StepDone {
            step_id: step_id.to_string(),
            outcome,
            elapsed_ms: started.elapsed().as_millis() as u64,
            exit_code: None,
            verdict: None,
            output: None,
            excerpt: None,
            log_tail: None,
            error: None,
            feedback: vec![],
        }
    }

    fn failed(mut self, node: &str, outcome: Outcome, error: String) -> StepDone {
        self.outcome = outcome;
        self.feedback.push(FeedbackItem {
            source: FeedbackSource::Error,
            node: node.to_string(),
            step_id: Some(self.step_id.clone()),
            summary: clamp_text(error.lines().next().unwrap_or_default(), MAX_SHORT_BYTES),
            detail: (error.contains('\n')).then(|| clamp_text(&error, MAX_DETAIL_BYTES)),
        });
        self.error = Some(clamp_text(&error, MAX_DETAIL_BYTES));
        self
    }
}

/// What an effect tells the runner.
#[derive(Debug)]
pub enum EffectMsg {
    /// A check's process group (for killing orphans after a runner crash). The check is
    /// held until `ack` fires, which the runner does once `step_spawned` is durable.
    Spawned {
        step_id: String,
        pid: u32,
        start_id: String,
        ack: tokio::sync::oneshot::Sender<()>,
    },
    /// Live agent output; `offset` is where `text` starts in `outputs/<step>.log`.
    Chunk {
        step_id: String,
        offset: u64,
        text: String,
    },
    Done(StepDone),
}

pub type EffectTx = mpsc::UnboundedSender<EffectMsg>;

/// Write `bytes` to `<loop_dir>/<rel>` durably (tmp + fsync + rename + dir fsync).
pub fn write_blob(loop_dir: &Path, rel: &str, bytes: &[u8]) -> std::io::Result<BlobRef> {
    let path = loop_dir.join(rel);
    let parent = path.parent().unwrap_or(loop_dir);
    std::fs::create_dir_all(parent)?;
    let tmp = path.with_extension("tmp");
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, &path)?;
    if let Ok(d) = std::fs::File::open(parent) {
        let _ = d.sync_all();
    }
    Ok(BlobRef {
        path: rel.to_string(),
        bytes: bytes.len() as u64,
    })
}

fn remaining(deadline_ms: u64) -> Duration {
    Duration::from_millis(deadline_ms.saturating_sub(agentpit_events::now_ms()))
}

// ---------------------------------------------------------------------------------------
// Agent.

/// Everything an agent attempt needs, prepared (and journaled in `step_started`) by the
/// runner.
pub struct AgentJob {
    pub step_id: String,
    pub node: String,
    pub loop_dir: PathBuf,
    pub cwd: PathBuf,
    pub prompt: String,
    /// `Err` = the cast could not be resolved; the attempt ends `error` without dispatching.
    pub backend: Result<BackendId, String>,
    pub model: Option<String>,
    pub effort: Option<Effort>,
    pub verdict: bool,
    pub deadline_ms: u64,
    /// The step's run in events.jsonl (dispatch only: a manager run logs itself).
    pub logger: Option<RunLogger>,
    pub regs: Arc<Registries>,
    pub work: AgentWork,
}

/// Who plays an agent step.
pub enum AgentWork {
    /// One stateless dispatch to the backend.
    Dispatch,
    /// A `manager` node: the workflow manager, with the prompt as its goal, dispatching to
    /// the configured roles itself (`agentpit workflow` in one step).
    Manager {
        workflow_type: Option<String>,
        link: crate::cli::workflow::ManagerLink,
    },
}

/// The final answer of either kind of agent step.
struct Answer {
    output: String,
    auth_failed: bool,
}

trait MapOkAnswer {
    fn map_ok_answer(self) -> impl std::future::Future<Output = anyhow::Result<Answer>> + Send;
}

impl<F> MapOkAnswer for F
where
    F: std::future::Future<Output = anyhow::Result<String>> + Send,
{
    async fn map_ok_answer(self) -> anyhow::Result<Answer> {
        self.await.map(|output| Answer {
            output,
            auth_failed: false,
        })
    }
}

/// Appends streamed output to `outputs/<step>.log` and forwards it with its offset.
struct OutputLog {
    file: Option<std::fs::File>,
    offset: u64,
}

pub async fn run_agent(job: AgentJob, cancel: CancellationToken, tx: EffectTx) -> StepDone {
    let started = Instant::now();
    let done = StepDone::new(&job.step_id, Outcome::Ok, started);
    let backend = match &job.backend {
        Ok(b) => *b,
        Err(e) => return done.failed(&job.node, Outcome::Error, e.clone()),
    };
    if let Some(logger) = &job.logger {
        logger.member_started(
            backend,
            false,
            job.model.as_deref(),
            job.effort.map(|e| e.as_str()),
        );
    }

    let log_path = job.loop_dir.join(output_log_rel(&job.step_id));
    if let Some(parent) = log_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let out = Arc::new(Mutex::new(OutputLog {
        file: std::fs::File::create(&log_path).ok(),
        offset: 0,
    }));
    let tee = job
        .logger
        .as_ref()
        .map(|l| output_streamer(l.run_id(), backend, false));
    let on_chunk: Arc<dyn Fn(&str) + Send + Sync> = {
        let out = Arc::clone(&out);
        let tx = tx.clone();
        let step_id = job.step_id.clone();
        Arc::new(move |chunk: &str| {
            if let Some(tee) = &tee {
                tee(chunk);
            }
            let offset = {
                let Ok(mut log) = out.lock() else { return };
                let offset = log.offset;
                if let Some(f) = log.file.as_mut() {
                    let _ = f.write_all(chunk.as_bytes());
                }
                log.offset += chunk.len() as u64;
                offset
            };
            let _ = tx.send(EffectMsg::Chunk {
                step_id: step_id.clone(),
                offset,
                text: chunk.to_string(),
            });
        })
    };

    let token = cancel.child_token();
    let fut: std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<Answer>> + Send>> =
        match &job.work {
            AgentWork::Dispatch => {
                let (prompt, cwd, regs) =
                    (job.prompt.clone(), job.cwd.clone(), Arc::clone(&job.regs));
                let (model, effort, token) = (job.model.clone(), job.effort, token.clone());
                Box::pin(async move {
                    dispatch_continuing(
                        backend,
                        &prompt,
                        &cwd,
                        token,
                        on_chunk,
                        &regs,
                        model.as_deref(),
                        effort,
                        None,
                    )
                    .await
                    .map(|r| Answer {
                        output: r.output,
                        auth_failed: r.auth_failed,
                    })
                })
            }
            AgentWork::Manager {
                workflow_type,
                link,
            } => Box::pin(
                crate::cli::workflow::run_capture(
                    job.prompt.clone(),
                    workflow_type.clone(),
                    Some(backend),
                    None,
                    None,
                    false,
                    job.model.clone(),
                    job.effort,
                    job.cwd.clone(),
                    token.clone(),
                    on_chunk,
                    Some(link.clone()),
                )
                .map_ok_answer(),
            ),
        };
    tokio::pin!(fut);
    let limit = remaining(job.deadline_ms);
    let (result, timed_out) = tokio::select! {
        r = &mut fut => (r, false),
        _ = tokio::time::sleep(limit) => {
            // Let the dispatch wind its process down (interrupt, then kill) before reporting.
            // A deadline that passes while a cancel is already winding it down is not a
            // timeout: the cancel is what ended it.
            let by_cancel = cancel.is_cancelled();
            token.cancel();
            (fut.await, !by_cancel)
        }
    };
    let mut done = StepDone {
        elapsed_ms: started.elapsed().as_millis() as u64,
        ..done
    };

    let (done, leg, error) = match result {
        _ if cancel.is_cancelled() && !timed_out => {
            done.outcome = Outcome::Cancelled;
            (done, LegStatus::Skipped, None)
        }
        _ if timed_out => {
            let msg = format!(
                "{} did not finish within its {}s deadline",
                job.step_id,
                limit.as_secs()
            );
            (
                done.failed(&job.node, Outcome::Timeout, msg.clone()),
                LegStatus::Error,
                Some(msg),
            )
        }
        _ if cancel.is_cancelled() => {
            done.outcome = Outcome::Cancelled;
            (done, LegStatus::Skipped, None)
        }
        Err(e) => {
            let msg = format!("{e:#}");
            // Only the dispatch's own cap is a timeout; an agent's output that merely
            // mentions one ("connection timed out") is an error.
            let outcome = if msg.contains("dispatch timed out after") {
                Outcome::Timeout
            } else {
                Outcome::Error
            };
            (
                done.failed(&job.node, outcome, msg.clone()),
                LegStatus::Error,
                Some(msg),
            )
        }
        Ok(res) if res.auth_failed => {
            let msg = format!(
                "{backend} is not logged in (authentication failed); run `agentpit login` and retry the step"
            );
            // Not a capability failure: kept out of the learning signal (design §10).
            (
                done.failed(&job.node, Outcome::Error, msg.clone()),
                LegStatus::Skipped,
                Some(msg),
            )
        }
        Ok(res) => {
            match write_blob(
                &job.loop_dir,
                &answer_rel(&job.step_id),
                res.output.as_bytes(),
            ) {
                Err(e) => {
                    let msg = format!("could not save the answer: {e}");
                    (
                        done.failed(&job.node, Outcome::Error, msg.clone()),
                        LegStatus::Error,
                        Some(msg),
                    )
                }
                Ok(blob) => {
                    done.output = Some(blob);
                    done.excerpt = excerpt(&res.output);
                    if job.verdict {
                        match parse_verdict(&res.output) {
                            Some(v) => {
                                if !v.pass {
                                    done.outcome = Outcome::Fail;
                                    done.feedback.push(FeedbackItem {
                                        source: FeedbackSource::Verdict,
                                        node: job.node.clone(),
                                        step_id: Some(job.step_id.clone()),
                                        summary: format!("{} said VERDICT: FAIL", job.node),
                                        detail: v.findings.clone(),
                                    });
                                }
                                done.verdict = Some(v);
                                (done, LegStatus::Ok, None)
                            }
                            None => {
                                let msg = format!(
                                    "{} ended without a `VERDICT: PASS|FAIL` line",
                                    job.step_id
                                );
                                (
                                    done.failed(&job.node, Outcome::Error, msg.clone()),
                                    LegStatus::Error,
                                    Some(msg),
                                )
                            }
                        }
                    } else {
                        (done, LegStatus::Ok, None)
                    }
                }
            }
        }
    };
    if let Some(logger) = &job.logger {
        logger.member_finished(
            backend,
            false,
            leg,
            done.elapsed_ms,
            done.output.as_ref().map(|b| b.bytes as usize),
            error,
        );
        logger.finished(leg);
    }
    done
}

// ---------------------------------------------------------------------------------------
// Check.

pub struct CheckJob {
    pub step_id: String,
    pub node: String,
    pub loop_id: String,
    pub loop_dir: PathBuf,
    pub cwd: PathBuf,
    pub command: String,
    pub deadline_ms: u64,
}

/// `sh -c <command>` in its own process group, stdout+stderr into `checks/<step>.log`.
/// Exit 0 = ok; anything else = fail, with the log's tail as feedback. Descendants still
/// running when the command ends are killed with the group.
pub async fn run_check(job: CheckJob, cancel: CancellationToken, tx: EffectTx) -> StepDone {
    let started = Instant::now();
    let done = StepDone::new(&job.step_id, Outcome::Ok, started);
    let log_path = job.loop_dir.join(check_log_rel(&job.step_id));
    let log = match log_path
        .parent()
        .map_or(Ok(()), std::fs::create_dir_all)
        .and_then(|_| std::fs::File::create(&log_path))
    {
        Ok(f) => f,
        Err(e) => {
            return done.failed(
                &job.node,
                Outcome::Error,
                format!("cannot create the check log: {e}"),
            );
        }
    };
    let stderr = match log.try_clone() {
        Ok(f) => f,
        Err(e) => {
            return done.failed(
                &job.node,
                Outcome::Error,
                format!("cannot create the check log: {e}"),
            );
        }
    };
    // The command waits for one line on stdin before it runs: the runner releases it only
    // after `step_spawned` (its process group) is durable, so a runner crash in between
    // leaves nothing the next runner cannot find. If the runner dies first, stdin closes
    // and the command never runs. After the release stdin is at EOF, like /dev/null.
    let mut cmd = tokio::process::Command::new("sh");
    cmd.arg("-c")
        .arg(r#"read -r _ || exit 125; exec sh -c "$1""#)
        .arg("agentpit-check")
        .arg(&job.command)
        .current_dir(&job.cwd)
        .env("AGENTPIT_LOOP_ID", &job.loop_id)
        .env("AGENTPIT_STEP_ID", &job.step_id)
        .stdin(Stdio::piped())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(stderr))
        .kill_on_drop(true);
    #[cfg(unix)]
    cmd.process_group(0);
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            return done.failed(
                &job.node,
                Outcome::Error,
                format!("cannot start `sh -c {}`: {e}", job.command),
            );
        }
    };
    let pid = child.id().unwrap_or(0);
    let stdin = child.stdin.take();
    let released = if pid > 0 {
        let (ack, acked) = tokio::sync::oneshot::channel();
        let _ = tx.send(EffectMsg::Spawned {
            step_id: job.step_id.clone(),
            pid,
            start_id: agentpit_events::session_lease::process_start_id(pid),
            ack,
        });
        tokio::select! {
            r = acked => r.is_ok(),
            _ = cancel.cancelled() => false,
        }
    } else {
        true
    };
    if !released {
        signal_group(pid, 9);
        let _ = child.kill().await;
        let _ = child.wait().await;
        return if cancel.is_cancelled() {
            StepDone {
                outcome: Outcome::Cancelled,
                ..done
            }
        } else {
            done.failed(
                &job.node,
                Outcome::Error,
                "the runner could not record the check's process; retry the step".into(),
            )
        };
    }
    if let Some(mut stdin) = stdin {
        use tokio::io::AsyncWriteExt;
        let _ = stdin.write_all(b"\n").await;
    }
    let limit = remaining(job.deadline_ms);
    enum End {
        Exited(std::io::Result<std::process::ExitStatus>),
        TimedOut,
        Cancelled,
    }
    let end = tokio::select! {
        s = child.wait() => End::Exited(s),
        _ = tokio::time::sleep(limit) => End::TimedOut,
        _ = cancel.cancelled() => End::Cancelled,
    };
    match &end {
        // Background descendants (`cmd &`) must not outlive the check.
        End::Exited(_) => {
            signal_group(pid, 9);
        }
        End::TimedOut | End::Cancelled => {
            // TERM the group, reaping the leader while waiting: an unreaped leader is a
            // zombie that keeps the group "alive" for the whole grace period.
            signal_group(pid, 15);
            let graceful = tokio::time::timeout(Duration::from_secs(2), async {
                let _ = child.wait().await;
                while group_alive(pid) {
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
            })
            .await;
            if graceful.is_err() {
                signal_group(pid, 9);
                let _ = child.kill().await;
                let _ = child.wait().await;
            }
        }
    }
    let tail = file_tail(&log_path, LOG_TAIL_BYTES).filter(|t| !t.trim().is_empty());
    let mut done = StepDone {
        elapsed_ms: started.elapsed().as_millis() as u64,
        log_tail: tail.clone(),
        ..done
    };
    let short = clamp_text(job.command.lines().next().unwrap_or_default(), 200);
    match end {
        End::Exited(Ok(st)) if st.success() => {
            done.exit_code = st.code();
            done
        }
        End::Exited(Ok(st)) => {
            done.exit_code = st.code();
            done.outcome = Outcome::Fail;
            let how = st.code().map_or("was killed by a signal".to_string(), |c| {
                format!("exited {c}")
            });
            done.feedback.push(FeedbackItem {
                source: FeedbackSource::Check,
                node: job.node.clone(),
                step_id: Some(job.step_id.clone()),
                summary: clamp_text(&format!("`{short}` {how}"), MAX_SHORT_BYTES),
                detail: tail,
            });
            done
        }
        End::Exited(Err(e)) => done.failed(
            &job.node,
            Outcome::Error,
            format!("waiting for the check failed: {e}"),
        ),
        End::Cancelled => {
            done.outcome = Outcome::Cancelled;
            done
        }
        End::TimedOut => {
            let mut done = done.failed(
                &job.node,
                Outcome::Timeout,
                format!("`{short}` did not finish within {}s", limit.as_secs()),
            );
            if let (Some(t), Some(f)) = (tail, done.feedback.last_mut()) {
                f.detail = Some(t);
            }
            done
        }
    }
}
