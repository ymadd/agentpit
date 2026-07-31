// Hide the console window on Windows release builds.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app_update;
mod arena;
mod asks;
mod cli_link;
mod cli_runner;
mod cli_versions;
mod learning;
mod model_catalog;
mod settings;
mod state;
mod workflow_gen;

use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::Mutex;
use std::time::Duration;

use notify::{RecursiveMode, Watcher};
use tauri::{AppHandle, Emitter, Manager, State};

use state::{Snapshot, Tracker};

const RECENT_CAP: usize = 30;
const MAX_OUTPUT_BYTES: u64 = 256 * 1024;

/// Shared, incrementally-updated view of the event log. Guarded by a mutex because both
/// the watcher thread and the `get_snapshot` command read/advance it.
struct AppState {
    tracker: Mutex<Tracker>,
}

/// Is a process with this pid currently alive?
#[cfg(unix)]
fn pid_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    // kill(pid, 0) probes existence without sending a signal.
    let rc = unsafe { libc::kill(pid as libc::pid_t, 0) };
    if rc == 0 {
        return true;
    }
    // EPERM means the process exists but we may not signal it.
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(not(unix))]
fn pid_alive(pid: u32) -> bool {
    // Non-unix fallback: assume alive; the run_finished event is the real signal.
    pid != 0
}

fn events_path() -> PathBuf {
    agentpit_events::events_path()
}

/// Re-read any newly-appended events into the tracker and produce a fresh snapshot.
fn refresh(state: &AppState) -> Snapshot {
    let mut tracker = state.tracker.lock().unwrap_or_else(|e| e.into_inner());
    tracker.ingest(&events_path());
    tracker.snapshot(pid_alive, RECENT_CAP)
}

#[tauri::command]
fn get_snapshot(state: State<AppState>) -> Snapshot {
    refresh(&state)
}

#[tauri::command]
async fn get_agent_clis() -> Result<Vec<cli_versions::AgentCliInfo>, String> {
    tauri::async_runtime::spawn_blocking(cli_versions::list)
        .await
        .map_err(|error| format!("agent CLI scan failed: {error}"))
}

#[tauri::command]
async fn update_agent_cli(id: String) -> Result<cli_versions::AgentCliUpdate, String> {
    tauri::async_runtime::spawn_blocking(move || cli_versions::update(&id))
        .await
        .map_err(|error| format!("agent CLI update failed: {error}"))?
}

#[tauri::command]
async fn get_model_catalogs(refresh: bool) -> Vec<model_catalog::ModelCatalog> {
    model_catalog::list(refresh).await
}

/// A delta of a backend leg's captured output: only the bytes appended since `offset`.
#[derive(serde::Serialize, Default)]
struct OutputChunk {
    /// New text to append (empty if nothing new). Always ends on a UTF-8 boundary.
    text: String,
    /// Byte offset to pass back on the next call.
    offset: u64,
    /// True when the caller should clear before appending (file rotated, or we skipped
    /// ahead because it was too far behind).
    reset: bool,
}

/// Stream a backend leg's captured output incrementally. The frontend passes the `offset`
/// from the previous call and appends `text`, so reading/scrolling/selecting isn't reset
/// every tick. Multibyte chars split across reads are held back until complete.
#[tauri::command]
fn get_output(run_id: String, backend: String, aggregator: bool, offset: u64) -> OutputChunk {
    // Reject run_id or backend values that could escape the runs directory via path
    // traversal (e.g. "../.ssh/id_rsa"). PathBuf::join does not sanitise ".." and an
    // absolute component replaces the entire prefix, so we must validate up front.
    if !agentpit_events::is_safe_log_component(&run_id)
        || !agentpit_events::is_safe_log_component(&backend)
    {
        return OutputChunk {
            offset,
            ..Default::default()
        };
    }
    let path = agentpit_events::backend_log_path(&run_id, &backend, aggregator);
    read_delta(&path, offset, MAX_OUTPUT_BYTES)
}

