//! The daemon layer (design §5): a per-user control-plane broker (`server`), per-session
//! worker processes (`worker`), the NDJSON wire (`protocol`), durable worker records
//! (`registry`), socket paths (`paths`), and the client side (`client`).

pub mod client;
pub mod paths;
pub mod protocol;
pub mod registry;
pub mod server;
pub mod worker;
