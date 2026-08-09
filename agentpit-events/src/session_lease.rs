//! Single-writer lease for session files (design §6.2).
//!
//! A session file must have exactly one writer at a time (a REPL in phase 1, a daemon
//! worker later). The lease is a directory under `session-leases/` keyed by a hash of the
//! session file path: `mkdir` is the atomic acquisition, and `owner.json` records who holds
//! it so a dead owner's lease can be reclaimed. PID reuse is guarded by the owner process's
//! start id (`/proc/<pid>/stat` on Linux, `ps -o lstart=` on macOS) — the same discipline
//! prime-agent uses. Unlike prime (leases off by default outside the daemon), agentpit
//! takes a lease for EVERY writer: daemonless REPL writes and worker writes coexist during
//! the phased rollout, and one rule for all writers is simpler and safer.
//!
//! Non-unix: process liveness can't be probed portably without extra deps, so a foreign
//! lease is conservatively treated as alive (never stolen). Windows is out of scope (Q1).

use std::fs;
use std::path::{Path, PathBuf};
use std::process;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::{fnv1a_64_hex, state_dir};

/// Root directory holding one lease dir per session file.
pub fn session_leases_dir() -> PathBuf {
    state_dir().join("session-leases")
}

/// An owner.json without a readable owner is treated as mid-write this long before it is
/// considered debris and reclaimed.
const ORPHAN_DIR_GRACE: Duration = Duration::from_secs(60);

#[derive(Debug, Serialize, Deserialize)]
struct OwnerInfo {
    pid: u32,
    /// Process start id; empty when the platform lookup failed (then pid liveness alone
    /// decides).
    start_id: String,
    taken_at_ms: u64,
}

#[derive(Debug)]
pub enum LeaseError {
    /// Another live process holds the lease. `pid` 0 = owner unknown (mid-write dir).
    Busy {
        pid: u32,
    },
    Io(std::io::Error),
}

impl std::fmt::Display for LeaseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LeaseError::Busy { pid } if *pid > 0 => {
                write!(f, "session is held by another process (pid {pid})")
            }
            LeaseError::Busy { .. } => write!(f, "session is held by another process"),
            LeaseError::Io(e) => write!(f, "lease I/O error: {e}"),
        }
    }
}

impl std::error::Error for LeaseError {}

impl From<std::io::Error> for LeaseError {
    fn from(e: std::io::Error) -> Self {
        LeaseError::Io(e)
    }
}

/// A held lease. Dropping it releases the lease (best-effort).
pub struct SessionLease {
    key_dir: PathBuf,
    released: bool,
}

impl SessionLease {
    /// Acquire the lease for `session_file` under the default state dir.
    pub fn acquire(session_file: &Path) -> Result<SessionLease, LeaseError> {
        Self::acquire_at(&session_leases_dir(), session_file)
    }

    /// Acquire under an explicit lease root (tests).
    pub fn acquire_at(leases_root: &Path, session_file: &Path) -> Result<SessionLease, LeaseError> {
        Self::acquire_inner(leases_root, session_file, true)
    }

    fn acquire_inner(
        leases_root: &Path,
        session_file: &Path,
        may_reclaim: bool,
    ) -> Result<SessionLease, LeaseError> {
        fs::create_dir_all(leases_root)?;
        // Canonicalize when possible so `./x.jsonl` and its absolute path share one lease.
        let canonical = session_file
            .canonicalize()
            .unwrap_or_else(|_| session_file.to_path_buf());
        let key = fnv1a_64_hex(canonical.to_string_lossy().as_bytes());
        let key_dir = leases_root.join(key);

        match fs::create_dir(&key_dir) {
            Ok(()) => {
                let owner = OwnerInfo {
                    pid: process::id(),
                    start_id: process_start_id(process::id()),
                    taken_at_ms: SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .map(|d| d.as_millis() as u64)
                        .unwrap_or(0),
                };
                let tmp = key_dir.join("owner.json.tmp");
                let body = serde_json::to_vec(&owner).map_err(std::io::Error::other)?;
                fs::write(&tmp, body)?;
                fs::rename(&tmp, key_dir.join("owner.json"))?;
                Ok(SessionLease {
                    key_dir,
                    released: false,
                })
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                let stale = match fs::read_to_string(key_dir.join("owner.json")) {
                    Ok(body) => match serde_json::from_str::<OwnerInfo>(&body) {
                        Ok(owner) => {
                            if owner_alive(&owner) {
                                return Err(LeaseError::Busy { pid: owner.pid });
                            }
                            true
                        }
                        Err(_) => true, // corrupt owner file = debris
                    },
                    // owner.json not there yet: another process may be mid-acquisition.
                    Err(_) => {
                        let old = fs::metadata(&key_dir)
                            .and_then(|m| m.modified())
                            .map(|m| {
                                SystemTime::now()
                                    .duration_since(m)
                                    .unwrap_or(Duration::ZERO)
                                    > ORPHAN_DIR_GRACE
                            })
                            .unwrap_or(false);
                        if !old {
                            return Err(LeaseError::Busy { pid: 0 });
                        }
                        true
                    }
                };
                if stale && may_reclaim {
                    let _ = fs::remove_dir_all(&key_dir);
                    // One retry only: if we lose the re-acquisition race, report Busy
                    // rather than fighting over reclaim in a loop.
                    return Self::acquire_inner(leases_root, session_file, false);
                }
                Err(LeaseError::Busy { pid: 0 })
            }
            Err(e) => Err(LeaseError::Io(e)),
        }
    }

