use crate::config::{
    load_config, save_config, ClusterConfig, Config, PriorityChannel, Twitch, Youtube,
};
use crate::plugins::{
    get_ffmpeg_network_stats, get_ffmpeg_speed, is_ffmpeg_hls_cache_active, is_ffmpeg_running,
    set_config_updated, set_manual_restart, stop_ffmpeg,
};
use crate::webui::state::{
    get_status_cache, refresh_status_cache_config_from, NetworkStatus, StatusData,
};
use futures_util::future::join_all;
use lazy_static::lazy_static;
use serde::{Deserialize, Serialize};
use std::collections::{hash_map::DefaultHasher, HashMap, HashSet};
use std::fs;
use std::hash::{Hash, Hasher};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

lazy_static! {
    static ref CLUSTER_STATE: RwLock<ClusterState> = RwLock::new(ClusterState::default());
}

static JSON_TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

fn recover_read_lock<'a, T>(lock: &'a RwLock<T>, name: &str) -> RwLockReadGuard<'a, T> {
    lock.read().unwrap_or_else(|poisoned| {
        tracing::warn!("Recovering poisoned {name} read lock");
        poisoned.into_inner()
    })
}

fn recover_write_lock<'a, T>(lock: &'a RwLock<T>, name: &str) -> RwLockWriteGuard<'a, T> {
    lock.write().unwrap_or_else(|poisoned| {
        tracing::warn!("Recovering poisoned {name} write lock");
        poisoned.into_inner()
    })
}

fn cluster_state_read() -> RwLockReadGuard<'static, ClusterState> {
    recover_read_lock(&CLUSTER_STATE, "cluster state")
}

fn cluster_state_write() -> RwLockWriteGuard<'static, ClusterState> {
    recover_write_lock(&CLUSTER_STATE, "cluster state")
}

const FFMPEG_FAILURE_WINDOW_SECS: u64 = 60 * 60;
const EXTERNAL_API_FAILURE_RETENTION_SECS: u64 = 60 * 60;
const HEARTBEAT_FAILURE_THRESHOLD: u32 = 3;
const NETWORK_ISOLATED_REASON: &str = "network_isolated";
const NETWORK_QUARANTINE_FILE: &str = "cluster-network-quarantine.json";

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
    local_network_quarantined: bool,
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
    pub preserve_drain: bool,
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

