//! Run-event emission.
//!
//! The schema, state-dir paths, and capture logic live in the `agentpit-events` crate so
//! the CLI and the desktop dashboard share one definition. This module re-exports them so
//! the rest of the CLI keeps referring to `crate::events::*`.

pub use agentpit_events::{
    Event, LegStatus, RunKind, RunLogger, ask_request_path, ask_response_path, asks_dir,
    backend_log_path, events_path, is_safe_log_component, next_ask_token, now_ms, output_streamer,
    prune_run_outputs, runs_dir, state_dir,
};
