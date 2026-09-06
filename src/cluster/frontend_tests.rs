//! Regressions for the node-state contract consumed by both frontends.
use super::*;
use crate::config::ClusterPeer;
use crate::webui::api::cluster_drain_for_config;
use tests::{test_config, ClusterStateGuard};

fn two_nodes() -> crate::config::Config {
    let mut cfg = test_config("local", 0);
    cfg.cluster.peers = vec![ClusterPeer {
        node_id: "peer".into(),
        name: "peer".into(),
        api_url: String::new(),
        priority: 10,
    }];
    let now = now_secs();
    let mut state = ClusterState::default();
    for (id, priority) in [("local", 0), ("peer", 10)] {
        let mut node = empty_node(id, id, "", priority, id == "local", now);
        node.last_seen = Some(now);
        node.health = ClusterHealth::healthy();
        state.nodes.insert(id.into(), node);
    }
    state.active_owner = Some("peer".into());
    *cluster_state_write() = state;
    cfg
}

#[test]
fn maintenance_fault_waiting_and_owner_have_distinct_roles() {
    let cfg = test_config("local", 0);
    for (draining, latched, seen, health, expected, reason) in [
        (
            false,
            false,
            Some(100),
            ClusterHealth::healthy(),
            ClusterNodeRole::Active,
            "healthy",
        ),
        (
            true,
            false,
            Some(100),
            ClusterHealth::healthy(),
            ClusterNodeRole::Draining,
            "draining",
        ),
        (
            false,
            true,
            Some(100),
            ClusterHealth::unhealthy("ffmpeg_repeated_failures", false, true),
            ClusterNodeRole::Unhealthy,
            "ffmpeg_repeated_failures",
        ),
        (
            true,
            true,
            Some(100),
            ClusterHealth::unhealthy("ffmpeg_repeated_failures", false, true),
            ClusterNodeRole::Draining,
            "ffmpeg_repeated_failures",
        ),
        (
            false,
            false,
            None,
            ClusterHealth::healthy(),
            ClusterNodeRole::Unhealthy,
            "waiting_for_heartbeat",
        ),
        (
            false,
            false,
            Some(1),
            ClusterHealth::healthy(),
            ClusterNodeRole::Unhealthy,
            "heartbeat_timeout",
        ),
        (
            true,
            false,
            Some(1),
            ClusterHealth::healthy(),
            ClusterNodeRole::Draining,
            "heartbeat_timeout",
        ),
    ] {
        let mut state = ClusterState::default();
        let mut node = empty_node("local", "local", "", 0, true, 100);
        node.last_seen = seen;
        node.draining = draining;
        node.network_unstable = latched;
        node.health = health;
        state.nodes.insert("local".into(), node);
        let status = build_status_from_state(&mut state, &cfg, String::new(), 100);
        assert_eq!(status.nodes[0].role, expected);
        assert_eq!(status.nodes[0].health.reason, reason);
    }
}

