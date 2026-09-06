//! Cluster status snapshots: persist vs view, merge, and UI change publish.

use super::election::{
    adopt_owner_view, choose_owner_with_configured, clear_invalid_forced_owner,
    configured_node_ids, is_stale,
};
use super::fencing::collect_local_snapshot;
use super::state::{
    cluster_state_read, cluster_state_write, now_secs, ClusterState, PeerOwnerView,
};
use super::types::*;
use super::version::monitored_config_version;
use crate::config::Config;
use std::collections::{hash_map::DefaultHasher, HashSet};
use std::hash::{Hash, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};

pub(crate) static LAST_CLUSTER_UI_SIG: AtomicU64 = AtomicU64::new(0);

pub(crate) fn cache_current_owner_monitor_state(state: &mut ClusterState) {
    let Some(owner) = state.active_owner.as_deref() else {
        return;
    };
    let Some(node) = state.nodes.get(owner) else {
        return;
    };

    let channel_targets =
        channel_targets_configured(&node.channel_targets).then(|| node.channel_targets.clone());

    if node_monitor_toggles_are_known(node) {
        state.last_known_active_toggles = Some(node.monitor_toggles.clone());
    }
    if let Some(channel_targets) = channel_targets {
        state.last_known_active_channel_targets = Some(channel_targets);
    }
}

pub(crate) fn current_active_owner() -> Option<String> {
    cluster_state_read().active_owner.clone()
}

pub fn local_node_is_active_owner(cfg: &Config) -> bool {
    if !cfg.cluster.enabled {
        return true;
    }

    current_active_owner().as_deref() == Some(cfg.cluster.node_id.as_str())
}

pub async fn get_cluster_status() -> Result<ClusterStatus, String> {
    let cfg = crate::config::load_config()
        .await
        .map_err(|e| e.to_string())?;
    Ok(get_cluster_status_for_config(&cfg).await)
}

pub async fn get_cluster_status_for_config(cfg: &Config) -> ClusterStatus {
    let config_version = monitored_config_version(cfg);
    if !cfg.cluster.enabled {
        return compute_cluster_status_view(cfg, config_version);
    }
    let local = collect_local_snapshot(cfg, config_version.clone()).await;
    update_node(local, &cfg.cluster.node_id);
    compute_cluster_status_view(cfg, config_version)
}

pub fn force_failover(cfg: &Config, target_node_id: Option<String>) -> ClusterStatus {
    {
        let mut state = cluster_state_write();
        state.forced_owner = target_node_id;
        state.lease_until = 0;
    }
    compute_cluster_status(cfg)
}

pub(crate) fn heartbeat_response_is_valid(status: &ClusterStatus, peer_node_id: &str) -> bool {
    status.local_node_id == peer_node_id
        && status.nodes.iter().any(|node| node.node_id == peer_node_id)
}

pub(crate) fn compute_cluster_status(cfg: &Config) -> ClusterStatus {
    compute_cluster_status_with_version(cfg, monitored_config_version(cfg))
}

pub(crate) fn compute_cluster_status_with_version(
    cfg: &Config,
    config_version: String,
) -> ClusterStatus {
    let status = build_cluster_status_with_version(cfg, config_version);
    publish_cluster_status_change(&status);
    status
}

pub(crate) fn compute_cluster_status_view(cfg: &Config, config_version: String) -> ClusterStatus {
    let status = build_cluster_status_view(cfg, config_version);
    publish_cluster_status_change(&status);
    status
}

