//! Orchestration REPL (design §10): TypeScript cells in a sandboxed Deno sidecar, with
//! the session worker's dispatch machinery as the only exit to the world.
//!
//! The deno process gets `--allow-read=<cwd>,<artifacts>` and
//! `--allow-write=<artifacts>` and NOTHING else — no net, no run, no env. Model-written
//! code therefore cannot act directly; every effect flows through `host_call` frames that
//! the embedding worker answers (dispatch → TurnEngine with full telemetry, store → disk
//! under the session's artifacts dir). Values persist across cells on the deno heap
//! (`S.x = …`), so large intermediate products never transit the manager's context
//! (§10.6); crash durability comes from `store` + the cell log, never a heap snapshot
//! (§10.7).

use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout};

use crate::events::state_dir;

/// The bootstrap program run inside deno (embedded — no runtime file to install).
pub const BOOTSTRAP_TS: &str = include_str!("bootstrap.ts");

/// The declaration header prepended to the virtual module `deno check` sees (§10.5).
/// It mirrors bootstrap.ts's runtime API exactly — drift between the two is a bug.
pub const DECL_HEADER: &str = r#"declare const S: Record<string, any>;
declare function dispatch(task: string, opts?: { backend?: string }): Promise<{ backend: string; status: string; answer: string }>;
declare const store: { put(key: string, value: unknown): Promise<void>; get(key: string): Promise<unknown>; list(): Promise<string[]> };
declare const session: { answers(n?: number): Promise<[string, string][]> };
declare function preview(value: unknown, n?: number): string;
"#;

/// `ext_type` of a logged REPL cell in the session JSONL (§10.7).
pub const REPL_CELL_EXT_TYPE: &str = "agentpit.repl_cell";

/// A host function invocation surfaced by the deno side mid-cell.
#[derive(Debug, Clone, Deserialize)]
pub struct HostCall {
    pub id: u64,
    pub r#fn: String,
    pub args: Value,
}

#[derive(Debug, Clone, Serialize)]
struct HostResult<'a> {
    r#type: &'static str,
    id: u64,
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    value: Option<&'a Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<&'a str>,
}

/// How a cell ended.
#[derive(Debug, Clone, PartialEq)]
pub enum CellOutcome {
    /// Ran to completion; `repr` is the truncated display value (§10.6).
    Ok { repr: String },
    /// Threw at runtime.
    RuntimeError { error: String },
    /// Refused before execution by `deno check` (§10.5).
    CheckFailed { error: String },
}

/// Locate the deno binary: explicit config path first, then `$PATH`. The error carries
/// the install command (A1 discipline) — this feature is simply off without deno (§10.3).
pub fn find_deno(configured: &str) -> Result<PathBuf> {
    if !configured.is_empty() {
        let p = PathBuf::from(configured);
        if p.exists() {
            return Ok(p);
        }
        return Err(anyhow!(
            "[repl] deno_path = \"{configured}\" does not exist. Fix the path or clear it to use $PATH."
        ));
    }
    which_deno().ok_or_else(|| {
        anyhow!(
            "the orchestration REPL needs Deno, which was not found on $PATH. \
             Install it with `brew install deno` (or set [repl] deno_path in config.toml)."
        )
    })
}

fn which_deno() -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join("deno"))
        .find(|candidate| candidate.is_file())
}

/// Per-session artifacts dir (`store/` + `checks/` live under it), created on demand.
pub fn artifacts_dir(session_id: &str) -> PathBuf {
    state_dir().join("sessions-artifacts").join(session_id)
}

/// A live deno sidecar bound to one session. One cell at a time (enforced by the caller
/// exactly like turns); host calls are serviced by the `host` closure during the cell.
pub struct DenoRepl {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_cell: u64,
    /// Every accepted cell, in order — the virtual module `deno check` re-checks (§10.5).
    cells: Vec<String>,
    deno: PathBuf,
    artifacts: PathBuf,
}

