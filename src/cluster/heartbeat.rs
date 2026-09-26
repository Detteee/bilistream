//! Heartbeat worker, outbound pings, and unreachable-peer accounting.

use super::election::{is_stale, last_seen_is_stale};
use super::fencing::{
    clear_local_stream, collect_local_snapshot, local_has_fresh_quorum,
    local_monitoring_block_reason, HEARTBEAT_FAILURE_THRESHOLD,
};
use super::state::{
    cluster_heartbeat_timeout, cluster_state_read, cluster_state_write, now_secs,
    CLUSTER_HTTP_CLIENT,
};
use super::status::{
    canonicalize_node_membership, compute_cluster_status_with_version, current_active_owner,
    empty_node, heartbeat_response_is_valid, merge_direct_peer_status, update_node,
};
use super::sync::{adopt_auto_failover_from_peer_view, retry_unconfirmed_demotion};
use super::types::*;
use super::version::monitored_config_version;
use crate::config::Config;
use crate::plugins::{
    enable_danmaku_commands, is_danmaku_running, is_ffmpeg_running, set_manual_restart,
    stop_danmaku, stop_ffmpeg,
};
use axum::body::Bytes;
use futures_util::future::join_all;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

pub(crate) static AUTO_TRANSITION_IN_FLIGHT: AtomicBool = AtomicBool::new(false);

pub struct ClusterWorker {
    heartbeat: tokio::task::JoinHandle<()>,
    self_check: tokio::task::JoinHandle<()>,
    external_api: tokio::task::JoinHandle<()>,
    yt_index: tokio::task::JoinHandle<()>,
}

impl Drop for ClusterWorker {
    fn drop(&mut self) {
        self.heartbeat.abort();
        self.self_check.abort();
        self.external_api.abort();
        self.yt_index.abort();
    }
}

pub fn start_cluster_worker() -> ClusterWorker {
    super::sync::mark_process_started();
    let heartbeat = tokio::spawn(async {
        let client = CLUSTER_HTTP_CLIENT.clone();

        loop {
            let cycle_started = Instant::now();
            let cfg = match crate::config::load_config()
                .await
                .map_err(|e| e.to_string())
            {
                Ok(cfg) => cfg,
                Err(error) => {
                    tracing::debug!("Cluster worker skipped config load: {}", error);
                    tokio::time::sleep(Duration::from_secs(15)).await;
                    continue;
                }
            };

            if !cfg.cluster.enabled {
                tokio::time::sleep(Duration::from_secs(15)).await;
                continue;
            }

            let config_version = monitored_config_version(&cfg);
            let local = collect_local_snapshot(&cfg, config_version.clone()).await;
            let previous_owner = current_active_owner();
            update_node(local.clone(), &cfg.cluster.node_id);
            send_heartbeats(&client, &cfg, local).await;
            let status = compute_cluster_status_with_version(&cfg, config_version);
            schedule_auto_owner_transition(&cfg, previous_owner, &status);
            schedule_unconfirmed_demotion(&cfg);
            let block_reason = local_monitoring_block_reason(&cfg);
            if let Some(reason) = block_reason {
                if is_ffmpeg_running().await {
                    tracing::warn!("{}，停止本节点 ffmpeg 推流", reason);
                    set_manual_restart();
                    clear_local_stream();
                    stop_ffmpeg().await;
                }
                enable_danmaku_commands(false);
                if is_danmaku_running() {
                    tracing::info!("{}，停止本节点弹幕客户端", reason);
                    stop_danmaku().await;
                }
            }

            tokio::time::sleep(heartbeat_cycle_delay(
                heartbeat_sleep_duration(&cfg),
                cycle_started.elapsed(),
            ))
            .await;
        }
    });
    // Self-check latency cannot delay heartbeats to healthy peers, and neither
    // can YouTube index fetches or the owner's Google calls.
    ClusterWorker {
        heartbeat,
        self_check: tokio::spawn(super::self_check::run_self_checks()),
        external_api: tokio::spawn(super::external_api::run_external_api_checks()),
        yt_index: tokio::spawn(super::yt_index::run_yt_index()),
    }
}

pub(crate) fn heartbeat_cycle_delay(period: Duration, elapsed: Duration) -> Duration {
    period.saturating_sub(elapsed)
}

pub(crate) fn heartbeat_sleep_duration(cfg: &Config) -> Duration {
    let base = Duration::from_secs(cfg.cluster.heartbeat_interval_secs.max(1));
    let jitter_ms = stable_node_jitter_ms(&cfg.cluster.node_id, 1_000);
    base + Duration::from_millis(jitter_ms)
}

