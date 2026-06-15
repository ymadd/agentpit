//! Recursion-depth guard for the model-driven workflow.
//!
//! A workflow manager drives sub-tasks by shelling out to `agentpit` again. Nothing in the
//! model stops it from invoking `agentpit workflow` recursively, which could fan out without
//! bound. The guard is the authoritative, Rust-enforced ceiling: each manager leg inherits an
//! incremented [`ENV_DEPTH`] in its exec env, and every `agentpit` it spawns reads that depth
//! back via [`current_depth`]. When the depth reaches the configured maximum, the run bails
//! before launching the manager — regardless of whether the model cooperates.

use anyhow::{Result, bail};

/// Env var carrying the current workflow recursion depth into spawned `agentpit` processes.
pub const ENV_DEPTH: &str = "AGENTPIT_WORKFLOW_DEPTH";
/// Env var carrying the parent run id for correlation (Phase 3 will use it for nesting).
pub const ENV_PARENT_RUN_ID: &str = "AGENTPIT_PARENT_RUN_ID";
/// Env var carrying the path to the agentpit binary the manager should re-invoke.
pub const ENV_SELF: &str = "AGENTPIT_SELF";
/// Default recursion ceiling when none is configured.
pub const DEFAULT_MAX_DEPTH: u32 = 3;
/// Hard upper bound on any configured/CLI `max_depth`.
///
/// Phase 1 workflows fan out one manager leg per level, so even a small depth multiplies the
/// process tree quickly; nothing legitimate needs more than this. Clamping here keeps the
/// `depth + 1` arithmetic in [`clamp_max_depth`]'s callers far from `u32::MAX`, so a hostile
/// `--max-depth 4294967295` cannot wrap the inherited depth back to `0` and defeat the guard.
pub const MAX_DEPTH_CEILING: u32 = 32;

/// Clamp a requested recursion ceiling into `1..=MAX_DEPTH_CEILING`.
///
/// A `max` of `0` would reject every run (depth `0` is never `< 0`), so the floor is `1`; the
/// upper bound defends the depth arithmetic against overflow regardless of how `max` was supplied.
pub fn clamp_max_depth(max: u32) -> u32 {
    max.clamp(1, MAX_DEPTH_CEILING)
}

/// The workflow recursion depth of the current process.
///
/// Reads [`ENV_DEPTH`] and parses it as a `u32`, defaulting to `0` when it is unset or holds a
/// non-numeric value. Tampering with the var (e.g. setting it to a non-number) therefore reads
/// as depth `0` rather than disabling the guard with a parse error.
pub fn current_depth() -> u32 {
    std::env::var(ENV_DEPTH)
        .ok()
        .and_then(|s| s.trim().parse::<u32>().ok())
        .unwrap_or(0)
}

/// Ensure the current depth is below `max`, returning it on success.
///
/// Bails with a clear message when the ceiling has been reached, so a manager that recursively
/// invoked `agentpit workflow` is stopped before launching another nested manager.
pub fn check_not_exceeded(max: u32) -> Result<u32> {
    let current = current_depth();
    if current < max {
        Ok(current)
    } else {
        bail!(
            "workflow recursion depth {current} reached the ceiling of {max}; aborting to prevent runaway fan-out"
        )
    }
}

/// The env pairs to inject into the manager's [`ExecSpec`](crate::exec::ExecSpec) env so every
/// `agentpit` it spawns inherits the incremented depth, the parent run id, and the self path.
pub fn child_env(new_depth: u32, parent_run_id: &str, self_path: &str) -> Vec<(String, String)> {
    vec![
        (ENV_DEPTH.to_string(), new_depth.to_string()),
        (ENV_PARENT_RUN_ID.to_string(), parent_run_id.to_string()),
        (ENV_SELF.to_string(), self_path.to_string()),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // Env vars are process-global; tests that mutate ENV_DEPTH must not run concurrently.
    static ENV_LOCK: Mutex<()> = Mutex::new(());
    fn lock_env() -> std::sync::MutexGuard<'static, ()> {
        ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn depth_defaults_to_zero_when_unset() {
        let _env = lock_env();
        // SAFETY: single-threaded under ENV_LOCK; only this process's env is touched.
        unsafe {
            std::env::remove_var(ENV_DEPTH);
        }
        assert_eq!(current_depth(), 0);
    }

    #[test]
    fn depth_reads_from_env() {
        let _env = lock_env();
        unsafe {
            std::env::set_var(ENV_DEPTH, "2");
        }
        assert_eq!(current_depth(), 2);
        unsafe {
            std::env::remove_var(ENV_DEPTH);
        }
    }

    #[test]
    fn depth_rejects_non_numeric_tampering() {
        let _env = lock_env();
        unsafe {
            std::env::set_var(ENV_DEPTH, "not-a-number");
        }
        assert_eq!(current_depth(), 0);
        unsafe {
            std::env::remove_var(ENV_DEPTH);
        }
    }

    #[test]
    fn check_allows_below_ceiling() {
        let _env = lock_env();
        unsafe {
            std::env::set_var(ENV_DEPTH, "1");
        }
        assert_eq!(check_not_exceeded(3).unwrap(), 1);
        unsafe {
            std::env::remove_var(ENV_DEPTH);
        }
    }

    #[test]
    fn check_bails_at_ceiling() {
        let _env = lock_env();
        unsafe {
            std::env::set_var(ENV_DEPTH, "3");
        }
        let err = check_not_exceeded(3).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("ceiling of 3"), "got: {msg}");
        unsafe {
            std::env::remove_var(ENV_DEPTH);
        }
    }

    #[test]
    fn clamp_max_depth_enforces_floor_and_ceiling() {
        assert_eq!(clamp_max_depth(0), 1);
        assert_eq!(clamp_max_depth(3), 3);
        assert_eq!(clamp_max_depth(MAX_DEPTH_CEILING), MAX_DEPTH_CEILING);
        assert_eq!(clamp_max_depth(u32::MAX), MAX_DEPTH_CEILING);
    }

    #[test]
    fn child_env_returns_three_pairs_with_incremented_depth() {
        let pairs = child_env(2, "run-7", "/usr/local/bin/agentpit");
        assert_eq!(pairs.len(), 3);
        assert!(pairs.contains(&(ENV_DEPTH.to_string(), "2".to_string())));
        assert!(pairs.contains(&(ENV_PARENT_RUN_ID.to_string(), "run-7".to_string())));
        assert!(pairs.contains(&(ENV_SELF.to_string(), "/usr/local/bin/agentpit".to_string())));
    }
}
