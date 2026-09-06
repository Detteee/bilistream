//! The payload the public status page reads.
//!
//! Deliberately a separate set of types from [`StatusData`] rather than a
//! filtered copy of it: a field added to the admin status must not appear here
//! by default. Everything public is listed by hand below, and
//! `serialized_keys_are_the_allowlist` fails if that stops being true.
//!
//! Excluded on purpose: room and RTMP details, credentials, channel ids, file
//! paths, peer API urls, heartbeat timestamps, and the per-node WebUI link.

use serde::{Deserialize, Serialize};

use crate::cluster::{ClusterNodeRole, ClusterNodeSnapshot, ClusterStatus, ClusterStreamIdentity};
use crate::webui::state::{BiliStatus, NetworkStatus, NicoStatus, StatusData, TwStatus, YtStatus};

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
pub struct PublicStatus {
    /// False when the serving node has no fresh view of the streaming node, so
    /// the page can say so instead of drawing stale numbers as live.
    pub in_sync: bool,
    pub bilibili: PublicBiliStatus,
    pub youtube: Option<PublicPlatformStatus>,
    pub twitch: Option<PublicPlatformStatus>,
    pub niconico: Option<PublicNiconicoStatus>,
    pub nodes: Vec<PublicNode>,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
pub struct PublicBiliStatus {
    pub is_live: bool,
    pub title: String,
    pub area_id: u64,
    pub area_name: String,
    /// Drives the 切换 buttons: with danmaku commands off, nothing is
    /// requestable and every button greys out.
    pub enable_danmaku_command: bool,
}

/// YouTube and Twitch differ only in what the second line is called, so they
/// share a shape. `topic` carries the YouTube topic or the Twitch game.
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
pub struct PublicPlatformStatus {
    pub is_live: bool,
    pub enable_monitor: bool,
    pub channel_name: String,
    pub title: Option<String>,
    pub topic: Option<String>,
    pub area_id: u64,
    pub area_name: String,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
pub struct PublicNiconicoStatus {
    pub is_live: bool,
    pub enable_monitor: bool,
    pub channel_name: String,
    pub title: Option<String>,
    pub live_id: Option<String>,
    pub scheduled_start: Option<String>,
    pub area_id: u64,
    pub area_name: String,
}

/// A node as viewers see it: is it up, is it streaming, and how is its
/// throughput. No heartbeat ages and no WebUI link.
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
pub struct PublicNode {
    pub name: String,
    pub role: ClusterNodeRole,
    pub healthy: bool,
    pub ffmpeg_running: bool,
    /// Who is on air, without channel or stream ids.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream: Option<PublicNodeStream>,
    pub network: Option<PublicNetwork>,
}

/// The inset on the featured node card. Platform is the same `YT` / `TW` /
/// `NC` code the cluster already uses.
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
pub struct PublicNodeStream {
    pub platform: String,
    pub channel_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}

/// The 60s RX/TX window sampled on the streaming node. Public still polls
/// every 10s; this is the last heartbeat's history, not a live 1 Hz feed.
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
pub struct PublicNetwork {
    pub stream_bitrate_kbps: Option<f32>,
    pub stream_cache_bitrate_kbps: Option<f32>,
    pub stream_speed: Option<f32>,
    pub stream_cache_speed: Option<f32>,
    pub stream_fps: Option<f32>,
    pub stream_frame: Option<u64>,
    pub stream_time_secs: Option<u32>,
    pub stream_cache_time_secs: Option<u32>,
    pub hls_cache_active: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stream_bitrate_history: Vec<f32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stream_cache_bitrate_history: Vec<f32>,
}

impl From<&BiliStatus> for PublicBiliStatus {
    fn from(bili: &BiliStatus) -> Self {
        Self {
            is_live: bili.is_live,
            title: bili.title.clone(),
            area_id: bili.area_id,
            area_name: bili.area_name.clone(),
            enable_danmaku_command: bili.enable_danmaku_command,
        }
    }
}

impl From<&YtStatus> for PublicPlatformStatus {
    fn from(yt: &YtStatus) -> Self {
        Self {
            is_live: yt.is_live,
            enable_monitor: yt.enable_monitor,
            channel_name: yt.channel_name.clone(),
            title: yt.title.clone(),
            topic: yt.topic.clone(),
            area_id: yt.area_id,
            area_name: yt.area_name.clone(),
        }
    }
}

impl From<&TwStatus> for PublicPlatformStatus {
    fn from(tw: &TwStatus) -> Self {
        Self {
            is_live: tw.is_live,
            enable_monitor: tw.enable_monitor,
            channel_name: tw.channel_name.clone(),
            title: tw.title.clone(),
            topic: tw.game.clone(),
            area_id: tw.area_id,
            area_name: tw.area_name.clone(),
        }
    }
}

impl From<&NicoStatus> for PublicNiconicoStatus {
    fn from(nc: &NicoStatus) -> Self {
        Self {
            is_live: nc.is_live,
            enable_monitor: nc.enable_monitor,
            channel_name: nc.channel_name.clone(),
            title: nc.title.clone(),
            live_id: nc.live_id.clone(),
            scheduled_start: nc.scheduled_start.clone(),
            area_id: nc.area_id,
            area_name: nc.area_name.clone(),
        }
    }
}

impl From<&NetworkStatus> for PublicNetwork {
    fn from(network: &NetworkStatus) -> Self {
        Self {
            stream_bitrate_kbps: network.stream_bitrate_kbps,
            stream_cache_bitrate_kbps: network.stream_cache_bitrate_kbps,
            stream_speed: network.stream_speed,
            stream_cache_speed: network.stream_cache_speed,
            stream_fps: network.stream_fps,
            stream_frame: network.stream_frame,
            stream_time_secs: network.stream_time_secs,
            stream_cache_time_secs: network.stream_cache_time_secs,
            hls_cache_active: network.hls_cache_active,
            stream_bitrate_history: network.stream_bitrate_history.clone(),
            stream_cache_bitrate_history: network.stream_cache_bitrate_history.clone(),
        }
    }
}

impl From<&ClusterStreamIdentity> for PublicNodeStream {
    fn from(stream: &ClusterStreamIdentity) -> Self {
        Self {
            platform: stream.platform.clone(),
            channel_name: stream.channel_name.clone(),
            title: stream
                .title
                .as_deref()
                .map(str::trim)
                .filter(|title| !title.is_empty())
                .map(str::to_string),
        }
    }
}

impl From<&ClusterNodeSnapshot> for PublicNode {
    fn from(node: &ClusterNodeSnapshot) -> Self {
        Self {
            // Fall back to the id only when a node was never given a name.
            name: if node.name.trim().is_empty() {
                node.node_id.clone()
            } else {
                node.name.clone()
            },
            role: node.role,
            healthy: node.health.healthy,
            ffmpeg_running: node.ffmpeg_running,
            stream: node.active_stream.as_ref().map(PublicNodeStream::from),
            network: node.network.as_ref().map(PublicNetwork::from),
        }
    }
}

impl PublicStatus {
    /// Builds the page payload from the streaming node's status and the
    /// cluster view the serving node holds.
    ///
    /// `status` is `None` when the serving node has no usable view of whoever
    /// owns the stream; the cards then render empty rather than stale.
    pub(super) fn build(status: Option<&StatusData>, cluster: &ClusterStatus) -> Self {
        let nodes = cluster.nodes.iter().map(PublicNode::from).collect();

        let Some(status) = status else {
            return Self {
                in_sync: false,
                bilibili: PublicBiliStatus::from(&BiliStatus::default()),
                youtube: None,
                twitch: None,
                niconico: None,
                nodes,
            };
        };

        Self {
            in_sync: true,
            bilibili: PublicBiliStatus::from(&status.bilibili),
            youtube: status.youtube.as_ref().map(PublicPlatformStatus::from),
            twitch: status.twitch.as_ref().map(PublicPlatformStatus::from),
            niconico: status.niconico.as_ref().map(PublicNiconicoStatus::from),
            nodes,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cluster::{ClusterHealth, ClusterStreamIdentity};
    use serde_json::Value;

    /// serde_json orders object keys itself, so compare the set.
    fn keys(value: &Value) -> Vec<String> {
        let mut keys: Vec<String> = value.as_object().expect("object").keys().cloned().collect();
        keys.sort();
        keys
    }

    fn sorted(names: &[&str]) -> Vec<String> {
        let mut names: Vec<String> = names.iter().map(|name| name.to_string()).collect();
        names.sort();
        names
    }

    fn sample_node() -> ClusterNodeSnapshot {
        ClusterNodeSnapshot {
            self_check: None,
            node_id: "ny".to_string(),
            name: "New York".to_string(),
            api_url: "http://ny.internal:3150".to_string(),
            priority: 5,
            last_seen: Some(1_700_000_000),
            is_local: true,
            role: ClusterNodeRole::Standby,
            health: ClusterHealth::healthy(),
            draining: false,
            network_unstable: false,
            ffmpeg_running: false,
            active_stream: None,
            status: None,
            network: Some(NetworkStatus {
                stream_bitrate_kbps: Some(4200.0),
                stream_bitrate_history: vec![1.0, 2.0, 3.0],
                ..NetworkStatus::default()
            }),
            config_version: "abc123".to_string(),
            failed_restarts: 2,
            monitor_toggles: Default::default(),
            channel_targets: Default::default(),
        }
    }

    fn sample_cluster() -> ClusterStatus {
        ClusterStatus {
            enabled: true,
            local_node_id: "ny".to_string(),
            active_owner: Some("jp".to_string()),
            lease_until: Some(1_700_000_020),
            config_version: "abc123".to_string(),
            auto_failover: true,
            public_status: Default::default(),
            nodes: vec![sample_node()],
        }
    }

    fn sample_status() -> StatusData {
        StatusData {
            bilibili: BiliStatus {
                is_live: true,
                title: "【转播】Kamito".to_string(),
                area_id: 86,
                area_name: "英雄联盟".to_string(),
                enable_danmaku_command: true,
                ..BiliStatus::default()
            },
            youtube: Some(YtStatus {
                is_live: true,
                enable_monitor: true,
                title: Some("YT title".to_string()),
                topic: Some("Gaming".to_string()),
                channel_name: "Kamito".to_string(),
                channel_id: "UCgYCMluaLpERsyNXlPOvBtA".to_string(),
                quality: "best".to_string(),
                area_id: 86,
                area_name: "英雄联盟".to_string(),
                crop_enabled: true,
                ffmpeg_cache_enabled: true,
                ffmpeg_cache_latency_secs: 6,
            }),
            twitch: None,
            niconico: None,
            priority_channel: None,
        }
    }

    /// The whole point of the separate types: adding a field to the admin
    /// status must not leak it here. Update this list consciously.
    #[test]
    fn serialized_keys_are_the_allowlist() {
        let payload = PublicStatus::build(Some(&sample_status()), &sample_cluster());
        let value = serde_json::to_value(&payload).unwrap();

        assert_eq!(
            keys(&value),
            sorted(&["in_sync", "bilibili", "youtube", "twitch", "niconico", "nodes"])
        );
        assert_eq!(
            keys(&value["bilibili"]),
            sorted(&[
                "is_live",
                "title",
                "area_id",
                "area_name",
                "enable_danmaku_command"
            ])
        );
        assert_eq!(
            keys(&value["youtube"]),
            sorted(&[
                "is_live",
                "enable_monitor",
                "channel_name",
                "title",
                "topic",
                "area_id",
                "area_name"
            ])
        );
        assert_eq!(
            keys(&value["nodes"][0]),
            sorted(&["name", "role", "healthy", "ffmpeg_running", "network"])
        );
        assert_eq!(
            keys(&value["nodes"][0]["network"]),
            sorted(&[
                "stream_bitrate_kbps",
                "stream_cache_bitrate_kbps",
                "stream_speed",
                "stream_cache_speed",
                "stream_fps",
                "stream_frame",
                "stream_time_secs",
                "stream_cache_time_secs",
                "hls_cache_active",
                "stream_bitrate_history"
            ])
        );
    }

    #[test]
    fn payload_never_carries_operator_detail() {
        let payload = PublicStatus::build(Some(&sample_status()), &sample_cluster());
        let json = serde_json::to_string(&payload).unwrap();

        for leaked in [
            "api_url",
            "last_seen",
            "node_id",
            "channel_id",
            "config_version",
            "failed_restarts",
            "priority",
            "quality",
            "crop_enabled",
            "ffmpeg_cache",
            "lease_until",
            "active_owner",
            "monitor_toggles",
            "is_local",
            "stream_id",
            "active_stream",
        ] {
            assert!(!json.contains(leaked), "public payload leaked {leaked}");
        }

        // Values, not just field names.
        assert!(!json.contains("ny.internal"));
        assert!(!json.contains("UCgYCMluaLpERsyNXlPOvBtA"));
        assert!(!json.contains("abc123"));
    }

    #[test]
    fn node_network_keeps_the_sampled_history() {
        let payload = PublicStatus::build(Some(&sample_status()), &sample_cluster());
        let network = payload.nodes[0].network.as_ref().expect("network");
        assert_eq!(network.stream_bitrate_history, vec![1.0, 2.0, 3.0]);
        assert!(network.stream_cache_bitrate_history.is_empty());

        let json = serde_json::to_string(&payload).unwrap();
        assert!(json.contains("stream_bitrate_history"));
        assert!(!json.contains("stream_cache_bitrate_history"));
    }

    #[test]
    fn twitch_game_lands_in_the_shared_topic_field() {
        let mut status = sample_status();
        status.twitch = Some(TwStatus {
            is_live: true,
            enable_monitor: true,
            title: Some("TW title".to_string()),
            game: Some("VALORANT".to_string()),
            channel_name: "kamito_jp".to_string(),
            channel_id: "twid".to_string(),
            quality: "best".to_string(),
            area_id: 329,
            area_name: "无畏契约".to_string(),
            crop_enabled: false,
            ffmpeg_cache_enabled: false,
            ffmpeg_cache_latency_secs: 0,
        });

        let payload = PublicStatus::build(Some(&status), &sample_cluster());
        let twitch = payload.twitch.expect("twitch card");
        assert_eq!(twitch.topic.as_deref(), Some("VALORANT"));
        assert_eq!(twitch.channel_name, "kamito_jp");
    }

    #[test]
    fn node_stream_drops_channel_and_stream_ids() {
        let mut cluster = sample_cluster();
        cluster.nodes[0].active_stream = Some(ClusterStreamIdentity {
            platform: "YT".to_string(),
            channel_name: "空澄セナ".to_string(),
            channel_id: "UCleakChannelId".to_string(),
            stream_id: Some("abc123video".to_string()),
            title: Some("  morning live  ".to_string()),
        });

        let payload = PublicStatus::build(Some(&sample_status()), &cluster);
        let stream = payload.nodes[0].stream.as_ref().expect("stream inset");
        assert_eq!(stream.platform, "YT");
        assert_eq!(stream.channel_name, "空澄セナ");
        assert_eq!(stream.title.as_deref(), Some("morning live"));

        let json = serde_json::to_string(&payload).unwrap();
        assert!(!json.contains("UCleakChannelId"));
        assert!(!json.contains("abc123video"));
        assert!(!json.contains("channel_id"));
        assert!(!json.contains("stream_id"));
        assert!(!json.contains("active_stream"));
    }

    #[test]
    fn a_node_without_a_name_falls_back_to_its_id() {
        let mut cluster = sample_cluster();
        cluster.nodes[0].name = "   ".to_string();

        let payload = PublicStatus::build(Some(&sample_status()), &cluster);
        assert_eq!(payload.nodes[0].name, "ny");
    }

    #[test]
    fn missing_status_renders_empty_rather_than_stale() {
        let payload = PublicStatus::build(None, &sample_cluster());

        assert!(!payload.in_sync);
        assert!(!payload.bilibili.is_live);
        assert!(payload.bilibili.title.is_empty());
        assert!(!payload.bilibili.enable_danmaku_command);
        assert!(payload.youtube.is_none());
        // Node cards still render: the page can say which servers are up even
        // when it cannot see the stream.
        assert_eq!(payload.nodes.len(), 1);
    }
}