/// Read the bytes of `path` after `offset`, capped to a trailing `max` window. Returns
/// only up to the last valid UTF-8 boundary so a split multibyte char waits for the rest.
fn read_delta(path: &std::path::Path, offset: u64, max: u64) -> OutputChunk {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(_) => {
            return OutputChunk {
                offset,
                ..Default::default()
            }
        }
    };
    let len = file.metadata().map(|m| m.len()).unwrap_or(0);

    let mut start = offset;
    let mut reset = false;
    if len < offset {
        // File was rotated/compacted; start over.
        start = 0;
        reset = true;
    }
    if len.saturating_sub(start) > max {
        // Too far behind (or first open of a big file): jump to the last window.
        start = len - max;
        reset = true;
    }
    if file.seek(SeekFrom::Start(start)).is_err() {
        return OutputChunk {
            offset: start,
            reset,
            ..Default::default()
        };
    }
    let mut buf = Vec::new();
    if file.read_to_end(&mut buf).is_err() {
        return OutputChunk {
            offset: start,
            reset,
            ..Default::default()
        };
    }
    let (valid_len, text) = match std::str::from_utf8(&buf) {
        Ok(s) => (buf.len(), s.to_string()),
        Err(e) => {
            let v = e.valid_up_to();
            (v, String::from_utf8_lossy(&buf[..v]).into_owned())
        }
    };
    OutputChunk {
        text,
        offset: start + valid_len as u64,
        reset,
    }
}

