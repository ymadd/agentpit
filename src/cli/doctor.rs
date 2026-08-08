//! `agentpit doctor [--fix]` — hygiene scan of the daemon layer (design §7.2 B6, after
//! prime's daemon-ps). Reports the daemon owner, every worker record, session leases,
//! and orphan sockets. `--fix` is deliberately SAFE-SIDE ONLY: it removes records and
//! files whose owners are provably dead and stops nothing that is alive — a running or
//! even an idle worker is never touched, and nothing is ever force-killed.

use std::path::{Path, PathBuf};

use anyhow::Result;
use console::style;

use crate::daemon::paths::{daemon_owner_path, runtime_dir, workers_dir};
use crate::daemon::registry;
use agentpit_events::session_lease::session_leases_dir;

/// One finding, classified. Pure data so the scan logic is unit-testable.
#[derive(Debug, Clone, PartialEq)]
pub struct Finding {
    pub what: String,
    pub status: FindingStatus,
    /// A path `--fix` may remove — only ever set for provably-dead debris.
    pub fixable: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum FindingStatus {
    Ok,
    /// Dead debris: safe to remove.
    Stale,
    /// Alive but wrong (unreachable daemon, lease without a session) — reported, never
    /// auto-fixed.
    Warn,
}

pub async fn run(fix: bool) -> Result<()> {
    let mut findings = Vec::new();

    // Daemon owner record.
    let owner_path = daemon_owner_path();
    match registry::load_owner(&owner_path) {
        None => findings.push(Finding {
            what: "daemon: not running (start with `agentpit daemon start`)".into(),
            status: FindingStatus::Ok,
            fixable: None,
        }),
        Some(owner) if !owner.alive() => findings.push(Finding {
            what: format!("daemon: stale owner record (pid {} is dead)", owner.pid),
            status: FindingStatus::Stale,
            fixable: Some(owner_path.clone()),
        }),
        Some(owner) => {
            let reachable = crate::daemon::client::Conn::connect(Path::new(&owner.socket))
                .await
                .is_ok();
            findings.push(Finding {
                what: if reachable {
                    format!("daemon: running (pid {})", owner.pid)
                } else {
                    format!(
                        "daemon: pid {} is alive but its socket does not answer — \
                         `agentpit daemon start` replaces it",
                        owner.pid
                    )
                },
                status: if reachable {
                    FindingStatus::Ok
                } else {
                    FindingStatus::Warn
                },
                fixable: None,
            });
        }
    }

    // Worker records: dead pid = stale; alive = probe the socket.
    let workers = workers_dir();
    let mut live_sockets: Vec<String> = Vec::new();
    for record in registry::load_all(&workers) {
        let record_path = workers.join(format!("{}.json", record.session_id));
        if !record.alive() {
            findings.push(Finding {
                what: format!(
                    "worker {}: record for a dead pid {}",
                    short(&record.session_id),
                    record.pid
                ),
                status: FindingStatus::Stale,
                fixable: Some(record_path),
            });
            continue;
        }
        live_sockets.push(record.socket.clone());
        let answers = crate::daemon::server::request_worker(
            Path::new(&record.socket),
            crate::daemon::protocol::RequestBody::Status,
        )
        .await
        .is_ok();
        findings.push(Finding {
            what: if answers {
                format!(
                    "worker {}: alive (pid {})",
                    short(&record.session_id),
                    record.pid
                )
            } else {
                format!(
                    "worker {}: pid {} is alive but its socket does not answer \
                     (a wedged worker; the daemon respawns it on the next attach)",
                    short(&record.session_id),
                    record.pid
                )
            },
            status: if answers {
                FindingStatus::Ok
            } else {
                FindingStatus::Warn
            },
            fixable: None,
        });
    }

    // Orphan worker sockets: a socket file with no live record behind it.
    if let Ok(entries) = std::fs::read_dir(runtime_dir()) {
        for entry in entries.filter_map(|e| e.ok()) {
            let name = entry.file_name().to_string_lossy().to_string();
            if !name.starts_with("worker-") || !name.ends_with(".sock") {
                continue;
            }
            let path = entry.path();
            if !live_sockets.iter().any(|s| Path::new(s) == path) {
                findings.push(Finding {
                    what: format!("socket {name}: no live worker behind it"),
                    status: FindingStatus::Stale,
                    fixable: Some(path),
                });
            }
        }
    }

    // Session leases: an owner that no longer exists is debris the next writer would
    // reclaim anyway — doctor just makes it visible (and removable).
    if let Ok(entries) = std::fs::read_dir(session_leases_dir()) {
        for entry in entries.filter_map(|e| e.ok()) {
            let dir = entry.path();
            if !dir.is_dir() {
                continue;
            }
            let owner_file = dir.join("owner.json");
            let alive = std::fs::read_to_string(&owner_file)
                .ok()
                .and_then(|body| serde_json::from_str::<serde_json::Value>(&body).ok())
                .and_then(|v| v.get("pid").and_then(|p| p.as_u64()))
                .map(|pid| agentpit_events::session_lease::pid_alive(pid as u32))
                .unwrap_or(false);
            if alive {
                findings.push(Finding {
                    what: format!(
                        "lease {}: held by a live process",
                        entry.file_name().to_string_lossy()
                    ),
                    status: FindingStatus::Ok,
                    fixable: None,
                });
            } else {
                findings.push(Finding {
                    what: format!(
                        "lease {}: owner is dead",
                        entry.file_name().to_string_lossy()
                    ),
                    status: FindingStatus::Stale,
                    fixable: Some(dir),
                });
            }
        }
    }

    render_and_fix(findings, fix)
}

fn render_and_fix(findings: Vec<Finding>, fix: bool) -> Result<()> {
    let mut stale = 0usize;
    for f in &findings {
        let tag = match f.status {
            FindingStatus::Ok => style("ok   ").green(),
            FindingStatus::Stale => {
                stale += 1;
                style("stale").yellow()
            }
            FindingStatus::Warn => style("warn ").red(),
        };
        println!("{tag}  {}", f.what);
    }
    if fix {
        let mut removed = 0usize;
        for f in findings.iter().filter(|f| f.status == FindingStatus::Stale) {
            if let Some(path) = &f.fixable {
                let ok = if path.is_dir() {
                    std::fs::remove_dir_all(path).is_ok()
                } else {
                    std::fs::remove_file(path).is_ok()
                };
                if ok {
                    removed += 1;
                }
            }
        }
        println!("\nremoved {removed} stale item(s). Nothing alive was touched.");
    } else if stale > 0 {
        println!(
            "\n{stale} stale item(s). Run `agentpit doctor --fix` to remove them \
             (only provably-dead debris is ever removed)."
        );
    }
    Ok(())
}

fn short(session_id: &str) -> &str {
    crate::cli::guidance::short_id(session_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fix_removes_only_stale_findings() {
        let tmp = tempfile::tempdir().unwrap();
        let stale_file = tmp.path().join("dead.json");
        let live_file = tmp.path().join("alive.json");
        std::fs::write(&stale_file, "x").unwrap();
        std::fs::write(&live_file, "x").unwrap();
        let findings = vec![
            Finding {
                what: "dead".into(),
                status: FindingStatus::Stale,
                fixable: Some(stale_file.clone()),
            },
            Finding {
                what: "alive".into(),
                status: FindingStatus::Ok,
                fixable: Some(live_file.clone()), // even if set, non-stale is never removed
            },
            Finding {
                what: "warn".into(),
                status: FindingStatus::Warn,
                fixable: None,
            },
        ];
        render_and_fix(findings, true).unwrap();
        assert!(!stale_file.exists(), "stale debris is removed");
        assert!(live_file.exists(), "non-stale is never touched");
    }
}