#[derive(Clone, Debug, Serialize, Deserialize)]
struct NetworkQuarantineSnapshot {
    node_id: String,
    monitor_toggles: MonitorToggleState,
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

pub fn cluster_sync_config_from_config(cfg: &Config) -> ClusterSyncConfigRequest {
    let monitored_config = monitored_config_from_config(cfg);
    let config_version = monitored_config_integrity_version_from_payload(&monitored_config);
    ClusterSyncConfigRequest {
        monitored_config,
        config_version,
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
    refresh_status_cache_config_from(&cfg);
    apply_danmaku_command_runtime_state(payload.enable_danmaku_command);
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

fn cache_active_monitor_state(
    state: &mut ClusterState,
    toggles: &MonitorToggleState,
    channel_targets: Option<&ChannelTargetState>,
) {
    state.last_known_active_toggles = Some(toggles.clone());
    if let Some(targets) = channel_targets.filter(|targets| channel_targets_configured(targets)) {
        state.last_known_active_channel_targets = Some(targets.clone());
    }
}

fn cache_current_owner_monitor_state(state: &mut ClusterState) {
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
    canonical_value_hash(&monitored_channel_target_value_from_config(cfg))
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

fn monitored_channel_target_value_from_config(cfg: &Config) -> serde_json::Value {
    serde_json::json!({
        "youtube": {
            "channel_name": cfg.youtube.channel_name,
            "channel_id": cfg.youtube.channel_id,
        },
        "twitch": {
            "channel_name": cfg.twitch.channel_name,
            "channel_id": cfg.twitch.channel_id,
        },
        "priority_channel": {
            "channel_name": cfg.priority_channel.channel_name,
            "youtube_channel_id": cfg.priority_channel.youtube_channel_id,
            "twitch_channel_id": cfg.priority_channel.twitch_channel_id,
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
    let mut output = String::new();
    write_canonical_json(value, &mut output);
    output
}

fn write_canonical_json(value: &serde_json::Value, output: &mut String) {
    match value {
        serde_json::Value::Null => output.push_str("null"),
        serde_json::Value::Bool(value) => output.push_str(if *value { "true" } else { "false" }),
        serde_json::Value::Number(value) => output.push_str(&value.to_string()),
        serde_json::Value::String(value) => {
            output.push_str(&serde_json::to_string(value).unwrap_or_default());
        }
        serde_json::Value::Array(values) => {
            output.push('[');
            for (index, item) in values.iter().enumerate() {
                if index > 0 {
                    output.push(',');
                }
                write_canonical_json(item, output);
            }
            output.push(']');
        }
        serde_json::Value::Object(map) => {
            let mut keys = map.keys().collect::<Vec<_>>();
            keys.sort();
            output.push('{');
            for (index, key) in keys.into_iter().enumerate() {
                if index > 0 {
                    output.push(',');
                }
                output.push_str(&serde_json::to_string(key).unwrap_or_default());
                output.push(':');
                write_canonical_json(&map[key], output);
            }
            output.push('}');
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
    refresh_status_cache_config_from(&cfg);

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

    let request = cluster_sync_config_from_config(cfg);
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

struct SourceConfigSnapshot {
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
    let client = reqwest::Client::new();
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
        },
        "enable_new_active",
    )
    .await?;

    let target_enable = ClusterDrainRequest {
        node_id: Some(target_node_id.to_string()),
        draining: false,
        ddos: false,
        propagate: Some(false),
    };
    post_cluster_drain_control(cfg, &target_enable, None).await;

    if source_node_id != target_node_id {
        if !preserve_source_drain {
            let source_standby = ClusterDrainRequest {
                node_id: Some(source_node_id.to_string()),
                draining: false,
                ddos: false,
                propagate: Some(false),
            };
            post_cluster_drain_control(cfg, &source_standby, None).await;
        }

        if let Err(e) = apply_cluster_node_mode_to_node_with_retry(
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
            },
            "disable_previous_active",
        )
        .await
        {
            tracing::warn!(
                "Failed to disable previous active {}; it will reconcile from the new active owner on recovery: {}",
                source_node_id,
                e
            );
        }
    }

    Ok(())
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

    let from_config = monitor_toggle_state_from_monitored_config(monitored_config);
    if monitored_config_toggles_known || monitor_toggles_any_enabled(&from_config) {
        return from_config;
    }

    all_monitor_toggles_on()
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

async fn export_cluster_config_from_node(
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
        .timeout(Duration::from_secs(
            cfg.cluster.heartbeat_interval_secs.max(5),
        ))
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

async fn apply_cluster_node_mode_to_node_with_retry(
    client: &reqwest::Client,
    cfg: &Config,
    node_id: &str,
    payload: ClusterApplyNodeModeRequest,
    phase: &str,
) -> Result<(), String> {
    let max_attempts = 3;
    let mut last_error = String::new();
    for attempt in 1..=max_attempts {
        match apply_cluster_node_mode_to_node(client, cfg, node_id, &payload).await {
            Ok(()) => return Ok(()),
            Err(e) => {
                last_error = e;
                tracing::warn!(
                    "Cluster node mode transfer {} attempt {}/{} failed for {}: {}",
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

async fn apply_cluster_node_mode_to_node(
    client: &reqwest::Client,
    cfg: &Config,
    node_id: &str,
    payload: &ClusterApplyNodeModeRequest,
) -> Result<(), String> {
    if node_id == cfg.cluster.node_id {
        apply_cluster_node_mode_locally(payload.clone()).await?;
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
    let response = client
        .post(url)
        .json(payload)
        .timeout(Duration::from_secs(
            cfg.cluster.heartbeat_interval_secs.max(5),
        ))
        .send()
        .await
        .map_err(|e| format!("更新节点 {} 模式失败: {}", node_id, e))?;

    if !response.status().is_success() {
        return Err(format!(
            "更新节点 {} 模式失败: HTTP {}",
            node_id,
            response.status()
        ));
    }

    let envelope = response
        .json::<PeerApiResponse<ClusterStatus>>()
        .await
        .map_err(|e| format!("解析节点 {} 模式响应失败: {}", node_id, e))?;

    if envelope.success {
        if let Some(status) = envelope.data {
            merge_cluster_status_from_direct_peer(status, node_id, cfg)?;
        }
        Ok(())
    } else {
        Err(envelope
            .message
            .unwrap_or_else(|| format!("节点 {} 拒绝模式更新", node_id)))
    }
}

pub async fn apply_cluster_node_mode_locally(
    payload: ClusterApplyNodeModeRequest,
) -> Result<ClusterStatus, String> {
    let has_monitored_config = payload.monitored_config.is_some();
    let active = payload.active;
    let restart = payload.restart;
    let preserve_drain = payload.preserve_drain;
    let monitor_toggles = payload.monitor_toggles;
    let channel_targets = payload.channel_targets;
    if let Some(monitored_config) = payload.monitored_config {
        apply_monitored_config(monitored_config).await?;
    }

    let mut cfg = load_config().await.map_err(|e| e.to_string())?;
    if let Some(channel_targets) = channel_targets.as_ref() {
        apply_channel_target_state_to_config(&mut cfg, channel_targets);
    }
    if let Some(monitor_toggles) = monitor_toggles.as_ref() {
        apply_monitor_toggle_state_to_config(&mut cfg, monitor_toggles);
    } else if !has_monitored_config {
        cfg.enable_youtube_monitor = active;
        cfg.enable_twitch_monitor = active;
        cfg.youtube.enable_monitor = active;
        cfg.twitch.enable_monitor = active;
        cfg.priority_channel.enabled = active;
        cfg.bililive.enable_danmaku_command = active;
    }

    save_config(&cfg).await.map_err(|e| e.to_string())?;

    // Promoting to active clears drain/ddos locks. Demoting to standby only
    // clears draining so the node stays eligible; disabled state is /drain only.
    if active {
        set_drain_state(&cfg, None, false, false);
    } else if !preserve_drain {
        let ddos = local_ddos_state();
        set_local_drain_state_preserving_fault(&cfg, false, ddos);
    }

    apply_danmaku_command_runtime_state(cfg.bililive.enable_danmaku_command);
    set_config_updated();
    refresh_status_cache_config_from(&cfg);

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

    get_cluster_status().await
}

async fn post_cluster_drain_control(
    cfg: &Config,
    payload: &ClusterDrainRequest,
    target_node_id: Option<&str>,
) {
    let client = reqwest::Client::new();
    let timeout = Duration::from_secs(cfg.cluster.heartbeat_interval_secs.max(5));
    let tasks = cfg
        .cluster
        .peers
        .iter()
        .filter(|peer| peer.node_id != cfg.cluster.node_id)
        .filter(|peer| {
            target_node_id
                .map(|target| peer.node_id == target)
                .unwrap_or(true)
        })
        .map(|peer| post_cluster_drain_control_to_peer(&client, cfg, peer, payload, timeout));
    join_all(tasks).await;
}

async fn post_cluster_drain_control_to_peer(
    client: &reqwest::Client,
    cfg: &Config,
    peer: &crate::config::ClusterPeer,
    payload: &ClusterDrainRequest,
    timeout: Duration,
) {
    let url = format!("{}/api/cluster/drain", peer.api_url.trim_end_matches('/'));
    match client.post(url).json(payload).timeout(timeout).send().await {
        Ok(response) if !response.status().is_success() => {
            tracing::warn!(
                "Cluster drain propagation failed for {}: HTTP {}",
                peer.node_id,
                response.status()
            );
        }
        Ok(response) => match response.json::<PeerApiResponse<ClusterStatus>>().await {
            Ok(envelope) if envelope.success => {
                if let Some(status) = envelope.data {
                    if let Err(e) =
                        merge_cluster_status_from_direct_peer(status, &peer.node_id, cfg)
                    {
                        tracing::warn!(
                            "Cluster drain response from {} ignored: {}",
                            peer.node_id,
                            e
                        );
                    }
                }
            }
            Ok(envelope) => {
                tracing::warn!(
                    "Cluster drain rejected by {}: {:?}",
                    peer.node_id,
                    envelope.message
                );
            }
            Err(e) => {
                tracing::warn!(
                    "Cluster drain response parse failed for {}: {}",
                    peer.node_id,
                    e
                );
            }
        },
        Err(e) => {
            tracing::warn!(
                "Cluster drain propagation failed for {}: {}",
                peer.node_id,
                e
            );
        }
    }
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

            let config_version = monitored_config_version(&cfg);
            let local = collect_local_snapshot(&cfg, config_version.clone()).await;
            let previous_owner = current_active_owner();
            update_node(local.clone(), &cfg.cluster.node_id);
            send_heartbeats(&client, &cfg, local).await;
            let status = compute_cluster_status_with_version(&cfg, config_version);
            handle_auto_owner_transition(&cfg, previous_owner, &status).await;
            enforce_local_network_quarantine(&cfg, &status).await;
            enforce_local_standby_toggles(&cfg, &status).await;

            if let Some(owner) = status.active_owner.as_deref() {
                if owner != cfg.cluster.node_id.as_str() && is_ffmpeg_running().await {
                    tracing::warn!("集群租约已转移，停止本节点 ffmpeg 推流");
                    set_manual_restart();
                    clear_local_stream();
                    stop_ffmpeg().await;
                }
            }

            tokio::time::sleep(heartbeat_sleep_duration(&cfg)).await;
        }
    });
}

fn current_active_owner() -> Option<String> {
    cluster_state_read().active_owner.clone()
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
    cluster_state_read().forced_owner.clone()
}

pub fn local_ddos_state() -> bool {
    cluster_state_read().local_ddos
}

fn local_effective_ddos_state(state: &ClusterState) -> bool {
    state.local_ddos || state.local_fault_ddos
}

async fn handle_auto_owner_transition(
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

    let preserve_source_drain = status
        .nodes
        .iter()
        .find(|node| node.node_id == previous_owner)
        .is_some_and(|node| node.draining);
    tracing::warn!(
        "集群自动故障转移: {} -> {}, transfer active config and monitor state",
        previous_owner,
        new_owner
    );

    if let Err(e) = finalize_cluster_node_switch(
        cfg,
        status,
        &previous_owner,
        new_owner,
        preserve_source_drain,
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
}

fn monitor_toggles_all_off(toggles: &MonitorToggleState) -> bool {
    *toggles == all_monitor_toggles_off()
}

pub(crate) fn monitor_toggles_any_enabled(toggles: &MonitorToggleState) -> bool {
    !monitor_toggles_all_off(toggles)
}

pub(crate) fn node_monitor_toggles_are_known(node: &ClusterNodeSnapshot) -> bool {
    monitor_toggles_any_enabled(&node.monitor_toggles)
        || node.last_seen.is_some()
        || node.is_local
        || node.status.is_some()
        || !node.config_version.is_empty()
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
    cluster_state_read().last_known_active_toggles.clone()
}

pub(crate) fn last_known_active_channel_targets() -> Option<ChannelTargetState> {
    cluster_state_read()
        .last_known_active_channel_targets
        .as_ref()
        .filter(|targets| channel_targets_configured(targets))
        .cloned()
}

async fn enforce_local_network_quarantine(cfg: &Config, status: &ClusterStatus) {
    let local_is_network_isolated = status
        .nodes
        .iter()
        .find(|node| node.node_id == cfg.cluster.node_id)
        .is_some_and(|node| node.health.reason == NETWORK_ISOLATED_REASON);

    if local_is_network_isolated {
        if let Err(e) = enter_local_network_quarantine(&cfg.cluster.node_id).await {
            tracing::warn!("Failed to quarantine local network-isolated node: {}", e);
        }
        return;
    }

    let active_owner = status.active_owner.as_deref();
    if active_owner == Some(cfg.cluster.node_id.as_str()) || active_owner.is_none() {
        if let Err(e) = recover_local_network_quarantine(&cfg.cluster.node_id).await {
            tracing::warn!("Failed to recover local network quarantine: {}", e);
        }
        return;
    }

    if let Err(e) = clear_local_network_quarantine() {
        tracing::warn!("Failed to clear local network quarantine snapshot: {}", e);
    }
}

async fn enter_local_network_quarantine(node_id: &str) -> Result<(), String> {
    let current_toggles = crate::config::load_config()
        .await
        .map_err(|e| e.to_string())
        .map(|cfg| monitor_toggle_state_from_config(&cfg))?;
    let persisted_toggles = read_network_quarantine_snapshot(node_id);
    let (should_apply_quarantine, snapshot) = {
        let mut state = cluster_state_write();
        enter_local_network_quarantine_state(
            &mut state,
            node_id,
            current_toggles,
            persisted_toggles,
        )
    };
    if let Some(snapshot) = snapshot {
        if let Err(e) = write_network_quarantine_snapshot(&snapshot) {
            tracing::warn!(
                "Failed to persist local network quarantine snapshot; continuing quarantine: {}",
                e
            );
        }
    }
    if !should_apply_quarantine {
        return Ok(());
    }
    apply_monitor_toggle_state(all_monitor_toggles_off()).await?;
    set_manual_restart();
    clear_local_stream();
    stop_ffmpeg().await;
    Ok(())
}

async fn recover_local_network_quarantine(node_id: &str) -> Result<(), String> {
    let persisted_toggles = read_network_quarantine_snapshot(node_id);
    let toggles = {
        let mut state = cluster_state_write();
        let Some(toggles) =
            take_local_network_quarantine_recovery_toggles(&mut state, persisted_toggles)
        else {
            return Ok(());
        };
        toggles
    };
    apply_monitor_toggle_state(toggles).await?;
    remove_network_quarantine_snapshot()
}

fn clear_local_network_quarantine() -> Result<(), String> {
    let mut state = cluster_state_write();
    state.local_network_quarantined = false;
    drop(state);
    remove_network_quarantine_snapshot()
}

fn enter_local_network_quarantine_state(
    state: &mut ClusterState,
    node_id: &str,
    current_toggles: MonitorToggleState,
    persisted_toggles: Option<MonitorToggleState>,
) -> (bool, Option<NetworkQuarantineSnapshot>) {
    let toggles_enabled = monitor_toggles_any_enabled(&current_toggles);
    let was_quarantined = state.local_network_quarantined;
    let has_persisted_toggles = persisted_toggles.is_some();
    let should_capture = toggles_enabled || (!was_quarantined && !has_persisted_toggles);
    let snapshot = if should_capture {
        state.last_known_active_toggles = Some(current_toggles.clone());
        Some(NetworkQuarantineSnapshot {
            node_id: node_id.to_string(),
            monitor_toggles: current_toggles,
        })
    } else {
        if let Some(persisted) = persisted_toggles {
            state.last_known_active_toggles = Some(persisted);
        }
        None
    };
    state.local_network_quarantined = true;
    (!was_quarantined || toggles_enabled, snapshot)
}

fn take_local_network_quarantine_recovery_toggles(
    state: &mut ClusterState,
    persisted_toggles: Option<MonitorToggleState>,
) -> Option<MonitorToggleState> {
    if !state.local_network_quarantined && persisted_toggles.is_none() {
        return None;
    }
    state.local_network_quarantined = false;
    Some(
        state
            .last_known_active_toggles
            .clone()
            .or(persisted_toggles)
            .unwrap_or_else(all_monitor_toggles_on),
    )
}

fn read_network_quarantine_snapshot(node_id: &str) -> Option<MonitorToggleState> {
    let value = read_json_file(NETWORK_QUARANTINE_FILE)?;
    let snapshot = serde_json::from_value::<NetworkQuarantineSnapshot>(value).ok()?;
    if snapshot.node_id != node_id {
        return None;
    }
    Some(snapshot.monitor_toggles)
}

fn write_network_quarantine_snapshot(snapshot: &NetworkQuarantineSnapshot) -> Result<(), String> {
    let value = serde_json::to_value(snapshot).map_err(|e| e.to_string())?;
    write_json_file(NETWORK_QUARANTINE_FILE, &value)
}

fn remove_network_quarantine_snapshot() -> Result<(), String> {
    let Some(path) = executable_sibling(NETWORK_QUARANTINE_FILE) else {
        return Ok(());
    };
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.to_string()),
    }
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
    if cluster_state_read()
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
        let mut state = cluster_state_write();
        state.local_stream = stream.clone();
    }

    let may_push = local_has_active_lease(cfg).await;
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

fn latch_local_fault(reason: impl Into<String>) {
    let mut state = cluster_state_write();
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
    let config_version = monitored_config_version(cfg);
    if !cfg.cluster.enabled {
        return compute_cluster_status_with_version(cfg, config_version);
    }
    let local = collect_local_snapshot(cfg, config_version.clone()).await;
    update_node(local, &cfg.cluster.node_id);
    compute_cluster_status_with_version(cfg, config_version)
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
    if !configured_node_id(cfg, &node.node_id) {
        tracing::debug!("Ignored heartbeat from unknown node {}", node.node_id);
        return false;
    }
    node.last_seen = Some(now_secs());
    mark_peer_reachable(&node.node_id);
    update_node(node, &cfg.cluster.node_id);
    true
}

pub fn receive_heartbeat(cfg: &Config, node: ClusterNodeSnapshot) -> ClusterStatus {
    record_heartbeat(cfg, node);
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
    let mut state = cluster_state_write();
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
        let mut state = cluster_state_write();
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
            mark_peer_unreachable(&peer.node_id, cfg);
        }
        Ok(response) => match response.json::<PeerApiResponse<ClusterStatus>>().await {
            Ok(envelope) if envelope.success => {
                if let Some(status) = envelope.data {
                    if heartbeat_response_is_valid(&status, &peer.node_id, cfg) {
                        let peer_auto_failover = status.auto_failover;
                        let peer_is_active_owner =
                            status.active_owner.as_deref() == Some(peer.node_id.as_str());
                        mark_peer_reachable(&peer.node_id);
                        merge_cluster_status_from_peer(status, &peer.node_id, cfg);
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

/// Adopt the active owner's auto-failover setting when a peer heartbeat shows we
/// missed a membership sync (e.g. node was offline during a toggle).
async fn adopt_auto_failover_from_peer_view(
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
    if let Err(e) = save_config(&updated).await {
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

fn should_adopt_auto_failover_from_peer(
    peer_auto_failover: bool,
    peer_is_active_owner: bool,
    cfg: &Config,
) -> bool {
    cfg.cluster.enabled && peer_is_active_owner && peer_auto_failover != cfg.cluster.auto_failover
}

fn heartbeat_response_is_valid(status: &ClusterStatus, peer_node_id: &str, _cfg: &Config) -> bool {
    status.local_node_id == peer_node_id
        && status.nodes.iter().any(|node| node.node_id == peer_node_id)
}

async fn collect_local_snapshot(cfg: &Config, config_version: String) -> ClusterNodeSnapshot {
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
            now,
        )
    };

    let stream_degraded = ffmpeg_restart_degraded(&cfg.cluster, failed_restarts);
    let external_api_degraded = external_api_degraded(&cfg.cluster, external_api_failures);
    if fault_latched
        && fault_reason.as_deref() == Some(NETWORK_ISOLATED_REASON)
        && !network_isolated
    {
        let mut state = cluster_state_write();
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
        last_seen: Some(observed_at),
        is_local: true,
        role: ClusterNodeRole::Standby,
        health,
        draining,
        ddos: effective_ddos,
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
    compute_cluster_status_with_version(cfg, monitored_config_version(cfg))
}

fn compute_cluster_status_with_version(cfg: &Config, config_version: String) -> ClusterStatus {
    if !cfg.cluster.enabled {
        return ClusterStatus {
            enabled: false,
            local_node_id: cfg.cluster.node_id.clone(),
            active_owner: None,
            lease_until: None,
            config_version,
            auto_failover: cfg.cluster.auto_failover,
            nodes: Vec::new(),
        };
    }

    let now = now_secs();
    let mut state = cluster_state_write();
    let configured = configured_node_ids(cfg);
    ensure_configured_nodes(&mut state, cfg, now, &configured);
    prune_peer_observations(&mut state, cfg, now, &configured);
    normalize_node_health(&mut state, cfg, now, &configured);
    clear_invalid_forced_owner(&mut state, cfg, now, &configured);

    let chosen = choose_owner_with_configured(&state, cfg, now, &configured);
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
        config_version,
        auto_failover: cfg.cluster.auto_failover,
        nodes,
    }
}

fn clear_invalid_forced_owner(
    state: &mut ClusterState,
    cfg: &Config,
    now: u64,
    configured: &HashSet<&str>,
) {
    let forced_owner_is_eligible = state
        .forced_owner
        .as_ref()
        .and_then(|owner| state.nodes.get(owner))
        .is_some_and(|node| node_is_eligible(node, state, cfg, now, configured));

    if state.forced_owner.is_some() && !forced_owner_is_eligible {
        state.forced_owner = None;
    }
}

#[cfg(test)]
fn choose_owner(state: &ClusterState, cfg: &Config, now: u64) -> Option<String> {
    let configured = configured_node_ids(cfg);
    choose_owner_with_configured(state, cfg, now, &configured)
}

fn choose_owner_with_configured(
    state: &ClusterState,
    cfg: &Config,
    now: u64,
    configured: &HashSet<&str>,
) -> Option<String> {
    let forced_owner = state
        .forced_owner
        .as_ref()
        .filter(|owner| {
            state
                .nodes
                .get(*owner)
                .is_some_and(|node| node_is_eligible(node, state, cfg, now, configured))
        })
        .cloned();
    if forced_owner.is_some() {
        return forced_owner;
    }

    if !cfg.cluster.auto_failover {
        return state
            .active_owner
            .as_ref()
            .filter(|owner| configured.contains(owner.as_str()))
            .cloned()
            .or_else(|| last_resort_local_owner(state, cfg));
    }

    if let Some(current_owner) = state.active_owner.as_ref() {
        if state
            .nodes
            .get(current_owner)
            .is_some_and(|node| node_is_eligible(node, state, cfg, now, configured))
        {
            return Some(current_owner.clone());
        }
    }

    state
        .nodes
        .values()
        .filter(|node| node_is_eligible(node, state, cfg, now, configured))
        .max_by(|a, b| {
            a.priority
                .cmp(&b.priority)
                .then_with(|| b.node_id.cmp(&a.node_id))
        })
        .map(|node| node.node_id.clone())
        .or_else(|| last_resort_local_owner(state, cfg))
}

/// When every peer is disabled/unavailable, keep an active local node running,
/// but never let an all-off standby self-elect.
fn last_resort_local_owner(state: &ClusterState, cfg: &Config) -> Option<String> {
    if !monitor_toggles_any_enabled(&monitor_toggle_state_from_config(cfg)) {
        return None;
    }

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
    configured: &HashSet<&str>,
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
    indirectly_observed_by_quorum(state, &node.node_id, cfg, now, configured)
}

fn indirectly_observed_by_quorum(
    state: &ClusterState,
    node_id: &str,
    cfg: &Config,
    now: u64,
    configured: &HashSet<&str>,
) -> bool {
    let Some(observations) = state.peer_observations.get(node_id) else {
        return false;
    };
    let timeout = cfg.cluster.failover_timeout_secs.max(1);
    let fresh_observers = observations
        .iter()
        .filter(|(observer, observed_at)| {
            configured.contains(observer.as_str())
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

fn configured_node_ids(cfg: &Config) -> HashSet<&str> {
    let mut configured = HashSet::with_capacity(cfg.cluster.peers.len() + 1);
    configured.insert(cfg.cluster.node_id.as_str());
    for peer in &cfg.cluster.peers {
        configured.insert(peer.node_id.as_str());
    }
    configured
}

fn normalize_node_health(
    state: &mut ClusterState,
    cfg: &Config,
    now: u64,
    configured: &HashSet<&str>,
) {
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
                && indirectly_observed_by_quorum(state, node_id, cfg, now, configured)
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

fn ensure_configured_nodes(
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
    let mut state = cluster_state_write();
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
    let mut state = cluster_state_write();
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
        cfg.map(|cfg| cfg.cluster.auto_failover).unwrap_or(true),
        direct_peer_id,
    );
    cache_current_owner_monitor_state(&mut state);
}

/// Adopts a peer's active-owner view only when it does not regress ours.
/// Without this, a peer holding a stale view could flip `active_owner`
/// back and forth on every heartbeat and defeat owner stickiness.
fn adopt_owner_view(
    state: &mut ClusterState,
    incoming_owner: Option<String>,
    incoming_lease: u64,
    auto_failover: bool,
    direct_peer_id: Option<&str>,
) {
    let Some(incoming_owner) = incoming_owner else {
        // Peer has no owner opinion: keep ours.
        return;
    };
    match state.active_owner.as_deref() {
        Some(local_owner) if local_owner == incoming_owner => {
            state.lease_until = state.lease_until.max(incoming_lease);
        }
        Some(_) => {
            if !auto_failover {
                if direct_peer_id == Some(incoming_owner.as_str()) {
                    if state.forced_owner.as_deref() != Some(incoming_owner.as_str()) {
                        state.forced_owner = None;
                    }
                    state.active_owner = Some(incoming_owner);
                    state.lease_until = incoming_lease;
                }
                return;
            }
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

fn prune_peer_observations(
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

fn snapshot_is_older(incoming: &ClusterNodeSnapshot, existing: &ClusterNodeSnapshot) -> bool {
    match (incoming.last_seen, existing.last_seen) {
        (Some(incoming), Some(existing)) => incoming < existing,
        (None, Some(_)) => true,
        _ => false,
    }
}

fn mark_peer_reachable(node_id: &str) {
    let mut state = cluster_state_write();
    state.heartbeat_failures.remove(node_id);
}

fn mark_peer_unreachable(node_id: &str, cfg: &Config) {
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

fn record_recent_time(times: &mut Vec<u64>, time: u64) {
    if times.last().is_none_or(|last| *last <= time) {
        times.push(time);
        return;
    }

    let insert_at = times.partition_point(|recorded| *recorded <= time);
    times.insert(insert_at, time);
}

fn prune_recent_times(times: &mut Vec<u64>, now: u64, window_secs: u64) {
    debug_assert!(
        times.windows(2).all(|window| window[0] <= window[1]),
        "recent timestamp windows must be sorted for prefix pruning"
    );
    let first_recent = times.partition_point(|time| now.saturating_sub(*time) > window_secs);
    if first_recent > 0 {
        times.drain(0..first_recent);
    }
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
    let json = serde_json::to_string_pretty(value).map_err(|e| e.to_string())?;
    write_json_file_atomic(&path, json.as_bytes()).map_err(|e| e.to_string())
}

fn write_json_file_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let (tmp_path, mut tmp_file) = create_unique_json_tmp_file(path)?;
    let write_result = tmp_file.write_all(bytes).and_then(|_| tmp_file.sync_all());
    drop(tmp_file);

    let result = write_result.and_then(|_| fs::rename(&tmp_path, path));

    if result.is_err() {
        let _ = fs::remove_file(&tmp_path);
    }
    result
}

fn create_unique_json_tmp_file(path: &Path) -> std::io::Result<(PathBuf, fs::File)> {
    const MAX_ATTEMPTS: usize = 16;
    for _ in 0..MAX_ATTEMPTS {
        let tmp_path = unique_json_tmp_path(path);
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp_path)
        {
            Ok(file) => return Ok((tmp_path, file)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        "failed to reserve unique cluster json temporary file",
    ))
}

fn unique_json_tmp_path(path: &Path) -> PathBuf {
    let extension = path
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or("json");
    let suffix = JSON_TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    path.with_extension(format!(
        "{}.tmp-{}-{}",
        extension,
        std::process::id(),
        suffix
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{
        BiliLive, ClusterHealthThresholds, Credentials, FfmpegCache, PriorityChannel,
    };
    use std::sync::{Mutex, MutexGuard};

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

    struct ClusterStateGuard {
        _lock: MutexGuard<'static, ()>,
        snapshot: ClusterState,
    }

    impl ClusterStateGuard {
        fn new() -> Self {
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

        assert!(heartbeat_response_is_valid(&valid, "peer", &cfg));

        let mut wrong_identity = valid.clone();
        wrong_identity.local_node_id = "other".to_string();
        assert!(!heartbeat_response_is_valid(&wrong_identity, "peer", &cfg));

        let mut skewed = valid.clone();
        skewed.nodes[0].last_seen = Some(now - cfg.cluster.failover_timeout_secs - 1);
        assert!(heartbeat_response_is_valid(&skewed, "peer", &cfg));

        let mut missing_snapshot = valid;
        missing_snapshot.nodes[0].node_id = "other".to_string();
        assert!(!heartbeat_response_is_valid(
            &missing_snapshot,
            "peer",
            &cfg
        ));
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

        cluster_state_write().nodes.remove("local");
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

        let configured = configured_node_ids(&cfg);
        normalize_node_health(&mut state, &cfg, now, &configured);

        let node = state.nodes.get("b").unwrap();
        assert!(node.health.healthy);
        assert!(node.health.stale);
        assert_eq!(node.health.reason, "indirectly_observed");
        assert_eq!(choose_owner(&state, &cfg, now), Some("b".to_string()));
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

        let configured = configured_node_ids(&cfg);
        normalize_node_health(&mut state, &cfg, now + 1_000, &configured);

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
    fn network_quarantine_caches_enabled_toggles_and_stays_idempotent_after_off() {
        let mut state = ClusterState::default();
        let enabled = MonitorToggleState {
            enable_youtube_monitor: true,
            youtube_enable_monitor: true,
            ..all_monitor_toggles_off()
        };

        let (should_apply, snapshot) =
            enter_local_network_quarantine_state(&mut state, "a", enabled.clone(), None);
        assert!(should_apply);
        assert_eq!(
            snapshot.map(|snapshot| snapshot.monitor_toggles),
            Some(enabled.clone())
        );
        assert!(state.local_network_quarantined);
        assert_eq!(state.last_known_active_toggles, Some(enabled.clone()));

        let (should_apply, snapshot) =
            enter_local_network_quarantine_state(&mut state, "a", all_monitor_toggles_off(), None);
        assert!(!should_apply);
        assert!(snapshot.is_none());
        assert_eq!(state.last_known_active_toggles, Some(enabled));
    }

    #[test]
    fn network_quarantine_hydrates_from_persisted_snapshot_after_restart() {
        let mut state = ClusterState::default();
        let persisted = MonitorToggleState {
            enable_twitch_monitor: true,
            twitch_enable_monitor: true,
            ..all_monitor_toggles_off()
        };

        let (should_apply, snapshot) = enter_local_network_quarantine_state(
            &mut state,
            "a",
            all_monitor_toggles_off(),
            Some(persisted.clone()),
        );

        assert!(should_apply);
        assert!(snapshot.is_none());
        assert!(state.local_network_quarantined);
        assert_eq!(state.last_known_active_toggles, Some(persisted));
    }

    #[test]
    fn network_quarantine_preserves_all_off_as_known_active_state() {
        let mut state = ClusterState::default();

        let (should_apply, snapshot) =
            enter_local_network_quarantine_state(&mut state, "a", all_monitor_toggles_off(), None);

        assert!(should_apply);
        assert_eq!(
            snapshot.map(|snapshot| snapshot.monitor_toggles),
            Some(all_monitor_toggles_off())
        );
        assert_eq!(
            take_local_network_quarantine_recovery_toggles(&mut state, None),
            Some(all_monitor_toggles_off())
        );
    }

    #[test]
    fn network_quarantine_recovery_uses_cached_persisted_or_all_on() {
        let cached = MonitorToggleState {
            enable_danmaku_command: true,
            priority_channel_enabled: true,
            ..all_monitor_toggles_off()
        };
        let mut state = ClusterState {
            local_network_quarantined: true,
            last_known_active_toggles: Some(cached.clone()),
            ..ClusterState::default()
        };

        assert_eq!(
            take_local_network_quarantine_recovery_toggles(&mut state, None),
            Some(cached)
        );
        assert!(!state.local_network_quarantined);
        assert_eq!(
            take_local_network_quarantine_recovery_toggles(&mut state, None),
            None
        );

        let persisted = MonitorToggleState {
            enable_youtube_monitor: true,
            youtube_enable_monitor: true,
            ..all_monitor_toggles_off()
        };
        let mut restarted_state = ClusterState::default();
        assert_eq!(
            take_local_network_quarantine_recovery_toggles(
                &mut restarted_state,
                Some(persisted.clone())
            ),
            Some(persisted)
        );

        let mut state_without_cache = ClusterState {
            local_network_quarantined: true,
            ..ClusterState::default()
        };
        assert_eq!(
            take_local_network_quarantine_recovery_toggles(&mut state_without_cache, None),
            Some(all_monitor_toggles_on())
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

        let configured = configured_node_ids(&cfg);
        normalize_node_health(&mut state, &cfg, now, &configured);

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
    fn last_known_active_toggles_preserves_all_off() {
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

        let configured = configured_node_ids(&cfg);
        clear_invalid_forced_owner(&mut state, &cfg, now, &configured);

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

        adopt_owner_view(&mut state, Some("b".to_string()), 900, true, None);
        assert_eq!(state.active_owner.as_deref(), Some("a"));
        assert_eq!(state.lease_until, 1_000);

        adopt_owner_view(&mut state, Some("b".to_string()), 1_100, true, None);
        assert_eq!(state.active_owner.as_deref(), Some("b"));
        assert_eq!(state.lease_until, 1_100);

        adopt_owner_view(&mut state, None, 0, true, None);
        assert_eq!(state.active_owner.as_deref(), Some("b"));

        adopt_owner_view(&mut state, Some("b".to_string()), 1_500, true, None);
        assert_eq!(state.lease_until, 1_500);
    }

    #[test]
    fn adopt_owner_view_accepts_first_owner_opinion() {
        let mut state = ClusterState::default();
        adopt_owner_view(&mut state, Some("a".to_string()), 42, false, None);
        assert_eq!(state.active_owner.as_deref(), Some("a"));
        assert_eq!(state.lease_until, 42);
    }

    #[test]
    fn manual_mode_peer_owner_view_does_not_replace_existing_owner() {
        let mut state = ClusterState {
            active_owner: Some("a".to_string()),
            lease_until: 1_000,
            ..ClusterState::default()
        };

        adopt_owner_view(&mut state, Some("b".to_string()), 1_100, false, None);

        assert_eq!(state.active_owner.as_deref(), Some("a"));
        assert_eq!(state.lease_until, 1_000);
    }

    #[test]
    fn manual_mode_accepts_direct_active_owner_view_after_recovery() {
        let mut state = ClusterState {
            active_owner: Some("a".to_string()),
            forced_owner: Some("a".to_string()),
            lease_until: 1_000,
            ..ClusterState::default()
        };

        adopt_owner_view(&mut state, Some("b".to_string()), 1_100, false, Some("b"));

        assert_eq!(state.active_owner.as_deref(), Some("b"));
        assert_eq!(state.forced_owner, None);
        assert_eq!(state.lease_until, 1_100);
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
            let mut state = cluster_state_write();
            state.forced_owner = Some("b".to_string());
        }

        assert!(!should_disable_local_standby_toggles(
            &cfg,
            &status_remote_owner
        ));

        cluster_state_write().forced_owner = None;
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

        let stored = CLUSTER_STATE
            .read()
            .unwrap()
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

        assert!(!local_may_push(&cfg, Some(stream)).await);
        assert_eq!(cluster_state_read().local_stream, None);
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
    fn manual_mode_keeps_unhealthy_remote_owner() {
        let mut cfg = test_config("a", 100);
        cfg.cluster.auto_failover = false;
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
            empty_node("a", "a", "http://a", 100, true, now),
        );
        state.nodes.insert(
            "b".to_string(),
            empty_node("b", "b", "http://b", 1, false, now),
        );
        state.nodes.get_mut("a").unwrap().health = ClusterHealth::healthy();
        state.nodes.get_mut("a").unwrap().last_seen = Some(now);
        state.nodes.get_mut("b").unwrap().ddos = true;
        state.nodes.get_mut("b").unwrap().health =
            ClusterHealth::unhealthy("ffmpeg_repeated_failures", false, true);
        state.nodes.get_mut("b").unwrap().last_seen = Some(now);

        assert_eq!(choose_owner(&state, &cfg, now), Some("b".to_string()));
    }

    #[test]
    fn manual_mode_standby_without_toggles_does_not_self_elect() {
        let mut cfg = test_config("a", 100);
        cfg.cluster.auto_failover = false;
        cfg.bililive.enable_danmaku_command = false;
        cfg.enable_youtube_monitor = false;
        cfg.enable_twitch_monitor = false;
        cfg.youtube.enable_monitor = false;
        cfg.twitch.enable_monitor = false;
        cfg.priority_channel.enabled = false;
        cfg.priority_channel.auto_restart = false;
        cfg.cluster.peers = vec![crate::config::ClusterPeer {
            node_id: "b".to_string(),
            name: "b".to_string(),
            api_url: "http://b".to_string(),
            priority: 1,
        }];

        let now = now_secs();
        let mut state = ClusterState::default();
        state.nodes.insert(
            "a".to_string(),
            empty_node("a", "a", "http://a", 100, true, now),
        );
        state.nodes.get_mut("a").unwrap().health = ClusterHealth::healthy();
        state.nodes.get_mut("a").unwrap().last_seen = Some(now);

        assert_eq!(choose_owner(&state, &cfg, now), None);
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
}
