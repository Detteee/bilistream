use super::*;
use crate::config::{
    BiliLive, ClusterConfig, ClusterHealthThresholds, Config, Credentials, FfmpegCache,
    PriorityChannel, Twitch, Youtube,
};
use std::collections::HashMap;
use std::fs;
use std::sync::atomic::Ordering;
use std::sync::{Mutex, MutexGuard, RwLock};
use std::time::Duration;

static CLUSTER_STATE_TEST_LOCK: Mutex<()> = Mutex::new(());

#[test]
fn recover_locks_return_inner_after_poison() {
    let lock = RwLock::new(1_u32);
    let _ = std::panic::catch_unwind(|| {
        let mut guard = lock.write().unwrap();
        *guard = 2;
        panic!("poison test lock");
    });

    {
        let mut guard = recover_write_lock(&lock, "test lock");
        assert_eq!(*guard, 2);
        *guard = 3;
    }

    assert_eq!(*recover_read_lock(&lock, "test lock"), 3);
}

pub(crate) struct ClusterStateGuard {
    _lock: MutexGuard<'static, ()>,
    snapshot: ClusterState,
}

impl ClusterStateGuard {
    pub(crate) fn new() -> Self {
        let lock = CLUSTER_STATE_TEST_LOCK.lock().unwrap_or_else(|poisoned| {
            tracing::warn!("Recovering poisoned cluster state test lock");
            poisoned.into_inner()
        });
        let snapshot = cluster_state_read().clone();
        Self {
            _lock: lock,
            snapshot,
        }
    }
}

impl Drop for ClusterStateGuard {
    fn drop(&mut self) {
        *cluster_state_write() = self.snapshot.clone();
    }
}

pub(crate) fn test_config(node_id: &str, priority: i32) -> Config {
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
        riot_api_key: None,
        enable_lol_monitor: false,
        lol_monitor_interval: None,
        anti_collision_list: HashMap::new(),
        priority_channel: PriorityChannel::default(),
        enable_youtube_monitor: true,
        enable_twitch_monitor: true,
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
        },
    }
}

#[test]
fn monitored_config_version_ignores_cluster_identity() {
    let cfg_a = test_config("a", 0);
    let cfg_b = test_config("b", 10);

    assert_eq!(
        monitored_config_version(&cfg_a),
        monitored_config_version(&cfg_b)
    );
}

#[test]
fn monitored_config_version_matches_payload_target_hash() {
    let mut cfg = test_config("a", 0);
    cfg.priority_channel.channel_name = "priority".to_string();
    cfg.priority_channel.youtube_channel_id = "priority-yt".to_string();
    let payload = monitored_config_from_config(&cfg);

    assert_eq!(
        monitored_config_version(&cfg),
        monitored_config_version_from_payload(&payload)
    );
}

#[test]
fn monitored_config_version_is_stable_for_map_order() {
    let mut cfg_a = test_config("a", 0);
    cfg_a.anti_collision_list.insert("alpha".to_string(), 1);
    cfg_a.anti_collision_list.insert("beta".to_string(), 2);

    let mut cfg_b = test_config("a", 0);
    cfg_b.anti_collision_list.insert("beta".to_string(), 2);
    cfg_b.anti_collision_list.insert("alpha".to_string(), 1);

    assert_eq!(
        monitored_config_integrity_version(&cfg_a),
        monitored_config_integrity_version(&cfg_b)
    );
}

#[test]
fn canonical_json_sorts_keys_without_changing_shape() {
    let value = serde_json::json!({
        "b": 1,
        "a": [
            true,
            null,
            {
                "z": 2,
                "x": "quote\"",
            }
        ],
    });

    assert_eq!(
        canonical_json(&value),
        r#"{"a":[true,null,{"x":"quote\"","z":2}],"b":1}"#
    );
}

#[test]
fn cluster_json_tmp_paths_are_unique_siblings() {
    let path = std::env::temp_dir().join("bilistream-cluster-state.json");

    let first = unique_json_tmp_path(&path);
    let second = unique_json_tmp_path(&path);

    assert_ne!(first, second);
    assert_eq!(first.parent(), path.parent());
    assert_eq!(second.parent(), path.parent());
    assert!(first
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with("bilistream-cluster-state.json.tmp-")));
}