    /// Release explicitly (also happens on drop).
    pub fn release(mut self) {
        self.release_inner();
    }

    fn release_inner(&mut self) {
        if self.released {
            return;
        }
        self.released = true;
        let _ = fs::remove_file(self.key_dir.join("owner.json"));
        let _ = fs::remove_dir(&self.key_dir);
    }
}

impl Drop for SessionLease {
    fn drop(&mut self) {
        self.release_inner();
    }
}

fn owner_alive(owner: &OwnerInfo) -> bool {
    if !pid_alive(owner.pid) {
        return false;
    }
    if owner.start_id.is_empty() {
        return true; // no start id recorded — pid liveness is all we have
    }
    let current = process_start_id(owner.pid);
    if current.is_empty() {
        return true; // lookup failed — be conservative, don't steal
    }
    current == owner.start_id
}

/// Probe whether `pid` is alive — and actually RUNNING, not a zombie. A SIGKILLed child
/// whose parent has not reaped it yet still answers `kill -0` and still has a /proc dir,
/// which made a crashed worker's lease look held forever (found 2026-08-08 in the daemon
/// crash-recovery test). Zombies are the walking dead: treat them as dead.
pub fn pid_alive(pid: u32) -> bool {
    #[cfg(target_os = "linux")]
    {
        // /proc/<pid>/stat state field (first char after the comm's closing paren):
        // Z = zombie.
        match fs::read_to_string(format!("/proc/{pid}/stat")) {
            Err(_) => false,
            Ok(stat) => !matches!(
                stat.rsplit_once(')')
                    .map(|(_, tail)| tail.trim_start().chars().next()),
                Some(Some('Z')) | None
            ),
        }
    }
    #[cfg(all(unix, not(target_os = "linux")))]
    {
        // `ps -o stat=` answers only for existing processes; a leading 'Z' is a zombie.
        // Same-uid processes only (foreign-uid EPERM cases don't arise for agentpit).
        std::process::Command::new("ps")
            .args(["-p", &pid.to_string(), "-o", "stat="])
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| {
                let stat = String::from_utf8_lossy(&o.stdout);
                let stat = stat.trim();
                !stat.is_empty() && !stat.starts_with('Z')
            })
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        true // out of scope (Q1): never treat a foreign lease as dead
    }
}

