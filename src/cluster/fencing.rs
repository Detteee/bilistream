//! Drain, fault latch, isolation, and “may this node monitor/push?” gates.

use super::election::configured_node_ids;
use super::state::{cluster_state_read, cluster_state_write, now_secs, ClusterState};
use super::types::*;
use crate::config::{ClusterConfig, Config};
use crate::plugins::is_ffmpeg_running;
use crate::webui::state::{get_status_cache, NetworkStatus};
use std::collections::HashMap;

pub(crate) const FFMPEG_FAILURE_WINDOW_SECS: u64 = 60 * 60;

pub(crate) const EXTERNAL_API_FAILURE_RETENTION_SECS: u64 = 60 * 60;

pub(crate) const HEARTBEAT_FAILURE_THRESHOLD: u32 = 3;

pub fn local_monitoring_allowed(cfg: &Config) -> bool {
    local_monitoring_block_reason(cfg).is_none()
}

/// `None` means this node may keep monitoring/pushing; `Some(reason)` is a
/// human-readable Chinese reason suitable for logging.
pub(crate) fn local_monitoring_block_reason(cfg: &Config) -> Option<String> {
    if !cfg.cluster.enabled {
        return None;
    }
    state_monitoring_block_reason(&cluster_state_read(), cfg, now_secs())
}

pub(crate) fn state_monitoring_block_reason(
    state: &ClusterState,
    cfg: &Config,
    now: u64,
) -> Option<String> {
    if state.active_owner.as_deref() != Some(cfg.cluster.node_id.as_str()) {
        return Some(format!(
            "本节点已不是集群活跃节点 (当前活跃节点: {})",
            state.active_owner.as_deref().unwrap_or("无")
        ));
    }
    if state.local_draining {
        return Some("本节点处于排空(drain)状态".to_string());
    }
    if state.local_fault_latched {
        return Some(format!(
            "本节点已被判定故障 ({})",
            state
                .local_fault_reason
                .as_deref()
                .unwrap_or("node_fault_latched")
        ));
    }
    if !state_has_fresh_quorum(state, cfg, now) {
        return Some("集群心跳多数派已丢失".to_string());
    }
    None
}

pub(crate) fn local_has_fresh_quorum(cfg: &Config) -> bool {
    if !cfg.cluster.enabled {
        return true;
    }
    state_has_fresh_quorum(&cluster_state_read(), cfg, now_secs())
}

pub(crate) fn state_has_fresh_quorum(state: &ClusterState, cfg: &Config, now: u64) -> bool {
    let configured = configured_node_ids(cfg);
    let required = configured.len() / 2 + 1;
    let timeout = cfg.cluster.failover_timeout_secs.max(1);
    let fresh_peers = state
        .peer_heartbeat_acks
        .iter()
        .filter(|(node_id, acknowledged_at)| {
            configured.contains(node_id.as_str())
                && now.saturating_sub(**acknowledged_at) <= timeout
        })
        .count();
    1 + fresh_peers >= required
}

pub fn local_may_push(cfg: &Config, stream: Option<ClusterStreamIdentity>) -> bool {
    if !cfg.cluster.enabled {
        return true;
    }

    {
        let mut state = cluster_state_write();
        state.local_stream = stream.clone();
    }

    let may_push = local_monitoring_allowed(cfg);
    if !may_push {
        let mut state = cluster_state_write();
        if state.local_stream == stream {
            state.local_stream = None;
        }
    }

    may_push
}

pub fn clear_local_stream() {
    let mut state = cluster_state_write();
    state.local_stream = None;
}

pub fn record_stream_exit(success: bool) {
    let mut state = cluster_state_write();
    let now = now_secs();
    prune_failed_restart_times(&mut state.local_failed_restart_times, now);
    if !success {
        record_recent_time(&mut state.local_failed_restart_times, now);
    }
    state.local_failed_restarts = state.local_failed_restart_times.len() as u32;
}

pub fn record_external_api_result(success: bool) {
    let mut state = cluster_state_write();
    let now = now_secs();
    prune_recent_times(
        &mut state.local_external_api_failure_times,
        now,
        EXTERNAL_API_FAILURE_RETENTION_SECS,
    );
    if success {
        state.local_external_api_failures = state.local_external_api_failure_times.len() as u32;
        return;
    }

    record_recent_time(&mut state.local_external_api_failure_times, now);
    state.local_external_api_failures = state.local_external_api_failure_times.len() as u32;
}