#[test]
fn cluster_json_atomic_write_replaces_target_without_leftover_tmp() {
    let dir = std::env::temp_dir().join(format!(
        "bilistream-cluster-json-test-{}-{}",
        std::process::id(),
        JSON_TMP_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("state.json");

    write_json_file_atomic(&path, br#"{"old":true}"#).unwrap();
    write_json_file_atomic(&path, br#"{"new":true}"#).unwrap();

    assert_eq!(fs::read_to_string(&path).unwrap(), r#"{"new":true}"#);
    let entries = fs::read_dir(&dir)
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].path(), path);

    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn cluster_sync_config_hashes_exported_payload() {
    let cfg = test_config("a", 0);
    let request = cluster_sync_config_from_config(&cfg);

    assert_eq!(
        request.config_version,
        monitored_config_integrity_version_from_payload(&request.monitored_config)
    );
}

#[test]
fn monitored_config_version_ignores_runtime_monitor_toggles() {
    let cfg_a = test_config("a", 0);
    let mut cfg_b = test_config("b", 10);
    cfg_b.bililive.enable_danmaku_command = !cfg_a.bililive.enable_danmaku_command;
    cfg_b.enable_youtube_monitor = !cfg_a.enable_youtube_monitor;
    cfg_b.enable_twitch_monitor = !cfg_a.enable_twitch_monitor;
    cfg_b.youtube.enable_monitor = !cfg_a.youtube.enable_monitor;
    cfg_b.twitch.enable_monitor = !cfg_a.twitch.enable_monitor;

    assert_eq!(
        monitored_config_version(&cfg_a),
        monitored_config_version(&cfg_b)
    );
    assert_ne!(
        monitored_config_integrity_version(&cfg_a),
        monitored_config_integrity_version(&cfg_b)
    );
}

#[test]
fn monitored_config_version_ignores_priority_channel_switches() {
    // enabled/auto_restart are per-node monitor toggles, so two nodes that
    // differ only by them are still in sync and must not trigger a push.
    let cfg_a = test_config("a", 0);
    let mut cfg_b = test_config("b", 10);
    cfg_b.priority_channel.enabled = !cfg_a.priority_channel.enabled;

    assert_eq!(
        monitored_config_version(&cfg_a),
        monitored_config_version(&cfg_b)
    );

    let mut cfg_c = test_config("c", 20);
    cfg_c.priority_channel.auto_restart = !cfg_a.priority_channel.auto_restart;

    assert_eq!(
        monitored_config_version(&cfg_a),
        monitored_config_version(&cfg_c)
    );

    // The shared channel target fields still take part in the version.
    let mut cfg_d = test_config("d", 30);
    cfg_d.priority_channel.default_area = cfg_a.priority_channel.default_area.wrapping_add(1);

    assert_ne!(
        monitored_config_version(&cfg_a),
        monitored_config_version(&cfg_d)
    );

    let mut cfg_e = test_config("e", 40);
    cfg_e.priority_channel.channel_name = "another priority".to_string();

    assert_ne!(
        monitored_config_version(&cfg_a),
        monitored_config_version(&cfg_e)
    );
}

#[test]
fn monitored_config_version_changes_for_managed_json_payloads() {
    let mut base = monitored_config_from_config(&test_config("a", 0));
    base.channels_json = Some(serde_json::json!({ "channels": [] }));
    base.areas_json = Some(serde_json::json!({ "areas": [] }));

    let mut changed_channels = base.clone();
    changed_channels.channels_json = Some(serde_json::json!({
        "channels": [
            {
                "name": "new",
                "aliases": [],
                "platforms": { "youtube": "yt" }
            }
        ]
    }));

    let mut changed_areas = base.clone();
    changed_areas.areas_json = Some(serde_json::json!({
        "areas": [
            {
                "id": 1,
                "name": "area",
                "title_keywords": [],
                "aliases": []
            }
        ]
    }));

    assert_ne!(
        monitored_config_version_from_payload(&base),
        monitored_config_version_from_payload(&changed_channels)
    );
    assert_ne!(
        monitored_config_version_from_payload(&base),
        monitored_config_version_from_payload(&changed_areas)
    );
}

#[test]
fn monitored_config_version_changes_for_channel_targets() {
    let cfg_a = test_config("a", 0);
    let mut cfg_b = test_config("a", 0);
    cfg_b.youtube.channel_name = "new yt".to_string();
    cfg_b.youtube.channel_id = "new-yt-id".to_string();

    assert_ne!(
        monitored_config_version(&cfg_a),
        monitored_config_version(&cfg_b)
    );

    let mut cfg_c = test_config("a", 0);
    cfg_c.twitch.channel_name = "new tw".to_string();
    cfg_c.twitch.channel_id = "new-tw-id".to_string();

    assert_ne!(
        monitored_config_version(&cfg_a),
        monitored_config_version(&cfg_c)
    );

    let mut cfg_d = test_config("a", 0);
    cfg_d.priority_channel.channel_name = "priority".to_string();
    cfg_d.priority_channel.youtube_channel_id = "priority-yt".to_string();
    cfg_d.priority_channel.twitch_channel_id = "priority-tw".to_string();

    assert_ne!(
        monitored_config_version(&cfg_a),
        monitored_config_version(&cfg_d)
    );
}

#[test]
fn applying_monitored_config_syncs_channels_and_preserves_runtime_toggles() {
    let mut local = test_config("local", 0);
    local.bililive.enable_danmaku_command = false;
    local.enable_youtube_monitor = false;
    local.enable_twitch_monitor = true;
    local.youtube.enable_monitor = false;
    local.twitch.enable_monitor = true;
    local.priority_channel.enabled = true;
    local.priority_channel.auto_restart = false;

    let mut source = test_config("source", 10);
    source.bililive.enable_danmaku_command = true;
    source.enable_youtube_monitor = true;
    source.enable_twitch_monitor = false;
    source.youtube.enable_monitor = true;
    source.youtube.channel_name = "remote yt".to_string();
    source.youtube.channel_id = "remote-yt-id".to_string();
    source.twitch.enable_monitor = false;
    source.twitch.channel_name = "remote tw".to_string();
    source.twitch.channel_id = "remote-tw-id".to_string();
    source.priority_channel.enabled = false;
    source.priority_channel.channel_name = "remote priority".to_string();
    source.priority_channel.youtube_channel_id = "remote-priority-yt".to_string();
    source.priority_channel.twitch_channel_id = "remote-priority-tw".to_string();
    source.priority_channel.auto_restart = true;

    apply_monitored_config_to_config(&mut local, monitored_config_from_config(&source));

    assert!(!local.bililive.enable_danmaku_command);
    assert!(!local.enable_youtube_monitor);
    assert!(local.enable_twitch_monitor);
    assert!(!local.youtube.enable_monitor);
    assert!(local.twitch.enable_monitor);
    // The priority switches are node-local runtime state like the other
    // monitor toggles: a pushed config must not flip them on this node.
    assert!(local.priority_channel.enabled);
    assert!(!local.priority_channel.auto_restart);

    assert_eq!(local.youtube.channel_name, "remote yt");
    assert_eq!(local.youtube.channel_id, "remote-yt-id");
    assert_eq!(local.twitch.channel_name, "remote tw");
    assert_eq!(local.twitch.channel_id, "remote-tw-id");
    assert_eq!(local.priority_channel.channel_name, "remote priority");
    assert_eq!(
        local.priority_channel.youtube_channel_id,
        "remote-priority-yt"
    );
    assert_eq!(
        local.priority_channel.twitch_channel_id,
        "remote-priority-tw"
    );
}

#[test]
fn monitor_toggle_state_applies_without_changing_channel_targets() {
    let mut cfg = test_config("local", 0);
    cfg.youtube.channel_name = "yt target".to_string();
    cfg.youtube.channel_id = "yt-id".to_string();
    cfg.twitch.channel_name = "tw target".to_string();
    cfg.twitch.channel_id = "tw-id".to_string();
    cfg.priority_channel.channel_name = "priority target".to_string();
    cfg.priority_channel.youtube_channel_id = "priority-yt".to_string();
    cfg.priority_channel.twitch_channel_id = "priority-tw".to_string();

    let toggles = MonitorToggleState {
        enable_danmaku_command: true,
        enable_youtube_monitor: false,
        enable_twitch_monitor: true,
        youtube_enable_monitor: true,
        twitch_enable_monitor: false,
        priority_channel_enabled: true,
        priority_channel_auto_restart: true,
    };

    apply_monitor_toggle_state_to_config(&mut cfg, &toggles);

    assert_eq!(monitor_toggle_state_from_config(&cfg), toggles);
    assert_eq!(cfg.youtube.channel_name, "yt target");
    assert_eq!(cfg.youtube.channel_id, "yt-id");
    assert_eq!(cfg.twitch.channel_name, "tw target");
    assert_eq!(cfg.twitch.channel_id, "tw-id");
    assert_eq!(cfg.priority_channel.channel_name, "priority target");
    assert_eq!(cfg.priority_channel.youtube_channel_id, "priority-yt");
    assert_eq!(cfg.priority_channel.twitch_channel_id, "priority-tw");
}

#[test]
fn node_mode_without_explicit_state_preserves_desired_config() {
    let mut cfg = test_config("local", 0);
    let toggles = monitor_toggle_state_from_config(&cfg);
    let targets = channel_target_state_from_config(&cfg);

    apply_node_mode_config_state(&mut cfg, None, None);

    assert_eq!(monitor_toggle_state_from_config(&cfg), toggles);
    assert_eq!(channel_target_state_from_config(&cfg), targets);
}

#[test]
fn handoff_preserves_source_toggles_and_applies_them_to_target() {
    let mut source = test_config("source", 10);
    let source_toggles = MonitorToggleState {
        enable_danmaku_command: true,
        enable_youtube_monitor: true,
        enable_twitch_monitor: false,
        youtube_enable_monitor: true,
        twitch_enable_monitor: false,
        priority_channel_enabled: true,
        priority_channel_auto_restart: true,
    };
    apply_monitor_toggle_state_to_config(&mut source, &source_toggles);

    let mut target = test_config("target", 5);
    apply_monitor_toggle_state_to_config(&mut target, &all_monitor_toggles_off());

    apply_node_mode_config_state(&mut source, None, None);
    apply_node_mode_config_state(&mut target, None, Some(&source_toggles));

    assert_eq!(monitor_toggle_state_from_config(&source), source_toggles);
    assert_eq!(monitor_toggle_state_from_config(&target), source_toggles);
}

#[test]
fn node_mode_precondition_rejects_delayed_reverse_transition() {
    assert!(validate_node_mode_precondition(Some("a"), Some("b"), false, "a").is_err());
    assert!(validate_node_mode_precondition(Some("b"), Some("b"), false, "a").is_ok());
    assert!(validate_node_mode_precondition(Some("b"), Some("a"), true, "b").is_ok());
    assert!(validate_node_mode_precondition(Some("a"), Some("a"), true, "b").is_ok());
    assert!(validate_node_mode_precondition(Some("c"), Some("a"), true, "b").is_err());
}

#[test]
fn failed_restart_window_prunes_old_failures() {
    let now = 10_000;
    let mut failures = vec![
        now - FFMPEG_FAILURE_WINDOW_SECS - 1,
        now - FFMPEG_FAILURE_WINDOW_SECS,
        now - 10,
    ];

    prune_failed_restart_times(&mut failures, now);

    assert_eq!(failures, vec![now - FFMPEG_FAILURE_WINDOW_SECS, now - 10]);
}

#[test]
fn recent_time_pruning_keeps_inclusive_cutoff_and_future_samples() {
    let now = 10_000;
    let window = 60;
    let mut failures = vec![now - window - 1, now - window, now - 1, now + 1];

    prune_recent_times(&mut failures, now, window);

    assert_eq!(failures, vec![now - window, now - 1, now + 1]);
}

#[test]
fn record_recent_time_preserves_chronological_order() {
    let mut failures = vec![10, 30];

    record_recent_time(&mut failures, 20);
    record_recent_time(&mut failures, 40);

    assert_eq!(failures, vec![10, 20, 30, 40]);
}

#[test]
fn restart_threshold_uses_windowed_failure_count() {
    let mut cfg = test_config("a", 0);
    cfg.cluster.thresholds.max_failed_restarts = 3;

    assert!(!ffmpeg_restart_degraded(&cfg.cluster, 2));
    assert!(ffmpeg_restart_degraded(&cfg.cluster, 3));
}

#[test]
fn cluster_request_timeouts_are_bounded() {
    let mut cfg = test_config("a", 0);

    cfg.cluster.heartbeat_interval_secs = 1;
    assert_eq!(cluster_heartbeat_timeout(&cfg), Duration::from_secs(3));
    assert_eq!(cluster_control_timeout(&cfg), Duration::from_secs(5));

    cfg.cluster.heartbeat_interval_secs = 3_600;
    assert_eq!(cluster_heartbeat_timeout(&cfg), Duration::from_secs(10));
    assert_eq!(cluster_control_timeout(&cfg), Duration::from_secs(15));
}

#[test]
fn heartbeat_cycle_accounts_for_request_time() {
    assert_eq!(
        heartbeat_cycle_delay(Duration::from_secs(10), Duration::from_secs(4)),
        Duration::from_secs(6)
    );
    assert_eq!(
        heartbeat_cycle_delay(Duration::from_secs(10), Duration::from_secs(12)),
        Duration::ZERO
    );
}

#[test]
fn execution_quorum_requires_fresh_direct_heartbeat_acks() {
    let mut cfg = test_config("a", 0);
    cfg.cluster.failover_timeout_secs = 10;
    cfg.cluster.peers = ["b", "c", "d"]
        .into_iter()
        .map(|node_id| crate::config::ClusterPeer {
            node_id: node_id.to_string(),
            name: node_id.to_string(),
            api_url: format!("http://{node_id}"),
            priority: 0,
        })
        .collect();
    let now = 100;
    let mut state = ClusterState::default();

    assert!(!state_has_fresh_quorum(&state, &cfg, now));
    state.peer_heartbeat_acks.insert("b".to_string(), now);
    assert!(!state_has_fresh_quorum(&state, &cfg, now));
    state.peer_heartbeat_acks.insert("c".to_string(), now);
    assert!(state_has_fresh_quorum(&state, &cfg, now));

    state.peer_heartbeat_acks.clear();
    state.peer_heartbeat_acks.insert("b".to_string(), now - 11);
    state.peer_heartbeat_acks.insert("c".to_string(), now);
    state.peer_heartbeat_acks.insert("d".to_string(), now);
    assert!(state_has_fresh_quorum(&state, &cfg, now));
    state.peer_heartbeat_acks.remove("d");
    assert!(!state_has_fresh_quorum(&state, &cfg, now));
}

#[test]
fn two_node_partition_fails_closed_after_ack_expiry() {
    let mut cfg = test_config("a", 0);
    cfg.cluster.failover_timeout_secs = 10;
    cfg.cluster.peers = vec![crate::config::ClusterPeer {
        node_id: "b".to_string(),
        name: "b".to_string(),
        api_url: "http://b".to_string(),
        priority: 0,
    }];
    let mut state = ClusterState::default();
    state.peer_heartbeat_acks.insert("b".to_string(), 100);

    assert!(state_has_fresh_quorum(&state, &cfg, 110));
    assert!(!state_has_fresh_quorum(&state, &cfg, 111));
}

#[test]
fn remote_heartbeat_sender_local_flag_is_not_trusted() {
    let cfg = test_config("local", 0);
    let now = now_secs();
    let mut node = empty_node("remote", "remote", "http://remote", 1, true, now);
    node.draining = true;
    node.health = ClusterHealth::unhealthy("draining", false, false);

    update_node(node, &cfg.cluster.node_id);

    let stored = cluster_state_read()
        .nodes
        .get("remote")
        .cloned()
        .expect("remote node should be stored");
    assert!(!stored.is_local);
    assert!(stored.draining);
    assert_eq!(stored.health.reason, "draining");

    cluster_state_write().nodes.remove("remote");
}

#[test]
fn heartbeat_response_requires_peer_identity_and_snapshot() {
    let mut cfg = test_config("local", 0);
    cfg.cluster.failover_timeout_secs = 15;
    let now = now_secs();
    let mut peer = empty_node("peer", "peer", "http://peer", 1, false, now);
    peer.last_seen = Some(now);
    let valid = ClusterStatus {
        enabled: true,
        local_node_id: "peer".to_string(),
        active_owner: Some("peer".to_string()),
        lease_until: Some(now + 10),
        config_version: String::new(),
        auto_failover: true,
        nodes: vec![peer],
    };

    assert!(heartbeat_response_is_valid(&valid, "peer"));

    let mut wrong_identity = valid.clone();
    wrong_identity.local_node_id = "other".to_string();
    assert!(!heartbeat_response_is_valid(&wrong_identity, "peer"));

    let mut skewed = valid.clone();
    skewed.nodes[0].last_seen = Some(now - cfg.cluster.failover_timeout_secs - 1);
    assert!(heartbeat_response_is_valid(&skewed, "peer"));

    let mut missing_snapshot = valid;
    missing_snapshot.nodes[0].node_id = "other".to_string();
    assert!(!heartbeat_response_is_valid(&missing_snapshot, "peer"));
}

#[test]
fn record_heartbeat_ignores_unconfigured_nodes() {
    let cfg = test_config("local", 0);
    let now = now_secs();
    let node = empty_node("unknown", "unknown", "http://unknown", 1, false, now);

    assert!(!record_heartbeat(&cfg, node));
    assert!(!cluster_state_read().nodes.contains_key("unknown"));
}

#[test]
fn record_heartbeat_uses_locally_configured_membership_metadata() {
    let _guard = ClusterStateGuard::new();
    let mut cfg = test_config("local", 0);
    cfg.cluster.peers = vec![crate::config::ClusterPeer {
        node_id: "peer".to_string(),
        name: "Configured peer".to_string(),
        api_url: "https://configured.invalid".to_string(),
        priority: 7,
    }];
    let mut node = empty_node(
        "peer",
        "Forged",
        "https://forged.invalid",
        999,
        false,
        now_secs(),
    );
    node.health = ClusterHealth::healthy();

    assert!(record_heartbeat(&cfg, node));

    let state = cluster_state_read();
    let stored = state.nodes.get("peer").expect("peer should be stored");
    assert_eq!(stored.name, "Configured peer");
    assert_eq!(stored.api_url, "https://configured.invalid");
    assert_eq!(stored.priority, 7);
}

#[test]
fn record_heartbeat_ignores_when_cluster_disabled() {
    let mut cfg = test_config("local", 0);
    cfg.cluster.enabled = false;
    let peer_id = "disabled-peer";
    cfg.cluster.peers = vec![crate::config::ClusterPeer {
        node_id: peer_id.to_string(),
        name: peer_id.to_string(),
        api_url: format!("http://{}", peer_id),
        priority: 1,
    }];
    let now = now_secs();
    cluster_state_write().nodes.remove(peer_id);
    let node = empty_node(
        peer_id,
        peer_id,
        &format!("http://{}", peer_id),
        1,
        false,
        now,
    );

    assert!(!record_heartbeat(&cfg, node));
    assert!(!cluster_state_read().nodes.contains_key(peer_id));
}

#[test]
fn record_heartbeat_rejects_local_node_identity() {
    let _guard = ClusterStateGuard::new();
    let cfg = test_config("local", 0);
    let now = now_secs();
    let local = empty_node("local", "local", "http://local", 10, true, now);
    update_node(local, &cfg.cluster.node_id);

    let mut spoofed = empty_node("local", "spoofed", "http://spoofed", 99, false, now);
    spoofed.draining = true;
    spoofed.health = ClusterHealth::unhealthy("spoofed", false, false);

    assert!(!record_heartbeat(&cfg, spoofed));

    let stored = cluster_state_read()
        .nodes
        .get("local")
        .cloned()
        .expect("local node should remain stored");
    assert_eq!(stored.name, "local");
    assert_eq!(stored.api_url, "http://local");
    assert_eq!(stored.priority, 10);
    assert!(!stored.draining);
    assert_ne!(stored.health.reason, "spoofed");
}

#[test]
fn auto_failover_adoption_requires_active_owner_change() {
    let mut cfg = test_config("local", 0);
    cfg.cluster.auto_failover = false;

    assert!(should_adopt_auto_failover_from_peer(true, true, &cfg));
    assert!(!should_adopt_auto_failover_from_peer(false, true, &cfg));
    assert!(!should_adopt_auto_failover_from_peer(true, false, &cfg));

    cfg.cluster.enabled = false;
    assert!(!should_adopt_auto_failover_from_peer(true, true, &cfg));
}

#[test]
fn older_snapshots_do_not_replace_newer_snapshots() {
    let mut newer = empty_node("peer", "peer", "http://peer", 1, false, 100);
    newer.last_seen = Some(100);
    let mut older = empty_node("peer", "peer", "http://peer", 1, false, 99);
    older.last_seen = Some(99);

    assert!(snapshot_is_older(&older, &newer));
    assert!(!snapshot_is_older(&newer, &older));
}

#[test]
fn stale_active_snapshot_does_not_overwrite_cached_active_state() {
    let _guard = ClusterStateGuard::new();
    let now = now_secs();
    let current_toggles = MonitorToggleState {
        enable_youtube_monitor: true,
        youtube_enable_monitor: true,
        ..all_monitor_toggles_off()
    };
    let stale_toggles = MonitorToggleState {
        enable_twitch_monitor: true,
        twitch_enable_monitor: true,
        ..all_monitor_toggles_off()
    };
    let current_targets = ChannelTargetState {
        youtube_channel_name: "current".to_string(),
        youtube_channel_id: "current-yt".to_string(),
        ..ChannelTargetState::default()
    };
    let stale_targets = ChannelTargetState {
        twitch_channel_name: "stale".to_string(),
        twitch_channel_id: "stale-tw".to_string(),
        ..ChannelTargetState::default()
    };

    let mut current_owner = empty_node("owner", "owner", "http://owner", 10, false, now);
    current_owner.last_seen = Some(now);
    current_owner.monitor_toggles = current_toggles.clone();
    current_owner.channel_targets = current_targets.clone();

    let mut stale_owner = current_owner.clone();
    stale_owner.last_seen = Some(now.saturating_sub(10));
    stale_owner.monitor_toggles = stale_toggles;
    stale_owner.channel_targets = stale_targets;

    {
        let mut state = cluster_state_write();
        *state = ClusterState::default();
        state.active_owner = Some("owner".to_string());
        state.lease_until = 1_000;
        state.nodes.insert("owner".to_string(), current_owner);
        state.last_known_active_toggles = Some(current_toggles.clone());
        state.last_known_active_channel_targets = Some(current_targets.clone());
    }

    merge_cluster_status_inner(
        ClusterStatus {
            enabled: true,
            local_node_id: "peer".to_string(),
            active_owner: Some("owner".to_string()),
            lease_until: Some(900),
            config_version: String::new(),
            auto_failover: true,
            nodes: vec![stale_owner],
        },
        None,
        None,
    );

    let state = cluster_state_read();
    assert_eq!(state.active_owner.as_deref(), Some("owner"));
    assert_eq!(state.lease_until, 1_000);
    assert_eq!(state.last_known_active_toggles, Some(current_toggles));
    assert_eq!(
        state.last_known_active_channel_targets,
        Some(current_targets)
    );
}

#[test]
fn unknown_active_snapshot_does_not_overwrite_cached_monitor_state() {
    let now = now_secs();
    let cached_toggles = MonitorToggleState {
        enable_youtube_monitor: true,
        youtube_enable_monitor: true,
        ..all_monitor_toggles_off()
    };
    let cached_targets = ChannelTargetState {
        youtube_channel_name: "cached".to_string(),
        youtube_channel_id: "cached-yt".to_string(),
        ..ChannelTargetState::default()
    };

    let mut state = ClusterState {
        active_owner: Some("owner".to_string()),
        last_known_active_toggles: Some(cached_toggles.clone()),
        last_known_active_channel_targets: Some(cached_targets.clone()),
        ..ClusterState::default()
    };
    let mut owner = empty_node("owner", "owner", "http://owner", 10, false, now);
    owner.last_seen = None;
    owner.monitor_toggles = all_monitor_toggles_off();
    owner.channel_targets = ChannelTargetState::default();
    state.nodes.insert("owner".to_string(), owner);

    cache_current_owner_monitor_state(&mut state);

    assert_eq!(state.last_known_active_toggles, Some(cached_toggles));
    assert_eq!(
        state.last_known_active_channel_targets,
        Some(cached_targets)
    );
}

#[test]
fn known_active_all_off_snapshot_updates_cached_monitor_state() {
    let now = now_secs();
    let cached_toggles = MonitorToggleState {
        enable_youtube_monitor: true,
        youtube_enable_monitor: true,
        ..all_monitor_toggles_off()
    };
    let mut state = ClusterState {
        active_owner: Some("owner".to_string()),
        last_known_active_toggles: Some(cached_toggles),
        ..ClusterState::default()
    };
    let mut owner = empty_node("owner", "owner", "http://owner", 10, false, now);
    owner.last_seen = Some(now);
    owner.monitor_toggles = all_monitor_toggles_off();
    state.nodes.insert("owner".to_string(), owner);

    cache_current_owner_monitor_state(&mut state);

    assert_eq!(
        state.last_known_active_toggles,
        Some(all_monitor_toggles_off())
    );
}

#[test]
fn heartbeat_merge_refreshes_only_direct_peer_liveness() {
    let mut existing_c = empty_node("c", "c", "http://c", 1, false, 100);
    existing_c.last_seen = Some(100);

    assert_eq!(
        merged_status_last_seen("b", Some("b"), None, 200),
        Some(200)
    );
    assert_eq!(
        merged_status_last_seen("c", Some("b"), Some(&existing_c), 200),
        Some(100)
    );
    assert_eq!(merged_status_last_seen("c", Some("b"), None, 200), None);
}

#[test]
fn direct_peer_status_merge_refreshes_peer_liveness() {
    let peer_id = "direct-merge-peer";
    let mut cfg = test_config("direct-merge-local", 0);
    cfg.cluster.peers = vec![crate::config::ClusterPeer {
        node_id: peer_id.to_string(),
        name: peer_id.to_string(),
        api_url: "http://direct-merge-peer".to_string(),
        priority: 1,
    }];
    let now = now_secs();

    {
        let mut state = cluster_state_write();
        let mut stale_peer =
            empty_node(peer_id, peer_id, "http://direct-merge-peer", 1, false, now);
        stale_peer.last_seen = Some(now - cfg.cluster.failover_timeout_secs - 1);
        state.nodes.insert(peer_id.to_string(), stale_peer);
        state
            .heartbeat_failures
            .insert(peer_id.to_string(), HEARTBEAT_FAILURE_THRESHOLD);
    }

    let mut peer_snapshot = empty_node(peer_id, peer_id, "http://direct-merge-peer", 1, false, now);
    peer_snapshot.last_seen = Some(now);
    peer_snapshot.health = ClusterHealth::healthy();
    let status = ClusterStatus {
        enabled: true,
        local_node_id: peer_id.to_string(),
        active_owner: Some(peer_id.to_string()),
        lease_until: Some(now + 10),
        config_version: String::new(),
        auto_failover: true,
        nodes: vec![peer_snapshot],
    };

    merge_cluster_status_from_direct_peer(status, peer_id, &cfg)
        .expect("direct peer status should merge");

    let state = cluster_state_read();
    let stored = state
        .nodes
        .get(peer_id)
        .cloned()
        .expect("peer should be stored");
    assert!(!is_stale(&stored, &cfg, now_secs()));
    assert!(!state.heartbeat_failures.contains_key(peer_id));
    drop(state);

    let mut state = cluster_state_write();
    state.nodes.remove(peer_id);
    state.heartbeat_failures.remove(peer_id);
}

#[test]
fn peer_observation_prune_drops_stale_and_unconfigured_entries() {
    let mut cfg = test_config("a", 1);
    cfg.cluster.failover_timeout_secs = 15;
    cfg.cluster.peers = vec![
        crate::config::ClusterPeer {
            node_id: "b".to_string(),
            name: "b".to_string(),
            api_url: "http://b".to_string(),
            priority: 10,
        },
        crate::config::ClusterPeer {
            node_id: "c".to_string(),
            name: "c".to_string(),
            api_url: "http://c".to_string(),
            priority: 5,
        },
    ];
    let now = now_secs();
    let mut state = ClusterState::default();
    state.peer_observations.insert(
        "b".to_string(),
        HashMap::from([
            ("a".to_string(), now - cfg.cluster.failover_timeout_secs - 1),
            ("c".to_string(), now),
            ("removed-observer".to_string(), now),
        ]),
    );
    state.peer_observations.insert(
        "removed-target".to_string(),
        HashMap::from([("c".to_string(), now)]),
    );

    let configured = configured_node_ids(&cfg);
    prune_peer_observations(&mut state, &cfg, now, &configured);

    assert!(!state.peer_observations.contains_key("removed-target"));
    let observations = state
        .peer_observations
        .get("b")
        .expect("configured target should keep fresh configured observer");
    assert_eq!(observations.len(), 1);
    assert_eq!(observations.get("c"), Some(&now));
}

#[test]
fn peer_observation_record_drops_empty_observer_buckets() {
    let mut cfg = test_config("a", 1);
    cfg.cluster.peers = vec![
        crate::config::ClusterPeer {
            node_id: "b".to_string(),
            name: "b".to_string(),
            api_url: "http://b".to_string(),
            priority: 10,
        },
        crate::config::ClusterPeer {
            node_id: "c".to_string(),
            name: "c".to_string(),
            api_url: "http://c".to_string(),
            priority: 5,
        },
    ];
    let now = now_secs();
    let mut state = ClusterState::default();
    state
        .peer_observations
        .insert("b".to_string(), HashMap::from([("c".to_string(), now - 1)]));
    state.peer_observations.insert(
        "removed-target".to_string(),
        HashMap::from([("c".to_string(), now - 1)]),
    );

    record_peer_observations(&mut state, "c", &[], &cfg, now);

    assert!(state.peer_observations.is_empty());
}

#[test]
fn never_seen_peer_waits_without_timeout_fault() {
    let cfg = test_config("a", 0);
    let now = now_secs();
    let mut state = ClusterState::default();
    state.nodes.insert(
        "b".to_string(),
        empty_node("b", "b", "http://b", 1, false, now),
    );

    normalize_node_health(&mut state, &cfg, now + 1_000);

    let node = state.nodes.get("b").unwrap();
    assert!(!node.health.healthy);
    assert_eq!(node.health.reason, "waiting_for_heartbeat");
    assert!(!is_stale(node, &cfg, now + 1_000));
}

#[test]
fn external_api_threshold_uses_windowed_failure_count() {
    let mut cfg = test_config("a", 0);
    cfg.cluster.thresholds.max_external_api_failures = 3;

    assert!(!external_api_degraded(&cfg.cluster, 2));
    assert!(external_api_degraded(&cfg.cluster, 3));
}

#[test]
fn external_api_failure_window_prunes_old_failures() {
    let now = 10_000;
    let window = 60;
    let mut failures = vec![now - window - 1, now - window, now - 5];

    prune_recent_times(&mut failures, now, window);

    assert_eq!(failures, vec![now - window, now - 5]);
}

#[test]
fn local_network_isolation_requires_external_api_and_majority_peer_failures() {
    let mut cfg = test_config("a", 0);
    cfg.cluster.peers = vec![
        crate::config::ClusterPeer {
            node_id: "b".to_string(),
            name: "b".to_string(),
            api_url: "http://b".to_string(),
            priority: 5,
        },
        crate::config::ClusterPeer {
            node_id: "c".to_string(),
            name: "c".to_string(),
            api_url: "http://c".to_string(),
            priority: 10,
        },
        crate::config::ClusterPeer {
            node_id: "d".to_string(),
            name: "d".to_string(),
            api_url: "http://d".to_string(),
            priority: 15,
        },
    ];
    let mut failures = HashMap::new();
    failures.insert("b".to_string(), HEARTBEAT_FAILURE_THRESHOLD);
    let peer_last_seen = HashMap::new();
    let now = now_secs();

    assert!(!local_network_isolated(
        &cfg.cluster,
        &failures,
        &peer_last_seen,
        cfg.cluster.thresholds.max_external_api_failures,
        now
    ));

    failures.insert("c".to_string(), HEARTBEAT_FAILURE_THRESHOLD);
    assert!(!local_network_isolated(
        &cfg.cluster,
        &failures,
        &peer_last_seen,
        0,
        now
    ));
    assert!(local_network_isolated(
        &cfg.cluster,
        &failures,
        &peer_last_seen,
        cfg.cluster.thresholds.max_external_api_failures,
        now
    ));
}

#[test]
fn two_node_link_failure_without_external_api_degradation_does_not_isolate_local() {
    let mut cfg = test_config("a", 0);
    cfg.cluster.peers = vec![crate::config::ClusterPeer {
        node_id: "b".to_string(),
        name: "b".to_string(),
        api_url: "http://b".to_string(),
        priority: 5,
    }];

    let now = now_secs();
    let mut failures = HashMap::new();
    failures.insert("b".to_string(), HEARTBEAT_FAILURE_THRESHOLD);
    let peer_last_seen = HashMap::new();

    assert!(!local_network_isolated(
        &cfg.cluster,
        &failures,
        &peer_last_seen,
        0,
        now
    ));
    assert!(local_network_isolated(
        &cfg.cluster,
        &failures,
        &peer_last_seen,
        cfg.cluster.thresholds.max_external_api_failures,
        now
    ));
}

#[test]
fn manual_mode_two_node_link_failure_keeps_original_local_owner() {
    let mut cfg = test_config("a", 1);
    cfg.cluster.auto_failover = false;
    cfg.cluster.peers = vec![crate::config::ClusterPeer {
        node_id: "b".to_string(),
        name: "b".to_string(),
        api_url: "http://b".to_string(),
        priority: 10,
    }];

    let now = now_secs();
    let mut heartbeat_failures = HashMap::new();
    heartbeat_failures.insert("b".to_string(), HEARTBEAT_FAILURE_THRESHOLD);
    let mut peer_last_seen = HashMap::new();
    peer_last_seen.insert(
        "b".to_string(),
        Some(now - cfg.cluster.failover_timeout_secs - 1),
    );

    assert!(!local_network_isolated(
        &cfg.cluster,
        &heartbeat_failures,
        &peer_last_seen,
        0,
        now
    ));

    let mut state = ClusterState {
        active_owner: Some("a".to_string()),
        ..ClusterState::default()
    };
    state.nodes.insert(
        "a".to_string(),
        empty_node("a", "a", "http://a", 1, true, now),
    );
    state.nodes.insert(
        "b".to_string(),
        empty_node("b", "b", "http://b", 10, false, now),
    );
    state.nodes.get_mut("a").unwrap().last_seen = Some(now);
    state.nodes.get_mut("a").unwrap().health = ClusterHealth::healthy();
    state.nodes.get_mut("b").unwrap().last_seen = Some(now - cfg.cluster.failover_timeout_secs - 1);
    state.nodes.get_mut("b").unwrap().health =
        ClusterHealth::unhealthy("heartbeat_timeout", true, false);

    assert_eq!(choose_owner(&state, &cfg, now), Some("a".to_string()));
}

#[test]
fn recovered_network_isolation_latch_clears_only_fault_quarantine() {
    let mut state = ClusterState {
        local_fault_latched: true,
        local_fault_reason: Some(NETWORK_ISOLATED_REASON.to_string()),
        ..ClusterState::default()
    };

    assert!(clear_recovered_local_network_isolation(&mut state, false));
    assert!(!state.local_fault_latched);
    assert_eq!(state.local_fault_reason, None);
}

#[test]
fn fault_fencing_follows_auto_failover_setting() {
    let mut cfg = test_config("a", 1);
    assert!(fault_fencing_enabled(&cfg));

    cfg.cluster.auto_failover = false;
    assert!(!fault_fencing_enabled(&cfg));
}

#[test]
fn monitoring_block_reason_names_the_latched_fault() {
    let cfg = test_config("a", 1);
    let state = ClusterState {
        active_owner: Some("a".to_string()),
        local_fault_latched: true,
        local_fault_reason: Some("ffmpeg_repeated_failures".to_string()),
        ..ClusterState::default()
    };

    let reason = state_monitoring_block_reason(&state, &cfg, now_secs())
        .expect("latched fault blocks monitoring");
    assert!(reason.contains("ffmpeg_repeated_failures"), "{}", reason);
    assert!(!reason.contains("多数派"), "{}", reason);
}

#[test]
fn monitoring_block_reason_is_none_for_healthy_single_node_owner() {
    let cfg = test_config("a", 1);
    let state = ClusterState {
        active_owner: Some("a".to_string()),
        ..ClusterState::default()
    };

    assert_eq!(
        state_monitoring_block_reason(&state, &cfg, now_secs()),
        None
    );
}

#[test]
fn network_isolation_latch_stays_while_evidence_remains() {
    let mut state = ClusterState {
        local_fault_latched: true,
        local_fault_reason: Some(NETWORK_ISOLATED_REASON.to_string()),
        ..ClusterState::default()
    };

    assert!(!clear_recovered_local_network_isolation(&mut state, true));
    assert!(state.local_fault_latched);
    assert_eq!(
        state.local_fault_reason.as_deref(),
        Some(NETWORK_ISOLATED_REASON)
    );
}

#[test]
fn four_node_last_survivor_stays_healthy_and_active() {
    let mut cfg = test_config("a", 1);
    cfg.cluster.peers = vec![
        crate::config::ClusterPeer {
            node_id: "b".to_string(),
            name: "b".to_string(),
            api_url: "http://b".to_string(),
            priority: 10,
        },
        crate::config::ClusterPeer {
            node_id: "c".to_string(),
            name: "c".to_string(),
            api_url: "http://c".to_string(),
            priority: 5,
        },
        crate::config::ClusterPeer {
            node_id: "d".to_string(),
            name: "d".to_string(),
            api_url: "http://d".to_string(),
            priority: 3,
        },
    ];
    let now = now_secs();
    let mut state = ClusterState::default();
    let mut local = empty_node("a", "a", "http://a", 1, true, now);
    local.health = ClusterHealth::healthy();
    state.nodes.insert("a".to_string(), local);

    let mut heartbeat_failures = HashMap::new();
    for peer in &cfg.cluster.peers {
        let mut peer_node = empty_node(
            &peer.node_id,
            &peer.name,
            &peer.api_url,
            peer.priority,
            false,
            now,
        );
        peer_node.last_seen = Some(now - cfg.cluster.failover_timeout_secs - 1);
        state.nodes.insert(peer.node_id.clone(), peer_node);
        heartbeat_failures.insert(peer.node_id.clone(), HEARTBEAT_FAILURE_THRESHOLD);
    }

    normalize_node_health(&mut state, &cfg, now);

    let peer_last_seen = state
        .nodes
        .iter()
        .map(|(node_id, node)| (node_id.clone(), node.last_seen))
        .collect::<HashMap<_, _>>();
    assert!(!local_network_isolated(
        &cfg.cluster,
        &heartbeat_failures,
        &peer_last_seen,
        0,
        now
    ));
    let local = state.nodes.get("a").unwrap();
    assert!(local.health.healthy);
    assert_eq!(choose_owner(&state, &cfg, now), Some("a".to_string()));
}

#[test]
fn local_network_isolation_ignores_fresh_inbound_heartbeats() {
    let mut cfg = test_config("a", 0);
    cfg.cluster.peers = vec![
        crate::config::ClusterPeer {
            node_id: "b".to_string(),
            name: "b".to_string(),
            api_url: "http://b".to_string(),
            priority: 5,
        },
        crate::config::ClusterPeer {
            node_id: "c".to_string(),
            name: "c".to_string(),
            api_url: "http://c".to_string(),
            priority: 10,
        },
        crate::config::ClusterPeer {
            node_id: "d".to_string(),
            name: "d".to_string(),
            api_url: "http://d".to_string(),
            priority: 15,
        },
    ];
    let mut failures = HashMap::new();
    failures.insert("b".to_string(), HEARTBEAT_FAILURE_THRESHOLD);
    failures.insert("c".to_string(), HEARTBEAT_FAILURE_THRESHOLD);
    let now = now_secs();
    let mut peer_last_seen = HashMap::new();
    peer_last_seen.insert("b".to_string(), Some(now));

    let mut state = ClusterState::default();
    state.nodes.insert(
        "b".to_string(),
        empty_node("b", "b", "http://b", 5, false, now),
    );
    state.nodes.get_mut("b").unwrap().last_seen = Some(now);
    state
        .heartbeat_failures
        .insert("b".to_string(), HEARTBEAT_FAILURE_THRESHOLD);
    state
        .heartbeat_failures
        .insert("c".to_string(), HEARTBEAT_FAILURE_THRESHOLD);

    assert!(!local_network_isolated(
        &cfg.cluster,
        &failures,
        &peer_last_seen,
        cfg.cluster.thresholds.max_external_api_failures,
        now
    ));
    assert!(!local_network_isolated_from_state(
        &cfg.cluster,
        &state,
        cfg.cluster.thresholds.max_external_api_failures,
        now
    ));

    peer_last_seen.insert(
        "b".to_string(),
        Some(now - cfg.cluster.failover_timeout_secs - 1),
    );
    state.nodes.get_mut("b").unwrap().last_seen = Some(now - cfg.cluster.failover_timeout_secs - 1);
    assert!(local_network_isolated(
        &cfg.cluster,
        &failures,
        &peer_last_seen,
        cfg.cluster.thresholds.max_external_api_failures,
        now
    ));
    assert!(local_network_isolated_from_state(
        &cfg.cluster,
        &state,
        cfg.cluster.thresholds.max_external_api_failures,
        now
    ));
}

#[test]
fn last_known_active_toggles_preserves_all_off_cache() {
    let previous = MonitorToggleState {
        enable_danmaku_command: true,
        enable_youtube_monitor: true,
        enable_twitch_monitor: true,
        youtube_enable_monitor: true,
        twitch_enable_monitor: true,
        priority_channel_enabled: true,
        priority_channel_auto_restart: true,
    };

    {
        let mut state = cluster_state_write();
        state.last_known_active_toggles = Some(previous.clone());
    }

    assert_eq!(last_known_active_toggles(), Some(previous));

    {
        let mut state = cluster_state_write();
        state.last_known_active_toggles = Some(all_monitor_toggles_off());
    }
    assert_eq!(last_known_active_toggles(), Some(all_monitor_toggles_off()));
}

#[test]
fn unreachable_peer_with_fresh_inbound_heartbeat_is_not_marked_unhealthy() {
    let _guard = ClusterStateGuard::new();
    let node_id = "fresh-inbound-guard-peer";
    let mut cfg = test_config("guard-local", 0);
    cfg.cluster.peers = vec![crate::config::ClusterPeer {
        node_id: node_id.to_string(),
        name: node_id.to_string(),
        api_url: "http://fresh-inbound".to_string(),
        priority: 1,
    }];

    let now = now_secs();
    {
        let mut state = cluster_state_write();
        let mut node = empty_node(node_id, node_id, "http://fresh-inbound", 1, false, now);
        node.last_seen = Some(now);
        node.health = ClusterHealth::healthy();
        state.nodes.insert(node_id.to_string(), node);
        state.heartbeat_failures.remove(node_id);
    }

    for _ in 0..(HEARTBEAT_FAILURE_THRESHOLD + 1) {
        mark_peer_unreachable(node_id, &cfg);
    }

    let stored = cluster_state_read()
        .nodes
        .get(node_id)
        .cloned()
        .expect("peer should exist");
    assert!(
        stored.health.healthy,
        "peer with fresh inbound heartbeat should stay healthy, got: {}",
        stored.health.reason
    );

    {
        let mut state = cluster_state_write();
        if let Some(node) = state.nodes.get_mut(node_id) {
            node.last_seen = Some(
                now_secs()
                    .saturating_sub(cfg.cluster.failover_timeout_secs)
                    .saturating_sub(5),
            );
        }
    }
    mark_peer_unreachable(node_id, &cfg);

    let stored = cluster_state_read()
        .nodes
        .get(node_id)
        .cloned()
        .expect("peer should exist");
    assert!(!stored.health.healthy);
    assert_eq!(stored.health.reason, "api_unreachable");
}

#[tokio::test]
async fn standby_denied_push_does_not_keep_candidate_stream() {
    let _guard = ClusterStateGuard::new();
    let peer_id = "active-peer";
    let mut cfg = test_config("standby-local", 0);
    cfg.cluster.auto_failover = false;
    cfg.cluster.peers = vec![crate::config::ClusterPeer {
        node_id: peer_id.to_string(),
        name: peer_id.to_string(),
        api_url: "http://active-peer".to_string(),
        priority: 1,
    }];

    {
        let now = now_secs();
        let mut state = cluster_state_write();
        state.active_owner = Some(peer_id.to_string());
        state.nodes.insert(
            peer_id.to_string(),
            empty_node(peer_id, peer_id, "http://active-peer", 1, false, now),
        );
    }

    let stream = ClusterStreamIdentity {
        platform: "YT".to_string(),
        channel_name: "standby-channel".to_string(),
        channel_id: "standby-channel-id".to_string(),
        stream_id: Some("video-id".to_string()),
        title: Some("standby title".to_string()),
    };

    assert!(!local_may_push(&cfg, Some(stream)));
    assert_eq!(cluster_state_read().local_stream, None);
}

#[test]
fn cached_toggles_fallback_when_previous_owner_snapshot_missing() {
    let cached = MonitorToggleState {
        enable_danmaku_command: true,
        enable_youtube_monitor: true,
        enable_twitch_monitor: true,
        youtube_enable_monitor: true,
        twitch_enable_monitor: true,
        priority_channel_enabled: true,
        priority_channel_auto_restart: false,
    };

    {
        let mut state = cluster_state_write();
        state.last_known_active_toggles = Some(cached.clone());
    }

    let from_cache = last_known_active_toggles().expect("cached toggles should be available");
    assert_eq!(from_cache, cached);
}

#[test]
fn monitoring_requires_local_ownership_and_healthy_runtime() {
    let _guard = ClusterStateGuard::new();
    let cfg = test_config("a", 1);

    {
        let mut state = cluster_state_write();
        state.active_owner = Some("b".to_string());
    }
    assert!(!local_node_is_active_owner(&cfg));
    assert!(!local_monitoring_allowed(&cfg));

    cluster_state_write().active_owner = Some("a".to_string());
    assert!(local_node_is_active_owner(&cfg));
    assert!(local_monitoring_allowed(&cfg));

    cluster_state_write().local_draining = true;
    assert!(local_node_is_active_owner(&cfg));
    assert!(!local_monitoring_allowed(&cfg));
    {
        let mut state = cluster_state_write();
        state.local_draining = false;
        state.local_fault_latched = true;
    }
    assert!(local_node_is_active_owner(&cfg));
    assert!(!local_monitoring_allowed(&cfg));

    let mut standalone = cfg;
    standalone.cluster.enabled = false;
    assert!(local_node_is_active_owner(&standalone));
    assert!(local_monitoring_allowed(&standalone));
}

#[test]
fn drained_handoff_target_is_rejected_before_side_effects() {
    let _guard = ClusterStateGuard::new();
    let mut cfg = test_config("source", 10);
    cfg.cluster.peers = vec![crate::config::ClusterPeer {
        node_id: "target".to_string(),
        name: "target".to_string(),
        api_url: "http://target".to_string(),
        priority: 5,
    }];
    let now = now_secs();
    let mut target = empty_node("target", "target", "http://target", 5, false, now);
    target.health = ClusterHealth::healthy();
    target.last_seen = Some(now);
    target.draining = true;
    cluster_state_write()
        .nodes
        .insert("target".to_string(), target);

    assert!(ensure_handoff_target_is_eligible(&cfg, "target").is_err());
}

#[test]
fn freshly_enabled_handoff_target_becomes_eligible_immediately() {
    let _guard = ClusterStateGuard::new();
    let mut cfg = test_config("source", 10);
    cfg.cluster.peers = vec![crate::config::ClusterPeer {
        node_id: "target".to_string(),
        name: "target".to_string(),
        api_url: "http://target".to_string(),
        priority: 5,
    }];
    let now = now_secs();
    let mut target = empty_node("target", "target", "http://target", 5, false, now);
    target.last_seen = Some(now);
    target.draining = true;
    target.network_unstable = false;
    target.health = ClusterHealth::unhealthy("draining", false, false);
    cluster_state_write()
        .nodes
        .insert("target".to_string(), target);

    set_drain_state(&cfg, Some("target".to_string()), false);

    assert!(ensure_handoff_target_is_eligible(&cfg, "target").is_ok());
    assert!(cluster_state_read().nodes["target"].health.healthy);
}
