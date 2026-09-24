use super::*;
use crate::config::{
    BiliLive, ClusterConfig, ClusterHealthThresholds, Config, Credentials, FfmpegCache,
    PriorityChannel, Twitch, Youtube,
};
use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

static CLUSTER_STATE_TEST_LOCK: Mutex<()> = Mutex::new(());

fn peer_view(owner: &str, members: &[&str], now: u64) -> PeerOwnerView {
    PeerOwnerView {
        owner: Some(owner.to_string()),
        received_at: now,
        members: members.iter().map(|member| member.to_string()).collect(),
        confirmed_by_heartbeat: true,
    }
}

#[test]
fn competing_healthy_owners_cannot_both_pass_the_execution_gate() {
    let now = 100;
    for (local, other) in [("a", "b"), ("b", "a")] {
        let mut cfg = test_config(local, 0);
        cfg.cluster.peers = [other, "c"]
            .into_iter()
            .map(|id| crate::config::ClusterPeer {
                node_id: id.to_string(),
                name: id.to_string(),
                api_url: format!("http://{id}"),
                priority: 0,
            })
            .collect();
        let mut state = ClusterState {
            active_owner: Some(local.to_string()),
            ..Default::default()
        };
        for id in ["a", "b", "c"] {
            let mut node = empty_node(id, id, "", if id == "a" { 10 } else { 0 }, id == local, now);
            node.last_seen = Some(now);
            node.health = ClusterHealth::healthy();
            state.nodes.insert(id.to_string(), node);
        }
        for peer in [other, "c"] {
            state.peer_heartbeat_acks.insert(peer.to_string(), now);
        }
        state
            .peer_owner_views
            .insert(other.to_string(), peer_view(other, &["a", "b", "c"], now));
        state
            .peer_owner_views
            .insert("c".to_string(), peer_view(local, &["a", "b", "c"], now));
        assert!(state_has_fresh_quorum(&state, &cfg, now));
        assert!(!state_has_owner_agreement(&state, &cfg, now));
        assert!(state_monitoring_block_reason(&state, &cfg, now).is_some());
        assert_eq!(choose_owner(&state, &cfg, now).as_deref(), Some("a"));
    }
}

#[test]
fn owner_confirmation_expires_and_cannot_cross_membership_changes() {
    let mut cfg = test_config("a", 0);
    cfg.cluster.failover_timeout_secs = 10;
    cfg.cluster.peers = vec![crate::config::ClusterPeer {
        node_id: "b".into(),
        name: "b".into(),
        api_url: "http://b".into(),
        priority: 0,
    }];
    let mut state = ClusterState {
        active_owner: Some("a".into()),
        ..Default::default()
    };
    state.peer_heartbeat_acks.insert("b".into(), 100);
    assert!(!state_has_owner_agreement(&state, &cfg, 100));
    state
        .peer_owner_views
        .insert("b".into(), peer_view("a", &["a", "b"], 100));
    assert!(state_has_owner_agreement(&state, &cfg, 110));
    assert!(!state_has_owner_agreement(&state, &cfg, 111));
    assert!(!state_has_owner_agreement(&state, &cfg, 99));
    state
        .peer_owner_views
        .get_mut("b")
        .unwrap()
        .confirmed_by_heartbeat = false;
    assert!(!state_has_owner_agreement(&state, &cfg, 100));
    state
        .peer_owner_views
        .insert("b".into(), peer_view("a", &["a", "b", "c"], 100));
    assert!(!state_has_owner_agreement(&state, &cfg, 100));
    state.peer_heartbeat_acks.clear();
    state.peer_heartbeat_acks.insert("a".into(), 100);
    assert!(!state_has_fresh_quorum(&state, &cfg, 100));
}

#[test]
fn a_new_local_owner_waits_for_handoff_even_with_agreement() {
    let cfg = test_config("a", 10);
    let now = 100;
    let mut state = ClusterState {
        active_owner: Some("old".into()),
        ..Default::default()
    };
    let mut local = empty_node("a", "a", "", 10, true, now);
    local.last_seen = Some(now);
    local.health = ClusterHealth::healthy();
    state.nodes.insert("a".into(), local);
    build_status_from_state(&mut state, &cfg, String::new(), now);
    assert_eq!(state.active_owner.as_deref(), Some("a"));
    assert_eq!(state.pending_handoff_source.as_deref(), Some("old"));
    assert!(state_monitoring_block_reason(&state, &cfg, now).is_some());
}

