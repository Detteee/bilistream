use super::*;

pub async fn get_config() -> Result<Json<serde_json::Value>, StatusCode> {
    let cfg = load_config()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let config_json = json!({
        "interval": cfg.interval,
        "auto_cover": cfg.auto_cover,
        "enable_anti_collision": cfg.enable_anti_collision,
        "enable_lol_monitor": cfg.enable_lol_monitor,
        "lol_monitor_interval": cfg.lol_monitor_interval,
        "riot_api_key": cfg.riot_api_key.clone().unwrap_or_default(),
        "holodex_api_key": cfg.holodex_api_key.clone().unwrap_or_default(),
        "holodex_jwt_configured": cfg
            .holodex_jwt
            .as_ref()
            .is_some_and(|j| !j.is_empty()),
        "holodex_skip_jwt_verify": cfg.holodex_skip_jwt_verify,
        "holodex_monitor_gate": cfg.holodex_monitor_gate,
        "youtube_api_key": cfg.youtube_api_key.clone().unwrap_or_default(),
        "youtube_websub_callback_url": cfg.youtube_websub_callback_url.clone().unwrap_or_default(),
        "youtube_websub_port": cfg.youtube_websub_port,
        "anti_collision_list": cfg.anti_collision_list.clone(),
        "enable_youtube_monitor": cfg.enable_youtube_monitor,
        "enable_twitch_monitor": cfg.enable_twitch_monitor,
        "cluster": cfg.cluster.clone(),
        "bilibili": {
            "room": cfg.bililive.room,
            "enable_danmaku_command": cfg.bililive.enable_danmaku_command,
        },
        "youtube": {
            "enable_monitor": cfg.youtube.enable_monitor,
            "channel_name": cfg.youtube.channel_name,
            "channel_id": cfg.youtube.channel_id,
            "area_v2": cfg.youtube.area_v2,
            "proxy": cfg.youtube.proxy,
            "cookies_file": cfg.youtube.cookies_file,
            "cookies_from_browser": cfg.youtube.cookies_from_browser,
            "deno_path": cfg.youtube.deno_path,
            "ffmpeg_cache": {
                "enabled": cfg.youtube.ffmpeg_cache.enabled,
                "latency_secs": cfg.youtube.ffmpeg_cache.latency_secs,
            },
        },
        "twitch": {
            "enable_monitor": cfg.twitch.enable_monitor,
            "channel_name": cfg.twitch.channel_name,
            "channel_id": cfg.twitch.channel_id,
            "area_v2": cfg.twitch.area_v2,
            "proxy_region": cfg.twitch.proxy_region,
            "proxy": cfg.twitch.proxy,
            "ffmpeg_cache": {
                "enabled": cfg.twitch.ffmpeg_cache.enabled,
                "latency_secs": cfg.twitch.ffmpeg_cache.latency_secs,
            },
        },
        "priority_channel": {
            "enabled": cfg.priority_channel.enabled,
            "channel_name": cfg.priority_channel.channel_name,
            "youtube_channel_id": cfg.priority_channel.youtube_channel_id,
            "twitch_channel_id": cfg.priority_channel.twitch_channel_id,
            "default_area": cfg.priority_channel.default_area,
            "auto_restart": cfg.priority_channel.auto_restart,
        }
    });

    Ok(Json(config_json))
}

#[derive(Deserialize, Serialize)]
pub struct UpdateConfigRequest {
    #[serde(skip_serializing)]
    expected: Option<HashMap<String, serde_json::Value>>,
    interval: Option<u64>,
    auto_cover: Option<bool>,
    enable_anti_collision: Option<bool>,
    enable_lol_monitor: Option<bool>,
    lol_monitor_interval: Option<u64>,
    riot_api_key: Option<String>,
    holodex_api_key: Option<String>,
    holodex_jwt: Option<String>,
    holodex_skip_jwt_verify: Option<bool>,
    holodex_monitor_gate: Option<bool>,
    youtube_api_key: Option<String>,
    youtube_websub_callback_url: Option<String>,
    youtube_websub_port: Option<u16>,
    twitch_proxy_region: Option<String>,
    twitch_proxy: Option<String>,
    youtube_proxy: Option<String>,
    youtube_deno_path: Option<String>,
    anti_collision_list: Option<HashMap<String, i32>>,
    enable_danmaku_command: Option<bool>,
    youtube_enable_monitor: Option<bool>,
    twitch_enable_monitor: Option<bool>,
    youtube_cookies_from_browser: Option<String>,
    youtube_cookies_file: Option<String>,
    cluster: Option<ClusterConfig>,
}