/// Self-fencing (latching a local fault and stopping ffmpeg) is only useful when
/// another node can take over. With 自动转移 off the operator owns failover, so a
/// degraded node keeps pushing instead of fencing itself into a dead stream.
pub(crate) fn fault_fencing_enabled(cfg: &Config) -> bool {
    cfg.cluster.auto_failover
}

pub(crate) fn latch_local_fault(reason: impl Into<String>) {
    let mut state = cluster_state_write();
    state.local_fault_latched = true;
    state.local_fault_reason = Some(reason.into());
}

pub(crate) fn clear_local_fault_latch(state: &mut ClusterState) {
    state.local_fault_latched = false;
    state.local_fault_reason = None;
}

pub(crate) fn clear_recovered_local_network_isolation(
    state: &mut ClusterState,
    network_isolated: bool,
) -> bool {
    if network_isolated
        || !state.local_fault_latched
        || state.local_fault_reason.as_deref() != Some(NETWORK_ISOLATED_REASON)
    {
        return false;
    }

    clear_local_fault_latch(state);
    true
}

pub(crate) fn set_drain_state_inner(
    cfg: &Config,
    node_id: Option<String>,
    draining: bool,
    clear_faults_when_enabled: bool,
) {
    let target = node_id.unwrap_or_else(|| cfg.cluster.node_id.clone());
    let mut state = cluster_state_write();
    let mut local_fault_latched = state.local_fault_latched;
    if target == cfg.cluster.node_id {
        state.local_draining = draining;
        if clear_faults_when_enabled && !draining {
            clear_local_fault_latch(&mut state);
            state.local_failed_restarts = 0;
            state.local_failed_restart_times.clear();
            state.local_external_api_failures = 0;
            state.local_external_api_failure_times.clear();
            state.heartbeat_failures.clear();
            local_fault_latched = false;
        }
    }
    if let Some(node) = state.nodes.get_mut(&target) {
        node.draining = draining;
        if target == cfg.cluster.node_id {
            node.network_unstable = local_fault_latched;
        }
    }
    drop(state);
}

pub(crate) async fn collect_local_snapshot(
    cfg: &Config,
    config_version: String,
) -> ClusterNodeSnapshot {
    let network = collect_network_status();
    let status = get_status_cache();
    let (
        draining,
        mut fault_latched,
        mut fault_reason,
        active_stream,
        failed_restarts,
        external_api_failures,
        network_isolated,
        observed_at,
    ) = {
        let mut state = cluster_state_write();
        let now = now_secs();
        prune_failed_restart_times(&mut state.local_failed_restart_times, now);
        prune_recent_times(
            &mut state.local_external_api_failure_times,
            now,
            cfg.cluster
                .thresholds
                .external_api_failure_window_secs
                .max(1),
        );
        state.local_failed_restarts = state.local_failed_restart_times.len() as u32;
        state.local_external_api_failures = state.local_external_api_failure_times.len() as u32;
        (
            state.local_draining,
            state.local_fault_latched,
            state.local_fault_reason.clone(),
            state.local_stream.clone(),
            state.local_failed_restarts,
            state.local_external_api_failures,
            local_network_isolated_from_state(
                &cfg.cluster,
                &state,
                state.local_external_api_failures,
                now,
            ),
            now,
        )
    };

    let stream_degraded = ffmpeg_restart_degraded(&cfg.cluster, failed_restarts);
    let external_api_degraded = external_api_degraded(&cfg.cluster, external_api_failures);
    if !fault_fencing_enabled(cfg) && fault_latched {
        // Fencing only exists to hand the stream to another node. With
        // 自动转移 off there is nowhere to hand it to, so a latched fault would
        // just kill this node's push forever.
        let mut state = cluster_state_write();
        clear_local_fault_latch(&mut state);
        fault_latched = false;
        fault_reason = None;
    }
    if fault_latched
        && fault_reason.as_deref() == Some(NETWORK_ISOLATED_REASON)
        && !network_isolated
    {
        let mut state = cluster_state_write();
        if clear_recovered_local_network_isolation(&mut state, network_isolated) {
            fault_latched = false;
            fault_reason = None;
        }
    }
    if fault_fencing_enabled(cfg)
        && (stream_degraded || external_api_degraded || network_isolated)
        && !fault_latched
    {
        let reason = if network_isolated {
            NETWORK_ISOLATED_REASON
        } else if external_api_degraded {
            "external_api_unreachable"
        } else {
            "ffmpeg_repeated_failures"
        }
        .to_string();
        latch_local_fault(reason.clone());
        fault_latched = true;
        fault_reason = Some(reason);
    }

    let health = if draining {
        ClusterHealth::unhealthy("draining", false, false)
    } else if fault_latched {
        ClusterHealth::unhealthy(
            fault_reason.unwrap_or_else(|| "node_fault_latched".to_string()),
            false,
            true,
        )
    } else {
        ClusterHealth::healthy()
    };

    ClusterNodeSnapshot {
        node_id: cfg.cluster.node_id.clone(),
        name: cfg.cluster.node_name.clone(),
        api_url: cfg.cluster.public_api_url.clone(),
        priority: cfg.cluster.priority,
        last_seen: Some(observed_at),
        is_local: true,
        role: ClusterNodeRole::Standby,
        health,
        draining,
        network_unstable: fault_latched,
        ffmpeg_running: is_ffmpeg_running().await,
        active_stream,
        status,
        network: Some(network),
        config_version,
        failed_restarts,
        monitor_toggles: monitor_toggle_state_from_config(cfg),
        channel_targets: channel_target_state_from_config(cfg),
    }
}

