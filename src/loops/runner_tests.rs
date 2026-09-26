//! Runner integration tests: a real runner on a real Unix socket, agents played by a fake
//! exec adapter (`sh -c` scripts), checks run as real shell commands (design §17 P2's
//! acceptance criteria).
//!
//! Every test holds the process-wide state-dir lock across awaits (XDG_STATE_HOME is
//! global), like the session worker tests.
#![allow(clippy::await_holding_lock)]

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use agentpit_events::loops::*;
use agentpit_events::wire::{Event, Frame, RequestBody, Response, ResponseData};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use super::runner::{RunnerEnv, RunnerPaths, run_until};
use crate::daemon::client::Conn;
use crate::dispatch::Registries;
use crate::exec::{ExecAdapter, ExecSpec};
use crate::types::BackendId;

const LOOP_ID: &str = "lp-0199a1b2c3d47e5f8a9b0c1d2e3f4a5b";

/// The fake agent: `sh -c SCRIPT sh <prompt>`, run in the loop's cwd.
struct ScriptAgent;

const AGENT_SCRIPT: &str = r#"
case "$1" in *HANG*) [ -f unhang ] || exec sleep 30;; esac
case "$1" in *"Iteration 2."*) touch fixed;; esac
case "$1" in *REVIEW*) printf 'the fix is missing a test\nVERDICT: FAIL\n'; exit 0;; esac
case "$1" in *FLAKY*) [ -f flaked ] || { touch flaked; echo 'transient failure' >&2; exit 1; };; esac
printf 'done: %s\n' "$(printf '%s' "$1" | head -n 1)"
"#;

impl ExecAdapter for ScriptAgent {
    fn id(&self) -> BackendId {
        BackendId::Codex
    }
    fn build_spec(
        &self,
        task: &str,
        _model: Option<&str>,
        _effort: Option<crate::effort::Effort>,
    ) -> ExecSpec {
        ExecSpec {
            command: "sh".into(),
            args: vec![
                "-c".into(),
                AGENT_SCRIPT.into(),
                "sh".into(),
                task.to_string(),
            ],
            env: vec![],
            stdin_input: None,
        }
    }
}

fn lock_env() -> std::sync::MutexGuard<'static, ()> {
    crate::ask::STATE_ENV_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

fn fix_until_green() -> Value {
    json!({
        "schema": "agentpit.blueprint/1",
        "name": "fix-until-green",
        "inputs": {"goal": {"required": true}},
        "budget": {"max_steps": 12, "max_active_secs": 600, "max_parallel": 1},
        "nodes": [
            {"id": "plan", "kind": "agent", "backend": "codex", "access": "read",
             "task": "Goal: {{goal}}\nWrite a plan."},
            {"id": "fix", "kind": "repeat", "max_iterations": 4},
            {"id": "implement", "kind": "agent", "parent": "fix", "backend": "codex",
             "access": "write",
             "task": "Goal: {{goal}}\nPlan:\n{{nodes.plan.output}}\nIteration {{iteration}}.\n{{feedback}}"},
            {"id": "test", "kind": "check", "parent": "fix",
             "command": "test -f fixed || { echo 'assertion failed: not fixed yet'; exit 1; }"},
            {"id": "signoff", "kind": "gate", "prompt": "Loop result: {{nodes.fix.outcome}}. Accept?"}
        ],
        "edges": [
            {"from": "plan", "to": "fix"},
            {"from": "implement", "to": "test"},
            {"from": "fix", "to": "signoff"},
            {"from": "fix", "to": "signoff", "on": "exhausted"}
        ]
    })
}

struct Harness {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    dir: PathBuf,
    socket: PathBuf,
    work: PathBuf,
    stop: CancellationToken,
    task: Option<tokio::task::JoinHandle<anyhow::Result<()>>>,
    park_after: Option<Duration>,
}