pub(crate) fn stable_node_jitter_ms(node_id: &str, max_ms: u64) -> u64 {
    if max_ms == 0 {
        return 0;
    }
    let mut hasher = DefaultHasher::new();
    node_id.hash(&mut hasher);
    hasher.finish() % max_ms
}

pub(crate) fn current_forced_owner() -> Option<String> {
    cluster_state_read().forced_owner.clone()
}

pub(crate) fn schedule_auto_owner_transition(
    cfg: &Config,
    previous_owner: Option<String>,
    status: &ClusterStatus,
) {
    if !cfg.cluster.auto_failover {
        return;
    }
    let previous_owner = cluster_state_read()
        .pending_handoff_source
        .clone()
        .or(previous_owner);
    let Some(previous_owner) = previous_owner else {
        return;
    };
    let Some(new_owner) = status.active_owner.as_deref() else {
        return;
    };
    if previous_owner == new_owner || current_forced_owner().is_some() {
        return;
    }
    if cfg.cluster.node_id != new_owner {
        return;
    }
    if !local_has_fresh_quorum(cfg) {
        tracing::warn!("Cluster owner transition deferred: no fresh heartbeat quorum");
        return;
    }

    if AUTO_TRANSITION_IN_FLIGHT
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        tracing::debug!("Cluster owner transition already in progress");
        return;
    }

    let preserve_source_drain = status
        .nodes
        .iter()
        .find(|node| node.node_id == previous_owner)
        .is_some_and(|node| node.draining);
    let cfg = cfg.clone();
    let status = status.clone();
    let new_owner = new_owner.to_string();
    tokio::spawn(async move {
        tracing::warn!(
            "集群自动故障转移: {} -> {}, transfer active config and monitor state",
            previous_owner,
            new_owner
        );
        if let Err(e) = super::sync::finalize_cluster_node_switch_with(
            &cfg,
            &status,
            &previous_owner,
            &new_owner,
            preserve_source_drain,
            true,
        )
        .await
        {
            tracing::warn!(
                "Failed to finalize automatic cluster failover {} -> {}: {}",
                previous_owner,
                new_owner,
                e
            );
        }
        AUTO_TRANSITION_IN_FLIGHT.store(false, Ordering::Release);
    });
}

static DEMOTION_RETRY_IN_FLIGHT: AtomicBool = AtomicBool::new(false);

/// Off the heartbeat path: a timeout to a dead source must not delay
/// heartbeats to healthy peers.
fn schedule_unconfirmed_demotion(cfg: &Config) {
    if cluster_state_read().unconfirmed_demotion.is_none()
        || DEMOTION_RETRY_IN_FLIGHT
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
    {
        return;
    }
    let cfg = cfg.clone();
    tokio::spawn(async move {
        retry_unconfirmed_demotion(&cfg).await;
        DEMOTION_RETRY_IN_FLIGHT.store(false, Ordering::Release);
    });
}

pub(crate) fn record_heartbeat(cfg: &Config, mut node: ClusterNodeSnapshot) -> bool {
    if !cfg.cluster.enabled {
        tracing::debug!("Ignored cluster heartbeat while cluster mode is disabled");
        return false;
    }
    if node.node_id == cfg.cluster.node_id {
        tracing::debug!(
            "Ignored heartbeat claiming local node identity {}",
            node.node_id
        );
        return false;
    }
    if !canonicalize_node_membership(&mut node, cfg) {
        tracing::debug!("Ignored heartbeat from unknown node {}", node.node_id);
        return false;
    }
    node.last_seen = Some(now_secs());
    node.is_local = false;
    let mut state = cluster_state_write();
    state.heartbeat_failures.remove(&node.node_id);
    state.nodes.insert(node.node_id.clone(), node);
    true
}

pub(crate) async fn send_heartbeats(
    client: &reqwest::Client,
    cfg: &Config,
    local: ClusterNodeSnapshot,
) {
    let request = ClusterHeartbeatRequest { node: local };
    let body = match encode_heartbeat(&request) {
        Ok(bytes) => bytes,
        Err(error) => {
            tracing::warn!("Could not encode local cluster heartbeat: {error}");
            return;
        }
    };
    // Fan out concurrently: one slow/dead peer must not delay heartbeats to the
    // others, otherwise healthy peers may see this node as stale.
    let tasks = cfg
        .cluster
        .peers
        .iter()
        .filter(|peer| peer.node_id != cfg.cluster.node_id)
        .map(|peer| send_heartbeat_to_peer(client, cfg, peer, body.clone()));
    join_all(tasks).await;
}

