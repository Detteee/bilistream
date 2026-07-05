use axum::{
    extract::{Json, Query},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use futures_util::future::join_all;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use super::state::{
    get_logs, get_status_cache, platform_channel_configured, refresh_status_cache_config_from,
    update_status_cache, update_status_cache_with, BiliStatus, NetworkStatus,
    PriorityChannelStatus, TwStatus, YtStatus,
};
use crate::cluster::{
    apply_cluster_node_mode_locally, apply_monitored_config, cache_active_monitor_state_from_peer,
    cluster_sync_config_from_config, finalize_cluster_node_switch,
    get_cluster_status as load_cluster_status, monitored_config_integrity_version_from_payload,
    monitored_config_version, push_active_monitor_state_to_peers, push_monitored_config_to_peers,
    sync_monitored_config_after_change, ClusterActiveMonitorStateRequest,
    ClusterApplyNodeModeRequest, ClusterDrainRequest, ClusterFailoverRequest,
    ClusterHeartbeatRequest, ClusterStatus, ClusterSyncConfigRequest,
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
    use crate::cluster::{
        all_monitor_toggles_off, all_monitor_toggles_on, monitor_toggle_state_from_config,
        monitored_config_from_config, resolve_source_channel_targets,
        resolve_source_monitor_toggles, resolve_source_monitor_toggles_with_cache,
        ChannelTargetState, ClusterHealth, ClusterNodeRole, ClusterNodeSnapshot,
        MonitorToggleState,
    };
    use crate::config::{
        BiliLive, ClusterPeer, Credentials, FfmpegCache, PriorityChannel, Twitch, Youtube,
    };
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
        let mut cluster = ClusterConfig {
            node_id: "local".to_string(),
            ..ClusterConfig::default()
        };

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
        let mut cluster = ClusterConfig {
            node_id: "removed".to_string(),
            enabled: true,
            peers: vec![ClusterPeer {
                node_id: "la".to_string(),
                name: "Los Angeles".to_string(),
                api_url: "http://la:3150".to_string(),
                priority: 20,
            }],
            ..ClusterConfig::default()
        };

        apply_cluster_membership_to_config(&mut cluster, &membership_payload(None));

        assert!(!cluster.enabled);
        assert!(cluster.peers.is_empty());
    }

    #[test]
    fn membership_syncs_auto_failover_setting() {
        let mut cluster = ClusterConfig {
            node_id: "ca".to_string(),
            auto_failover: true,
            ..ClusterConfig::default()
        };

        let mut payload = membership_payload(Some("ca"));
        payload.auto_failover = false;
        apply_cluster_membership_to_config(&mut cluster, &payload);

        assert!(!cluster.auto_failover);
    }

    #[test]
    fn membership_apply_normalizes_inbound_nodes() {
        let mut cluster = ClusterConfig {
            node_id: "ca".to_string(),
            enabled: false,
            ..ClusterConfig::default()
        };
        let mut payload = membership_payload(Some(" ca "));
        payload.nodes[0].node_id = " la ".to_string();
        payload.nodes[0].name = " Los Angeles ".to_string();
        payload.nodes[0].api_url = " http://la:3150/ ".to_string();
        payload.nodes[1].node_id = " ca ".to_string();
        payload.nodes[1].name = " Canada ".to_string();
        payload.nodes[1].api_url = " http://ca:3150/ ".to_string();

        apply_cluster_membership_to_config(&mut cluster, &payload);

        assert!(cluster.enabled);
        assert_eq!(cluster.node_id, "ca");
        assert_eq!(cluster.node_name, "Canada");
        assert_eq!(cluster.public_api_url, "http://ca:3150");
        assert_eq!(cluster.peers.len(), 1);
        assert_eq!(cluster.peers[0].node_id, "la");
        assert_eq!(cluster.peers[0].name, "Los Angeles");
        assert_eq!(cluster.peers[0].api_url, "http://la:3150");
    }

    #[test]
    fn membership_export_skips_urls_that_normalize_empty() {
        let cluster = ClusterConfig {
            node_id: "local".to_string(),
            node_name: "Local".to_string(),
            public_api_url: "http://local:3150/".to_string(),
            peers: vec![
                ClusterPeer {
                    node_id: "slash".to_string(),
                    name: "Slash".to_string(),
                    api_url: " / ".to_string(),
                    priority: 1,
                },
                ClusterPeer {
                    node_id: "valid".to_string(),
                    name: " Valid ".to_string(),
                    api_url: " http://valid:3150/ ".to_string(),
                    priority: 2,
                },
            ],
            ..ClusterConfig::default()
        };

        let request = cluster_membership_from_config(&cluster);

        assert_eq!(request.nodes.len(), 2);
        assert!(request
            .nodes
            .iter()
            .any(|node| { node.node_id == "local" && node.api_url == "http://local:3150" }));
        assert!(request.nodes.iter().any(|node| {
            node.node_id == "valid" && node.name == "Valid" && node.api_url == "http://valid:3150"
        }));
        assert!(!request.nodes.iter().any(|node| node.node_id == "slash"));
    }

    #[test]
    fn membership_propagation_targets_include_new_and_removed_peers() {
        let old_cluster = ClusterConfig {
            node_id: "local".to_string(),
            peers: vec![
                ClusterPeer {
                    node_id: "removed".to_string(),
                    name: "Removed".to_string(),
                    api_url: " http://removed:3150/ ".to_string(),
                    priority: 1,
                },
                ClusterPeer {
                    node_id: "empty".to_string(),
                    name: "Empty".to_string(),
                    api_url: " ".to_string(),
                    priority: 1,
                },
                ClusterPeer {
                    node_id: "slash".to_string(),
                    name: "Slash".to_string(),
                    api_url: " / ".to_string(),
                    priority: 1,
                },
            ],
            ..ClusterConfig::default()
        };
        let new_cluster = ClusterConfig {
            node_id: "local".to_string(),
            peers: Vec::new(),
            ..ClusterConfig::default()
        };
        let request = ClusterMembershipRequest {
            target_node_id: None,
            enabled: true,
            sync_monitored_channels: true,
            auto_failover: true,
            heartbeat_interval_secs: 5,
            failover_timeout_secs: 15,
            lease_ttl_secs: 20,
            thresholds: ClusterHealthThresholds::default(),
            nodes: vec![
                ClusterMembershipNode {
                    node_id: "local".to_string(),
                    name: "Local".to_string(),
                    api_url: "http://local:3150".to_string(),
                    priority: 10,
                },
                ClusterMembershipNode {
                    node_id: "new".to_string(),
                    name: "New".to_string(),
                    api_url: "http://new:3150/".to_string(),
                    priority: 20,
                },
                ClusterMembershipNode {
                    node_id: "slash-new".to_string(),
                    name: "Slash New".to_string(),
                    api_url: "/".to_string(),
                    priority: 1,
                },
            ],
        };

        let targets = cluster_membership_propagation_targets(&old_cluster, &new_cluster, &request);

        assert_eq!(targets.len(), 2);
        assert_eq!(
            targets.get("new").map(String::as_str),
            Some("http://new:3150")
        );
        assert_eq!(
            targets.get("removed").map(String::as_str),
            Some("http://removed:3150")
        );
        assert!(!targets.contains_key("local"));
        assert!(!targets.contains_key("empty"));
        assert!(!targets.contains_key("slash"));
        assert!(!targets.contains_key("slash-new"));
    }

    #[test]
    fn membership_target_request_matches_owned_payload_shape() {
        let request = membership_payload(None);
        let borrowed = cluster_membership_target_request(&request, "ca");
        let mut owned = request.clone();
        owned.target_node_id = Some("ca".to_string());

        assert_eq!(
            serde_json::to_value(&borrowed).unwrap(),
            serde_json::to_value(&owned).unwrap()
        );
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
    fn monitor_toggle_match_requires_legacy_and_platform_flags() {
        let mut cfg = status_cache_test_config();
        cfg.youtube.enable_monitor = true;
        cfg.enable_youtube_monitor = false;
        cfg.twitch.enable_monitor = false;
        cfg.enable_twitch_monitor = true;

        assert!(!youtube_monitor_toggle_matches(&cfg, true));
        assert!(!twitch_monitor_toggle_matches(&cfg, false));

        cfg.enable_youtube_monitor = true;
        cfg.enable_twitch_monitor = false;

        assert!(youtube_monitor_toggle_matches(&cfg, true));
        assert!(twitch_monitor_toggle_matches(&cfg, false));
    }

    fn healthy_cluster_node(
        node_id: &str,
        monitor_toggles: MonitorToggleState,
        channel_targets: ChannelTargetState,
    ) -> ClusterNodeSnapshot {
        ClusterNodeSnapshot {
            node_id: node_id.to_string(),
            name: node_id.to_string(),
            api_url: format!("http://{}", node_id),
            priority: 0,
            last_seen: Some(1),
            is_local: false,
            role: ClusterNodeRole::Standby,
            health: ClusterHealth {
                healthy: true,
                reason: "healthy".to_string(),
                stale: false,
                stream_degraded: false,
            },
            draining: false,
            ddos: false,
            ffmpeg_running: false,
            active_stream: None,
            status: None,
            network: None,
            config_version: String::new(),
            failed_restarts: 0,
            monitor_toggles,
            channel_targets,
        }
    }

    fn cluster_status_with_node(node: ClusterNodeSnapshot) -> ClusterStatus {
        ClusterStatus {
            enabled: true,
            local_node_id: "local".to_string(),
            active_owner: Some(node.node_id.clone()),
            lease_until: Some(30),
            config_version: String::new(),
            auto_failover: true,
            nodes: vec![node],
        }
    }

    fn enabled_monitor_toggles() -> MonitorToggleState {
        MonitorToggleState {
            enable_danmaku_command: true,
            enable_youtube_monitor: true,
            enable_twitch_monitor: false,
            youtube_enable_monitor: true,
            twitch_enable_monitor: false,
            priority_channel_enabled: true,
            priority_channel_auto_restart: false,
        }
    }

    fn channel_targets(label: &str) -> ChannelTargetState {
        ChannelTargetState {
            youtube_channel_name: format!("yt-{}", label),
            youtube_channel_id: format!("ytid-{}", label),
            twitch_channel_name: format!("tw-{}", label),
            twitch_channel_id: format!("twid-{}", label),
            priority_channel_name: format!("priority-{}", label),
            priority_youtube_channel_id: format!("priority-yt-{}", label),
            priority_twitch_channel_id: format!("priority-tw-{}", label),
        }
    }

    #[test]
    fn node_switch_toggle_resolution_uses_snapshot_before_exported_off_toggles() {
        let cfg = status_cache_test_config();
        let expected_toggles = enabled_monitor_toggles();
        let before = cluster_status_with_node(healthy_cluster_node(
            "source",
            expected_toggles.clone(),
            ChannelTargetState::default(),
        ));
        let exported = monitored_config_from_config(&status_cache_test_config());

        let resolved = resolve_source_monitor_toggles(&cfg, &before, "source", &exported, true);

        assert_eq!(resolved, expected_toggles);
    }

    #[test]
    fn node_switch_toggle_resolution_preserves_known_all_off_source() {
        let cfg = status_cache_test_config();
        let before = cluster_status_with_node(healthy_cluster_node(
            "source",
            all_monitor_toggles_off(),
            ChannelTargetState::default(),
        ));
        let exported = monitored_config_from_config(&status_cache_test_config());

        let resolved = resolve_source_monitor_toggles(&cfg, &before, "source", &exported, true);

        assert_eq!(resolved, all_monitor_toggles_off());
    }

    #[test]
    fn node_switch_toggle_resolution_uses_current_local_toggles() {
        let mut cfg = status_cache_test_config();
        cfg.cluster.node_id = "local".to_string();
        cfg.bililive.enable_danmaku_command = true;
        cfg.enable_youtube_monitor = true;
        cfg.youtube.enable_monitor = true;
        cfg.priority_channel.enabled = true;
        let before = ClusterStatus {
            enabled: true,
            local_node_id: "local".to_string(),
            active_owner: Some("local".to_string()),
            lease_until: Some(30),
            config_version: String::new(),
            auto_failover: true,
            nodes: Vec::new(),
        };
        let exported = monitored_config_from_config(&status_cache_test_config());

        let resolved = resolve_source_monitor_toggles(&cfg, &before, "local", &exported, true);

        assert_eq!(resolved, monitor_toggle_state_from_config(&cfg));
    }

    #[test]
    fn node_switch_toggle_resolution_defaults_new_active_to_all_on() {
        let cfg = status_cache_test_config();
        let before = ClusterStatus {
            enabled: true,
            local_node_id: "local".to_string(),
            active_owner: Some("source".to_string()),
            lease_until: Some(30),
            config_version: String::new(),
            auto_failover: true,
            nodes: Vec::new(),
        };
        let exported = monitored_config_from_config(&status_cache_test_config());

        let resolved = resolve_source_monitor_toggles_with_cache(
            &cfg, &before, "source", &exported, false, None,
        );

        assert_eq!(resolved, all_monitor_toggles_on());
    }

    #[test]
    fn node_switch_toggle_resolution_accepts_authoritative_exported_all_off() {
        let cfg = status_cache_test_config();
        let before = ClusterStatus {
            enabled: true,
            local_node_id: "local".to_string(),
            active_owner: Some("source".to_string()),
            lease_until: Some(30),
            config_version: String::new(),
            auto_failover: true,
            nodes: Vec::new(),
        };
        let exported = monitored_config_from_config(&status_cache_test_config());

        let resolved = resolve_source_monitor_toggles_with_cache(
            &cfg, &before, "source", &exported, true, None,
        );

        assert_eq!(resolved, all_monitor_toggles_off());
    }

    #[test]
    fn cluster_failover_target_validation_rejects_unknown_nodes() {
        let mut cfg = status_cache_test_config();
        cfg.cluster.node_id = "local".to_string();
        cfg.cluster.peers = vec![ClusterPeer {
            node_id: "peer".to_string(),
            name: "Peer".to_string(),
            api_url: "http://peer".to_string(),
            priority: 1,
        }];

        assert_eq!(
            normalize_cluster_failover_target(&cfg, Some(" peer ".to_string())).unwrap(),
            Some("peer".to_string())
        );
        assert_eq!(normalize_cluster_failover_target(&cfg, None).unwrap(), None);
        assert!(normalize_cluster_failover_target(&cfg, Some(" ".to_string())).is_err());
        assert!(normalize_cluster_failover_target(&cfg, Some("missing".to_string())).is_err());
    }

    #[test]
    fn node_switch_channel_resolution_prefers_current_exported_config() {
        let cfg = status_cache_test_config();
        let stale_targets = channel_targets("stale");
        let current_targets = channel_targets("current");
        let before = cluster_status_with_node(healthy_cluster_node(
            "source",
            MonitorToggleState::default(),
            stale_targets,
        ));
        let mut exported_cfg = status_cache_test_config();
        exported_cfg.youtube.channel_name = current_targets.youtube_channel_name.clone();
        exported_cfg.youtube.channel_id = current_targets.youtube_channel_id.clone();
        exported_cfg.twitch.channel_name = current_targets.twitch_channel_name.clone();
        exported_cfg.twitch.channel_id = current_targets.twitch_channel_id.clone();
        exported_cfg.priority_channel.channel_name = current_targets.priority_channel_name.clone();
        exported_cfg.priority_channel.youtube_channel_id =
            current_targets.priority_youtube_channel_id.clone();
        exported_cfg.priority_channel.twitch_channel_id =
            current_targets.priority_twitch_channel_id.clone();
        let exported = monitored_config_from_config(&exported_cfg);

        let resolved = resolve_source_channel_targets(&cfg, &before, "source", &exported);

        assert_eq!(resolved, current_targets);
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
    fn monitor_toggle_sync_generation_rejects_stale_work() {
        let first = next_active_monitor_sync_generation();
        assert!(active_monitor_sync_generation_is_current(first));

        let second = next_active_monitor_sync_generation();
        assert!(!active_monitor_sync_generation_is_current(first));
        assert!(active_monitor_sync_generation_is_current(second));
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
