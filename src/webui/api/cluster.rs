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

pub(crate) async fn local_node_can_enable_monitor_toggles(cfg: &Config) -> bool {
    if !cfg.cluster.enabled {
        return true;
    }
    load_cluster_status()
        .await
        .map(|status| status.active_owner.as_deref() == Some(cfg.cluster.node_id.as_str()))
        .unwrap_or(false)
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
    crate::cluster::record_heartbeat(&cfg, payload.node);
    let status = crate::cluster::get_cluster_status_for_config(&cfg).await;

    Ok(Json(ApiResponse {
        success: true,
        data: Some(status),
        message: None,
    }))
}

pub async fn cluster_drain(
    Json(payload): Json<ClusterDrainRequest>,
) -> Result<Json<ApiResponse<ClusterStatus>>, StatusCode> {
    let cfg = load_config()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let should_propagate = payload.propagate.unwrap_or(true);
    let target_node_id = payload.node_id.clone();
    let status =
        crate::cluster::set_drain_state(&cfg, payload.node_id, payload.draining, payload.ddos);

    if should_propagate && cfg.cluster.enabled {
        let propagation_target = target_node_id
            .as_deref()
            .filter(|target| *target != cfg.cluster.node_id);
        let forwarded = ClusterDrainRequest {
            node_id: target_node_id.clone(),
            draining: payload.draining,
            ddos: payload.ddos,
            propagate: Some(false),
        };
        post_cluster_control(&cfg, "/api/cluster/drain", &forwarded, propagation_target).await;
    }

    Ok(Json(ApiResponse {
        success: true,
        data: Some(status),
        message: Some("集群节点状态已更新".to_string()),
    }))
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
    crate::config::save_config(&cfg)
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
            before.map(|status| {
                let source_node_id = status
                    .active_owner
                    .clone()
                    .unwrap_or_else(|| cfg.cluster.node_id.clone());
                (status, source_node_id, target.to_string())
            })
        } else {
            None
        }
    } else {
        None
    };

    let status = crate::cluster::force_failover(&cfg, target_node_id.clone());

    if status.active_owner.as_deref() != Some(cfg.cluster.node_id.as_str()) {
        crate::plugins::set_manual_restart();
        crate::plugins::stop_ffmpeg().await;
    }

    if should_propagate && cfg.cluster.enabled {
        let forwarded = ClusterFailoverRequest {
            target_node_id: target_node_id.clone(),
            propagate: Some(false),
        };
        post_cluster_control(&cfg, "/api/cluster/failover", &forwarded, None).await;
    }

    if let Some((before, source_node_id, target)) = transfer_plan {
        if let Err(e) = finalize_cluster_node_switch(&cfg, &before, &source_node_id, &target).await
        {
            return Ok(Json(ApiResponse {
                success: false,
                data: Some(status),
                message: Some(format!("集群节点切换后状态同步失败: {}", e)),
            }));
        }
    }

    Ok(Json(ApiResponse {
        success: true,
        data: Some(status),
        message: Some("集群节点切换已触发".to_string()),
    }))
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
    let response = reqwest::Client::new()
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

