use super::*;

pub(crate) static ACTIVE_MONITOR_SYNC_GENERATION: AtomicU64 = AtomicU64::new(0);

const MONITOR_TOGGLE_SYNC_DEBOUNCE_MS: u64 = 150;

lazy_static::lazy_static! {
    static ref ACTIVE_MONITOR_SYNC_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::new(());
}

pub(crate) fn schedule_active_monitor_state_sync_after_toggle_change(cfg: &Config) -> String {
    if !cfg.cluster.enabled {
        return String::new();
    }

    let generation = next_active_monitor_sync_generation();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(MONITOR_TOGGLE_SYNC_DEBOUNCE_MS)).await;
        if !active_monitor_sync_generation_is_current(generation) {
            tracing::debug!(
                "Skipped stale cluster monitor toggle sync generation {}",
                generation
            );
            return;
        }

        let _guard = ACTIVE_MONITOR_SYNC_LOCK.lock().await;
        if !active_monitor_sync_generation_is_current(generation) {
            tracing::debug!(
                "Skipped superseded cluster monitor toggle sync generation {}",
                generation
            );
            return;
        }

        let cfg = match load_config().await {
            Ok(cfg) => cfg,
            Err(e) => {
                tracing::warn!("Cluster monitor toggle background sync skipped: {}", e);
                return;
            }
        };

        match push_active_monitor_state_to_peers(&cfg).await {
            Ok(count) if count > 0 => {
                tracing::info!("Cluster monitor toggles synced to {} peer nodes", count);
            }
            Ok(_) => {}
            Err(e) => {
                tracing::warn!("Cluster monitor toggle background sync failed: {}", e);
            }
        }
    });

    "；监控开关同步已在后台执行".to_string()
}

pub(crate) fn next_active_monitor_sync_generation() -> u64 {
    ACTIVE_MONITOR_SYNC_GENERATION.fetch_add(1, Ordering::AcqRel) + 1
}

pub(crate) fn active_monitor_sync_generation_is_current(generation: u64) -> bool {
    ACTIVE_MONITOR_SYNC_GENERATION.load(Ordering::Acquire) == generation
}

pub(crate) fn local_node_can_enable_monitor_toggles(cfg: &Config) -> bool {
    if !cfg.cluster.enabled {
        return true;
    }
    crate::cluster::local_node_is_active_owner(cfg)
}

pub(crate) fn monitor_toggle_enable_rejected_response() -> ApiResponse<()> {
    ApiResponse {
        success: false,
        data: None,
        message: Some("只有活跃节点可启用监控开关".to_string()),
    }
}

pub async fn get_cluster_status() -> Json<ApiResponse<ClusterStatus>> {
    match load_cluster_status().await {
        Ok(status) => Json(ApiResponse {
            success: true,
            data: Some(status),
            message: None,
        }),
        Err(e) => Json(ApiResponse {
            success: false,
            data: None,
            message: Some(e),
        }),
    }
}

pub async fn cluster_heartbeat(
    Json(payload): Json<ClusterHeartbeatRequest>,
) -> Result<Json<ApiResponse<ClusterStatus>>, StatusCode> {
    let cfg = load_config()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    if !crate::cluster::record_heartbeat(&cfg, payload.node) {
        return Ok(Json(ApiResponse {
            success: false,
            data: None,
            message: Some("集群心跳节点未被本机成员配置接受".to_string()),
        }));
    }
    let status = crate::cluster::get_cluster_status_for_config(&cfg).await;

    Ok(Json(ApiResponse {
        success: true,
        data: Some(status),
        message: None,
    }))
}

pub async fn cluster_self_check(
    Json(payload): Json<crate::cluster::SelfCheckRequest>,
) -> Result<impl IntoResponse, StatusCode> {
    let cfg = load_config()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let reply =
        crate::cluster::self_check_reply(&cfg.cluster, payload).ok_or(StatusCode::BAD_REQUEST)?;
    Ok((
        [(axum::http::header::CACHE_CONTROL, "no-store")],
        Json(ApiResponse {
            success: true,
            data: Some(reply),
            message: None,
        }),
    ))
}

pub async fn cluster_drain(
    Json(payload): Json<ClusterDrainRequest>,
) -> Result<Json<ApiResponse<ClusterStatus>>, StatusCode> {
    let cfg = load_config()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    cluster_drain_for_config(&cfg, payload).await.map(Json)
}

