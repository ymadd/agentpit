//! The daemon's loop verbs (design §7 "デーモン側の verb"): create a loop, find or spawn
//! its runner, list loops, stop a runner. The daemon stays a stateless broker — every
//! answer comes from the loop directories and the runner registry on disk.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use agentpit_events::loops::*;
use agentpit_events::wire::{BlueprintSource, LoopRow, RequestBody, ResponseData};
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::classify::op_error;
use super::paths::{RUNNER_LOG, head_path, loop_socket_path, runners_dir};
use crate::daemon::registry::{process_alive, process_same_incarnation};
use crate::events::{RunKind, RunLink, RunLogger};

/// How long `loop_ensure` waits for a fresh runner to answer.
const RUNNER_START_TIMEOUT: Duration = Duration::from_secs(10);
/// Environment a runner must not inherit: it links its runs explicitly (design §10).
const SCRUBBED_ENV: &[&str] = &[
    "AGENTPIT_PARENT_RUN_ID",
    "AGENTPIT_WORKFLOW_DEPTH",
    crate::ask::ENV_ASK_ALLOWED,
];

/// A live runner, as the daemon registered it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunnerRecord {
    pub loop_id: String,
    pub pid: u32,
    pub start_id: String,
    pub socket: String,
}

impl RunnerRecord {
    pub fn alive(&self) -> bool {
        process_alive(self.pid, &self.start_id)
    }
}

fn record_path(loop_id: &str) -> PathBuf {
    runners_dir().join(format!("{loop_id}.json"))
}

pub fn load_runner(loop_id: &str) -> Option<RunnerRecord> {
    serde_json::from_str(&std::fs::read_to_string(record_path(loop_id)).ok()?).ok()
}

pub(crate) fn save_runner(record: &RunnerRecord) -> std::io::Result<()> {
    std::fs::create_dir_all(runners_dir())?;
    let path = record_path(&record.loop_id);
    let tmp = path.with_extension("json.tmp");
    std::fs::write(
        &tmp,
        serde_json::to_vec(record).map_err(std::io::Error::other)?,
    )?;
    std::fs::rename(tmp, path)
}

fn remove_runner(loop_id: &str) {
    let _ = std::fs::remove_file(record_path(loop_id));
}

/// Remove the registry record only if it still describes `me` (a newer runner may have
/// replaced it).
pub(crate) fn remove_runner_if(me: &RunnerRecord) {
    if load_runner(&me.loop_id).is_some_and(|r| r.pid == me.pid && r.start_id == me.start_id) {
        remove_runner(&me.loop_id);
    }
}

/// One `loop_ensure` at a time per loop, so two clients never spawn two runners (the
/// loser would exit on the lease, but only after racing for the socket path).
static ENSURE_LOCKS: LazyLock<Mutex<BTreeMap<String, Arc<tokio::sync::Mutex<()>>>>> =
    LazyLock::new(|| Mutex::new(BTreeMap::new()));

fn ensure_lock(loop_id: &str) -> Arc<tokio::sync::Mutex<()>> {
    let mut map = ENSURE_LOCKS.lock().unwrap_or_else(|p| p.into_inner());
    Arc::clone(map.entry(loop_id.to_string()).or_default())
}

// ---------------------------------------------------------------------------------------
// loop_start.

pub struct StartRequest {
    pub op_id: String,
    pub blueprint: BlueprintSource,
    pub inputs: BTreeMap<String, String>,
    pub cwd: String,
    pub title: Option<String>,
    pub start: bool,
    pub origin: Option<Origin>,
}

/// A located, parsed blueprint document.
struct Located {
    doc: serde_json::Value,
    scope: BlueprintScope,
    path: Option<String>,
}

