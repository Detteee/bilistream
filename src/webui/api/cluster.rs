use super::*;
use crate::cluster::peer_call::AuthenticatedNode;
use axum::Extension;

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

pub(crate) async fn cluster_heartbeat(
    Extension(peer): Extension<AuthenticatedNode>,
    Json(payload): Json<ClusterHeartbeatRequest>,
) -> Result<Json<ApiResponse<ClusterStatus>>, StatusCode> {
    let cfg = load_config()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    if payload.node.node_id != peer.node_id || !crate::cluster::record_heartbeat(&cfg, payload.node)
    {
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
    cfg.cluster.auto_failover = payload.enabled;
    crate::config::save_config(&mut cfg)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let status = crate::cluster::get_cluster_status_for_config(&cfg).await;

    if should_propagate && cfg.cluster.enabled {
        if let Err(e) = crate::cluster::push_cluster_settings_to_peers(&cfg).await {
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

/// Pushes the settings to every peer, reporting how many took them.
///
/// This endpoint acknowledges settings without the node snapshot required by
/// post_cluster_control.
async fn push_public_status_to_peers(
    cfg: &Config,
    public_status: &crate::config::PublicStatusConfig,
) -> Result<usize, String> {
    let timeout = Duration::from_secs(cfg.cluster.heartbeat_interval_secs.max(5));

    let tasks = cfg
        .cluster
        .peers
        .iter()
        .filter(|peer| peer.node_id != cfg.cluster.node_id)
        .map(|peer| async move {
            let response = crate::cluster::peer_call::send_ordinary(
                cfg,
                &peer.node_id,
                crate::cluster::peer_call::routes::APPLY_PUBLIC_STATUS,
                serde_json::to_vec(public_status).map_err(|e| e.to_string())?,
                timeout,
            )
            .await
            .map_err(|e| format!("{}: {}", peer.node_id, e))?;
            if !(200..300).contains(&response.status) {
                return Err(format!("{}: HTTP {}", peer.node_id, response.status));
            }
            Ok(())
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
    // The public node is a committed membership role, never a peer edit.
    if cfg.cluster.public_status.node_id != payload.node_id {
        return Ok(Json(ApiResponse {
            success: false,
            data: None,
            message: Some(crate::cluster::TOPOLOGY_EDIT_REJECTED.to_string()),
        }));
    }
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
    if cfg.cluster.public_status.node_id != payload.config.node_id
        && crate::cluster::membership::current_lifecycle()
            != crate::cluster::membership::Lifecycle::Standalone
    {
        return Ok(Json(ApiResponse {
            success: false,
            data: None,
            message: Some(crate::cluster::TOPOLOGY_EDIT_REJECTED.to_string()),
        }));
    }
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

/// Ordinary non-topology settings from a current member.
pub async fn cluster_apply_settings(
    Json(payload): Json<crate::cluster::ClusterSettings>,
) -> Result<Json<ApiResponse<()>>, StatusCode> {
    let mut cfg = load_config()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    if let Err(message) = payload.apply(&mut cfg.cluster) {
        return Ok(Json(ApiResponse {
            success: false,
            data: None,
            message: Some(message),
        }));
    }
    crate::config::save_config(&mut cfg)
        .await
        .map_err(config_save_status)?;
    Ok(Json(ApiResponse {
        success: true,
        data: None,
        message: Some("集群设置已同步".to_string()),
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

/// The owner's YouTube index for peers (`cluster::yt_index`).
pub async fn cluster_yt_index() -> Json<ApiResponse<crate::cluster::YtIndexPayload>> {
    match crate::cluster::owner_yt_index() {
        Some(index) => Json(ApiResponse {
            success: true,
            data: Some((*index).clone()),
            message: None,
        }),
        None => Json(ApiResponse {
            success: false,
            data: None,
            message: Some("本节点不负责集群的 YouTube 索引".to_string()),
        }),
    }
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

    match restart_peer_server(&cfg, peer).await {
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

pub(crate) async fn restart_peer_server(
    cfg: &Config,
    peer: &ClusterPeer,
) -> Result<String, String> {
    let envelope: crate::cluster::PeerApiResponse<()> = crate::cluster::peer_call::call::<(), _>(
        cfg,
        &peer.node_id,
        crate::cluster::peer_call::routes::RESTART,
        None,
        Duration::from_secs(5),
    )
    .await
    .map_err(|e| format!("节点 {} 重启请求失败: {}", peer.node_id, e))?;
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

pub(crate) async fn cluster_cache_active_monitor_state(
    Extension(peer): Extension<AuthenticatedNode>,
    Json(payload): Json<ClusterActiveMonitorStateRequest>,
) -> Result<Json<ApiResponse<ClusterStatus>>, StatusCode> {
    let cfg = load_config()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let status = crate::cluster::get_cluster_status_for_config(&cfg).await;
    if payload.sender_node_id != peer.node_id
        || status.active_owner.as_deref() != Some(payload.sender_node_id.as_str())
    {
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
    let route = match path {
        "/api/cluster/drain" => crate::cluster::peer_call::routes::DRAIN,
        "/api/cluster/failover" => crate::cluster::peer_call::routes::FAILOVER,
        _ => return Err("不支持的节点控制请求".to_string()),
    };
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
        .map(|peer| post_cluster_control_to_peer(cfg, peer, route, payload, timeout));
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
    cfg: &Config,
    peer: &ClusterPeer,
    route: crate::cluster::peer_auth::RoutePolicy,
    payload: &T,
    timeout: Duration,
) -> Result<(), String> {
    let envelope: crate::cluster::PeerApiResponse<ClusterStatus> =
        crate::cluster::peer_call::call(cfg, &peer.node_id, route, Some(payload), timeout)
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

#[cfg(test)]
mod yt_index_tests {
    use super::*;

    /// A peer that asks a node which is not the index owner must get a
    /// refusal, not an empty index that reads as "nothing is live".
    #[tokio::test]
    async fn only_the_index_owner_serves_the_youtube_index() {
        let reply = cluster_yt_index().await.0;
        assert!(!reply.success);
        assert!(reply.data.is_none());
    }
}
