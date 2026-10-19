//! The provider-agnostic stream item.
//!
//! One [`StreamItem`] is one 直播/预告 entry as a list serves it: the shape the
//! Holodex backstop deserializes, that YouTube discovery, the Twitch GQL overlay
//! and the Niconico listing overlay build, and that every projection
//! (`StreamListFacts`, the admin/public DTOs) maps from.
//!
//! Provider wire values keep their original names (`stream_type`, `topic_id`,
//! `link`, `channel`): they are the item's wire vocabulary, not a second model.

use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct StreamItem {
    pub id: String,
    pub title: String,
    #[serde(rename = "type")]
    pub stream_type: String,
    pub topic_id: Option<String>,
    pub published_at: Option<String>,
    pub available_at: Option<String>,
    pub status: String,
    pub start_scheduled: Option<String>,
    pub start_actual: Option<String>,
    pub live_viewers: Option<i32>,
    #[serde(default)]
    pub channel: StreamChannel,
    #[serde(default)]
    pub link: Option<String>,
    #[serde(default)]
    pub thumbnail: Option<String>,
    #[serde(default, rename = "placeholderType")]
    pub placeholder_type: Option<String>,
    /// Set when YouTube `videos.list` classified this item, so its status is
    /// trusted over the backstop's stale-schedule heuristics.
    #[serde(skip)]
    pub yt_confirmed: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct StreamChannel {
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub photo: Option<String>,
}