pub(crate) async fn cluster_drain_for_config(
    cfg: &Config,
    payload: ClusterDrainRequest,
) -> Result<ApiResponse<ClusterStatus>, StatusCode> {
    let should_propagate = payload.propagate.unwrap_or(true);
    // Resolve None once at the entry node. Forwarding None would toggle each
    // recipient's own maintenance state instead of the requested node.
    let target_node_id = payload
        .node_id
        .as_deref()
        .unwrap_or(&cfg.cluster.node_id)
        .trim()
        .to_string();
    let target_peer = cfg
        .cluster
        .peers
        .iter()
        .find(|peer| peer.node_id == target_node_id);
    if target_node_id.is_empty() || (target_node_id != cfg.cluster.node_id && target_peer.is_none())
    {
        return Err(StatusCode::BAD_REQUEST);
    }
    let active_drain_plan = if should_propagate && cfg.cluster.enabled && payload.draining {
        let before = crate::cluster::get_cluster_status_for_config(cfg).await;
        if before.active_owner.as_deref() == Some(target_node_id.as_str()) {
            crate::cluster::replacement_owner_for_drain(cfg, &target_node_id)
                .map(|target| (before, target_node_id.clone(), target))
        } else {
            None
        }
    } else {
        None
    };

    if let Some((before, source_node_id, target_node_id)) = active_drain_plan {
        if let Err(e) =
            finalize_cluster_node_switch(cfg, &before, &source_node_id, &target_node_id, true).await
        {
            return Ok(ApiResponse {
                success: false,
                data: Some(crate::cluster::get_cluster_status_for_config(cfg).await),
                message: Some(format!("集群节点禁用前切换失败: {}", e)),
            });
        }
    }

    let forwarded = ClusterDrainRequest {
        node_id: Some(target_node_id.clone()),
        draining: payload.draining,
        propagate: Some(false),
    };
    let mut message = "集群节点状态已更新".to_string();
    if should_propagate && cfg.cluster.enabled && target_node_id != cfg.cluster.node_id {
        // Only the target can reset its fault counters. Wait for its direct,
        // identity-checked reply before changing our view of that node.
        if let Err(error) =
            post_cluster_control(cfg, "/api/cluster/drain", &forwarded, Some(&target_node_id)).await
        {
            return Ok(ApiResponse {
                success: false,
                data: Some(crate::cluster::get_cluster_status_for_config(cfg).await),
                message: Some(format!("目标节点未确认状态更新: {error}")),
            });
        }
    } else {
        crate::cluster::set_drain_state(cfg, Some(target_node_id), payload.draining);
        // Recollect local health after resetting counters, before publishing
        // or returning a snapshot. Cached stream_degraded may describe the
        // fault that was just cleared; a still-detected fault must stay faulted.
        if should_propagate && cfg.cluster.enabled {
            crate::cluster::get_cluster_status_for_config(cfg).await;
            if let Err(error) =
                post_cluster_control(cfg, "/api/cluster/drain", &forwarded, None).await
            {
                message = format!("本节点状态已更新；部分节点同步失败: {error}");
            }
        }
    }

    let status = crate::cluster::get_cluster_status_for_config(cfg).await;
    crate::webui::state::request_status_refresh();
    Ok(ApiResponse {
        success: true,
        data: Some(status),
        message: Some(message),
    })
}

pub async fn cluster_set_auto_failover(
    Json(payload): Json<crate::cluster::ClusterAutoFailoverRequest>,
) -> Result<Json<ApiResponse<ClusterStatus>>, StatusCode> {
    let mut cfg = load_config()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let should_propagate = payload.propagate.unwrap_or(true);
    let old_cluster = cfg.cluster.clone();
    cfg.cluster.auto_failover = payload.enabled;
    crate::config::save_config(&mut cfg)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let status = crate::cluster::get_cluster_status_for_config(&cfg).await;

    if should_propagate && cfg.cluster.enabled {
        if let Err(e) = propagate_cluster_membership(&old_cluster, &cfg.cluster).await {
            tracing::warn!("Cluster auto-failover sync failed: {}", e);
        }
    }

    Ok(Json(ApiResponse {
        success: true,
        data: Some(status),
        message: Some(if payload.enabled {
            "已启用自动故障转移".to_string()
        } else {
            "已关闭自动故障转移，仅支持手动切换".to_string()
        }),
    }))
}

/// Registered under the `/api` nest in webui::server, and used to build the
/// peer url below. Kept as one constant so the two cannot drift: an earlier
/// hand-written path silently produced a malformed url and the sync never ran.
pub const APPLY_PUBLIC_STATUS_ROUTE: &str = "/cluster/apply-public-status";