fn read_doc(path: &Path) -> Result<serde_json::Value, OpError> {
    use std::io::Read;
    let unreadable = |e: std::io::Error| {
        op_error(
            ErrorCode::NotFound,
            format!("cannot read {}: {e}; check the path", path.display()),
        )
    };
    let meta = std::fs::metadata(path).map_err(unreadable)?;
    // Only regular files: a FIFO would block, a device could stream forever.
    if !meta.is_file() {
        return Err(op_error(
            ErrorCode::Validation,
            format!(
                "{} is not a regular file; pass a blueprint .json file",
                path.display()
            ),
        ));
    }
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .and_then(|f| f.take(MAX_DOC_BYTES as u64 + 1).read_to_end(&mut bytes))
        .map_err(unreadable)?;
    if bytes.len() > MAX_DOC_BYTES {
        return Err(op_error(
            ErrorCode::Validation,
            format!(
                "{} is larger than {MAX_DOC_BYTES} bytes; split the blueprint",
                path.display()
            ),
        ));
    }
    let text = String::from_utf8(bytes).map_err(|_| {
        op_error(
            ErrorCode::Validation,
            format!("{} is not UTF-8 text; save it as UTF-8", path.display()),
        )
    })?;
    serde_json::from_str(&text).map_err(|e| {
        op_error(
            ErrorCode::Validation,
            format!("{} is not JSON ({e}); fix the syntax", path.display()),
        )
    })
}

/// Where a named blueprint lives: the project's `.agentpit/blueprints`, then the user's.
pub fn named_blueprint_paths(cwd: &Path, name: &str) -> Vec<(BlueprintScope, PathBuf)> {
    vec![
        (
            BlueprintScope::Project,
            cwd.join(".agentpit")
                .join("blueprints")
                .join(format!("{name}.json")),
        ),
        (
            BlueprintScope::User,
            crate::config::xdg_config_home()
                .join("agentpit")
                .join("blueprints")
                .join(format!("{name}.json")),
        ),
    ]
}

fn locate(source: &BlueprintSource, cwd: &Path) -> Result<Located, OpError> {
    match source {
        BlueprintSource::Inline { doc } => Ok(Located {
            doc: doc.clone(),
            scope: BlueprintScope::Inline,
            path: None,
        }),
        BlueprintSource::Path { path } => {
            let p = Path::new(path);
            let full = if p.is_absolute() {
                p.to_path_buf()
            } else {
                cwd.join(p)
            };
            Ok(Located {
                doc: read_doc(&full)?,
                scope: BlueprintScope::Project,
                path: Some(full.display().to_string()),
            })
        }
        BlueprintSource::Named { name } => {
            if !is_valid_blueprint_name(name) {
                return Err(op_error(
                    ErrorCode::Validation,
                    format!("{name:?} is not a blueprint name; use [a-z0-9_-]"),
                ));
            }
            for (scope, path) in named_blueprint_paths(cwd, name) {
                if path.is_file() {
                    return Ok(Located {
                        doc: read_doc(&path)?,
                        scope,
                        path: Some(path.display().to_string()),
                    });
                }
            }
            Err(op_error(
                ErrorCode::NotFound,
                format!(
                    "no blueprint named {name} in .agentpit/blueprints or ~/.config/agentpit/blueprints; \
                     save it there or pass a path"
                ),
            ))
        }
        BlueprintSource::Unknown => Err(op_error(
            ErrorCode::Unsupported,
            "this daemon does not know that blueprint source; pass a path, a name or the document",
        )),
    }
}

/// Validation against the local config (role and backend names, for warnings only).
pub fn validate_env() -> ValidateEnv {
    let Ok(loaded) = crate::config::load_config(None) else {
        return ValidateEnv::default();
    };
    ValidateEnv {
        roles: Some(
            crate::workflow::roles::worker_roles(&loaded.config.workflow.roles)
                .map(|(n, _)| n.clone())
                .collect(),
        ),
        backends: Some(
            crate::types::BackendId::ALL
                .iter()
                .map(|b| b.as_str().to_string())
                .collect(),
        ),
    }
}

