//! Shared fixtures for the crate's tests.
//!
//! `Config` deliberately has no `Default` impl: a zeroed interval or an empty
//! channel id would silently change what a test exercises. That means every
//! test module that needs a config has to spell one out — and before this
//! module existed they did, four slightly different times, drifting apart
//! (two of them differed by a single field). Keep the one full literal here and
//! let tests override the fields they actually care about.

use crate::config::{
    BiliLive, ClusterConfig, ClusterHealthThresholds, Config, Credentials, FfmpegCache,
    PriorityChannel, Twitch, Youtube,
};
use std::collections::HashMap;

/// A complete config for tests: cluster enabled under `node_id`, one YouTube
/// and one Twitch channel, no credentials, no proxies.
///
/// This is the only full `Config` literal in the tree. Adding a field to
/// `Config` breaks compilation here, once — which is the point.
pub(crate) fn config(node_id: &str, priority: i32) -> Config {
    Config {
        snapshot: None,
        auto_cover: false,
        enable_anti_collision: false,
        interval: 60,
        bililive: BiliLive {
            enable_danmaku_command: false,
            room: 1,
            bili_rtmp_url: String::new(),
            bili_rtmp_key: String::new(),
            credentials: Credentials::default(),
        },
        twitch: Twitch {
            enable_monitor: true,
            channel_name: "tw".to_string(),
            area_v2: 235,
            channel_id: "twid".to_string(),
            proxy_region: String::new(),
            quality: "best".to_string(),
            proxy: None,
            crop: None,
            ffmpeg_cache: FfmpegCache::default(),
        },
        youtube: Youtube {
            enable_monitor: true,
            channel_name: "yt".to_string(),
            channel_id: "ytid".to_string(),
            area_v2: 235,
            quality: "best".to_string(),
            cookies_file: None,
            cookies_from_browser: None,
            proxy: None,
            deno_path: None,
            crop: None,
            ffmpeg_cache: FfmpegCache::default(),
        },
        holodex_api_key: None,
        holodex_jwt: None,
        holodex_jwt_refreshed_at: None,
        holodex_username: None,
        holodex_skip_jwt_verify: false,
        holodex_monitor_gate: true,
        youtube_api_key: None,
        youtube_rss_enabled: true,
        show_priority_channel: false,
        show_twitch: true,
        show_niconico: false,
        youtube_websub_callback_url: None,
        youtube_websub_port: crate::config::default_websub_port(),
        riot_api_key: None,
        enable_lol_monitor: false,
        lol_monitor_interval: None,
        anti_collision_list: HashMap::new(),
        priority_channel: PriorityChannel::default(),
        enable_youtube_monitor: true,
        enable_twitch_monitor: true,
        niconico: crate::config::Niconico::default(),
        cluster: ClusterConfig {
            enabled: true,
            node_id: node_id.to_string(),
            node_name: node_id.to_string(),
            public_api_url: format!("http://{}", node_id),
            peers: Vec::new(),
            priority,
            heartbeat_interval_secs: 5,
            failover_timeout_secs: 15,
            lease_ttl_secs: 20,
            sync_monitored_channels: true,
            auto_failover: true,
            thresholds: ClusterHealthThresholds::default(),
            public_status: crate::config::PublicStatusConfig::default(),
        },
        history: Default::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The fixture must stay a *realistic* config, not just a compiling one:
    /// it round-trips through the JSON shape the WebUI and cluster payloads
    /// use, and the public-status block passes its own validation. Adding a
    /// field that serializes asymmetrically fails here instead of surprising
    /// an unrelated test.
    #[test]
    fn config_fixture_is_a_valid_config() {
        let config = config("guard", 0);
        let json = serde_json::to_value(&config).expect("the fixture must serialize");
        let reparsed: Config =
            serde_json::from_value(json.clone()).expect("the fixture must deserialize");
        assert_eq!(
            json,
            serde_json::to_value(&reparsed).unwrap(),
            "the fixture must survive a JSON round trip"
        );
        config
            .cluster
            .public_status
            .validate()
            .expect("the fixture's public-status block must be valid");
        assert!(
            !config.youtube.channel_id.is_empty() && !config.twitch.channel_id.is_empty(),
            "the fixture's monitored channels must have ids; tests rely on them"
        );
    }
}
