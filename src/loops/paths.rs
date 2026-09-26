//! Where a loop's runner listens and registers, and where its per-step files live
//! (design §9.1).

use std::path::{Path, PathBuf};

use crate::daemon::paths::runtime_dir;
use crate::events::state_dir;

pub use agentpit_events::loops::{answer_rel, check_log_rel, output_log_rel, prompt_rel};

/// `head.json`: the loop's [`LoopSummary`](agentpit_events::loops::LoopSummary), rewritten
/// after every commit (never ahead of the journal).
pub const HEAD_FILE: &str = "head.json";
/// The runner's stderr, appended across restarts (the daemon redirects it here).
pub const RUNNER_LOG: &str = "runner.log";

/// `$XDG_RUNTIME_DIR/agentpit/loop-<32hex>.sock` (42-byte file name, inside the socket-path
/// budget of `daemon::paths`). `None` for an invalid loop id.
pub fn loop_socket_path(loop_id: &str) -> Option<PathBuf> {
    agentpit_events::loops::is_valid_loop_id(loop_id)
        .then(|| runtime_dir().join(format!("loop-{}.sock", &loop_id[3..])))
}

/// Durable runner records (`<loop_id>.json`), like `daemon/workers`.
pub fn runners_dir() -> PathBuf {
    state_dir().join("daemon").join("loops")
}

pub fn head_path(loop_dir: &Path) -> PathBuf {
    loop_dir.join(HEAD_FILE)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loop_sockets_fit_sun_path() {
        let sock = loop_socket_path("lp-0199a1b2c3d47e5f8a9b0c1d2e3f4a5b").unwrap();
        assert_eq!(sock.file_name().unwrap().len(), 42, "{}", sock.display());
        assert!(sock.as_os_str().len() <= 104, "{}", sock.display());
        assert!(loop_socket_path("../../etc").is_none());
    }
}