/// Watch the event log (and its parent dir, since the file may not exist yet) and push a
/// fresh snapshot to the frontend on every change. A periodic tick also re-evaluates pid
/// liveness so a crashed run leaves the live list even with no new log writes.
fn spawn_watcher(app: AppHandle) {
    std::thread::spawn(move || {
        let path = events_path();
        let dir = path.parent().map(PathBuf::from).unwrap_or_default();
        let _ = std::fs::create_dir_all(&dir);

        let (tx, rx) = mpsc::channel();
        let mut watcher = match notify::recommended_watcher(move |res| {
            let _ = tx.send(res);
        }) {
            Ok(w) => w,
            Err(e) => {
                eprintln!("dashboard: failed to create watcher: {e}");
                return;
            }
        };
        if let Err(e) = watcher.watch(&dir, RecursiveMode::NonRecursive) {
            eprintln!("dashboard: failed to watch {}: {e}", dir.display());
        }

        let emit = |app: &AppHandle| {
            let state = app.state::<AppState>();
            let _ = app.emit("snapshot", refresh(&state));
        };
        emit(&app);

        loop {
            // Coalesce a burst of fs events, and tick every 2s for liveness re-checks.
            match rx.recv_timeout(Duration::from_secs(2)) {
                Ok(_) => {
                    while rx.try_recv().is_ok() {}
                    std::thread::sleep(Duration::from_millis(60));
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
            emit(&app);
        }
    });
}

/// GUI apps launched from Finder/Dock inherit launchd's minimal PATH
/// (`/usr/bin:/bin:/usr/sbin:/sbin`), which misses the directories where the agent
/// CLIs live (e.g. `~/.local/bin/claude`). The sidecar `agentpit` CLI inherits this
/// process's environment, so its auth probes and spawns would fail with exit 127 and
/// surface as "[claude] not authenticated". Ask the user's login shell for its PATH
/// once at startup and adopt it. Markers guard against shell startup noise on stdout.
#[cfg(unix)]
fn adopt_login_shell_path() {
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
    let Ok(output) = std::process::Command::new(&shell)
        .args([
            "-ilc",
            "printf '__AGENTPIT_PATH__%s__AGENTPIT_PATH__' \"$PATH\"",
        ])
        .output()
    else {
        return;
    };
    if !output.status.success() {
        return;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    if let Some(path) = extract_marked_path(&stdout) {
        std::env::set_var("PATH", path);
    }
}

fn extract_marked_path(stdout: &str) -> Option<&str> {
    const MARKER: &str = "__AGENTPIT_PATH__";
    let start = stdout.find(MARKER)? + MARKER.len();
    let end = stdout[start..].find(MARKER)? + start;
    let path = &stdout[start..end];
    (!path.is_empty()).then_some(path)
}

fn main() {
    #[cfg(unix)]
    adopt_login_shell_path();
    tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .manage(AppState {
            tracker: Mutex::new(Tracker::new()),
        })
        .invoke_handler(tauri::generate_handler![
            get_snapshot,
            get_agent_clis,
            update_agent_cli,
            get_model_catalogs,
            get_output,
            learning::learning_status,
            asks::get_pending_asks,
            asks::answer_ask,
            settings::settings_get,
            settings::settings_save,
            settings::config_get,
            settings::config_save,
            arena::arena_templates,
            arena::arena_rounds,
            arena::arena_round,
            arena::arena_reveal,
            arena::arena_leaderboard,
            arena::arena_vote,
            arena::arena_run,
            cli_link::cli_link_status,
            cli_link::cli_link_install,
            cli_link::cli_link_remove,
            app_update::app_update_check,
            app_update::skills_refresh,
            app_update::app_update_install,
            app_update::app_restart,
            workflow_gen::workflow_generate,
            workflow_gen::workflow_describe
        ])
        .setup(|app| {
            spawn_watcher(app.handle().clone());
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running agentpit dashboard");
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn tmp_with(content: &[u8]) -> tempfile::NamedTempFile {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(content).unwrap();
        f.flush().unwrap();
        f
    }

    #[test]
    fn delta_reads_only_new_bytes() {
        let mut f = tmp_with(b"hello\n");
        let a = read_delta(f.path(), 0, 1024);
        assert_eq!(a.text, "hello\n");
        assert_eq!(a.offset, 6);
        assert!(!a.reset);
        // Append, then read from the prior offset — only the new bytes come back.
        f.write_all(b"world\n").unwrap();
        f.flush().unwrap();
        let b = read_delta(f.path(), a.offset, 1024);
        assert_eq!(b.text, "world\n");
        assert_eq!(b.offset, 12);
    }

    #[test]
    fn nothing_new_returns_empty() {
        let f = tmp_with(b"done\n");
        let a = read_delta(f.path(), 0, 1024);
        let b = read_delta(f.path(), a.offset, 1024);
        assert_eq!(b.text, "");
        assert_eq!(b.offset, a.offset);
    }

    #[test]
    fn truncation_resets_offset() {
        let f = tmp_with(b"aaaaaaaaaa\n");
        let a = read_delta(f.path(), 0, 1024);
        // Caller's offset is now past a shorter file → reset from 0.
        std::fs::write(f.path(), b"xy\n").unwrap();
        let b = read_delta(f.path(), a.offset, 1024);
        assert!(b.reset);
        assert_eq!(b.text, "xy\n");
    }

    #[test]
    fn far_behind_jumps_to_tail_window() {
        let f = tmp_with(b"0123456789ABCDEF"); // 16 bytes
        let c = read_delta(f.path(), 0, 8);
        assert!(c.reset);
        assert_eq!(c.text, "89ABCDEF");
        assert_eq!(c.offset, 16);
    }

    #[test]
    fn split_multibyte_char_waits() {
        // "あ" is 3 bytes (E3 81 82). Write only the first 2 bytes after an ASCII char.
        let mut bytes = b"x".to_vec();
        bytes.extend_from_slice(&[0xE3, 0x81]); // partial あ
        let f = tmp_with(&bytes);
        let a = read_delta(f.path(), 0, 1024);
        assert_eq!(a.text, "x"); // partial multibyte held back
        assert_eq!(a.offset, 1);
        // Complete the char.
        std::fs::OpenOptions::new()
            .append(true)
            .open(f.path())
            .unwrap()
            .write_all(&[0x82])
            .unwrap();
        let b = read_delta(f.path(), a.offset, 1024);
        assert_eq!(b.text, "あ");
        assert_eq!(b.offset, 4);
    }

    #[test]
    fn marked_path_survives_shell_startup_noise() {
        let noisy =
            "welcome banner\n__AGENTPIT_PATH__/Users/x/.local/bin:/usr/bin__AGENTPIT_PATH__";
        assert_eq!(
            extract_marked_path(noisy),
            Some("/Users/x/.local/bin:/usr/bin")
        );
    }

    #[test]
    fn marked_path_rejects_missing_or_empty() {
        assert_eq!(extract_marked_path("no markers here"), None);
        assert_eq!(
            extract_marked_path("__AGENTPIT_PATH____AGENTPIT_PATH__"),
            None
        );
    }
}
