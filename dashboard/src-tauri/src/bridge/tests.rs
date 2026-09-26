//! The bridge against an in-process fake daemon and fake runner that speak the real wire
//! types, over real unix sockets, with a real loop journal on disk.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agentpit_events::loops::{
    loop_view, read_loop, replay, scan_file, CursorCheck, GateStatus, LoopOp, LoopStatus,
    OpOutcome, OpResult,
};
use agentpit_events::wire::{
    Event, LoopFile, LoopRow, Request, RequestBody, Response, ResponseData, FEATURE_LOOPS,
    PROTO_VERSION, ROLE_LOOP,
};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc, watch};

use super::*;

const FIXTURE: &str =
    include_str!("../../../../agentpit-events/tests/fixtures/loops/journal_fix_until_green.jsonl");
const LOOP_ID: &str = "lp-0199a1b2c3d47e5f8a9b0c1d2e3f4a5b";
/// seq 23 of a loop that parked while waiting at gate g1 (seq 22).
const PARKED: &str = r#"{"v":1,"seq":23,"ts":1790381026200,"kind":"writer_closed","anc":true,"data":{"epoch":1,"reason":"idle"}}"#;

fn fixture_lines(range: std::ops::RangeInclusive<usize>) -> Vec<String> {
    FIXTURE
        .lines()
        .enumerate()
        .filter(|(i, _)| range.contains(&(i + 1)))
        .map(|(_, l)| l.to_string())
        .collect()
}

// ---------------------------------------------------------------------------------------
// Host.

#[derive(Default)]
struct TestHost {
    events: Mutex<Vec<(String, Value)>>,
    starts: AtomicUsize,
    #[allow(clippy::type_complexity)]
    on_start: Mutex<Option<Box<dyn FnMut() + Send>>>,
}

impl Host for TestHost {
    fn emit(&self, event: &str, payload: Value) {
        self.events
            .lock()
            .unwrap()
            .push((event.to_string(), payload));
    }

    fn start_daemon(&self) -> BoxFuture<'_, Result<(), String>> {
        Box::pin(async move {
            self.starts.fetch_add(1, Ordering::SeqCst);
            match self.on_start.lock().unwrap().as_mut() {
                Some(start) => {
                    start();
                    Ok(())
                }
                None => Err("no daemon in this test".into()),
            }
        })
    }
}

impl TestHost {
    /// The latest `event` payload satisfying `pred`.
    fn last(&self, event: &str, pred: impl Fn(&Value) -> bool) -> Option<Value> {
        self.events
            .lock()
            .unwrap()
            .iter()
            .rev()
            .find(|(e, p)| e == event && pred(p))
            .map(|(_, p)| p.clone())
    }

    fn count(&self, event: &str) -> usize {
        self.events
            .lock()
            .unwrap()
            .iter()
            .filter(|(e, _)| e == event)
            .count()
    }
}

