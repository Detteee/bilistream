//! Cluster snapshots, requests, and monitor-toggle / channel-target helpers.

use crate::config::{Config, PriorityChannel, Twitch, Youtube};
use crate::plugins::stream_item::StreamItem;
use crate::plugins::youtube_data::YtVideo;
use crate::webui::state::{NetworkStatus, StatusData};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};

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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub self_check: Option<super::self_check::SelfCheckStatus>,
    pub node_id: String,
    pub name: String,
    pub api_url: String,
    pub priority: i32,
    pub last_seen: Option<u64>,
    pub is_local: bool,
    pub role: ClusterNodeRole,
    pub health: ClusterHealth,
    /// Explicit operator maintenance. Automatic faults never set this flag.
    pub draining: bool,
    /// Historical wire name for the local fault latch, including upstream and
    /// publisher failures as well as network isolation.
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
    /// Version of the YouTube index this node serves while it answers for the
    /// cluster (`cluster::yt_index`). Older nodes neither send nor read it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub yt_index_version: Option<String>,
    /// This node's WebSub subscriptions while it subscribes (the index node).
    /// Older nodes neither send nor read it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub websub: Option<WebSubCounts>,
    /// How this node answers YouTube; `None` without a cluster or public node.
    /// Older nodes neither send nor read it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub yt_index: Option<YtIndexNodeState>,
    /// What the notify authority already sent and spent (`notify::ledger`).
    /// Holds no secret and no openid. Older nodes neither send nor read it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notify: Option<crate::notify::NotifyLedger>,
    /// This node has the QQ AppSecret, AppID and admin openid, so it may
    /// become the notify sender. Older nodes report `false`.
    #[serde(default)]
    pub notify_capable: bool,
    /// This node is the notify sender right now (review §7). Peers use it
    /// to decide whether the no-quorum fallback may start.
    #[serde(default)]
    pub notify_authority: bool,
    /// This node holds a QQ gateway session right now (contract line 70: the
    /// old holder closes before the new one identifies). `None` = an older
    /// node that does not report it (unknown).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notify_gateway_held: Option<bool>,
    /// Batch 1b: this node has the AppSecret / the admin openid (booleans
    /// only, never the values). `None` = an older node.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notify_appsecret_set: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notify_appid_set: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notify_openid_set: Option<bool>,
    /// Addendum F: this node takes the synced credential set on the openid
    /// route; its set generation; its openid is bound under another AppID.
    /// Never any value. `None` = an older node.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notify_credentials_sync: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notify_credentials_generation: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notify_openid_rebind: Option<bool>,
    /// This build replicates the shared follow list on the openid route.
    /// Heartbeats carry only the flag, never the channel list. `None` means
    /// an older node: it must not be sent the list.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notify_watch_sync: Option<bool>,
    /// The B站 live book (`history::live_book`, analytics design §17): which
    /// `live_key` the shared room is in. Peer heartbeat only; stripped from
    /// `/api/cluster/status`. Older nodes neither send nor read it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bili_live: Option<crate::history::live_book::BiliLiveBook>,
    /// Recent exempt warning-hold restarts this node recorded; peer heartbeat
    /// only, stripped from `/api/cluster/status`. Older nodes ignore it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warning_exempt_cooldown: Option<crate::plugins::warning_exempt::ExemptCooldown>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum YtIndexNodeState {
    /// Answers YouTube for the cluster.
    Index,
    /// Answers from `node`'s index copy.
    Follows { node: String },
    /// Answers for itself. `reason`: `no_key` / `budget_spent` (the
    /// public-status node cannot serve), or `index_down`.
    Local { reason: String },
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct WebSubCounts {
    pub verified: usize,
    pub pending: usize,
    pub failed: usize,
}

impl ClusterNodeSnapshot {
    pub(crate) fn is_serviceable(&self) -> bool {
        self.health.healthy && !self.draining && !self.network_unstable
    }

    /// Same condition as the public page's 转播中 badge: ffmpeg with RTMP TX.
    pub(crate) fn is_restreaming(&self) -> bool {
        if !self.ffmpeg_running {
            return false;
        }
        let Some(network) = self.network.as_ref() else {
            return false;
        };
        network.stream_bitrate_kbps.is_some_and(|kbps| kbps > 0.0)
            || network.stream_speed.is_some_and(|speed| speed > 0.0)
    }
}

