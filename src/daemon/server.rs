//! The daemon process (design §5.2, §5.4-§5.5): a control-plane broker.
//!
//! Clients ask the daemon to create/ensure sessions; the daemon spawns detached worker
//! processes (which own their sessions) and hands back the worker's socket path. Workers
//! outlive the daemon (detached + own sockets + durable registry records), so a daemon
//! restart reconnects instead of restarting the world. Crash recovery is lazy: a dead
//! worker is discovered on the next `ensure`, its record cleaned, and a fresh worker
//! spawned — which marks interrupted exchanges itself on startup (worker::run_worker).

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};

use crate::daemon::paths::{
    daemon_owner_path, daemon_socket_path, ensure_runtime_dir, worker_socket_path, workers_dir,
};
use crate::daemon::protocol::{
    PROTO_VERSION, Request, RequestBody, Response, ResponseData, SessionRow,
};
use crate::daemon::registry::{self, OwnerRecord, WorkerRecord};
use crate::session::list_all;

/// Foreground daemon main (the hidden `daemon run`). `daemon start` spawns this detached.
pub async fn run_daemon() -> Result<()> {
    let owner_path = daemon_owner_path();
    if let Some(existing) = registry::load_owner(&owner_path)
        && existing.alive()
    {
        // Live-probe the RECORDED socket, never trust the pid alone: a daemon whose
        // runtime dir was wiped or whose $XDG_RUNTIME_DIR changed is alive but
        // unreachable, and refusing to start here would leave the user daemon-less
        // forever (found live 2026-08-08). An unreachable owner is killed and replaced.
        if crate::daemon::client::Conn::connect(Path::new(&existing.socket))
            .await
            .is_ok()
        {
            return Err(anyhow!(
                "a daemon is already running (pid {}). Use `agentpit daemon status` to inspect it.",
                existing.pid
            ));
        }
        let _ = kill_pid(existing.pid);
    }
    ensure_runtime_dir()?;
    let socket = daemon_socket_path();
    let _ = std::fs::remove_file(&socket);
    let listener =
        UnixListener::bind(&socket).with_context(|| format!("bind {}", socket.display()))?;
    registry::save_owner(&owner_path, &OwnerRecord::current(&socket))?;

    let shutdown = tokio_util::sync::CancellationToken::new();
    // Idle-eviction sweeper (§7.1): every 5 minutes, unload workers that have been idle
    // past [session] idle_evict_minutes with no attached clients. Config is re-read per
    // sweep so edits apply without a daemon restart. Files always survive; the next
    // address rehydrates lazily via ensure_worker.
    let sweeper = {
        let stop = shutdown.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(300));
            interval.tick().await; // skip the immediate tick
            loop {
                tokio::select! {
                    _ = interval.tick() => {
                        let minutes = crate::config::load_config(None)
                            .map(|l| l.config.session.idle_evict_minutes)
                            .unwrap_or(30);
                        if minutes > 0 {
                            sweep_idle_workers(&workers_dir(), minutes * 60_000).await;
                        }
                    }
                    _ = stop.cancelled() => break,
                }
            }
        })
    };
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let Ok((stream, _)) = accepted else { break };
                let shutdown = shutdown.clone();
                tokio::spawn(async move { handle_connection(stream, shutdown).await });
            }
            _ = shutdown.cancelled() => break,
            _ = tokio::signal::ctrl_c() => break,
        }
    }
    sweeper.abort();
    registry::remove_owner(&owner_path);
    let _ = std::fs::remove_file(&socket);
    Ok(())
}

/// One eviction pass: gracefully shut down every worker that is not busy, has no attached
/// clients, and has idled at least `min_idle_ms`. A worker's own `shutdown` handler
/// re-checks busyness, so a turn that starts mid-probe still refuses eviction — Running is
/// never unloaded (§7.1).
pub async fn sweep_idle_workers(workers: &Path, min_idle_ms: u64) {
    for record in registry::load_all(workers) {
        if !record.alive() {
            registry::remove(workers, &record.session_id);
            continue;
        }
        let status = request_worker(Path::new(&record.socket), RequestBody::Status).await;
        let Ok(resp) = status else { continue };
        let Some(ResponseData::WorkerStatus {
            busy,
            attached_clients,
            idle_ms,
            ..
        }) = resp.data
        else {
            continue;
        };
        if busy || attached_clients > 0 || idle_ms < min_idle_ms {
            continue;
        }
        if let Ok(resp) = request_worker(
            Path::new(&record.socket),
            RequestBody::Shutdown { all: false },
        )
        .await
            && resp.ok
        {
            registry::remove(workers, &record.session_id);
        }
    }
}

async fn handle_connection(stream: UnixStream, shutdown: tokio_util::sync::CancellationToken) {
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line).await {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        if line.trim().is_empty() {
            continue;
        }
        let response = match serde_json::from_str::<Request>(&line) {
            Err(e) => Response::err(0, format!("bad request: {e}")),
            Ok(req) => handle_request(req, &shutdown).await,
        };
        let Ok(resp_line) = serde_json::to_string(&response) else {
            break;
        };
        if write_half.write_all(resp_line.as_bytes()).await.is_err()
            || write_half.write_all(b"\n").await.is_err()
        {
            break;
        }
    }
}

