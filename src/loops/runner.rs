//! The loop runner (design §11): one process per loop, the loop journal's single writer.
//!
//! One actor task owns the [`LoopJournal`] and the connection table. For every input — a
//! client request, an effect report, a timer — it asks the pure scheduler for the next
//! decision, commits it (admit → append → fsync → apply), broadcasts the new records, and
//! starts the effects they authorize, until the scheduler has nothing left to do. Effects
//! never touch the journal; clients never see a record that is not durable.
//!
//! Wire rules kept here:
//! - `loop_attach` is answered inside the actor, so the answer precedes every frame that
//!   follows it; the replay (from disk, `(since, head]`) is done by the connection's
//!   writer, and live frames queue behind it — no gap, no duplicate.
//! - An operation's answer is queued after the records it caused (read-your-writes).
//! - Each connection's queue is bounded. Chunks and heartbeats are dropped when it is full;
//!   a durable record that does not fit closes the connection (the client reattaches with
//!   its cursor).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use agentpit_events::loops::*;
use agentpit_events::wire::{
    CODE_BAD_REQUEST, CODE_UNSUPPORTED, FEATURE_LOOPS, LoopFile, PROTO_VERSION, ROLE_LOOP, Request,
    RequestBody, Response, ResponseData,
};
use anyhow::{Context, Result, anyhow};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::classify::{Admission, OpPlan, admit_op, op_error, rejection_error};
use super::effects::{
    AgentJob, AgentWork, CheckJob, EffectMsg, EffectTx, StepDone, run_agent, run_check, write_blob,
};
use super::paths::head_path;
use super::prompt::{agent_prompt, gate_prompt};
use super::records::{GatePrep, Prep, decision_events, start_events};
use super::sched::{self, Decision, StartPlan};
use crate::config::{HubConfig, RouteKey};
use crate::dispatch::Registries;
use crate::effort::Effort;
use crate::events::{LegStatus, RunKind, RunLink, RunLogger};
use crate::router::{RouteRequest, Router};
use crate::types::BackendId;

/// Agent steps without `timeout_secs`.
pub const DEFAULT_AGENT_TIMEOUT_SECS: u64 = 30 * 60;
/// How long a starting runner waits for a lease still held by an exiting one.
const LEASE_WAIT: Duration = Duration::from_secs(3);
/// A manager node runs a whole workflow (several dispatches) in its one step.
pub const DEFAULT_MANAGER_TIMEOUT_SECS: u64 = 2 * 60 * 60;
/// Check steps without `timeout_secs`.
pub const DEFAULT_CHECK_TIMEOUT_SECS: u64 = 15 * 60;
/// Frames queued per connection before chunks are dropped / the connection is closed.
const OUT_CAPACITY: usize = 4096;
const HEARTBEAT: Duration = Duration::from_secs(15);
/// Longest request line accepted.
const MAX_REQUEST_BYTES: u64 = 1024 * 1024;
/// Scheduler rounds per input: far above any real fan-out, a guard against a livelock bug.
const MAX_ROUNDS: usize = 10_000;

/// What the runner dispatches with. Production loads it from config; tests inject fakes.
pub struct RunnerEnv {
    pub config: HubConfig,
    pub regs: Arc<Registries>,
    /// Recorded in `writer_opened.build`.
    pub build: String,
    /// Let the router consult learned profiles and suspensions on disk (off in tests).
    pub learned_routing: bool,
    /// Park (close the journal and exit) after this long with nothing to do and no client;
    /// `None` never parks.
    pub park_after: Option<Duration>,
}

pub struct RunnerPaths {
    pub dir: PathBuf,
    pub leases_root: PathBuf,
    pub socket: PathBuf,
}

/// Run a loop until it is finished (or asked to shut down). Takes the journal lease
/// BEFORE touching the socket path, so a second runner for the same loop exits without
/// disturbing the first.
pub async fn run(loop_id: &str, paths: RunnerPaths, env: RunnerEnv) -> Result<()> {
    run_until(loop_id, paths, env, CancellationToken::new()).await
}