impl DenoRepl {
    /// Spawn the sidecar with the §10.2 permission set.
    pub fn spawn(deno: &Path, cwd: &Path, artifacts: &Path, max_heap_mb: u64) -> Result<DenoRepl> {
        std::fs::create_dir_all(artifacts.join("store"))?;
        std::fs::create_dir_all(artifacts.join("checks"))?;
        // The bootstrap is embedded; materialize it into the artifacts dir so deno can
        // read it without widening --allow-read.
        let bootstrap = artifacts.join("bootstrap.ts");
        std::fs::write(&bootstrap, BOOTSTRAP_TS)?;

        let mut cmd = tokio::process::Command::new(deno);
        cmd.arg("run")
            .arg(format!(
                "--allow-read={},{}",
                cwd.display(),
                artifacts.display()
            ))
            .arg(format!("--allow-write={}", artifacts.display()))
            .arg(format!("--v8-flags=--max-old-space-size={max_heap_mb}"))
            .arg(&bootstrap)
            .arg(artifacts)
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let mut child = cmd.spawn().context("spawn deno")?;
        let stdin = child.stdin.take().context("deno stdin")?;
        let stdout = BufReader::new(child.stdout.take().context("deno stdout")?);
        Ok(DenoRepl {
            child,
            stdin,
            stdout,
            next_cell: 0,
            cells: Vec::new(),
            deno: deno.to_path_buf(),
            artifacts: artifacts.to_path_buf(),
        })
    }

    /// Type-check accumulated cells + `code` as one virtual module (§10.5): each cell in
    /// its own block inside an async function, under the declaration header — the same
    /// scoping the runtime gives them.
    pub async fn typecheck(&self, code: &str) -> Result<Option<String>> {
        let mut module = String::from(DECL_HEADER);
        module.push_str("async function __cells(): Promise<unknown> {\n");
        for cell in &self.cells {
            module.push_str("  {\n");
            module.push_str(cell);
            module.push_str("\n  }\n");
        }
        module.push_str("  {\n");
        module.push_str(code);
        module.push_str("\n  }\n  return undefined;\n}\nvoid __cells;\n");
        let check_file = self.artifacts.join("checks").join("cells.ts");
        tokio::fs::write(&check_file, module).await?;
        let out = tokio::process::Command::new(&self.deno)
            .args(["check", "--quiet"])
            .arg(&check_file)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .await
            .context("run deno check")?;
        if out.status.success() {
            return Ok(None);
        }
        let mut msg = String::from_utf8_lossy(&out.stderr).to_string();
        if msg.trim().is_empty() {
            msg = String::from_utf8_lossy(&out.stdout).to_string();
        }
        Ok(Some(compact_check_error(&msg)))
    }

    /// Run one cell to completion, answering host calls via `host`. `None` from `host`
    /// = unknown function (reported to the cell as an error, never a hang).
    pub async fn eval_cell<F, Fut>(&mut self, code: &str, mut host: F) -> Result<CellOutcome>
    where
        F: FnMut(HostCall) -> Fut,
        Fut: std::future::Future<Output = Result<Value>>,
    {
        self.next_cell += 1;
        let id = self.next_cell;
        let frame = serde_json::json!({ "type": "cell", "id": id, "code": code });
        self.stdin
            .write_all(format!("{frame}\n").as_bytes())
            .await
            .context("deno died — the REPL heap is gone (store/* survives); retry to respawn")?;
        self.stdin.flush().await?;

        let mut line = String::new();
        loop {
            line.clear();
            let n = self.stdout.read_line(&mut line).await?;
            if n == 0 {
                return Err(anyhow!(
                    "deno exited mid-cell — heap variables are lost (store/* survives). \
                     The next cell respawns a fresh REPL."
                ));
            }
            let Ok(value) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            match value.get("type").and_then(Value::as_str) {
                Some("host_call") => {
                    let call: HostCall = match serde_json::from_value(value.clone()) {
                        Ok(c) => c,
                        Err(_) => continue,
                    };
                    let call_id = call.id;
                    let reply = host(call).await;
                    let (ok, val, err) = match &reply {
                        Ok(v) => (true, Some(v), None),
                        Err(e) => (false, None, Some(format!("{e:#}"))),
                    };
                    let frame = HostResult {
                        r#type: "host_result",
                        id: call_id,
                        ok,
                        value: val,
                        error: err.as_deref(),
                    };
                    self.stdin
                        .write_all(format!("{}\n", serde_json::to_string(&frame)?).as_bytes())
                        .await?;
                    self.stdin.flush().await?;
                }
                Some("cell_result") => {
                    let ok = value.get("ok").and_then(Value::as_bool).unwrap_or(false);
                    if ok {
                        self.cells.push(code.to_string());
                        return Ok(CellOutcome::Ok {
                            repr: value
                                .get("repr")
                                .and_then(Value::as_str)
                                .unwrap_or("undefined")
                                .to_string(),
                        });
                    }
                    return Ok(CellOutcome::RuntimeError {
                        error: value
                            .get("error")
                            .and_then(Value::as_str)
                            .unwrap_or("cell failed")
                            .to_string(),
                    });
                }
                _ => continue,
            }
        }
    }