async fn until<T>(what: &str, mut probe: impl FnMut() -> Option<T>) -> T {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(v) = probe() {
            return v;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {what}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

// ---------------------------------------------------------------------------------------
// Fake servers.

enum Kind {
    Daemon {
        features: Vec<String>,
        rows: Mutex<Vec<LoopRow>>,
        /// `loop_ensure`'s answer: the runner socket, or an error code.
        ensure: Mutex<Result<PathBuf, String>>,
    },
    Runner {
        journal: PathBuf,
    },
}

struct Fake {
    kind: Kind,
    requests: Mutex<Vec<RequestBody>>,
    subscribers: Mutex<Vec<mpsc::UnboundedSender<String>>>,
    kill: watch::Sender<u64>,
}

impl Fake {
    fn new(kind: Kind) -> Arc<Fake> {
        Arc::new(Fake {
            kind,
            requests: Mutex::new(Vec::new()),
            subscribers: Mutex::new(Vec::new()),
            kill: watch::channel(0).0,
        })
    }

    fn daemon(rows: Vec<LoopRow>, runner: &Path) -> Arc<Fake> {
        Fake::new(Kind::Daemon {
            features: vec![FEATURE_LOOPS.into()],
            rows: Mutex::new(rows),
            ensure: Mutex::new(Ok(runner.to_path_buf())),
        })
    }

    fn runner(journal: &Path) -> Arc<Fake> {
        Fake::new(Kind::Runner {
            journal: journal.to_path_buf(),
        })
    }

    fn listen(self: &Arc<Self>, socket: &Path) {
        let listener = UnixListener::bind(socket).unwrap();
        let me = Arc::clone(self);
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                tokio::spawn(Arc::clone(&me).connection(stream));
            }
        });
    }

    fn count(&self, pred: impl Fn(&RequestBody) -> bool) -> usize {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .filter(|r| pred(r))
            .count()
    }

    fn broadcast(&self, frame: &Event) {
        let line = serde_json::to_string(frame).unwrap();
        self.subscribers
            .lock()
            .unwrap()
            .retain(|tx| tx.send(line.clone()).is_ok());
    }

    /// Drop every connection (the process died).
    fn kill_all(&self) {
        self.subscribers.lock().unwrap().clear();
        self.kill.send_modify(|n| *n += 1);
    }

    async fn connection(self: Arc<Self>, stream: UnixStream) {
        let (read, mut write) = stream.into_split();
        let mut lines = BufReader::new(read).lines();
        let (tx, mut rx) = mpsc::unbounded_channel::<String>();
        let mut kill = self.kill.subscribe();
        loop {
            tokio::select! {
                line = lines.next_line() => {
                    let Ok(Some(line)) = line else { return };
                    let req: Request = serde_json::from_str(&line).unwrap();
                    self.requests.lock().unwrap().push(req.body.clone());
                    let subscribe = matches!(
                        req.body,
                        RequestBody::LoopWatch { .. } | RequestBody::LoopAttach { .. }
                    );
                    for out in self.answer(req.id, &req.body) {
                        let _ = tx.send(out);
                    }
                    if subscribe {
                        self.subscribers.lock().unwrap().push(tx.clone());
                    }
                }
                Some(out) = rx.recv() => {
                    if write.write_all(format!("{out}\n").as_bytes()).await.is_err() {
                        return;
                    }
                }
                _ = kill.changed() => return,
            }
        }
    }

    fn answer(&self, id: u64, body: &RequestBody) -> Vec<String> {
        let line = |r: Response| serde_json::to_string(&r).unwrap();
        if let RequestBody::Hello { .. } = body {
            let (role, features) = match &self.kind {
                Kind::Daemon { features, .. } => ("daemon", features.clone()),
                Kind::Runner { .. } => (ROLE_LOOP, vec![FEATURE_LOOPS.to_string()]),
            };
            return vec![line(Response::ok(
                id,
                ResponseData::Hello {
                    proto: PROTO_VERSION,
                    role: role.into(),
                    pid: 1,
                    features,
                },
            ))];
        }
        match (&self.kind, body) {
            (Kind::Daemon { rows, .. }, RequestBody::LoopWatch { .. }) => vec![line(Response::ok(
                id,
                ResponseData::Loops {
                    loops: rows.lock().unwrap().clone(),
                },
            ))],
            (Kind::Daemon { ensure, .. }, RequestBody::LoopEnsure { loop_id }) => {
                match ensure.lock().unwrap().clone() {
                    Ok(socket) => vec![line(Response::ok(
                        id,
                        ResponseData::LoopRunner {
                            loop_id: loop_id.clone(),
                            socket: socket.display().to_string(),
                        },
                    ))],
                    Err(code) => vec![line(Response::err_code(id, &code, "no runner"))],
                }
            }
            (Kind::Daemon { .. }, RequestBody::LoopStart { .. }) => vec![line(Response::ok(
                id,
                ResponseData::LoopStarted {
                    loop_id: LOOP_ID.into(),
                    socket: None,
                    duplicate: false,
                },
            ))],
            (Kind::Runner { journal }, RequestBody::LoopAttach { since, .. }) => {
                let scan = scan_file(journal).unwrap();
                let state = replay(&scan.records);
                let (after, reset) = match since.as_ref().map(|c| state.check_cursor(c)) {
                    Some(CursorCheck::Resume { after }) => (after, false),
                    Some(CursorCheck::Reset) => (0, true),
                    None => (0, false),
                };
                let mut out = vec![line(Response::ok(
                    id,
                    ResponseData::LoopAttached {
                        loop_id: LOOP_ID.into(),
                        uid: state.uid().unwrap_or_default().to_string(),
                        head_seq: state.head_seq,
                        reset,
                    },
                ))];
                for r in scan.records.iter().filter(|r| r.seq > after) {
                    out.push(record_frame(&r.raw));
                }
                out
            }
            (Kind::Runner { journal }, RequestBody::LoopOp(req)) => {
                let head = replay(&scan_file(journal).unwrap().records).head_seq;
                vec![line(Response::ok(
                    id,
                    ResponseData::OpResult(OpResult {
                        op_id: req.op_id.clone(),
                        outcome: OpOutcome::Applied,
                        seq: Some(head + 1),
                        head_seq: head + 1,
                        result: None,
                    }),
                ))]
            }
            _ => vec![line(Response::err(id, "not in this fake"))],
        }
    }
}

