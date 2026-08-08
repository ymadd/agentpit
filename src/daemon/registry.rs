//! Durable worker descriptors + the daemon's single-instance owner record (design §5.4-5.5).
//!
//! One JSON file per worker under `state/daemon/workers/<session_id>.json`. The daemon
//! writes it at spawn, checks pid + process-start-id (PID-reuse guard) when asked for the
//! worker again, and removes it when the worker is confirmed dead. Files, not sockets, so
//! a restarted daemon can find every surviving worker (§5.4: workers outlive the daemon).

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use agentpit_events::session_lease::{pid_alive, process_start_id};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WorkerRecord {
    pub session_id: String,
    pub pid: u32,
    pub start_id: String,
    pub socket: String,
}

impl WorkerRecord {
    /// True when the recorded pid is alive AND is still the same incarnation.
    pub fn alive(&self) -> bool {
        if !pid_alive(self.pid) {
            return false;
        }
        if self.start_id.is_empty() {
            return true;
        }
        let current = process_start_id(self.pid);
        current.is_empty() || current == self.start_id
    }
}

fn record_path(dir: &Path, session_id: &str) -> PathBuf {
    dir.join(format!("{session_id}.json"))
}

/// Persist a worker record (atomic tmp+rename so a reader never sees a torn file).
pub fn save(dir: &Path, record: &WorkerRecord) -> std::io::Result<()> {
    fs::create_dir_all(dir)?;
    let path = record_path(dir, &record.session_id);
    let tmp = path.with_extension("json.tmp");
    fs::write(
        &tmp,
        serde_json::to_vec(record).map_err(std::io::Error::other)?,
    )?;
    fs::rename(&tmp, &path)
}

/// Load one session's record, if present and parseable.
pub fn load(dir: &Path, session_id: &str) -> Option<WorkerRecord> {
    let body = fs::read_to_string(record_path(dir, session_id)).ok()?;
    serde_json::from_str(&body).ok()
}

/// Remove a record (worker confirmed dead or stopped).
pub fn remove(dir: &Path, session_id: &str) {
    let _ = fs::remove_file(record_path(dir, session_id));
}

/// All records on disk. Corrupt files are skipped (and left for `doctor` to report).
pub fn load_all(dir: &Path) -> Vec<WorkerRecord> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("json"))
        .filter_map(|e| {
            serde_json::from_str::<WorkerRecord>(&fs::read_to_string(e.path()).ok()?).ok()
        })
        .collect()
}

/// session_ids whose `<id>.json` exists but does NOT parse. A running worker with a
/// corrupt record would otherwise be invisible to `load_all`, so `doctor` must not treat
/// its (still-live) socket as an orphan (H6).
pub fn corrupt_session_ids(dir: &Path) -> Vec<String> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("json"))
        .filter(|e| {
            fs::read_to_string(e.path())
                .ok()
                .and_then(|b| serde_json::from_str::<WorkerRecord>(&b).ok())
                .is_none()
        })
        .filter_map(|e| {
            e.path()
                .file_stem()
                .and_then(|s| s.to_str())
                .map(str::to_string)
        })
        .collect()
}

/// The daemon's own single-instance record. Same alive() discipline as workers.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct OwnerRecord {
    pub pid: u32,
    pub start_id: String,
    pub socket: String,
}

impl OwnerRecord {
    pub fn current(socket: &Path) -> OwnerRecord {
        let pid = std::process::id();
        OwnerRecord {
            pid,
            start_id: process_start_id(pid),
            socket: socket.display().to_string(),
        }
    }

    pub fn alive(&self) -> bool {
        WorkerRecord {
            session_id: String::new(),
            pid: self.pid,
            start_id: self.start_id.clone(),
            socket: String::new(),
        }
        .alive()
    }
}

pub fn save_owner(path: &Path, record: &OwnerRecord) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    fs::write(
        &tmp,
        serde_json::to_vec(record).map_err(std::io::Error::other)?,
    )?;
    fs::rename(&tmp, path)
}

pub fn load_owner(path: &Path) -> Option<OwnerRecord> {
    serde_json::from_str(&fs::read_to_string(path).ok()?).ok()
}

pub fn remove_owner(path: &Path) {
    let _ = fs::remove_file(path);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(session: &str, pid: u32, start_id: &str) -> WorkerRecord {
        WorkerRecord {
            session_id: session.into(),
            pid,
            start_id: start_id.into(),
            socket: format!("/tmp/worker-{session}.sock"),
        }
    }

    #[test]
    fn save_load_roundtrip_and_removal() {
        let tmp = tempfile::tempdir().unwrap();
        let record = rec("s1", 1234, "boot");
        save(tmp.path(), &record).unwrap();
        assert_eq!(load(tmp.path(), "s1"), Some(record.clone()));
        assert_eq!(load_all(tmp.path()), vec![record]);
        remove(tmp.path(), "s1");
        assert_eq!(load(tmp.path(), "s1"), None);
    }

    #[test]
    fn corrupt_records_are_skipped_not_fatal() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("bad.json"), "not json").unwrap();
        save(tmp.path(), &rec("good", 1, "x")).unwrap();
        let all = load_all(tmp.path());
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].session_id, "good");
    }

    #[test]
    fn corrupt_session_ids_names_the_unparseable_records_only() {
        // H6: doctor uses this to shield a live-but-corrupt worker's socket from reaping.
        let tmp = tempfile::tempdir().unwrap();
        save(tmp.path(), &rec("good", 1, "x")).unwrap();
        fs::write(tmp.path().join("019fe0-broken.json"), "{ not valid").unwrap();
        fs::write(tmp.path().join("ignore.txt"), "not a record file").unwrap();
        let corrupt = corrupt_session_ids(tmp.path());
        assert_eq!(corrupt, vec!["019fe0-broken".to_string()]);
    }

    #[test]
    fn dead_pid_is_not_alive() {
        assert!(!rec("s", 4_000_000, "anything").alive());
    }

    #[test]
    fn own_pid_with_current_start_id_is_alive() {
        let pid = std::process::id();
        let start = process_start_id(pid);
        assert!(rec("s", pid, &start).alive());
        // A mismatching incarnation is dead — unless the platform can't say (empty).
        if !start.is_empty() {
            assert!(!rec("s", pid, "some-other-incarnation").alive());
        }
    }

    #[test]
    fn owner_record_mirrors_worker_liveness() {
        let sock = PathBuf::from("/tmp/daemon.sock");
        let owner = OwnerRecord::current(&sock);
        assert!(owner.alive());
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("owner.json");
        save_owner(&path, &owner).unwrap();
        assert_eq!(load_owner(&path), Some(owner));
        remove_owner(&path);
        assert_eq!(load_owner(&path), None);
    }
}
