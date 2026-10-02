//! Drain, fault latch, isolation, and “may this node monitor/push?” gates.

use super::election::configured_node_ids;
use super::state::{cluster_state_read, cluster_state_write, now_secs, ClusterState};
use super::types::*;
use crate::config::{ClusterConfig, Config};
use crate::plugins::is_danmaku_commands_enabled;
use crate::webui::state::{get_status_cache, NetworkStatus};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

#[cfg(test)]
use std::collections::HashMap;

pub(crate) const FFMPEG_FAILURE_WINDOW_SECS: u64 = 60 * 60;

pub(crate) const EXTERNAL_API_FAILURE_RETENTION_SECS: u64 = 60 * 60;

pub(crate) const HEARTBEAT_FAILURE_THRESHOLD: u32 = 3;

/// The signed fencing policy promises a bounded stop even if cluster settings
/// drift or the operator configured an excessively long heartbeat interval.
pub(crate) const EXECUTION_QUORUM_MAX_SECS: u64 = 120;
const EXECUTION_WATCHDOG_PERIOD: Duration = Duration::from_secs(1);
static EXECUTION_WATCHDOG: OnceLock<tokio::task::JoinHandle<()>> = OnceLock::new();
static EXECUTION_WATCHDOG_READY: AtomicBool = AtomicBool::new(false);

pub(crate) fn fencing_policy_ready() -> bool {
    EXECUTION_WATCHDOG_READY.load(Ordering::Acquire)
        && EXECUTION_WATCHDOG
            .get()
            .is_some_and(|task| !task.is_finished())
}

/// This safety task belongs to the process, not the admin listener. Closing or
/// restarting that listener cannot revoke a fencing promise already pinned by
/// peers while local monitors are still running.
pub(crate) fn start_execution_fence_watchdog() {
    EXECUTION_WATCHDOG.get_or_init(|| {
        tokio::spawn(async {
            let mut ticks = tokio::time::interval(EXECUTION_WATCHDOG_PERIOD);
            ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                ticks.tick().await;
                cluster_state_write().execution_fencing_active = true;
                let blocked = match crate::config::load_config().await {
                    Ok(cfg) => local_monitoring_block_reason(&cfg).is_some(),
                    // A failed config read cannot extend a previously issued
                    // policy's execution lease.
                    Err(_) => true,
                };
                if blocked {
                    crate::plugins::set_manual_restart();
                    clear_local_stream();
                    // No is-running probe or normal stop here: both can wait for
                    // the supervisor mutex held by network/config preparation.
                    crate::plugins::ffmpeg::fence_ffmpeg().await;
                }
                // First confirm that the safety loop reached the independent
                // supervisor fence before the API may attest its stop bound.
                EXECUTION_WATCHDOG_READY.store(true, Ordering::Release);
            }
        })
    });
}

pub fn local_monitoring_allowed(cfg: &Config) -> bool {
    local_monitoring_block_reason(cfg).is_none()
}

pub(crate) fn notify_monitoring_changed() {
    crate::AppState::process_cluster_monitoring_notify().notify_waiters();
}

/// Wake on owner/acknowledgement changes rather than spending a full heartbeat
/// interval asleep after the cluster has already allowed this node to execute.
pub(crate) async fn wait_for_local_monitoring(cfg: &Config) {
    loop {
        // Register before checking the gate so a concurrent confirmation
        // cannot be lost between the check and the await.
        let changed = crate::AppState::process_cluster_monitoring_notify().notified();
        tokio::pin!(changed);
        changed.as_mut().enable();
        if local_monitoring_allowed(cfg) {
            return;
        }
        changed.await;
    }
}