/// Hash of the fields the WebUI cluster panel renders. Volatile fields that
/// change every heartbeat (last_seen, lease, network metrics, embedded status
/// snapshots) are excluded so an event only fires on a meaningful change.
pub(crate) fn cluster_ui_signature(status: &ClusterStatus) -> u64 {
    let mut hasher = DefaultHasher::new();
    status.enabled.hash(&mut hasher);
    status.local_node_id.hash(&mut hasher);
    status.active_owner.hash(&mut hasher);
    status.auto_failover.hash(&mut hasher);
    status.config_version.hash(&mut hasher);
    for node in &status.nodes {
        node.node_id.hash(&mut hasher);
        node.name.hash(&mut hasher);
        node.api_url.hash(&mut hasher);
        node.priority.hash(&mut hasher);
        node.role.hash(&mut hasher);
        node.health.hash(&mut hasher);
        node.draining.hash(&mut hasher);
        node.network_unstable.hash(&mut hasher);
        node.self_check
            .as_ref()
            .map(|check| {
                (
                    &check.state,
                    check.consecutive_failures,
                    check.latency_ms,
                    &check.failure,
                )
            })
            .hash(&mut hasher);
        node.ffmpeg_running.hash(&mut hasher);
        node.last_seen.is_some().hash(&mut hasher);
        node.active_stream.hash(&mut hasher);
        node.config_version.hash(&mut hasher);
        node.monitor_toggles.hash(&mut hasher);
        node.channel_targets.hash(&mut hasher);
    }
    hasher.finish()
}

pub(crate) fn publish_cluster_status_change(status: &ClusterStatus) {
    let signature = cluster_ui_signature(status);
    if LAST_CLUSTER_UI_SIG.swap(signature, Ordering::AcqRel) != signature {
        crate::webui::events::publish(crate::webui::events::CLUSTER);
    }
}

pub(crate) fn build_cluster_status_with_version(
    cfg: &Config,
    config_version: String,
) -> ClusterStatus {
    if !cfg.cluster.enabled {
        return disabled_cluster_status(cfg, config_version);
    }

    let now = now_secs();
    let mut state = cluster_state_write();
    build_status_from_state(&mut state, cfg, config_version, now)
}

/// Same status pipeline on a cloned state: no write lock and no persisted
/// election/lease updates. The worker, heartbeat, and mutation paths own the
/// authoritative state; read paths only need a consistent view of it.
pub(crate) fn build_cluster_status_view(cfg: &Config, config_version: String) -> ClusterStatus {
    if !cfg.cluster.enabled {
        return disabled_cluster_status(cfg, config_version);
    }

    let now = now_secs();
    let mut state = cluster_state_read().clone();
    build_status_from_state(&mut state, cfg, config_version, now)
}

pub(crate) fn disabled_cluster_status(cfg: &Config, config_version: String) -> ClusterStatus {
    ClusterStatus {
        enabled: false,
        local_node_id: cfg.cluster.node_id.clone(),
        active_owner: None,
        lease_until: None,
        config_version,
        auto_failover: cfg.cluster.auto_failover,
        public_status: cfg.cluster.public_status.clone(),
        nodes: Vec::new(),
    }
}

pub(crate) fn build_status_from_state(
    state: &mut ClusterState,
    cfg: &Config,
    config_version: String,
    now: u64,
) -> ClusterStatus {
    let configured = configured_node_ids(cfg);
    ensure_configured_nodes(state, cfg, now, &configured);
    prune_peer_observations(state, cfg, now, &configured);
    normalize_node_health(state, cfg, now);
    clear_invalid_forced_owner(state, cfg, now, &configured);

    let chosen = choose_owner_with_configured(state, cfg, now, &configured);
    hold_new_local_owner(state, &chosen, cfg);
    state.active_owner = chosen.clone();
    cache_current_owner_monitor_state(state);
    state.lease_until = if chosen.is_some() {
        now + cfg.cluster.lease_ttl_secs.max(1)
    } else {
        0
    };
    let active_owner = state.active_owner.clone();
    let lease_until = (state.lease_until > 0).then_some(state.lease_until);
    let mut nodes: Vec<_> = state.nodes.values().cloned().collect();
    for node in &mut nodes {
        node.is_local = node.node_id == cfg.cluster.node_id;
        node.role = if node.draining {
            ClusterNodeRole::Draining
        } else if node.network_unstable || !node.health.healthy {
            ClusterNodeRole::Unhealthy
        } else if Some(node.node_id.as_str()) == chosen.as_deref() {
            ClusterNodeRole::Active
        } else {
            ClusterNodeRole::Standby
        };
    }
    nodes.sort_by(|a, b| a.node_id.cmp(&b.node_id));

    ClusterStatus {
        enabled: true,
        local_node_id: cfg.cluster.node_id.clone(),
        active_owner,
        lease_until,
        config_version,
        auto_failover: cfg.cluster.auto_failover,
        public_status: cfg.cluster.public_status.clone(),
        nodes,
    }
}