/// [`run`] with an external stop signal (tests; SIGTERM). Stopping this way is a crash as
/// far as the journal knows: running steps are recovered by the next runner.
pub async fn run_until(
    loop_id: &str,
    paths: RunnerPaths,
    env: RunnerEnv,
    stop: CancellationToken,
) -> Result<()> {
    if !is_valid_loop_id(loop_id) {
        return Err(anyhow!("{loop_id} is not a loop id"));
    }
    let writer = WriterInfo::current(&env.build);
    // The previous runner may still be on its way out (it parked a moment ago): give its
    // lease a moment to be released before calling the loop taken.
    let busy_until = tokio::time::Instant::now() + LEASE_WAIT;
    let opened = loop {
        match LoopJournal::open(&paths.dir, &paths.leases_root, now_ms(), &writer) {
            Err(JournalError::Busy { .. }) if tokio::time::Instant::now() < busy_until => {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            other => break other,
        }
    };
    let journal = match opened {
        Ok(Opened::Writable(j)) => j,
        Ok(Opened::ReadOnly { reason, .. }) => {
            return Err(anyhow!(
                "{loop_id} cannot be run by this agentpit: {reason}"
            ));
        }
        Err(JournalError::Busy { pid }) => {
            return Err(anyhow!(
                "{loop_id} already has a runner (pid {pid}); use `loop_ensure` to reach it"
            ));
        }
        Err(e) => return Err(anyhow!("open {loop_id}: {e}")),
    };
    if journal.state().loop_id() != Some(loop_id) {
        return Err(anyhow!("{} does not hold {loop_id}", paths.dir.display()));
    }

    let (tx, mut rx) = mpsc::unbounded_channel::<Input>();
    let (effect_tx, mut effect_rx) = mpsc::unbounded_channel::<EffectMsg>();
    let mut runner = Runner {
        loop_id: loop_id.to_string(),
        dir: paths.dir.clone(),
        journal,
        env: Arc::new(env),
        effect_tx,
        effects: HashMap::new(),
        conns: HashMap::new(),
        wake_at: None,
        stalled: None,
        warned: HashSet::new(),
        head_written: 0,
        exit: None,
        fatal: None,
    };
    runner.recover().await;

    crate::daemon::paths::ensure_runtime_dir().ok();
    let _ = std::fs::remove_file(&paths.socket);
    let listener = UnixListener::bind(&paths.socket)
        .with_context(|| format!("bind {}", paths.socket.display()))?;
    // Register ourselves, so the daemon can find (and, forced, stop) this runner however
    // it was started.
    let me = super::control::RunnerRecord {
        loop_id: loop_id.to_string(),
        pid: std::process::id(),
        start_id: agentpit_events::session_lease::process_start_id(std::process::id()),
        socket: paths.socket.display().to_string(),
    };
    let _ = super::control::save_runner(&me);

    let mut writers: Vec<tokio::task::JoinHandle<()>> = Vec::new();
    let mut next_conn: u64 = 1;
    let mut heartbeat = tokio::time::interval(HEARTBEAT);
    heartbeat.tick().await;
    let journal_file = journal_path(&paths.dir);

    let mut idle_since: Option<tokio::time::Instant> = None;
    let result = loop {
        runner.drive();
        runner.flush_head();
        if let Some(e) = runner.fatal.take() {
            break Err(e);
        }
        if runner.exit.is_none() && runner.is_done() {
            runner.close(CloseReason::Terminal);
        }
        // Nothing to do and nobody watching: park until a client or a deadline needs us.
        let mut park_in = None;
        match (runner.parkable(), runner.env.park_after) {
            (true, Some(after)) => {
                let since = *idle_since.get_or_insert_with(tokio::time::Instant::now);
                let idle = since.elapsed();
                if idle >= after {
                    runner.close(CloseReason::Idle);
                } else {
                    park_in = Some(after - idle);
                }
            }
            _ => idle_since = None,
        }
        if runner.exit.is_some() {
            break Ok(());
        }
        let mut sleep = runner
            .wake_at
            .map(|w| Duration::from_millis(w.saturating_sub(now_ms()).max(1)))
            .unwrap_or(Duration::from_secs(3600));
        if let Some(p) = park_in {
            sleep = sleep.min(p);
        }
        tokio::select! {
            Some(msg) = effect_rx.recv() => runner.on_effect(msg),
            Some(input) = rx.recv() => runner.on_input(input),
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => {
                    let id = next_conn;
                    next_conn += 1;
                    writers.push(spawn_connection(
                        stream,
                        id,
                        tx.clone(),
                        journal_file.clone(),
                        runner.loop_id.clone(),
                    ));
                    writers.retain(|h| !h.is_finished());
                }
                // Out of descriptors (EMFILE) keeps failing at once: back off, don't spin.
                Err(_) => tokio::time::sleep(Duration::from_millis(100)).await,
            },
            _ = tokio::time::sleep(sleep) => {}
            _ = heartbeat.tick() => runner.heartbeat(),
            _ = stop.cancelled() => break Ok(()),
        }
    };

    // Let queued frames reach their clients, then leave. Effects still running (only after
    // an external stop) die with the process; the next runner recovers their steps.
    drop(listener);
    let _ = std::fs::remove_file(&paths.socket);
    for e in runner.effects.values() {
        e.cancel.cancel();
    }
    runner.conns.clear();
    drop(tx);
    let _ = tokio::time::timeout(Duration::from_secs(2), async {
        for w in writers {
            let _ = w.await;
        }
    })
    .await;
    // Release the journal's lease BEFORE deregistering: once the record is gone the daemon
    // may start the next runner at once (a parked loop woken by an answer), and that
    // runner must find the lease free.
    let closed = runner.exit.is_some();
    drop(runner);
    if closed {
        super::control::remove_runner_if(&me);
    }
    result
}

// ---------------------------------------------------------------------------------------
// The actor.

enum Input {
    Open { conn: u64, tx: mpsc::Sender<Out> },
    Line { conn: u64, line: String },
    Closed { conn: u64 },
}

/// A frame for one connection's writer.
enum Out {
    Line(String),
    /// The `loop_attached` answer, then a disk replay of `(after, head]`.
    Attached {
        line: String,
        after: u64,
        head: u64,
    },
}

struct Conn {
    tx: mpsc::Sender<Out>,
    client: Option<String>,
    attached: bool,
    chunks: bool,
}

/// A running agent/check process.
struct Effect {
    cancel: CancellationToken,
    /// Why it is being cancelled (the first reason asked for wins).
    cause: Option<CancelCause>,
    /// The `cancel_step` op the `step_finished` is credited to.
    op: Option<String>,
    /// `cancel_step` requests to answer when the step has ended: (conn, request id, op id).
    waiters: Vec<(u64, u64, String)>,
}

struct Runner {
    loop_id: String,
    dir: PathBuf,
    journal: LoopJournal,
    env: Arc<RunnerEnv>,
    effect_tx: EffectTx,
    effects: HashMap<String, Effect>,
    conns: HashMap<u64, Conn>,
    wake_at: Option<u64>,
    /// The scheduler asked for something `admit` refused: wait for new input.
    stalled: Option<String>,
    warned: HashSet<String>,
    head_written: u64,
    exit: Option<CloseReason>,
    fatal: Option<anyhow::Error>,
}

fn now_ms() -> u64 {
    agentpit_events::now_ms()
}

