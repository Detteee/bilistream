//! Monitored-config push/apply, node-mode handoff, and toggle cache.

use super::election::{configured_node_ids, node_is_eligible};
use super::fencing::{clear_local_stream, local_monitoring_allowed};
use super::state::{
    cluster_control_timeout, cluster_state_read, cluster_state_write, cluster_switch_lock,
    node_mode_apply_lock, now_secs, ClusterState, CLUSTER_HTTP_CLIENT,
};
use super::status::{
    current_active_owner, force_failover, get_cluster_status,
    merge_cluster_status_from_direct_peer, set_drain_state, set_local_drain_state_preserving_fault,
};
use super::types::*;
use super::version::{
    monitored_config_from_config, monitored_config_integrity_version_from_payload,
};
use crate::config::{load_config, save_config, Config};
use crate::plugins::{set_config_updated, set_manual_restart, stop_ffmpeg};
use crate::webui::state::refresh_status_cache_config_from;
use futures_util::future::join_all;
use std::time::Duration;

pub fn cluster_sync_config_from_config(cfg: &Config) -> ClusterSyncConfigRequest {
    let monitored_config = monitored_config_from_config(cfg);
    let config_version = monitored_config_integrity_version_from_payload(&monitored_config);
    ClusterSyncConfigRequest {
        monitored_config,
        config_version,
    }
}

pub async fn apply_monitor_toggle_state(payload: MonitorToggleState) -> Result<(), String> {
    let mut cfg = crate::config::load_config()
        .await
        .map_err(|e| e.to_string())?;
    apply_monitor_toggle_state_to_config(&mut cfg, &payload);
    save_config(&mut cfg).await.map_err(|e| e.to_string())?;
    refresh_status_cache_config_from(&cfg);
    apply_danmaku_command_runtime_state(payload.enable_danmaku_command).await;
    if monitor_toggles_any_enabled(&payload)
        || current_active_owner().as_deref() == Some(cfg.cluster.node_id.as_str())
    {
        cache_active_monitor_state_from_owner(
            &payload,
            Some(&channel_target_state_from_config(&cfg)),
        );
    }
    set_config_updated();
    Ok(())
}

pub fn cache_active_monitor_state_from_owner(
    toggles: &MonitorToggleState,
    channel_targets: Option<&ChannelTargetState>,
) {
    let mut state = cluster_state_write();
    cache_active_monitor_state(&mut state, toggles, channel_targets);
}

pub fn cache_active_monitor_state_from_peer(
    toggles: MonitorToggleState,
    channel_targets: Option<ChannelTargetState>,
) {
    cache_active_monitor_state_from_owner(&toggles, channel_targets.as_ref());
}

pub(crate) fn cache_active_monitor_state(
    state: &mut ClusterState,
    toggles: &MonitorToggleState,
    channel_targets: Option<&ChannelTargetState>,
) {
    state.last_known_active_toggles = Some(toggles.clone());
    if let Some(targets) = channel_targets.filter(|targets| channel_targets_configured(targets)) {
        state.last_known_active_channel_targets = Some(targets.clone());
    }
}

pub async fn push_active_monitor_state_to_peers(cfg: &Config) -> Result<usize, String> {
    if !cfg.cluster.enabled {
        return Ok(0);
    }

    if !local_monitoring_allowed(cfg) {
        return Ok(0);
    }

    let toggles = monitor_toggle_state_from_config(cfg);
    let channel_targets = channel_target_state_from_config(cfg);
    cache_active_monitor_state_from_owner(&toggles, Some(&channel_targets));

    let request = ClusterActiveMonitorStateRequest {
        sender_node_id: cfg.cluster.node_id.clone(),
        monitor_toggles: toggles,
        channel_targets: Some(channel_targets),
    };
    let client = CLUSTER_HTTP_CLIENT.clone();
    let timeout = cluster_control_timeout(cfg);

    let tasks = cfg
        .cluster
        .peers
        .iter()
        .filter(|peer| peer.node_id != cfg.cluster.node_id)
        .map(|peer| push_active_monitor_state_to_peer(&client, cfg, peer, &request, timeout));
    summarize_peer_push_results(join_all(tasks).await, "部分节点监控开关缓存失败")
}

