//! Run-event emission.
//!
//! The schema, state-dir paths, and capture logic live in the `agentpit-events` crate so
//! the CLI and the desktop dashboard share one definition. This module re-exports them so
//! the rest of the CLI keeps referring to `crate::events::*`.

pub use agentpit_events::{
    backend_log_path, events_path, output_streamer, prune_run_outputs, runs_dir, state_dir, Event,
    LegStatus, RunKind, RunLogger,
};