pub(crate) fn normalize_node_health(state: &mut ClusterState, cfg: &Config, now: u64) {
    for node in state.nodes.values_mut() {
        if node.last_seen.is_none() {
            node.health = ClusterHealth::unhealthy("waiting_for_heartbeat", true, false);
            continue;
        }

        let stale = is_stale(node, cfg, now);
        if stale {
            node.health = ClusterHealth::unhealthy("heartbeat_timeout", true, false);
        } else if node.network_unstable {
            let reason = if node.health.stream_degraded {
                node.health.reason.clone()
            } else {
                NETWORK_UNSTABLE_REASON.to_string()
            };
            node.health = ClusterHealth::unhealthy(reason, false, node.health.stream_degraded);
        } else if node.health.stream_degraded {
            node.health.healthy = false;
            node.health.reason = "stream_metrics_degraded".to_string();
        } else if node.draining {
            node.health = ClusterHealth::unhealthy("draining", false, false);
        } else {
            // Operator-controlled drain/fault flags can be cleared immediately
            // before a manual handoff. Recover the cached snapshot here so the
            // takeover eligibility check does not keep rejecting the freshly
            // enabled node until its next heartbeat.
            node.health = ClusterHealth::healthy();
        }
    }
}

pub(crate) fn ensure_configured_nodes(
    state: &mut ClusterState,
    cfg: &Config,
    now: u64,
    configured: &HashSet<&str>,
) {
    state
        .nodes
        .retain(|node_id, _| configured.contains(node_id.as_str()));
    state
        .heartbeat_failures
        .retain(|node_id, _| configured.contains(node_id.as_str()));
    state
        .peer_heartbeat_acks
        .retain(|node_id, _| configured.contains(node_id.as_str()));
    state
        .peer_owner_views
        .retain(|node_id, _| configured.contains(node_id.as_str()));

    state
        .nodes
        .entry(cfg.cluster.node_id.clone())
        .or_insert_with(|| {
            empty_node(
                &cfg.cluster.node_id,
                &cfg.cluster.node_name,
                &cfg.cluster.public_api_url,
                cfg.cluster.priority,
                true,
                now,
            )
        });

    for peer in &cfg.cluster.peers {
        state.nodes.entry(peer.node_id.clone()).or_insert_with(|| {
            empty_node(
                &peer.node_id,
                &peer.name,
                &peer.api_url,
                peer.priority,
                false,
                now,
            )
        });
    }
}

pub(crate) fn empty_node(
    node_id: &str,
    name: &str,
    api_url: &str,
    priority: i32,
    is_local: bool,
    now: u64,
) -> ClusterNodeSnapshot {
    ClusterNodeSnapshot {
        self_check: None,
        node_id: node_id.to_string(),
        name: if name.is_empty() {
            node_id.to_string()
        } else {
            name.to_string()
        },
        api_url: api_url.to_string(),
        priority,
        last_seen: if is_local { Some(now) } else { None },
        is_local,
        role: ClusterNodeRole::Unhealthy,
        health: ClusterHealth::unhealthy("waiting_for_heartbeat", !is_local, false),
        draining: false,
        network_unstable: false,
        ffmpeg_running: false,
        active_stream: None,
        status: None,
        network: None,
        config_version: String::new(),
        failed_restarts: 0,
        monitor_toggles: all_monitor_toggles_off(),
        channel_targets: ChannelTargetState::default(),
    }
}

pub(crate) fn canonicalize_node_membership(node: &mut ClusterNodeSnapshot, cfg: &Config) -> bool {
    if node.node_id == cfg.cluster.node_id {
        node.name = cfg.cluster.node_name.clone();
        node.api_url = cfg.cluster.public_api_url.clone();
        node.priority = cfg.cluster.priority;
        return true;
    }
    let Some(peer) = cfg
        .cluster
        .peers
        .iter()
        .find(|peer| peer.node_id == node.node_id)
    else {
        return false;
    };
    node.name = peer.name.clone();
    node.api_url = peer.api_url.clone();
    node.priority = peer.priority;
    true
}