pub(crate) async fn push_active_monitor_state_to_peer(
    client: &reqwest::Client,
    cfg: &Config,
    peer: &crate::config::ClusterPeer,
    request: &ClusterActiveMonitorStateRequest,
    timeout: Duration,
) -> Result<(), String> {
    let url = format!(
        "{}/api/cluster/cache-active-monitor-state",
        peer.api_url.trim_end_matches('/')
    );
    let response = client
        .post(url)
        .json(request)
        .timeout(timeout)
        .send()
        .await
        .map_err(|e| format!("节点 {} 缓存监控开关失败: {}", peer.node_id, e))?;

    let status = response.status();
    if !status.is_success() {
        return Err(format!(
            "节点 {} 缓存监控开关失败: HTTP {}",
            peer.node_id, status
        ));
    }

    let envelope = response
        .json::<PeerApiResponse<ClusterStatus>>()
        .await
        .map_err(|e| format!("节点 {} 缓存监控开关响应解析失败: {}", peer.node_id, e))?;
    if !envelope.success {
        return Err(format!(
            "节点 {} 缓存监控开关失败: {}",
            peer.node_id,
            envelope.message.unwrap_or_else(|| "缓存被拒绝".to_string())
        ));
    }
    if let Some(status) = envelope.data {
        merge_cluster_status_from_direct_peer(status, &peer.node_id, cfg)
            .map_err(|e| format!("{} {}", peer.node_id, e))?;
    }
    Ok(())
}

pub(crate) fn summarize_peer_push_results(
    results: Vec<Result<(), String>>,
    partial_failure_prefix: &str,
) -> Result<usize, String> {
    let mut synced = 0usize;
    let mut errors = Vec::new();
    for result in results {
        match result {
            Ok(()) => synced += 1,
            Err(e) => errors.push(e),
        }
    }

    if errors.is_empty() {
        Ok(synced)
    } else if synced > 0 {
        Err(format!(
            "{} ({} 成功): {}",
            partial_failure_prefix,
            synced,
            errors.join("; ")
        ))
    } else {
        Err(errors.join("; "))
    }
}

pub(crate) fn apply_node_mode_config_state(
    cfg: &mut Config,
    channel_targets: Option<&ChannelTargetState>,
    monitor_toggles: Option<&MonitorToggleState>,
) {
    if let Some(channel_targets) = channel_targets {
        apply_channel_target_state_to_config(cfg, channel_targets);
    }
    if let Some(monitor_toggles) = monitor_toggles {
        apply_monitor_toggle_state_to_config(cfg, monitor_toggles);
    }
}

pub(crate) async fn apply_danmaku_command_runtime_state(enabled: bool) {
    crate::plugins::enable_danmaku_commands(enabled);
    if enabled {
        if !crate::plugins::is_danmaku_running() {
            crate::plugins::run_danmaku();
        }
    } else if crate::plugins::is_danmaku_running() {
        crate::plugins::stop_danmaku().await;
    }
}

pub async fn sync_monitored_config_after_change(cfg: &Config) -> String {
    if !cfg.cluster.enabled || !cfg.cluster.sync_monitored_channels {
        return String::new();
    }

    match push_monitored_config_to_peers(cfg).await {
        Ok(count) => format!("；已同步到 {} 个节点", count),
        Err(e) => {
            tracing::warn!("Cluster monitored config auto-sync failed: {}", e);
            format!("；集群同步失败: {}", e)
        }
    }
}

pub async fn apply_monitored_config(payload: MonitoredConfig) -> Result<(), String> {
    let mut cfg = crate::config::load_config()
        .await
        .map_err(|e| e.to_string())?;

    write_monitored_json_files(payload.channels_json.clone(), payload.areas_json.clone()).await?;

    apply_monitored_config_to_config(&mut cfg, payload);

    save_config(&mut cfg).await.map_err(|e| e.to_string())?;
    refresh_status_cache_config_from(&cfg);
    crate::webui::state::request_status_refresh();

    set_config_updated();
    Ok(())
}

/// Writes the managed JSON files off the async runtime (the atomic writes
/// fsync).
pub(crate) async fn write_monitored_json_files(
    channels_json: Option<serde_json::Value>,
    areas_json: Option<serde_json::Value>,
) -> Result<(), String> {
    for (name, data) in [("channels.json", channels_json), ("areas.json", areas_json)] {
        if let Some(data) = data {
            let path = super::state::executable_sibling(name)
                .ok_or_else(|| "failed to resolve executable path".to_string())?;
            crate::config::replace_json_file(path, data).await?;
            if name == "areas.json" {
                crate::webui::public::remap_after_areas_change();
            }
        }
    }
    Ok(())
}

