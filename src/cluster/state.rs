//! Process-wide cluster state and timing helpers.

use super::types::*;
use crate::config::Config;
use std::collections::HashMap;
use std::sync::{RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub(crate) fn cluster_control_timeout(cfg: &Config) -> Duration {
    Duration::from_secs(cfg.cluster.heartbeat_interval_secs.clamp(5, 15))
}

pub(crate) fn cluster_heartbeat_timeout(cfg: &Config) -> Duration {
    Duration::from_secs(cfg.cluster.heartbeat_interval_secs.clamp(3, 10))
}

pub(crate) fn recover_read_lock<'a, T>(lock: &'a RwLock<T>, name: &str) -> RwLockReadGuard<'a, T> {
    lock.read().unwrap_or_else(|poisoned| {
        tracing::warn!("Recovering poisoned {name} read lock");
        poisoned.into_inner()
    })
}

pub(crate) fn recover_write_lock<'a, T>(
    lock: &'a RwLock<T>,
    name: &str,
) -> RwLockWriteGuard<'a, T> {
    lock.write().unwrap_or_else(|poisoned| {
        tracing::warn!("Recovering poisoned {name} write lock");
        poisoned.into_inner()
    })
}

pub(crate) fn cluster_state_read() -> RwLockReadGuard<'static, ClusterState> {
    recover_read_lock(crate::AppState::process_cluster(), "cluster state")
}

pub(crate) fn cluster_state_write() -> RwLockWriteGuard<'static, ClusterState> {
    recover_write_lock(crate::AppState::process_cluster(), "cluster state")
}

pub(crate) fn cluster_switch_lock() -> &'static tokio::sync::Mutex<()> {
    crate::AppState::process_cluster_switch_lock()
}

pub(crate) fn node_mode_apply_lock() -> &'static tokio::sync::Mutex<()> {
    crate::AppState::process_node_mode_apply_lock()
}

#[derive(Clone, Debug, Default)]
pub(crate) struct ClusterState {
    pub(crate) local_self_check: Option<super::self_check::SelfCheckRecord>,
    pub(crate) nodes: HashMap<String, ClusterNodeSnapshot>,
    pub(crate) local_draining: bool,
    pub(crate) local_fault_latched: bool,
    pub(crate) local_fault_reason: Option<String>,
    pub(crate) forced_owner: Option<String>,
    pub(crate) active_owner: Option<String>,
    pub(crate) lease_until: u64,
    pub(crate) local_stream: Option<ClusterStreamIdentity>,
    pub(crate) local_failed_restarts: u32,
    pub(crate) local_failed_restart_times: Vec<u64>,
    pub(crate) local_external_api_failures: u32,
    pub(crate) local_external_api_failure_times: Vec<u64>,
    pub(crate) heartbeat_failures: HashMap<String, u32>,
    pub(crate) peer_heartbeat_acks: HashMap<String, u64>,
    /// Monotonic observations back the bounded execution lease advertised by
    /// upgraded nodes. Wall-clock adjustments must never extend that lease.
    pub(crate) peer_heartbeat_observed: HashMap<String, Instant>,
    pub(crate) execution_fencing_active: bool,
    pub(crate) peer_owner_views: HashMap<String, PeerOwnerView>,
    /// A newly elected local owner cannot execute until handoff completes.
    pub(crate) pending_handoff_source: Option<String>,
    /// Demotion fences execution before any await, even if persistence fails.
    pub(crate) local_execution_held: bool,
    pub(crate) peer_observations: HashMap<String, HashMap<String, u64>>,
    pub(crate) last_known_active_toggles: Option<MonitorToggleState>,
    pub(crate) last_known_active_channel_targets: Option<ChannelTargetState>,
    /// A source taken over without its confirmation (it was unreachable):
    /// the all-off demotion is still owed to it.
    pub(crate) unconfirmed_demotion: Option<String>,
}

#[derive(Clone, Debug)]
pub(crate) struct PeerOwnerView {
    pub(crate) owner: Option<String>,
    pub(crate) received_at: u64,
    pub(crate) members: std::collections::HashSet<String>,
    pub(crate) confirmed_by_heartbeat: bool,
}

pub(crate) fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

pub(crate) fn read_json_file(name: &str) -> Option<serde_json::Value> {
    crate::storage::read_json(name).ok()
}
