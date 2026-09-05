//! Process-wide cluster state, HTTP client, and atomic JSON helpers.

use super::types::*;
use crate::config::Config;
use lazy_static::lazy_static;
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::{RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

lazy_static! {
    pub(crate) static ref CLUSTER_HTTP_CLIENT: reqwest::Client = build_cluster_http_client();
}

pub(crate) fn build_cluster_http_client() -> reqwest::Client {
    let mut builder = reqwest::Client::builder();
    if let Some(session) = crate::webui::session_cookie() {
        let mut headers = reqwest::header::HeaderMap::new();
        if let Ok(value) =
            reqwest::header::HeaderValue::from_str(&format!("bilistream_session={session}"))
        {
            headers.insert(reqwest::header::COOKIE, value);
            builder = builder.default_headers(headers);
        }
    }
    builder.build().unwrap_or_else(|_| reqwest::Client::new())
}

pub(crate) fn cluster_http_client() -> reqwest::Client {
    CLUSTER_HTTP_CLIENT.clone()
}

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
    pub(crate) peer_owner_views: HashMap<String, PeerOwnerView>,
    /// A newly elected local owner cannot execute until handoff completes.
    pub(crate) pending_handoff_source: Option<String>,
    /// Demotion fences execution before any await, even if persistence fails.
    pub(crate) local_execution_held: bool,
    pub(crate) peer_observations: HashMap<String, HashMap<String, u64>>,
    pub(crate) last_known_active_toggles: Option<MonitorToggleState>,
    pub(crate) last_known_active_channel_targets: Option<ChannelTargetState>,
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

pub(crate) fn executable_sibling(name: &str) -> Option<PathBuf> {
    std::env::current_exe()
        .ok()
        .map(|path| path.with_file_name(name))
}

pub(crate) fn read_json_file(name: &str) -> Option<serde_json::Value> {
    let path = executable_sibling(name)?;
    let content = fs::read_to_string(path).ok()?;
    serde_json::from_str(&content).ok()
}
