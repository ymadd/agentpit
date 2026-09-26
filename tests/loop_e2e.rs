//! End to end through the real binary: the CLI talks to an autostarted daemon, which
//! spawns the loop's runner; the runner is SIGKILLed mid-step and the next `loop` command
//! brings the loop back (design §17 P2, acceptance 3 with a check step).

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

use serde_json::Value;

struct Env {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    work: PathBuf,
    /// Extra environment for every command (and so for the daemon and its runners).
    extra: Vec<(&'static str, &'static str)>,
}

impl Env {
    fn new() -> Env {
        // Short: the runtime dir must leave room for socket names (sun_path).
        let tmp = tempfile::Builder::new()
            .prefix("ape2e")
            .tempdir_in("/tmp")
            .unwrap();
        let root = tmp.path().to_path_buf();
        let work = root.join("w");
        for d in ["state", "run", "config", "w"] {
            std::fs::create_dir_all(root.join(d)).unwrap();
        }
        Env {
            _tmp: tmp,
            root,
            work,
            extra: vec![],
        }
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_agentpit"));
        cmd.args(args)
            .current_dir(&self.work)
            .env("XDG_STATE_HOME", self.root.join("state"))
            .env("XDG_RUNTIME_DIR", self.root.join("run"))
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("NO_COLOR", "1")
            .env_remove("AGENTPIT_PARENT_RUN_ID");
        for (k, v) in &self.extra {
            cmd.env(k, v);
        }
        cmd
    }

    fn cmd(&self, args: &[&str]) -> Output {
        self.command(args).output().unwrap()
    }