fn startup_monitoring_fixture() -> (Config, ClusterStatus) {
    let mut cfg = test_config("jp", 100);
    cfg.cluster.heartbeat_interval_secs = 15;
    cfg.cluster.peers = ["us", "eu"]
        .into_iter()
        .map(|id| crate::config::ClusterPeer {
            node_id: id.into(),
            name: id.into(),
            api_url: format!("http://{id}"),
            priority: 50,
        })
        .collect();
    let now = now_secs();
    let mut state = ClusterState {
        active_owner: Some("jp".into()),
        ..Default::default()
    };
    for id in ["jp", "us", "eu"] {
        let mut node = empty_node(
            id,
            id,
            &format!("http://{id}"),
            if id == "jp" { 100 } else { 50 },
            id == "jp",
            now,
        );
        node.last_seen = Some(now);
        node.health = ClusterHealth::healthy();
        state.nodes.insert(id.into(), node);
    }
    *cluster_state_write() = state;
    let mut peer_status = compute_cluster_status(&cfg);
    peer_status.local_node_id = "us".into();
    (cfg, peer_status)
}

#[tokio::test]
async fn startup_monitor_wakes_on_quorum_before_the_next_heartbeat_interval() {
    let _guard = ClusterStateGuard::new();
    let (cfg, peer_status) = startup_monitoring_fixture();
    let signature = cluster_ui_signature(&compute_cluster_status(&cfg));
    let ready = wait_for_local_monitoring(&cfg);
    tokio::pin!(ready);
    assert!(futures_util::poll!(&mut ready).is_pending());

    let started = std::time::Instant::now();
    // One direct, agreeing peer plus JP is a majority. EU can still be slow;
    // do not wait for the worker's full fan-out or its next 15-second tick.
    merge_direct_peer_status(peer_status, "us", &cfg, true);
    tokio::time::timeout(Duration::from_millis(250), &mut ready)
        .await
        .expect("quorum confirmation must wake the monitor immediately");
    assert!(local_monitoring_allowed(&cfg));
    assert_eq!(
        signature,
        cluster_ui_signature(&compute_cluster_status(&cfg))
    );
    println!(
        "startup wake after quorum: {:?}; configured heartbeat interval: {} s",
        started.elapsed(),
        cfg.cluster.heartbeat_interval_secs
    );

    // A confirmation just before the waiter registers cannot be lost either.
    tokio::time::timeout(Duration::from_millis(250), wait_for_local_monitoring(&cfg))
        .await
        .expect("an already-ready node should not wait for another notification");
}

#[tokio::test]
async fn startup_monitor_wait_preserves_owner_agreement_and_handoff_fencing() {
    let _guard = ClusterStateGuard::new();
    let (cfg, peer_status) = startup_monitoring_fixture();
    let ready = wait_for_local_monitoring(&cfg);
    tokio::pin!(ready);
    assert!(futures_util::poll!(&mut ready).is_pending());

    let mut disagreement = peer_status.clone();
    disagreement.active_owner = Some("us".into());
    merge_direct_peer_status(disagreement, "us", &cfg, true);
    assert!(futures_util::poll!(&mut ready).is_pending());

    {
        let mut state = cluster_state_write();
        state.pending_handoff_source = Some("us".into());
        state.local_execution_held = true;
    }
    merge_direct_peer_status(peer_status, "us", &cfg, true);
    assert!(futures_util::poll!(&mut ready).is_pending());
    cluster_state_write().pending_handoff_source = None;
    compute_cluster_status(&cfg);
    assert!(futures_util::poll!(&mut ready).is_pending());
    cluster_state_write().local_execution_held = false;
    compute_cluster_status(&cfg);
    tokio::time::timeout(Duration::from_millis(250), &mut ready)
        .await
        .expect("confirmed handoff should release the waiting monitor");
}