pub(crate) fn apply_monitored_config_to_config(cfg: &mut Config, payload: MonitoredConfig) {
    // Monitor toggles are per-node runtime state, not shared configuration:
    // only the active node runs monitors, so a pushed config must never flip
    // them on a standby. Channel targets below are shared, so a standby can
    // take over the same channel on failover.
    let local_enable_danmaku_command = cfg.bililive.enable_danmaku_command;
    let local_enable_youtube_monitor = cfg.enable_youtube_monitor;
    let local_enable_twitch_monitor = cfg.enable_twitch_monitor;
    let local_youtube_enable_monitor = cfg.youtube.enable_monitor;
    let local_twitch_enable_monitor = cfg.twitch.enable_monitor;
    let local_priority_channel_enabled = cfg.priority_channel.enabled;
    let local_priority_channel_auto_restart = cfg.priority_channel.auto_restart;
    cfg.interval = payload.interval;
    cfg.auto_cover = payload.auto_cover;
    cfg.enable_anti_collision = payload.enable_anti_collision;
    cfg.anti_collision_list = payload.anti_collision_list;
    cfg.youtube = payload.youtube;
    cfg.twitch = payload.twitch;
    cfg.priority_channel = payload.priority_channel;

    cfg.bililive.enable_danmaku_command = local_enable_danmaku_command;
    cfg.enable_youtube_monitor = local_enable_youtube_monitor;
    cfg.enable_twitch_monitor = local_enable_twitch_monitor;
    cfg.youtube.enable_monitor = local_youtube_enable_monitor;
    cfg.twitch.enable_monitor = local_twitch_enable_monitor;
    cfg.priority_channel.enabled = local_priority_channel_enabled;
    cfg.priority_channel.auto_restart = local_priority_channel_auto_restart;
    crate::config::update_priority_channel_from_channels(cfg);
}

pub async fn push_monitored_config_to_peers(cfg: &Config) -> Result<usize, String> {
    if !cfg.cluster.enabled || !cfg.cluster.sync_monitored_channels {
        return Err("集群配置同步未启用".to_string());
    }

    let request = cluster_sync_config_from_config(cfg);
    let client = CLUSTER_HTTP_CLIENT.clone();
    let timeout = cluster_control_timeout(cfg);

    let tasks = cfg
        .cluster
        .peers
        .iter()
        .filter(|peer| peer.node_id != cfg.cluster.node_id)
        .map(|peer| push_monitored_config_to_peer(&client, cfg, peer, &request, timeout));
    summarize_peer_push_results(join_all(tasks).await, "部分节点监控频道配置同步失败")
}

pub(crate) async fn push_monitored_config_to_peer(
    client: &reqwest::Client,
    cfg: &Config,
    peer: &crate::config::ClusterPeer,
    request: &ClusterSyncConfigRequest,
    timeout: Duration,
) -> Result<(), String> {
    let url = format!(
        "{}/api/cluster/sync-config",
        peer.api_url.trim_end_matches('/')
    );
    let response = client
        .post(url)
        .json(request)
        .timeout(timeout)
        .send()
        .await
        .map_err(|e| format!("{} {}", peer.node_id, e))?;

    let status = response.status();
    if !status.is_success() {
        return Err(format!("{} HTTP {}", peer.node_id, status));
    }

    let envelope = response
        .json::<PeerApiResponse<ClusterStatus>>()
        .await
        .map_err(|e| format!("{} 响应解析失败: {}", peer.node_id, e))?;
    if !envelope.success {
        return Err(format!(
            "{} {}",
            peer.node_id,
            envelope.message.unwrap_or_else(|| "同步被拒绝".to_string())
        ));
    }
    if let Some(status) = envelope.data {
        merge_cluster_status_from_direct_peer(status, &peer.node_id, cfg)
            .map_err(|e| format!("{} {}", peer.node_id, e))?;
    }
    Ok(())
}

pub(crate) struct SourceConfigSnapshot {
    payload: ClusterSyncConfigRequest,
    toggle_state_authoritative: bool,
}

pub async fn finalize_cluster_node_switch(
    cfg: &Config,
    before: &ClusterStatus,
    source_node_id: &str,
    target_node_id: &str,
    preserve_source_drain: bool,
) -> Result<(), String> {
    finalize_cluster_node_switch_with(
        cfg,
        before,
        source_node_id,
        target_node_id,
        preserve_source_drain,
        false,
    )
    .await
}

/// `automatic`: the heartbeat's failover, which may take over from a source
/// that cannot be reached (`may_take_over_unconfirmed`). Panel switches pass
/// false and always need the source's confirmation.
pub(crate) async fn finalize_cluster_node_switch_with(
    cfg: &Config,
    before: &ClusterStatus,
    source_node_id: &str,
    target_node_id: &str,
    preserve_source_drain: bool,
    automatic: bool,
) -> Result<(), String> {
    // Complete the ordered handoff even if the requesting HTTP client disconnects.
    let cfg = cfg.clone();
    let before = before.clone();
    let source = source_node_id.to_string();
    let target = target_node_id.to_string();
    tokio::spawn(async move {
        finalize_cluster_node_switch_inner(
            &cfg,
            &before,
            &source,
            &target,
            preserve_source_drain,
            automatic,
        )
        .await
    })
    .await
    .map_err(|e| format!("集群交接任务失败: {e}"))?
}