fn apply_public_status_url(api_url: &str) -> String {
    format!(
        "{}/api{}",
        api_url.trim_end_matches('/'),
        APPLY_PUBLIC_STATUS_ROUTE
    )
}

/// Pushes the settings to every peer, reporting how many took them.
///
/// This endpoint acknowledges settings without the node snapshot required by
/// post_cluster_control.
async fn push_public_status_to_peers(
    cfg: &Config,
    public_status: &crate::config::PublicStatusConfig,
) -> Result<usize, String> {
    let client = crate::cluster::cluster_http_client();
    let timeout = Duration::from_secs(cfg.cluster.heartbeat_interval_secs.max(5));

    let tasks = cfg
        .cluster
        .peers
        .iter()
        .filter(|peer| peer.node_id != cfg.cluster.node_id)
        .map(|peer| {
            let client = client.clone();
            let url = apply_public_status_url(&peer.api_url);
            async move {
                let response = client
                    .post(url)
                    .json(public_status)
                    .timeout(timeout)
                    .send()
                    .await
                    .map_err(|e| format!("{}: {}", peer.node_id, e))?;

                if !response.status().is_success() {
                    return Err(format!("{}: HTTP {}", peer.node_id, response.status()));
                }
                Ok(())
            }
        });

    let mut synced = 0usize;
    let mut errors = Vec::new();
    for result in join_all(tasks).await {
        match result {
            Ok(()) => synced += 1,
            Err(error) => errors.push(error),
        }
    }

    if errors.is_empty() {
        Ok(synced)
    } else {
        Err(errors.join("; "))
    }
}

/// Applies the public status page settings pushed by another node.
pub async fn cluster_apply_public_status(
    Json(payload): Json<crate::config::PublicStatusConfig>,
) -> Result<Json<ApiResponse<()>>, StatusCode> {
    if let Err(message) = payload.validate() {
        return Ok(Json(ApiResponse {
            success: false,
            data: None,
            message: Some(message),
        }));
    }

    let mut cfg = load_config()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    cfg.cluster.public_status = payload;
    crate::config::save_config(&mut cfg)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(Json(ApiResponse {
        success: true,
        data: None,
        message: Some("公开状态页配置已同步".to_string()),
    }))
}

/// Picks which node serves the public status page, and on what port. Every
/// node keeps the same copy so the panel shows one answer wherever it is open.
pub async fn cluster_set_public_status(
    Json(payload): Json<crate::cluster::ClusterPublicStatusRequest>,
) -> Result<Json<ApiResponse<ClusterStatus>>, StatusCode> {
    if let Err(message) = payload.config.validate() {
        return Ok(Json(ApiResponse {
            success: false,
            data: None,
            message: Some(message),
        }));
    }

    let mut cfg = load_config()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let unchanged = cfg.cluster.public_status == payload.config;
    cfg.cluster.public_status = payload.config.clone();
    crate::config::save_config(&mut cfg)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let mut sync_note = String::new();
    if payload.propagate.unwrap_or(true) && cfg.cluster.enabled && !unchanged {
        match push_public_status_to_peers(&cfg, &payload.config).await {
            Ok(0) => {}
            Ok(synced) => sync_note = format!("，已同步到 {} 个节点", synced),
            // Saved locally but the peers disagree now, which the operator
            // needs to know rather than read a plain success.
            Err(e) => {
                let status = crate::cluster::get_cluster_status_for_config(&cfg).await;
                return Ok(Json(ApiResponse {
                    success: false,
                    data: Some(status),
                    message: Some(format!("本节点已保存，但同步失败: {}", e)),
                }));
            }
        }
    }

    let status = crate::cluster::get_cluster_status_for_config(&cfg).await;
    let message = match cfg.cluster.public_status.node_id.trim() {
        "" => format!("已关闭公开状态页{}", sync_note),
        node => format!("公开状态页由 {} 提供{}", node, sync_note),
    };

    Ok(Json(ApiResponse {
        success: true,
        data: Some(status),
        message: Some(message),
    }))
}

pub async fn cluster_export_config(
) -> Result<Json<ApiResponse<ClusterSyncConfigRequest>>, StatusCode> {
    let cfg = load_config()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(Json(ApiResponse {
        success: true,
        data: Some(cluster_sync_config_from_config(&cfg)),
        message: None,
    }))
}