#[tokio::test]
async fn recovery_reply_has_fresh_health_without_waiting_for_a_heartbeat() {
    let _guard = ClusterStateGuard::new();
    let cfg = two_nodes();
    {
        let mut state = cluster_state_write();
        state.local_fault_latched = true;
        state.local_fault_reason = Some("ffmpeg_repeated_failures".into());
        state.local_failed_restarts = 3;
        state.local_failed_restart_times = vec![now_secs(); 3];
        let local = state.nodes.get_mut("local").unwrap();
        local.network_unstable = true;
        local.health = ClusterHealth::unhealthy("ffmpeg_repeated_failures", false, true);
    }
    let reply = cluster_drain_for_config(
        &cfg,
        ClusterDrainRequest {
            node_id: None,
            draining: false,
            propagate: Some(false),
        },
    )
    .await
    .unwrap();
    let reply = serde_json::to_value(reply).unwrap();
    assert_eq!(reply["success"], true);
    let local = reply["data"]["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|node| node["node_id"] == "local")
        .unwrap();
    assert_eq!(local["role"], "standby");
    assert_eq!(local["health"]["healthy"], true);
    assert_eq!(local["health"]["stream_degraded"], false);
    assert_eq!(local["network_unstable"], false);
    assert_eq!(cluster_state_read().local_failed_restarts, 0);
}

async fn control_server(
    reply: serde_json::Value,
) -> (
    String,
    tokio::task::JoinHandle<()>,
    std::sync::Arc<std::sync::Mutex<Vec<serde_json::Value>>>,
) {
    use axum::{routing::post, Json, Router};
    let calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let captured = calls.clone();
    let app = Router::new().route(
        "/api/cluster/drain",
        post(move |Json(payload): Json<serde_json::Value>| {
            let reply = reply.clone();
            captured.lock().unwrap().push(payload);
            async move { Json(reply) }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://{address}"), task, calls)
}

#[tokio::test]
async fn remote_recovery_requires_acknowledgement_and_returns_the_merged_state() {
    let _guard = ClusterStateGuard::new();
    crate::install_crypto_provider();
    for mode in ["rejected", "missing", "wrong_identity", "accepted"] {
        let mut cfg = two_nodes();
        {
            let mut state = cluster_state_write();
            let peer = state.nodes.get_mut("peer").unwrap();
            peer.network_unstable = true;
            peer.health = ClusterHealth::unhealthy("ffmpeg_repeated_failures", false, true);
        }
        let mut accepted = compute_cluster_status(&cfg);
        accepted.local_node_id = if mode == "wrong_identity" {
            "other"
        } else {
            "peer"
        }
        .into();
        let peer = accepted
            .nodes
            .iter_mut()
            .find(|node| node.node_id == "peer")
            .unwrap();
        peer.network_unstable = false;
        peer.health = ClusterHealth::healthy();
        let envelope = serde_json::json!({
            "success": mode != "rejected",
            "message": "fixture reply",
            "data": if mode == "missing" { None } else { Some(accepted) },
        });
        let (url, server, calls) = control_server(envelope).await;
        cfg.cluster.peers[0].api_url = url;
        let reply = cluster_drain_for_config(
            &cfg,
            ClusterDrainRequest {
                node_id: Some("peer".into()),
                draining: false,
                propagate: None,
            },
        )
        .await
        .unwrap();
        server.abort();
        let reply = serde_json::to_value(reply).unwrap();
        assert_eq!(reply["success"], mode == "accepted", "{mode}");
        let peer = reply["data"]["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|node| node["node_id"] == "peer")
            .unwrap();
        assert_eq!(peer["health"]["healthy"], mode == "accepted", "{mode}");
        assert_eq!(peer["network_unstable"], mode != "accepted", "{mode}");
        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0]["node_id"], "peer");
        assert_eq!(calls[0]["propagate"], false);
    }
}

#[tokio::test]
async fn implicit_local_drain_propagates_an_explicit_target_and_unknown_targets_fail() {
    let _guard = ClusterStateGuard::new();
    crate::install_crypto_provider();
    let mut cfg = two_nodes();
    let mut accepted = compute_cluster_status(&cfg);
    accepted.local_node_id = "peer".into();
    let (url, server, calls) =
        control_server(serde_json::json!({"success": true, "data": accepted})).await;
    cfg.cluster.peers[0].api_url = url;
    let reply = cluster_drain_for_config(
        &cfg,
        ClusterDrainRequest {
            node_id: None,
            draining: true,
            propagate: None,
        },
    )
    .await
    .unwrap();
    assert_eq!(serde_json::to_value(reply).unwrap()["success"], true);
    assert_eq!(calls.lock().unwrap()[0]["node_id"], "local");
    assert!(cluster_state_read().nodes["local"].draining);
    assert!(!cluster_state_read().nodes["peer"].draining);
    let reply = cluster_drain_for_config(
        &cfg,
        ClusterDrainRequest {
            node_id: Some("unconfigured".into()),
            draining: false,
            propagate: None,
        },
    )
    .await;
    assert!(matches!(reply, Err(axum::http::StatusCode::BAD_REQUEST)));
    assert_eq!(calls.lock().unwrap().len(), 1);
    server.abort();
}