pub(crate) fn collect_network_status() -> NetworkStatus {
    crate::webui::api::current_network_status()
}

pub(crate) fn ffmpeg_restart_degraded(cluster: &ClusterConfig, failed_restarts: u32) -> bool {
    failed_restarts >= cluster.thresholds.max_failed_restarts.max(1)
}

pub(crate) fn external_api_degraded(cluster: &ClusterConfig, failures: u32) -> bool {
    failures >= cluster.thresholds.max_external_api_failures.max(1)
}

#[cfg(test)]
pub(crate) fn local_network_isolated(
    cluster: &ClusterConfig,
    heartbeat_failures: &HashMap<String, u32>,
    peer_last_seen: &HashMap<String, Option<u64>>,
    external_api_failures: u32,
    now: u64,
) -> bool {
    local_network_isolated_with(cluster, external_api_failures, |peer| {
        let inbound_is_fresh = peer_last_seen
            .get(&peer.node_id)
            .and_then(|last_seen| *last_seen)
            .is_some_and(|last_seen| {
                now.saturating_sub(last_seen) <= cluster.failover_timeout_secs.max(1)
            });
        if inbound_is_fresh {
            return false;
        }

        let failures = heartbeat_failures
            .get(&peer.node_id)
            .copied()
            .unwrap_or_default();
        failures >= HEARTBEAT_FAILURE_THRESHOLD
    })
}

pub(crate) fn local_network_isolated_from_state(
    cluster: &ClusterConfig,
    state: &ClusterState,
    external_api_failures: u32,
    now: u64,
) -> bool {
    local_network_isolated_with(cluster, external_api_failures, |peer| {
        let inbound_is_fresh = state
            .nodes
            .get(&peer.node_id)
            .and_then(|node| node.last_seen)
            .is_some_and(|last_seen| {
                now.saturating_sub(last_seen) <= cluster.failover_timeout_secs.max(1)
            });
        if inbound_is_fresh {
            return false;
        }

        state
            .heartbeat_failures
            .get(&peer.node_id)
            .copied()
            .unwrap_or_default()
            >= HEARTBEAT_FAILURE_THRESHOLD
    })
}

pub(crate) fn local_network_isolated_with(
    cluster: &ClusterConfig,
    external_api_failures: u32,
    mut peer_failed: impl FnMut(&crate::config::ClusterPeer) -> bool,
) -> bool {
    let peer_count = cluster.peers.len();
    if peer_count == 0 {
        return false;
    }
    if !external_api_degraded(cluster, external_api_failures) {
        return false;
    }

    let required_failed_peers = peer_count / 2 + 1;
    let failed_peers = cluster
        .peers
        .iter()
        .filter(|peer| peer_failed(peer))
        .count();

    failed_peers >= required_failed_peers
}

pub(crate) fn prune_failed_restart_times(times: &mut Vec<u64>, now: u64) {
    prune_recent_times(times, now, FFMPEG_FAILURE_WINDOW_SECS);
}

pub(crate) fn record_recent_time(times: &mut Vec<u64>, time: u64) {
    if times.last().is_none_or(|last| *last <= time) {
        times.push(time);
        return;
    }

    let insert_at = times.partition_point(|recorded| *recorded <= time);
    times.insert(insert_at, time);
}

pub(crate) fn prune_recent_times(times: &mut Vec<u64>, now: u64, window_secs: u64) {
    debug_assert!(
        times.windows(2).all(|window| window[0] <= window[1]),
        "recent timestamp windows must be sorted for prefix pruning"
    );
    let first_recent = times.partition_point(|time| now.saturating_sub(*time) > window_secs);
    if first_recent > 0 {
        times.drain(0..first_recent);
    }
}