async fn handle_request(req: Request, shutdown: &tokio_util::sync::CancellationToken) -> Response {
    let id = req.id;
    match req.body {
        RequestBody::Hello { proto } => {
            if proto != PROTO_VERSION {
                return Response::err(
                    id,
                    format!(
                        "protocol mismatch: daemon speaks v{PROTO_VERSION}, client sent v{proto}. \
                         Run `agentpit daemon stop` then retry so both sides restart on one version."
                    ),
                );
            }
            Response::ok(
                id,
                ResponseData::Hello {
                    proto: PROTO_VERSION,
                    role: "daemon".into(),
                    pid: std::process::id(),
                },
            )
        }

        RequestBody::Create { cwd } => {
            let dir = agentpit_events::session::sessions_dir();
            let created = match agentpit_events::session::SessionLog::create(&dir, &cwd, None, None)
            {
                Ok(log) => log.session_id().to_string(),
                Err(e) => return Response::err(id, format!("create session: {e}")),
            };
            // The file exists with no lease holder; the worker takes the lease on spawn.
            match ensure_worker(&created).await {
                Ok(socket) => Response::ok(
                    id,
                    ResponseData::Session {
                        session_id: created,
                        socket: socket.display().to_string(),
                    },
                ),
                Err(e) => Response::err(id, format!("{e:#}")),
            }
        }

        RequestBody::Ensure { session } => {
            let resolved = match crate::session::resolve(&session) {
                Ok(meta) => meta.session_id,
                Err(e) => return Response::err(id, format!("{e:#}")),
            };
            match ensure_worker(&resolved).await {
                Ok(socket) => Response::ok(
                    id,
                    ResponseData::Session {
                        session_id: resolved,
                        socket: socket.display().to_string(),
                    },
                ),
                Err(e) => Response::err(id, format!("{e:#}")),
            }
        }

        RequestBody::List => {
            let mut rows = Vec::new();
            let workers = workers_dir();
            for meta in list_all() {
                let state = match registry::load(&workers, &meta.session_id) {
                    Some(record) if record.alive() => probe_worker_state(&record)
                        .await
                        .unwrap_or("idle")
                        .to_string(),
                    _ => "inactive".to_string(),
                };
                rows.push(SessionRow {
                    session_id: meta.session_id,
                    state,
                    title: meta.title,
                    cwd: meta.cwd,
                    updated_at_ms: meta
                        .updated_at
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_millis() as u64)
                        .unwrap_or(0),
                });
            }
            Response::ok(id, ResponseData::Sessions { sessions: rows })
        }

        RequestBody::StopWorker { session, force } => {
            let workers = workers_dir();
            let resolved = match crate::session::resolve(&session) {
                Ok(meta) => meta.session_id,
                Err(e) => return Response::err(id, format!("{e:#}")),
            };
            let Some(record) = registry::load(&workers, &resolved) else {
                return Response::err(id, "no worker is running for that session");
            };
            if !record.alive() {
                registry::remove(&workers, &resolved);
                return Response::ok(id, ResponseData::Unit);
            }
            match request_worker(
                Path::new(&record.socket),
                RequestBody::Shutdown { all: false },
            )
            .await
            {
                Ok(resp) if resp.ok => {
                    registry::remove(&workers, &resolved);
                    Response::ok(id, ResponseData::Unit)
                }
                Ok(resp) if force => {
                    let _ = kill_pid(record.pid);
                    registry::remove(&workers, &resolved);
                    Response::ok(
                        id,
                        ResponseData::Lines {
                            lines: vec![format!(
                                "worker refused graceful stop ({}); killed pid {}",
                                resp.error.unwrap_or_default(),
                                record.pid
                            )],
                        },
                    )
                }
                Ok(resp) => Response::err(
                    id,
                    resp.error
                        .unwrap_or_else(|| "worker refused to stop".into()),
                ),
                Err(_) if force => {
                    let _ = kill_pid(record.pid);
                    registry::remove(&workers, &resolved);
                    Response::ok(id, ResponseData::Unit)
                }
                Err(e) => {
                    Response::err(id, format!("worker unreachable: {e:#}. Retry with force."))
                }
            }
        }

        RequestBody::Shutdown { all } => {
            if all {
                let workers = workers_dir();
                for record in registry::load_all(&workers) {
                    if record.alive() {
                        let _ = request_worker(
                            Path::new(&record.socket),
                            RequestBody::Shutdown { all: false },
                        )
                        .await;
                    }
                    registry::remove(&workers, &record.session_id);
                }
            }
            shutdown.cancel();
            Response::ok(id, ResponseData::Unit)
        }

        // Worker-only verbs on the daemon socket = a mixup; answer clearly (mirror image
        // of the worker's guard).
        _ => Response::err(
            id,
            "this is the DAEMON socket; session verbs go to the worker socket returned by `ensure`",
        ),
    }
}

