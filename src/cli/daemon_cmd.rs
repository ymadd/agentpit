//! `agentpit daemon` — start/stop/status plus the hidden `run` (foreground daemon) and
//! `worker` (per-session worker process) entrypoints (design §5.5).

use anyhow::Result;
use clap::Subcommand;
use console::style;

use crate::daemon::client::{connect_daemon, spawn_daemon};
use crate::daemon::paths::{daemon_owner_path, daemon_socket_path, workers_dir};
use crate::daemon::protocol::{RequestBody, ResponseData};
use crate::daemon::registry;

#[derive(Debug, Subcommand)]
pub enum Action {
    /// Start the daemon in the background (no-op if one is already running).
    Start,
    /// Stop the daemon. Workers keep running unless --all.
    Stop {
        /// Also stop every session worker.
        #[arg(long)]
        all: bool,
    },
    /// Show daemon and worker liveness.
    Status,
    /// Run the daemon in the foreground (what `start` spawns).
    #[command(hide = true)]
    Run,
    /// Run a session worker (spawned by the daemon; never run by hand).
    #[command(hide = true)]
    Worker {
        #[arg(long)]
        session: String,
        #[arg(long)]
        socket: String,
    },
}

pub async fn run(action: Action) -> Result<()> {
    match action {
        Action::Start => {
            if connect_daemon(false).await.is_ok() {
                println!("daemon is already running");
                return Ok(());
            }
            spawn_daemon()?;
            // Confirm it actually came up before reporting success.
            let mut conn = connect_daemon(false).await;
            for _ in 0..50 {
                if conn.is_ok() {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                conn = connect_daemon(false).await;
            }
            match conn {
                Ok(_) => {
                    println!("daemon started ({})", daemon_socket_path().display());
                    Ok(())
                }
                Err(e) => Err(e),
            }
        }

        Action::Stop { all } => {
            let mut conn = match connect_daemon(false).await {
                Ok(c) => c,
                Err(_) => {
                    println!("no daemon is running");
                    return Ok(());
                }
            };
            conn.request(RequestBody::Shutdown { all }).await?;
            println!(
                "daemon stopped{}",
                if all {
                    " (workers included)"
                } else {
                    " (workers keep running)"
                }
            );
            Ok(())
        }

        Action::Status => {
            match registry::load_owner(&daemon_owner_path()) {
                Some(owner) if owner.alive() => {
                    println!("daemon:  {} (pid {})", style("running").green(), owner.pid);
                }
                Some(_) => println!(
                    "daemon:  {} (stale record; start with `agentpit daemon start`)",
                    style("dead").red()
                ),
                None => println!(
                    "daemon:  {} (start with `agentpit daemon start`)",
                    style("not running").yellow()
                ),
            }
            let workers = registry::load_all(&workers_dir());
            if workers.is_empty() {
                println!("workers: none");
            } else {
                for w in workers {
                    let alive = w.alive();
                    println!(
                        "worker:  {} {} (pid {})",
                        &w.session_id[w.session_id.len().saturating_sub(12)..],
                        if alive {
                            style("alive").green()
                        } else {
                            style("dead").red()
                        },
                        w.pid
                    );
                }
            }
            // Live list through the daemon when it answers (states included).
            if let Ok(mut conn) = connect_daemon(false).await
                && let Ok(ResponseData::Sessions { sessions }) =
                    conn.request(RequestBody::List).await
            {
                for row in sessions {
                    println!(
                        "session: {}  {}",
                        &row.session_id[row.session_id.len().saturating_sub(12)..],
                        row.state
                    );
                }
            }
            Ok(())
        }

        Action::Run => crate::daemon::server::run_daemon().await,

        Action::Worker { session, socket } => {
            crate::daemon::worker::run_worker(session, socket.into()).await
        }
    }
}
