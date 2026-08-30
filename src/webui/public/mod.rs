//! The public read-only status page: its payload, its snapshot cache, and the
//! listener that serves it.
//!
//! Runs on one node picked for bandwidth (see `cluster.public_status`), which
//! is usually a standby, so the streaming node's state is read out of the
//! cluster snapshots that heartbeats already carry.

pub mod holodex_cache;
pub mod payload;
pub mod server;
pub mod snapshot;
pub mod streams;
pub mod thumbnails;

pub use payload::PublicStatus;
pub use server::start_public_status_supervisor;
pub use snapshot::{current_public_status, refresh_public_status};
pub use streams::{current_public_streams, refresh_public_streams};