impl Harness {
    /// Create the loop (started) in a fresh state dir; the runner is not running yet.
    fn new(doc: Value, inputs: &[(&str, &str)]) -> Harness {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        unsafe { std::env::set_var("XDG_STATE_HOME", root.join("state")) };
        let work = root.join("work");
        std::fs::create_dir_all(&work).unwrap();
        let v = validate(&doc, &ValidateEnv::default());
        assert!(v.is_runnable(), "{:?}", v.diagnostics);
        let bp = v.blueprint.clone().unwrap();
        let dir = root.join("loops").join(LOOP_ID);
        std::fs::create_dir_all(&dir).unwrap();
        let created = LoopCreated {
            loop_id: LOOP_ID.into(),
            uid: new_uid(),
            title: "test".into(),
            blueprint: FrozenBlueprint {
                name: bp.name.clone(),
                scope: BlueprintScope::Inline,
                path: None,
                rev: v.rev.clone(),
                doc,
            },
            inputs: inputs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            cwd: work.display().to_string(),
            repo_root: None,
            workspace: bp.workspace.mode,
            budget: bp.budget,
            origin: Origin {
                surface: Surface::Cli,
                client: None,
                session_id: None,
            },
            root_run_id: Some("root-run".into()),
        };
        let journal = LoopJournal::create(
            &dir,
            &root.join("leases"),
            agentpit_events::now_ms(),
            created,
            Some("0199a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5b"),
            &WriterInfo::current("test"),
            false,
        )
        .unwrap();
        drop(journal);
        Harness {
            socket: root.join("loop.sock"),
            _tmp: tmp,
            root,
            dir,
            work,
            stop: CancellationToken::new(),
            task: None,
            park_after: None,
        }
    }

    fn start(&mut self) {
        let mut regs = Registries::empty();
        regs.execs.insert(BackendId::Codex, Box::new(ScriptAgent));
        let env = RunnerEnv {
            config: crate::config::HubConfig::default(),
            regs: Arc::new(regs),
            build: "test".into(),
            learned_routing: false,
            park_after: self.park_after,
        };
        let paths = RunnerPaths {
            dir: self.dir.clone(),
            leases_root: self.root.join("leases"),
            socket: self.socket.clone(),
        };
        self.stop = CancellationToken::new();
        let stop = self.stop.clone();
        let _ = std::fs::remove_file(&self.socket);
        self.task = Some(tokio::spawn(async move {
            run_until(LOOP_ID, paths, env, stop).await
        }));
    }

    /// Stop the runner without closing the journal: a crash, as far as the journal knows.
    async fn crash(&mut self) {
        self.stop.cancel();
        if let Some(t) = self.task.take() {
            t.await.unwrap().unwrap();
        }
    }

    async fn finished(&mut self) -> anyhow::Result<()> {
        let t = self.task.take().expect("runner running");
        tokio::time::timeout(Duration::from_secs(20), t)
            .await
            .expect("runner did not exit")
            .unwrap()
    }