pub async fn cluster_apply_node_mode(
    Json(payload): Json<ClusterApplyNodeModeRequest>,
) -> Result<Json<ApiResponse<ClusterStatus>>, StatusCode> {
    match apply_cluster_node_mode_locally(payload).await {
        Ok(status) => Ok(Json(ApiResponse {
            success: true,
            data: Some(status),
            message: Some("集群节点模式已更新".to_string()),
        })),
        Err(e) => Ok(Json(ApiResponse {
            success: false,
            data: None,
            message: Some(e),
        })),
    }
}

pub async fn cluster_sync_membership(
    Json(payload): Json<ClusterMembershipRequest>,
) -> Result<Json<ApiResponse<()>>, StatusCode> {
    match apply_cluster_membership_locally(&payload).await {
        Ok(()) => Ok(Json(ApiResponse {
            success: true,
            data: None,
            message: Some("集群节点配置已同步".to_string()),
        })),
        Err(e) => Ok(Json(ApiResponse {
            success: false,
            data: None,
            message: Some(e),
        })),
    }
}

pub async fn cluster_failover(
    Json(payload): Json<ClusterFailoverRequest>,
) -> Result<Json<ApiResponse<ClusterStatus>>, StatusCode> {
    let cfg = load_config()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let should_propagate = payload.propagate.unwrap_or(true);
    let target_node_id = match normalize_cluster_failover_target(&cfg, payload.target_node_id) {
        Ok(target_node_id) => target_node_id,
        Err(message) => {
            return Ok(Json(ApiResponse {
                success: false,
                data: None,
                message: Some(message),
            }));
        }
    };
    let transfer_plan = if should_propagate && cfg.cluster.enabled {
        if let Some(target) = target_node_id.as_deref() {
            let before = load_cluster_status().await.ok();
            before.and_then(|status| cluster_transfer_plan(status, target))
        } else {
            None
        }
    } else {
        None
    };

    let performed_transfer = transfer_plan.is_some();
    let mut status = if let Some((before, source_node_id, target)) = transfer_plan {
        if let Err(e) =
            finalize_cluster_node_switch(&cfg, &before, &source_node_id, &target, false).await
        {
            return Ok(Json(ApiResponse {
                success: false,
                data: load_cluster_status().await.ok(),
                message: Some(format!("集群节点切换失败: {}", e)),
            }));
        }
        crate::cluster::get_cluster_status_for_config(&cfg).await
    } else {
        crate::cluster::force_failover(&cfg, target_node_id.clone())
    };

    if let Some(target) = target_node_id.as_deref() {
        if status.active_owner.as_deref() != Some(target) {
            return Ok(Json(ApiResponse {
                success: false,
                data: Some(status),
                message: Some(format!("目标节点 {} 当前不可接管", target)),
            }));
        }
    }

    if !performed_transfer && status.active_owner.as_deref() != Some(cfg.cluster.node_id.as_str()) {
        crate::plugins::set_manual_restart();
        crate::cluster::clear_local_stream();
        crate::plugins::stop_ffmpeg().await;
        status = crate::cluster::get_cluster_status_for_config(&cfg).await;
    }

    let mut message = "集群节点切换已触发".to_string();
    if should_propagate && cfg.cluster.enabled {
        let forwarded = ClusterFailoverRequest {
            target_node_id: target_node_id.clone(),
            propagate: Some(false),
        };
        if let Err(error) =
            post_cluster_control(&cfg, "/api/cluster/failover", &forwarded, None).await
        {
            message = format!("集群节点切换已触发；部分节点同步失败: {error}");
        }
        status = crate::cluster::get_cluster_status_for_config(&cfg).await;
    }

    Ok(Json(ApiResponse {
        success: true,
        data: Some(status),
        message: Some(message),
    }))
}

pub(crate) fn cluster_transfer_plan(
    status: ClusterStatus,
    target_node_id: &str,
) -> Option<(ClusterStatus, String, String)> {
    let source_node_id = status.active_owner.clone()?;
    Some((status, source_node_id, target_node_id.to_string()))
}