pub(crate) fn update_node(mut node: ClusterNodeSnapshot, local_node_id: &str) {
    node.last_seen = node.last_seen.or_else(|| Some(now_secs()));
    let mut state = cluster_state_write();
    node.is_local = node.node_id == local_node_id;
    if node.is_local {
        node.draining = state.local_draining;
        node.network_unstable = state.local_fault_latched;
        node.active_stream = state.local_stream.clone();
        node.failed_restarts = state.local_failed_restarts;
    }
    let prev_gate = state
        .nodes
        .get(&node.node_id)
        .and_then(|existing| existing.status.as_ref())
        .map(|status| status.bilibili.enable_danmaku_command);
    let next_gate = node
        .status
        .as_ref()
        .map(|status| status.bilibili.enable_danmaku_command);
    let owner_changed =
        state.active_owner.as_deref() == Some(node.node_id.as_str()) && prev_gate != next_gate;
    state.nodes.insert(node.node_id.clone(), node);
    drop(state);
    if owner_changed {
        crate::webui::public::snapshot::invalidate_public_status_snapshot();
    }
}

pub(crate) fn merge_cluster_status_from_direct_peer(
    status: ClusterStatus,
    peer_node_id: &str,
    cfg: &Config,
) -> Result<(), String> {
    if !heartbeat_response_is_valid(&status, peer_node_id) {
        return Err(format!(
            "节点 {} 返回的集群状态身份或时间戳无效",
            peer_node_id
        ));
    }

    merge_direct_peer_status(status, peer_node_id, cfg, false);
    Ok(())
}

#[cfg(test)]
pub(crate) fn merge_cluster_status_inner(
    status: ClusterStatus,
    direct_peer_id: Option<&str>,
    cfg: Option<&Config>,
) {
    let mut state = cluster_state_write();
    let received_at = now_secs();
    merge_cluster_status_into(&mut state, status, direct_peer_id, cfg, received_at);
}

pub(crate) fn merge_direct_peer_status(
    status: ClusterStatus,
    peer_node_id: &str,
    cfg: &Config,
    heartbeat_ack: bool,
) -> bool {
    let mut state = cluster_state_write();
    let received_at = now_secs();
    state.heartbeat_failures.remove(peer_node_id);
    if heartbeat_ack {
        state
            .peer_heartbeat_acks
            .insert(peer_node_id.to_string(), received_at);
        state.peer_owner_views.insert(
            peer_node_id.to_string(),
            PeerOwnerView {
                owner: status.active_owner.clone().filter(|_| status.enabled),
                received_at,
                members: status
                    .nodes
                    .iter()
                    .map(|node| node.node_id.clone())
                    .collect(),
                confirmed_by_heartbeat: true,
            },
        );
    } else if let Some(view) = state.peer_owner_views.get_mut(peer_node_id) {
        // A control reply can revoke a previous confirmation, but cannot
        // refresh it: only the heartbeat exchange supplies fresh evidence.
        let owner = status.active_owner.clone().filter(|_| status.enabled);
        if view.owner != owner {
            view.confirmed_by_heartbeat = false;
        }
        view.owner = owner;
    }
    let prev_gate = owner_danmaku_gate(&state);
    merge_cluster_status_into(
        &mut state,
        status,
        Some(peer_node_id),
        Some(cfg),
        received_at,
    );
    let next_gate = owner_danmaku_gate(&state);
    let peer_is_owner = state.active_owner.as_deref() == Some(peer_node_id);
    drop(state);
    if prev_gate != next_gate {
        crate::webui::public::snapshot::invalidate_public_status_snapshot();
    }
    peer_is_owner
}

fn owner_danmaku_gate(state: &ClusterState) -> Option<bool> {
    let owner = state.active_owner.as_deref()?;
    state
        .nodes
        .get(owner)?
        .status
        .as_ref()
        .map(|status| status.bilibili.enable_danmaku_command)
}

