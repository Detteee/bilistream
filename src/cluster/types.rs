//! Cluster snapshots, requests, and monitor-toggle / channel-target helpers.

use crate::config::{Config, PriorityChannel, Twitch, Youtube};
use crate::webui::state::{NetworkStatus, StatusData};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

pub(crate) const NETWORK_ISOLATED_REASON: &str = "network_isolated";
pub(crate) const NETWORK_UNSTABLE_REASON: &str = "network_unstable";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ClusterStatus {
    pub enabled: bool,
    pub local_node_id: String,
    pub active_owner: Option<String>,
    pub lease_until: Option<u64>,
    pub config_version: String,
    #[serde(default = "default_true")]
    pub auto_failover: bool,
    #[serde(default)]
    pub public_status: crate::config::PublicStatusConfig,
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
    #[serde(default)]
    pub network_unstable: bool,
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

impl ClusterNodeSnapshot {
    pub(crate) fn is_serviceable(&self) -> bool {
        self.health.healthy && !self.draining && !self.network_unstable
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum ClusterNodeRole {
    Active,
    Standby,
    Draining,
    Unhealthy,
}

#[derive(Clone, Debug, Serialize, Deserialize, Hash)]
pub struct ClusterHealth {
    pub healthy: bool,
    pub reason: String,
    pub stale: bool,
    pub stream_degraded: bool,
}

impl ClusterHealth {
    pub(crate) fn healthy() -> Self {
        Self {
            healthy: true,
            reason: "healthy".to_string(),
            stale: false,
            stream_degraded: false,
        }
    }

    pub(crate) fn unhealthy(reason: impl Into<String>, stale: bool, stream_degraded: bool) -> Self {
        Self {
            healthy: false,
            reason: reason.into(),
            stale,
            stream_degraded,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, Hash)]
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
pub struct ClusterPublicStatusRequest {
    pub config: crate::config::PublicStatusConfig,
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
    #[serde(default)]
    pub sender_node_id: String,
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
    #[serde(default)]
    pub expected_active_owner: Option<String>,
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

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq, Hash)]
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

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq, Hash)]
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
pub(crate) struct PeerApiResponse<T> {
    pub(crate) success: bool,
    pub(crate) data: Option<T>,
    pub(crate) message: Option<String>,
}

pub(crate) fn default_true() -> bool {
    true
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

pub fn apply_monitor_toggle_state_to_config(cfg: &mut Config, payload: &MonitorToggleState) {
    cfg.bililive.enable_danmaku_command = payload.enable_danmaku_command;
    cfg.enable_youtube_monitor = payload.enable_youtube_monitor;
    cfg.enable_twitch_monitor = payload.enable_twitch_monitor;
    cfg.youtube.enable_monitor = payload.youtube_enable_monitor;
    cfg.twitch.enable_monitor = payload.twitch_enable_monitor;
    cfg.priority_channel.enabled = payload.priority_channel_enabled;
    cfg.priority_channel.auto_restart = payload.priority_channel_auto_restart;
}

pub(crate) fn monitor_toggles_all_off(toggles: &MonitorToggleState) -> bool {
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