fn record_frame(loop_id: &str, raw: &str) -> String {
    format!(r#"{{"event":"loop_record","loop_id":"{loop_id}","rec":{raw}}}"#)
}

fn response_line(resp: &Response) -> String {
    serde_json::to_string(resp).unwrap_or_default()
}

fn op_error_response(id: u64, e: &OpError) -> Response {
    let resp = Response::err_code(id, e.code.as_str(), e.message.clone());
    match &e.details {
        Some(d) => resp.with_details(d.clone()),
        None => resp,
    }
}

impl Runner {
    fn state(&self) -> &LoopState {
        self.journal.state()
    }

    fn is_done(&self) -> bool {
        self.state().status.is_terminal() && self.effects.is_empty()
    }

    /// Nothing running, nothing to decide, nobody connected: only a person (or a gate
    /// deadline, which the daemon wakes us for) can move the loop now.
    fn parkable(&self) -> bool {
        self.effects.is_empty()
            && self.conns.is_empty()
            && self.stalled.is_none()
            && self.exit.is_none()
            && !self.state().status.is_terminal()
    }

    // --- committing ---------------------------------------------------------------------

    fn commit(&mut self, drafts: &[Draft]) -> Result<Vec<LoadedRecord>, CommitError> {
        match self.journal.commit(now_ms(), drafts) {
            Ok(records) => {
                self.broadcast(&records);
                Ok(records)
            }
            Err(CommitError::Journal(e @ JournalError::LineTooLarge { .. })) => {
                Err(CommitError::Journal(e))
            }
            Err(CommitError::Journal(e)) => {
                // Disk trouble: the writer is poisoned. Leave; the next runner recovers.
                self.fatal = Some(anyhow!("journal write failed: {e}"));
                Err(CommitError::Journal(e))
            }
            Err(e) => Err(e),
        }
    }

    fn broadcast(&mut self, records: &[LoadedRecord]) {
        let mut dead = Vec::new();
        for r in records {
            let frame = record_frame(&self.loop_id, &r.raw);
            for (id, c) in &self.conns {
                // Once one record did not fit, the connection gets nothing more: a later
                // record without the earlier one would be a gap the cursor then skips.
                if c.attached
                    && !dead.contains(id)
                    && c.tx.try_send(Out::Line(frame.clone())).is_err()
                {
                    dead.push(*id);
                }
            }
        }
        for id in dead {
            // A durable record that does not fit: close, the client reattaches.
            self.conns.remove(&id);
        }
    }

    fn reply(&mut self, conn: u64, resp: Response) {
        if let Some(c) = self.conns.get(&conn)
            && c.tx.try_send(Out::Line(response_line(&resp))).is_err()
        {
            self.conns.remove(&conn);
        }
    }

    /// A scheduler decision `admit` refused: journal it once as a warning and stop driving
    /// until something changes.
    fn stall(&mut self, what: &str, e: &CommitError) {
        let msg = clamp_text(&format!("{what}: {e}"), MAX_SHORT_BYTES);
        eprintln!("agentpit loop runner {}: {msg}", self.loop_id);
        if self.warned.insert(msg.clone()) {
            let _ = self.commit(&[Draft::new(LoopEvent::Warning(Warning {
                code: "scheduler_rejected".into(),
                message: msg.clone(),
                step_id: None,
            }))]);
        }
        self.stalled = Some(msg);
        // No timer while stalled: a past deadline would otherwise wake us every millisecond.
        self.wake_at = None;
    }

    // --- scheduling -----------------------------------------------------------------------

    fn drive(&mut self) {
        if self.stalled.is_some() || self.exit.is_some() || self.fatal.is_some() {
            return;
        }
        for _ in 0..MAX_ROUNDS {
            let now = now_ms();
            let plan = sched::next(self.state(), now);
            for (step_id, cause) in &plan.cancels {
                if let Some(e) = self.effects.get_mut(step_id) {
                    e.cause.get_or_insert(*cause);
                    e.cancel.cancel();
                }
            }
            let Some(decision) = plan.decision else {
                self.wake_at = plan.wake_at;
                return;
            };
            let ok = match &decision {
                Decision::Start(p) => self.start_step(p, now),
                d => {
                    let drafts: Vec<Draft> = decision_events(self.state(), d)
                        .into_iter()
                        .map(Draft::new)
                        .collect();
                    match self.commit(&drafts) {
                        Ok(_) => true,
                        Err(e) => {
                            if self.fatal.is_none() {
                                self.stall("scheduler decision", &e);
                            }
                            false
                        }
                    }
                }
            };
            if !ok {
                return;
            }
        }
        self.stalled = Some("the scheduler did not settle".into());
    }

    fn start_step(&mut self, plan: &StartPlan, now: u64) -> bool {
        let Some(node) = self
            .state()
            .blueprint
            .as_ref()
            .and_then(|bp| bp.node(&plan.node))
            .cloned()
        else {
            return false;
        };
        let step_id = plan.step_id();
        let (prep, job) = match &node.spec {
            NodeSpec::Agent(spec) => match self.prep_agent(plan, spec, now) {
                Ok((prep, job)) => (prep, Some(Job::Agent(Box::new(job)))),
                Err(e) => {
                    self.fatal = Some(anyhow!("write the prompt of {step_id}: {e}"));
                    return false;
                }
            },
            NodeSpec::Manager(spec) => match self.prep_manager(plan, spec, now) {
                Ok((prep, job)) => (prep, Some(Job::Agent(Box::new(job)))),
                Err(e) => {
                    self.fatal = Some(anyhow!("write the prompt of {step_id}: {e}"));
                    return false;
                }
            },
            NodeSpec::Check(spec) => {
                let (prep, job) = self.prep_check(plan, spec, now);
                (prep, Some(Job::Check(job)))
            }
            NodeSpec::Gate(spec) => (self.prep_gate(plan, spec, now), None),
            NodeSpec::Repeat(_) | NodeSpec::Unsupported => (Prep::default(), None),
        };
        let drafts: Vec<Draft> = start_events(self.state(), plan, prep)
            .into_iter()
            .map(Draft::new)
            .collect();
        if let Err(e) = self.commit(&drafts) {
            if self.fatal.is_none() {
                self.stall(&format!("start {step_id}"), &e);
            }
            return false;
        }
        if let Some(job) = job {
            let cancel = CancellationToken::new();
            self.effects.insert(
                step_id,
                Effect {
                    cancel: cancel.clone(),
                    cause: None,
                    op: None,
                    waiters: vec![],
                },
            );
            let tx = self.effect_tx.clone();
            tokio::spawn(async move {
                let done = match job {
                    Job::Agent(job) => run_agent(*job, cancel, tx.clone()).await,
                    Job::Check(job) => run_check(job, cancel, tx.clone()).await,
                };
                let _ = tx.send(EffectMsg::Done(done));
            });
        }
        true
    }

    fn prep_agent(
        &self,
        plan: &StartPlan,
        spec: &AgentSpec,
        now: u64,
    ) -> std::io::Result<(Prep, AgentJob)> {
        let state = self.state();
        let created = state
            .created
            .as_deref()
            .expect("a writable loop has a header");
        let step_id = plan.step_id();
        let texts = instruction_texts(state, plan);
        let body = agent_prompt(
            state,
            &self.dir,
            &plan.node,
            &spec.task,
            spec.verdict,
            &plan.iter,
            &texts,
        );
        let cast = self.resolve_cast(spec, &body);
        let prompt = match &cast.role {
            Some(role) => {
                crate::workflow::roles::persona_task(role, cast.persona.as_deref(), &body)
            }
            None => body,
        };
        // The durable copy (served by `loop_read`) masks credentials; the agent gets the
        // prompt as written.
        let saved = crate::exec::redact_secrets(&prompt);
        let blob = write_blob(&self.dir, &prompt_rel(&step_id), saved.as_bytes())?;
        let cwd = PathBuf::from(&created.cwd);
        let logger = cast.backend.as_ref().ok().map(|b| {
            let logger = RunLogger::start_linked(
                RunKind::Rescue,
                &[*b],
                &cwd,
                RunLink {
                    role: Some(cast.role.as_deref().unwrap_or(&plan.node)),
                    parent_run_id: created.root_run_id.as_deref(),
                    depth: 1,
                    loop_ref: Some(&self.loop_id),
                    run_id: None,
                },
            );
            match &cast.decision {
                Some(d) => d.log(&logger, &prompt, cast.model.as_deref(), cast.effort),
                None => logger.route_decided(
                    *b,
                    &cast.route,
                    None,
                    None,
                    None,
                    cast.model.as_deref(),
                    cast.effort.map(|e| e.as_str()),
                    &prompt,
                ),
            }
            logger
        });
        let deadline_ms = now
            + spec
                .timeout_secs
                .unwrap_or(DEFAULT_AGENT_TIMEOUT_SECS)
                .saturating_mul(1000);
        let backend_name = match &cast.backend {
            Ok(b) => b.as_str().to_string(),
            Err(_) => spec
                .backend
                .clone()
                .unwrap_or_else(|| "unassigned".to_string()),
        };
        let prep = Prep {
            access: Some(spec.access),
            assignee: Some(Assignee {
                role: cast.role.clone(),
                backend: backend_name,
                model: cast.model.clone(),
                effort: cast.effort.map(|e| e.as_str().to_string()),
                transport: cast
                    .backend
                    .as_ref()
                    .ok()
                    .and_then(|b| crate::dispatch::resolve_transport(*b, &self.env.regs))
                    .map(|t| t.as_str().to_string()),
                route: Some(if cast.backend.is_ok() {
                    cast.route.clone()
                } else {
                    "unresolved".to_string()
                }),
            }),
            run_id: logger.as_ref().map(|l| l.run_id().to_string()),
            prompt: Some(blob),
            command: None,
            deadline_ms: Some(deadline_ms),
            gate: None,
        };
        let job = AgentJob {
            step_id,
            node: plan.node.clone(),
            loop_dir: self.dir.clone(),
            cwd,
            prompt,
            backend: cast.backend,
            model: cast.model,
            effort: cast.effort,
            verdict: spec.verdict,
            deadline_ms,
            logger,
            regs: Arc::clone(&self.env.regs),
            work: AgentWork::Dispatch,
        };
        Ok((prep, job))
    }

    /// A manager node's step: an agent step played by the workflow manager. The manager is
    /// resolved here (explicit backend, the workflow type's, the manager role's, the
    /// configured one) and passed on explicitly, so the journaled assignee is what runs.
    fn prep_manager(
        &self,
        plan: &StartPlan,
        spec: &ManagerSpec,
        now: u64,
    ) -> std::io::Result<(Prep, AgentJob)> {
        let state = self.state();
        let created = state
            .created
            .as_deref()
            .expect("a writable loop has a header");
        let step_id = plan.step_id();
        let texts = instruction_texts(state, plan);
        let prompt = agent_prompt(
            state,
            &self.dir,
            &plan.node,
            &spec.task,
            spec.verdict,
            &plan.iter,
            &texts,
        );
        let saved = crate::exec::redact_secrets(&prompt);
        let blob = write_blob(&self.dir, &prompt_rel(&step_id), saved.as_bytes())?;
        let cwd = PathBuf::from(&created.cwd);
        let cast =
            (|| -> std::result::Result<(BackendId, Option<String>, Option<Effort>), String> {
                let explicit = match spec.backend.as_deref() {
                    Some(name) => Some(
                        BackendId::from_str(name).map_err(|_| format!("unknown backend {name}"))?,
                    ),
                    None => None,
                };
                let effort = match spec.effort.as_deref() {
                    Some(e) => Some(Effort::from_str(e).map_err(|x| format!("effort {e:?}: {x}"))?),
                    None => None,
                };
                crate::cli::workflow::manager_cast(
                    &self.env.config,
                    spec.workflow.as_deref(),
                    explicit,
                    spec.model.as_deref(),
                    effort,
                )
                .map_err(|e| format!("{e:#}"))
            })();
        let run_id = agentpit_events::next_run_id();
        let deadline_ms = now
            + spec
                .timeout_secs
                .unwrap_or(DEFAULT_MANAGER_TIMEOUT_SECS)
                .saturating_mul(1000);
        let (backend_name, model, effort) = match &cast {
            Ok((b, m, e)) => (b.as_str().to_string(), m.clone(), *e),
            Err(_) => (
                spec.backend
                    .clone()
                    .unwrap_or_else(|| "unassigned".to_string()),
                spec.model.clone(),
                None,
            ),
        };
        let prep = Prep {
            access: Some(spec.access),
            assignee: Some(Assignee {
                role: Some(crate::workflow::roles::MANAGER_ROLE.to_string()),
                backend: backend_name,
                model: model.clone(),
                effort: effort.map(|e| e.as_str().to_string()),
                transport: cast.is_ok().then(|| "exec".to_string()),
                route: Some(
                    if cast.is_ok() {
                        "manager"
                    } else {
                        "unresolved"
                    }
                    .to_string(),
                ),
            }),
            run_id: cast.is_ok().then(|| run_id.clone()),
            prompt: Some(blob),
            command: None,
            deadline_ms: Some(deadline_ms),
            gate: None,
        };
        let job = AgentJob {
            step_id,
            node: plan.node.clone(),
            loop_dir: self.dir.clone(),
            cwd,
            prompt,
            backend: cast.map(|(b, _, _)| b),
            model,
            effort,
            verdict: spec.verdict,
            deadline_ms,
            logger: None,
            regs: Arc::clone(&self.env.regs),
            work: AgentWork::Manager {
                workflow_type: spec.workflow.clone(),
                link: crate::cli::workflow::ManagerLink {
                    parent_run_id: created.root_run_id.clone(),
                    loop_ref: self.loop_id.clone(),
                    role: plan.node.clone(),
                    run_id,
                },
            },
        };
        Ok((prep, job))
    }

    /// Who plays an agent step: its role, its explicit backend, or the router.
    fn resolve_cast(&self, spec: &AgentSpec, prompt: &str) -> Cast {
        let config = &self.env.config;
        let mut available: Vec<BackendId> = self.env.regs.available().into_iter().collect();
        available.sort();
        let explicit_effort = match spec.effort.as_deref().map(Effort::from_str) {
            Some(Err(e)) => {
                return Cast::failed(spec, format!("effort {:?}: {e}", spec.effort));
            }
            Some(Ok(e)) => Some(e),
            None => None,
        };
        let backend_defaults = |b: BackendId| {
            let o = config.backends.get(&b);
            (o.and_then(|o| o.model.clone()), o.and_then(|o| o.effort))
        };
        if let Some(role) = &spec.role {
            return match crate::workflow::roles::resolve_role(
                role,
                &config.workflow.roles,
                &available,
            ) {
                Ok(r) => {
                    let (model, effort) = backend_defaults(r.backend);
                    Cast {
                        backend: Ok(r.backend),
                        role: Some(r.name),
                        persona: r.prompt,
                        model: crate::workflow::roles::resolve_model(
                            spec.model.as_deref(),
                            r.model.as_deref(),
                            model.as_deref(),
                        ),
                        effort: crate::effort::resolve_effort(explicit_effort, r.effort, effort)
                            .map(|e| e.clamp_for(r.backend)),
                        route: "role".into(),
                        decision: None,
                    }
                }
                Err(e) => Cast::failed(spec, format!("{e:#}")),
            };
        }
        let (backend, route, decision) = match &spec.backend {
            Some(name) => match BackendId::from_str(name) {
                Ok(b) if available.contains(&b) => (b, "explicit".to_string(), None),
                Ok(_) => {
                    return Cast::failed(
                        spec,
                        format!(
                            "backend {name} is not available here (not installed or no transport)"
                        ),
                    );
                }
                Err(_) => return Cast::failed(spec, format!("unknown backend {name}")),
            },
            None => {
                let profiles = if self.env.learned_routing {
                    crate::profile::load_profiles(None).unwrap_or_default()
                } else {
                    Default::default()
                };
                let mut router = Router::new(
                    config.clone(),
                    available.iter().copied().collect(),
                    profiles,
                );
                if self.env.learned_routing {
                    router = router.with_suspended(crate::availability::recently_suspended());
                }
                let d = router.resolve(&RouteRequest {
                    tool: RouteKey::Rescue,
                    explicit_backend: None,
                    task: Some(prompt),
                });
                if !available.contains(&d.backend) {
                    return Cast::failed(
                        spec,
                        format!(
                            "the router picked {}, which is not available here",
                            d.backend
                        ),
                    );
                }
                (d.backend, d.reason.as_str().to_string(), Some(d))
            }
        };
        let (model, effort) = backend_defaults(backend);
        Cast {
            backend: Ok(backend),
            role: None,
            persona: None,
            model: crate::workflow::roles::resolve_model(
                spec.model.as_deref(),
                None,
                model.as_deref(),
            ),
            effort: crate::effort::resolve_effort(explicit_effort, None, effort)
                .map(|e| e.clamp_for(backend)),
            route,
            decision,
        }
    }

    fn prep_check(&self, plan: &StartPlan, spec: &CheckSpec, now: u64) -> (Prep, CheckJob) {
        let created = self
            .state()
            .created
            .as_deref()
            .expect("a writable loop has a header");
        let mut cwd = PathBuf::from(&created.cwd);
        if let Some(rel) = spec.cwd.as_deref().filter(|r| is_safe_rel_path(r)) {
            cwd = cwd.join(rel);
        }
        let deadline_ms = now
            + spec
                .timeout_secs
                .unwrap_or(DEFAULT_CHECK_TIMEOUT_SECS)
                .saturating_mul(1000);
        let prep = Prep {
            access: Some(Access::Read),
            command: Some(spec.command.clone()),
            deadline_ms: Some(deadline_ms),
            ..Prep::default()
        };
        let job = CheckJob {
            step_id: plan.step_id(),
            node: plan.node.clone(),
            loop_id: self.loop_id.clone(),
            loop_dir: self.dir.clone(),
            cwd,
            command: spec.command.clone(),
            deadline_ms,
        };
        (prep, job)
    }

    fn prep_gate(&self, plan: &StartPlan, spec: &GateSpec, now: u64) -> Prep {
        let options = if spec.options.is_empty() {
            default_gate_options()
        } else {
            spec.options.clone()
        };
        Prep {
            gate: Some(GatePrep {
                prompt: gate_prompt(self.state(), &self.dir, &plan.node, spec, &plan.iter),
                options: options
                    .into_iter()
                    .map(|o| GateOption {
                        id: o.id,
                        label: o.label,
                        outcome: Some(o.outcome),
                    })
                    .collect(),
                deadline_ms: spec
                    .timeout_secs
                    .map(|s| now.saturating_add(s.saturating_mul(1000))),
                on_timeout: spec.on_timeout.clone(),
            }),
            ..Prep::default()
        }
    }

    // --- effects --------------------------------------------------------------------------

    fn on_effect(&mut self, msg: EffectMsg) {
        match msg {
            EffectMsg::Spawned {
                step_id,
                pid,
                start_id,
                ack,
            } => {
                self.stalled = None;
                let committed = self.commit(&[Draft::new(LoopEvent::StepSpawned(StepSpawned {
                    step_id,
                    pid,
                    start_id,
                }))]);
                // The check waits for this: nothing runs before its group is on disk, so
                // a crash can never leave an orphan the next runner cannot find.
                if committed.is_ok() {
                    let _ = ack.send(());
                }
            }
            EffectMsg::Chunk {
                step_id,
                offset,
                text,
            } => {
                let frame = serde_json::to_string(&agentpit_events::wire::Event::LoopChunk {
                    loop_id: self.loop_id.clone(),
                    step_id,
                    offset,
                    text,
                })
                .unwrap_or_default();
                for c in self.conns.values() {
                    // Lossy by design (the output log has every byte), and never allowed to
                    // crowd out the durable records behind it.
                    if c.attached && c.chunks && c.tx.capacity() > OUT_CAPACITY / 4 {
                        let _ = c.tx.try_send(Out::Line(frame.clone()));
                    }
                }
            }
            EffectMsg::Done(done) => {
                self.stalled = None;
                self.on_done(done);
            }
        }
    }

    fn on_done(&mut self, done: StepDone) {
        let effect = self.effects.remove(&done.step_id);
        let cancelled = done.outcome == Outcome::Cancelled;
        let cancel = cancelled.then(|| {
            effect
                .as_ref()
                .and_then(|e| e.cause)
                .unwrap_or(CancelCause::CancelStep)
        });
        // Credit a `cancel_step` op only when it is what ended the step.
        let op = effect
            .as_ref()
            .filter(|e| cancelled && e.cause == Some(CancelCause::CancelStep))
            .and_then(|e| e.op.clone());
        let finished = LoopEvent::StepFinished(Box::new(StepFinished {
            step_id: done.step_id.clone(),
            outcome: done.outcome,
            elapsed_ms: done.elapsed_ms,
            exit_code: done.exit_code,
            verdict: done.verdict,
            output: done.output,
            excerpt: done.excerpt,
            log_tail: done.log_tail,
            error: done.error,
            feedback: done.feedback,
            cancel,
        }));
        let draft = match &op {
            Some(op) => Draft::by_op(op, finished),
            None => Draft::new(finished),
        };
        let committed = self.commit(&[draft]);
        if let Err(e) = &committed
            && self.fatal.is_none()
        {
            let what = format!("finish {}", done.step_id);
            self.stall(&what, e);
        }
        let Some(effect) = effect else { return };
        for (conn, req_id, op_id) in effect.waiters {
            let resp = match &committed {
                // The op the finish was credited to.
                Ok(records) if op.as_deref() == Some(op_id.as_str()) => Response::ok(
                    req_id,
                    ResponseData::OpResult(OpResult {
                        op_id,
                        outcome: OpOutcome::Applied,
                        seq: records.first().map(|r| r.seq),
                        head_seq: self.state().head_seq,
                        result: None,
                    }),
                ),
                // Someone else's cancel (or a pause, a stop, the step's own end) got there
                // first: nothing of this op was written.
                Ok(_) => Response::ok(
                    req_id,
                    ResponseData::OpResult(OpResult {
                        op_id,
                        outcome: OpOutcome::Noop,
                        seq: None,
                        head_seq: self.state().head_seq,
                        result: None,
                    }),
                ),
                Err(e) => Response::err_code(req_id, ErrorCode::Internal.as_str(), e.to_string()),
            };
            self.reply(conn, resp);
        }
    }

    // --- recovery -------------------------------------------------------------------------

    /// Design §9.4: kill what the dead writer left running (only provably its processes),
    /// then record every orphaned step as interrupted. The scheduler takes it from there:
    /// a recovery gate for agents, an automatic retry for checks.
    async fn recover(&mut self) {
        let orphans: Vec<StepRun> = self.state().orphaned_steps().into_iter().cloned().collect();
        if orphans.is_empty() {
            return;
        }
        let writers = writer_pids(&self.dir);
        let mut reaped: HashSet<u32> = HashSet::new();
        for s in &orphans {
            if let (Some(pid), Some(start_id)) = (s.pid, s.pid_start_id.as_deref()) {
                super::proc::reap_orphan_group(pid, start_id).await;
            }
            if s.kind == NodeKind::Agent
                && reaped.insert(s.epoch)
                && let Some((pid, start_id)) = writers.get(&s.epoch)
            {
                // Agents ran in the dead runner's own process group.
                super::proc::reap_orphan_group(*pid, start_id).await;
            }
        }
        let drafts: Vec<Draft> = orphans
            .iter()
            .map(|s| {
                Draft::new(LoopEvent::StepInterrupted(StepInterrupted {
                    step_id: s.step_id.clone(),
                    epoch: s.epoch,
                }))
            })
            .collect();
        if let Err(e) = self.commit(&drafts) {
            self.stall("record interrupted steps", &e);
        }
    }

    // --- connections ----------------------------------------------------------------------

    fn on_input(&mut self, input: Input) {
        match input {
            Input::Open { conn, tx } => {
                self.conns.insert(
                    conn,
                    Conn {
                        tx,
                        client: None,
                        attached: false,
                        chunks: false,
                    },
                );
            }
            Input::Closed { conn } => {
                self.conns.remove(&conn);
            }
            // A line from a connection we already dropped (its queue overflowed): it can
            // no longer be answered, so it must not act either.
            Input::Line { conn, .. } if !self.conns.contains_key(&conn) => {}
            Input::Line { conn, line } => {
                self.stalled = None;
                self.on_request(conn, &line);
            }
        }
    }

    fn on_request(&mut self, conn: u64, line: &str) {
        let req = match serde_json::from_str::<Request>(line) {
            Ok(r) => r,
            Err(e) => {
                self.reply(conn, Response::bad_request(line, e));
                return;
            }
        };
        let id = req.id;
        match req.body {
            RequestBody::Hello { proto, client, .. } => {
                if proto != PROTO_VERSION {
                    self.reply(
                        conn,
                        Response::err(
                            id,
                            format!(
                                "protocol mismatch: loop runner speaks v{PROTO_VERSION}, client sent v{proto}"
                            ),
                        ),
                    );
                    return;
                }
                if let Some(c) = self.conns.get_mut(&conn) {
                    c.client = client;
                }
                self.reply(
                    conn,
                    Response::ok(
                        id,
                        ResponseData::Hello {
                            proto: PROTO_VERSION,
                            role: ROLE_LOOP.into(),
                            pid: std::process::id(),
                            features: vec![FEATURE_LOOPS.into()],
                        },
                    ),
                );
            }
            RequestBody::LoopStatus | RequestBody::Status => {
                let resp = match self.state().summary() {
                    Some(summary) => Response::ok(
                        id,
                        ResponseData::LoopSummary {
                            summary: Box::new(summary),
                        },
                    ),
                    None => Response::err(id, "the loop has no header"),
                };
                self.reply(conn, resp);
            }
            RequestBody::LoopAttach { since, chunks } => {
                let state = self.state();
                let head = state.head_seq;
                let (after, reset) = match &since {
                    None => (0, false),
                    Some(c) => match state.check_cursor(c) {
                        CursorCheck::Resume { after } => (after, false),
                        CursorCheck::Reset => (0, true),
                    },
                };
                let resp = Response::ok(
                    id,
                    ResponseData::LoopAttached {
                        loop_id: self.loop_id.clone(),
                        uid: state.uid().unwrap_or_default().to_string(),
                        head_seq: head,
                        reset,
                    },
                );
                if let Some(c) = self.conns.get_mut(&conn) {
                    c.attached = true;
                    c.chunks = chunks;
                    let out = Out::Attached {
                        line: response_line(&resp),
                        after,
                        head,
                    };
                    if c.tx.try_send(out).is_err() {
                        self.conns.remove(&conn);
                    }
                }
            }
            RequestBody::Detach => {
                if let Some(c) = self.conns.get_mut(&conn) {
                    c.attached = false;
                }
                self.reply(conn, Response::ok(id, ResponseData::Unit));
            }
            RequestBody::LoopRead {
                what,
                step_id,
                offset,
                max_bytes,
            } => {
                let resp = self.read_step_file(id, what, &step_id, offset, max_bytes);
                self.reply(conn, resp);
            }
            RequestBody::LoopOp(op) => self.on_op(conn, id, op),
            RequestBody::Shutdown { .. } => {
                let running = self.effects.len();
                if running > 0 {
                    self.reply(
                        conn,
                        Response::err_code(
                            id,
                            ErrorCode::Busy.as_str(),
                            format!(
                                "{running} step(s) are running; pause or stop the loop first, or force the runner down"
                            ),
                        ),
                    );
                    return;
                }
                self.close(CloseReason::Shutdown);
                self.reply(conn, Response::ok(id, ResponseData::Unit));
            }
            RequestBody::Unknown => self.reply(
                conn,
                Response::err_code(
                    id,
                    CODE_UNSUPPORTED,
                    "this loop runner does not know that request (it predates your agentpit); \
                     restart the loop's runner on your current version",
                ),
            ),
            _ => self.reply(
                conn,
                Response::err_code(
                    id,
                    CODE_BAD_REQUEST,
                    "this is a LOOP runner socket; session and daemon verbs go elsewhere",
                ),
            ),
        }
    }

    fn on_op(&mut self, conn: u64, id: u64, req: OpRequest) {
        let by = Actor::human(
            self.conns
                .get(&conn)
                .and_then(|c| c.client.as_deref())
                .unwrap_or("unknown"),
        );
        let admission = match admit_op(self.state(), &self.loop_id, &req, &by) {
            Ok(a) => a,
            Err(e) => {
                self.reply(conn, op_error_response(id, &e));
                return;
            }
        };
        let head = self.state().head_seq;
        let result = |outcome, seq, head_seq, result| {
            Response::ok(
                id,
                ResponseData::OpResult(OpResult {
                    op_id: req.op_id.clone(),
                    outcome,
                    seq,
                    head_seq,
                    result,
                }),
            )
        };
        let resp = match admission {
            Admission::Duplicate { seq } => result(OpOutcome::Duplicate, Some(seq), head, None),
            Admission::Plan(OpPlan::Noop) => result(OpOutcome::Noop, None, head, None),
            Admission::Plan(OpPlan::Commit {
                events,
                result: extra,
            }) => {
                let drafts: Vec<Draft> = events
                    .into_iter()
                    .map(|e| Draft::by_op(&req.op_id, e))
                    .collect();
                match self.commit(&drafts) {
                    Ok(records) => result(
                        OpOutcome::Applied,
                        records.first().map(|r| r.seq),
                        self.state().head_seq,
                        extra,
                    ),
                    Err(CommitError::Rejected { rejection, .. }) => {
                        op_error_response(id, &rejection_error(&rejection))
                    }
                    Err(e) => op_error_response(id, &op_error(ErrorCode::Internal, e.to_string())),
                }
            }
            Admission::Plan(OpPlan::CancelStep { step_id }) => {
                match self.effects.get_mut(&step_id) {
                    Some(effect) => {
                        effect.cause.get_or_insert(CancelCause::CancelStep);
                        if effect.cause == Some(CancelCause::CancelStep) {
                            effect.op.get_or_insert_with(|| req.op_id.clone());
                        }
                        effect.waiters.push((conn, id, req.op_id.clone()));
                        effect.cancel.cancel();
                        // Answered when the step's `step_finished` is written.
                        return;
                    }
                    None => op_error_response(
                        id,
                        &op_error(
                            ErrorCode::InvalidState,
                            format!(
                                "{step_id} has no running process in this runner; refresh and retry"
                            ),
                        ),
                    ),
                }
            }
        };
        self.reply(conn, resp);
    }

    fn read_step_file(
        &self,
        id: u64,
        what: LoopFile,
        step_id: &str,
        offset: u64,
        max_bytes: Option<u64>,
    ) -> Response {
        if self.state().step(step_id).is_none() {
            return Response::err_code(
                id,
                ErrorCode::NotFound.as_str(),
                format!("{step_id} is not a step of this loop"),
            );
        }
        let Some(rel) = step_file_rel(what, step_id) else {
            return Response::err_code(id, CODE_UNSUPPORTED, "unknown file kind");
        };
        match read_range(
            &self.dir.join(rel),
            offset,
            max_bytes.unwrap_or(READ_DEFAULT_BYTES).min(READ_MAX_BYTES),
        ) {
            Ok(data) => Response::ok(id, data),
            Err(e) => Response::err_code(id, ErrorCode::Internal.as_str(), e.to_string()),
        }
    }

    fn heartbeat(&mut self) {
        let frame = serde_json::to_string(&agentpit_events::wire::Event::LoopHeartbeat {
            loop_id: self.loop_id.clone(),
            head_seq: self.state().head_seq,
        })
        .unwrap_or_default();
        for c in self.conns.values() {
            if c.attached {
                let _ = c.tx.try_send(Out::Line(frame.clone()));
            }
        }
    }

    /// Write `writer_closed` and leave after this round. At the end of the loop, also close
    /// the loop's root run in events.jsonl.
    fn close(&mut self, reason: CloseReason) {
        let epoch = self.state().epoch;
        let _ = self.commit(&[Draft::new(LoopEvent::WriterClosed(WriterClosed {
            epoch,
            reason,
        }))]);
        if reason == CloseReason::Terminal
            && let Some(root) = self
                .state()
                .created
                .as_deref()
                .and_then(|c| c.root_run_id.as_deref())
        {
            let status = match self.state().finish.as_ref().map(|f| f.status) {
                Some(FinishStatus::Succeeded) => LegStatus::Ok,
                Some(FinishStatus::Cancelled) => LegStatus::Skipped,
                _ => LegStatus::Error,
            };
            RunLogger::resume(root).finished(status);
        }
        self.flush_head();
        self.exit = Some(reason);
    }

    /// Rewrite `head.json` when the journal moved (tmp + rename; never ahead of the journal
    /// because it is written after the commit).
    fn flush_head(&mut self) {
        let head = self.state().head_seq;
        if head == self.head_written {
            return;
        }
        if let Some(summary) = self.state().summary()
            && let Ok(bytes) = serde_json::to_vec(&summary)
        {
            let path = head_path(&self.dir);
            let tmp = path.with_extension("json.tmp");
            if std::fs::write(&tmp, &bytes).is_ok() && std::fs::rename(&tmp, &path).is_ok() {
                self.head_written = head;
            }
        }
    }
}

enum Job {
    Agent(Box<AgentJob>),
    Check(CheckJob),
}

/// The resolved cast of an agent step.
struct Cast {
    backend: Result<BackendId, String>,
    role: Option<String>,
    persona: Option<String>,
    model: Option<String>,
    effort: Option<Effort>,
    route: String,
    decision: Option<crate::router::RouteDecision>,
}

impl Cast {
    fn failed(spec: &AgentSpec, why: String) -> Cast {
        Cast {
            backend: Err(why),
            role: spec.role.clone(),
            persona: None,
            model: spec.model.clone(),
            effort: None,
            route: "unresolved".into(),
            decision: None,
        }
    }
}

/// The hidden `agentpit daemon loop` entry: lead a process group of our own (so the next
/// runner can reap our agents after a crash), load the config, and run.
pub async fn run_from_cli(loop_id: &str, socket: &Path) -> Result<()> {
    super::proc::lead_own_group();
    let dir = loop_dir(loop_id).ok_or_else(|| anyhow!("{loop_id} is not a loop id"))?;
    let loaded = crate::config::load_config(None)?;
    let regs = crate::dispatch::build_registries(&loaded.config);
    // AGENTPIT_LOOP_PARK_SECS overrides the config (tests, and trying parking out).
    let park_after = std::env::var("AGENTPIT_LOOP_PARK_SECS")
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(loaded.config.loops.park_after_minutes.saturating_mul(60));
    run(
        loop_id,
        RunnerPaths {
            dir,
            leases_root: loop_leases_dir(),
            socket: socket.to_path_buf(),
        },
        RunnerEnv {
            config: loaded.config,
            regs: Arc::new(regs),
            build: format!("agentpit/{}", env!("CARGO_PKG_VERSION")),
            learned_routing: true,
            park_after: (park_after > 0).then(|| Duration::from_secs(park_after)),
        },
    )
    .await
}

/// `epoch → (pid, start_id)` of every writer that ever opened the journal.
fn writer_pids(dir: &Path) -> BTreeMap<u32, (u32, String)> {
    let Ok(scan) = scan_file(&journal_path(dir)) else {
        return BTreeMap::new();
    };
    scan.records
        .iter()
        .filter_map(|r| match r.event() {
            Some(LoopEvent::WriterOpened(w)) => Some((w.epoch, (w.pid, w.start_id.clone()))),
            _ => None,
        })
        .collect()
}

/// The instructions an attempt carries: those every earlier attempt of the same retry chain
/// consumed (a retry must not lose what a person said, design §8.2), then its own.
fn instruction_texts<'a>(state: &'a LoopState, plan: &StartPlan) -> Vec<&'a str> {
    let mut chain = Vec::new();
    let mut prev = plan.retry_of.as_deref();
    while let Some(id) = prev {
        if chain.contains(&id) {
            break;
        }
        chain.push(id);
        prev = state.step(id).and_then(|s| s.retry_of.as_deref());
    }
    let mut ids: Vec<&str> = state
        .instructions
        .iter()
        .filter(|i| i.consumed_by.as_deref().is_some_and(|c| chain.contains(&c)))
        .map(|i| i.instruction_id.as_str())
        .collect();
    ids.extend(plan.instructions.iter().map(String::as_str));
    ids.iter()
        .filter_map(|id| state.instructions.iter().find(|i| i.instruction_id == *id))
        .map(|i| i.text.as_str())
        .collect()
}

