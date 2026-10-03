//! The public read-only status page: its payload, its snapshot cache, and the
//! listener that serves it.
//!
//! Runs on one node picked for bandwidth (see `cluster.public_status`), which
//! is usually a standby, so the streaming node's state is read out of the
//! cluster snapshots that heartbeats already carry.

pub mod payload;
pub mod server;
pub mod snapshot;
pub(crate) mod stream_list;
pub mod thumbnails;

pub use server::start_public_status_supervisor;
pub use stream_list::remap_after_areas_change;

/// Content-derived validators survive process restarts. Weak validators also
/// remain valid when the compression layer changes the wire representation.
fn body_etag(body: &[u8]) -> String {
    use sha2::Digest;
    format!("W/\"{:x}\"", sha2::Sha256::digest(body))
}