impl ClusterStatus {
    /// An active owner is on the wire. Idle active is 活跃, not 转播中.
    pub(crate) fn owner_is_restreaming(&self) -> bool {
        self.nodes
            .iter()
            .any(|node| node.role == ClusterNodeRole::Active && node.is_restreaming())
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum ClusterNodeRole {
    /// Selected owner; may be idle rather than publishing.
    Active,
    /// Healthy and eligible, but not selected.
    Standby,
    /// Operator-requested maintenance takes display precedence over faults.
    Draining,
    /// Detected/latched fault, or missing/stale heartbeat.
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
    /// Allows the source to acknowledge demotion after its election has
    /// already accepted this replacement as owner.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handoff_target_node_id: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MonitoredConfig {
    pub interval: u64,
    pub auto_cover: bool,
    #[serde(default = "default_true")]
    pub auto_stop_after_source_end: bool,
    #[serde(default = "crate::config::default_auto_stop_threshold_hours")]
    pub auto_stop_threshold_hours: u64,
    pub enable_anti_collision: bool,
    pub anti_collision_list: HashMap<String, i32>,
    #[serde(default = "default_true")]
    pub enable_danmaku_command: bool,
    pub enable_youtube_monitor: bool,
    pub enable_twitch_monitor: bool,
    pub youtube: Youtube,
    pub twitch: Twitch,
    pub priority_channel: PriorityChannel,
    #[serde(default)]
    pub niconico_enable_monitor: bool,
    #[serde(default)]
    pub niconico_channel_name: String,
    #[serde(default)]
    pub niconico_channel_id: String,
    #[serde(default)]
    pub niconico_live_id: String,
    #[serde(default)]
    pub niconico_area_v2: u64,
    pub channels_json: Option<serde_json::Value>,
    pub areas_json: Option<serde_json::Value>,
    /// Warning-hold exemption list and allowed reason ids (PRD 10-20-warn-hold-exempt).
    /// Joins the integrity hash, so older peers reject a push carrying them.
    #[serde(default)]
    pub warning_hold_exempt: Vec<crate::plugins::warning_exempt::ExemptChannel>,
    #[serde(default)]
    pub warning_hold_exempt_reasons: Vec<String>,
    /// Low-risk reasons apply to every channel (mid/high still need the list).
    #[serde(default)]
    pub warning_hold_exempt_all_low_risk: bool,
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
    #[serde(default)]
    pub niconico_enable_monitor: bool,
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

/// The owner's YouTube index, served to peers at `GET /api/cluster/yt-index`.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct YtIndexPayload {
    pub version: String,
    /// `videos.list` answers by video ID. `None` means YouTube omitted the ID
    /// (private, members-only, deleted).
    pub videos: BTreeMap<String, Option<YtVideo>>,
    /// Discovered and recently live rows the owner merges into Holodex's.
    pub discovered: Vec<StreamItem>,
}

impl YtIndexPayload {
    /// The owner's answers for `ids`. IDs without one are left out, so the
    /// overlay keeps Holodex's values for them.
    pub(crate) fn answers<'a>(
        &self,
        ids: impl IntoIterator<Item = &'a String>,
    ) -> HashMap<String, YtVideo> {
        ids.into_iter()
            .filter_map(|id| Some((id.clone(), self.videos.get(id)?.clone()?)))
            .collect()
    }
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
        niconico_enable_monitor: cfg.niconico.enable_monitor,
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
        niconico_enable_monitor: payload.niconico_enable_monitor,
    }
}

pub fn all_monitor_toggles_off() -> MonitorToggleState {
    MonitorToggleState::default()
}

/// Demotion persists all-off. The dashboard reads `/api/config`, so the
/// previous owner's switches must actually be cleared, not only masked in
/// `/api/status`.
pub(crate) fn resolved_node_mode_monitor_toggles(
    active: bool,
    requested: Option<MonitorToggleState>,
) -> Option<MonitorToggleState> {
    match requested {
        _ if !active => Some(all_monitor_toggles_off()),
        Some(toggles) => Some(toggles),
        None => None,
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

pub fn apply_monitor_toggle_state_to_config(cfg: &mut Config, payload: &MonitorToggleState) {
    cfg.bililive.enable_danmaku_command = payload.enable_danmaku_command;
    cfg.enable_youtube_monitor = payload.enable_youtube_monitor;
    cfg.enable_twitch_monitor = payload.enable_twitch_monitor;
    cfg.youtube.enable_monitor = payload.youtube_enable_monitor;
    cfg.twitch.enable_monitor = payload.twitch_enable_monitor;
    cfg.priority_channel.enabled = payload.priority_channel_enabled;
    cfg.priority_channel.auto_restart = payload.priority_channel_auto_restart;
    cfg.niconico.enable_monitor = payload.niconico_enable_monitor;
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