fn read_range(path: &Path, offset: u64, max: u64) -> std::io::Result<ResponseData> {
    let page = read_page(path, offset, max)?;
    Ok(ResponseData::LoopBytes {
        offset: page.offset,
        next_offset: page.next_offset,
        size: page.size,
        text: page.text,
    })
}

// ---------------------------------------------------------------------------------------
// Connection tasks.

fn spawn_connection(
    stream: UnixStream,
    conn: u64,
    runner: mpsc::UnboundedSender<Input>,
    journal: PathBuf,
    loop_id: String,
) -> tokio::task::JoinHandle<()> {
    let (read_half, write_half) = stream.into_split();
    let (tx, rx) = mpsc::channel::<Out>(OUT_CAPACITY);
    let _ = runner.send(Input::Open { conn, tx });
    let reader_runner = runner.clone();
    tokio::spawn(async move {
        let mut reader = BufReader::new(read_half);
        let mut buf = Vec::new();
        loop {
            buf.clear();
            let n = match (&mut reader)
                .take(MAX_REQUEST_BYTES)
                .read_until(b'\n', &mut buf)
                .await
            {
                Ok(n) => n,
                Err(_) => break,
            };
            if n == 0 || (n as u64 == MAX_REQUEST_BYTES && !buf.ends_with(b"\n")) {
                break;
            }
            let line = String::from_utf8_lossy(&buf).trim().to_string();
            if line.is_empty() {
                continue;
            }
            if reader_runner.send(Input::Line { conn, line }).is_err() {
                break;
            }
        }
        let _ = reader_runner.send(Input::Closed { conn });
    });
    tokio::spawn(write_frames(write_half, rx, journal, loop_id))
}