/// Return a live worker socket for `session_id`, spawning a fresh worker when the recorded
/// one is dead or absent (lazy crash recovery, §5.4).
pub async fn ensure_worker(session_id: &str) -> Result<PathBuf> {
    let workers = workers_dir();
    if let Some(record) = registry::load(&workers, session_id) {
        if record.alive() {
            // Verify the socket actually answers — a live pid with a dead socket is a
            // wedged worker; treat as dead.
            if request_worker(Path::new(&record.socket), RequestBody::Status)
                .await
                .is_ok()
            {
                return Ok(PathBuf::from(record.socket));
            }
        }
        registry::remove(&workers, session_id);
    }

    ensure_runtime_dir()?;
    let socket = worker_socket_path(session_id);
    let _ = std::fs::remove_file(&socket);

    let exe = std::env::current_exe().context("resolve agentpit binary path")?;
    let mut cmd = tokio::process::Command::new(exe);
    cmd.args([
        "daemon",
        "worker",
        "--session",
        session_id,
        "--socket",
        &socket.display().to_string(),
    ])
    .stdin(Stdio::null())
    .stdout(Stdio::null())
    .stderr(Stdio::null())
    // The worker must OUTLIVE the daemon (§5.5) — never kill on handle drop.
    .kill_on_drop(false);
    // Detach into its own process group so terminal signals and the daemon's own death
    // never propagate to workers.
    #[cfg(unix)]
    cmd.process_group(0);
    let mut child = cmd.spawn().context("spawn worker")?;
    let pid = child.id().unwrap_or(0);
    // Reap on exit: an unreaped SIGKILLed worker lingers as a zombie that still answers
    // `kill -0`, which blocked lease reclaim and crash recovery (found 2026-08-08).
    tokio::spawn(async move {
        let _ = child.wait().await;
    });

    // Wait for the worker to answer hello (it must first open the session + take the
    // lease). Slow-but-bounded: session replay is a file scan.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(resp) = request_worker(&socket, RequestBody::Status).await
            && resp.ok
        {
            break;
        }
        if std::time::Instant::now() > deadline {
            return Err(anyhow!(
                "worker for {session_id} did not come up within 10s. \
                 Check `agentpit sessions` for a lease conflict, or retry."
            ));
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    let record = WorkerRecord {
        session_id: session_id.to_string(),
        pid,
        start_id: agentpit_events::session_lease::process_start_id(pid),
        socket: socket.display().to_string(),
    };
    registry::save(&workers, &record)?;
    Ok(socket)
}

/// One request/response against a worker socket (fresh connection, hello included).
pub async fn request_worker(socket: &Path, body: RequestBody) -> Result<Response> {
    let stream = UnixStream::connect(socket).await?;
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);

    let hello = serde_json::to_string(&Request {
        id: 0,
        body: RequestBody::Hello {
            proto: PROTO_VERSION,
        },
    })?;
    write_half.write_all(hello.as_bytes()).await?;
    write_half.write_all(b"\n").await?;
    let mut line = String::new();
    reader.read_line(&mut line).await?;
    let hello_resp: Response = serde_json::from_str(&line)?;
    if !hello_resp.ok {
        return Err(anyhow!(
            hello_resp.error.unwrap_or_else(|| "hello refused".into())
        ));
    }

    let req = serde_json::to_string(&Request { id: 1, body })?;
    write_half.write_all(req.as_bytes()).await?;
    write_half.write_all(b"\n").await?;
    // Skip event frames (this helper subscribes to nothing, but a broadcast can race in).
    loop {
        line.clear();
        let n = reader.read_line(&mut line).await?;
        if n == 0 {
            return Err(anyhow!("worker closed the connection mid-request"));
        }
        if let Ok(resp) = serde_json::from_str::<Response>(&line)
            && resp.id == 1
        {
            return Ok(resp);
        }
    }
}

/// "running" | "idle" from a worker's status probe (short timeout — used by `list`).
async fn probe_worker_state(record: &WorkerRecord) -> Option<&'static str> {
    let fut = request_worker(Path::new(&record.socket), RequestBody::Status);
    match tokio::time::timeout(Duration::from_millis(300), fut).await {
        Ok(Ok(resp)) => match resp.data {
            Some(ResponseData::WorkerStatus { busy: true, .. }) => Some("running"),
            Some(ResponseData::WorkerStatus { busy: false, .. }) => Some("idle"),
            _ => None,
        },
        _ => None,
    }
}

fn kill_pid(pid: u32) -> Result<()> {
    #[cfg(unix)]
    {
        let status = std::process::Command::new("kill")
            .arg(pid.to_string())
            .status()?;
        if !status.success() {
            return Err(anyhow!("kill {pid} failed"));
        }
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
    }
    Ok(())
}