fn record_frame(raw: &str) -> String {
    serde_json::to_string(&Event::LoopRecord {
        loop_id: LOOP_ID.into(),
        rec: serde_json::from_str(raw).unwrap(),
    })
    .unwrap()
}

// ---------------------------------------------------------------------------------------
// World.

struct World {
    tmp: tempfile::TempDir,
    host: Arc<TestHost>,
    bridge: Arc<Bridge>,
    loop_dir: PathBuf,
}

impl World {
    fn new(journal: &[String]) -> World {
        let tmp = tempfile::tempdir().unwrap();
        let loops_root = tmp.path().join("loops");
        let loop_dir = loops_root.join(LOOP_ID);
        std::fs::create_dir_all(&loop_dir).unwrap();
        let world_paths = Paths {
            owner_file: tmp.path().join("state/daemon/owner.json"),
            fallback_socket: tmp.path().join("fallback.sock"),
            loops_root,
        };
        std::fs::create_dir_all(world_paths.owner_file.parent().unwrap()).unwrap();
        std::fs::write(
            &world_paths.owner_file,
            serde_json::json!({
                "pid": 1,
                "start_id": "x",
                "socket": tmp.path().join("d.sock").display().to_string()
            })
            .to_string(),
        )
        .unwrap();
        let host = Arc::new(TestHost::default());
        let bridge = Bridge::new(host.clone(), world_paths);
        let world = World {
            tmp,
            host,
            bridge,
            loop_dir,
        };
        world.write_journal(journal);
        world
    }

    fn daemon_socket(&self) -> PathBuf {
        self.tmp.path().join("d.sock")
    }

    fn runner_socket(&self) -> PathBuf {
        self.tmp.path().join("r.sock")
    }

    fn journal(&self) -> PathBuf {
        agentpit_events::loops::journal_path(&self.loop_dir)
    }

    fn write_journal(&self, lines: &[String]) {
        let mut text = lines.join("\n");
        if !text.is_empty() {
            text.push('\n');
        }
        std::fs::write(self.journal(), text).unwrap();
    }

    fn append(&self, lines: &[String]) {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(self.journal())
            .unwrap();
        for l in lines {
            writeln!(f, "{l}").unwrap();
        }
    }

    fn row(&self, runner: &str) -> LoopRow {
        let (state, _) = read_loop(&self.loop_dir).unwrap();
        LoopRow {
            summary: state.summary().unwrap(),
            runner: runner.into(),
        }
    }

    /// The view folded straight from disk, as the webview receives it.
    fn disk_view(&self) -> Value {
        let (state, _) = read_loop(&self.loop_dir).unwrap();
        serde_json::to_value(loop_view(&state, true).unwrap()).unwrap()
    }

    fn view_at(&self, seq: u64) -> Option<Value> {
        self.host.last("loops:view", |p| {
            p["view"]["summary"]["head_seq"].as_u64() == Some(seq)
        })
    }
}

// ---------------------------------------------------------------------------------------
// Tests.