/// `None` means this node may keep monitoring/pushing; `Some(reason)` is a
/// human-readable Chinese reason suitable for logging.
pub(crate) fn local_monitoring_block_reason(cfg: &Config) -> Option<String> {
    if let Some(reason) = super::membership::membership_block_reason() {
        return Some(reason);
    }
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
    if state.local_execution_held || state.pending_handoff_source.is_some() {
        return Some("集群节点交接尚未确认，暂停本节点执行".to_string());
    }
    if !state_has_fresh_quorum(state, cfg, now) {
        return Some("集群心跳多数派已丢失".to_string());
    }
    if !state_has_owner_agreement(state, cfg, now) {
        return Some("集群活跃节点存在分歧或尚未获得多数节点确认".to_string());
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
    if state.execution_fencing_active {
        return state_has_bounded_execution_quorum(state, cfg, Instant::now());
    }
    let configured = configured_node_ids(cfg);
    let required = configured.len() / 2 + 1;
    let timeout = cfg.cluster.failover_timeout_secs.max(1);
    let fresh_peers = state
        .peer_heartbeat_acks
        .iter()
        .filter(|(node_id, acknowledged_at)| {
            node_id.as_str() != cfg.cluster.node_id
                && configured.contains(node_id.as_str())
                && now
                    .checked_sub(**acknowledged_at)
                    .is_some_and(|age| age <= timeout)
        })
        .count();
    1 + fresh_peers >= required
}

pub(crate) fn state_has_bounded_execution_quorum(
    state: &ClusterState,
    cfg: &Config,
    now: Instant,
) -> bool {
    let configured = configured_node_ids(cfg);
    let required = configured.len() / 2 + 1;
    let timeout = Duration::from_secs(
        cfg.cluster
            .failover_timeout_secs
            .clamp(1, EXECUTION_QUORUM_MAX_SECS),
    );
    let fresh_peers = state
        .peer_heartbeat_observed
        .iter()
        .filter(|(node_id, observed)| {
            node_id.as_str() != cfg.cluster.node_id
            && configured.contains(node_id.as_str())
            // Clearing old-revision acknowledgements also invalidates their
            // monotonic observations, before the next heartbeat can arrive.
            && state.peer_heartbeat_acks.contains_key(node_id.as_str())
            && now.checked_duration_since(**observed).is_some_and(|age| age <= timeout)
        })
        .count();
    1 + fresh_peers >= required
}

/// Fresh, direct owner observations are stronger than connectivity, but are
/// still not quorum-granted leases. In particular, this cannot fence a paused
/// process or an external publisher. Never use the display lease as authority.
pub(crate) fn state_has_owner_agreement(state: &ClusterState, cfg: &Config, now: u64) -> bool {
    let Some(owner) = state.active_owner.as_deref() else {
        return false;
    };
    let members = configured_node_ids(cfg);
    let timeout = cfg.cluster.failover_timeout_secs.max(1);
    let mut confirmations = 1;
    for (peer, view) in &state.peer_owner_views {
        if peer == &cfg.cluster.node_id || !members.contains(peer.as_str()) {
            continue;
        }
        if now
            .checked_sub(view.received_at)
            .is_none_or(|age| age > timeout)
        {
            continue;
        }
        // Membership changes invalidate old confirmations, and a known
        // disagreement vetoes execution even if other peers form a majority.
        if view.members.len() != members.len()
            || !members.iter().all(|member| view.members.contains(*member))
            || view.owner.as_deref().is_some_and(|claim| claim != owner)
        {
            return false;
        }
        if view.confirmed_by_heartbeat && view.owner.as_deref() == Some(owner) {
            confirmations += 1;
        }
    }
    confirmations > members.len() / 2
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

pub(crate) fn record_external_api_result(success: bool) {
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
    let network = collect_network_status().await;
    let mut status = get_status_cache();
    if let Some(ref mut status) = status {
        // Config stays on during a restream; the processor that accepts `%转播%` does not.
        status.bilibili.enable_danmaku_command = is_danmaku_commands_enabled();
    }
    let (
        draining,
        mut fault_latched,
        mut fault_reason,
        active_stream,
        failed_restarts,
        external_api_failures,
        network_isolated,
        observed_at,
        self_check,
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
            super::self_check::self_check_status(
                state.local_self_check.as_ref(),
                &cfg.cluster,
                std::time::Instant::now(),
            ),
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
        tracing::warn!(
            "集群本节点故障: {}（外部 API 失败 {} 次，ffmpeg 失败 {} 次），暂停执行并等待自动转移",
            reason,
            external_api_failures,
            failed_restarts
        );
        latch_local_fault(reason.clone());
        fault_latched = true;
        fault_reason = Some(reason);
    }

    let health = if fault_latched {
        ClusterHealth::unhealthy(
            fault_reason.unwrap_or_else(|| "node_fault_latched".to_string()),
            false,
            true,
        )
    } else if draining {
        ClusterHealth::unhealthy("draining", false, false)
    } else {
        ClusterHealth::healthy()
    };

    ClusterNodeSnapshot {
        self_check,
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
        ffmpeg_running: network.ffmpeg_running,
        active_stream,
        status,
        network: Some(network),
        config_version,
        failed_restarts,
        monitor_toggles: monitor_toggle_state_from_config(cfg),
        channel_targets: channel_target_state_from_config(cfg),
        yt_index_version: super::yt_index::advertised_version(),
        yt_index: super::yt_index::node_state(),
        websub: crate::plugins::youtube_websub::counts().map(|(verified, pending, failed)| {
            super::types::WebSubCounts {
                verified,
                pending,
                failed,
            }
        }),
    }
}

pub(crate) async fn collect_network_status() -> NetworkStatus {
    crate::webui::api::current_network_status().await
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
    let self_check_failed = super::self_check::self_check_status(
        state.local_self_check.as_ref(),
        cluster,
        std::time::Instant::now(),
    )
    .is_some_and(|status| status.state == super::self_check::SelfCheckState::Unreachable);
    local_network_isolated_by(
        cluster,
        self_check_failed || external_api_degraded(cluster, external_api_failures),
        |peer| {
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
        },
    )
}

#[cfg(test)]
pub(crate) fn local_network_isolated_with(
    cluster: &ClusterConfig,
    external_api_failures: u32,
    peer_failed: impl FnMut(&crate::config::ClusterPeer) -> bool,
) -> bool {
    local_network_isolated_by(
        cluster,
        external_api_degraded(cluster, external_api_failures),
        peer_failed,
    )
}

fn local_network_isolated_by(
    cluster: &ClusterConfig,
    local_path_failed: bool,
    mut peer_failed: impl FnMut(&crate::config::ClusterPeer) -> bool,
) -> bool {
    let peer_count = cluster.peers.len();
    if peer_count == 0 {
        return false;
    }
    if !local_path_failed {
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