async fn finalize_cluster_node_switch_inner(
    cfg: &Config,
    before: &ClusterStatus,
    source_node_id: &str,
    target_node_id: &str,
    preserve_source_drain: bool,
    automatic: bool,
) -> Result<(), String> {
    let _switch_guard = cluster_switch_lock().lock().await;
    let pending_source = (target_node_id == cfg.cluster.node_id)
        .then(|| cluster_state_read().pending_handoff_source.clone())
        .flatten();
    let source_node_id = pending_source.as_deref().unwrap_or(source_node_id);
    let current_owner = current_active_owner();
    if current_owner.as_deref() != Some(source_node_id)
        && current_owner.as_deref() != Some(target_node_id)
    {
        return Err(format!(
            "取消过期集群切换: 当前活跃节点为 {}, 请求源/目标为 {}/{}",
            current_owner.as_deref().unwrap_or("none"),
            source_node_id,
            target_node_id
        ));
    }
    ensure_handoff_target_is_eligible(cfg, target_node_id)?;
    let client = CLUSTER_HTTP_CLIENT.clone();
    let source_config = match export_cluster_config_from_node(&client, cfg, source_node_id).await {
        Ok(payload) => SourceConfigSnapshot {
            payload,
            toggle_state_authoritative: true,
        },
        Err(e) => {
            tracing::warn!(
                "Failed to export active source config from {}; using local synced config and cached active state: {}",
                source_node_id,
                e
            );
            SourceConfigSnapshot {
                payload: cluster_sync_config_from_config(cfg),
                toggle_state_authoritative: false,
            }
        }
    };
    let source_toggles = resolve_source_monitor_toggles(
        cfg,
        before,
        source_node_id,
        &source_config.payload.monitored_config,
        source_config.toggle_state_authoritative,
    );
    let source_channel_targets = resolve_source_channel_targets(
        cfg,
        before,
        source_node_id,
        &source_config.payload.monitored_config,
    );

    // Never enable the replacement until the source confirms persisted all-off
    // toggles and a stopped process. An automatic failover may take over from a
    // source that cannot be reached once its self-fence has provably run out
    // (`may_take_over_unconfirmed`); anything else leaves the handoff pending.
    if source_node_id != target_node_id {
        let disabled = apply_node_mode_with_retry_classified(
            &client,
            cfg,
            source_node_id,
            ClusterApplyNodeModeRequest {
                monitored_config: None,
                active: false,
                restart: false,
                preserve_drain: preserve_source_drain,
                monitor_toggles: Some(all_monitor_toggles_off()),
                channel_targets: None,
                expected_active_owner: Some(
                    if source_node_id == cfg.cluster.node_id {
                        current_owner.as_deref().unwrap_or(source_node_id)
                    } else {
                        source_node_id
                    }
                    .to_string(),
                ),
                handoff_target_node_id: Some(target_node_id.to_string()),
            },
            "disable_previous_active",
        )
        .await;
        if let Err(e) = disabled {
            let source_last_seen = cluster_state_read()
                .nodes
                .get(source_node_id)
                .and_then(|node| node.last_seen);
            if !(may_take_over_unconfirmed(
                automatic,
                &e,
                source_last_seen,
                &cfg.cluster,
                now_secs(),
            ) && super::fencing::local_has_fresh_quorum(cfg))
            {
                return Err(format!("源节点尚未确认停止，取消接管: {e}"));
            }
            tracing::warn!(
                "源节点 {} 无法连接且已超过自隔离时限，未经确认接管: {}",
                source_node_id,
                e
            );
            cluster_state_write().unconfirmed_demotion = Some(source_node_id.to_string());
        }
    }

    ensure_handoff_target_is_eligible(cfg, target_node_id)?;
    apply_cluster_node_mode_to_node_with_retry(
        &client,
        cfg,
        target_node_id,
        ClusterApplyNodeModeRequest {
            monitored_config: Some(source_config.payload.monitored_config),
            active: true,
            restart: false,
            preserve_drain: false,
            monitor_toggles: Some(source_toggles),
            channel_targets: Some(source_channel_targets),
            expected_active_owner: Some(source_node_id.to_string()),
            handoff_target_node_id: None,
        },
        "enable_new_active",
    )
    .await?;

    if current_active_owner().as_deref() != Some(target_node_id) {
        let status = force_failover(cfg, Some(target_node_id.to_string()));
        if status.active_owner.as_deref() != Some(target_node_id) {
            return Err(format!("目标节点 {} 当前不可接管", target_node_id));
        }
    }

    Ok(())
}