    fn ok(&self, args: &[&str]) -> String {
        let out = self.cmd(args);
        assert!(
            out.status.success(),
            "agentpit {args:?} failed:\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    fn show(&self, loop_id: &str) -> Value {
        serde_json::from_str(&self.ok(&["loop", "show", loop_id, "--json"])).unwrap()
    }

    fn loop_dir(&self, loop_id: &str) -> PathBuf {
        self.root.join("state/agentpit/loops").join(loop_id)
    }

    fn records(&self, loop_id: &str) -> Vec<Value> {
        std::fs::read_to_string(self.loop_dir(loop_id).join("journal.jsonl"))
            .unwrap_or_default()
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect()
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        let _ = self.cmd(&["daemon", "stop"]);
        // Any runner left behind (a failed assertion mid-test).
        if let Ok(entries) = std::fs::read_dir(self.root.join("state/agentpit/daemon/loops")) {
            for e in entries.flatten() {
                if let Ok(v) =
                    serde_json::from_str::<Value>(&std::fs::read_to_string(e.path()).unwrap())
                    && let Some(pid) = v["pid"].as_u64()
                {
                    let _ = Command::new("kill").args(["-9", &pid.to_string()]).status();
                }
            }
        }
    }
}

fn wait_for(what: &str, timeout: Duration, mut f: impl FnMut() -> bool) {
    let deadline = Instant::now() + timeout;
    while !f() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn alive(pid: u64) -> bool {
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

fn write_blueprint(dir: &Path) -> PathBuf {
    let path = dir.join("bp.json");
    std::fs::write(
        &path,
        serde_json::json!({
            "schema": "agentpit.blueprint/1",
            "name": "check-then-signoff",
            "inputs": {"goal": {"required": true}},
            "nodes": [
                {"id": "work", "kind": "check",
                 "command": "[ -f done ] && exit 0; echo $$ > started; sleep 60"},
                {"id": "signoff", "kind": "gate", "prompt": "Goal {{goal}} done?"}
            ],
            "edges": [{"from": "work", "to": "signoff"}]
        })
        .to_string(),
    )
    .unwrap();
    path
}

#[test]
fn a_killed_runner_is_recovered_by_the_next_command_and_the_loop_completes() {
    let env = Env::new();
    let bp = write_blueprint(&env.work);

    // A bad blueprint is refused before any daemon is involved.
    std::fs::write(
        env.work.join("bad.json"),
        r#"{"schema":"agentpit.blueprint/1"}"#,
    )
    .unwrap();
    let out = env.cmd(&["loop", "validate", "bad.json"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("error["));
    assert!(
        env.ok(&["loop", "validate", bp.to_str().unwrap()])
            .contains("ok check-then-signoff")
    );

    let started: Value = serde_json::from_str(&env.ok(&[
        "loop",
        "start",
        bp.to_str().unwrap(),
        "--goal",
        "ship",
        "--json",
    ]))
    .unwrap();
    let loop_id = started["loop_id"].as_str().unwrap().to_string();

    // The check is running (its shell wrote its pid) and its process group is journaled.
    let started_file = env.work.join("started");
    wait_for("the check to start", Duration::from_secs(20), || {
        started_file.exists()
            && env
                .records(&loop_id)
                .iter()
                .any(|r| r["kind"] == "step_spawned")
    });
    let check_pid = env
        .records(&loop_id)
        .iter()
        .find(|r| r["kind"] == "step_spawned")
        .and_then(|r| r["data"]["pid"].as_u64())
        .unwrap();
    let registry = env
        .root
        .join(format!("state/agentpit/daemon/loops/{loop_id}.json"));
    let runner: Value = serde_json::from_str(&std::fs::read_to_string(&registry).unwrap()).unwrap();
    let runner_pid = runner["pid"].as_u64().unwrap();

    // Crash the runner. Its check keeps running, orphaned.
    assert!(
        Command::new("kill")
            .args(["-9", &runner_pid.to_string()])
            .status()
            .unwrap()
            .success()
    );
    wait_for("the runner to die", Duration::from_secs(10), || {
        !alive(runner_pid)
    });
    assert!(alive(check_pid), "the check should outlive its runner");
    std::fs::write(env.work.join("done"), "").unwrap();

    // Any command that acts on the loop brings the runner back (`show` only reads the
    // journal): it kills the orphan, records the interruption, and retries the check.
    env.ok(&["loop", "resume", &loop_id]);
    wait_for("the sign-off gate", Duration::from_secs(30), || {
        env.show(&loop_id)["waiting"] == true
    });
    assert!(!alive(check_pid), "the orphaned check survived recovery");
    let records = env.records(&loop_id);
    let kinds: Vec<&str> = records.iter().filter_map(|r| r["kind"].as_str()).collect();
    assert!(kinds.contains(&"step_interrupted"), "{kinds:?}");
    let retry = records
        .iter()
        .find(|r| r["kind"] == "step_started" && r["data"]["step_id"] == "work.a2")
        .expect("the check was retried");
    assert_eq!(retry["data"]["cause"], "recovery");
    let epochs: Vec<u64> = records
        .iter()
        .filter(|r| r["kind"] == "writer_opened")
        .filter_map(|r| r["data"]["epoch"].as_u64())
        .collect();
    assert_eq!(epochs, vec![1, 2, 3]);

    // Answer the gate from the CLI; the loop finishes and its runner leaves.
    let short = &loop_id[loop_id.len() - 8..];
    let gate = env.show(&loop_id)["open_gates"][0]["gate_id"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(
        env.ok(&["loop", "gate", short, &gate, "approve"])
            .contains("done")
    );
    wait_for("the loop to succeed", Duration::from_secs(20), || {
        env.show(&loop_id)["status"] == "succeeded"
    });
    let listed: Value = serde_json::from_str(&env.ok(&["loop", "ls", "--all", "--json"])).unwrap();
    assert_eq!(listed[0]["summary"]["status"], "succeeded");
    let watched = env.ok(&["loop", "watch", short]);
    assert!(watched.contains("finished succeeded"), "{watched}");
    assert!(watched.contains("runner restarted (epoch 3)"), "{watched}");
}

#[test]
fn a_damaged_journal_is_shown_from_disk_and_refuses_changes() {
    let env = Env::new();
    let bp = env.work.join("gate.json");
    std::fs::write(
        &bp,
        serde_json::json!({
            "schema": "agentpit.blueprint/1",
            "name": "gate-only",
            "nodes": [{"id": "ok", "kind": "gate", "prompt": "Ship it?"}]
        })
        .to_string(),
    )
    .unwrap();
    let started: Value =
        serde_json::from_str(&env.ok(&["loop", "start", bp.to_str().unwrap(), "--json"])).unwrap();
    let loop_id = started["loop_id"].as_str().unwrap().to_string();
    wait_for("the gate", Duration::from_secs(20), || {
        env.show(&loop_id)["waiting"] == true
    });

    // Crash the runner, then damage a line in the middle of the journal.
    let registry = env
        .root
        .join(format!("state/agentpit/daemon/loops/{loop_id}.json"));
    let runner: Value = serde_json::from_str(&std::fs::read_to_string(&registry).unwrap()).unwrap();
    let runner_pid = runner["pid"].as_u64().unwrap();
    Command::new("kill")
        .args(["-9", &runner_pid.to_string()])
        .status()
        .unwrap();
    wait_for("the runner to die", Duration::from_secs(10), || {
        !alive(runner_pid)
    });
    let journal = env.loop_dir(&loop_id).join("journal.jsonl");
    let text = std::fs::read_to_string(&journal).unwrap();
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
    lines[2] = "{this is not json}".into();
    std::fs::write(&journal, lines.join("\n") + "\n").unwrap();

    // `show` answers at once, from disk; an operation says why it cannot be done.
    let begin = Instant::now();
    let summary = env.show(&loop_id);
    assert!(
        begin.elapsed() < Duration::from_secs(5),
        "show took {:?}",
        begin.elapsed()
    );
    assert_eq!(summary["loop_id"], loop_id.as_str());
    let out = env.cmd(&["loop", "gate", &loop_id, "g1", "approve"]);
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("damaged"), "{err}");
}

#[test]
fn a_parked_loop_wakes_when_its_gate_is_answered_and_watchers_see_it() {
    let mut env = Env::new();
    env.extra.push(("AGENTPIT_LOOP_PARK_SECS", "1"));
    let bp = env.work.join("gate.json");
    std::fs::write(
        &bp,
        serde_json::json!({
            "schema": "agentpit.blueprint/1",
            "name": "gate-only",
            "nodes": [{"id": "ok", "kind": "gate", "prompt": "Ship it?"}]
        })
        .to_string(),
    )
    .unwrap();
    let started: Value =
        serde_json::from_str(&env.ok(&["loop", "start", bp.to_str().unwrap(), "--json"])).unwrap();
    let loop_id = started["loop_id"].as_str().unwrap().to_string();

    // With nobody attached, the waiting runner parks: its journal is closed `idle` and its
    // registration is gone, but the board row still shows the gate.
    let registry = env
        .root
        .join(format!("state/agentpit/daemon/loops/{loop_id}.json"));
    wait_for("the runner to park", Duration::from_secs(20), || {
        env.records(&loop_id)
            .last()
            .is_some_and(|r| r["kind"] == "writer_closed" && r["data"]["reason"] == "idle")
            && !registry.exists()
    });
    assert_eq!(env.show(&loop_id)["waiting"], true);

    // A watcher sees the loop change live.
    let mut watcher = env
        .command(&["loop", "ls", "--watch", "--all", "--json"])
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let stdout = watcher.stdout.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    std::thread::spawn(move || {
        use std::io::BufRead;
        for line in std::io::BufReader::new(stdout)
            .lines()
            .map_while(Result::ok)
        {
            if tx.send(line).is_err() {
                break;
            }
        }
    });

    // Answering wakes it and the loop completes.
    let begin = Instant::now();
    assert!(
        env.ok(&["loop", "gate", &loop_id, "g1", "approve"])
            .contains("done")
    );
    wait_for("the loop to succeed", Duration::from_secs(20), || {
        env.show(&loop_id)["status"] == "succeeded"
    });
    let took = begin.elapsed();
    assert!(took < Duration::from_secs(5), "waking took {took:?}");
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut seen = false;
    while Instant::now() < deadline && !seen {
        if let Ok(line) = rx.recv_timeout(Duration::from_millis(200)) {
            seen = line.contains(&loop_id) && line.contains("\"succeeded\"");
        }
    }
    let _ = watcher.kill();
    let _ = watcher.wait();
    assert!(seen, "the watcher never saw the loop finish");
}

/// A display that keeps itself up: `loop ls --watch --all --json` (the board's feed),
/// restarted whenever it exits (its daemon died) or is killed (the app died), remembering
/// the last row it printed for each loop.
struct Display {
    rows: std::sync::Arc<std::sync::Mutex<std::collections::BTreeMap<String, Value>>>,
    child: std::sync::Arc<std::sync::Mutex<Option<std::process::Child>>>,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Display {
    fn start(env: &Env) -> Display {
        use std::io::BufRead;
        use std::sync::atomic::Ordering;
        let rows: std::sync::Arc<std::sync::Mutex<std::collections::BTreeMap<String, Value>>> =
            Default::default();
        let child: std::sync::Arc<std::sync::Mutex<Option<std::process::Child>>> =
            Default::default();
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut cmd = env.command(&["loop", "ls", "--watch", "--all", "--json"]);
        cmd.stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null());
        let (r, c, s) = (rows.clone(), child.clone(), stop.clone());
        let thread = std::thread::spawn(move || {
            while !s.load(Ordering::SeqCst) {
                let Ok(mut proc) = cmd.spawn() else {
                    std::thread::sleep(Duration::from_millis(200));
                    continue;
                };
                let stdout = proc.stdout.take().unwrap();
                *c.lock().unwrap() = Some(proc);
                for line in std::io::BufReader::new(stdout)
                    .lines()
                    .map_while(Result::ok)
                {
                    if let Ok(v) = serde_json::from_str::<Value>(&line)
                        && let Some(id) = v["summary"]["loop_id"].as_str()
                    {
                        r.lock()
                            .unwrap()
                            .insert(id.to_string(), v["summary"].clone());
                    }
                }
                if let Some(mut p) = c.lock().unwrap().take() {
                    let _ = p.wait();
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        });
        Display {
            rows,
            child,
            stop,
            thread: Some(thread),
        }
    }

    /// Kill the display process (it is restarted, and starts from a fresh snapshot).
    fn kill(&self) {
        if let Some(p) = self.child.lock().unwrap().as_mut() {
            let _ = p.kill();
        }
    }

    fn row(&self, loop_id: &str) -> Option<Value> {
        self.rows.lock().unwrap().get(loop_id).cloned()
    }
}

impl Drop for Display {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
        self.kill();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn pid_in(path: &Path) -> Option<u64> {
    let v: Value = serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
    v["pid"].as_u64()
}

#[test]
fn random_kills_of_daemon_runner_and_display_end_on_what_loop_show_says() {
    let env = Env::new();
    // Ten short check attempts before one passes: plenty of commits to land kills between.
    let bp = env.work.join("ticks.json");
    std::fs::write(
        &bp,
        serde_json::json!({
            "schema": "agentpit.blueprint/1",
            "name": "ticks",
            "budget": {"max_steps": 200, "max_active_secs": 600, "max_parallel": 1},
            "nodes": [
                {"id": "until", "kind": "repeat", "max_iterations": 20},
                {"id": "tick", "kind": "check", "parent": "until",
                 "command": "n=$(cat count 2>/dev/null || echo 0); n=$((n+1)); echo $n > count; sleep 0.3; [ $n -ge 10 ]"}
            ]
        })
        .to_string(),
    )
    .unwrap();
    let started: Value =
        serde_json::from_str(&env.ok(&["loop", "start", bp.to_str().unwrap(), "--json"])).unwrap();
    let loop_id = started["loop_id"].as_str().unwrap().to_string();
    let display = Display::start(&env);

    // A fixed seed keeps a failure reproducible; it is printed to find it again.
    let mut seed: u64 = 0x9e37_79b9_7f4a_7c15;
    let mut next = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    let registry = env
        .root
        .join(format!("state/agentpit/daemon/loops/{loop_id}.json"));
    let owner = env.root.join("state/agentpit/daemon/owner.json");
    let mut kills = Vec::new();
    for _ in 0..20 {
        std::thread::sleep(Duration::from_millis(150 + next() % 350));
        // Kill the chosen process; when it is not running right now, kill the display.
        let target = match next() % 3 {
            0 => pid_in(&registry)
                .filter(|p| alive(*p))
                .map(|pid| ("runner", pid)),
            1 => pid_in(&owner)
                .filter(|p| alive(*p))
                .map(|pid| ("daemon", pid)),
            _ => None,
        };
        match target {
            Some((what, pid)) => {
                let _ = Command::new("kill").args(["-9", &pid.to_string()]).status();
                kills.push(what);
            }
            None => {
                display.kill();
                kills.push("display");
            }
        }
        // What an open canvas does: bring a runner that should be running back (this also
        // autostarts the daemon). Refused once the loop has finished, which is fine.
        let _ = env.cmd(&["loop", "resume", &loop_id]);
    }
    eprintln!("kills: {kills:?}");
    assert_eq!(kills.len(), 20);

    wait_for("the loop to finish", Duration::from_secs(90), || {
        let _ = env.cmd(&["loop", "resume", &loop_id]);
        env.show(&loop_id)["status"] == "succeeded"
    });
    // The display ends on exactly the state `loop show` folds from the journal.
    let truth = env.show(&loop_id);
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if display.row(&loop_id).as_ref() == Some(&truth) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the display ended on {}\nbut loop show says {}\n(kills: {kills:?})",
            serde_json::to_string_pretty(&display.row(&loop_id)).unwrap(),
            serde_json::to_string_pretty(&truth).unwrap()
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    // And the journal stayed one valid history through every crash.
    let records = env.records(&loop_id);
    let seqs: Vec<u64> = records.iter().filter_map(|r| r["seq"].as_u64()).collect();
    assert_eq!(seqs, (1..=seqs.len() as u64).collect::<Vec<_>>());
}
