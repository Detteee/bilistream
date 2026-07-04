use crate::config::{
    load_config, save_config, ClusterConfig, Config, PriorityChannel, Twitch, Youtube,
};
use crate::plugins::{
    get_ffmpeg_network_stats, get_ffmpeg_speed, is_ffmpeg_hls_cache_active, is_ffmpeg_running,
    set_config_updated, set_manual_restart, stop_ffmpeg,
};
use crate::webui::state::{get_status_cache, NetworkStatus, StatusData};
use futures_util::future::join_all;
use lazy_static::lazy_static;
use serde::{Deserialize, Serialize};
use std::collections::{hash_map::DefaultHasher, HashMap};
use std::fs;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::sync::RwLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

lazy_static! {
    static ref CLUSTER_STATE: RwLock<ClusterState> = RwLock::new(ClusterState::default());
}

const FFMPEG_FAILURE_WINDOW_SECS: u64 = 60 * 60;
const EXTERNAL_API_FAILURE_RETENTION_SECS: u64 = 60 * 60;
const HEARTBEAT_FAILURE_THRESHOLD: u32 = 3;
const NETWORK_ISOLATED_REASON: &str = "network_isolated";

#[derive(Clone, Debug, Default)]
struct ClusterState {
    nodes: HashMap<String, ClusterNodeSnapshot>,
    local_draining: bool,
    local_ddos: bool,
    local_fault_ddos: bool,
    local_fault_latched: bool,
    local_fault_reason: Option<String>,
    forced_owner: Option<String>,
    active_owner: Option<String>,
    lease_until: u64,
    local_stream: Option<ClusterStreamIdentity>,
    local_failed_restarts: u32,
    local_failed_restart_times: Vec<u64>,
    local_external_api_failures: u32,
    local_external_api_failure_times: Vec<u64>,
    heartbeat_failures: HashMap<String, u32>,
    peer_observations: HashMap<String, HashMap<String, u64>>,
    last_known_active_toggles: Option<MonitorToggleState>,
    last_known_active_channel_targets: Option<ChannelTargetState>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ClusterStatus {
    pub enabled: bool,
    pub local_node_id: String,
    pub active_owner: Option<String>,
    pub lease_until: Option<u64>,
    pub config_version: String,
    #[serde(default = "default_true")]
    pub auto_failover: bool,
    pub nodes: Vec<ClusterNodeSnapshot>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ClusterNodeSnapshot {
    pub node_id: String,
    pub name: String,
    pub api_url: String,
    pub priority: i32,
    pub last_seen: Option<u64>,
    pub is_local: bool,
    pub role: ClusterNodeRole,
    pub health: ClusterHealth,
    pub draining: bool,
    pub ddos: bool,
    pub ffmpeg_running: bool,
    pub active_stream: Option<ClusterStreamIdentity>,
    pub status: Option<StatusData>,
    pub network: Option<NetworkStatus>,
    pub config_version: String,
    pub failed_restarts: u32,
    #[serde(default)]
    pub monitor_toggles: MonitorToggleState,
    #[serde(default)]
    pub channel_targets: ChannelTargetState,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ClusterNodeRole {
    Active,
    Standby,
    Draining,
    Unhealthy,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ClusterHealth {
    pub healthy: bool,
    pub reason: String,
    pub stale: bool,
    pub stream_degraded: bool,
}

impl ClusterHealth {
    fn healthy() -> Self {
        Self {
            healthy: true,
            reason: "healthy".to_string(),
            stale: false,
            stream_degraded: false,
        }
    }

    fn unhealthy(reason: impl Into<String>, stale: bool, stream_degraded: bool) -> Self {
        Self {
            healthy: false,
            reason: reason.into(),
            stale,
            stream_degraded,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ClusterStreamIdentity {
    pub platform: String,
    pub channel_name: String,
    pub channel_id: String,
    pub stream_id: Option<String>,
    pub title: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ClusterHeartbeatRequest {
    pub node: ClusterNodeSnapshot,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ClusterDrainRequest {
    pub node_id: Option<String>,
    pub draining: bool,
    #[serde(default)]
    pub ddos: bool,
    #[serde(default)]
    pub propagate: Option<bool>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ClusterFailoverRequest {
    pub target_node_id: Option<String>,
    #[serde(default)]
    pub propagate: Option<bool>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ClusterAutoFailoverRequest {
    pub enabled: bool,
    #[serde(default)]
    pub propagate: Option<bool>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ClusterSyncConfigRequest {
    pub monitored_config: MonitoredConfig,
    pub config_version: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ClusterActiveMonitorStateRequest {
    pub monitor_toggles: MonitorToggleState,
    #[serde(default)]
    pub channel_targets: Option<ChannelTargetState>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ClusterApplyNodeModeRequest {
    pub monitored_config: Option<MonitoredConfig>,
    pub active: bool,
    #[serde(default)]
    pub restart: bool,
    #[serde(default)]
    pub monitor_toggles: Option<MonitorToggleState>,
    #[serde(default)]
    pub channel_targets: Option<ChannelTargetState>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MonitoredConfig {
    pub interval: u64,
    pub auto_cover: bool,
    pub enable_anti_collision: bool,
    pub anti_collision_list: HashMap<String, i32>,
    #[serde(default = "default_true")]
    pub enable_danmaku_command: bool,
    pub enable_youtube_monitor: bool,
    pub enable_twitch_monitor: bool,
    pub youtube: Youtube,
    pub twitch: Twitch,
    pub priority_channel: PriorityChannel,
    pub channels_json: Option<serde_json::Value>,
    pub areas_json: Option<serde_json::Value>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct MonitorToggleState {
    #[serde(default)]
    pub enable_danmaku_command: bool,
    #[serde(default)]
    pub enable_youtube_monitor: bool,
    #[serde(default)]
    pub enable_twitch_monitor: bool,
    #[serde(default)]
    pub youtube_enable_monitor: bool,
    #[serde(default)]
    pub twitch_enable_monitor: bool,
    #[serde(default)]
    pub priority_channel_enabled: bool,
    #[serde(default)]
    pub priority_channel_auto_restart: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ChannelTargetState {
    #[serde(default)]
    pub youtube_channel_name: String,
    #[serde(default)]
    pub youtube_channel_id: String,
    #[serde(default)]
    pub twitch_channel_name: String,
    #[serde(default)]
    pub twitch_channel_id: String,
    #[serde(default)]
    pub priority_channel_name: String,
    #[serde(default)]
    pub priority_youtube_channel_id: String,
    #[serde(default)]
    pub priority_twitch_channel_id: String,
}

#[derive(Deserialize)]
struct PeerApiResponse<T> {
    success: bool,
    data: Option<T>,
    message: Option<String>,
}

pub fn is_enabled(cfg: &Config) -> bool {
    cfg.cluster.enabled
}

fn default_true() -> bool {
    true
}

pub fn monitored_config_from_config(cfg: &Config) -> MonitoredConfig {
    MonitoredConfig {
        interval: cfg.interval,
        auto_cover: cfg.auto_cover,
        enable_anti_collision: cfg.enable_anti_collision,
        anti_collision_list: cfg.anti_collision_list.clone(),
        enable_danmaku_command: cfg.bililive.enable_danmaku_command,
        enable_youtube_monitor: cfg.enable_youtube_monitor,
        enable_twitch_monitor: cfg.enable_twitch_monitor,
        youtube: cfg.youtube.clone(),
        twitch: cfg.twitch.clone(),
        priority_channel: cfg.priority_channel.clone(),
        channels_json: read_json_file("channels.json"),
        areas_json: read_json_file("areas.json"),
    }
}

pub fn monitor_toggle_state_from_config(cfg: &Config) -> MonitorToggleState {
    MonitorToggleState {
        enable_danmaku_command: cfg.bililive.enable_danmaku_command,
        enable_youtube_monitor: cfg.enable_youtube_monitor,
        enable_twitch_monitor: cfg.enable_twitch_monitor,
        youtube_enable_monitor: cfg.youtube.enable_monitor,
        twitch_enable_monitor: cfg.twitch.enable_monitor,
        priority_channel_enabled: cfg.priority_channel.enabled,
        priority_channel_auto_restart: cfg.priority_channel.auto_restart,
    }
}

pub fn monitor_toggle_state_from_monitored_config(payload: &MonitoredConfig) -> MonitorToggleState {
    MonitorToggleState {
        enable_danmaku_command: payload.enable_danmaku_command,
        enable_youtube_monitor: payload.enable_youtube_monitor,
        enable_twitch_monitor: payload.enable_twitch_monitor,
        youtube_enable_monitor: payload.youtube.enable_monitor,
        twitch_enable_monitor: payload.twitch.enable_monitor,
        priority_channel_enabled: payload.priority_channel.enabled,
        priority_channel_auto_restart: payload.priority_channel.auto_restart,
    }
}

pub fn all_monitor_toggles_off() -> MonitorToggleState {
    MonitorToggleState::default()
}

pub fn all_monitor_toggles_on() -> MonitorToggleState {
    MonitorToggleState {
        enable_danmaku_command: true,
        enable_youtube_monitor: true,
        enable_twitch_monitor: true,
        youtube_enable_monitor: true,
        twitch_enable_monitor: true,
        priority_channel_enabled: true,
        priority_channel_auto_restart: true,
    }
}

pub fn channel_target_state_from_config(cfg: &Config) -> ChannelTargetState {
    ChannelTargetState {
        youtube_channel_name: cfg.youtube.channel_name.clone(),
        youtube_channel_id: cfg.youtube.channel_id.clone(),
        twitch_channel_name: cfg.twitch.channel_name.clone(),
        twitch_channel_id: cfg.twitch.channel_id.clone(),
        priority_channel_name: cfg.priority_channel.channel_name.clone(),
        priority_youtube_channel_id: cfg.priority_channel.youtube_channel_id.clone(),
        priority_twitch_channel_id: cfg.priority_channel.twitch_channel_id.clone(),
    }
}

pub fn channel_target_state_from_monitored_config(payload: &MonitoredConfig) -> ChannelTargetState {
    ChannelTargetState {
        youtube_channel_name: payload.youtube.channel_name.clone(),
        youtube_channel_id: payload.youtube.channel_id.clone(),
        twitch_channel_name: payload.twitch.channel_name.clone(),
        twitch_channel_id: payload.twitch.channel_id.clone(),
        priority_channel_name: payload.priority_channel.channel_name.clone(),
        priority_youtube_channel_id: payload.priority_channel.youtube_channel_id.clone(),
        priority_twitch_channel_id: payload.priority_channel.twitch_channel_id.clone(),
    }
}

pub fn apply_channel_target_state_to_config(cfg: &mut Config, payload: &ChannelTargetState) {
    cfg.youtube.channel_name = payload.youtube_channel_name.clone();
    cfg.youtube.channel_id = payload.youtube_channel_id.clone();
    cfg.twitch.channel_name = payload.twitch_channel_name.clone();
    cfg.twitch.channel_id = payload.twitch_channel_id.clone();
    cfg.priority_channel.channel_name = payload.priority_channel_name.clone();
    cfg.priority_channel.youtube_channel_id = payload.priority_youtube_channel_id.clone();
    cfg.priority_channel.twitch_channel_id = payload.priority_twitch_channel_id.clone();
}

pub async fn apply_monitor_toggle_state(payload: MonitorToggleState) -> Result<(), String> {
    let mut cfg = crate::config::load_config()
        .await
        .map_err(|e| e.to_string())?;
    apply_monitor_toggle_state_to_config(&mut cfg, &payload);
    save_config(&cfg).await.map_err(|e| e.to_string())?;
    apply_danmaku_command_runtime_state(payload.enable_danmaku_command);
    if monitor_toggles_any_enabled(&payload) {
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
    let mut state = CLUSTER_STATE.write().unwrap();
    cache_active_monitor_state(&mut state, toggles, channel_targets);
}

pub fn cache_active_monitor_state_from_peer(
    toggles: MonitorToggleState,
    channel_targets: Option<ChannelTargetState>,
) {
    cache_active_monitor_state_from_owner(&toggles, channel_targets.as_ref());
}

fn cache_active_monitor_state(
    state: &mut ClusterState,
    toggles: &MonitorToggleState,
    channel_targets: Option<&ChannelTargetState>,
) {
    if monitor_toggles_any_enabled(toggles) {
        state.last_known_active_toggles = Some(toggles.clone());
    }
    if let Some(targets) = channel_targets.filter(|targets| channel_targets_configured(targets)) {
        state.last_known_active_channel_targets = Some(targets.clone());
    }
}

fn cache_current_owner_monitor_state(state: &mut ClusterState) {
    let Some(owner) = state.active_owner.as_deref() else {
        return;
    };
    let Some((toggles, channel_targets)) = state
        .nodes
        .get(owner)
        .map(|node| (node.monitor_toggles.clone(), node.channel_targets.clone()))
    else {
        return;
    };
    cache_active_monitor_state(state, &toggles, Some(&channel_targets));
}

pub async fn push_active_monitor_state_to_peers(cfg: &Config) -> Result<usize, String> {
    if !cfg.cluster.enabled {
        return Ok(0);
    }

    let status = get_cluster_status_for_config(cfg).await;
    if status.active_owner.as_deref() != Some(cfg.cluster.node_id.as_str()) {
        return Ok(0);
    }

    let toggles = monitor_toggle_state_from_config(cfg);
    let channel_targets = channel_target_state_from_config(cfg);
    cache_active_monitor_state_from_owner(&toggles, Some(&channel_targets));

    let request = ClusterActiveMonitorStateRequest {
        monitor_toggles: toggles,
        channel_targets: Some(channel_targets),
    };
    let client = reqwest::Client::new();
    let timeout = Duration::from_secs(cfg.cluster.heartbeat_interval_secs.max(5));

    let tasks = cfg
        .cluster
        .peers
        .iter()
        .filter(|peer| peer.node_id != cfg.cluster.node_id)
        .map(|peer| push_active_monitor_state_to_peer(&client, cfg, peer, &request, timeout));
    summarize_peer_push_results(join_all(tasks).await, "部分节点监控开关缓存失败")
}

async fn push_active_monitor_state_to_peer(
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

fn summarize_peer_push_results(
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

pub fn apply_monitor_toggle_state_to_config(cfg: &mut Config, payload: &MonitorToggleState) {
    cfg.bililive.enable_danmaku_command = payload.enable_danmaku_command;
    cfg.enable_youtube_monitor = payload.enable_youtube_monitor;
    cfg.enable_twitch_monitor = payload.enable_twitch_monitor;
    cfg.youtube.enable_monitor = payload.youtube_enable_monitor;
    cfg.twitch.enable_monitor = payload.twitch_enable_monitor;
    cfg.priority_channel.enabled = payload.priority_channel_enabled;
    cfg.priority_channel.auto_restart = payload.priority_channel_auto_restart;
}

fn apply_danmaku_command_runtime_state(enabled: bool) {
    crate::plugins::enable_danmaku_commands(enabled);
    if enabled {
        if !crate::plugins::is_danmaku_running() {
            crate::plugins::run_danmaku();
        }
    } else if crate::plugins::is_danmaku_running() {
        crate::plugins::stop_danmaku();
    }
}

pub fn monitored_config_version(cfg: &Config) -> String {
    monitored_config_version_from_payload(&monitored_config_from_config(cfg))
}

pub fn monitored_config_version_from_payload(payload: &MonitoredConfig) -> String {
    let value = monitored_channel_target_value(payload);
    canonical_value_hash(&value)
}

pub fn monitored_config_integrity_version(cfg: &Config) -> String {
    monitored_config_integrity_version_from_payload(&monitored_config_from_config(cfg))
}

pub fn monitored_config_integrity_version_from_payload(payload: &MonitoredConfig) -> String {
    let value = serde_json::to_value(payload).unwrap_or(serde_json::Value::Null);
    canonical_value_hash(&value)
}

fn monitored_channel_target_value(payload: &MonitoredConfig) -> serde_json::Value {
    serde_json::json!({
        "youtube": {
            "channel_name": payload.youtube.channel_name,
            "channel_id": payload.youtube.channel_id,
        },
        "twitch": {
            "channel_name": payload.twitch.channel_name,
            "channel_id": payload.twitch.channel_id,
        },
        "priority_channel": {
            "channel_name": payload.priority_channel.channel_name,
            "youtube_channel_id": payload.priority_channel.youtube_channel_id,
            "twitch_channel_id": payload.priority_channel.twitch_channel_id,
        },
    })
}

fn canonical_value_hash(value: &serde_json::Value) -> String {
    let serialized = canonical_json(value);
    let mut hasher = DefaultHasher::new();
    serialized.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

fn canonical_json(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Null => "null".to_string(),
        serde_json::Value::Bool(value) => value.to_string(),
        serde_json::Value::Number(value) => value.to_string(),
        serde_json::Value::String(value) => serde_json::to_string(value).unwrap_or_default(),
        serde_json::Value::Array(values) => {
            let items = values
                .iter()
                .map(canonical_json)
                .collect::<Vec<_>>()
                .join(",");
            format!("[{}]", items)
        }
        serde_json::Value::Object(map) => {
            let mut keys = map.keys().collect::<Vec<_>>();
            keys.sort();
            let items = keys
                .into_iter()
                .map(|key| {
                    format!(
                        "{}:{}",
                        serde_json::to_string(key).unwrap_or_default(),
                        canonical_json(&map[key])
                    )
                })
                .collect::<Vec<_>>()
                .join(",");
            format!("{{{}}}", items)
        }
    }
}

pub async fn apply_monitored_config(payload: MonitoredConfig) -> Result<(), String> {
    let mut cfg = crate::config::load_config()
        .await
        .map_err(|e| e.to_string())?;

    if let Some(channels_json) = &payload.channels_json {
        write_json_file("channels.json", channels_json)?;
    }
    if let Some(areas_json) = &payload.areas_json {
        write_json_file("areas.json", areas_json)?;
    }

    apply_monitored_config_to_config(&mut cfg, payload);

    save_config(&cfg).await.map_err(|e| e.to_string())?;

    set_config_updated();
    Ok(())
}

fn apply_monitored_config_to_config(cfg: &mut Config, payload: MonitoredConfig) {
    let local_enable_danmaku_command = cfg.bililive.enable_danmaku_command;
    let local_enable_youtube_monitor = cfg.enable_youtube_monitor;
    let local_enable_twitch_monitor = cfg.enable_twitch_monitor;
    let local_youtube_enable_monitor = cfg.youtube.enable_monitor;
    let local_twitch_enable_monitor = cfg.twitch.enable_monitor;
    let local_priority_enabled = cfg.priority_channel.enabled;
    let local_priority_auto_restart = cfg.priority_channel.auto_restart;

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
    cfg.priority_channel.enabled = local_priority_enabled;
    cfg.priority_channel.auto_restart = local_priority_auto_restart;

    crate::config::update_priority_channel_from_channels(cfg);
}

pub async fn push_monitored_config_to_peers(cfg: &Config) -> Result<usize, String> {
    if !cfg.cluster.enabled || !cfg.cluster.sync_monitored_channels {
        return Err("集群配置同步未启用".to_string());
    }

    let request = ClusterSyncConfigRequest {
        monitored_config: monitored_config_from_config(cfg),
        config_version: monitored_config_integrity_version(cfg),
    };
    let client = reqwest::Client::new();
    let timeout = Duration::from_secs(cfg.cluster.heartbeat_interval_secs.max(5));

    let tasks = cfg
        .cluster
        .peers
        .iter()
        .filter(|peer| peer.node_id != cfg.cluster.node_id)
        .map(|peer| push_monitored_config_to_peer(&client, cfg, peer, &request, timeout));
    summarize_peer_push_results(join_all(tasks).await, "部分节点监控频道配置同步失败")
}

async fn push_monitored_config_to_peer(
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

pub fn start_cluster_worker() {
    tokio::spawn(async {
        let client = reqwest::Client::new();

        loop {
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

            let local = collect_local_snapshot(&cfg).await;
            let previous_owner = current_active_owner();
            update_node(local.clone(), &cfg.cluster.node_id);
            send_heartbeats(&client, &cfg, local).await;
            let status = get_cluster_status_for_config(&cfg).await;
            handle_auto_owner_transition(&client, &cfg, previous_owner, &status).await;
            enforce_local_standby_toggles(&cfg, &status).await;

            if let Some(owner) = status.active_owner.as_deref() {
                if owner != cfg.cluster.node_id.as_str() && is_ffmpeg_running().await {
                    tracing::warn!("集群租约已转移，停止本节点 ffmpeg 推流");
                    set_manual_restart();
                    stop_ffmpeg().await;
                }
            }

            tokio::time::sleep(heartbeat_sleep_duration(&cfg)).await;
        }
    });
}

fn current_active_owner() -> Option<String> {
    CLUSTER_STATE.read().unwrap().active_owner.clone()
}

fn heartbeat_sleep_duration(cfg: &Config) -> Duration {
    let base = Duration::from_secs(cfg.cluster.heartbeat_interval_secs.max(1));
    let jitter_ms = stable_node_jitter_ms(&cfg.cluster.node_id, 1_000);
    base + Duration::from_millis(jitter_ms)
}

fn stable_node_jitter_ms(node_id: &str, max_ms: u64) -> u64 {
    if max_ms == 0 {
        return 0;
    }
    let mut hasher = DefaultHasher::new();
    node_id.hash(&mut hasher);
    hasher.finish() % max_ms
}

fn current_forced_owner() -> Option<String> {
    CLUSTER_STATE.read().unwrap().forced_owner.clone()
}

pub fn local_ddos_state() -> bool {
    CLUSTER_STATE.read().unwrap().local_ddos
}

fn local_effective_ddos_state(state: &ClusterState) -> bool {
    state.local_ddos || state.local_fault_ddos
}

async fn handle_auto_owner_transition(
    client: &reqwest::Client,
    cfg: &Config,
    previous_owner: Option<String>,
    status: &ClusterStatus,
) {
    if !cfg.cluster.auto_failover {
        return;
    }
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

    let previous_node = status
        .nodes
        .iter()
        .find(|node| node.node_id == previous_owner);
    tracing::warn!(
        "集群自动故障转移: {} -> {}, transfer active channel targets and monitor toggles",
        previous_owner,
        new_owner
    );

    let mut toggles = previous_node
        .map(|node| node.monitor_toggles.clone())
        .unwrap_or_else(all_monitor_toggles_off);
    if monitor_toggles_all_off(&toggles) {
        if let Some(cached) = last_known_active_toggles() {
            tracing::info!(
                "previous owner toggles are all off, using cached last-known active toggles"
            );
            toggles = cached;
        } else {
            tracing::warn!(
                "previous owner toggles unavailable, enabling all monitor toggles on fallback active node"
            );
            toggles = all_monitor_toggles_on();
        }
    }
    let channel_targets = previous_node
        .map(|node| node.channel_targets.clone())
        .filter(channel_targets_configured)
        .or_else(last_known_active_channel_targets);

    if let Err(e) = apply_monitor_toggles_to_node_with_retry(
        client,
        cfg,
        new_owner,
        toggles,
        channel_targets.clone(),
        true,
        "enable_new_active",
    )
    .await
    {
        tracing::warn!("Failed to transfer monitor toggles to {}: {}", new_owner, e);
    }

    let should_disable_previous =
        previous_node.is_none_or(|node| !node.health.healthy || node.draining || node.ddos);
    if should_disable_previous {
        if let Err(e) = apply_monitor_toggles_to_node_with_retry(
            client,
            cfg,
            &previous_owner,
            all_monitor_toggles_off(),
            None,
            false,
            "disable_previous_active",
        )
        .await
        {
            tracing::warn!(
                "Failed to disable monitor toggles on {}: {}",
                previous_owner,
                e
            );
        }
    }
}

async fn apply_monitor_toggles_to_node_with_retry(
    client: &reqwest::Client,
    cfg: &Config,
    node_id: &str,
    monitor_toggles: MonitorToggleState,
    channel_targets: Option<ChannelTargetState>,
    active: bool,
    phase: &str,
) -> Result<(), String> {
    let max_attempts = 3;
    let mut last_error = String::new();
    for attempt in 1..=max_attempts {
        match apply_monitor_toggles_to_node(
            client,
            cfg,
            node_id,
            monitor_toggles.clone(),
            channel_targets.clone(),
            active,
        )
        .await
        {
            Ok(()) => return Ok(()),
            Err(e) => {
                last_error = e;
                tracing::warn!(
                    "Monitor toggle transfer {} attempt {}/{} failed for {}: {}",
                    phase,
                    attempt,
                    max_attempts,
                    node_id,
                    last_error
                );
                if attempt < max_attempts {
                    tokio::time::sleep(Duration::from_millis(300 * attempt as u64)).await;
                }
            }
        }
    }
    Err(last_error)
}

fn monitor_toggles_all_off(toggles: &MonitorToggleState) -> bool {
    *toggles == all_monitor_toggles_off()
}

pub(crate) fn monitor_toggles_any_enabled(toggles: &MonitorToggleState) -> bool {
    !monitor_toggles_all_off(toggles)
}

pub(crate) fn channel_targets_configured(targets: &ChannelTargetState) -> bool {
    !targets.youtube_channel_name.is_empty()
        || !targets.youtube_channel_id.is_empty()
        || !targets.twitch_channel_name.is_empty()
        || !targets.twitch_channel_id.is_empty()
        || !targets.priority_channel_name.is_empty()
        || !targets.priority_youtube_channel_id.is_empty()
        || !targets.priority_twitch_channel_id.is_empty()
}

pub(crate) fn last_known_active_toggles() -> Option<MonitorToggleState> {
    CLUSTER_STATE
        .read()
        .unwrap()
        .last_known_active_toggles
        .as_ref()
        .filter(|toggles| monitor_toggles_any_enabled(toggles))
        .cloned()
}

pub(crate) fn last_known_active_channel_targets() -> Option<ChannelTargetState> {
    CLUSTER_STATE
        .read()
        .unwrap()
        .last_known_active_channel_targets
        .as_ref()
        .filter(|targets| channel_targets_configured(targets))
        .cloned()
}

async fn enforce_local_standby_toggles(cfg: &Config, status: &ClusterStatus) {
    if !should_disable_local_standby_toggles(cfg, status) {
        return;
    }
    if let Err(e) = apply_monitor_toggle_state(all_monitor_toggles_off()).await {
        tracing::warn!("Failed to disable local standby monitor toggles: {}", e);
    }
}

fn should_disable_local_standby_toggles(cfg: &Config, status: &ClusterStatus) -> bool {
    if CLUSTER_STATE
        .read()
        .unwrap()
        .forced_owner
        .as_deref()
        .is_some_and(|owner| owner == cfg.cluster.node_id)
    {
        return false;
    }

    let Some(owner) = status.active_owner.as_deref() else {
        // No elected owner (startup / transient partition): keep local toggles
        // untouched so the last-active node does not lose its configuration.
        return false;
    };
    if owner == cfg.cluster.node_id {
        return false;
    }
    monitor_toggle_state_from_config(cfg) != all_monitor_toggles_off()
}

async fn apply_monitor_toggles_to_node(
    client: &reqwest::Client,
    cfg: &Config,
    node_id: &str,
    monitor_toggles: MonitorToggleState,
    channel_targets: Option<ChannelTargetState>,
    active: bool,
) -> Result<(), String> {
    if node_id == cfg.cluster.node_id {
        let mut cfg = crate::config::load_config()
            .await
            .map_err(|e| e.to_string())?;
        if let Some(channel_targets) = channel_targets.as_ref() {
            apply_channel_target_state_to_config(&mut cfg, channel_targets);
        }
        apply_monitor_toggle_state_to_config(&mut cfg, &monitor_toggles);
        save_config(&cfg).await.map_err(|e| e.to_string())?;
        apply_danmaku_command_runtime_state(cfg.bililive.enable_danmaku_command);
        if active {
            let cache_targets =
                channel_targets.unwrap_or_else(|| channel_target_state_from_config(&cfg));
            cache_active_monitor_state_from_owner(&monitor_toggles, Some(&cache_targets));
        }
        set_config_updated();
        return Ok(());
    }

    let peer = cfg
        .cluster
        .peers
        .iter()
        .find(|peer| peer.node_id == node_id)
        .ok_or_else(|| format!("未找到目标节点 {}", node_id))?;
    let url = format!(
        "{}/api/cluster/apply-node-mode",
        peer.api_url.trim_end_matches('/')
    );
    let payload = ClusterApplyNodeModeRequest {
        monitored_config: None,
        active,
        restart: false,
        monitor_toggles: Some(monitor_toggles),
        channel_targets,
    };
    let response = client
        .post(url)
        .json(&payload)
        .timeout(Duration::from_secs(
            cfg.cluster.heartbeat_interval_secs.max(5),
        ))
        .send()
        .await
        .map_err(|e| format!("更新节点 {} 监控开关失败: {}", node_id, e))?;

    if !response.status().is_success() {
        return Err(format!(
            "更新节点 {} 监控开关失败: HTTP {}",
            node_id,
            response.status()
        ));
    }

    let envelope = response
        .json::<PeerApiResponse<ClusterStatus>>()
        .await
        .map_err(|e| format!("解析节点 {} 监控开关响应失败: {}", node_id, e))?;
    if envelope.success {
        if let Some(status) = envelope.data {
            merge_cluster_status_from_direct_peer(status, node_id, cfg)?;
        }
        Ok(())
    } else {
        Err(envelope
            .message
            .unwrap_or_else(|| format!("节点 {} 拒绝监控开关更新", node_id)))
    }
}

pub async fn local_has_active_lease(cfg: &Config) -> bool {
    if !cfg.cluster.enabled {
        return true;
    }

    let status = get_cluster_status_for_config(cfg).await;
    status.active_owner.as_deref() == Some(cfg.cluster.node_id.as_str())
}

pub async fn local_may_push(cfg: &Config, stream: Option<ClusterStreamIdentity>) -> bool {
    if !cfg.cluster.enabled {
        return true;
    }

    {
        let mut state = CLUSTER_STATE.write().unwrap();
        state.local_stream = stream;
    }

    local_has_active_lease(cfg).await
}

pub fn clear_local_stream() {
    let mut state = CLUSTER_STATE.write().unwrap();
    state.local_stream = None;
}

pub fn record_stream_exit(success: bool) {
    let mut state = CLUSTER_STATE.write().unwrap();
    let now = now_secs();
    prune_failed_restart_times(&mut state.local_failed_restart_times, now);
    if !success {
        state.local_failed_restart_times.push(now);
    }
    state.local_failed_restarts = state.local_failed_restart_times.len() as u32;
}

pub fn record_external_api_result(success: bool) {
    let mut state = CLUSTER_STATE.write().unwrap();
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

    state.local_external_api_failure_times.push(now);
    state.local_external_api_failures = state.local_external_api_failure_times.len() as u32;
}

fn latch_local_fault(reason: impl Into<String>) {
    let mut state = CLUSTER_STATE.write().unwrap();
    state.local_fault_latched = true;
    state.local_fault_ddos = true;
    state.local_fault_reason = Some(reason.into());
}

fn clear_local_fault_latch(state: &mut ClusterState) {
    state.local_fault_latched = false;
    state.local_fault_ddos = false;
    state.local_fault_reason = None;
}

fn clear_recovered_local_network_isolation(
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

pub async fn get_cluster_status() -> Result<ClusterStatus, String> {
    let cfg = crate::config::load_config()
        .await
        .map_err(|e| e.to_string())?;
    Ok(get_cluster_status_for_config(&cfg).await)
}

pub async fn get_cluster_status_for_config(cfg: &Config) -> ClusterStatus {
    let local = collect_local_snapshot(cfg).await;
    update_node(local, &cfg.cluster.node_id);
    compute_cluster_status(cfg)
}

pub fn receive_heartbeat(cfg: &Config, mut node: ClusterNodeSnapshot) -> ClusterStatus {
    if !configured_node_id(cfg, &node.node_id) {
        tracing::debug!("Ignored heartbeat from unknown node {}", node.node_id);
        return compute_cluster_status(cfg);
    }
    node.last_seen = Some(now_secs());
    mark_peer_reachable(&node.node_id);
    update_node(node, &cfg.cluster.node_id);
    compute_cluster_status(cfg)
}

pub fn set_drain_state(
    cfg: &Config,
    node_id: Option<String>,
    draining: bool,
    ddos: bool,
) -> ClusterStatus {
    set_drain_state_inner(cfg, node_id, draining, ddos, true)
}

pub fn set_local_drain_state_preserving_fault(
    cfg: &Config,
    draining: bool,
    ddos: bool,
) -> ClusterStatus {
    set_drain_state_inner(cfg, None, draining, ddos, false)
}

fn set_drain_state_inner(
    cfg: &Config,
    node_id: Option<String>,
    draining: bool,
    ddos: bool,
    clear_faults_when_enabled: bool,
) -> ClusterStatus {
    let target = node_id.unwrap_or_else(|| cfg.cluster.node_id.clone());
    let mut state = CLUSTER_STATE.write().unwrap();
    let mut effective_ddos = ddos;
    if target == cfg.cluster.node_id {
        state.local_draining = draining;
        state.local_ddos = ddos;
        if clear_faults_when_enabled && !draining && !ddos {
            clear_local_fault_latch(&mut state);
            state.local_failed_restarts = 0;
            state.local_failed_restart_times.clear();
            state.local_external_api_failures = 0;
            state.local_external_api_failure_times.clear();
            state.heartbeat_failures.clear();
        }
        effective_ddos = local_effective_ddos_state(&state);
    }
    if let Some(node) = state.nodes.get_mut(&target) {
        node.draining = draining;
        node.ddos = effective_ddos;
    }
    drop(state);
    compute_cluster_status(cfg)
}

pub fn force_failover(cfg: &Config, target_node_id: Option<String>) -> ClusterStatus {
    {
        let mut state = CLUSTER_STATE.write().unwrap();
        state.forced_owner = target_node_id;
        state.lease_until = 0;
    }
    compute_cluster_status(cfg)
}

async fn send_heartbeats(client: &reqwest::Client, cfg: &Config, local: ClusterNodeSnapshot) {
    let request = ClusterHeartbeatRequest { node: local };
    // Fan out concurrently: one slow/dead peer must not delay heartbeats to the
    // others, otherwise healthy peers may see this node as stale.
    let tasks = cfg
        .cluster
        .peers
        .iter()
        .filter(|peer| peer.node_id != cfg.cluster.node_id)
        .map(|peer| send_heartbeat_to_peer(client, cfg, peer, &request));
    join_all(tasks).await;
}

async fn send_heartbeat_to_peer(
    client: &reqwest::Client,
    cfg: &Config,
    peer: &crate::config::ClusterPeer,
    request: &ClusterHeartbeatRequest,
) {
    let url = format!(
        "{}/api/cluster/heartbeat",
        peer.api_url.trim_end_matches('/')
    );
    let result = client
        .post(url)
        .json(&request)
        .timeout(Duration::from_secs(
            cfg.cluster.heartbeat_interval_secs.max(3),
        ))
        .send()
        .await;

    match result {
        Ok(response) if !response.status().is_success() => {
            tracing::debug!(
                "Cluster heartbeat failed for {}: HTTP {}",
                peer.node_id,
                response.status()
            );
            mark_peer_unreachable(peer.node_id.clone(), cfg);
        }
        Ok(response) => match response.json::<PeerApiResponse<ClusterStatus>>().await {
            Ok(envelope) if envelope.success => {
                if let Some(status) = envelope.data {
                    if heartbeat_response_is_valid(&status, &peer.node_id, cfg) {
                        mark_peer_reachable(&peer.node_id);
                        merge_cluster_status_from_peer(status.clone(), &peer.node_id, cfg);
                        adopt_auto_failover_from_peer_status(&status, &peer.node_id, cfg).await;
                    } else {
                        tracing::debug!(
                            "Cluster heartbeat response from {} failed identity/freshness validation",
                            peer.node_id
                        );
                        mark_peer_unreachable(peer.node_id.clone(), cfg);
                    }
                } else {
                    tracing::debug!(
                        "Cluster heartbeat response from {} had no status",
                        peer.node_id
                    );
                    mark_peer_unreachable(peer.node_id.clone(), cfg);
                }
            }
            Ok(envelope) => {
                tracing::debug!(
                    "Cluster heartbeat rejected by {}: {:?}",
                    peer.node_id,
                    envelope.message
                );
                mark_peer_unreachable(peer.node_id.clone(), cfg);
            }
            Err(e) => {
                tracing::debug!("Cluster heartbeat parse failed for {}: {}", peer.node_id, e);
                mark_peer_unreachable(peer.node_id.clone(), cfg);
            }
        },
        Err(e) => {
            tracing::debug!("Cluster heartbeat failed for {}: {}", peer.node_id, e);
            mark_peer_unreachable(peer.node_id.clone(), cfg);
        }
    }
}

/// Adopt the active owner's auto-failover setting when a peer heartbeat shows we
/// missed a membership sync (e.g. node was offline during a toggle).
async fn adopt_auto_failover_from_peer_status(
    status: &ClusterStatus,
    peer_node_id: &str,
    cfg: &Config,
) {
    if !cfg.cluster.enabled || status.auto_failover == cfg.cluster.auto_failover {
        return;
    }

    let adopt_from_peer = status
        .active_owner
        .as_deref()
        .is_some_and(|owner| owner == peer_node_id);
    if !adopt_from_peer {
        return;
    }

    let Ok(mut updated) = load_config().await else {
        return;
    };
    if !updated.cluster.enabled || updated.cluster.auto_failover == status.auto_failover {
        return;
    }

    updated.cluster.auto_failover = status.auto_failover;
    if let Err(e) = save_config(&updated).await {
        tracing::warn!(
            "Failed to adopt cluster auto_failover from peer {}: {}",
            peer_node_id,
            e
        );
    } else {
        tracing::info!(
            "Adopted cluster auto_failover={} from active owner {}",
            status.auto_failover,
            peer_node_id
        );
    }
}

fn heartbeat_response_is_valid(status: &ClusterStatus, peer_node_id: &str, cfg: &Config) -> bool {
    status.local_node_id == peer_node_id
        && status.nodes.iter().any(|node| {
            node.node_id == peer_node_id
                && node
                    .last_seen
                    .is_some_and(|last_seen| !last_seen_is_stale(last_seen, cfg, now_secs()))
        })
}

async fn collect_local_snapshot(cfg: &Config) -> ClusterNodeSnapshot {
    let network = collect_network_status();
    let status = get_status_cache();
    let (
        draining,
        operator_ddos,
        mut fault_ddos,
        mut fault_latched,
        mut fault_reason,
        active_stream,
        failed_restarts,
        external_api_failures,
        network_isolated,
    ) = {
        let mut state = CLUSTER_STATE.write().unwrap();
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
            state.local_ddos,
            state.local_fault_ddos,
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
        )
    };

    let stream_degraded = ffmpeg_restart_degraded(&cfg.cluster, failed_restarts);
    let external_api_degraded = external_api_degraded(&cfg.cluster, external_api_failures);
    if fault_latched
        && fault_reason.as_deref() == Some(NETWORK_ISOLATED_REASON)
        && !network_isolated
    {
        let mut state = CLUSTER_STATE.write().unwrap();
        if clear_recovered_local_network_isolation(&mut state, network_isolated) {
            fault_latched = false;
            fault_ddos = false;
            fault_reason = None;
        }
    }
    if (stream_degraded || external_api_degraded || network_isolated) && !fault_latched {
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
        fault_ddos = true;
        fault_reason = Some(reason);
    }

    let effective_ddos = operator_ddos || fault_ddos || fault_latched;
    let health = if draining {
        ClusterHealth::unhealthy("draining", false, false)
    } else if fault_latched {
        ClusterHealth::unhealthy(
            fault_reason.unwrap_or_else(|| "node_fault_latched".to_string()),
            false,
            true,
        )
    } else if operator_ddos {
        ClusterHealth::unhealthy("ddos_or_network_unstable", false, false)
    } else {
        ClusterHealth::healthy()
    };

    ClusterNodeSnapshot {
        node_id: cfg.cluster.node_id.clone(),
        name: cfg.cluster.node_name.clone(),
        api_url: cfg.cluster.public_api_url.clone(),
        priority: cfg.cluster.priority,
        last_seen: Some(now_secs()),
        is_local: true,
        role: ClusterNodeRole::Standby,
        health,
        draining,
        ddos: effective_ddos,
        ffmpeg_running: is_ffmpeg_running().await,
        active_stream,
        status,
        network: Some(network),
        config_version: monitored_config_version(cfg),
        failed_restarts,
        monitor_toggles: monitor_toggle_state_from_config(cfg),
        channel_targets: channel_target_state_from_config(cfg),
    }
}

fn collect_network_status() -> NetworkStatus {
    let hls_cache_active = is_ffmpeg_hls_cache_active();
    let stats = get_ffmpeg_network_stats();

    NetworkStatus {
        stream_speed: get_ffmpeg_speed(),
        stream_cache_speed: None,
        stream_bitrate_kbps: stats.push_bitrate_kbps,
        stream_cache_bitrate_kbps: if hls_cache_active {
            stats.cache_bitrate_kbps
        } else {
            None
        },
        stream_fps: stats.push_fps,
        stream_frame: stats.push_frame,
        stream_total_bytes: stats.push_total_bytes,
        stream_cache_total_bytes: if hls_cache_active {
            stats.cache_total_bytes
        } else {
            0
        },
        hls_cache_active,
    }
}

fn compute_cluster_status(cfg: &Config) -> ClusterStatus {
    if !cfg.cluster.enabled {
        return ClusterStatus {
            enabled: false,
            local_node_id: cfg.cluster.node_id.clone(),
            active_owner: None,
            lease_until: None,
            config_version: monitored_config_version(cfg),
            auto_failover: cfg.cluster.auto_failover,
            nodes: Vec::new(),
        };
    }

    let now = now_secs();
    let mut state = CLUSTER_STATE.write().unwrap();
    ensure_configured_nodes(&mut state, cfg, now);
    prune_peer_observations(&mut state, cfg, now);
    normalize_node_health(&mut state, cfg, now);
    clear_invalid_forced_owner(&mut state, cfg, now);

    let chosen = choose_owner(&state, cfg, now);
    state.active_owner = chosen.clone();
    cache_current_owner_monitor_state(&mut state);
    state.lease_until = if chosen.is_some() {
        now + cfg.cluster.lease_ttl_secs.max(1)
    } else {
        0
    };

    let mut nodes: Vec<_> = state.nodes.values().cloned().collect();
    for node in &mut nodes {
        node.is_local = node.node_id == cfg.cluster.node_id;
        node.role = if node.draining || node.ddos {
            ClusterNodeRole::Draining
        } else if !node.health.healthy {
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
        active_owner: state.active_owner.clone(),
        lease_until: (state.lease_until > 0).then_some(state.lease_until),
        config_version: monitored_config_version(cfg),
        auto_failover: cfg.cluster.auto_failover,
        nodes,
    }
}

fn clear_invalid_forced_owner(state: &mut ClusterState, cfg: &Config, now: u64) {
    let forced_owner_is_eligible = state
        .forced_owner
        .as_ref()
        .and_then(|owner| state.nodes.get(owner))
        .is_some_and(|node| node_is_eligible(node, state, cfg, now));

    if state.forced_owner.is_some() && !forced_owner_is_eligible {
        state.forced_owner = None;
    }
}

fn choose_owner(state: &ClusterState, cfg: &Config, now: u64) -> Option<String> {
    let forced_owner = state
        .forced_owner
        .as_ref()
        .filter(|owner| {
            state
                .nodes
                .get(*owner)
                .is_some_and(|node| node_is_eligible(node, state, cfg, now))
        })
        .cloned();
    if forced_owner.is_some() {
        return forced_owner;
    }

    if let Some(current_owner) = state.active_owner.as_ref() {
        if state
            .nodes
            .get(current_owner)
            .is_some_and(|node| node_is_eligible(node, state, cfg, now))
        {
            return Some(current_owner.clone());
        }
    }

    if !cfg.cluster.auto_failover {
        return last_resort_local_owner(state, cfg);
    }

    state
        .nodes
        .values()
        .filter(|node| node_is_eligible(node, state, cfg, now))
        .max_by(|a, b| {
            a.priority
                .cmp(&b.priority)
                .then_with(|| b.node_id.cmp(&a.node_id))
        })
        .map(|node| node.node_id.clone())
        .or_else(|| last_resort_local_owner(state, cfg))
}

/// When every peer is disabled/unavailable, keep the local node active so
/// streaming does not stop with no failover target.
fn last_resort_local_owner(state: &ClusterState, cfg: &Config) -> Option<String> {
    state
        .nodes
        .get(&cfg.cluster.node_id)
        .filter(|node| !node.draining)
        .map(|_| cfg.cluster.node_id.clone())
}

fn node_is_eligible(
    node: &ClusterNodeSnapshot,
    state: &ClusterState,
    cfg: &Config,
    now: u64,
) -> bool {
    if node.health.healthy && !node.draining && !node.ddos && !is_stale(node, cfg, now) {
        return true;
    }
    if node.draining || node.ddos || node.node_id == cfg.cluster.node_id {
        return false;
    }
    if !node.health.stale
        && node.health.reason != "api_unreachable"
        && node.health.reason != "heartbeat_timeout"
        && node.health.reason != "waiting_for_heartbeat"
    {
        return false;
    }
    indirectly_observed_by_quorum(state, &node.node_id, cfg, now)
}

fn indirectly_observed_by_quorum(
    state: &ClusterState,
    node_id: &str,
    cfg: &Config,
    now: u64,
) -> bool {
    let Some(observations) = state.peer_observations.get(node_id) else {
        return false;
    };
    let timeout = cfg.cluster.failover_timeout_secs.max(1);
    let fresh_observers = observations
        .iter()
        .filter(|(observer, observed_at)| {
            configured_node_id(cfg, observer)
                && now.saturating_sub(**observed_at) <= timeout
                && observer_node_is_reliable(state, observer, cfg, now)
        })
        .count();

    fresh_observers >= indirect_observer_threshold(cfg)
}

fn observer_node_is_reliable(
    state: &ClusterState,
    observer_node_id: &str,
    cfg: &Config,
    now: u64,
) -> bool {
    let Some(observer) = state.nodes.get(observer_node_id) else {
        return false;
    };
    observer.health.healthy && !observer.draining && !observer.ddos && !is_stale(observer, cfg, now)
}

fn indirect_observer_threshold(cfg: &Config) -> usize {
    let cluster_size = cfg.cluster.peers.len() + 1;
    (cluster_size / 2).max(1)
}

fn configured_node_id(cfg: &Config, node_id: &str) -> bool {
    node_id == cfg.cluster.node_id || cfg.cluster.peers.iter().any(|peer| peer.node_id == node_id)
}

fn normalize_node_health(state: &mut ClusterState, cfg: &Config, now: u64) {
    for node in state.nodes.values_mut() {
        if node.last_seen.is_none() {
            node.health = ClusterHealth::unhealthy("waiting_for_heartbeat", true, false);
            continue;
        }

        let stale = is_stale(node, cfg, now);
        if stale {
            node.health = ClusterHealth::unhealthy("heartbeat_timeout", true, false);
        } else if node.draining {
            node.health = ClusterHealth::unhealthy("draining", false, false);
        } else if node.ddos {
            let reason = if node.health.stream_degraded {
                node.health.reason.clone()
            } else {
                "ddos_or_network_unstable".to_string()
            };
            node.health = ClusterHealth::unhealthy(reason, false, node.health.stream_degraded);
        } else if node.health.stream_degraded {
            node.health.healthy = false;
            node.health.reason = "stream_metrics_degraded".to_string();
        }
    }

    let indirectly_observed_nodes = state
        .nodes
        .iter()
        .filter(|(node_id, node)| {
            node.node_id != cfg.cluster.node_id
                && !node.draining
                && !node.ddos
                && node.health.stale
                && matches!(
                    node.health.reason.as_str(),
                    "api_unreachable" | "heartbeat_timeout" | "waiting_for_heartbeat"
                )
                && indirectly_observed_by_quorum(state, node_id, cfg, now)
        })
        .map(|(node_id, _)| node_id.clone())
        .collect::<Vec<_>>();

    for node_id in indirectly_observed_nodes {
        if let Some(node) = state.nodes.get_mut(&node_id) {
            node.health = ClusterHealth {
                healthy: true,
                reason: "indirectly_observed".to_string(),
                stale: true,
                stream_degraded: false,
            };
        }
    }
}

fn is_stale(node: &ClusterNodeSnapshot, cfg: &Config, now: u64) -> bool {
    node.last_seen
        .map(|last_seen| last_seen_is_stale(last_seen, cfg, now))
        .unwrap_or(false)
}

fn last_seen_is_stale(last_seen: u64, cfg: &Config, now: u64) -> bool {
    now.saturating_sub(last_seen) > cfg.cluster.failover_timeout_secs.max(1)
}

fn ensure_configured_nodes(state: &mut ClusterState, cfg: &Config, now: u64) {
    let mut configured: std::collections::HashSet<&str> =
        std::collections::HashSet::from([cfg.cluster.node_id.as_str()]);
    for peer in &cfg.cluster.peers {
        configured.insert(peer.node_id.as_str());
    }
    state
        .nodes
        .retain(|node_id, _| configured.contains(node_id.as_str()));
    state
        .heartbeat_failures
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

fn empty_node(
    node_id: &str,
    name: &str,
    api_url: &str,
    priority: i32,
    is_local: bool,
    now: u64,
) -> ClusterNodeSnapshot {
    ClusterNodeSnapshot {
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
        ddos: false,
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

fn update_node(mut node: ClusterNodeSnapshot, local_node_id: &str) {
    node.last_seen = node.last_seen.or_else(|| Some(now_secs()));
    let mut state = CLUSTER_STATE.write().unwrap();
    node.is_local = node.node_id == local_node_id;
    if node.is_local {
        node.draining = state.local_draining;
        node.ddos = local_effective_ddos_state(&state);
        node.active_stream = state.local_stream.clone();
        node.failed_restarts = state.local_failed_restarts;
    }
    state.nodes.insert(node.node_id.clone(), node);
}

pub(crate) fn merge_cluster_status_from_direct_peer(
    status: ClusterStatus,
    peer_node_id: &str,
    cfg: &Config,
) -> Result<(), String> {
    if !heartbeat_response_is_valid(&status, peer_node_id, cfg) {
        return Err(format!(
            "节点 {} 返回的集群状态身份或时间戳无效",
            peer_node_id
        ));
    }

    mark_peer_reachable(peer_node_id);
    merge_cluster_status_from_peer(status, peer_node_id, cfg);
    Ok(())
}

fn merge_cluster_status_from_peer(status: ClusterStatus, peer_node_id: &str, cfg: &Config) {
    merge_cluster_status_inner(status, Some(peer_node_id), Some(cfg));
}

fn merge_cluster_status_inner(
    status: ClusterStatus,
    direct_peer_id: Option<&str>,
    cfg: Option<&Config>,
) {
    let mut state = CLUSTER_STATE.write().unwrap();
    let received_at = now_secs();
    if let (Some(peer_node_id), Some(cfg)) = (direct_peer_id, cfg) {
        record_peer_observations(&mut state, peer_node_id, &status.nodes, cfg, received_at);
    }
    for mut node in status.nodes {
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
    adopt_owner_view(
        &mut state,
        status.active_owner,
        status.lease_until.unwrap_or(0),
    );
    cache_current_owner_monitor_state(&mut state);
}

/// Adopts a peer's active-owner view only when it does not regress ours.
/// Without this, a peer holding a stale view could flip `active_owner`
/// back and forth on every heartbeat and defeat owner stickiness.
fn adopt_owner_view(state: &mut ClusterState, incoming_owner: Option<String>, incoming_lease: u64) {
    let Some(incoming_owner) = incoming_owner else {
        // Peer has no owner opinion: keep ours.
        return;
    };
    match state.active_owner.as_deref() {
        Some(local_owner) if local_owner == incoming_owner => {
            state.lease_until = state.lease_until.max(incoming_lease);
        }
        Some(_) => {
            // Conflicting views: adopt the peer's owner only if its lease is
            // at least as fresh as ours; otherwise keep the local view and let
            // the next election round converge.
            if incoming_lease >= state.lease_until {
                state.active_owner = Some(incoming_owner);
                state.lease_until = incoming_lease;
            }
        }
        None => {
            state.active_owner = Some(incoming_owner);
            state.lease_until = incoming_lease;
        }
    }
}

fn merged_status_last_seen(
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

fn record_peer_observations(
    state: &mut ClusterState,
    observer_node_id: &str,
    nodes: &[ClusterNodeSnapshot],
    cfg: &Config,
    now: u64,
) {
    for observations in state.peer_observations.values_mut() {
        observations.remove(observer_node_id);
    }

    for node in nodes {
        if node.node_id == cfg.cluster.node_id || node.node_id == observer_node_id {
            continue;
        }
        if !configured_node_id(cfg, &node.node_id) {
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
}

fn prune_peer_observations(state: &mut ClusterState, cfg: &Config, now: u64) {
    let timeout = cfg.cluster.failover_timeout_secs.max(1);
    state.peer_observations.retain(|node_id, observations| {
        configured_node_id(cfg, node_id) && {
            observations.retain(|observer, observed_at| {
                configured_node_id(cfg, observer) && now.saturating_sub(*observed_at) <= timeout
            });
            !observations.is_empty()
        }
    });
}

fn snapshot_is_older(incoming: &ClusterNodeSnapshot, existing: &ClusterNodeSnapshot) -> bool {
    match (incoming.last_seen, existing.last_seen) {
        (Some(incoming), Some(existing)) => incoming < existing,
        (None, Some(_)) => true,
        _ => false,
    }
}

fn mark_peer_reachable(node_id: &str) {
    let mut state = CLUSTER_STATE.write().unwrap();
    state.heartbeat_failures.remove(node_id);
}

fn mark_peer_unreachable(node_id: String, cfg: &Config) {
    let mut state = CLUSTER_STATE.write().unwrap();
    let failures = {
        let count = state.heartbeat_failures.entry(node_id.clone()).or_insert(0);
        *count = count.saturating_add(1);
        *count
    };
    let peer = cfg
        .cluster
        .peers
        .iter()
        .find(|peer| peer.node_id == node_id);
    let node = state.nodes.entry(node_id.clone()).or_insert_with(|| {
        if let Some(peer) = peer {
            empty_node(
                &peer.node_id,
                &peer.name,
                &peer.api_url,
                peer.priority,
                false,
                now_secs(),
            )
        } else {
            empty_node(&node_id, &node_id, "", 0, false, now_secs())
        }
    });
    let now = now_secs();
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

fn ffmpeg_restart_degraded(cluster: &ClusterConfig, failed_restarts: u32) -> bool {
    failed_restarts >= cluster.thresholds.max_failed_restarts.max(1)
}

fn external_api_degraded(cluster: &ClusterConfig, failures: u32) -> bool {
    failures >= cluster.thresholds.max_external_api_failures.max(1)
}

#[cfg(test)]
fn local_network_isolated(
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

fn local_network_isolated_from_state(
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

fn local_network_isolated_with(
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

fn prune_failed_restart_times(times: &mut Vec<u64>, now: u64) {
    prune_recent_times(times, now, FFMPEG_FAILURE_WINDOW_SECS);
}

fn prune_recent_times(times: &mut Vec<u64>, now: u64, window_secs: u64) {
    times.retain(|time| now.saturating_sub(*time) <= window_secs);
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn executable_sibling(name: &str) -> Option<PathBuf> {
    std::env::current_exe()
        .ok()
        .map(|path| path.with_file_name(name))
}

fn read_json_file(name: &str) -> Option<serde_json::Value> {
    let path = executable_sibling(name)?;
    let content = fs::read_to_string(path).ok()?;
    serde_json::from_str(&content).ok()
}

fn write_json_file(name: &str, value: &serde_json::Value) -> Result<(), String> {
    let path =
        executable_sibling(name).ok_or_else(|| "failed to resolve executable path".to_string())?;
    let tmp_path = path.with_extension(format!(
        "{}.tmp",
        path.extension()
            .and_then(|ext| ext.to_str())
            .unwrap_or("json")
    ));
    let json = serde_json::to_string_pretty(value).map_err(|e| e.to_string())?;
    fs::write(&tmp_path, json).map_err(|e| e.to_string())?;
    fs::rename(&tmp_path, &path).map_err(|e| e.to_string())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{
        BiliLive, ClusterHealthThresholds, Credentials, FfmpegCache, PriorityChannel,
    };

    struct ClusterStateGuard(ClusterState);

    impl ClusterStateGuard {
        fn new() -> Self {
            Self(CLUSTER_STATE.read().unwrap().clone())
        }
    }

    impl Drop for ClusterStateGuard {
        fn drop(&mut self) {
            *CLUSTER_STATE.write().unwrap() = self.0.clone();
        }
    }

    fn test_config(node_id: &str, priority: i32) -> Config {
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
    fn monitored_config_version_ignores_monitor_toggles() {
        let cfg_a = test_config("a", 0);
        let mut cfg_b = test_config("b", 10);
        cfg_b.bililive.enable_danmaku_command = !cfg_a.bililive.enable_danmaku_command;
        cfg_b.enable_youtube_monitor = !cfg_a.enable_youtube_monitor;
        cfg_b.enable_twitch_monitor = !cfg_a.enable_twitch_monitor;
        cfg_b.youtube.enable_monitor = !cfg_a.youtube.enable_monitor;
        cfg_b.twitch.enable_monitor = !cfg_a.twitch.enable_monitor;
        cfg_b.priority_channel.enabled = !cfg_a.priority_channel.enabled;
        cfg_b.priority_channel.auto_restart = !cfg_a.priority_channel.auto_restart;

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
    fn applying_monitored_config_preserves_local_monitor_toggles() {
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
    fn restart_threshold_uses_windowed_failure_count() {
        let mut cfg = test_config("a", 0);
        cfg.cluster.thresholds.max_failed_restarts = 3;

        assert!(!ffmpeg_restart_degraded(&cfg.cluster, 2));
        assert!(ffmpeg_restart_degraded(&cfg.cluster, 3));
    }

    #[test]
    fn remote_heartbeat_sender_local_flag_is_not_trusted() {
        let cfg = test_config("local", 0);
        let now = now_secs();
        let mut node = empty_node("remote", "remote", "http://remote", 1, true, now);
        node.draining = true;
        node.health = ClusterHealth::unhealthy("draining", false, false);

        update_node(node, &cfg.cluster.node_id);

        let stored = CLUSTER_STATE
            .read()
            .unwrap()
            .nodes
            .get("remote")
            .cloned()
            .expect("remote node should be stored");
        assert!(!stored.is_local);
        assert!(stored.draining);
        assert_eq!(stored.health.reason, "draining");

        CLUSTER_STATE.write().unwrap().nodes.remove("remote");
    }

    #[test]
    fn heartbeat_response_requires_peer_identity_and_fresh_snapshot() {
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

        assert!(heartbeat_response_is_valid(&valid, "peer", &cfg));

        let mut wrong_identity = valid.clone();
        wrong_identity.local_node_id = "other".to_string();
        assert!(!heartbeat_response_is_valid(&wrong_identity, "peer", &cfg));

        let mut stale = valid;
        stale.nodes[0].last_seen = Some(now - cfg.cluster.failover_timeout_secs - 1);
        assert!(!heartbeat_response_is_valid(&stale, "peer", &cfg));
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
            let mut state = CLUSTER_STATE.write().unwrap();
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

        let state = CLUSTER_STATE.read().unwrap();
        assert_eq!(state.active_owner.as_deref(), Some("owner"));
        assert_eq!(state.lease_until, 1_000);
        assert_eq!(state.last_known_active_toggles, Some(current_toggles));
        assert_eq!(
            state.last_known_active_channel_targets,
            Some(current_targets)
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
            let mut state = CLUSTER_STATE.write().unwrap();
            let mut stale_peer =
                empty_node(peer_id, peer_id, "http://direct-merge-peer", 1, false, now);
            stale_peer.last_seen = Some(now - cfg.cluster.failover_timeout_secs - 1);
            state.nodes.insert(peer_id.to_string(), stale_peer);
            state
                .heartbeat_failures
                .insert(peer_id.to_string(), HEARTBEAT_FAILURE_THRESHOLD);
        }

        let mut peer_snapshot =
            empty_node(peer_id, peer_id, "http://direct-merge-peer", 1, false, now);
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

        let state = CLUSTER_STATE.read().unwrap();
        let stored = state
            .nodes
            .get(peer_id)
            .cloned()
            .expect("peer should be stored");
        assert!(!is_stale(&stored, &cfg, now_secs()));
        assert!(!state.heartbeat_failures.contains_key(peer_id));
        drop(state);

        let mut state = CLUSTER_STATE.write().unwrap();
        state.nodes.remove(peer_id);
        state.heartbeat_failures.remove(peer_id);
    }

    #[test]
    fn indirect_quorum_can_keep_peer_eligible_without_refreshing_heartbeat() {
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
        let mut b = empty_node("b", "b", "http://b", 10, false, now);
        b.last_seen = Some(now - cfg.cluster.failover_timeout_secs - 1);
        b.health = ClusterHealth::unhealthy("heartbeat_timeout", true, false);
        state.nodes.insert("b".to_string(), b);

        // Observers must themselves be reliable nodes for their vouching to count.
        let mut c = empty_node("c", "c", "http://c", 5, false, now);
        c.last_seen = Some(now);
        c.health = ClusterHealth::healthy();
        state.nodes.insert("c".to_string(), c);
        let mut d = empty_node("d", "d", "http://d", 3, false, now);
        d.last_seen = Some(now);
        d.health = ClusterHealth::healthy();
        state.nodes.insert("d".to_string(), d);

        state
            .peer_observations
            .entry("b".to_string())
            .or_default()
            .insert("c".to_string(), now);
        state
            .peer_observations
            .entry("b".to_string())
            .or_default()
            .insert("d".to_string(), now);

        assert_eq!(choose_owner(&state, &cfg, now), Some("b".to_string()));
        assert!(is_stale(state.nodes.get("b").unwrap(), &cfg, now));
    }

    #[test]
    fn indirect_quorum_marks_peer_healthy_but_stale_for_status() {
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
        let mut b = empty_node("b", "b", "http://b", 10, false, now);
        b.last_seen = Some(now - cfg.cluster.failover_timeout_secs - 1);
        state.nodes.insert("b".to_string(), b);

        for (node_id, priority) in [("c", 5), ("d", 3)] {
            let mut observer = empty_node(
                node_id,
                node_id,
                &format!("http://{}", node_id),
                priority,
                false,
                now,
            );
            observer.last_seen = Some(now);
            observer.health = ClusterHealth::healthy();
            state.nodes.insert(node_id.to_string(), observer);
            state
                .peer_observations
                .entry("b".to_string())
                .or_default()
                .insert(node_id.to_string(), now);
        }

        normalize_node_health(&mut state, &cfg, now);

        let node = state.nodes.get("b").unwrap();
        assert!(node.health.healthy);
        assert!(node.health.stale);
        assert_eq!(node.health.reason, "indirectly_observed");
        assert_eq!(choose_owner(&state, &cfg, now), Some("b".to_string()));
    }

    #[test]
    fn single_indirect_observer_does_not_keep_peer_eligible_in_four_node_cluster() {
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
        let mut b = empty_node("b", "b", "http://b", 10, false, now);
        b.last_seen = Some(now - cfg.cluster.failover_timeout_secs - 1);
        b.health = ClusterHealth::unhealthy("heartbeat_timeout", true, false);
        state.nodes.insert("b".to_string(), b);
        state
            .peer_observations
            .entry("b".to_string())
            .or_default()
            .insert("c".to_string(), now);

        assert_eq!(choose_owner(&state, &cfg, now), None);
    }

    #[test]
    fn indirect_observers_must_be_fresh_and_healthy() {
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

        let mut b = empty_node("b", "b", "http://b", 10, false, now);
        b.last_seen = Some(now - cfg.cluster.failover_timeout_secs - 1);
        b.health = ClusterHealth::unhealthy("heartbeat_timeout", true, false);
        state.nodes.insert("b".to_string(), b);

        let mut c = empty_node("c", "c", "http://c", 5, false, now);
        c.last_seen = Some(now);
        c.health = ClusterHealth::healthy();
        state.nodes.insert("c".to_string(), c);

        let mut d = empty_node("d", "d", "http://d", 3, false, now);
        d.last_seen = Some(now);
        d.health = ClusterHealth::healthy();
        state.nodes.insert("d".to_string(), d);

        state
            .peer_observations
            .entry("b".to_string())
            .or_default()
            .insert("c".to_string(), now);
        state
            .peer_observations
            .entry("b".to_string())
            .or_default()
            .insert("d".to_string(), now);

        assert_eq!(choose_owner(&state, &cfg, now), Some("b".to_string()));

        state.nodes.get_mut("d").unwrap().health =
            ClusterHealth::unhealthy("draining", false, false);
        assert_ne!(choose_owner(&state, &cfg, now), Some("b".to_string()));
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
        state.nodes.get_mut("b").unwrap().last_seen =
            Some(now - cfg.cluster.failover_timeout_secs - 1);
        state.nodes.get_mut("b").unwrap().health =
            ClusterHealth::unhealthy("heartbeat_timeout", true, false);

        assert_eq!(choose_owner(&state, &cfg, now), Some("a".to_string()));
    }

    #[test]
    fn recovered_network_isolation_latch_clears_only_fault_quarantine() {
        let mut state = ClusterState {
            local_ddos: true,
            local_fault_ddos: true,
            local_fault_latched: true,
            local_fault_reason: Some(NETWORK_ISOLATED_REASON.to_string()),
            ..ClusterState::default()
        };

        assert!(clear_recovered_local_network_isolation(&mut state, false));
        assert!(!state.local_fault_latched);
        assert!(!state.local_fault_ddos);
        assert_eq!(state.local_fault_reason, None);
        assert!(state.local_ddos);
        assert!(local_effective_ddos_state(&state));
    }

    #[test]
    fn network_isolation_latch_stays_while_evidence_remains() {
        let mut state = ClusterState {
            local_fault_ddos: true,
            local_fault_latched: true,
            local_fault_reason: Some(NETWORK_ISOLATED_REASON.to_string()),
            ..ClusterState::default()
        };

        assert!(!clear_recovered_local_network_isolation(&mut state, true));
        assert!(state.local_fault_latched);
        assert!(state.local_fault_ddos);
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
        state.nodes.get_mut("b").unwrap().last_seen =
            Some(now - cfg.cluster.failover_timeout_secs - 1);
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
    fn owner_transition_uses_cached_active_toggles_when_previous_snapshot_is_off() {
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
            let mut state = CLUSTER_STATE.write().unwrap();
            state.last_known_active_toggles = Some(previous.clone());
        }

        assert_eq!(last_known_active_toggles(), Some(previous));

        {
            let mut state = CLUSTER_STATE.write().unwrap();
            state.last_known_active_toggles = Some(all_monitor_toggles_off());
        }
        assert!(last_known_active_toggles().is_none());
    }

    #[test]
    fn lease_selection_prefers_highest_healthy_priority() {
        let mut cfg = test_config("a", 1);
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
        ];

        let now = now_secs();
        let mut state = ClusterState::default();
        state.nodes.insert(
            "a".to_string(),
            empty_node("a", "a", "http://a", 1, true, now),
        );
        state.nodes.insert(
            "b".to_string(),
            empty_node("b", "b", "http://b", 5, false, now),
        );
        state.nodes.insert(
            "c".to_string(),
            empty_node("c", "c", "http://c", 10, false, now),
        );
        for node in state.nodes.values_mut() {
            node.health = ClusterHealth::healthy();
            node.last_seen = Some(now);
        }

        assert_eq!(choose_owner(&state, &cfg, now), Some("c".to_string()));

        state.nodes.get_mut("c").unwrap().last_seen = Some(now - 60);
        assert_eq!(choose_owner(&state, &cfg, now), Some("b".to_string()));
    }

    #[test]
    fn invalid_forced_owner_clears_to_priority_selection() {
        let mut cfg = test_config("a", 1);
        cfg.cluster.peers = vec![crate::config::ClusterPeer {
            node_id: "b".to_string(),
            name: "b".to_string(),
            api_url: "http://b".to_string(),
            priority: 5,
        }];

        let now = now_secs();
        let mut state = ClusterState {
            forced_owner: Some("a".to_string()),
            ..ClusterState::default()
        };
        state.nodes.insert(
            "a".to_string(),
            empty_node("a", "a", "http://a", 1, true, now),
        );
        state.nodes.insert(
            "b".to_string(),
            empty_node("b", "b", "http://b", 5, false, now),
        );
        for node in state.nodes.values_mut() {
            node.health = ClusterHealth::healthy();
            node.last_seen = Some(now);
        }
        state.nodes.get_mut("a").unwrap().draining = true;

        clear_invalid_forced_owner(&mut state, &cfg, now);

        assert!(state.forced_owner.is_none());
        assert_eq!(choose_owner(&state, &cfg, now), Some("b".to_string()));
    }

    #[test]
    fn healthy_active_owner_is_not_preempted_by_higher_priority_peer() {
        let mut cfg = test_config("a", 10);
        cfg.cluster.peers = vec![crate::config::ClusterPeer {
            node_id: "b".to_string(),
            name: "b".to_string(),
            api_url: "http://b".to_string(),
            priority: 1,
        }];

        let now = now_secs();
        let mut state = ClusterState {
            active_owner: Some("b".to_string()),
            ..ClusterState::default()
        };
        state.nodes.insert(
            "a".to_string(),
            empty_node("a", "a", "http://a", 10, true, now),
        );
        state.nodes.insert(
            "b".to_string(),
            empty_node("b", "b", "http://b", 1, false, now),
        );
        for node in state.nodes.values_mut() {
            node.health = ClusterHealth::healthy();
            node.last_seen = Some(now);
        }

        assert_eq!(choose_owner(&state, &cfg, now), Some("b".to_string()));
    }

    #[test]
    fn last_resort_keeps_local_when_all_peers_are_draining() {
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
        state.nodes.insert(
            "a".to_string(),
            empty_node("a", "a", "http://a", 1, true, now),
        );
        state.nodes.insert(
            "b".to_string(),
            empty_node("b", "b", "http://b", 10, false, now),
        );
        state.nodes.insert(
            "c".to_string(),
            empty_node("c", "c", "http://c", 5, false, now),
        );
        for node in state.nodes.values_mut() {
            node.last_seen = Some(now);
        }
        state.nodes.get_mut("a").unwrap().health = ClusterHealth::healthy();
        for peer_id in ["b", "c"] {
            let node = state.nodes.get_mut(peer_id).unwrap();
            node.draining = true;
            node.health = ClusterHealth::unhealthy("draining", false, false);
        }

        assert_eq!(choose_owner(&state, &cfg, now), Some("a".to_string()));
    }

    #[test]
    fn last_resort_keeps_local_when_temporarily_faulted_and_no_peer_available() {
        let mut cfg = test_config("a", 1);
        cfg.cluster.peers = vec![crate::config::ClusterPeer {
            node_id: "b".to_string(),
            name: "b".to_string(),
            api_url: "http://b".to_string(),
            priority: 10,
        }];

        let now = now_secs();
        let mut state = ClusterState::default();
        state.nodes.insert(
            "a".to_string(),
            empty_node("a", "a", "http://a", 1, true, now),
        );
        state.nodes.insert(
            "b".to_string(),
            empty_node("b", "b", "http://b", 10, false, now),
        );
        state.nodes.get_mut("a").unwrap().last_seen = Some(now);
        state.nodes.get_mut("a").unwrap().ddos = true;
        state.nodes.get_mut("a").unwrap().health =
            ClusterHealth::unhealthy("ffmpeg_repeated_failures", false, true);
        let peer = state.nodes.get_mut("b").unwrap();
        peer.last_seen = Some(now);
        peer.draining = true;
        peer.health = ClusterHealth::unhealthy("draining", false, false);

        assert_eq!(choose_owner(&state, &cfg, now), Some("a".to_string()));
    }

    #[test]
    fn no_owner_when_local_is_operator_disabled_and_peers_unavailable() {
        let mut cfg = test_config("a", 1);
        cfg.cluster.peers = vec![crate::config::ClusterPeer {
            node_id: "b".to_string(),
            name: "b".to_string(),
            api_url: "http://b".to_string(),
            priority: 10,
        }];

        let now = now_secs();
        let mut state = ClusterState::default();
        state.nodes.insert(
            "a".to_string(),
            empty_node("a", "a", "http://a", 1, true, now),
        );
        state.nodes.insert(
            "b".to_string(),
            empty_node("b", "b", "http://b", 10, false, now),
        );
        state.nodes.get_mut("a").unwrap().last_seen = Some(now);
        state.nodes.get_mut("a").unwrap().draining = true;
        state.nodes.get_mut("a").unwrap().health =
            ClusterHealth::unhealthy("draining", false, false);
        let peer = state.nodes.get_mut("b").unwrap();
        peer.last_seen = Some(now);
        peer.draining = true;
        peer.health = ClusterHealth::unhealthy("draining", false, false);

        assert_eq!(choose_owner(&state, &cfg, now), None);
    }

    #[test]
    fn stale_peer_owner_view_does_not_regress_local_view() {
        let mut state = ClusterState {
            active_owner: Some("a".to_string()),
            lease_until: 1_000,
            ..ClusterState::default()
        };

        adopt_owner_view(&mut state, Some("b".to_string()), 900);
        assert_eq!(state.active_owner.as_deref(), Some("a"));
        assert_eq!(state.lease_until, 1_000);

        adopt_owner_view(&mut state, Some("b".to_string()), 1_100);
        assert_eq!(state.active_owner.as_deref(), Some("b"));
        assert_eq!(state.lease_until, 1_100);

        adopt_owner_view(&mut state, None, 0);
        assert_eq!(state.active_owner.as_deref(), Some("b"));

        adopt_owner_view(&mut state, Some("b".to_string()), 1_500);
        assert_eq!(state.lease_until, 1_500);
    }

    #[test]
    fn adopt_owner_view_accepts_first_owner_opinion() {
        let mut state = ClusterState::default();
        adopt_owner_view(&mut state, Some("a".to_string()), 42);
        assert_eq!(state.active_owner.as_deref(), Some("a"));
        assert_eq!(state.lease_until, 42);
    }

    #[test]
    fn standby_toggles_are_kept_when_no_owner_is_elected() {
        let cfg = test_config("a", 0);
        let status_without_owner = ClusterStatus {
            enabled: true,
            local_node_id: "a".to_string(),
            active_owner: None,
            lease_until: None,
            config_version: String::new(),
            auto_failover: true,
            nodes: Vec::new(),
        };
        assert!(!should_disable_local_standby_toggles(
            &cfg,
            &status_without_owner
        ));

        let mut status_local_owner = status_without_owner.clone();
        status_local_owner.active_owner = Some("a".to_string());
        assert!(!should_disable_local_standby_toggles(
            &cfg,
            &status_local_owner
        ));

        let mut status_remote_owner = status_without_owner;
        status_remote_owner.active_owner = Some("b".to_string());
        // test_config enables youtube/twitch monitors, so toggles are non-empty.
        assert!(should_disable_local_standby_toggles(
            &cfg,
            &status_remote_owner
        ));
    }

    #[test]
    fn forced_owner_keeps_local_toggles_before_ownership_applies() {
        let cfg = test_config("b", 0);
        let status_remote_owner = ClusterStatus {
            enabled: true,
            local_node_id: "b".to_string(),
            active_owner: Some("a".to_string()),
            lease_until: Some(now_secs() + 30),
            config_version: String::new(),
            auto_failover: true,
            nodes: Vec::new(),
        };

        {
            let mut state = CLUSTER_STATE.write().unwrap();
            state.forced_owner = Some("b".to_string());
        }

        assert!(!should_disable_local_standby_toggles(
            &cfg,
            &status_remote_owner
        ));

        CLUSTER_STATE.write().unwrap().forced_owner = None;
    }

    #[test]
    fn unreachable_peer_with_fresh_inbound_heartbeat_is_not_marked_unhealthy() {
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
            let mut state = CLUSTER_STATE.write().unwrap();
            let mut node = empty_node(node_id, node_id, "http://fresh-inbound", 1, false, now);
            node.last_seen = Some(now);
            node.health = ClusterHealth::healthy();
            state.nodes.insert(node_id.to_string(), node);
            state.heartbeat_failures.remove(node_id);
        }

        for _ in 0..(HEARTBEAT_FAILURE_THRESHOLD + 1) {
            mark_peer_unreachable(node_id.to_string(), &cfg);
        }

        let stored = CLUSTER_STATE
            .read()
            .unwrap()
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
            let mut state = CLUSTER_STATE.write().unwrap();
            if let Some(node) = state.nodes.get_mut(node_id) {
                node.last_seen = Some(now - cfg.cluster.failover_timeout_secs - 5);
            }
        }
        mark_peer_unreachable(node_id.to_string(), &cfg);

        let stored = CLUSTER_STATE
            .read()
            .unwrap()
            .nodes
            .get(node_id)
            .cloned()
            .expect("peer should exist");
        assert!(!stored.health.healthy);
        assert_eq!(stored.health.reason, "api_unreachable");

        let mut state = CLUSTER_STATE.write().unwrap();
        state.nodes.remove(node_id);
        state.heartbeat_failures.remove(node_id);
    }

    #[test]
    fn auto_failover_disabled_does_not_pick_higher_priority_peer() {
        let mut cfg = test_config("a", 1);
        cfg.cluster.auto_failover = false;
        cfg.cluster.peers = vec![crate::config::ClusterPeer {
            node_id: "b".to_string(),
            name: "b".to_string(),
            api_url: "http://b".to_string(),
            priority: 100,
        }];

        let now = now_secs();
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
            empty_node("b", "b", "http://b", 100, false, now),
        );
        for node in state.nodes.values_mut() {
            node.health = ClusterHealth::healthy();
            node.last_seen = Some(now);
        }
        state.nodes.get_mut("a").unwrap().ddos = true;
        state.nodes.get_mut("a").unwrap().health =
            ClusterHealth::unhealthy("ffmpeg_repeated_failures", false, true);

        assert_eq!(choose_owner(&state, &cfg, now), Some("a".to_string()));
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
            let mut state = CLUSTER_STATE.write().unwrap();
            state.last_known_active_toggles = Some(cached.clone());
        }

        let from_cache = last_known_active_toggles().expect("cached toggles should be available");
        assert_eq!(from_cache, cached);
    }
}