pub(crate) fn ensure_handoff_target_is_eligible(
    cfg: &Config,
    target_node_id: &str,
) -> Result<(), String> {
    let state = cluster_state_read();
    let configured = configured_node_ids(cfg);
    let eligible = state
        .nodes
        .get(target_node_id)
        .is_some_and(|node| node_is_eligible(node, &state, cfg, now_secs(), &configured));
    if eligible {
        Ok(())
    } else {
        Err(format!("目标节点 {} 当前不可接管", target_node_id))
    }
}

pub(crate) fn resolve_source_monitor_toggles(
    cfg: &Config,
    before: &ClusterStatus,
    source_node_id: &str,
    monitored_config: &MonitoredConfig,
    monitored_config_toggles_known: bool,
) -> MonitorToggleState {
    resolve_source_monitor_toggles_with_cache(
        cfg,
        before,
        source_node_id,
        monitored_config,
        monitored_config_toggles_known,
        last_known_active_toggles(),
    )
}

pub(crate) fn resolve_source_monitor_toggles_with_cache(
    cfg: &Config,
    before: &ClusterStatus,
    source_node_id: &str,
    monitored_config: &MonitoredConfig,
    monitored_config_toggles_known: bool,
    cached_toggles: Option<MonitorToggleState>,
) -> MonitorToggleState {
    if source_node_id == cfg.cluster.node_id {
        return monitor_toggle_state_from_config(cfg);
    }

    let from_config = monitor_toggle_state_from_monitored_config(monitored_config);
    if monitored_config_toggles_known {
        // A successful source export is the freshest view available. Heartbeat
        // snapshots and the local cache can lag behind a just-saved toggle
        // change when an operator immediately switches the active node.
        return from_config;
    }

    if let Some(node) = before
        .nodes
        .iter()
        .find(|node| node.node_id == source_node_id)
    {
        if node_monitor_toggles_are_known(node) {
            return node.monitor_toggles.clone();
        }
    }

    if let Some(cached) = cached_toggles {
        return cached;
    }

    monitor_toggle_state_from_config(cfg)
}

pub(crate) fn resolve_source_channel_targets(
    cfg: &Config,
    before: &ClusterStatus,
    source_node_id: &str,
    monitored_config: &MonitoredConfig,
) -> ChannelTargetState {
    if source_node_id == cfg.cluster.node_id {
        let local_targets = channel_target_state_from_config(cfg);
        if channel_targets_configured(&local_targets) {
            return local_targets;
        }
    }

    let from_config = channel_target_state_from_monitored_config(monitored_config);
    if channel_targets_configured(&from_config) {
        return from_config;
    }

    if let Some(node) = before
        .nodes
        .iter()
        .find(|node| node.node_id == source_node_id)
    {
        if channel_targets_configured(&node.channel_targets) {
            return node.channel_targets.clone();
        }
    }

    last_known_active_channel_targets().unwrap_or(from_config)
}

pub(crate) async fn export_cluster_config_from_node(
    client: &reqwest::Client,
    cfg: &Config,
    node_id: &str,
) -> Result<ClusterSyncConfigRequest, String> {
    if node_id == cfg.cluster.node_id {
        return Ok(cluster_sync_config_from_config(cfg));
    }

    let peer = cfg
        .cluster
        .peers
        .iter()
        .find(|peer| peer.node_id == node_id)
        .ok_or_else(|| format!("未找到源节点 {}", node_id))?;
    let url = format!(
        "{}/api/cluster/export-config",
        peer.api_url.trim_end_matches('/')
    );
    let response = client
        .get(url)
        .timeout(cluster_control_timeout(cfg))
        .send()
        .await
        .map_err(|e| format!("读取源节点配置失败: {}", e))?;

    if !response.status().is_success() {
        return Err(format!("读取源节点配置失败: HTTP {}", response.status()));
    }

    let envelope = response
        .json::<PeerApiResponse<ClusterSyncConfigRequest>>()
        .await
        .map_err(|e| format!("解析源节点配置失败: {}", e))?;

    if envelope.success {
        envelope.data.ok_or_else(|| "源节点未返回配置".to_string())
    } else {
        Err(envelope
            .message
            .unwrap_or_else(|| "源节点拒绝导出配置".to_string()))
    }
}

/// When this process started, standing in for a source never heard from.
static PROCESS_STARTED: std::sync::OnceLock<u64> = std::sync::OnceLock::new();

