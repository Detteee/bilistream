use axum::{
    extract::{Json, Query},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use super::state::{
    get_logs, get_status_cache, refresh_status_cache_config_from, update_status_cache,
    update_status_cache_with, BiliStatus, NetworkStatus, PriorityChannelStatus, TwStatus, YtStatus,
};
use crate::cluster::{
    all_monitor_toggles_off, apply_channel_target_state_to_config,
    apply_monitor_toggle_state_to_config, apply_monitored_config,
    channel_target_state_from_monitored_config, get_cluster_status as load_cluster_status,
    monitor_toggle_state_from_monitored_config, monitored_config_from_config,
    monitored_config_integrity_version, monitored_config_integrity_version_from_payload,
    monitored_config_version, push_monitored_config_to_peers, ClusterApplyNodeModeRequest,
    ClusterDrainRequest, ClusterFailoverRequest, ClusterHeartbeatRequest, ClusterStatus,
    ClusterSyncConfigRequest,
};
use crate::config::{load_config, ClusterConfig, ClusterHealthThresholds, ClusterPeer, Config};
use crate::plugins::{
    bili_change_live_title, bili_start_live, bili_stop_live, bili_update_area, bilibili,
    get_bili_live_status, get_ffmpeg_cache_speed, get_ffmpeg_network_stats, get_ffmpeg_speed,
    is_ffmpeg_hls_cache_active, send_danmaku as send_danmaku_to_bili, set_config_updated,
};
use crate::updater;

mod cluster;
mod config;
mod crop;
mod holodex;
mod manage;
mod setup;
mod status;
mod stream;

pub use cluster::*;
pub use config::*;
pub use crop::*;
pub use holodex::*;
pub use manage::*;
pub use setup::*;
pub use status::*;
pub use stream::*;

fn config_save_status(error: Box<dyn std::error::Error>) -> StatusCode {
    if error
        .downcast_ref::<std::io::Error>()
        .is_some_and(|error| error.kind() == std::io::ErrorKind::WouldBlock)
    {
        StatusCode::CONFLICT
    } else {
        tracing::error!("Configuration save failed: {error}");
        StatusCode::INTERNAL_SERVER_ERROR
    }
}

#[derive(Serialize)]
pub struct ApiResponse<T> {
    success: bool,
    data: Option<T>,
    message: Option<String>,
}

impl<T: Serialize> IntoResponse for ApiResponse<T> {
    fn into_response(self) -> Response {
        Json(self).into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{BiliLive, Credentials, FfmpegCache, PriorityChannel, Twitch, Youtube};
    use crate::StatusData;

    #[test]
    fn websub_settings_accept_only_http_urls_and_real_ports() {
        assert!(validate_websub(None, None).is_ok());
        assert!(validate_websub(Some(""), Some(3151)).is_ok());
        assert!(validate_websub(Some("https://yt.example.com/websub/youtube"), None).is_ok());
        assert!(validate_websub(Some("http://1.2.3.4:3151/websub/youtube"), None).is_ok());
        assert!(validate_websub(Some("ftp://example.com/x"), None).is_err());
        assert!(validate_websub(Some("example.com/websub"), None).is_err());
        assert!(validate_websub(None, Some(0)).is_err());
    }

    #[test]
    fn browser_edits_merge_unrelated_changes_but_reject_stale_fields() {
        let expected = HashMap::from([("interval".to_string(), json!(30))]);
        let patch = json!({"interval": 75, "auto_cover": null});
        assert!(validate_edit_preconditions(
            &json!({"interval": 30, "auto_cover": true}),
            &patch,
            Some(&expected)
        )
        .is_ok());
        assert_eq!(
            validate_edit_preconditions(
                &json!({"interval": 45, "auto_cover": true}),
                &patch,
                Some(&expected)
            ),
            Err(EDIT_CONFLICT)
        );
        assert_eq!(
            validate_edit_preconditions(
                &json!({"interval": 30, "auto_cover": true}),
                &json!({"interval": 75, "auto_cover": false}),
                Some(&expected)
            ),
            Err(EDIT_INVALID)
        );
        assert!(validate_edit_preconditions(&json!({}), &patch, None).is_ok());
    }

    #[test]
    fn keyword_preconditions_compare_array_contents() {
        let expected = HashMap::from([("streaming_banned_keywords".to_string(), json!(["old"]))]);
        let patch = json!({"streaming_banned_keywords": []});
        assert_eq!(
            validate_edit_preconditions(
                &json!({"streaming_banned_keywords": ["new"]}),
                &patch,
                Some(&expected)
            ),
            Err(EDIT_CONFLICT)
        );
        assert!(validate_edit_preconditions(
            &json!({"streaming_banned_keywords": ["old"]}),
            &patch,
            Some(&expected)
        )
        .is_ok());
    }

    #[test]
    fn qr_images_are_local_svg_and_oversized_payloads_fail() {
        use base64::Engine as _;
        let image = qr_code_data_url("https://example.invalid/login?token=测试").unwrap();
        let encoded = image.strip_prefix("data:image/svg+xml;base64,").unwrap();
        let svg = String::from_utf8(
            base64::engine::general_purpose::STANDARD
                .decode(encoded)
                .unwrap(),
        )
        .unwrap();
        assert!(svg.contains("<svg"));
        assert!(!svg.contains("example.invalid"));
        assert!(qr_code_data_url(&"x".repeat(10000)).is_err());
    }

    #[test]
    fn monitor_target_reload_needed_only_for_enable_or_enabled_channel_change() {
        assert!(!monitor_target_reload_needed(
            true, true, "Channel", "Channel", "id", "id",
        ));
        assert!(monitor_target_reload_needed(
            true, false, "Channel", "Channel", "id", "id",
        ));
        assert!(monitor_target_reload_needed(
            false, true, "Channel", "Channel", "id", "id",
        ));
        assert!(monitor_target_reload_needed(
            true, true, "Channel", "Other", "id", "id",
        ));
        assert!(monitor_target_reload_needed(
            true, true, "Channel", "Channel", "id", "other-id",
        ));
        assert!(!monitor_target_reload_needed(
            false, false, "Channel", "Other", "id", "other-id",
        ));
    }

    #[test]
    fn crop_update_validation_rejects_incomplete_or_zero_size() {
        let disabled = UpdateCropRequest {
            platform: "youtube".to_string(),
            enabled: false,
            width: None,
            height: None,
            x: None,
            y: None,
        };
        assert!(crop_config_from_update(&disabled).unwrap().is_none());

        let missing = UpdateCropRequest {
            platform: "youtube".to_string(),
            enabled: true,
            width: Some(1920),
            height: None,
            x: Some(0),
            y: Some(0),
        };
        assert_eq!(
            crop_config_from_update(&missing).unwrap_err(),
            "Crop dimensions required when enabled"
        );

        let zero_width = UpdateCropRequest {
            platform: "youtube".to_string(),
            enabled: true,
            width: Some(0),
            height: Some(1080),
            x: Some(0),
            y: Some(0),
        };
        assert_eq!(
            crop_config_from_update(&zero_width).unwrap_err(),
            "Crop width and height must be greater than 0"
        );

        let valid = UpdateCropRequest {
            platform: "youtube".to_string(),
            enabled: true,
            width: Some(1280),
            height: Some(720),
            x: Some(10),
            y: Some(20),
        };
        let crop = crop_config_from_update(&valid)
            .unwrap()
            .expect("valid crop should be returned");
        assert_eq!(crop.width, 1280);
        assert_eq!(crop.height, 720);
        assert_eq!(crop.x, 10);
        assert_eq!(crop.y, 20);
    }

    #[test]
    fn ordered_channel_ids_deduplicates_without_losing_order() {
        let mut channel_ids = OrderedChannelIds::default();

        assert!(channel_ids.insert(" first "));
        assert!(channel_ids.insert("second"));
        assert!(!channel_ids.insert("first"));
        assert!(!channel_ids.insert("  "));

        let (ordered, seen) = channel_ids.into_parts();
        assert_eq!(ordered, vec!["first".to_string(), "second".to_string()]);
        assert!(seen.contains("first"));
        assert!(seen.contains("second"));
        assert_eq!(seen.len(), 2);
    }

    fn membership_payload(target_node_id: Option<&str>) -> ClusterMembershipRequest {
        ClusterMembershipRequest {
            target_node_id: target_node_id.map(str::to_string),
            enabled: true,
            sync_monitored_channels: true,
            auto_failover: true,
            heartbeat_interval_secs: 5,
            failover_timeout_secs: 15,
            lease_ttl_secs: 20,
            thresholds: ClusterHealthThresholds::default(),
            nodes: vec![
                ClusterMembershipNode {
                    node_id: "la".to_string(),
                    name: "Los Angeles".to_string(),
                    api_url: "http://la:3150".to_string(),
                    priority: 20,
                },
                ClusterMembershipNode {
                    node_id: "ca".to_string(),
                    name: "Canada".to_string(),
                    api_url: "http://ca:3150".to_string(),
                    priority: 10,
                },
            ],
        }
    }

    #[test]
    fn membership_target_bootstraps_node_identity() {
        let mut cluster = ClusterConfig::default();
        cluster.node_id = "local".to_string();

        apply_cluster_membership_to_config(&mut cluster, &membership_payload(Some("ca")));

        assert!(cluster.enabled);
        assert_eq!(cluster.node_id, "ca");
        assert_eq!(cluster.node_name, "Canada");
        assert_eq!(cluster.public_api_url, "http://ca:3150");
        assert_eq!(cluster.priority, 10);
        assert_eq!(cluster.peers.len(), 1);
        assert_eq!(cluster.peers[0].node_id, "la");
    }

    #[test]
    fn membership_without_matching_node_disables_cluster() {
        let mut cluster = ClusterConfig::default();
        cluster.node_id = "removed".to_string();
        cluster.enabled = true;
        cluster.peers = vec![ClusterPeer {
            node_id: "la".to_string(),
            name: "Los Angeles".to_string(),
            api_url: "http://la:3150".to_string(),
            priority: 20,
        }];

        apply_cluster_membership_to_config(&mut cluster, &membership_payload(None));

        assert!(!cluster.enabled);
        assert!(cluster.peers.is_empty());
    }

    #[test]
    fn membership_syncs_auto_failover_setting() {
        let mut cluster = ClusterConfig::default();
        cluster.node_id = "ca".to_string();
        cluster.auto_failover = true;

        let mut payload = membership_payload(Some("ca"));
        payload.auto_failover = false;
        apply_cluster_membership_to_config(&mut cluster, &payload);

        assert!(!cluster.auto_failover);
    }

    fn status_cache_test_config() -> Config {
        Config {
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
                enable_monitor: false,
                channel_name: "tw-channel".to_string(),
                area_v2: 235,
                channel_id: "twid".to_string(),
                proxy_region: String::new(),
                quality: "best".to_string(),
                proxy: None,
                crop: None,
                ffmpeg_cache: FfmpegCache::default(),
            },
            youtube: Youtube {
                enable_monitor: false,
                channel_name: "yt-channel".to_string(),
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
            riot_api_key: None,
            enable_lol_monitor: false,
            lol_monitor_interval: None,
            anti_collision_list: HashMap::new(),
            priority_channel: PriorityChannel::default(),
            enable_youtube_monitor: false,
            enable_twitch_monitor: false,
            cluster: ClusterConfig::default(),
        }
    }

    #[test]
    fn disabled_platform_monitors_keep_configured_channel_status() {
        update_status_cache(StatusData::default());

        refresh_status_cache_config_from(&status_cache_test_config());

        let status = get_status_cache().expect("status cache should be initialized");
        let youtube = status.youtube.expect("youtube status should be present");
        let twitch = status.twitch.expect("twitch status should be present");

        assert!(!youtube.is_live);
        assert_eq!(youtube.channel_name, "yt-channel");
        assert_eq!(youtube.channel_id, "ytid");
        assert!(!twitch.is_live);
        assert_eq!(twitch.channel_name, "tw-channel");
        assert_eq!(twitch.channel_id, "twid");
    }

    #[test]
    fn screen_session_parser_matches_named_session() {
        let screen_list = "\
There is a screen on:
\t906.b\t(06/28/2026 11:22:57 PM)\t(Attached)
1 Socket in /run/screen/S-root.
";

        assert_eq!(
            find_screen_session_by_name(screen_list, "b"),
            Some("906.b".to_string())
        );
        assert_eq!(find_screen_session_by_name(screen_list, "bb"), None);
    }
}