pub(crate) fn merge_cluster_status_into(
    state: &mut ClusterState,
    status: ClusterStatus,
    direct_peer_id: Option<&str>,
    cfg: Option<&Config>,
    received_at: u64,
) {
    if let (Some(peer_node_id), Some(cfg)) = (direct_peer_id, cfg) {
        record_peer_observations(state, peer_node_id, &status.nodes, cfg, received_at);
    }
    for mut node in status.nodes {
        if let Some(cfg) = cfg {
            if !canonicalize_node_membership(&mut node, cfg) {
                continue;
            }
        }
        if let Some(local) = state.nodes.get(&node.node_id) {
            if local.is_local {
                continue;
            }
            if direct_peer_id != Some(node.node_id.as_str()) && snapshot_is_older(&node, local) {
                continue;
            }
        }
        node.last_seen = merged_status_last_seen(
            &node.node_id,
            direct_peer_id,
            state.nodes.get(&node.node_id),
            received_at,
        );
        node.is_local = false;
        state.nodes.insert(node.node_id.clone(), node);
    }
    let previous_owner = state.active_owner.clone();
    adopt_owner_view(
        state,
        status.active_owner,
        status.lease_until.unwrap_or(0),
        cfg.map(|cfg| cfg.cluster.auto_failover).unwrap_or(true),
        direct_peer_id,
    );
    if let Some(cfg) = cfg {
        let chosen = state.active_owner.clone();
        state.active_owner = previous_owner;
        hold_new_local_owner(state, &chosen, cfg);
        state.active_owner = chosen;
    }
    cache_current_owner_monitor_state(state);
}

fn hold_new_local_owner(state: &mut ClusterState, chosen: &Option<String>, cfg: &Config) {
    if chosen.as_deref() == Some(cfg.cluster.node_id.as_str())
        && state.active_owner != *chosen
        && state.active_owner.is_some()
    {
        state.pending_handoff_source = state.active_owner.clone();
    }
}

pub(crate) fn merged_status_last_seen(
    node_id: &str,
    direct_peer_id: Option<&str>,
    existing: Option<&ClusterNodeSnapshot>,
    received_at: u64,
) -> Option<u64> {
    if direct_peer_id == Some(node_id) {
        Some(received_at)
    } else {
        existing.and_then(|node| node.last_seen)
    }
}

pub(crate) fn record_peer_observations(
    state: &mut ClusterState,
    observer_node_id: &str,
    nodes: &[ClusterNodeSnapshot],
    cfg: &Config,
    now: u64,
) {
    let configured = configured_node_ids(cfg);
    for observations in state.peer_observations.values_mut() {
        observations.remove(observer_node_id);
    }

    for node in nodes {
        if node.node_id == cfg.cluster.node_id || node.node_id == observer_node_id {
            continue;
        }
        if !configured.contains(node.node_id.as_str()) {
            continue;
        }
        if node.health.healthy && !node.health.stale && node.last_seen.is_some() {
            state
                .peer_observations
                .entry(node.node_id.clone())
                .or_default()
                .insert(observer_node_id.to_string(), now);
        }
    }
    state.peer_observations.retain(|node_id, observations| {
        configured.contains(node_id.as_str()) && !observations.is_empty()
    });
}

pub(crate) fn prune_peer_observations(
    state: &mut ClusterState,
    cfg: &Config,
    now: u64,
    configured: &HashSet<&str>,
) {
    let timeout = cfg.cluster.failover_timeout_secs.max(1);
    state.peer_observations.retain(|node_id, observations| {
        configured.contains(node_id.as_str()) && {
            observations.retain(|observer, observed_at| {
                configured.contains(observer.as_str())
                    && now.saturating_sub(*observed_at) <= timeout
            });
            !observations.is_empty()
        }
    });
}

pub(crate) fn snapshot_is_older(
    incoming: &ClusterNodeSnapshot,
    existing: &ClusterNodeSnapshot,
) -> bool {
    match (incoming.last_seen, existing.last_seen) {
        (Some(incoming), Some(existing)) => incoming < existing,
        (None, Some(_)) => true,
        _ => false,
    }
}

pub fn set_drain_state(cfg: &Config, node_id: Option<String>, draining: bool) -> ClusterStatus {
    super::fencing::set_drain_state_inner(cfg, node_id, draining, true);
    compute_cluster_status(cfg)
}

pub fn set_local_drain_state_preserving_fault(cfg: &Config, draining: bool) -> ClusterStatus {
    super::fencing::set_drain_state_inner(cfg, None, draining, false);
    compute_cluster_status(cfg)
}
