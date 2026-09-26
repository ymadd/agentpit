//! Finding the daemon (design §11: `daemon/owner.json`, else the runtime-dir socket),
//! starting it with the bundled CLI when it is not running, and reaching loop runners
//! through it.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use agentpit_events::wire::{RequestBody, ResponseData, ROLE_LOOP};
use serde::Deserialize;

use super::board::StatusState;
use super::conn::Conn;
use super::{Bridge, BridgeError};

/// At most one `agentpit daemon start` per this long: a daemon that dies at startup must
/// not be respawned in a tight loop by every reconnect.
const AUTOSTART_EVERY: Duration = Duration::from_secs(10);

/// The per-user runtime dir the daemon's socket lives in. Mirrors the CLI's
/// `daemon::paths::runtime_dir` (the dashboard does not depend on the CLI crate): an
/// `$XDG_RUNTIME_DIR` too long for `sun_path` falls back to `/tmp/agentpit-<uid>`.
pub fn runtime_dir() -> PathBuf {
    const MAX_SOCKET_DIR_BYTES: usize = 100 - 48;
    if let Ok(dir) = std::env::var("XDG_RUNTIME_DIR") {
        if !dir.is_empty() {
            let candidate = PathBuf::from(&dir).join("agentpit");
            if candidate.as_os_str().len() <= MAX_SOCKET_DIR_BYTES {
                return candidate;
            }
        }
    }
    PathBuf::from("/tmp").join(format!("agentpit-{}", uid()))
}

#[cfg(unix)]
fn uid() -> u32 {
    // SAFETY: geteuid has no preconditions and cannot fail.
    unsafe { libc::geteuid() }
}

#[cfg(not(unix))]
fn uid() -> u32 {
    0
}

#[derive(Deserialize)]
struct OwnerRecord {
    #[serde(default)]
    socket: String,
}

/// Sockets to try, best first: the one the running daemon recorded, then the default.
pub fn daemon_sockets(owner_file: &Path, fallback: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(owner) = std::fs::read_to_string(owner_file)
        .ok()
        .and_then(|s| serde_json::from_str::<OwnerRecord>(&s).ok())
    {
        if !owner.socket.is_empty() {
            out.push(PathBuf::from(owner.socket));
        }
    }
    if !out.iter().any(|p| p == fallback) {
        out.push(fallback.to_path_buf());
    }
    out
}

fn daemon_down() -> BridgeError {
    BridgeError::new(
        "daemon_down",
        "the agentpit daemon is not running and could not be started; run \
         `agentpit daemon start` in a terminal to see why",
    )
}

impl Bridge {
    /// A daemon connection that speaks `loops/1`, starting the daemon when `autostart`.
    pub(super) async fn daemon(&self, autostart: bool) -> Result<Conn, BridgeError> {
        if let Some(conn) = self.probe_daemon().await? {
            return Ok(conn);
        }
        if !autostart {
            return Err(daemon_down());
        }
        let mut last = self.starting.lock().await;
        // Another caller may have started it while this one waited for the lock.
        if let Some(conn) = self.probe_daemon().await? {
            return Ok(conn);
        }
        if last.is_some_and(|t| t.elapsed() < AUTOSTART_EVERY) {
            return Err(daemon_down());
        }
        *last = Some(Instant::now());
        self.set_status(
            StatusState::Connecting,
            Some("starting the agentpit daemon"),
        );
        if let Err(e) = self.host.start_daemon().await {
            return Err(BridgeError::new(
                "daemon_down",
                format!("could not start the agentpit daemon: {e}"),
            ));
        }
        // `daemon start` returns once the socket answers; give the owner record a moment.
        for _ in 0..20 {
            if let Some(conn) = self.probe_daemon().await? {
                return Ok(conn);
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        Err(daemon_down())
    }

    /// The first daemon socket that answers, or `None`. A daemon without `loops/1` is an
    /// error: it is running, just too old.
    async fn probe_daemon(&self) -> Result<Option<Conn>, BridgeError> {
        for socket in daemon_sockets(&self.paths.owner_file, &self.paths.fallback_socket) {
            let Ok(conn) = Conn::connect(&socket, &self.client).await else {
                continue;
            };
            if conn.role != "daemon" {
                continue;
            }
            if !conn.has_loops() {
                return Err(BridgeError::new(
                    "daemon_outdated",
                    "the running agentpit daemon predates blueprint loops; run \
                     `agentpit daemon stop` so it restarts on this version",
                ));
            }
            return Ok(Some(conn));
        }
        Ok(None)
    }

    /// The socket of a live runner for `loop_id`, spawning one if needed (this wakes a
    /// parked loop). `gone` / `read_only` errors mean no runner will serve it.
    pub(super) async fn ensure_runner(&self, loop_id: &str) -> Result<PathBuf, BridgeError> {
        let mut daemon = self.daemon(true).await?;
        match daemon
            .request(RequestBody::LoopEnsure {
                loop_id: loop_id.to_string(),
            })
            .await?
        {
            ResponseData::LoopRunner { socket, .. } => Ok(PathBuf::from(socket)),
            other => Err(BridgeError::protocol(format!(
                "unexpected answer to loop_ensure: {other:?}"
            ))),
        }
    }

    /// A connection to a runner socket.
    pub(super) async fn runner(&self, socket: &Path) -> Result<Conn, BridgeError> {
        let conn = Conn::connect(socket, &self.client).await?;
        if conn.role != ROLE_LOOP {
            return Err(BridgeError::protocol(format!(
                "{} is a {}, not a loop runner",
                socket.display(),
                conn.role
            )));
        }
        Ok(conn)
    }
}