pub(crate) fn mark_process_started() {
    PROCESS_STARTED.get_or_init(now_secs);
}

/// An unreachable source fences itself: without a fresh quorum its ffmpeg
/// loop and heartbeat stop pushing within `failover_timeout`. Past that, plus
/// one loop tick and a margin, nothing can still be pushing there.
pub(crate) fn source_fence_deadline_secs(cluster: &crate::config::ClusterConfig) -> u64 {
    cluster.failover_timeout_secs.max(1) + 2 * cluster.heartbeat_interval_secs.max(1) + 5
}

/// Whether an automatic failover may enable the replacement although the
/// source never confirmed its stop.
pub(crate) fn may_take_over_unconfirmed(
    automatic: bool,
    error: &NodeModeError,
    source_last_seen: Option<u64>,
    cluster: &crate::config::ClusterConfig,
    now: u64,
) -> bool {
    if !automatic || !matches!(error, NodeModeError::Unreachable(_)) {
        return false;
    }
    let Some(heard) = source_last_seen.or_else(|| PROCESS_STARTED.get().copied()) else {
        return false;
    };
    now.saturating_sub(heard) > source_fence_deadline_secs(cluster)
}

/// Delivers the all-off demotion owed to a source taken over without its
/// confirmation, once it answers. One attempt per heartbeat.
pub(crate) async fn retry_unconfirmed_demotion(cfg: &Config) {
    let source = {
        let mut state = cluster_state_write();
        let Some(source) = state.unconfirmed_demotion.clone() else {
            return;
        };
        let still_owed = state.active_owner.as_deref() == Some(cfg.cluster.node_id.as_str())
            && configured_node_ids(cfg).contains(source.as_str());
        if !still_owed {
            state.unconfirmed_demotion = None;
            return;
        }
        source
    };
    let payload = ClusterApplyNodeModeRequest {
        monitored_config: None,
        active: false,
        restart: false,
        preserve_drain: true,
        monitor_toggles: Some(all_monitor_toggles_off()),
        channel_targets: None,
        expected_active_owner: Some(cfg.cluster.node_id.clone()),
        handoff_target_node_id: Some(cfg.cluster.node_id.clone()),
    };
    match apply_node_mode_classified(&CLUSTER_HTTP_CLIENT, cfg, &source, &payload).await {
        Ok(()) => {
            let mut state = cluster_state_write();
            if state.unconfirmed_demotion.as_deref() == Some(source.as_str()) {
                state.unconfirmed_demotion = None;
            }
            tracing::info!("已确认源节点 {} 停止并关闭监控", source);
        }
        Err(e) => tracing::debug!("源节点 {} 仍未确认停止: {}", source, e),
    }
}

/// Three attempts; a refusal on any attempt wins over later timeouts, since
/// the node was alive to answer.
async fn apply_node_mode_with_retry_classified(
    client: &reqwest::Client,
    cfg: &Config,
    node_id: &str,
    payload: ClusterApplyNodeModeRequest,
    phase: &str,
) -> Result<(), NodeModeError> {
    let max_attempts = 3;
    let mut refused: Option<NodeModeError> = None;
    let mut last_error = NodeModeError::Unreachable(String::new());
    for attempt in 1..=max_attempts {
        match apply_node_mode_classified(client, cfg, node_id, &payload).await {
            Ok(()) => return Ok(()),
            Err(e) => {
                tracing::warn!(
                    "Cluster node mode transfer {} attempt {}/{} failed for {}: {}",
                    phase,
                    attempt,
                    max_attempts,
                    node_id,
                    e
                );
                if matches!(e, NodeModeError::Refused(_)) {
                    refused = Some(e.clone());
                }
                last_error = e;
                if attempt < max_attempts {
                    tokio::time::sleep(Duration::from_millis(300 * attempt as u64)).await;
                }
            }
        }
    }
    Err(refused.unwrap_or(last_error))
}

pub(crate) async fn apply_cluster_node_mode_to_node_with_retry(
    client: &reqwest::Client,
    cfg: &Config,
    node_id: &str,
    payload: ClusterApplyNodeModeRequest,
    phase: &str,
) -> Result<(), String> {
    apply_node_mode_with_retry_classified(client, cfg, node_id, payload, phase)
        .await
        .map_err(|e| e.to_string())
}

/// Why a node-mode request failed: the node did not answer (down, or its
/// tunnel has no origin), or it answered and refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum NodeModeError {
    Unreachable(String),
    Refused(String),
}

impl std::fmt::Display for NodeModeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NodeModeError::Unreachable(message) | NodeModeError::Refused(message) => {
                f.write_str(message)
            }
        }
    }
}