/// Check and complete the inputs: every required one present, none undeclared, defaults
/// filled in.
fn resolve_inputs(
    bp: &Blueprint,
    given: &BTreeMap<String, String>,
) -> Result<BTreeMap<String, String>, OpError> {
    if let Some(unknown) = given.keys().find(|k| !bp.inputs.contains_key(*k)) {
        let declared: Vec<&str> = bp.inputs.keys().map(String::as_str).collect();
        return Err(op_error(
            ErrorCode::Validation,
            format!(
                "{} declares no input {unknown:?}; pass one of: {}",
                bp.name,
                if declared.is_empty() {
                    "(none)".to_string()
                } else {
                    declared.join(", ")
                }
            ),
        ));
    }
    let mut out = BTreeMap::new();
    for (name, spec) in &bp.inputs {
        let value = given
            .get(name)
            .filter(|v| !v.trim().is_empty())
            .cloned()
            .or_else(|| spec.default.clone());
        match value {
            Some(v) => {
                if v.len() > MAX_TEXT_BYTES {
                    return Err(op_error(
                        ErrorCode::Validation,
                        format!("input {name} is longer than {MAX_TEXT_BYTES} bytes; shorten it"),
                    ));
                }
                out.insert(name.clone(), v);
            }
            None if spec.required => {
                return Err(op_error(
                    ErrorCode::Validation,
                    format!(
                        "{} needs the input {name}; pass it (e.g. --input {name}=…)",
                        bp.name
                    ),
                ));
            }
            None => {}
        }
    }
    Ok(out)
}

fn repo_root(cwd: &Path) -> Option<String> {
    cwd.ancestors()
        .find(|d| d.join(".git").exists())
        .map(|d| d.display().to_string())
}

/// Create the loop (or find the one this op id already created) and ensure its runner.
/// Returns `(loop_id, socket, duplicate)`; `socket` is `None` for a finished loop.
pub async fn start_loop(req: StartRequest) -> Result<(String, Option<PathBuf>, bool), OpError> {
    let Some(loop_id) = loop_id_from_op(&req.op_id) else {
        return Err(op_error(
            ErrorCode::BadRequest,
            "loop_start needs op_id = a lowercase hyphenated UUID (v7 recommended); generate one",
        ));
    };
    let dir = loop_dir(&loop_id).expect("derived ids are valid");
    // Creation is serialized per loop id, so a concurrent retry of the same op waits and
    // then finds the loop (`duplicate`) instead of racing the creation.
    let lock = ensure_lock(&loop_id);
    let guard = lock.lock().await;
    if journal_path(&dir).exists() {
        drop(guard);
        return existing(&loop_id).await.map(|s| (loop_id, s, true));
    }

    let cwd = PathBuf::from(&req.cwd);
    if !cwd.is_absolute() || !cwd.is_dir() {
        return Err(op_error(
            ErrorCode::Validation,
            format!("cwd {} is not an absolute directory; pass one", req.cwd),
        ));
    }
    let cwd = cwd.canonicalize().unwrap_or(cwd);
    let located = {
        let (source, cwd) = (req.blueprint.clone(), cwd.clone());
        tokio::task::spawn_blocking(move || locate(&source, &cwd))
            .await
            .map_err(|e| op_error(ErrorCode::Internal, format!("read the blueprint: {e}")))??
    };
    let v = validate(&located.doc, &validate_env());
    let Some(bp) = v.blueprint.clone().filter(|_| v.is_runnable()) else {
        let first = v
            .diagnostics
            .iter()
            .find(|d| d.severity == Severity::Error)
            .map(|d| d.message.clone())
            .unwrap_or_else(|| "the blueprint does not parse".into());
        return Err(OpError {
            code: ErrorCode::Validation,
            message: format!("the blueprint cannot run: {first}"),
            details: Some(json!({"rev": v.rev, "diagnostics": v.diagnostics})),
        });
    };
    let inputs = resolve_inputs(&bp, &req.inputs)?;
    let title = req
        .title
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(|t| clamp_text(t, MAX_SHORT_BYTES))
        .or_else(|| bp.title.clone())
        .unwrap_or_else(|| bp.name.clone());

    // The loop's root run: the existing dashboard shows every step as its child. Named
    // now (it goes into loop_created), announced only once the loop exists.
    let root_run_id = agentpit_events::next_run_id();
    let bp_name = bp.name.clone();
    let created = LoopCreated {
        loop_id: loop_id.clone(),
        uid: new_uid(),
        title,
        blueprint: FrozenBlueprint {
            name: bp.name.clone(),
            scope: located.scope,
            path: located.path,
            rev: v.rev.clone(),
            doc: located.doc,
        },
        inputs,
        cwd: cwd.display().to_string(),
        repo_root: repo_root(&cwd),
        workspace: bp.workspace.mode,
        budget: bp.budget,
        origin: req.origin.unwrap_or(Origin {
            surface: Surface::Cli,
            client: None,
            session_id: None,
        }),
        root_run_id: Some(root_run_id.clone()),
    };
    let writer = WriterInfo::current(&format!("agentpit/{}", env!("CARGO_PKG_VERSION")));
    std::fs::create_dir_all(&dir).map_err(|e| {
        op_error(
            ErrorCode::Internal,
            format!("could not create {}: {e}", dir.display()),
        )
    })?;
    match LoopJournal::create(
        &dir,
        &loop_leases_dir(),
        agentpit_events::now_ms(),
        created,
        Some(&req.op_id),
        &writer,
        req.start,
    ) {
        // Dropping the journal releases the lease for the runner.
        Ok(journal) => drop(journal),
        Err(CommitError::Journal(JournalError::AlreadyExists | JournalError::Busy { .. })) => {
            drop(guard);
            return existing(&loop_id).await.map(|s| (loop_id, s, true));
        }
        Err(CommitError::Journal(e @ JournalError::LineTooLarge { .. })) => {
            return Err(op_error(
                ErrorCode::Validation,
                format!(
                    "the blueprint and inputs are too large for one record ({e}); shorten them"
                ),
            ));
        }
        Err(e) => {
            return Err(op_error(
                ErrorCode::Internal,
                format!("could not create the loop: {e}"),
            ));
        }
    }
    RunLogger::start_linked(
        RunKind::Workflow,
        &[],
        &cwd,
        RunLink {
            role: Some(&format!("loop:{bp_name}")),
            parent_run_id: None,
            depth: 0,
            loop_ref: Some(&loop_id),
            run_id: Some(&root_run_id),
        },
    );
    drop(guard);
    let socket = ensure_runner(&loop_id).await?;
    Ok((loop_id, Some(socket), false))
}

