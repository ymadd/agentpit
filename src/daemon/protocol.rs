//! NDJSON wire protocol for daemon⇔client and worker⇔client conversations (design §5.1).
//!
//! The types live in [`agentpit_events::wire`], shared with the dashboard (which depends
//! only on `agentpit-events`) so both speak exactly the same frames. This module re-exports
//! them so `crate::daemon::protocol::...` paths keep working.

pub use agentpit_events::wire::*;