pub(crate) fn normalize_cluster_failover_target(
    cfg: &Config,
    target_node_id: Option<String>,
) -> Result<Option<String>, String> {
    let Some(target_node_id) = target_node_id else {
        return Ok(None);
    };
    let target_node_id = target_node_id.trim();
    if target_node_id.is_empty() {
        return Err("目标节点不能为空".to_string());
    }
    if target_node_id == cfg.cluster.node_id
        || cfg
            .cluster
            .peers
            .iter()
            .any(|peer| peer.node_id == target_node_id)
    {
        Ok(Some(target_node_id.to_string()))
    } else {
        Err(format!("未知集群节点: {}", target_node_id))
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ClusterRestartNodeRequest {
    pub node_id: String,
}

pub async fn cluster_restart_node(
    Json(payload): Json<ClusterRestartNodeRequest>,
) -> Result<Json<ApiResponse<()>>, StatusCode> {
    let cfg = load_config()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let target = payload.node_id.trim();
    if target.is_empty() {
        return Ok(Json(ApiResponse {
            success: false,
            data: None,
            message: Some("节点 ID 不能为空".to_string()),
        }));
    }

    if target == cfg.cluster.node_id {
        return restart_server_process().await.map(Json);
    }

    let Some(peer) = cfg.cluster.peers.iter().find(|peer| peer.node_id == target) else {
        return Ok(Json(ApiResponse {
            success: false,
            data: None,
            message: Some(format!("未找到节点 {}", target)),
        }));
    };

    match restart_peer_server(peer).await {
        Ok(message) => Ok(Json(ApiResponse {
            success: true,
            data: None,
            message: Some(message),
        })),
        Err(e) => Ok(Json(ApiResponse {
            success: false,
            data: None,
            message: Some(e),
        })),
    }
}

pub(crate) async fn restart_peer_server(peer: &ClusterPeer) -> Result<String, String> {
    let url = format!("{}/api/server/restart", peer.api_url.trim_end_matches('/'));
    let response = crate::cluster::cluster_http_client()
        .post(url)
        .timeout(Duration::from_secs(5))
        .send()
        .await
        .map_err(|e| format!("节点 {} 重启请求失败: {}", peer.node_id, e))?;

    if !response.status().is_success() {
        return Err(format!("节点 {} HTTP {}", peer.node_id, response.status()));
    }

    let envelope = response
        .json::<ClusterPeerApiResponse<()>>()
        .await
        .map_err(|e| format!("节点 {} 响应解析失败: {}", peer.node_id, e))?;
    if envelope.success {
        Ok(envelope
            .message
            .unwrap_or_else(|| format!("节点 {} 重启命令已发送", peer.node_id)))
    } else {
        Err(envelope
            .message
            .unwrap_or_else(|| format!("节点 {} 重启失败", peer.node_id)))
    }
}

#[derive(Deserialize)]
pub(crate) struct ClusterPeerApiResponse<T> {
    success: bool,
    data: Option<T>,
    message: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ClusterMembershipNode {
    pub node_id: String,
    #[serde(default)]
    pub name: String,
    pub api_url: String,
    #[serde(default)]
    pub priority: i32,
}

pub(crate) fn default_membership_auto_failover() -> bool {
    true
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ClusterMembershipRequest {
    #[serde(default)]
    pub target_node_id: Option<String>,
    pub enabled: bool,
    pub sync_monitored_channels: bool,
    #[serde(default = "default_membership_auto_failover")]
    pub auto_failover: bool,
    pub heartbeat_interval_secs: u64,
    pub failover_timeout_secs: u64,
    pub lease_ttl_secs: u64,
    pub thresholds: ClusterHealthThresholds,
    pub nodes: Vec<ClusterMembershipNode>,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct NormalizedMembershipNode<'a> {
    node_id: &'a str,
    name: &'a str,
    api_url: &'a str,
    priority: i32,
}

#[derive(Serialize)]
pub(crate) struct ClusterMembershipTargetRequest<'a> {
    #[serde(default)]
    target_node_id: Option<&'a str>,
    enabled: bool,
    sync_monitored_channels: bool,
    #[serde(default = "default_membership_auto_failover")]
    auto_failover: bool,
    heartbeat_interval_secs: u64,
    failover_timeout_secs: u64,
    lease_ttl_secs: u64,
    thresholds: &'a ClusterHealthThresholds,
    nodes: &'a [ClusterMembershipNode],
}

pub async fn cluster_sync_config(
    Json(payload): Json<ClusterSyncConfigRequest>,
) -> Result<Json<ApiResponse<ClusterStatus>>, StatusCode> {
    if monitored_config_version_from_request(&payload)
        .is_some_and(|actual| actual != payload.config_version)
    {
        return Ok(Json(ApiResponse {
            success: false,
            data: None,
            message: Some("同步配置版本校验失败".to_string()),
        }));
    }

    apply_monitored_config(payload.monitored_config)
        .await
        .map_err(|e| {
            tracing::error!("Cluster config sync failed: {}", e);
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    let status = load_cluster_status()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(Json(ApiResponse {
        success: true,
        data: Some(status),
        message: Some("监控频道配置已同步".to_string()),
    }))
}

pub async fn cluster_cache_active_monitor_state(
    Json(payload): Json<ClusterActiveMonitorStateRequest>,
) -> Result<Json<ApiResponse<ClusterStatus>>, StatusCode> {
    let cfg = load_config()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let status = crate::cluster::get_cluster_status_for_config(&cfg).await;
    if status.active_owner.as_deref() != Some(payload.sender_node_id.as_str()) {
        return Ok(Json(ApiResponse {
            success: false,
            data: Some(status),
            message: Some("监控状态缓存请求不是来自当前活跃节点".to_string()),
        }));
    }
    cache_active_monitor_state_from_peer(payload.monitor_toggles, payload.channel_targets);

    Ok(Json(ApiResponse {
        success: true,
        data: Some(status),
        message: Some("活跃节点监控开关已缓存".to_string()),
    }))
}

pub async fn cluster_push_config() -> Result<Json<ApiResponse<ClusterStatus>>, StatusCode> {
    let cfg = load_config()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let synced = match push_monitored_config_to_peers(&cfg).await {
        Ok(synced) => synced,
        Err(e) => {
            return Ok(Json(ApiResponse {
                success: false,
                data: None,
                message: Some(format!("监控频道配置同步失败: {}", e)),
            }));
        }
    };

    let status = load_cluster_status()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(Json(ApiResponse {
        success: true,
        data: Some(status),
        message: Some(format!("监控频道配置已同步到 {} 个节点", synced)),
    }))
}

pub(crate) fn monitored_config_version_from_request(
    payload: &ClusterSyncConfigRequest,
) -> Option<String> {
    Some(monitored_config_integrity_version_from_payload(
        &payload.monitored_config,
    ))
}

pub(crate) async fn post_cluster_control<T: Serialize>(
    cfg: &Config,
    path: &str,
    payload: &T,
    target_node_id: Option<&str>,
) -> Result<usize, String> {
    let client = crate::cluster::cluster_http_client();
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
        .map(|peer| post_cluster_control_to_peer(&client, cfg, peer, path, payload, timeout));
    let results = join_all(tasks).await;
    if target_node_id.is_some() && results.is_empty() {
        return Err("目标节点不在成员配置中".to_string());
    }
    let count = results.iter().filter(|result| result.is_ok()).count();
    let errors: Vec<_> = results.into_iter().filter_map(Result::err).collect();
    if errors.is_empty() {
        Ok(count)
    } else {
        Err(errors.join("；"))
    }
}

pub(crate) async fn post_cluster_control_to_peer<T: Serialize>(
    client: &reqwest::Client,
    cfg: &Config,
    peer: &ClusterPeer,
    path: &str,
    payload: &T,
    timeout: Duration,
) -> Result<(), String> {
    let url = format!("{}{}", peer.api_url.trim_end_matches('/'), path);
    let response = client
        .post(url)
        .json(payload)
        .timeout(timeout)
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .map_err(|error| format!("{}: {error}", peer.node_id))?;
    let envelope = crate::plugins::http::response_json_limited::<
        ClusterPeerApiResponse<ClusterStatus>,
    >(response)
    .await
    .map_err(|error| format!("{}: {error}", peer.node_id))?;
    if !envelope.success {
        return Err(format!(
            "{}: {}",
            peer.node_id,
            envelope.message.as_deref().unwrap_or("节点拒绝操作")
        ));
    }
    let status = envelope
        .data
        .ok_or_else(|| format!("{}: 缺少确认状态", peer.node_id))?;
    crate::cluster::merge_cluster_status_from_direct_peer(status, &peer.node_id, cfg)
}

pub(crate) fn cluster_membership_from_config(cluster: &ClusterConfig) -> ClusterMembershipRequest {
    let mut nodes = vec![ClusterMembershipNode {
        node_id: cluster.node_id.clone(),
        name: cluster.node_name.clone(),
        api_url: cluster.public_api_url.clone(),
        priority: cluster.priority,
    }];

    nodes.extend(cluster.peers.iter().map(|peer| ClusterMembershipNode {
        node_id: peer.node_id.clone(),
        name: peer.name.clone(),
        api_url: peer.api_url.clone(),
        priority: peer.priority,
    }));
    normalize_cluster_membership_nodes(&mut nodes);

    ClusterMembershipRequest {
        enabled: cluster.enabled,
        sync_monitored_channels: cluster.sync_monitored_channels,
        auto_failover: cluster.auto_failover,
        heartbeat_interval_secs: cluster.heartbeat_interval_secs,
        failover_timeout_secs: cluster.failover_timeout_secs,
        lease_ttl_secs: cluster.lease_ttl_secs,
        thresholds: cluster.thresholds.clone(),
        nodes,
        target_node_id: None,
    }
}

pub(crate) fn normalize_cluster_membership_nodes(nodes: &mut Vec<ClusterMembershipNode>) {
    let mut seen = HashSet::new();
    nodes.retain(|node| {
        let node_id = node.node_id.trim();
        let api_url = normalized_cluster_api_url(&node.api_url);
        if node_id.is_empty() || api_url.is_empty() || seen.contains(node_id) {
            return false;
        }
        seen.insert(node_id.to_string());
        true
    });

    for node in nodes {
        node.node_id = node.node_id.trim().to_string();
        node.name = node.name.trim().to_string();
        node.api_url = normalized_cluster_api_url(&node.api_url).to_string();
    }
}

pub(crate) async fn apply_cluster_membership_locally(
    payload: &ClusterMembershipRequest,
) -> Result<(), String> {
    let mut cfg = load_config().await.map_err(|e| e.to_string())?;
    apply_cluster_membership_to_config(&mut cfg.cluster, payload);

    crate::config::save_config(&mut cfg)
        .await
        .map_err(|e| e.to_string())?;

    if !cfg.cluster.enabled {
        crate::plugins::set_manual_restart();
        crate::cluster::clear_local_stream();
        crate::plugins::stop_ffmpeg().await;
    }

    Ok(())
}

pub(crate) fn apply_cluster_membership_to_config(
    cluster: &mut ClusterConfig,
    payload: &ClusterMembershipRequest,
) {
    let nodes = normalized_membership_nodes(&payload.nodes);
    let local_node_id = payload
        .target_node_id
        .as_deref()
        .filter(|node_id| !node_id.trim().is_empty())
        .unwrap_or(cluster.node_id.as_str())
        .trim()
        .to_string();
    let local_node = nodes.iter().find(|node| node.node_id == local_node_id);

    if let Some(local_node) = local_node {
        cluster.enabled = payload.enabled;
        cluster.node_id = local_node.node_id.to_string();
        cluster.node_name = if local_node.name.is_empty() {
            local_node.node_id.to_string()
        } else {
            local_node.name.to_string()
        };
        cluster.public_api_url = local_node.api_url.to_string();
        cluster.priority = local_node.priority;
        cluster.peers = nodes
            .iter()
            .filter(|node| node.node_id != local_node_id)
            .map(|node| ClusterPeer {
                node_id: node.node_id.to_string(),
                name: node.name.to_string(),
                api_url: node.api_url.to_string(),
                priority: node.priority,
            })
            .collect();
    } else {
        cluster.enabled = false;
        cluster.peers.clear();
    }

    cluster.sync_monitored_channels = payload.sync_monitored_channels;
    cluster.auto_failover = payload.auto_failover;
    cluster.heartbeat_interval_secs = payload.heartbeat_interval_secs.max(1);
    cluster.failover_timeout_secs = payload.failover_timeout_secs.max(1);
    cluster.lease_ttl_secs = payload.lease_ttl_secs.max(1);
    cluster.thresholds = payload.thresholds.clone();
}

pub(crate) fn normalized_membership_nodes(
    nodes: &[ClusterMembershipNode],
) -> Vec<NormalizedMembershipNode<'_>> {
    let mut seen = HashSet::new();
    let mut normalized = Vec::with_capacity(nodes.len());

    for node in nodes {
        let node_id = node.node_id.trim();
        let api_url = normalized_cluster_api_url(&node.api_url);
        if node_id.is_empty() || api_url.is_empty() || !seen.insert(node_id) {
            continue;
        }

        normalized.push(NormalizedMembershipNode {
            node_id,
            name: node.name.trim(),
            api_url,
            priority: node.priority,
        });
    }

    normalized
}

pub(crate) async fn propagate_cluster_membership(
    old_cluster: &ClusterConfig,
    new_cluster: &ClusterConfig,
) -> Result<usize, String> {
    let request = cluster_membership_from_config(new_cluster);
    let targets = cluster_membership_propagation_targets(old_cluster, new_cluster, &request);
    let client = crate::cluster::cluster_http_client();
    let timeout = Duration::from_secs(new_cluster.heartbeat_interval_secs.max(5));

    let tasks = targets.into_iter().map(|(node_id, api_url)| {
        push_cluster_membership_to_target(&client, &request, node_id, api_url, timeout)
    });
    let results = join_all(tasks).await;
    let mut synced = 0usize;
    let mut errors = Vec::new();

    for result in results {
        match result {
            Ok(()) => synced += 1,
            Err(error) => errors.push(error),
        }
    }

    if errors.is_empty() {
        Ok(synced)
    } else {
        Err(errors.join("; "))
    }
}

pub(crate) fn cluster_membership_propagation_targets(
    old_cluster: &ClusterConfig,
    new_cluster: &ClusterConfig,
    request: &ClusterMembershipRequest,
) -> HashMap<String, String> {
    let mut targets = HashMap::new();

    for node in &request.nodes {
        insert_membership_target(
            &mut targets,
            &new_cluster.node_id,
            &node.node_id,
            &node.api_url,
        );
    }

    for peer in &old_cluster.peers {
        insert_membership_target(
            &mut targets,
            &new_cluster.node_id,
            &peer.node_id,
            &peer.api_url,
        );
    }

    targets
}

pub(crate) fn insert_membership_target(
    targets: &mut HashMap<String, String>,
    local_node_id: &str,
    node_id: &str,
    api_url: &str,
) {
    let api_url = normalized_cluster_api_url(api_url);
    if node_id != local_node_id && !api_url.is_empty() {
        targets.insert(node_id.to_string(), api_url.to_string());
    }
}

pub(crate) fn normalized_cluster_api_url(api_url: &str) -> &str {
    api_url.trim().trim_end_matches('/')
}

pub(crate) async fn push_cluster_membership_to_target(
    client: &reqwest::Client,
    request: &ClusterMembershipRequest,
    node_id: String,
    api_url: String,
    timeout: Duration,
) -> Result<(), String> {
    let url = format!("{}/api/cluster/sync-membership", api_url);
    let targeted_request = cluster_membership_target_request(request, &node_id);
    let response = client
        .post(url)
        .json(&targeted_request)
        .timeout(timeout)
        .send()
        .await
        .map_err(|e| format!("{} {}", node_id, e))?;

    let status = response.status();
    if !status.is_success() {
        return Err(format!("{} HTTP {}", node_id, status));
    }

    match response.json::<ClusterPeerApiResponse<()>>().await {
        Ok(envelope) if envelope.success => Ok(()),
        Ok(envelope) => Err(format!(
            "{} {}",
            node_id,
            envelope.message.unwrap_or_else(|| "同步被拒绝".to_string())
        )),
        Err(e) => Err(format!("{} 响应解析失败: {}", node_id, e)),
    }
}

pub(crate) fn cluster_membership_target_request<'a>(
    request: &'a ClusterMembershipRequest,
    target_node_id: &'a str,
) -> ClusterMembershipTargetRequest<'a> {
    ClusterMembershipTargetRequest {
        target_node_id: Some(target_node_id),
        enabled: request.enabled,
        sync_monitored_channels: request.sync_monitored_channels,
        auto_failover: request.auto_failover,
        heartbeat_interval_secs: request.heartbeat_interval_secs,
        failover_timeout_secs: request.failover_timeout_secs,
        lease_ttl_secs: request.lease_ttl_secs,
        thresholds: &request.thresholds,
        nodes: &request.nodes,
    }
}

#[cfg(test)]
mod public_status_tests {
    use super::*;

    /// The url the panel posts to must match the route the peer registers.
    /// A hand-written path without the /api prefix produced
    /// "http://ny:3150cluster/apply-public-status", which failed silently and
    /// left every peer on stale settings.
    #[test]
    fn the_peer_url_matches_the_registered_route() {
        assert_eq!(
            apply_public_status_url("http://ny.example.com:3150"),
            format!("http://ny.example.com:3150/api{APPLY_PUBLIC_STATUS_ROUTE}")
        );
        assert!(APPLY_PUBLIC_STATUS_ROUTE.starts_with('/'));
    }

    #[test]
    fn a_trailing_slash_on_the_peer_url_does_not_double_up() {
        assert_eq!(
            apply_public_status_url("http://ny.example.com:3150/"),
            "http://ny.example.com:3150/api/cluster/apply-public-status"
        );
    }
}