#[tokio::test]
async fn a_view_follows_its_runner_and_ends_equal_to_the_journal_on_disk() {
    let world = World::new(&fixture_lines(1..=10));
    let runner = Fake::runner(&world.journal());
    runner.listen(&world.runner_socket());
    let daemon = Fake::daemon(vec![world.row("live")], &world.runner_socket());
    daemon.listen(&world.daemon_socket());

    let view = world.bridge.open_loop(LOOP_ID).await.unwrap();
    assert_eq!(view.summary.head_seq, 10);
    assert!(
        view.blueprint.is_some(),
        "the canvas needs the frozen blueprint"
    );

    // It attaches from its cursor: nothing is replayed twice.
    until("the attach", || {
        (runner.count(|r| matches!(r, RequestBody::LoopAttach { .. })) == 1).then_some(())
    })
    .await;
    match &runner.requests.lock().unwrap()[1] {
        RequestBody::LoopAttach {
            since: Some(c),
            chunks: true,
        } => assert_eq!(c.seq, 10),
        other => panic!("{other:?}"),
    }

    // Live records move the view.
    let more = fixture_lines(11..=15);
    world.append(&more);
    for l in &more {
        runner.broadcast(&serde_json::from_str(&record_frame(l)).unwrap());
    }
    until("seq 15", || world.view_at(15)).await;

    // Live output is batched per step.
    for (offset, text) in [(0u64, "run"), (3, "ning "), (8, "tests")] {
        runner.broadcast(&Event::LoopChunk {
            loop_id: LOOP_ID.into(),
            step_id: "implement.i2.a1".into(),
            offset,
            text: text.into(),
        });
    }
    let chunks = until("output", || {
        let all: String = world
            .host
            .events
            .lock()
            .unwrap()
            .iter()
            .filter(|(e, _)| e == "loops:chunks")
            .flat_map(|(_, p)| p["chunks"].as_array().cloned().unwrap_or_default())
            .map(|c| c["text"].as_str().unwrap_or_default().to_string())
            .collect();
        (all == "running tests").then_some(all)
    })
    .await;
    assert_eq!(chunks, "running tests");

    // The runner writes the rest and dies before the bridge reads it: the journal on disk
    // is the truth, and the final view is exactly its fold.
    world.append(&fixture_lines(16..=26));
    runner.kill_all();
    let last = until("the finished view", || world.view_at(26)).await;
    assert_eq!(last["view"], world.disk_view());
    assert_eq!(last["view"]["summary"]["status"], "succeeded");
    world.bridge.close_loop(LOOP_ID);
}

#[tokio::test]
async fn a_reset_cursor_replays_the_journal_without_duplicates() {
    let world = World::new(&fixture_lines(1..=12));
    // The runner serves another incarnation of the loop (deleted and recreated under the
    // same id, so another uid): it resets the bridge's cursor and replays from seq 1.
    let other = world.tmp.path().join("other.jsonl");
    let recreated: Vec<String> = fixture_lines(1..=5)
        .into_iter()
        .map(|l| l.replace("9c41e07a2b5d4f18", "0000111122223333"))
        .collect();
    std::fs::write(&other, recreated.join("\n") + "\n").unwrap();
    let runner = Fake::runner(&other);
    runner.listen(&world.runner_socket());
    let daemon = Fake::daemon(vec![world.row("live")], &world.runner_socket());
    daemon.listen(&world.daemon_socket());

    let view = world.bridge.open_loop(LOOP_ID).await.unwrap();
    assert_eq!(view.summary.head_seq, 12);
    let reset = until("the replayed view", || {
        world.host.last("loops:view", |p| {
            p["view"]["summary"]["uid"] == "0000111122223333"
                && p["view"]["summary"]["head_seq"] == 5
        })
    })
    .await;
    let expected = loop_view(&replay(&scan_file(&other).unwrap().records), true).unwrap();
    assert_eq!(reset["view"], serde_json::to_value(expected).unwrap());
    assert!(
        reset["view"]["warnings"]
            .as_array()
            .is_none_or(|w| w.is_empty()),
        "a replay after a reset must not look like duplicate records: {}",
        reset["view"]["warnings"]
    );
}