    /// Kill the sidecar (heap gone; store and cell log survive by design).
    pub async fn shutdown(mut self) {
        let _ = self.child.start_kill();
        let _ = self.child.wait().await;
    }
}

/// First ~6 relevant lines of a deno check error — the model needs the message, not the
/// whole snippet dump.
fn compact_check_error(raw: &str) -> String {
    raw.lines()
        .filter(|l| !l.trim().is_empty())
        .take(6)
        .collect::<Vec<_>>()
        .join("\n")
}

// ── store: durable key→JSON values under the artifacts dir (§10.6) ──────────

pub fn store_put(artifacts: &Path, key: &str, value: &Value) -> Result<()> {
    if !crate::events::is_safe_log_component(key) {
        return Err(anyhow!(
            "store keys must be simple names (letters, digits, ., _, -); got {key:?}"
        ));
    }
    let dir = artifacts.join("store");
    std::fs::create_dir_all(&dir)?;
    let tmp = dir.join(format!("{key}.json.tmp"));
    std::fs::write(&tmp, serde_json::to_vec_pretty(value)?)?;
    std::fs::rename(&tmp, dir.join(format!("{key}.json")))?;
    Ok(())
}

pub fn store_get(artifacts: &Path, key: &str) -> Result<Value> {
    if !crate::events::is_safe_log_component(key) {
        return Err(anyhow!("invalid store key {key:?}"));
    }
    let path = artifacts.join("store").join(format!("{key}.json"));
    let body = std::fs::read_to_string(&path)
        .map_err(|_| anyhow!("no stored value {key:?} (store.list() shows what exists)"))?;
    Ok(serde_json::from_str(&body)?)
}

pub fn store_list(artifacts: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(artifacts.join("store")) else {
        return Vec::new();
    };
    let mut keys: Vec<String> = entries
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            e.path()
                .file_name()?
                .to_str()?
                .strip_suffix(".json")
                .map(str::to_string)
        })
        .collect();
    keys.sort();
    keys
}