/// A loop this op id already created: its runner, unless it has finished.
async fn existing(loop_id: &str) -> Result<Option<PathBuf>, OpError> {
    match ensure_runner(loop_id).await {
        Ok(socket) => Ok(Some(socket)),
        Err(e) if e.code == ErrorCode::Gone => Ok(None),
        Err(e) => Err(e),
    }
}

// ---------------------------------------------------------------------------------------
// loop_ensure.

/// A live runner's socket for an unfinished loop, spawning the runner when needed. A
/// finished loop is `gone` (read it from disk instead of reopening it as a writer).
pub async fn ensure_runner(loop_id: &str) -> Result<PathBuf, OpError> {
    let exe = std::env::current_exe().map_err(|e| {
        op_error(
            ErrorCode::Internal,
            format!("resolve the agentpit binary: {e}"),
        )
    })?;
    ensure_runner_with(loop_id, &exe).await
}

pub async fn ensure_runner_with(loop_id: &str, exe: &Path) -> Result<PathBuf, OpError> {
    let (Some(dir), Some(socket)) = (loop_dir(loop_id), loop_socket_path(loop_id)) else {
        return Err(op_error(
            ErrorCode::BadRequest,
            format!("{loop_id:?} is not a loop id (lp-<32 hex>)"),
        ));
    };
    if !journal_path(&dir).exists() {
        return Err(op_error(
            ErrorCode::NotFound,
            format!("no loop {loop_id}; list loops with `agentpit loop ls`"),
        ));
    }
    let lock = ensure_lock(loop_id);
    let _guard = lock.lock().await;
    if probe(&socket).await {
        return Ok(socket);
    }
    let (state, scan) = read_loop(&dir)
        .map_err(|e| op_error(ErrorCode::Internal, format!("read {loop_id}: {e}")))?;
    if state.status.is_terminal() {
        return Err(OpError {
            code: ErrorCode::Gone,
            message: format!(
                "{loop_id} has finished ({}); read it from disk",
                state.status
            ),
            details: state.summary().map(|s| json!({"summary": s})),
        });
    }
    if let Err(reason) = state.writable() {
        return Err(op_error(ErrorCode::ReadOnly, reason.to_string()));
    }
    // A damaged journal opens read-only for any writer: a runner would only exit again.
    if let Some(issue) = scan.issues.first() {
        return Err(op_error(
            ErrorCode::ReadOnly,
            format!("the journal of {loop_id} is damaged ({issue}); it can only be read"),
        ));
    }
    // A registered runner that is alive but not answering is wedged: it holds the lease,
    // so a new one could not start anyway.
    if let Some(record) = load_runner(loop_id)
        && record.alive()
    {
        return Err(op_error(
            ErrorCode::Unavailable,
            format!(
                "the runner of {loop_id} (pid {}) is alive but not answering; stop it with force",
                record.pid
            ),
        ));
    }
    remove_runner(loop_id);

    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join(RUNNER_LOG))
        .map(Stdio::from)
        .unwrap_or_else(|_| Stdio::null());
    let mut cmd = tokio::process::Command::new(exe);
    cmd.args([
        "daemon",
        "loop",
        "--loop",
        loop_id,
        "--socket",
        &socket.display().to_string(),
    ])
    .stdin(Stdio::null())
    .stdout(Stdio::null())
    .stderr(log)
    // The runner outlives the daemon, like a session worker.
    .kill_on_drop(false);
    for var in SCRUBBED_ENV {
        cmd.env_remove(var);
    }
    #[cfg(unix)]
    cmd.process_group(0);
    let mut child = cmd
        .spawn()
        .map_err(|e| op_error(ErrorCode::Internal, format!("spawn the runner: {e}")))?;
    let pid = child.id().unwrap_or(0);

    let deadline = tokio::time::Instant::now() + RUNNER_START_TIMEOUT;
    while !probe(&socket).await {
        // A runner that exits before answering will never answer: say why at once.
        if let Ok(Some(status)) = child.try_wait() {
            let why = super::prompt::file_tail(&dir.join(RUNNER_LOG), 600)
                .and_then(|t| {
                    t.lines()
                        .rev()
                        .find(|l| l.contains("Error"))
                        .map(str::to_string)
                })
                .unwrap_or_else(|| format!("it exited with {status}"));
            return Err(op_error(
                ErrorCode::Unavailable,
                format!("the runner of {loop_id} could not start: {}", why.trim()),
            ));
        }
        if tokio::time::Instant::now() > deadline {
            return Err(op_error(
                ErrorCode::Unavailable,
                format!(
                    "the runner of {loop_id} did not come up within {}s; see {}",
                    RUNNER_START_TIMEOUT.as_secs(),
                    dir.join(RUNNER_LOG).display()
                ),
            ));
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // Reap it when it ends: an unreaped runner lingers as a zombie that still looks alive.
    tokio::spawn(async move {
        let _ = child.wait().await;
    });
    let _ = save_runner(&RunnerRecord {
        loop_id: loop_id.to_string(),
        pid,
        start_id: agentpit_events::session_lease::process_start_id(pid),
        socket: socket.display().to_string(),
    });
    Ok(socket)
}

/// Whether a runner answers `loop_status` on `socket`.
async fn probe(socket: &Path) -> bool {
    let fut = crate::daemon::server::request_worker(socket, RequestBody::LoopStatus);
    matches!(
        tokio::time::timeout(Duration::from_secs(2), fut).await,
        Ok(Ok(resp)) if resp.ok && matches!(resp.data, Some(ResponseData::LoopSummary { .. }))
    )
}

// ---------------------------------------------------------------------------------------
// loop_list.

/// A loop's board row from `head.json`, or from a fold of its journal when the runner
/// never wrote one (or it is unreadable).
pub fn loop_summary(dir: &Path) -> Option<LoopSummary> {
    if let Ok(text) = std::fs::read_to_string(head_path(dir))
        && let Ok(summary) = serde_json::from_str::<LoopSummary>(&text)
    {
        return Some(summary);
    }
    read_loop(dir).ok().and_then(|(state, _)| state.summary())
}

pub fn list_loops(include_terminal: bool, limit: Option<usize>) -> Vec<LoopRow> {
    let Ok(entries) = std::fs::read_dir(loops_dir()) else {
        return vec![];
    };
    let mut rows: Vec<LoopRow> = entries
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            if !is_valid_loop_id(&name) {
                return None;
            }
            let summary = loop_summary(&e.path())?;
            if !include_terminal && summary.status.is_terminal() {
                return None;
            }
            let runner = match load_runner(&name) {
                Some(r) if r.alive() => "live",
                _ => "absent",
            };
            Some(LoopRow {
                summary,
                runner: runner.into(),
            })
        })
        .collect();
    rows.sort_by(|a, b| b.summary.updated_ts.cmp(&a.summary.updated_ts));
    if let Some(limit) = limit {
        rows.truncate(limit);
    }
    rows
}