#[tokio::test]
async fn looking_at_a_parked_loop_does_not_wake_it() {
    let mut parked = fixture_lines(1..=22);
    parked.push(PARKED.to_string());
    let world = World::new(&parked);
    let runner = Fake::runner(&world.journal());
    runner.listen(&world.runner_socket());
    // The daemon's row may still say "live" for a moment after the runner parked.
    let daemon = Fake::daemon(vec![world.row("live")], &world.runner_socket());
    daemon.listen(&world.daemon_socket());
    world.bridge.board_snapshot();
    until("the board", || world.host.last("loops:board", |_| true)).await;

    let view = world.bridge.open_loop(LOOP_ID).await.unwrap();
    assert!(view.summary.waiting);
    tokio::time::sleep(Duration::from_millis(700)).await;
    assert_eq!(
        daemon.count(|r| matches!(r, RequestBody::LoopEnsure { .. })),
        0,
        "opening a parked loop must not ensure its runner"
    );

    // Answering the gate wakes it (through the daemon), and the open view attaches.
    let result = world
        .bridge
        .control(
            LOOP_ID,
            LoopOp::ResolveGate {
                gate_id: "g1".into(),
                option: "approve".into(),
                comment: None,
            },
            None,
            None,
        )
        .await
        .unwrap();
    assert_eq!(result.outcome, OpOutcome::Applied);
    assert_eq!(
        daemon.count(|r| matches!(r, RequestBody::LoopEnsure { .. })),
        1
    );
    until("the view to attach", || {
        (runner.count(|r| matches!(r, RequestBody::LoopAttach { .. })) == 1).then_some(())
    })
    .await;
    let op = runner
        .requests
        .lock()
        .unwrap()
        .iter()
        .find_map(|r| match r {
            RequestBody::LoopOp(op) => Some(op.clone()),
            _ => None,
        })
        .unwrap();
    assert_eq!(op.loop_id, LOOP_ID);
    assert!(matches!(op.op, LoopOp::ResolveGate { .. }));
}

#[tokio::test]
async fn a_parked_loop_is_followed_on_disk() {
    let mut parked = fixture_lines(1..=22);
    parked.push(PARKED.to_string());
    let world = World::new(&parked);
    let daemon = Fake::daemon(vec![world.row("absent")], &world.runner_socket());
    daemon.listen(&world.daemon_socket());
    world.bridge.open_loop(LOOP_ID).await.unwrap();

    // Someone else (the CLI) woke it and it finished; no runner is reachable from here.
    let mut rest = vec![
        r#"{"v":1,"seq":0,"ts":1790381210000,"kind":"writer_opened","data":{"epoch":2,"pid":41300,"start_id":"887799","build":"0.3.0","schema_minor":0}}"#
            .to_string(),
    ];
    rest.extend(fixture_lines(23..=26));
    let rest: Vec<String> = rest
        .into_iter()
        .enumerate()
        .map(|(i, l)| {
            let mut v: Value = serde_json::from_str(&l).unwrap();
            v["seq"] = (24 + i as u64).into();
            if v["kind"] == "writer_closed" {
                v["data"]["epoch"] = 2.into();
            }
            v.to_string()
        })
        .collect();
    world.append(&rest);
    let (state, scan) = read_loop(&world.loop_dir).unwrap();
    assert!(scan.is_clean(), "{:?}", scan.issues);
    assert_eq!(state.status, LoopStatus::Succeeded);
    let done = until("the finished view", || world.view_at(state.head_seq)).await;
    assert_eq!(done["view"], world.disk_view());
    assert_eq!(
        daemon.count(|r| matches!(r, RequestBody::LoopEnsure { .. })),
        0
    );
}

#[tokio::test]
async fn the_board_follows_rows_and_the_inbox_remembers_who_answered() {
    let world = World::new(&fixture_lines(1..=22));
    let daemon = Fake::daemon(vec![world.row("absent")], &world.runner_socket());
    daemon.listen(&world.daemon_socket());

    let snap = world.bridge.board_snapshot();
    assert!(
        snap.rows.is_empty(),
        "nothing is known before the watch answers"
    );
    let inbox = until("the inbox", || {
        world.host.last("loops:inbox", |p| {
            p["gates"].as_array().is_some_and(|g| !g.is_empty())
        })
    })
    .await;
    assert_eq!(inbox["gates"][0]["gate_id"], "g1");
    assert_eq!(inbox["gates"][0]["parked"], true);
    assert_eq!(inbox["gates"][0]["loop_id"], LOOP_ID);
    let status = until("connected", || {
        world
            .host
            .last("daemon:status", |p| p["state"] == "connected")
    })
    .await;
    assert_eq!(status["state"], "connected");

    // The gate is answered elsewhere; the row drops it and the journal says who.
    world.append(&fixture_lines(23..=26));
    daemon.broadcast(&Event::LoopRow {
        row: Box::new(world.row("absent")),
    });
    let answered = until("the answer", || {
        world.host.last("loops:inbox", |p| {
            p["answered"].as_array().is_some_and(|a| !a.is_empty())
        })
    })
    .await;
    assert_eq!(answered["answered"][0]["gate_id"], "g1");
    assert_eq!(answered["answered"][0]["option"], "approve");
    assert_eq!(answered["answered"][0]["option_label"], "Approve");
    assert_eq!(answered["answered"][0]["by"]["kind"], "human");
    assert!(answered["gates"].as_array().unwrap().is_empty());

    daemon.broadcast(&Event::LoopGone {
        loop_id: LOOP_ID.into(),
    });
    until("the loop to leave the board", || {
        world.host.last("loops:board", |p| {
            p["rows"].as_array().is_some_and(|r| r.is_empty())
        })
    })
    .await;
    let snap = world.bridge.board_snapshot();
    assert!(snap.rows.is_empty());
    assert_eq!(snap.inbox.answered.len(), 1);
}