fn encode_heartbeat(request: &ClusterHeartbeatRequest) -> Result<Bytes, serde_json::Error> {
    // Every peer receives identical bytes. Encoding once also avoids one
    // payload-sized allocation per peer; Bytes clones only share ownership.
    serde_json::to_vec(request).map(Bytes::from)
}

pub(crate) async fn send_heartbeat_to_peer(
    client: &reqwest::Client,
    cfg: &Config,
    peer: &crate::config::ClusterPeer,
    body: Bytes,
) {
    let url = format!(
        "{}/api/cluster/heartbeat",
        peer.api_url.trim_end_matches('/')
    );
    let result = client
        .post(url)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body(body)
        .timeout(cluster_heartbeat_timeout(cfg))
        .send()
        .await;

    match result {
        Ok(response) if !response.status().is_success() => {
            tracing::debug!(
                "Cluster heartbeat failed for {}: HTTP {}",
                peer.node_id,
                response.status()
            );
            mark_peer_unreachable(&peer.node_id, cfg);
        }
        Ok(response) => match crate::plugins::http::response_json_limited::<
            PeerApiResponse<ClusterStatus>,
        >(response)
        .await
        {
            Ok(envelope) if envelope.success => {
                if let Some(status) = envelope.data {
                    if heartbeat_response_is_valid(&status, &peer.node_id) {
                        let peer_auto_failover = status.auto_failover;
                        let peer_is_active_owner =
                            merge_direct_peer_status(status, &peer.node_id, cfg, true);
                        adopt_auto_failover_from_peer_view(
                            peer_auto_failover,
                            peer_is_active_owner,
                            &peer.node_id,
                            cfg,
                        )
                        .await;
                    } else {
                        tracing::debug!(
                            "Cluster heartbeat response from {} failed identity/freshness validation",
                            peer.node_id
                        );
                        mark_peer_unreachable(&peer.node_id, cfg);
                    }
                } else {
                    tracing::debug!(
                        "Cluster heartbeat response from {} had no status",
                        peer.node_id
                    );
                    mark_peer_unreachable(&peer.node_id, cfg);
                }
            }
            Ok(envelope) => {
                tracing::debug!(
                    "Cluster heartbeat rejected by {}: {:?}",
                    peer.node_id,
                    envelope.message
                );
                mark_peer_unreachable(&peer.node_id, cfg);
            }
            Err(e) => {
                tracing::debug!("Cluster heartbeat parse failed for {}: {}", peer.node_id, e);
                mark_peer_unreachable(&peer.node_id, cfg);
            }
        },
        Err(e) => {
            tracing::debug!("Cluster heartbeat failed for {}: {}", peer.node_id, e);
            mark_peer_unreachable(&peer.node_id, cfg);
        }
    }
}

pub(crate) fn mark_peer_unreachable(node_id: &str, cfg: &Config) {
    let mut state = cluster_state_write();
    let now = now_secs();
    let failures = if let Some(count) = state.heartbeat_failures.get_mut(node_id) {
        *count = count.saturating_add(1);
        *count
    } else {
        state.heartbeat_failures.insert(node_id.to_string(), 1);
        1
    };
    let peer = cfg
        .cluster
        .peers
        .iter()
        .find(|peer| peer.node_id == node_id);
    if !state.nodes.contains_key(node_id) {
        let node = if let Some(peer) = peer {
            empty_node(
                &peer.node_id,
                &peer.name,
                &peer.api_url,
                peer.priority,
                false,
                now,
            )
        } else {
            empty_node(node_id, node_id, "", 0, false, now)
        };
        state.nodes.insert(node_id.to_string(), node);
    }
    let Some(node) = state.nodes.get_mut(node_id) else {
        return;
    };
    let stale = is_stale(node, cfg, now);
    if failures < HEARTBEAT_FAILURE_THRESHOLD && !stale {
        tracing::debug!(
            "Cluster heartbeat failure for {} ({}/{})",
            node_id,
            failures,
            HEARTBEAT_FAILURE_THRESHOLD
        );
        return;
    }
    // Asymmetric partition guard: if the peer's own inbound heartbeats are
    // still fresh, it is demonstrably alive even though we cannot reach its
    // API. Leave its health to normalize_node_health instead of latching
    // api_unreachable here.
    let recently_seen = node
        .last_seen
        .is_some_and(|last_seen| !last_seen_is_stale(last_seen, cfg, now));
    if recently_seen {
        tracing::debug!(
            "Cluster heartbeat outbound failures for {} ({}) but inbound heartbeat is fresh; not marking unreachable",
            node_id,
            failures
        );
        return;
    }
    node.health = ClusterHealth::unhealthy("api_unreachable", true, false);
}