/// 502–504 and Cloudflare's 520–530 mean the origin behind the URL is down.
pub(crate) fn origin_down_status(status: u16) -> bool {
    matches!(status, 502..=504 | 520..=530)
}

async fn apply_node_mode_classified(
    client: &reqwest::Client,
    cfg: &Config,
    node_id: &str,
    payload: &ClusterApplyNodeModeRequest,
) -> Result<(), NodeModeError> {
    use NodeModeError::{Refused, Unreachable};
    if node_id == cfg.cluster.node_id {
        let status = apply_cluster_node_mode_locally(payload.clone())
            .await
            .map_err(Refused)?;
        return validate_node_mode_response(&status, node_id, payload.active).map_err(Refused);
    }

    let peer = cfg
        .cluster
        .peers
        .iter()
        .find(|peer| peer.node_id == node_id)
        .ok_or_else(|| Refused(format!("未找到目标节点 {}", node_id)))?;
    let url = format!(
        "{}/api/cluster/apply-node-mode",
        peer.api_url.trim_end_matches('/')
    );
    let response = client
        .post(url)
        .json(payload)
        .timeout(cluster_control_timeout(cfg))
        .send()
        .await
        .map_err(|e| {
            let message = format!("更新节点 {} 模式失败: {}", node_id, e);
            if e.is_connect() || e.is_timeout() || e.is_request() {
                Unreachable(message)
            } else {
                Refused(message)
            }
        })?;

    if !response.status().is_success() {
        let message = format!("更新节点 {} 模式失败: HTTP {}", node_id, response.status());
        return Err(if origin_down_status(response.status().as_u16()) {
            Unreachable(message)
        } else {
            Refused(message)
        });
    }

    let envelope = response
        .json::<PeerApiResponse<ClusterStatus>>()
        .await
        .map_err(|e| Refused(format!("解析节点 {} 模式响应失败: {}", node_id, e)))?;

    if envelope.success {
        let status = envelope
            .data
            .ok_or_else(|| Refused(format!("节点 {node_id} 未返回模式确认状态")))?;
        validate_node_mode_response(&status, node_id, payload.active).map_err(Refused)?;
        merge_cluster_status_from_direct_peer(status, node_id, cfg).map_err(Refused)?;
        Ok(())
    } else {
        Err(Refused(envelope.message.unwrap_or_else(|| {
            format!("节点 {} 拒绝模式更新", node_id)
        })))
    }
}

pub async fn apply_cluster_node_mode_locally(
    payload: ClusterApplyNodeModeRequest,
) -> Result<ClusterStatus, String> {
    // The hold, commit, runtime changes and acknowledgement form one owned task.
    tokio::spawn(apply_cluster_node_mode_inner(payload))
        .await
        .map_err(|e| format!("节点模式更新任务失败: {e}"))?
}

pub(crate) fn validate_node_mode_response(
    status: &ClusterStatus,
    node_id: &str,
    active: bool,
) -> Result<(), String> {
    if status.local_node_id != node_id {
        return Err(format!("节点 {node_id} 模式确认身份不符"));
    }
    let node = status
        .nodes
        .iter()
        .find(|node| node.node_id == node_id)
        .ok_or_else(|| format!("节点 {node_id} 模式确认缺少本机状态"))?;
    if !active && (node.ffmpeg_running || !monitor_toggles_all_off(&node.monitor_toggles)) {
        return Err(format!("节点 {node_id} 未确认停止推流和监控"));
    }
    Ok(())
}