#[tokio::test]
async fn the_board_starts_the_daemon_when_none_answers_and_reconnects_after_it_dies() {
    let world = World::new(&fixture_lines(1..=5));
    let socket = world.daemon_socket();
    let row = world.row("live");
    let runner_socket = world.runner_socket();
    let daemons: Arc<Mutex<Vec<Arc<Fake>>>> = Arc::default();
    let started = Arc::clone(&daemons);
    *world.host.on_start.lock().unwrap() = Some(Box::new(move || {
        let _ = std::fs::remove_file(&socket);
        let d = Fake::daemon(vec![row.clone()], &runner_socket);
        d.listen(&socket);
        started.lock().unwrap().push(d);
    }));

    world.bridge.board_snapshot();
    until("the first row", || {
        world.host.last("loops:board", |p| {
            p["rows"].as_array().is_some_and(|r| r.len() == 1)
        })
    })
    .await;
    assert_eq!(world.host.starts.load(Ordering::SeqCst), 1);
    assert_eq!(
        world.bridge.daemon_status().state,
        board::StatusState::Connected
    );

    // The daemon dies: the board says so and comes back once one answers again.
    let first = Arc::clone(&daemons.lock().unwrap()[0]);
    first.kill_all();
    let _ = std::fs::remove_file(world.daemon_socket());
    until("not connected", || {
        (world.bridge.daemon_status().state != board::StatusState::Connected).then_some(())
    })
    .await;
    let again = Fake::daemon(vec![world.row("live")], &world.runner_socket());
    again.listen(&world.daemon_socket());
    until("reconnected", || {
        (again.count(|r| matches!(r, RequestBody::LoopWatch { .. })) == 1).then_some(())
    })
    .await;
    until("connected again", || {
        (world.bridge.daemon_status().state == board::StatusState::Connected).then_some(())
    })
    .await;
    assert!(world.host.count("loops:board") >= 2);
}

