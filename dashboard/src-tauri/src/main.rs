// Hide the console window on Windows release builds.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod state;

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

/// Read a backend leg's captured output for live tailing. Returns the tail (last
/// `MAX_OUTPUT_BYTES`) so a huge transcript can't blow up the webview.
#[tauri::command]
fn get_output(run_id: String, backend: String, aggregator: bool) -> String {
    use std::io::{Read, Seek, SeekFrom};
    let path = agentpit_events::backend_log_path(&run_id, &backend, aggregator);
    let mut file = match std::fs::File::open(&path) {
        Ok(f) => f,
        Err(_) => return String::new(),
    };
    let len = file.metadata().map(|m| m.len()).unwrap_or(0);
    if len > MAX_OUTPUT_BYTES {
        let _ = file.seek(SeekFrom::Start(len - MAX_OUTPUT_BYTES));
    }
    let mut bytes = Vec::new();
    if file.read_to_end(&mut bytes).is_err() {
        return String::new();
    }
    String::from_utf8_lossy(&bytes).into_owned()
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

fn main() {
    tauri::Builder::default()
        .manage(AppState {
            tracker: Mutex::new(Tracker::new()),
        })
        .invoke_handler(tauri::generate_handler![get_snapshot, get_output])
        .setup(|app| {
            spawn_watcher(app.handle().clone());
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running agentpit dashboard");
}
