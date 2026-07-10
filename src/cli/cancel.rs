//! Cancellation detection and typed navigation outcomes for the interactive menu.
//!
//! cliclack returns `io::ErrorKind::Interrupted` when the user presses Esc or
//! Ctrl-C (see cliclack-0.5.4 `src/prompt/interaction.rs:97`).  Any other
//! `io::Error` — notably `io::ErrorKind::NotConnected` when there is no TTY
//! (see `interaction.rs:67`) — is a genuine I/O failure and must propagate as
//! `Err` so callers can surface it to the user.
//!
//! The central type here is [`Nav`].  A menu step returns `Nav<T>` where `T`
//! is the value type produced on a successful selection.  Helper functions
//! convert a raw `io::Result<T>` into `anyhow::Result<Nav<T>>`.
//!
//! # Usage
//!
//! ```ignore
//! use crate::cli::cancel::{Nav, prompt};
//!
//! match prompt(cliclack::select("Pick one").item(...).interact())? {
//!     Nav::Back     => return Ok(Nav::Back),
//!     Nav::Value(v) => { /* use v */ }
//! }
//! ```

use std::io;

use anyhow::Result;
use console::style;

// ─── Nav ────────────────────────────────────────────────────────────────────

/// Typed outcome of a prompt interaction in the interactive menu.
///
/// * `Value(T)` — the user made a selection / entered input.
/// * `Back` — the user pressed Esc/Ctrl-C; the caller returns to its parent
///   menu without executing the action (and at the top-level main menu the
///   caller breaks the loop and exits cleanly with code 0).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Nav<T> {
    Value(T),
    Back,
}

// ─── Core conversion ────────────────────────────────────────────────────────

/// Convert a raw `io::Result<T>` from a cliclack prompt into `Result<Nav<T>>`.
///
/// * `Ok(v)` → `Ok(Nav::Value(v))`
/// * `Err(e)` where `e.kind() == Interrupted` → `Ok(Nav::Back)`
///   (at the top-level menu the caller treats this as a clean exit)
/// * Any other `Err` → `Err(…)` (genuine I/O failure)
///
/// This function does NOT swallow non-cancellation errors.  In particular,
/// `io::ErrorKind::NotConnected` (no TTY) is treated as a hard failure.
pub fn prompt<T>(result: io::Result<T>) -> Result<Nav<T>> {
    match result {
        Ok(v) => Ok(Nav::Value(v)),
        Err(e) if e.kind() == io::ErrorKind::Interrupted => Ok(Nav::Back),
        Err(e) => Err(anyhow::Error::new(e).context("prompt interaction failed")),
    }
}

// ─── Confirm-change helper ───────────────────────────────────────────────────

/// Print a uniform before→after confirmation line for a config mutation.
///
/// All three config flows (backend, route, ensemble) call this helper so the
/// output is byte-identical in style.
///
/// Example output:
/// ```text
///   set backend.codex.transport = exec  (was: (default))
/// ```
pub fn confirm_change(label: &str, was: &str, now: &str) {
    println!(
        "  set {} = {}  (was: {})",
        style(label).bold(),
        style(now).cyan().bold(),
        style(was).dim(),
    );
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;

    // ── prompt() ────────────────────────────────────────────────────────────

    #[test]
    fn interrupted_maps_to_back() {
        let err: io::Result<u32> = Err(io::Error::from(io::ErrorKind::Interrupted));
        let nav = prompt(err).expect("Interrupted should yield Ok(Nav::Back)");
        assert_eq!(nav, Nav::Back);
    }

    #[test]
    fn ok_maps_to_value() {
        let result: io::Result<u32> = Ok(42);
        let nav = prompt(result).expect("Ok should yield Ok(Nav::Value)");
        assert_eq!(nav, Nav::Value(42));
    }

    #[test]
    fn not_connected_propagates_as_err() {
        let err: io::Result<u32> = Err(io::Error::from(io::ErrorKind::NotConnected));
        let result = prompt(err);
        assert!(
            result.is_err(),
            "NotConnected must propagate as Err, not be swallowed"
        );
        // Confirm the underlying kind is preserved in the error chain.
        let anyhow_err = result.unwrap_err();
        let io_err = anyhow_err
            .downcast_ref::<io::Error>()
            .expect("root cause should be io::Error");
        assert_eq!(io_err.kind(), io::ErrorKind::NotConnected);
    }

    #[test]
    fn other_error_kind_propagates_as_err() {
        let err: io::Result<String> = Err(io::Error::from(io::ErrorKind::BrokenPipe));
        let result = prompt(err);
        assert!(
            result.is_err(),
            "BrokenPipe must propagate as Err, not be swallowed"
        );
    }

    #[test]
    fn permission_denied_propagates_as_err() {
        let err: io::Result<i32> = Err(io::Error::from(io::ErrorKind::PermissionDenied));
        let result = prompt(err);
        assert!(result.is_err(), "PermissionDenied must propagate as Err");
    }

    // ── Nav enum ────────────────────────────────────────────────────────────

    #[test]
    fn nav_variants_are_distinct() {
        assert_ne!(Nav::<u8>::Back, Nav::Value(0));
    }
}
