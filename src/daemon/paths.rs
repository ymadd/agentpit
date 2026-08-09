//! Socket and registry paths for the daemon layer (design §5.1).
//!
//! Sockets live under the per-user runtime dir (`$XDG_RUNTIME_DIR/agentpit`, falling back
//! to `/tmp/agentpit-<uid>` with mode 0700) — one daemon per OS user, all projects shared.
//! Durable worker records live under the STATE dir (they must survive a reboot's tmpfs
//! wipe only as garbage to clean, so runtime-dir loss is harmless).

use std::path::PathBuf;

use crate::events::state_dir;

/// The longest socket filename this layer creates: `worker-<uuid36>.sock` = 48 bytes.
/// A unix socket PATH must fit in `sockaddr_un.sun_path` (104 bytes on macOS, 108 on
/// Linux), so the runtime dir itself must stay comfortably short.
const MAX_SOCKET_DIR_BYTES: usize = 100 - 48;

/// The per-user runtime directory holding every agentpit socket. Created 0700 on unix.
/// An `$XDG_RUNTIME_DIR` too long for `sun_path` (SUN_LEN) silently falls back to the
/// short `/tmp/agentpit-<uid>` — a long runtime dir would make every bind fail.
pub fn runtime_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("XDG_RUNTIME_DIR")
        && !dir.is_empty()
    {
        let candidate = PathBuf::from(&dir).join("agentpit");
        if candidate.as_os_str().len() <= MAX_SOCKET_DIR_BYTES {
            return candidate;
        }
    }
    // /tmp fallback: suffix with the uid so users never collide.
    #[cfg(unix)]
    let suffix = unsafe { libc_uid() }.to_string();
    #[cfg(not(unix))]
    let suffix = "user".to_string();
    PathBuf::from("/tmp").join(format!("agentpit-{suffix}"))
}

#[cfg(unix)]
unsafe fn libc_uid() -> u32 {
    // `std::os::unix` has no uid accessor; read it via the effective uid syscall wrapper
    // that ships in std's platform support: fall back to parsing `id -u` only if the
    // (always-present) geteuid is somehow unavailable is unnecessary — geteuid never fails.
    unsafe extern "C" {
        fn geteuid() -> u32;
    }
    unsafe { geteuid() }
}

/// Create the runtime dir with owner-only permissions and return it.
///
/// The `/tmp/agentpit-<uid>` fallback path is predictable, so on unix the directory must
/// actually BE ours and private before any socket lives in it: a directory another local
/// user pre-created there would let them unlink or replace our sockets. `$XDG_RUNTIME_DIR`
/// gets the same check — it is cheap, and a wrong owner there is just as fatal.
pub fn ensure_runtime_dir() -> std::io::Result<PathBuf> {
    let dir = runtime_dir();
    std::fs::create_dir_all(&dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let meta = std::fs::metadata(&dir)?;
        let uid = unsafe { libc_uid() };
        if meta.uid() != uid {
            return Err(std::io::Error::other(format!(
                "runtime dir {} is owned by uid {}, not us (uid {uid}) — refusing to \
                 place sockets in a directory another user controls",
                dir.display(),
                meta.uid(),
            )));
        }
        if meta.permissions().mode() & 0o077 != 0 {
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
        }
    }
    Ok(dir)
}

/// The daemon's own listening socket.
pub fn daemon_socket_path() -> PathBuf {
    runtime_dir().join("daemon.sock")
}

/// A worker's listening socket, keyed by session id (a uuid — safe as a path component).
pub fn worker_socket_path(session_id: &str) -> PathBuf {
    runtime_dir().join(format!("worker-{session_id}.sock"))
}

/// Durable worker descriptors (`<session_id>.json`) the daemon uses to find or reap
/// workers across its own restarts (design §5.4-§5.5).
pub fn workers_dir() -> PathBuf {
    state_dir().join("daemon").join("workers")
}

/// The daemon's single-instance owner record (pid + start id).
pub fn daemon_owner_path() -> PathBuf {
    state_dir().join("daemon").join("owner.json")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn socket_paths_live_under_the_runtime_dir() {
        let root = runtime_dir();
        assert!(daemon_socket_path().starts_with(&root));
        assert!(worker_socket_path("0198-abc").starts_with(&root));
        assert!(
            worker_socket_path("0198-abc")
                .file_name()
                .unwrap()
                .to_string_lossy()
                .contains("0198-abc")
        );
    }

    #[test]
    fn durable_records_live_under_the_state_dir() {
        assert!(workers_dir().starts_with(state_dir()));
        assert!(daemon_owner_path().starts_with(state_dir()));
    }

    #[test]
    fn worker_socket_paths_always_fit_sun_path() {
        // Regression (2026-08-08): a deep $XDG_RUNTIME_DIR made every bind fail with
        // "path must be shorter than SUN_LEN". The dir must fall back to /tmp instead.
        let sock = worker_socket_path("0198f3f2-7c1a-7000-8000-3f2a9b1c4d5e");
        assert!(
            sock.as_os_str().len() <= 104,
            "socket path too long for sun_path: {} bytes ({})",
            sock.as_os_str().len(),
            sock.display()
        );
    }
}