    async fn connect(&self) -> Conn {
        for _ in 0..200 {
            if let Ok(c) = Conn::connect(&self.socket).await {
                return c;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("runner socket never answered");
    }

    /// Start the runner, attach a watcher, then start the loop (so even a loop that
    /// finishes at once is seen from its first record).
    async fn run_watched(&mut self) -> (Watch, Conn) {
        self.start();
        let (w, resp) = Watch::attach(self.connect().await, None).await;
        assert!(resp.ok, "{resp:?}");
        let mut conn = self.connect().await;
        let resp = conn
            .request_raw(op_request(LoopOp::Start, "op-start-0001"))
            .await
            .unwrap();
        assert_eq!(op_result(&resp).outcome, OpOutcome::Applied);
        (w, conn)
    }

    fn state(&self) -> LoopState {
        read_loop(&self.dir).unwrap().0
    }

    fn prompt(&self, step_id: &str) -> String {
        std::fs::read_to_string(self.dir.join(format!("prompts/{step_id}.md"))).unwrap()
    }
}

/// A client attached to the journal, folding what it receives.
struct Watch {
    conn: Conn,
    state: LoopState,
    seqs: Vec<u64>,
}

impl Watch {
    async fn attach(conn: Conn, since: Option<Cursor>) -> (Watch, Response) {
        let mut conn = conn;
        let resp = conn
            .request_raw(RequestBody::LoopAttach {
                since,
                chunks: false,
            })
            .await
            .unwrap();
        (
            Watch {
                conn,
                state: LoopState::default(),
                seqs: vec![],
            },
            resp,
        )
    }

    async fn next_record(&mut self) -> Option<LoadedRecord> {
        loop {
            match self.conn.recv_frame().await {
                Ok(Frame::Event(Event::LoopRecord { rec, .. })) => {
                    let r = decode_line(&serde_json::to_string(&rec).unwrap()).unwrap();
                    self.seqs.push(r.seq);
                    self.state.apply(&r);
                    return Some(r);
                }
                Ok(_) => continue,
                Err(_) => return None,
            }
        }
    }

    async fn until(&mut self, what: &str, f: impl Fn(&LoopState) -> bool) {
        let fut = async {
            while !f(&self.state) {
                if self.next_record().await.is_none() {
                    panic!("connection closed while waiting for {what}");
                }
            }
        };
        if tokio::time::timeout(Duration::from_secs(20), fut)
            .await
            .is_err()
        {
            panic!(
                "timed out waiting for {what}; status {} head {} gates {:?}",
                self.state.status,
                self.state.head_seq,
                self.state
                    .gates
                    .iter()
                    .map(|g| (&g.gate_id, g.kind, g.status))
                    .collect::<Vec<_>>()
            );
        }
    }
}

/// Read frames until the answer to request `id`.
async fn answer_to(conn: &mut Conn, id: u64) -> Response {
    loop {
        if let Frame::Response(r) = conn.recv_frame().await.unwrap()
            && r.id == id
        {
            return r;
        }
    }
}

fn op_request(op: LoopOp, op_id: &str) -> RequestBody {
    RequestBody::LoopOp(OpRequest {
        loop_id: LOOP_ID.into(),
        op_id: op_id.into(),
        expect_seq: None,
        op,
    })
}

fn op_result(resp: &Response) -> &OpResult {
    match &resp.data {
        Some(ResponseData::OpResult(r)) => r,
        other => panic!("expected op_result, got {other:?} ({:?})", resp.error),
    }
}

fn open_gate(state: &LoopState) -> Option<GateRun> {
    state.open_gates().next().cloned()
}

#[tokio::test]
async fn fix_until_green_runs_to_signoff_and_threads_check_feedback() {
    let _env = lock_env();
    let mut h = Harness::new(fix_until_green(), &[("goal", "fix the parser")]);
    let (mut w, _) = h.run_watched().await;
    w.until("the signoff gate", |s| {
        s.open_gates().any(|g| g.kind == GateKind::Approval)
    })
    .await;

    // Iteration 1 failed its check; iteration 2 was told why, and broke the repeat.
    let fix = &w.state.repeats["fix"];
    assert_eq!(
        fix.decisions,
        vec![IterationDecision::Continue, IterationDecision::Break]
    );
    let second = h.prompt("implement.i2.a1");
    assert!(
        second.contains("assertion failed: not fixed yet"),
        "iteration 2 prompt lacks the check output:\n{second}"
    );
    assert!(second.contains("Iteration 2."), "{second}");
    assert!(
        second.contains("done: Goal: fix the parser"),
        "the plan's output is threaded in:\n{second}"
    );
    assert!(!h.prompt("implement.i1.a1").contains("assertion failed"));
    let gate = open_gate(&w.state).unwrap();
    assert_eq!(gate.prompt, "Loop result: ok. Accept?");

    // The summary says who is waiting on what.
    let mut conn = h.connect().await;
    match conn.request(RequestBody::LoopStatus).await.unwrap() {
        ResponseData::LoopSummary { summary } => {
            assert!(summary.waiting, "{summary:?}");
            assert_eq!(summary.open_gates[0].gate_id, gate.gate_id);
        }
        other => panic!("{other:?}"),
    }

    // Approve over the socket; the loop ends succeeded and the runner leaves.
    let resp = conn
        .request_raw(op_request(
            LoopOp::ResolveGate {
                gate_id: gate.gate_id.clone(),
                option: "approve".into(),
                comment: None,
            },
            "0199a1c0-7b1e-7f00-9c11-3e5f6a7b8c9d",
        ))
        .await
        .unwrap();
    assert_eq!(op_result(&resp).outcome, OpOutcome::Applied);
    w.until("the end", |s| s.status.is_terminal()).await;
    assert_eq!(w.state.status, LoopStatus::Succeeded);
    h.finished().await.unwrap();

    // The file on disk is what the client folded, record for record, ending closed.
    let disk = h.state();
    assert_eq!(disk.status, LoopStatus::Succeeded);
    assert!(disk.warnings.is_empty(), "{:?}", disk.warnings);
    let head: LoopSummary =
        serde_json::from_slice(&std::fs::read(h.dir.join("head.json")).unwrap()).unwrap();
    assert_eq!(head.head_seq, disk.head_seq);
    assert_eq!(head.status, LoopStatus::Succeeded);
    let (_, scan) = read_loop(&h.dir).unwrap();
    assert_eq!(scan.records.last().unwrap().kind, "writer_closed");

    // events.jsonl: every agent step is a child run of the loop's root.
    let events = std::fs::read_to_string(h.root.join("state/agentpit/events.jsonl")).unwrap();
    let children: Vec<Value> = events
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|v| v["event"] == "run_started")
        .collect();
    assert_eq!(children.len(), 3, "plan + two implements:\n{events}");
    for c in &children {
        assert_eq!(c["parent_run_id"], "root-run");
        assert_eq!(c["depth"], 1);
        assert_eq!(c["loop_ref"], LOOP_ID);
    }
    let root_finished = events
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .find(|v| v["event"] == "run_finished" && v["run_id"] == "root-run")
        .unwrap_or_else(|| panic!("the root run is not closed:\n{events}"));
    assert_eq!(root_finished["status"], "ok");
}

#[tokio::test]
async fn ops_are_idempotent_checked_and_answered_after_their_records() {
    let _env = lock_env();
    let doc = json!({
        "schema": "agentpit.blueprint/1",
        "name": "two-gates",
        "nodes": [
            {"id": "a", "kind": "gate", "prompt": "First?"},
            {"id": "b", "kind": "gate", "prompt": "Second?"}
        ],
        "edges": [{"from": "a", "to": "b"}]
    });
    let mut h = Harness::new(doc, &[]);
    let (mut w, _) = h.run_watched().await;
    w.until("the gate", |s| s.open_gates().next().is_some())
        .await;
    let head = w.state.head_seq;

    // A stale expect_seq is a conflict and writes nothing.
    let mut conn = h.connect().await;
    let resp = conn
        .request_raw(RequestBody::LoopOp(OpRequest {
            loop_id: LOOP_ID.into(),
            op_id: "op-conflict-1".into(),
            expect_seq: Some(head - 1),
            op: LoopOp::Pause {
                mode: PauseMode::Drain,
            },
        }))
        .await
        .unwrap();
    assert_eq!(resp.code.as_deref(), Some("conflict"), "{resp:?}");
    assert_eq!(resp.details.as_ref().unwrap()["head_seq"], head);

    // An invalid option is a validation error that lists the choices.
    let resp = conn
        .request_raw(op_request(
            LoopOp::ResolveGate {
                gate_id: "g1".into(),
                option: "maybe".into(),
                comment: None,
            },
            "op-bad-option",
        ))
        .await
        .unwrap();
    assert_eq!(resp.code.as_deref(), Some("validation"));
    assert!(resp.error.unwrap().contains("approve"));

    // The answer arrives after the records it caused, on the same (attached) connection.
    let id = w
        .conn
        .send_request(op_request(
            LoopOp::ResolveGate {
                gate_id: "g1".into(),
                option: "approve".into(),
                comment: Some("lgtm".into()),
            },
            "op-approve-1",
        ))
        .await
        .unwrap();
    let mut seen_resolved = false;
    let first_seq = loop {
        match w.conn.recv_frame().await.unwrap() {
            Frame::Event(Event::LoopRecord { rec, .. }) => {
                let r = decode_line(&serde_json::to_string(&rec).unwrap()).unwrap();
                seen_resolved |= r.kind == "gate_resolved";
                w.state.apply(&r);
            }
            Frame::Response(resp) if resp.id == id => {
                assert!(seen_resolved, "the answer overtook its record");
                break op_result(&resp).seq.unwrap();
            }
            _ => {}
        }
    };

    // The same op id again: duplicate, same seq — even though the loop has moved on.
    let resp = conn
        .request_raw(op_request(
            LoopOp::ResolveGate {
                gate_id: "g1".into(),
                option: "approve".into(),
                comment: None,
            },
            "op-approve-1",
        ))
        .await
        .unwrap();
    let r = op_result(&resp);
    assert_eq!(r.outcome, OpOutcome::Duplicate);
    assert_eq!(r.seq, Some(first_seq));

    // Unknown request types are answered with the caller's id.
    let resp = conn
        .request_raw(
            serde_json::from_value(json!({"type": "teleport"})).unwrap_or(RequestBody::Unknown),
        )
        .await
        .unwrap();
    assert_eq!(resp.code.as_deref(), Some("unsupported"));

    w.until("g2", |s| s.gate("g2").is_some()).await;
    let resp = conn
        .request_raw(op_request(
            LoopOp::ResolveGate {
                gate_id: "g2".into(),
                option: "approve".into(),
                comment: None,
            },
            "op-approve-2",
        ))
        .await
        .unwrap();
    assert!(resp.ok, "{resp:?}");
    w.until("the end", |s| s.status.is_terminal()).await;
    h.finished().await.unwrap();
}

#[tokio::test]
async fn a_second_answer_to_a_closed_gate_says_who_answered() {
    let _env = lock_env();
    let doc = json!({
        "schema": "agentpit.blueprint/1",
        "name": "two-gates",
        "nodes": [
            {"id": "a", "kind": "gate", "prompt": "First?"},
            {"id": "b", "kind": "gate", "prompt": "Second?"}
        ],
        "edges": [{"from": "a", "to": "b"}]
    });
    let mut h = Harness::new(doc, &[]);
    let (mut w, _) = h.run_watched().await;
    w.until("g1", |s| s.gate("g1").is_some()).await;
    let mut conn = h.connect().await;
    let approve = |op_id: &str| {
        op_request(
            LoopOp::ResolveGate {
                gate_id: "g1".into(),
                option: "approve".into(),
                comment: None,
            },
            op_id,
        )
    };
    assert!(conn.request_raw(approve("op-first-1")).await.unwrap().ok);
    let resp = conn.request_raw(approve("op-second-1")).await.unwrap();
    assert_eq!(resp.code.as_deref(), Some("invalid_state"), "{resp:?}");
    let details = resp.details.unwrap();
    assert_eq!(details["status"], "resolved");
    assert_eq!(details["option"], "approve");
    h.crash().await;
}

#[tokio::test]
async fn attach_replays_exactly_what_follows_the_cursor() {
    let _env = lock_env();
    let doc = json!({
        "schema": "agentpit.blueprint/1",
        "name": "one-gate",
        "nodes": [{"id": "ok", "kind": "gate", "prompt": "Ship it?"}]
    });
    let mut h = Harness::new(doc, &[]);
    let (mut w, _) = h.run_watched().await;
    w.until("the gate", |s| s.open_gates().next().is_some())
        .await;
    let uid = w.state.uid().unwrap().to_string();
    let head = w.state.head_seq;
    assert_eq!(w.seqs, (1..=head).collect::<Vec<_>>());

    // Instructions pile up while a second client attaches mid-stream from a cursor.
    let mut writer = h.connect().await;
    let mut late_client = None;
    for i in 0..60 {
        let resp = writer
            .request_raw(op_request(
                LoopOp::Instruct {
                    text: format!("note {i}"),
                    target: None,
                },
                &format!("op-note-{i:04}"),
            ))
            .await
            .unwrap();
        assert!(resp.ok, "{resp:?}");
        if i == 20 {
            let (mut late, resp) = Watch::attach(
                h.connect().await,
                Some(Cursor {
                    uid: uid.clone(),
                    seq: 3,
                }),
            )
            .await;
            match resp.data {
                Some(ResponseData::LoopAttached { reset, .. }) => assert!(!reset),
                other => panic!("{other:?}"),
            }
            // Drain until the late client has seen everything the first one will.
            let target = head + 60;
            late_client = Some(tokio::spawn(async move {
                while late.seqs.last().copied().unwrap_or(0) < target {
                    late.next_record().await.unwrap();
                }
                late.seqs
            }));
        }
    }
    w.until("all notes", |s| s.instructions.len() == 60).await;
    assert_eq!(w.seqs, (1..=head + 60).collect::<Vec<_>>());
    let late = tokio::time::timeout(Duration::from_secs(10), late_client.unwrap())
        .await
        .expect("the late client stalled")
        .unwrap();
    assert_eq!(
        late,
        (4..=head + 60).collect::<Vec<_>>(),
        "no gap, no duplicate"
    );

    // A cursor from another incarnation resets to the start.
    let (_, resp) = Watch::attach(
        h.connect().await,
        Some(Cursor {
            uid: "someone-else".into(),
            seq: 5,
        }),
    )
    .await;
    assert!(matches!(
        resp.data,
        Some(ResponseData::LoopAttached { reset: true, .. })
    ));
    h.crash().await;
}

#[tokio::test]
async fn a_crash_mid_write_asks_a_person_and_retry_finishes_the_loop() {
    let _env = lock_env();
    let doc = json!({
        "schema": "agentpit.blueprint/1",
        "name": "crashy",
        "nodes": [
            {"id": "edit", "kind": "agent", "backend": "codex", "access": "write",
             "task": "{{inputs.mode}} please"}
        ],
        "inputs": {"mode": {"required": true}}
    });
    let mut h = Harness::new(doc, &[("mode", "HANG")]);
    let (mut w, _) = h.run_watched().await;
    w.until("the agent to run", |s| {
        s.running_steps()
            .iter()
            .any(|st| st.kind == NodeKind::Agent)
    })
    .await;
    drop(w);
    h.crash().await;
    assert_eq!(h.state().status, LoopStatus::Running);

    // The next runner records the interruption and asks; nothing is re-run on its own.
    h.start();
    let (mut w, _) = Watch::attach(h.connect().await, None).await;
    w.until("the recovery gate", |s| {
        s.open_gates().any(|g| g.kind == GateKind::Recovery)
    })
    .await;
    // Epoch 1 created the loop, 2 was the crashed runner.
    assert_eq!(w.state.epoch, 3);
    assert_eq!(w.state.steps["edit.a1"].status, StepStatus::Interrupted);
    assert!(!w.state.steps.contains_key("edit.a2"));

    assert_eq!(h.prompt("edit.a1"), "HANG please");

    // Retry (the agent finishes this time): a fresh attempt, and the loop completes.
    std::fs::write(h.work.join("unhang"), "").unwrap();
    let gate = open_gate(&w.state).unwrap();
    let mut conn = h.connect().await;
    let resp = conn
        .request_raw(op_request(
            LoopOp::ResolveGate {
                gate_id: gate.gate_id.clone(),
                option: "retry".into(),
                comment: None,
            },
            "op-retry-1",
        ))
        .await
        .unwrap();
    assert!(resp.ok, "{resp:?}");
    w.until("the end", |s| s.status.is_terminal()).await;
    assert_eq!(w.state.status, LoopStatus::Succeeded);
    assert_eq!(w.state.steps["edit.a2"].cause, StartCause::Recovery);
    h.finished().await.unwrap();
}

#[tokio::test]
async fn cancel_step_is_answered_when_the_step_has_ended() {
    let _env = lock_env();
    let doc = json!({
        "schema": "agentpit.blueprint/1",
        "name": "slow-check",
        "policy": {"on_error": "fail"},
        "nodes": [{"id": "wait", "kind": "check", "command": "sleep 30"}]
    });
    let mut h = Harness::new(doc, &[]);
    let (mut w, _) = h.run_watched().await;
    w.until("the check to spawn", |s| {
        s.steps.get("wait.a1").is_some_and(|st| st.pid.is_some())
    })
    .await;
    let pid = w.state.steps["wait.a1"].pid.unwrap();
    let mut conn = h.connect().await;
    let resp = conn
        .request_raw(op_request(
            LoopOp::CancelStep {
                step_id: "wait.a1".into(),
            },
            "op-cancel-wait",
        ))
        .await
        .unwrap();
    let r = op_result(&resp);
    assert_eq!(r.outcome, OpOutcome::Applied);
    let seq = r.seq.unwrap();
    w.until("the finish", |s| s.head_seq >= seq).await;
    let step = &w.state.steps["wait.a1"];
    assert_eq!(step.outcome, Some(Outcome::Cancelled));
    assert_eq!(step.cancel, Some(CancelCause::CancelStep));
    assert!(
        !super::proc::group_alive(pid),
        "the check's process group outlived the cancel"
    );
    // An unhandled cancel_step ends the loop failed.
    w.until("the end", |s| s.status.is_terminal()).await;
    assert_eq!(w.state.status, LoopStatus::Failed);
    h.finished().await.unwrap();
}

#[tokio::test]
async fn an_exhausted_budget_asks_and_extend_continues() {
    let _env = lock_env();
    let doc = json!({
        "schema": "agentpit.blueprint/1",
        "name": "tight",
        "budget": {"max_steps": 1, "max_active_secs": 600, "max_parallel": 1},
        "nodes": [
            {"id": "a", "kind": "check", "command": "true"},
            {"id": "b", "kind": "check", "command": "true"}
        ],
        "edges": [{"from": "a", "to": "b"}]
    });
    let mut h = Harness::new(doc, &[]);
    let (mut w, _) = h.run_watched().await;
    w.until("the budget gate", |s| {
        s.open_gates().any(|g| g.kind == GateKind::Budget)
    })
    .await;
    let gate = open_gate(&w.state).unwrap();
    let mut conn = h.connect().await;
    let resp = conn
        .request_raw(op_request(
            LoopOp::ResolveGate {
                gate_id: gate.gate_id,
                option: "extend".into(),
                comment: None,
            },
            "op-extend-1",
        ))
        .await
        .unwrap();
    assert!(resp.ok, "{resp:?}");
    w.until("the end", |s| s.status.is_terminal()).await;
    assert_eq!(w.state.status, LoopStatus::Succeeded);
    assert!(w.state.budget.max_steps > 1);
    h.finished().await.unwrap();
}

#[tokio::test]
async fn a_failing_verdict_is_feedback_and_step_files_are_readable() {
    let _env = lock_env();
    let doc = json!({
        "schema": "agentpit.blueprint/1",
        "name": "review",
        "nodes": [
            {"id": "loop", "kind": "repeat", "max_iterations": 2},
            {"id": "review", "kind": "agent", "parent": "loop", "backend": "codex",
             "verdict": true, "task": "REVIEW iteration {{iteration}}\n{{feedback}}"}
        ]
    });
    let mut h = Harness::new(doc, &[]);
    let (mut w, _) = h.run_watched().await;
    w.until("the end", |s| s.status.is_terminal()).await;
    // Two FAIL verdicts: the repeat is exhausted, which nothing handles → failed.
    assert_eq!(w.state.status, LoopStatus::Failed);
    let second = h.prompt("review.i2.a1");
    assert!(second.contains("the fix is missing a test"), "{second}");
    assert!(
        second.contains("VERDICT: PASS"),
        "the verdict contract is appended"
    );
    let step = &w.state.steps["review.i1.a1"];
    assert_eq!(step.outcome, Some(Outcome::Fail));
    h.finished().await.unwrap();
    let answer = std::fs::read_to_string(h.dir.join("outputs/review.i1.a1.md")).unwrap();
    assert!(answer.ends_with("VERDICT: FAIL\n"));
}

#[tokio::test]
async fn a_retry_keeps_the_instructions_its_first_attempt_consumed() {
    let _env = lock_env();
    let doc = json!({
        "schema": "agentpit.blueprint/1",
        "name": "flaky",
        "nodes": [{"id": "work", "kind": "agent", "backend": "codex", "retries": 1,
                   "task": "FLAKY work\n{{instructions}}"}]
    });
    let mut h = Harness::new(doc, &[]);
    h.start();
    let (mut w, _) = Watch::attach(h.connect().await, None).await;
    let mut conn = h.connect().await;
    let resp = conn
        .request_raw(op_request(
            LoopOp::Instruct {
                text: "keep the public API stable".into(),
                target: None,
            },
            "op-instruct-1",
        ))
        .await
        .unwrap();
    assert!(resp.ok, "{resp:?}");
    let resp = conn
        .request_raw(op_request(LoopOp::Start, "op-start-flaky"))
        .await
        .unwrap();
    assert!(resp.ok, "{resp:?}");
    w.until("the end", |s| s.status.is_terminal()).await;
    assert_eq!(w.state.status, LoopStatus::Succeeded);
    assert_eq!(w.state.steps["work.a1"].outcome, Some(Outcome::Error));
    assert_eq!(w.state.steps["work.a2"].cause, StartCause::Retry);
    assert!(h.prompt("work.a1").contains("keep the public API stable"));
    assert!(
        h.prompt("work.a2").contains("keep the public API stable"),
        "the retry lost the instruction:\n{}",
        h.prompt("work.a2")
    );
    h.finished().await.unwrap();
}

#[tokio::test]
async fn every_cancel_step_is_answered_with_its_own_op_id() {
    let _env = lock_env();
    // Ignores SIGTERM, so the cancel takes the full grace period: both cancels land while
    // the step is still winding down.
    let doc = json!({
        "schema": "agentpit.blueprint/1",
        "name": "slow-check",
        "nodes": [{"id": "wait", "kind": "check", "command": "trap '' TERM; touch running; sleep 30"}]
    });
    let mut h = Harness::new(doc, &[]);
    let (mut w, _) = h.run_watched().await;
    let running = h.work.join("running");
    for _ in 0..200 {
        if running.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(running.exists(), "the check never ran");
    let mut a = h.connect().await;
    let mut b = h.connect().await;
    let cancel = |op_id: &str| {
        op_request(
            LoopOp::CancelStep {
                step_id: "wait.a1".into(),
            },
            op_id,
        )
    };
    let id_a = a.send_request(cancel("op-cancel-aaaa")).await.unwrap();
    // Make sure A is registered first.
    tokio::time::sleep(Duration::from_millis(50)).await;
    let id_b = b.send_request(cancel("op-cancel-bbbb")).await.unwrap();
    let ra = answer_to(&mut a, id_a).await;
    let rb = answer_to(&mut b, id_b).await;
    let (ra, rb) = (op_result(&ra).clone(), op_result(&rb).clone());
    assert_eq!(ra.op_id, "op-cancel-aaaa");
    assert_eq!(ra.outcome, OpOutcome::Applied);
    assert_eq!(rb.op_id, "op-cancel-bbbb");
    assert_eq!(rb.outcome, OpOutcome::Noop);
    w.until("the end", |s| s.status.is_terminal()).await;
    h.finished().await.unwrap();
}

#[tokio::test]
async fn an_idle_waiting_runner_parks_and_a_new_one_resumes_the_gate() {
    let _env = lock_env();
    let doc = json!({
        "schema": "agentpit.blueprint/1",
        "name": "one-gate",
        "nodes": [{"id": "ok", "kind": "gate", "prompt": "Ship it?"}]
    });
    let mut h = Harness::new(doc, &[]);
    h.park_after = Some(Duration::from_millis(300));
    {
        let (mut w, _) = h.run_watched().await;
        w.until("the gate", |s| s.open_gates().next().is_some())
            .await;
        // Connections keep it up; once they are gone it parks.
    }
    let parked = h.finished().await;
    assert!(parked.is_ok(), "{parked:?}");
    let (state, scan) = read_loop(&h.dir).unwrap();
    assert_eq!(state.status, LoopStatus::Running);
    let last = scan.records.last().unwrap();
    assert_eq!(last.kind, "writer_closed");
    assert!(last.raw.contains("\"idle\""), "{}", last.raw);
    let head: LoopSummary =
        serde_json::from_slice(&std::fs::read(h.dir.join("head.json")).unwrap()).unwrap();
    assert!(head.waiting, "head.json still shows the waiting gate");

    // A new runner picks the gate up where it was; answering it finishes the loop.
    h.park_after = None;
    h.start();
    let (mut w, _) = Watch::attach(h.connect().await, None).await;
    w.until("the gate again", |s| s.open_gates().next().is_some())
        .await;
    let mut conn = h.connect().await;
    let resp = conn
        .request_raw(op_request(
            LoopOp::ResolveGate {
                gate_id: "g1".into(),
                option: "approve".into(),
                comment: None,
            },
            "op-approve-parked",
        ))
        .await
        .unwrap();
    assert!(resp.ok, "{resp:?}");
    w.until("the end", |s| s.status.is_terminal()).await;
    assert_eq!(w.state.status, LoopStatus::Succeeded);
    h.finished().await.unwrap();
}
