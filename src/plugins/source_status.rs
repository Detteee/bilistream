//! The one answer to "is this channel live, and what do we play".
//!
//! YouTube, Twitch and Niconico each declared their own byte-identical
//! six-element tuple for it, and the same tuple was re-spelled inline in two
//! more signatures plus a private alias in `runtime.rs`. Positional fields make
//! a mis-ordered answer a runtime bug instead of a compile error:
//! `confirmed_index_status` (`youtube.rs`) exists because a stale index
//! `video_id` once got labelled onto a freshly resolved URL.

use chrono::{DateTime, Local};

#[derive(Clone, Debug, Default, PartialEq)]
pub struct SourceStatus {
    pub is_live: bool,
    /// Danmaku/announcement topic when the source exposes one.
    pub topic: Option<String>,
    pub title: Option<String>,
    pub m3u8_url: Option<String>,
    pub start_time: Option<DateTime<Local>>,
    /// The id of the broadcast this answer belongs to — the field whose
    /// staleness caused the original mix-up.
    pub video_id: Option<String>,
}

impl SourceStatus {
    /// The "nothing is live and we know nothing" answer. It used to be a private
    /// `const OFFLINE_SOURCE_STATUS` in `runtime.rs`, spelled positionally.
    pub const OFFLINE: Self = Self {
        is_live: false,
        topic: None,
        title: None,
        m3u8_url: None,
        start_time: None,
        video_id: None,
    };
}