/// Answer one host call with the session-scoped implementations. `dispatch` is delegated
/// to `run_dispatch` (the worker wires its TurnEngine in); everything else is local fs.
pub async fn handle_host_call<F, Fut>(
    call: HostCall,
    artifacts: &Path,
    session_answers: impl Fn(usize) -> Vec<(String, String)>,
    run_dispatch: F,
) -> Result<Value>
where
    F: FnOnce(String, Option<String>) -> Fut,
    Fut: std::future::Future<Output = Result<Value>>,
{
    match call.r#fn.as_str() {
        "dispatch" => {
            let task = call
                .args
                .get("task")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("dispatch needs a task string"))?
                .to_string();
            let backend = call
                .args
                .get("backend")
                .and_then(Value::as_str)
                .map(str::to_string);
            run_dispatch(task, backend).await
        }
        "store_put" => {
            let key = call
                .args
                .get("key")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("store.put needs a key"))?;
            let value = call.args.get("value").cloned().unwrap_or(Value::Null);
            store_put(artifacts, key, &value)?;
            Ok(Value::Null)
        }
        "store_get" => {
            let key = call
                .args
                .get("key")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("store.get needs a key"))?;
            store_get(artifacts, key)
        }
        "store_list" => Ok(Value::Array(
            store_list(artifacts)
                .into_iter()
                .map(Value::String)
                .collect(),
        )),
        "session_answers" => {
            let n = call
                .args
                .get("n")
                .and_then(Value::as_u64)
                .unwrap_or(10)
                .min(200) as usize;
            let items = session_answers(n);
            Ok(serde_json::to_value(items)?)
        }
        other => Err(anyhow!(
            "unknown host function {other:?} — the cell API is dispatch/store/session/preview"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn deno() -> Option<PathBuf> {
        which_deno()
    }

    #[test]
    fn store_roundtrip_and_key_safety() {
        let tmp = tempfile::tempdir().unwrap();
        let value = serde_json::json!({ "reviews": ["a", "b"], "n": 2 });
        store_put(tmp.path(), "reviews", &value).unwrap();
        assert_eq!(store_get(tmp.path(), "reviews").unwrap(), value);
        assert_eq!(store_list(tmp.path()), vec!["reviews".to_string()]);
        assert!(store_put(tmp.path(), "../escape", &value).is_err());
        assert!(store_get(tmp.path(), "absent").is_err());
    }

    /// Full sidecar pass against the real deno (skipped when absent — CI without deno
    /// still runs every other test): S persists across cells, reprs truncate with total
    /// size, host dispatch round-trips, runtime errors surface.
    #[tokio::test]
    async fn deno_sidecar_runs_cells_with_persistent_scope_and_host_calls() {
        let Some(deno) = deno() else {
            eprintln!("skipping: deno not on PATH");
            return;
        };
        let tmp = tempfile::tempdir().unwrap();
        let mut repl = DenoRepl::spawn(&deno, tmp.path(), &tmp.path().join("art"), 128).unwrap();

        // Cell 1: persist a big string via S; return a repr that must carry TOTAL size.
        let out = repl
            .eval_cell(
                "S.big = 'x'.repeat(50_000); return S.big;",
                |_call| async move { Ok(Value::Null) },
            )
            .await
            .unwrap();
        match out {
            CellOutcome::Ok { repr } => {
                assert!(
                    repr.contains("50000 chars"),
                    "repr must show total size: {repr}"
                );
                assert!(repr.len() < 400, "repr must truncate: {} bytes", repr.len());
            }
            other => panic!("cell 1 failed: {other:?}"),
        }

        // Cell 2: S persisted; host dispatch answers flow back into the cell. TYPE
        // ANNOTATIONS must survive at runtime (regression: eval rejected TS syntax that
        // deno check had accepted — cells are now imported as modules, not eval'd).
        let out = repl
            .eval_cell(
                "S.r = await dispatch('summarize', {backend: 'codex'}); \
                 const total: number = (S.big as string).length; \
                 return [total, S.r.answer];",
                |call| async move {
                    assert_eq!(call.r#fn, "dispatch");
                    assert_eq!(call.args["task"], "summarize");
                    assert_eq!(call.args["backend"], "codex");
                    Ok(serde_json::json!({
                        "backend": "codex", "status": "ok", "answer": "fine"
                    }))
                },
            )
            .await
            .unwrap();
        match out {
            CellOutcome::Ok { repr } => {
                assert!(repr.contains("50000"), "S must persist: {repr}");
            }
            other => panic!("cell 2 failed: {other:?}"),
        }

        // Cell 3: runtime errors surface as errors, not hangs.
        let out = repl
            .eval_cell(
                "throw new Error('boom');",
                |_c| async move { Ok(Value::Null) },
            )
            .await
            .unwrap();
        assert!(matches!(
            out,
            CellOutcome::RuntimeError { ref error } if error.contains("boom")
        ));

        repl.shutdown().await;
    }

    /// `deno check` refuses a type-broken cell BEFORE execution, with prior cells' scope
    /// in view (§10.5) — and passes a well-typed one.
    #[tokio::test]
    async fn typecheck_gates_cells_before_execution() {
        let Some(deno) = deno() else {
            eprintln!("skipping: deno not on PATH");
            return;
        };
        let tmp = tempfile::tempdir().unwrap();
        let mut repl = DenoRepl::spawn(&deno, tmp.path(), &tmp.path().join("art"), 128).unwrap();

        assert!(
            repl.typecheck("const n: number = 1; return n;")
                .await
                .unwrap()
                .is_none(),
            "well-typed cell must pass"
        );
        let err = repl
            .typecheck("const n: number = 'not a number'; return n;")
            .await
            .unwrap()
            .expect("type error must be caught");
        assert!(err.contains("TS"), "compact error keeps the TS code: {err}");

        // The dispatch signature is part of the checked surface.
        let err = repl
            .typecheck("await dispatch(42);")
            .await
            .unwrap()
            .expect("bad dispatch arg must be caught");
        assert!(err.contains("TS"), "{err}");

        // Accepted cells join the virtual module for later checks.
        let out = repl
            .eval_cell("S.count = 1; return S.count;", |_c| async move {
                Ok(Value::Null)
            })
            .await
            .unwrap();
        assert!(matches!(out, CellOutcome::Ok { .. }));
        assert!(
            repl.typecheck("return S.count;").await.unwrap().is_none(),
            "S is typed for later cells"
        );
        repl.shutdown().await;
    }

    #[tokio::test]
    async fn handle_host_call_covers_the_api_surface() {
        let tmp = tempfile::tempdir().unwrap();
        // store put/get/list
        let put = HostCall {
            id: 1,
            r#fn: "store_put".into(),
            args: serde_json::json!({ "key": "k", "value": {"a": 1} }),
        };
        handle_host_call(
            put,
            tmp.path(),
            |_| Vec::new(),
            |_t, _b| async move { Ok(Value::Null) },
        )
        .await
        .unwrap();
        let get = HostCall {
            id: 2,
            r#fn: "store_get".into(),
            args: serde_json::json!({ "key": "k" }),
        };
        let v = handle_host_call(
            get,
            tmp.path(),
            |_| Vec::new(),
            |_t, _b| async move { Ok(Value::Null) },
        )
        .await
        .unwrap();
        assert_eq!(v, serde_json::json!({"a": 1}));

        // session_answers threads through
        let ans = HostCall {
            id: 3,
            r#fn: "session_answers".into(),
            args: serde_json::json!({ "n": 2 }),
        };
        let v = handle_host_call(
            ans,
            tmp.path(),
            |n| {
                assert_eq!(n, 2);
                vec![("user".into(), "hi".into())]
            },
            |_t, _b| async move { Ok(Value::Null) },
        )
        .await
        .unwrap();
        assert_eq!(v[0][0], "user");

        // dispatch delegates with parsed args
        let d = HostCall {
            id: 4,
            r#fn: "dispatch".into(),
            args: serde_json::json!({ "task": "t", "backend": "codex" }),
        };
        let v = handle_host_call(
            d,
            tmp.path(),
            |_| Vec::new(),
            |task, backend| async move {
                assert_eq!(task, "t");
                assert_eq!(backend.as_deref(), Some("codex"));
                Ok(serde_json::json!({"status": "ok"}))
            },
        )
        .await
        .unwrap();
        assert_eq!(v["status"], "ok");

        // unknown fn = error, not hang
        let u = HostCall {
            id: 5,
            r#fn: "nope".into(),
            args: Value::Null,
        };
        assert!(
            handle_host_call(
                u,
                tmp.path(),
                |_| Vec::new(),
                |_t, _b| async move { Ok(Value::Null) }
            )
            .await
            .is_err()
        );
    }
}