pub(crate) const EDIT_CONFLICT: &str = "配置已被其他操作修改，请重新加载后再保存";
pub(crate) const EDIT_INVALID: &str = "配置修改缺少有效的原始值";

/// Check the values the browser actually edited, before applying its patch.
/// save_config then detects changes racing this request under its write lock.
pub(crate) fn validate_edit_preconditions(
    current: &serde_json::Value,
    patch: &serde_json::Value,
    expected: Option<&HashMap<String, serde_json::Value>>,
) -> Result<(), &'static str> {
    let Some(expected) = expected else {
        return Ok(()); // Compatible with existing API clients.
    };
    for (key, value) in patch.as_object().ok_or(EDIT_INVALID)? {
        if value.is_null() {
            continue;
        }
        let before = expected.get(key).ok_or(EDIT_INVALID)?;
        let now = current.get(key).ok_or(EDIT_INVALID)?;
        if before != now {
            return Err(EDIT_CONFLICT);
        }
    }
    Ok(())
}

fn config_form_values(cfg: &Config) -> serde_json::Value {
    json!({
        "interval": cfg.interval,
        "auto_cover": cfg.auto_cover,
        "enable_anti_collision": cfg.enable_anti_collision,
        "enable_danmaku_command": cfg.bililive.enable_danmaku_command,
        "holodex_monitor_gate": cfg.holodex_monitor_gate,
        "enable_lol_monitor": cfg.enable_lol_monitor,
        "lol_monitor_interval": cfg.lol_monitor_interval.unwrap_or(1),
        "riot_api_key": cfg.riot_api_key.as_deref().unwrap_or_default().trim(),
        "holodex_api_key": cfg.holodex_api_key.as_deref().unwrap_or_default().trim(),
        "youtube_api_key": cfg.youtube_api_key.as_deref().unwrap_or_default().trim(),
        "youtube_websub_callback_url": cfg.youtube_websub_callback_url.as_deref().unwrap_or_default().trim(),
        "youtube_websub_port": cfg.youtube_websub_port,
        "anti_collision_list": cfg.anti_collision_list,
        "youtube_proxy": cfg.youtube.proxy.as_deref().unwrap_or_default().trim(),
        "twitch_proxy": cfg.twitch.proxy.as_deref().unwrap_or_default().trim(),
        "twitch_proxy_region": cfg.twitch.proxy_region,
        "youtube_cookies_from_browser": cfg.youtube.cookies_from_browser.as_deref().unwrap_or_default().trim(),
        "youtube_cookies_file": cfg.youtube.cookies_file.as_deref().unwrap_or_default().trim(),
        "youtube_deno_path": cfg.youtube.deno_path.as_deref().unwrap_or_default().trim(),
    })
}

/// The callback URL must be empty or `http(s)`, and the callback port must be
/// a real port other than the WebUI's.
pub(crate) fn validate_websub(url: Option<&str>, port: Option<u16>) -> Result<(), String> {
    if let Some(url) = url.map(str::trim).filter(|url| !url.is_empty()) {
        let parsed = reqwest::Url::parse(url).map_err(|_| "WebSub 回调地址无效".to_string())?;
        if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
            return Err("WebSub 回调地址必须是 http(s) 地址".to_string());
        }
    }
    if let Some(port) = port {
        if port == 0 {
            return Err("WebSub 回调端口无效".to_string());
        }
        if Some(port) == crate::plugins::youtube_websub::webui_port() {
            return Err("WebSub 回调端口不能与 Web UI 端口相同".to_string());
        }
    }
    Ok(())
}

pub(crate) fn monitor_target_reload_needed(
    previous_enabled: bool,
    current_enabled: bool,
    previous_channel_name: &str,
    current_channel_name: &str,
    previous_channel_id: &str,
    current_channel_id: &str,
) -> bool {
    previous_enabled != current_enabled
        || (current_enabled
            && (previous_channel_name != current_channel_name
                || previous_channel_id != current_channel_id))
}