async fn write_frames(
    mut w: tokio::net::unix::OwnedWriteHalf,
    mut rx: mpsc::Receiver<Out>,
    journal: PathBuf,
    loop_id: String,
) {
    while let Some(out) = rx.recv().await {
        let ok = match out {
            Out::Line(line) => write_line(&mut w, &line).await,
            Out::Attached { line, after, head } => {
                if !write_line(&mut w, &line).await {
                    break;
                }
                replay(&mut w, &journal, &loop_id, after, head).await
            }
        };
        if !ok {
            break;
        }
    }
    let _ = w.shutdown().await;
}

async fn write_line(w: &mut tokio::net::unix::OwnedWriteHalf, line: &str) -> bool {
    w.write_all(line.as_bytes()).await.is_ok() && w.write_all(b"\n").await.is_ok()
}

/// Every record with `after < seq <= head`, from disk (everything up to `head` is durable
/// before the actor answered the attach).
async fn replay(
    w: &mut tokio::net::unix::OwnedWriteHalf,
    journal: &Path,
    loop_id: &str,
    after: u64,
    head: u64,
) -> bool {
    if after >= head {
        return true;
    }
    let path = journal.to_path_buf();
    let Ok(Ok(scan)) = tokio::task::spawn_blocking(move || scan_file(&path)).await else {
        return false;
    };
    for r in scan
        .records
        .iter()
        .filter(|r| r.seq > after && r.seq <= head)
    {
        if !write_line(w, &record_frame(loop_id, &r.raw)).await {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page(path: &Path, offset: u64, max: u64) -> (u64, u64, String) {
        match read_range(path, offset, max).unwrap() {
            ResponseData::LoopBytes {
                offset,
                next_offset,
                text,
                ..
            } => (offset, next_offset, text),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn pages_never_split_a_character() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("out.log");
        let text = "修正しました。テストは通ります。";
        std::fs::write(&path, text).unwrap();
        // Read in 7-byte pages (not a multiple of the 3-byte characters).
        let mut offset = 0;
        let mut joined = String::new();
        while offset < text.len() as u64 {
            let (_, next, chunk) = page(&path, offset, 7);
            assert!(!chunk.contains('\u{FFFD}'), "{chunk:?}");
            assert!(next > offset, "no progress at {offset}");
            joined.push_str(&chunk);
            offset = next;
        }
        assert_eq!(joined, text);
        // An offset inside a character starts at the next one.
        let (start, _, chunk) = page(&path, 1, 64);
        assert_eq!(start, 3);
        assert!(chunk.starts_with('正'), "{chunk}");
        // A missing file is an empty page, not an error.
        assert_eq!(page(&tmp.path().join("nope"), 5, 10), (5, 5, String::new()));
    }
}