async fn apply_cluster_node_mode_inner(
    payload: ClusterApplyNodeModeRequest,
) -> Result<ClusterStatus, String> {
    let _apply_guard = node_mode_apply_lock().lock().await;
    let active = payload.active;
    let restart = payload.restart;
    let preserve_drain = payload.preserve_drain;
    let monitor_toggles = resolved_node_mode_monitor_toggles(active, payload.monitor_toggles);
    let channel_targets = payload.channel_targets;
    let mut config_changed = monitor_toggles.is_some() || channel_targets.is_some();
    let mut cfg = load_config().await.map_err(|e| e.to_string())?;
    validate_node_mode_precondition(
        current_active_owner().as_deref(),
        payload.expected_active_owner.as_deref(),
        active,
        &cfg.cluster.node_id,
        payload.handoff_target_node_id.as_deref(),
    )?;
    if active {
        let state = cluster_state_read();
        if state
            .pending_handoff_source
            .as_deref()
            .is_some_and(|source| payload.expected_active_owner.as_deref() != Some(source))
        {
            return Err("节点仍在等待原活跃节点交接确认".to_string());
        }
    }
    if !active {
        // Hold execution before the first write/await so a monitor cannot restart
        // between demotion's config commit and process shutdown.
        cluster_state_write().local_execution_held = true;
        set_manual_restart();
        clear_local_stream();
        stop_ffmpeg().await;
    }
    if let Some(monitored_config) = payload.monitored_config {
        write_monitored_json_files(
            monitored_config.channels_json.clone(),
            monitored_config.areas_json.clone(),
        )
        .await?;
        apply_monitored_config_to_config(&mut cfg, monitored_config);
        config_changed = true;
    }

    apply_node_mode_config_state(&mut cfg, channel_targets.as_ref(), monitor_toggles.as_ref());

    if config_changed {
        save_config(&mut cfg).await.map_err(|e| e.to_string())?;
        refresh_status_cache_config_from(&cfg);
        set_config_updated();
        crate::webui::state::request_status_refresh();
    }

    // Promoting to active clears drain and latched faults. Demoting to standby
    // only clears draining so the node stays eligible; disabled state is /drain only.
    if active {
        set_drain_state(&cfg, None, false);
    } else if !preserve_drain {
        set_local_drain_state_preserving_fault(&cfg, false);
    }

    apply_danmaku_command_runtime_state(active && cfg.bililive.enable_danmaku_command).await;

    if active {
        if let Some(monitor_toggles) = monitor_toggles.as_ref() {
            let cache_targets =
                channel_targets.unwrap_or_else(|| channel_target_state_from_config(&cfg));
            cache_active_monitor_state_from_owner(monitor_toggles, Some(&cache_targets));
        }
    }

    if !active || restart {
        set_manual_restart();
        clear_local_stream();
        stop_ffmpeg().await;
    }

    if active {
        force_failover(&cfg, Some(cfg.cluster.node_id.clone()));
        let mut state = cluster_state_write();
        state.pending_handoff_source = None;
        state.local_execution_held = false;
    }
    get_cluster_status().await
}

pub(crate) fn validate_node_mode_precondition(
    current_owner: Option<&str>,
    expected_owner: Option<&str>,
    active: bool,
    local_node_id: &str,
    handoff_target_node_id: Option<&str>,
) -> Result<(), String> {
    let Some(expected_owner) = expected_owner else {
        return Ok(());
    };
    if current_owner == Some(expected_owner) || (active && current_owner == Some(local_node_id)) {
        return Ok(());
    }
    if !active
        && expected_owner == local_node_id
        && handoff_target_node_id.is_some()
        && handoff_target_node_id != Some(local_node_id)
        && current_owner == handoff_target_node_id
    {
        return Ok(());
    }
    Err(format!(
        "拒绝过期节点模式更新: 当前活跃节点为 {}, 请求期望 {}",
        current_owner.unwrap_or("none"),
        expected_owner
    ))
}

pub(crate) fn last_known_active_toggles() -> Option<MonitorToggleState> {
    cluster_state_read().last_known_active_toggles.clone()
}

pub(crate) fn last_known_active_channel_targets() -> Option<ChannelTargetState> {
    cluster_state_read()
        .last_known_active_channel_targets
        .as_ref()
        .filter(|targets| channel_targets_configured(targets))
        .cloned()
}

/// Adopt the active owner's auto-failover setting when a peer heartbeat shows we
/// missed a membership sync (e.g. node was offline during a toggle).
pub(crate) async fn adopt_auto_failover_from_peer_view(
    peer_auto_failover: bool,
    peer_is_active_owner: bool,
    peer_node_id: &str,
    cfg: &Config,
) {
    if !should_adopt_auto_failover_from_peer(peer_auto_failover, peer_is_active_owner, cfg) {
        return;
    }

    let Ok(mut updated) = load_config().await else {
        return;
    };
    if !updated.cluster.enabled || updated.cluster.auto_failover == peer_auto_failover {
        return;
    }

    updated.cluster.auto_failover = peer_auto_failover;
    if let Err(e) = save_config(&mut updated).await {
        tracing::warn!(
            "Failed to adopt cluster auto_failover from peer {}: {}",
            peer_node_id,
            e
        );
    } else {
        tracing::info!(
            "Adopted cluster auto_failover={} from active owner {}",
            peer_auto_failover,
            peer_node_id
        );
    }
}

pub(crate) fn should_adopt_auto_failover_from_peer(
    peer_auto_failover: bool,
    peer_is_active_owner: bool,
    cfg: &Config,
) -> bool {
    cfg.cluster.enabled && peer_is_active_owner && peer_auto_failover != cfg.cluster.auto_failover
}