pub(crate) fn youtube_monitor_reload_needed(previous: &Config, current: &Config) -> bool {
    monitor_target_reload_needed(
        previous.youtube.enable_monitor,
        current.youtube.enable_monitor,
        &previous.youtube.channel_name,
        &current.youtube.channel_name,
        &previous.youtube.channel_id,
        &current.youtube.channel_id,
    )
}

pub(crate) fn twitch_monitor_reload_needed(previous: &Config, current: &Config) -> bool {
    monitor_target_reload_needed(
        previous.twitch.enable_monitor,
        current.twitch.enable_monitor,
        &previous.twitch.channel_name,
        &current.twitch.channel_name,
        &previous.twitch.channel_id,
        &current.twitch.channel_id,
    )
}

pub(crate) fn monitor_reload_needed(previous: &Config, current: &Config) -> bool {
    youtube_monitor_reload_needed(previous, current)
        || twitch_monitor_reload_needed(previous, current)
}

pub async fn update_config(
    Json(payload): Json<UpdateConfigRequest>,
) -> Result<ApiResponse<()>, StatusCode> {
    // Load current config
    let mut cfg = load_config()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let previous_cfg = cfg.clone();
    let old_cluster = cfg.cluster.clone();
    let old_monitored_config_version = monitored_config_version(&cfg);
    let cluster_changed = payload.cluster.is_some();
    let danmaku_command_changed = payload.enable_danmaku_command;
    let requests_monitor_toggle_enable = payload.enable_danmaku_command == Some(true)
        || payload.youtube_enable_monitor == Some(true)
        || payload.twitch_enable_monitor == Some(true);

    if requests_monitor_toggle_enable && !local_node_can_enable_monitor_toggles(&cfg) {
        return Ok(monitor_toggle_enable_rejected_response());
    }

    validate_edit_preconditions(
        &config_form_values(&cfg),
        &serde_json::to_value(&payload).map_err(|_| StatusCode::BAD_REQUEST)?,
        payload.expected.as_ref(),
    )
    .map_err(|error| {
        if error == EDIT_CONFLICT {
            StatusCode::CONFLICT
        } else {
            StatusCode::BAD_REQUEST
        }
    })?;

    if let Err(message) = validate_websub(
        payload.youtube_websub_callback_url.as_deref(),
        payload.youtube_websub_port,
    ) {
        return Ok(ApiResponse {
            success: false,
            data: None,
            message: Some(message),
        });
    }

    let mut holodex_jwt_saved = false;

    // Update fields
    if let Some(interval) = payload.interval {
        cfg.interval = interval;
    }
    if let Some(auto_cover) = payload.auto_cover {
        cfg.auto_cover = auto_cover;
    }
    if let Some(enable_anti_collision) = payload.enable_anti_collision {
        cfg.enable_anti_collision = enable_anti_collision;
    }
    if let Some(enable_lol_monitor) = payload.enable_lol_monitor {
        cfg.enable_lol_monitor = enable_lol_monitor;
    }
    if let Some(lol_monitor_interval) = payload.lol_monitor_interval {
        cfg.lol_monitor_interval = Some(lol_monitor_interval);
    }
    if let Some(riot_api_key) = payload.riot_api_key {
        if !riot_api_key.is_empty() {
            cfg.riot_api_key = Some(riot_api_key);
        } else {
            cfg.riot_api_key = None;
        }
    }
    if let Some(holodex_api_key) = payload.holodex_api_key {
        if !holodex_api_key.is_empty() {
            cfg.holodex_api_key = Some(holodex_api_key);
        } else {
            cfg.holodex_api_key = None;
        }
    }
    if let Some(youtube_api_key) = payload.youtube_api_key {
        let key = youtube_api_key.trim();
        cfg.youtube_api_key = (!key.is_empty()).then(|| key.to_string());
    }
    if let Some(url) = payload.youtube_websub_callback_url {
        let url = url.trim();
        cfg.youtube_websub_callback_url = (!url.is_empty()).then(|| url.to_string());
    }
    if let Some(port) = payload.youtube_websub_port {
        cfg.youtube_websub_port = port;
    }
    if let Some(holodex_jwt) = payload.holodex_jwt {
        let jwt = holodex_jwt.trim();
        let jwt = jwt
            .strip_prefix("BEARER ")
            .or_else(|| jwt.strip_prefix("bearer "))
            .unwrap_or(jwt)
            .trim();
        if jwt.is_empty() {
            cfg.holodex_jwt = None;
            cfg.holodex_jwt_refreshed_at = None;
            cfg.holodex_username = None;
        } else {
            cfg.holodex_jwt = Some(jwt.to_string());
            cfg.holodex_username = None;
            holodex_jwt_saved = true;
        }
    }
    if let Some(holodex_skip_jwt_verify) = payload.holodex_skip_jwt_verify {
        cfg.holodex_skip_jwt_verify = holodex_skip_jwt_verify;
    }
    let holodex_monitor_gate_changed = payload
        .holodex_monitor_gate
        .is_some_and(|gate| gate != cfg.holodex_monitor_gate);
    if let Some(holodex_monitor_gate) = payload.holodex_monitor_gate {
        cfg.holodex_monitor_gate = holodex_monitor_gate;
    }

    if let Some(anti_collision_list) = payload.anti_collision_list {
        cfg.anti_collision_list = anti_collision_list;
    }
    if let Some(twitch_proxy_region) = payload.twitch_proxy_region {
        cfg.twitch.proxy_region = twitch_proxy_region;
    }
    if let Some(twitch_proxy) = payload.twitch_proxy {
        cfg.twitch.proxy = if twitch_proxy.is_empty() {
            None
        } else {
            Some(twitch_proxy)
        };
    }
    if let Some(youtube_proxy) = payload.youtube_proxy {
        cfg.youtube.proxy = if youtube_proxy.is_empty() {
            None
        } else {
            Some(youtube_proxy)
        };
    }
    if let Some(enable_danmaku_command) = payload.enable_danmaku_command {
        cfg.bililive.enable_danmaku_command = enable_danmaku_command;
    }
    if let Some(youtube_enable_monitor) = payload.youtube_enable_monitor {
        cfg.youtube.enable_monitor = youtube_enable_monitor;
        cfg.enable_youtube_monitor = youtube_enable_monitor;
    }
    if let Some(twitch_enable_monitor) = payload.twitch_enable_monitor {
        cfg.twitch.enable_monitor = twitch_enable_monitor;
        cfg.enable_twitch_monitor = twitch_enable_monitor;
    }
    if let Some(youtube_cookies_from_browser) = payload.youtube_cookies_from_browser {
        cfg.youtube.cookies_from_browser = if youtube_cookies_from_browser.is_empty() {
            None
        } else {
            Some(youtube_cookies_from_browser)
        };
    }
    if let Some(youtube_cookies_file) = payload.youtube_cookies_file {
        cfg.youtube.cookies_file = if youtube_cookies_file.is_empty() {
            None
        } else {
            Some(youtube_cookies_file)
        };
    }
    if let Some(youtube_deno_path) = payload.youtube_deno_path {
        cfg.youtube.deno_path = if youtube_deno_path.is_empty() {
            None
        } else {
            Some(youtube_deno_path)
        };
    }
    if let Some(cluster) = payload.cluster {
        cfg.cluster = cluster;
    }

    // Save config
    crate::config::save_config(&mut cfg)
        .await
        .map_err(config_save_status)?;

    if let Some(enabled) = danmaku_command_changed {
        crate::cluster::apply_danmaku_command_runtime_state(enabled);
    }

    if holodex_jwt_saved {
        if let Some(jwt) = cfg.holodex_jwt.clone() {
            ensure_holodex_username(&mut cfg, &jwt).await;
        }
    }

    if monitor_reload_needed(&previous_cfg, &cfg) {
        set_config_updated();
    }

    if holodex_monitor_gate_changed {
        crate::webui::state::request_status_refresh();
    }

    // Apply the exact saved config to the cache without re-reading config.json.
    refresh_status_cache_config_from(&cfg);

    let monitor_toggle_changed = danmaku_command_changed.is_some()
        || payload.youtube_enable_monitor.is_some()
        || payload.twitch_enable_monitor.is_some();
    let toggle_sync_message = if monitor_toggle_changed {
        schedule_active_monitor_state_sync_after_toggle_change(&cfg)
    } else {
        String::new()
    };
    let sync_message = if old_monitored_config_version != monitored_config_version(&cfg) {
        sync_monitored_config_after_change(&cfg).await
    } else {
        String::new()
    };
    let membership_message = if cluster_changed {
        match propagate_cluster_membership(&old_cluster, &cfg.cluster).await {
            Ok(count) => format!("；节点配置已同步到 {} 个节点", count),
            Err(e) => {
                tracing::warn!("Cluster membership sync failed: {}", e);
                format!("；节点配置同步失败: {}", e)
            }
        }
    } else {
        String::new()
    };

    Ok(ApiResponse {
        success: true,
        data: None,
        message: Some(format!(
            "配置已更新{}{}{}",
            sync_message, toggle_sync_message, membership_message
        )),
    })
}

