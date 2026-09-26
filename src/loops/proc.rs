//! Process-group plumbing for the loop runner (design §9.4).
//!
//! The runner leads its own process group, so every agent it dispatches (and their
//! children) share the runner's pgid; each check runs in a group of its own so a timeout
//! can take down the whole command tree. After a crash the next runner kills what the dead
//! one left behind by group — never by bare pid, and never a group whose leader has been
//! replaced by an unrelated process (pid reuse).

use std::time::Duration;

use agentpit_events::session_lease::{pid_alive, process_start_id};

#[cfg(unix)]
unsafe extern "C" {
    fn kill(pid: i32, sig: i32) -> i32;
    fn setpgid(pid: i32, pgid: i32) -> i32;
}

const SIGTERM: i32 = 15;
const SIGKILL: i32 = 9;

/// Make this process the leader of its own process group (a no-op when the daemon
/// already spawned it that way).
pub fn lead_own_group() {
    #[cfg(unix)]
    unsafe {
        setpgid(0, 0);
    }
}

/// Whether any process is still in group `pgid`.
pub fn group_alive(pgid: u32) -> bool {
    #[cfg(unix)]
    {
        pgid > 1 && i32::try_from(pgid).is_ok_and(|p| unsafe { kill(-p, 0) } == 0)
    }
    #[cfg(not(unix))]
    {
        let _ = pgid;
        false
    }
}

/// Signal every process in group `pgid`. Returns whether the group existed.
pub fn signal_group(pgid: u32, sig: i32) -> bool {
    #[cfg(unix)]
    {
        pgid > 1 && i32::try_from(pgid).is_ok_and(|p| unsafe { kill(-p, sig) } == 0)
    }
    #[cfg(not(unix))]
    {
        let _ = (pgid, sig);
        false
    }
}

/// SIGTERM the group, give it `grace` to exit, then SIGKILL whatever is left.
pub async fn terminate_group(pgid: u32, grace: Duration) {
    if !signal_group(pgid, SIGTERM) {
        return;
    }
    let deadline = tokio::time::Instant::now() + grace;
    while tokio::time::Instant::now() < deadline {
        if !group_alive(pgid) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    signal_group(pgid, SIGKILL);
}

/// Whether the group led by `pid` may be killed as the leftovers of the process recorded
/// as (`pid`, `start_id`).
///
/// - Our own group: never.
/// - The leader is alive: only when it is provably the recorded incarnation (a different
///   start id means the pid was reused, and the old group is necessarily empty then —
///   the kernel does not hand out a pid that is still some group's id).
/// - The leader is gone: the group, if it still exists, can only be the recorded
///   process's orphaned children, for the same reason.
pub fn may_kill_orphan_group(pid: u32, start_id: &str) -> bool {
    if pid <= 1 || pid == std::process::id() {
        return false;
    }
    if pid_alive(pid) {
        let current = process_start_id(pid);
        return !start_id.is_empty() && !current.is_empty() && current == start_id;
    }
    true
}

/// Kill what a dead runner (or one of its checks) left running, when that is provably
/// safe. Returns whether a group was signalled.
pub async fn reap_orphan_group(pid: u32, start_id: &str) -> bool {
    if !may_kill_orphan_group(pid, start_id) || !group_alive(pid) {
        return false;
    }
    terminate_group(pid, Duration::from_secs(2)).await;
    true
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn our_own_group_and_low_pids_are_never_killable() {
        assert!(!may_kill_orphan_group(std::process::id(), ""));
        assert!(!may_kill_orphan_group(0, ""));
        assert!(!may_kill_orphan_group(1, ""));
    }

    #[tokio::test]
    async fn a_group_is_terminated_with_its_children() {
        let mut cmd = std::process::Command::new("sh");
        cmd.args(["-c", "sleep 30 & sleep 30"]);
        std::os::unix::process::CommandExt::process_group(&mut cmd, 0);
        let mut child = cmd.spawn().unwrap();
        let pgid = child.id();
        let start_id = process_start_id(pgid);
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(group_alive(pgid));
        // A live leader with a different start id is someone else's process.
        assert!(!may_kill_orphan_group(pgid, "not-the-recorded-one"));
        assert!(may_kill_orphan_group(pgid, &start_id));
        assert!(reap_orphan_group(pgid, &start_id).await);
        let _ = child.wait();
        for _ in 0..40 {
            if !group_alive(pgid) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("the background sleep survived the group kill");
    }
}