// ---------------------------------------------------------------------------------------
// loop_stop_runner.

/// Stop a loop's runner. Graceful (`writer_closed{shutdown}`) unless steps are running;
/// then only with `force`, which kills the runner — its steps are recovered by the next
/// runner. Returns a note on what happened.
pub async fn stop_runner(loop_id: &str, force: bool) -> Result<String, OpError> {
    let Some(socket) = loop_socket_path(loop_id) else {
        return Err(op_error(
            ErrorCode::BadRequest,
            format!("{loop_id:?} is not a loop id"),
        ));
    };
    let record = load_runner(loop_id);
    let answer =
        crate::daemon::server::request_worker(&socket, RequestBody::Shutdown { all: false }).await;
    match answer {
        Ok(resp) if resp.ok => {
            remove_runner(loop_id);
            Ok("runner stopped".into())
        }
        Ok(resp) if !force => Err(op_error(
            resp.code.as_deref().map_or(ErrorCode::Internal, |c| {
                serde_json::from_value(json!(c)).unwrap_or(ErrorCode::Internal)
            }),
            resp.error
                .unwrap_or_else(|| "the runner refused to stop".into()),
        )),
        Err(_) if !force && record.as_ref().is_some_and(RunnerRecord::alive) => Err(op_error(
            ErrorCode::Unavailable,
            "the runner is alive but not answering; stop it with force",
        )),
        Err(_) if !force => {
            remove_runner(loop_id);
            Ok("no runner was running".into())
        }
        _ => {
            let note = match &record {
                Some(r) if process_same_incarnation(r.pid, &r.start_id) => {
                    // The runner leads a process group holding its agents; each check has
                    // a group of its own. Leaving them running would leave work going on
                    // with no deadline and nobody watching.
                    if !super::proc::signal_group(r.pid, 9) {
                        let _ = crate::daemon::server::kill_pid(r.pid);
                    }
                    let mut reaped = 0;
                    if let Some(dir) = loop_dir(loop_id)
                        && let Ok((state, _)) = read_loop(&dir)
                    {
                        for step in state.running_steps() {
                            if let (Some(pid), Some(start_id)) =
                                (step.pid, step.pid_start_id.as_deref())
                                && super::proc::reap_orphan_group(pid, start_id).await
                            {
                                reaped += 1;
                            }
                        }
                    }
                    format!(
                        "killed runner pid {} with its agents{}",
                        r.pid,
                        if reaped > 0 {
                            format!(" and {reaped} check(s)")
                        } else {
                            String::new()
                        }
                    )
                }
                Some(r) => format!(
                    "pid {} is no longer the recorded runner; removed its record without killing",
                    r.pid
                ),
                None => "no runner record; nothing to kill".into(),
            };
            remove_runner(loop_id);
            Ok(note)
        }
    }
}