#[tokio::test]
async fn an_old_daemon_is_reported_instead_of_used() {
    let world = World::new(&fixture_lines(1..=5));
    let old = Fake::new(Kind::Daemon {
        features: vec![],
        rows: Mutex::new(vec![]),
        ensure: Mutex::new(Err("unsupported".into())),
    });
    old.listen(&world.daemon_socket());
    let err = world.bridge.daemon(true).await.err().unwrap();
    assert_eq!(err.code, "daemon_outdated");
    assert_eq!(world.host.starts.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn operations_on_a_finished_loop_carry_the_daemons_error() {
    let world = World::new(&fixture_lines(1..=26));
    let daemon = Fake::daemon(vec![], &world.runner_socket());
    *match &daemon.kind {
        Kind::Daemon { ensure, .. } => ensure,
        Kind::Runner { .. } => unreachable!(),
    }
    .lock()
    .unwrap() = Err("gone".into());
    daemon.listen(&world.daemon_socket());
    let err = world
        .bridge
        .control(LOOP_ID, LoopOp::Resume, None, None)
        .await
        .unwrap_err();
    assert_eq!(err.code, "gone");
    let bad = world
        .bridge
        .control("lp-nope", LoopOp::Resume, None, None)
        .await
        .unwrap_err();
    assert_eq!(bad.code, "bad_request");
    let bad_op = world
        .bridge
        .control(LOOP_ID, LoopOp::Resume, Some("../x".into()), None)
        .await
        .unwrap_err();
    assert_eq!(bad_op.code, "bad_request");
}

#[tokio::test]
async fn starting_a_loop_goes_through_the_daemon_with_the_dashboard_origin() {
    let world = World::new(&fixture_lines(1..=5));
    let daemon = Fake::daemon(vec![], &world.runner_socket());
    daemon.listen(&world.daemon_socket());
    let cwd = world.tmp.path().display().to_string();
    let req: StartRequest = serde_json::from_value(serde_json::json!({
        "blueprint": {"source": "named", "name": "fix-until-green"},
        "inputs": {"goal": "fix it"},
        "cwd": cwd,
        "op_id": "0199a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5b"
    }))
    .unwrap();
    let started = world.bridge.start_loop(req.clone()).await.unwrap();
    assert_eq!(started.loop_id, LOOP_ID);
    let sent = daemon
        .requests
        .lock()
        .unwrap()
        .iter()
        .find_map(|r| match r {
            RequestBody::LoopStart {
                op_id,
                origin,
                start,
                inputs,
                ..
            } => Some((op_id.clone(), origin.clone(), *start, inputs.clone())),
            _ => None,
        })
        .unwrap();
    assert_eq!(sent.0, "0199a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5b");
    let origin = sent.1.unwrap();
    assert_eq!(origin.surface, agentpit_events::loops::Surface::Dashboard);
    assert!(origin.client.unwrap().starts_with("agentpit-dashboard/"));
    assert!(sent.2, "start defaults to true");
    assert_eq!(sent.3["goal"], "fix it");

    let mut relative = req;
    relative.cwd = "relative".into();
    assert_eq!(
        world.bridge.start_loop(relative).await.unwrap_err().code,
        "bad_request"
    );
}

#[tokio::test]
async fn step_output_pages_the_file_on_disk_for_steps_of_the_loop_only() {
    let world = World::new(&fixture_lines(1..=12));
    std::fs::create_dir_all(world.loop_dir.join("outputs")).unwrap();
    std::fs::write(
        world.loop_dir.join("outputs/implement.i1.a1.log"),
        "hello world",
    )
    .unwrap();
    let tail = world
        .bridge
        .step_output(LOOP_ID, "implement.i1.a1", LoopFile::Output, None, Some(5))
        .await
        .unwrap();
    assert_eq!(
        (tail.offset, tail.next_offset, tail.size, tail.text.as_str()),
        (6, 11, 11, "world")
    );
    let head = world
        .bridge
        .step_output(
            LOOP_ID,
            "implement.i1.a1",
            LoopFile::Output,
            Some(0),
            Some(5),
        )
        .await
        .unwrap();
    assert_eq!(head.text, "hello");
    // Not written yet: empty, not an error.
    let prompt = world
        .bridge
        .step_output(LOOP_ID, "plan.a1", LoopFile::Prompt, Some(0), None)
        .await
        .unwrap();
    assert_eq!(prompt.size, 0);
    assert_eq!(
        world
            .bridge
            .step_output(LOOP_ID, "signoff.a1", LoopFile::Output, None, None)
            .await
            .unwrap_err()
            .code,
        "not_found"
    );
    assert_eq!(
        world
            .bridge
            .step_output(LOOP_ID, "../../etc", LoopFile::Output, None, None)
            .await
            .unwrap_err()
            .code,
        "bad_request"
    );
}

#[tokio::test]
async fn the_gate_resolution_in_the_final_view_names_who_answered() {
    let world = World::new(&fixture_lines(1..=26));
    let daemon = Fake::daemon(vec![], &world.runner_socket());
    daemon.listen(&world.daemon_socket());
    let view = world.bridge.open_loop(LOOP_ID).await.unwrap();
    let g1 = view.gates.iter().find(|g| g.gate_id == "g1").unwrap();
    assert_eq!(g1.status, GateStatus::Resolved);
    assert_eq!(
        g1.resolution.as_ref().unwrap().by.client.as_deref(),
        Some("agentpit-dashboard/0.3.0")
    );
    // A finished loop never needs a runner.
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert_eq!(
        daemon.count(|r| matches!(r, RequestBody::LoopEnsure { .. })),
        0
    );
}