#[tokio::test]
async fn failed_source_demotion_never_sends_target_promotion() {
    use axum::{routing::post, Json, Router};
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };
    let _state_guard = ClusterStateGuard::new();
    crate::install_crypto_provider();
    let promotions = Arc::new(AtomicUsize::new(0));
    let seen = promotions.clone();
    let app = Router::new()
        .route(
            "/source/api/cluster/apply-node-mode",
            post(|| async {
                Json(serde_json::json!({"success": false, "message": "source still running"}))
            }),
        )
        .route(
            "/target/api/cluster/apply-node-mode",
            post(move || {
                seen.fetch_add(1, Ordering::SeqCst);
                async { Json(serde_json::json!({"success": false})) }
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let mut cfg = test_config("coordinator", 0);
    cfg.cluster.peers = ["source", "target"]
        .into_iter()
        .map(|id| crate::config::ClusterPeer {
            node_id: id.to_string(),
            name: id.to_string(),
            api_url: format!("http://{addr}/{id}"),
            priority: 10,
        })
        .collect();
    let now = now_secs();
    let nodes: Vec<_> = ["source", "target"]
        .into_iter()
        .map(|id| {
            let mut node = empty_node(id, id, "", 10, false, now);
            node.last_seen = Some(now);
            node.health = ClusterHealth::healthy();
            node
        })
        .collect();
    {
        let mut state = cluster_state_write();
        *state = ClusterState::default();
        state.active_owner = Some("source".into());
        state.nodes = nodes
            .iter()
            .map(|node| (node.node_id.clone(), node.clone()))
            .collect();
    }
    let mut before = ClusterStatus {
        enabled: true,
        local_node_id: "coordinator".into(),
        active_owner: Some("source".into()),
        lease_until: None,
        config_version: String::new(),
        auto_failover: true,
        public_status: Default::default(),
        nodes,
    };
    let result = finalize_cluster_node_switch(&cfg, &before, "source", "target", false).await;
    server.abort();
    assert!(result.is_err());
    assert_eq!(promotions.load(Ordering::SeqCst), 0);
    assert_eq!(current_active_owner().as_deref(), Some("source"));

    // A success envelope with a still-running process also cannot confirm demotion.
    before.local_node_id = "source".into();
    before.nodes[0].ffmpeg_running = true;
    assert!(validate_node_mode_response(&before, "source", false).is_err());
    before.nodes[0].ffmpeg_running = false;
    assert!(validate_node_mode_response(&before, "source", false).is_ok());
}

#[tokio::test]
async fn automatic_handoff_confirms_source_shutdown_after_owner_has_changed() {
    use axum::{
        routing::{get, post},
        Json, Router,
    };
    use std::sync::Arc;

    let _guard = ClusterStateGuard::new();
    crate::install_crypto_provider();
    let mut source_cfg = test_config("jp", 100);
    source_cfg.bililive.enable_danmaku_command = true;
    let source_toggles = monitor_toggle_state_from_config(&source_cfg);
    let source_targets = channel_target_state_from_config(&source_cfg);
    let exported = cluster_sync_config_from_config(&source_cfg);
    let now = now_secs();
    let mut jp = empty_node("jp", "JP", "", 100, false, now);
    jp.last_seen = Some(now);
    jp.network_unstable = true;
    jp.health = ClusterHealth::unhealthy("external_api_unreachable", false, true);
    jp.monitor_toggles = source_toggles.clone();
    jp.channel_targets = source_targets.clone();
    jp.ffmpeg_running = true;
    let mut us = empty_node("us", "US", "", 50, false, now);
    us.last_seen = Some(now);
    us.health = ClusterHealth::healthy();
    let before = ClusterStatus {
        enabled: true,
        local_node_id: "coordinator".into(),
        // Both elections have already moved off JP, but its configuration
        // still has the old monitor toggles enabled until demotion is applied.
        active_owner: Some("us".into()),
        lease_until: None,
        config_version: String::new(),
        auto_failover: true,
        public_status: Default::default(),
        nodes: vec![jp, us],
    };
    let mut source_reply = before.clone();
    source_reply.local_node_id = "jp".into();
    source_reply.nodes[0].monitor_toggles = all_monitor_toggles_off();
    source_reply.nodes[0].ffmpeg_running = false;
    let mut target_reply = source_reply.clone();
    target_reply.local_node_id = "us".into();

    let calls = Arc::new(Mutex::new(
        Vec::<(String, ClusterApplyNodeModeRequest)>::new(),
    ));
    let source_calls = calls.clone();
    let target_calls = calls.clone();
    let app = Router::new()
        .route(
            "/jp/api/cluster/export-config",
            get(move || {
                let payload = exported.clone();
                async move { Json(serde_json::json!({"success": true, "data": payload})) }
            }),
        )
        .route(
            "/jp/api/cluster/apply-node-mode",
            post(move |Json(payload): Json<ClusterApplyNodeModeRequest>| {
                let calls = source_calls.clone();
                let reply = source_reply.clone();
                async move {
                    if let Err(error) = validate_node_mode_precondition(
                        Some("us"),
                        payload.expected_active_owner.as_deref(),
                        payload.active,
                        "jp",
                        payload.handoff_target_node_id.as_deref(),
                    ) {
                        return Json(serde_json::json!({"success": false, "message": error}));
                    }
                    calls.lock().unwrap().push(("jp".into(), payload));
                    Json(serde_json::json!({"success": true, "data": reply}))
                }
            }),
        )
        .route(
            "/us/api/cluster/apply-node-mode",
            post(move |Json(payload): Json<ClusterApplyNodeModeRequest>| {
                let calls = target_calls.clone();
                let mut reply = target_reply.clone();
                async move {
                    assert_eq!(calls.lock().unwrap().len(), 1, "source must confirm first");
                    reply.nodes[1].monitor_toggles = payload.monitor_toggles.clone().unwrap();
                    reply.nodes[1].channel_targets = payload.channel_targets.clone().unwrap();
                    calls.lock().unwrap().push(("us".into(), payload));
                    Json(serde_json::json!({"success": true, "data": reply}))
                }
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let mut cfg = test_config("coordinator", 0);
    cfg.cluster.peers = before
        .nodes
        .iter()
        .map(|node| crate::config::ClusterPeer {
            node_id: node.node_id.clone(),
            name: node.name.clone(),
            api_url: format!("http://{address}/{}", node.node_id),
            priority: node.priority,
        })
        .collect();
    {
        let mut state = cluster_state_write();
        *state = ClusterState {
            active_owner: Some("us".into()),
            nodes: before
                .nodes
                .iter()
                .map(|node| (node.node_id.clone(), node.clone()))
                .collect(),
            ..Default::default()
        };
    }

    let result = finalize_cluster_node_switch(&cfg, &before, "jp", "us", false).await;
    server.abort();
    result.unwrap();
    let calls = calls.lock().unwrap();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].0, "jp");
    assert!(!calls[0].1.active);
    assert_eq!(calls[0].1.monitor_toggles, Some(all_monitor_toggles_off()));
    assert_eq!(calls[1].0, "us");
    assert!(calls[1].1.active);
    assert_eq!(calls[1].1.monitor_toggles, Some(source_toggles));
    assert_eq!(calls[1].1.channel_targets, Some(source_targets));
    assert_eq!(current_active_owner().as_deref(), Some("us"));
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
    }
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
    local.niconico.enable_monitor = true;

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
    source.niconico.enable_monitor = false;

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
    assert!(local.niconico.enable_monitor);

    assert_eq!(local.youtube.channel_name, "remote yt");
    assert_eq!(local.youtube.channel_id, "remote-yt-id");
    assert_eq!(local.twitch.channel_name, "remote tw");
    assert_eq!(local.twitch.channel_id, "remote-tw-id");
    assert_eq!(local.priority_channel.channel_name, "remote priority");
    // The synced channel is absent from the local registry, so IDs must not
    // remain usable after a missing/removed channel lookup.
    assert!(local.priority_channel.youtube_channel_id.is_empty());
    assert!(local.priority_channel.twitch_channel_id.is_empty());
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
        niconico_enable_monitor: true,
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
    assert!(cfg.niconico.enable_monitor);
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
fn handoff_applies_source_toggles_to_target_and_clears_source() {
    let mut source = test_config("source", 10);
    let source_toggles = MonitorToggleState {
        enable_danmaku_command: true,
        enable_youtube_monitor: true,
        enable_twitch_monitor: false,
        youtube_enable_monitor: true,
        twitch_enable_monitor: false,
        priority_channel_enabled: true,
        priority_channel_auto_restart: true,
        niconico_enable_monitor: true,
    };
    apply_monitor_toggle_state_to_config(&mut source, &source_toggles);

    let mut target = test_config("target", 5);
    apply_monitor_toggle_state_to_config(&mut target, &all_monitor_toggles_off());

    let source_applied = resolved_node_mode_monitor_toggles(false, None);
    let target_applied = resolved_node_mode_monitor_toggles(true, Some(source_toggles.clone()));
    apply_node_mode_config_state(&mut source, None, source_applied.as_ref());
    apply_node_mode_config_state(&mut target, None, target_applied.as_ref());

    assert_eq!(
        monitor_toggle_state_from_config(&source),
        all_monitor_toggles_off()
    );
    assert_eq!(monitor_toggle_state_from_config(&target), source_toggles);
}

#[test]
fn demoted_node_mode_clears_unspecified_monitor_toggles() {
    assert_eq!(
        resolved_node_mode_monitor_toggles(false, None),
        Some(all_monitor_toggles_off())
    );
    assert_eq!(resolved_node_mode_monitor_toggles(true, None), None);

    let requested = MonitorToggleState {
        enable_danmaku_command: true,
        enable_youtube_monitor: true,
        enable_twitch_monitor: true,
        youtube_enable_monitor: true,
        twitch_enable_monitor: true,
        priority_channel_enabled: true,
        priority_channel_auto_restart: false,
        niconico_enable_monitor: true,
    };
    assert_eq!(
        resolved_node_mode_monitor_toggles(false, Some(requested)),
        Some(all_monitor_toggles_off())
    );
}

#[test]
fn node_mode_precondition_rejects_delayed_reverse_transition() {
    assert!(validate_node_mode_precondition(Some("a"), Some("b"), false, "a", None).is_err());
    assert!(validate_node_mode_precondition(Some("b"), Some("b"), false, "a", None).is_ok());
    assert!(validate_node_mode_precondition(Some("b"), Some("a"), true, "b", None).is_ok());
    assert!(validate_node_mode_precondition(Some("a"), Some("a"), true, "b", None).is_ok());
    assert!(validate_node_mode_precondition(Some("c"), Some("a"), true, "b", None).is_err());
}

#[test]
fn source_can_confirm_demotion_after_accepting_the_handoff_target() {
    for owner in ["jp", "us"] {
        assert!(
            validate_node_mode_precondition(Some(owner), Some("jp"), false, "jp", Some("us"))
                .is_ok()
        );
    }
    for (owner, expected, target) in [
        ("eu", "jp", Some("us")),
        ("jp", "us", Some("jp")),
        ("us", "jp", None),
    ] {
        assert!(
            validate_node_mode_precondition(Some(owner), Some(expected), false, "jp", target)
                .is_err()
        );
    }
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
        public_status: crate::config::PublicStatusConfig::default(),
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
            public_status: crate::config::PublicStatusConfig::default(),
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
        public_status: crate::config::PublicStatusConfig::default(),
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

#[tokio::test]
async fn external_api_outage_fences_owner_despite_fresh_inbound_heartbeats() {
    let _guard = ClusterStateGuard::new();
    let mut cfg = test_config("jp", 100);
    cfg.cluster.peers = ["us", "eu"]
        .into_iter()
        .enumerate()
        .map(|(index, id)| crate::config::ClusterPeer {
            node_id: id.into(),
            name: id.into(),
            api_url: format!("http://{id}"),
            priority: 50 - index as i32,
        })
        .collect();
    let now = now_secs();
    {
        let mut state = cluster_state_write();
        *state = ClusterState {
            active_owner: Some("jp".into()),
            ..Default::default()
        };
        for peer in &cfg.cluster.peers {
            let mut node = empty_node(
                &peer.node_id,
                &peer.name,
                &peer.api_url,
                peer.priority,
                false,
                now,
            );
            node.last_seen = Some(now);
            node.health = ClusterHealth::healthy();
            state.nodes.insert(peer.node_id.clone(), node);
            state.peer_heartbeat_acks.insert(peer.node_id.clone(), now);
            state.peer_owner_views.insert(
                peer.node_id.clone(),
                peer_view("jp", &["jp", "us", "eu"], now),
            );
        }
        for _ in 0..3 {
            super::self_check::record_self_check(
                &mut state.local_self_check,
                &cfg.cluster,
                Err(SelfCheckFailure::RequestFailed),
                std::time::Instant::now(),
                now,
            );
        }
    }

    // A failed loop through the tunnel alone does not override live peers.
    let local = collect_local_snapshot(&cfg, String::new()).await;
    update_node(local, "jp");
    assert_eq!(
        compute_cluster_status(&cfg).active_owner.as_deref(),
        Some("jp")
    );
    assert!(local_monitoring_allowed(&cfg));
    for failure in 1..=cfg.cluster.thresholds.max_external_api_failures {
        record_external_api_result(false);
        let local = collect_local_snapshot(&cfg, String::new()).await;
        update_node(local, "jp");
        let status = compute_cluster_status(&cfg);
        let jp = status
            .nodes
            .iter()
            .find(|node| node.node_id == "jp")
            .unwrap();
        assert!(!jp.health.stale);
        if failure < cfg.cluster.thresholds.max_external_api_failures {
            assert_eq!(status.active_owner.as_deref(), Some("jp"));
        } else {
            assert_eq!(status.active_owner.as_deref(), Some("us"));
            assert_eq!(jp.health.reason, "external_api_unreachable");
            assert_eq!(jp.role, ClusterNodeRole::Unhealthy);
            assert!(!local_monitoring_allowed(&cfg));
        }
    }

    // Manual-only mode retains its existing operator-controlled behavior.
    cfg.cluster.auto_failover = false;
    let local = collect_local_snapshot(&cfg, String::new()).await;
    assert!(local.health.healthy);
    assert!(!cluster_state_read().local_fault_latched);
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
        niconico_enable_monitor: true,
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
        niconico_enable_monitor: true,
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