#[derive(Deserialize, Serialize)]
pub struct UpdatePriorityChannelRequest {
    #[serde(skip_serializing)]
    expected: Option<HashMap<String, serde_json::Value>>,
    enabled: Option<bool>,
    channel_name: Option<String>,
    default_area: Option<u64>,
    auto_restart: Option<bool>,
}

pub async fn update_priority_channel(
    Json(payload): Json<UpdatePriorityChannelRequest>,
) -> Result<ApiResponse<()>, StatusCode> {
    let mut cfg = load_config()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let old_monitored_config_version = monitored_config_version(&cfg);
    let requests_monitor_toggle_enable =
        payload.enabled == Some(true) || payload.auto_restart == Some(true);
    if requests_monitor_toggle_enable && !local_node_can_enable_monitor_toggles(&cfg) {
        return Ok(monitor_toggle_enable_rejected_response());
    }

    validate_edit_preconditions(
        &json!({
            "enabled": cfg.priority_channel.enabled,
            "auto_restart": cfg.priority_channel.auto_restart,
            "channel_name": cfg.priority_channel.channel_name,
            "default_area": cfg.priority_channel.default_area,
        }),
        &serde_json::to_value(&payload).map_err(|_| StatusCode::BAD_REQUEST)?,
        payload.expected.as_ref(),
    )
    .map_err(|error| {
        if error == EDIT_CONFLICT {
            StatusCode::CONFLICT
        } else {
            StatusCode::BAD_REQUEST
        }
    })?;

    // Update fields
    if let Some(enabled) = payload.enabled {
        cfg.priority_channel.enabled = enabled;
    }
    if let Some(channel_name) = payload.channel_name {
        cfg.priority_channel.channel_name = channel_name;
        // Update platform IDs from channels.json
        crate::config::update_priority_channel_from_channels(&mut cfg);
    }
    if let Some(default_area) = payload.default_area {
        cfg.priority_channel.default_area = default_area;
    }
    if let Some(auto_restart) = payload.auto_restart {
        cfg.priority_channel.auto_restart = auto_restart;
    }

    // Save config
    crate::config::save_config(&mut cfg)
        .await
        .map_err(config_save_status)?;

    // Set config updated flag so main loop can detect the change
    set_config_updated();

    // Refresh status cache with updated configuration
    refresh_status_cache_config_from(&cfg);

    // Refresh priority channel status in background (independent of main loop)
    tokio::spawn(async {
        let _ = refresh_priority_channel_status().await;
    });

    let priority_toggle_changed = payload.enabled.is_some() || payload.auto_restart.is_some();
    let sync_message = if old_monitored_config_version != monitored_config_version(&cfg) {
        sync_monitored_config_after_change(&cfg).await
    } else {
        String::new()
    };
    let toggle_sync_message = if priority_toggle_changed {
        schedule_active_monitor_state_sync_after_toggle_change(&cfg)
    } else {
        String::new()
    };

    Ok(ApiResponse {
        success: true,
        data: None,
        message: Some(format!(
            "优先频道配置已更新{}{}",
            sync_message, toggle_sync_message
        )),
    })
}
