//! The dashboard's window onto workspace loops (design §11, "Tauri ブリッジ").
//!
//! One daemon connection follows every loop's board row (`loop_watch`), and each loop the
//! webview has open gets a task that folds the loop's journal with the same code the CLI
//! and the runner use (`agentpit_events::loops`). The webview never re-implements the
//! state machine: it renders what the bridge emits.
//!
//! - `loops:board`: every loop's row, plus the daemon status.
//! - `loops:inbox`: open gates across all loops, and recently answered ones with who
//!   answered (the existing AskCards stay on `get_pending_asks`).
//! - `loops:view`: one open loop's [`LoopView`](agentpit_events::loops::LoopView).
//! - `loops:chunks`: live agent output of open loops.
//! - `daemon:status`: connecting / connected / down.
//!
//! Looking never wakes a parked loop: a view folds the journal from disk and attaches only
//! to a runner that is already live, or to one that should be running (restarting a
//! crashed runner is recovery). Operations go through the daemon's `loop_ensure` like any
//! other client, which wakes a parked loop.

mod blueprints;
mod board;
mod commands;
mod conn;
mod daemon;
mod view;

#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Instant;

use serde::Serialize;
use serde_json::Value;

pub use blueprints::*;
pub use commands::*;

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// What the bridge needs from the app: a way to reach the webview and a way to start the
/// daemon (the bundled CLI). Tests substitute a recorder and an in-process fake daemon.
pub trait Host: Send + Sync + 'static {
    fn emit(&self, event: &str, payload: Value);
    /// Run `agentpit daemon start`; resolves once the daemon answers (or failed).
    fn start_daemon(&self) -> BoxFuture<'_, Result<(), String>>;
}

/// Every bridge failure the webview sees: a stable `code` to branch on (`conflict` opens
/// the conflict dialog, `daemon_down` shows the banner), a sentence for people, and the
/// server's structured details when it sent any (validation diagnostics, current rev).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct BridgeError {
    pub code: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
}

impl BridgeError {
    pub fn new(code: &str, message: impl Into<String>) -> Self {
        BridgeError {
            code: code.to_string(),
            message: message.into(),
            details: None,
        }
    }

    pub fn with_details(mut self, details: Value) -> Self {
        self.details = Some(details);
        self
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self::new("internal", message)
    }

    pub fn protocol(message: impl Into<String>) -> Self {
        Self::new("protocol", message)
    }

    pub fn closed(message: impl Into<String>) -> Self {
        Self::new("closed", message)
    }

    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new("bad_request", message)
    }
}

impl fmt::Display for BridgeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ({})", self.message, self.code)
    }
}

/// Where the bridge finds the daemon and the loops. From the environment in the app;
/// a temp dir in tests.
#[derive(Debug, Clone)]
pub struct Paths {
    /// `<state>/daemon/owner.json`: the running daemon's pid and socket.
    pub owner_file: PathBuf,
    /// Where the daemon listens when there is no owner record to ask.
    pub fallback_socket: PathBuf,
    /// `<state>/loops`.
    pub loops_root: PathBuf,
}

impl Paths {
    pub fn from_env() -> Paths {
        let state = agentpit_events::state_dir();
        Paths {
            owner_file: state.join("daemon").join("owner.json"),
            fallback_socket: daemon::runtime_dir().join("daemon.sock"),
            loops_root: agentpit_events::loops::loops_dir(),
        }
    }
}

pub struct Bridge {
    host: Arc<dyn Host>,
    paths: Paths,
    /// `hello.client` and the actor recorded on this app's operations.
    client: String,
    board: Mutex<board::Board>,
    views: Mutex<HashMap<String, view::ViewEntry>>,
    /// When the bridge last ran `agentpit daemon start`; held while starting so
    /// concurrent callers wait for one start instead of each spawning a daemon.
    starting: tokio::sync::Mutex<Option<Instant>>,
}

impl Bridge {
    pub fn new(host: Arc<dyn Host>, paths: Paths) -> Arc<Bridge> {
        Arc::new(Bridge {
            host,
            paths,
            client: format!("agentpit-dashboard/{}", env!("CARGO_PKG_VERSION")),
            board: Mutex::new(board::Board::default()),
            views: Mutex::new(HashMap::new()),
            starting: tokio::sync::Mutex::new(None),
        })
    }

    fn emit<T: Serialize>(&self, event: &str, payload: &T) {
        match serde_json::to_value(payload) {
            Ok(v) => self.host.emit(event, v),
            Err(e) => eprintln!("agentpit-dashboard: could not encode {event}: {e}"),
        }
    }
}

/// A poisoned lock only means another thread panicked mid-update of plain data; keep going.
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}