pub(crate) async fn finalize_cluster_node_switch(
    cfg: &Config,
    before: &ClusterStatus,
    source_node_id: &str,
    target_node_id: &str,
) -> Result<(), String> {
    let client = reqwest::Client::new();
    let source_config = match export_cluster_config_from_node(&client, cfg, source_node_id).await {
        Ok(source_config) => source_config,
        Err(e) => {
            tracing::warn!(
                "Failed to export active source config from {}; using local synced config and cached active state: {}",
                source_node_id,
                e
            );
            cluster_sync_config_from_config(cfg)
        }
    };
    let source_toggles = resolve_source_monitor_toggles(
        cfg,
        before,
        source_node_id,
        &source_config.monitored_config,
    );
    let source_channel_targets = resolve_source_channel_targets(
        cfg,
        before,
        source_node_id,
        &source_config.monitored_config,
    );

    apply_cluster_node_mode_to_node_with_retry(
        &client,
        cfg,
        target_node_id,
        ClusterApplyNodeModeRequest {
            monitored_config: Some(source_config.monitored_config),
            active: true,
            restart: false,
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
    post_cluster_control(cfg, "/api/cluster/drain", &target_enable, None).await;

    if source_node_id != target_node_id {
        let source_standby = ClusterDrainRequest {
            node_id: Some(source_node_id.to_string()),
            draining: false,
            ddos: false,
            propagate: Some(false),
        };
        post_cluster_control(cfg, "/api/cluster/drain", &source_standby, None).await;

        if let Err(e) = apply_cluster_node_mode_to_node_with_retry(
            &client,
            cfg,
            source_node_id,
            ClusterApplyNodeModeRequest {
                monitored_config: None,
                active: false,
                restart: false,
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
        .json::<ClusterPeerApiResponse<ClusterSyncConfigRequest>>()
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

pub(crate) async fn apply_cluster_node_mode_to_node_with_retry(
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

pub(crate) async fn apply_cluster_node_mode_to_node(
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
        .json::<ClusterPeerApiResponse<ClusterStatus>>()
        .await
        .map_err(|e| format!("解析节点 {} 模式响应失败: {}", node_id, e))?;

    if envelope.success {
        if let Some(status) = envelope.data {
            crate::cluster::merge_cluster_status_from_direct_peer(status, node_id, cfg)?;
        }
        Ok(())
    } else {
        Err(envelope
            .message
            .unwrap_or_else(|| format!("节点 {} 拒绝模式更新", node_id)))
    }
}

pub(crate) async fn apply_cluster_node_mode_locally(
    payload: ClusterApplyNodeModeRequest,
) -> Result<ClusterStatus, String> {
    let has_monitored_config = payload.monitored_config.is_some();
    let active = payload.active;
    let restart = payload.restart;
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

    crate::config::save_config(&cfg)
        .await
        .map_err(|e| e.to_string())?;

    // Promoting to active clears drain/ddos locks. Demoting to standby only
    // clears draining so the node stays eligible; disabled state is /drain only.
    if active {
        crate::cluster::set_drain_state(&cfg, None, false, false);
    } else {
        let ddos = crate::cluster::local_ddos_state();
        crate::cluster::set_local_drain_state_preserving_fault(&cfg, false, ddos);
    }

    apply_danmaku_command_runtime_state(cfg.bililive.enable_danmaku_command);
    set_config_updated();
    refresh_status_cache_config_from(&cfg);

    if active {
        if let Some(monitor_toggles) = monitor_toggles.as_ref() {
            let cache_targets =
                channel_targets.unwrap_or_else(|| channel_target_state_from_config(&cfg));
            crate::cluster::cache_active_monitor_state_from_owner(
                monitor_toggles,
                Some(&cache_targets),
            );
        }
    }

    if !active || restart {
        crate::plugins::set_manual_restart();
        crate::plugins::stop_ffmpeg().await;
    }

    load_cluster_status().await
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
    cache_active_monitor_state_from_peer(payload.monitor_toggles, payload.channel_targets);

    let status = load_cluster_status()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

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
        .map(|peer| post_cluster_control_to_peer(&client, cfg, peer, path, payload, timeout));
    join_all(tasks).await;
}

pub(crate) async fn post_cluster_control_to_peer<T: Serialize>(
    client: &reqwest::Client,
    cfg: &Config,
    peer: &ClusterPeer,
    path: &str,
    payload: &T,
    timeout: Duration,
) {
    let url = format!("{}{}", peer.api_url.trim_end_matches('/'), path);
    match client.post(url).json(payload).timeout(timeout).send().await {
        Ok(response) if !response.status().is_success() => {
            tracing::warn!(
                "Cluster control propagation failed for {}: HTTP {}",
                peer.node_id,
                response.status()
            );
        }
        Ok(response) => match response
            .json::<ClusterPeerApiResponse<ClusterStatus>>()
            .await
        {
            Ok(envelope) if envelope.success => {
                if let Some(status) = envelope.data {
                    if let Err(e) = crate::cluster::merge_cluster_status_from_direct_peer(
                        status,
                        &peer.node_id,
                        cfg,
                    ) {
                        tracing::warn!(
                            "Cluster control response from {} ignored: {}",
                            peer.node_id,
                            e
                        );
                    }
                }
            }
            Ok(envelope) => {
                tracing::warn!(
                    "Cluster control rejected by {}: {:?}",
                    peer.node_id,
                    envelope.message
                );
            }
            Err(e) => {
                tracing::warn!(
                    "Cluster control response parse failed for {}: {}",
                    peer.node_id,
                    e
                );
            }
        },
        Err(e) => {
            tracing::warn!(
                "Cluster control propagation failed for {}: {}",
                peer.node_id,
                e
            );
        }
    }
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

    crate::config::save_config(&cfg)
        .await
        .map_err(|e| e.to_string())?;

    if !cfg.cluster.enabled {
        crate::plugins::set_manual_restart();
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
    let client = reqwest::Client::new();
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