/// A string that identifies THIS incarnation of `pid` — stable across the process's life,
/// different for a later process that recycles the pid. Empty when unavailable.
/// Exposed for the daemon's worker registry (PID-reuse guard on reconnect).
pub fn process_start_id(pid: u32) -> String {
    #[cfg(target_os = "linux")]
    {
        // /proc/<pid>/stat: fields after the last ')' — comm can contain spaces/parens.
        // starttime is overall field 22, i.e. index 19 of the post-comm tail.
        let Ok(stat) = fs::read_to_string(format!("/proc/{pid}/stat")) else {
            return String::new();
        };
        let Some(tail) = stat.rsplit_once(')').map(|(_, t)| t) else {
            return String::new();
        };
        tail.split_whitespace().nth(19).unwrap_or("").to_string()
    }
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("ps")
            .args(["-p", &pid.to_string(), "-o", "lstart="])
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_default()
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = pid;
        String::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn acquire_release_acquire() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("s.jsonl");
        fs::write(&file, "x").unwrap();
        let root = tmp.path().join("leases");

        let lease = SessionLease::acquire_at(&root, &file).unwrap();
        match SessionLease::acquire_at(&root, &file) {
            Err(LeaseError::Busy { pid }) => assert_eq!(pid, process::id()),
            other => panic!("expected Busy, got {other:?}", other = other.map(|_| ())),
        }
        lease.release();
        SessionLease::acquire_at(&root, &file).unwrap();
    }

    #[test]
    fn drop_releases() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("s.jsonl");
        fs::write(&file, "x").unwrap();
        let root = tmp.path().join("leases");
        {
            let _lease = SessionLease::acquire_at(&root, &file).unwrap();
        }
        SessionLease::acquire_at(&root, &file).unwrap();
    }

    #[test]
    fn dead_owner_is_reclaimed() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("s.jsonl");
        fs::write(&file, "x").unwrap();
        let root = tmp.path().join("leases");

        // Forge a lease held by a (dead) pid. Pid 1 is init and always alive, so use a
        // huge pid that cannot exist on either dev platform.
        let canonical = file.canonicalize().unwrap();
        let key = fnv1a_64_hex(canonical.to_string_lossy().as_bytes());
        let key_dir = root.join(key);
        fs::create_dir_all(&key_dir).unwrap();
        let owner = OwnerInfo {
            pid: 4_000_000,
            start_id: "whatever".into(),
            taken_at_ms: 0,
        };
        fs::write(
            key_dir.join("owner.json"),
            serde_json::to_vec(&owner).unwrap(),
        )
        .unwrap();

        SessionLease::acquire_at(&root, &file).expect("dead owner must be reclaimed");
    }

    #[test]
    fn live_pid_with_wrong_start_id_is_reclaimed() {
        // Our own pid is alive, but a mismatching start id proves the lease belonged to a
        // previous incarnation — it must be reclaimable. (On platforms where the start-id
        // lookup fails this degrades to Busy; both dev targets implement the lookup.)
        if process_start_id(process::id()).is_empty() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("s.jsonl");
        fs::write(&file, "x").unwrap();
        let root = tmp.path().join("leases");

        let canonical = file.canonicalize().unwrap();
        let key = fnv1a_64_hex(canonical.to_string_lossy().as_bytes());
        let key_dir = root.join(key);
        fs::create_dir_all(&key_dir).unwrap();
        let owner = OwnerInfo {
            pid: process::id(),
            start_id: "not-the-current-incarnation".into(),
            taken_at_ms: 0,
        };
        fs::write(
            key_dir.join("owner.json"),
            serde_json::to_vec(&owner).unwrap(),
        )
        .unwrap();

        SessionLease::acquire_at(&root, &file).expect("stale incarnation must be reclaimed");
    }

    #[test]
    fn corrupt_owner_file_is_reclaimed() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("s.jsonl");
        fs::write(&file, "x").unwrap();
        let root = tmp.path().join("leases");

        let canonical = file.canonicalize().unwrap();
        let key = fnv1a_64_hex(canonical.to_string_lossy().as_bytes());
        let key_dir = root.join(key);
        fs::create_dir_all(&key_dir).unwrap();
        fs::write(key_dir.join("owner.json"), b"not json").unwrap();

        SessionLease::acquire_at(&root, &file).expect("corrupt owner must be reclaimed");
    }

    #[test]
    fn killed_unreaped_child_is_dead_not_alive() {
        // Regression (2026-08-08): a SIGKILLed worker left as a zombie (parent never
        // waited) kept answering `kill -0`, so its lease was never reclaimed. pid_alive
        // must see through zombies.
        let mut child = std::process::Command::new("sleep")
            .arg("100")
            .spawn()
            .unwrap();
        let pid = child.id();
        assert!(pid_alive(pid), "freshly spawned child is alive");
        let _ = std::process::Command::new("kill")
            .args(["-9", &pid.to_string()])
            .status();
        // Deliberately do NOT reap yet: the child is now a zombie.
        std::thread::sleep(std::time::Duration::from_millis(200));
        assert!(!pid_alive(pid), "a zombie must be treated as dead");
        let _ = child.wait(); // clean up
    }

    #[test]
    fn same_file_by_relative_and_absolute_path_shares_one_lease() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("s.jsonl");
        fs::write(&file, "x").unwrap();
        let root = tmp.path().join("leases");

        let _lease = SessionLease::acquire_at(&root, &file).unwrap();
        // A second acquisition through a non-canonical spelling must still collide.
        let dotted = tmp.path().join(".").join("s.jsonl");
        assert!(matches!(
            SessionLease::acquire_at(&root, &dotted),
            Err(LeaseError::Busy { .. })
        ));
    }
}
