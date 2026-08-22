//! Process-wide cluster state, HTTP client, and atomic JSON helpers.

use super::types::*;
use crate::config::Config;
use lazy_static::lazy_static;
use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
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

pub(crate) static JSON_TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

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
    pub(crate) peer_observations: HashMap<String, HashMap<String, u64>>,
    pub(crate) last_known_active_toggles: Option<MonitorToggleState>,
    pub(crate) last_known_active_channel_targets: Option<ChannelTargetState>,
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

pub(crate) fn write_json_file(name: &str, value: &serde_json::Value) -> Result<(), String> {
    let path =
        executable_sibling(name).ok_or_else(|| "failed to resolve executable path".to_string())?;
    let json = serde_json::to_string_pretty(value).map_err(|e| e.to_string())?;
    write_json_file_atomic(&path, json.as_bytes()).map_err(|e| e.to_string())
}

pub(crate) fn write_json_file_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let (tmp_path, mut tmp_file) = create_unique_json_tmp_file(path)?;
    let write_result = tmp_file.write_all(bytes).and_then(|_| tmp_file.sync_all());
    drop(tmp_file);

    let result = write_result.and_then(|_| fs::rename(&tmp_path, path));

    if result.is_err() {
        let _ = fs::remove_file(&tmp_path);
    }
    result
}

pub(crate) fn create_unique_json_tmp_file(path: &Path) -> std::io::Result<(PathBuf, fs::File)> {
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

pub(crate) fn unique_json_tmp_path(path: &Path) -> PathBuf {
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
