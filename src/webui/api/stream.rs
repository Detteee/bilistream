use super::*;

pub(crate) async fn sync_active_monitor_state_after_toggle_change(cfg: &Config) -> String {
    if !cfg.cluster.enabled {
        return String::new();
    }

    match push_active_monitor_state_to_peers(cfg).await {
        Ok(count) if count > 0 => format!("；监控开关已同步到 {} 个节点", count),
        Ok(_) => String::new(),
        Err(e) => {
            tracing::warn!("Cluster monitor toggle sync failed: {}", e);
            format!("；监控开关同步失败: {}", e)
        }
    }
}

pub(crate) fn apply_danmaku_command_runtime_state(enabled: bool) {
    crate::plugins::enable_danmaku_commands(enabled);
    if enabled {
        if !crate::plugins::is_danmaku_running() {
            crate::plugins::run_danmaku();
        }
    } else if crate::plugins::is_danmaku_running() {
        crate::plugins::stop_danmaku();
    }
}

pub(crate) fn resolve_source_monitor_toggles(
    cfg: &Config,
    before: &ClusterStatus,
    source_node_id: &str,
    monitored_config: &MonitoredConfig,
) -> MonitorToggleState {
    if source_node_id == cfg.cluster.node_id {
        let local_toggles = monitor_toggle_state_from_config(cfg);
        if monitor_toggles_any_enabled(&local_toggles) {
            return local_toggles;
        }
    }

    if let Some(node) = before
        .nodes
        .iter()
        .find(|node| node.node_id == source_node_id)
    {
        if monitor_toggles_any_enabled(&node.monitor_toggles) {
            return node.monitor_toggles.clone();
        }
    }

    if let Some(cached) = last_known_active_toggles() {
        return cached;
    }

    let from_config = monitor_toggle_state_from_monitored_config(monitored_config);
    if monitor_toggles_any_enabled(&from_config) {
        return from_config;
    }

    all_monitor_toggles_on()
}

#[derive(Deserialize)]
pub struct StartStreamRequest {
    platform: Option<String>,
}

pub async fn start_stream(
    Json(payload): Json<StartStreamRequest>,
) -> Result<ApiResponse<serde_json::Value>, StatusCode> {
    let mut cfg = load_config()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let area_v2 = match payload.platform.as_deref() {
        Some("YT") => cfg.youtube.area_v2,
        Some("TW") => cfg.twitch.area_v2,
        _ => 235,
    };

    match bili_start_live(&mut cfg, area_v2).await {
        Ok(_) => Ok(ApiResponse {
            success: true,
            data: Some(json!({})),
            message: Some("直播已开始".to_string()),
        }),
        Err(e) => {
            let error_msg = e.to_string();
            // Check if it's a face verification error
            if error_msg.starts_with("FACE_AUTH_REQUIRED:") {
                let qr_url = error_msg.strip_prefix("FACE_AUTH_REQUIRED:").unwrap_or("");
                Ok(ApiResponse {
                    success: false,
                    data: Some(json!({
                        "requires_face_auth": true,
                        "qr_url": qr_url,
                        "qr_image": qr_code_data_url(qr_url).ok(),
                    })),
                    message: Some("需要人脸验证，请扫描二维码完成验证后重试".to_string()),
                })
            } else {
                // Return proper JSON error response instead of HTTP error
                Ok(ApiResponse {
                    success: false,
                    data: None,
                    message: Some(error_msg),
                })
            }
        }
    }
}

pub async fn stop_stream() -> Result<ApiResponse<()>, StatusCode> {
    let cfg = load_config()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    bili_stop_live(&cfg)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(ApiResponse {
        success: true,
        data: None,
        message: Some("直播已停止".to_string()),
    })
}

pub async fn restart_stream() -> Result<ApiResponse<()>, StatusCode> {
    // Clear any warning stops to allow restreaming
    crate::plugins::danmaku::clear_warning_stop();

    // Publish the restart reason before stopping ffmpeg so the main loop sees a complete state.
    crate::plugins::set_manual_restart();
    set_config_updated();

    // Stop current ffmpeg process
    crate::plugins::stop_ffmpeg().await;

    Ok(ApiResponse {
        success: true,
        data: None,
        message: Some("已停止当前流并重新加载配置".to_string()),
    })
}

pub async fn restart_server_process() -> Result<ApiResponse<()>, StatusCode> {
    let Some(screen_session) = current_screen_session() else {
        return Ok(ApiResponse {
            success: false,
            data: None,
            message: Some(
                "未检测到 screen 会话；请在 screen -R bb 中运行 bilistream 后再重启".to_string(),
            ),
        });
    };
    let restart_command = match restart_command_line() {
        Ok(command) => command,
        Err(e) => {
            tracing::error!("Failed to build server restart command: {}", e);
            return Ok(ApiResponse {
                success: false,
                data: None,
                message: Some(format!("生成重启命令失败: {}", e)),
            });
        }
    };

    if let Err(e) = schedule_screen_restart(&screen_session, &restart_command, std::process::id()) {
        tracing::error!("Failed to schedule screen restart: {}", e);
        return Ok(ApiResponse {
            success: false,
            data: None,
            message: Some(format!("发送 screen 重启命令失败: {}", e)),
        });
    }

    Ok(ApiResponse {
        success: true,
        data: None,
        message: Some(format!("程序将在 screen 会话 {} 中重启", screen_session)),
    })
}

pub(crate) fn current_screen_session() -> Option<String> {
    non_empty_env("BILISTREAM_SCREEN_SESSION")
        .or_else(|| non_empty_env("STY"))
        .or_else(find_named_screen_session)
}

pub(crate) fn non_empty_env(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

pub(crate) fn find_named_screen_session() -> Option<String> {
    let output = Command::new("screen").arg("-ls").output().ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    for preferred in ["bb", "b"] {
        if let Some(session) = find_screen_session_by_name(&text, preferred) {
            return Some(session);
        }
    }
    None
}

pub(crate) fn find_screen_session_by_name(screen_list: &str, name: &str) -> Option<String> {
    screen_list.lines().find_map(|line| {
        let session = line.split_whitespace().next()?;
        let session_name = session.rsplit_once('.').map(|(_, name)| name)?;
        (session_name == name).then(|| session.to_string())
    })
}

pub(crate) fn restart_command_line() -> Result<String, String> {
    let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let exe_word = resolve_restart_executable_word(&cwd, &exe);

    let mut parts = vec![
        "cd".to_string(),
        shell_word(&cwd.to_string_lossy()),
        "&&".to_string(),
        shell_word(&exe_word),
    ];
    parts.extend(std::env::args_os().skip(1).map(|arg| {
        let arg = arg.to_string_lossy();
        shell_word(&arg)
    }));

    Ok(parts.join(" "))
}

pub(crate) fn resolve_restart_executable_word(
    cwd: &std::path::Path,
    current_exe: &std::path::Path,
) -> String {
    let local_bin = cwd.join("bilistream");
    let local_bin_exists = local_bin.exists();
    let current_exe_display = current_exe.to_string_lossy();
    let running_deleted_binary = current_exe_display.ends_with(" (deleted)");

    if local_bin_exists {
        if running_deleted_binary {
            return "./bilistream".to_string();
        }
        if std::fs::canonicalize(&local_bin).ok().as_ref()
            == std::fs::canonicalize(current_exe).ok().as_ref()
        {
            return "./bilistream".to_string();
        }
    }

    if running_deleted_binary {
        return current_exe_display
            .strip_suffix(" (deleted)")
            .unwrap_or(&current_exe_display)
            .to_string();
    }

    current_exe_display.to_string()
}

pub(crate) fn shell_word(value: &str) -> String {
    if value.is_empty() {
        return "''".to_string();
    }

    format!("'{}'", value.replace('\'', "'\\''"))
}

pub(crate) fn schedule_screen_restart(
    screen_session: &str,
    restart_command: &str,
    old_pid: u32,
) -> Result<(), String> {
    let script = concat!(
        "sleep 1; ",
        "screen -S \"$BILISTREAM_RESTART_SCREEN\" -X stuff \"$(printf '\\003')\"; ",
        "for i in $(seq 1 60); do ",
        "kill -0 \"$BILISTREAM_RESTART_OLD_PID\" 2>/dev/null || break; ",
        "sleep 1; ",
        "done; ",
        "if kill -0 \"$BILISTREAM_RESTART_OLD_PID\" 2>/dev/null; then ",
        "kill -TERM \"$BILISTREAM_RESTART_OLD_PID\" 2>/dev/null; ",
        "sleep 2; ",
        "fi; ",
        "sleep 1; ",
        "screen -S \"$BILISTREAM_RESTART_SCREEN\" -X stuff \"$(printf '%s\\015' \"$BILISTREAM_RESTART_COMMAND\")\""
    );

    Command::new("setsid")
        .arg("sh")
        .arg("-c")
        .arg(script)
        .env("BILISTREAM_RESTART_SCREEN", screen_session)
        .env("BILISTREAM_RESTART_COMMAND", restart_command)
        .env("BILISTREAM_RESTART_OLD_PID", old_pid.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|_| ())
        .map_err(|e| e.to_string())
}

#[derive(Deserialize)]
pub struct SendDanmakuRequest {
    message: String,
}

pub async fn send_danmaku(
    Json(payload): Json<SendDanmakuRequest>,
) -> Result<ApiResponse<()>, (StatusCode, String)> {
    let cfg = load_config()
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    match send_danmaku_to_bili(&cfg, &payload.message).await {
        Ok(_) => Ok(ApiResponse {
            success: true,
            data: None,
            message: Some("弹幕已发送".to_string()),
        }),
        Err(e) => {
            let error_msg = e.to_string();
            // Check if it's a rate limit error
            if error_msg.contains("频率过快") {
                Err((
                    StatusCode::TOO_MANY_REQUESTS,
                    "发送频率过快，请稍后再试".to_string(),
                ))
            } else {
                Err((StatusCode::INTERNAL_SERVER_ERROR, error_msg))
            }
        }
    }
}

#[derive(Deserialize)]
pub struct UpdateCoverRequest {
    image_path: String,
}

pub async fn update_cover(
    Json(payload): Json<UpdateCoverRequest>,
) -> Result<ApiResponse<()>, StatusCode> {
    let cfg = load_config()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    bilibili::bili_change_cover(&cfg, &payload.image_path)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(ApiResponse {
        success: true,
        data: None,
        message: Some("封面已更新".to_string()),
    })
}

#[derive(Deserialize)]
pub struct UpdateAreaRequest {
    area_id: u64,
}

pub async fn update_area(
    Json(payload): Json<UpdateAreaRequest>,
) -> Result<ApiResponse<()>, StatusCode> {
    let cfg = load_config()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    bili_update_area(&cfg, payload.area_id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(ApiResponse {
        success: true,
        data: None,
        message: Some("分区已更新".to_string()),
    })
}

#[derive(Deserialize)]
pub struct UpdateTitleRequest {
    title: String,
}

pub async fn update_title(
    Json(payload): Json<UpdateTitleRequest>,
) -> Result<ApiResponse<()>, StatusCode> {
    let cfg = load_config()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    bili_change_live_title(&cfg, &payload.title)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(ApiResponse {
        success: true,
        data: None,
        message: Some("直播标题已更新".to_string()),
    })
}

#[derive(Deserialize)]
pub struct UpdateChannelRequest {
    platform: String, // "youtube" or "twitch"
    channel_id: Option<String>,
    channel_name: Option<String>,
    area_id: Option<u64>,
    quality: Option<String>,
    riot_api_key: Option<String>,
    cookies_file: Option<String>,
    cookies_from_browser: Option<String>,
}

pub async fn update_channel(
    Json(payload): Json<UpdateChannelRequest>,
) -> Result<ApiResponse<()>, StatusCode> {
    let mut cfg = load_config()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let previous_cfg = cfg.clone();
    let old_monitored_config_version = monitored_config_version(&cfg);

    match payload.platform.as_str() {
        "youtube" => {
            if let Some(channel_id) = payload.channel_id {
                cfg.youtube.channel_id = channel_id;
            }
            if let Some(channel_name) = payload.channel_name {
                cfg.youtube.channel_name = channel_name;
            }
            if let Some(area_id) = payload.area_id {
                cfg.youtube.area_v2 = area_id;
                // If area is LOL (86) and riot_api_key is provided, update it
                if area_id == 86 {
                    if let Some(riot_api_key) = payload.riot_api_key {
                        if !riot_api_key.is_empty() {
                            cfg.riot_api_key = Some(riot_api_key);
                        }
                    }
                }
            }
            if let Some(quality) = payload.quality {
                cfg.youtube.quality = quality;
            }
            if let Some(cookies_file) = payload.cookies_file {
                cfg.youtube.cookies_file = if cookies_file.is_empty() {
                    None
                } else {
                    Some(cookies_file)
                };
            }
            if let Some(cookies_from_browser) = payload.cookies_from_browser {
                cfg.youtube.cookies_from_browser = if cookies_from_browser.is_empty() {
                    None
                } else {
                    Some(cookies_from_browser)
                };
            }
        }
        "twitch" => {
            if let Some(channel_id) = payload.channel_id {
                cfg.twitch.channel_id = channel_id;
            }
            if let Some(channel_name) = payload.channel_name {
                cfg.twitch.channel_name = channel_name;
            }
            if let Some(area_id) = payload.area_id {
                cfg.twitch.area_v2 = area_id;
                // If area is LOL (86) and riot_api_key is provided, update it
                if area_id == 86 {
                    if let Some(riot_api_key) = payload.riot_api_key {
                        if !riot_api_key.is_empty() {
                            cfg.riot_api_key = Some(riot_api_key);
                        }
                    }
                }
            }
            if let Some(quality) = payload.quality {
                cfg.twitch.quality = quality;
            }
        }
        _ => return Err(StatusCode::BAD_REQUEST),
    }

    // Save config
    crate::config::save_config(&mut cfg)
        .await
        .map_err(config_save_status)?;

    let refresh_youtube = youtube_monitor_reload_needed(&previous_cfg, &cfg);
    let refresh_twitch = twitch_monitor_reload_needed(&previous_cfg, &cfg);

    if refresh_youtube || refresh_twitch {
        set_config_updated();
    }

    // Refresh status cache with updated configuration
    refresh_status_cache_config_from(&cfg);

    // Refresh live status in background only when the active monitor target changed.
    if refresh_youtube || refresh_twitch {
        tokio::spawn(async move {
            if refresh_youtube {
                let _ = refresh_youtube_status().await;
            }
            if refresh_twitch {
                let _ = refresh_twitch_status().await;
            }
        });
    }

    let sync_message = if old_monitored_config_version != monitored_config_version(&cfg) {
        sync_monitored_config_after_change(&cfg).await
    } else {
        String::new()
    };

    Ok(ApiResponse {
        success: true,
        data: None,
        message: Some(format!("{} 频道已更新{}", payload.platform, sync_message)),
    })
}

pub async fn get_channels() -> Result<Json<serde_json::Value>, StatusCode> {
    let channels_path = std::env::current_exe()
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .with_file_name("channels.json");

    let content = tokio::fs::read_to_string(channels_path)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let channels: serde_json::Value =
        serde_json::from_str(&content).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(Json(channels))
}

pub async fn get_areas() -> Result<Json<serde_json::Value>, StatusCode> {
    let areas_path = std::env::current_exe()
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .with_file_name("areas.json");

    let content = tokio::fs::read_to_string(areas_path)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let areas: serde_json::Value =
        serde_json::from_str(&content).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(Json(areas))
}

#[derive(Serialize)]
pub struct BannedKeywordsResponse {
    danmaku_banned_keywords: Vec<String>,
    streaming_banned_keywords: Vec<String>,
}

pub async fn get_banned_keywords() -> Result<Json<BannedKeywordsResponse>, StatusCode> {
    let areas_path = std::env::current_exe()
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .with_file_name("areas.json");

    let content = tokio::fs::read_to_string(areas_path)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let data: serde_json::Value =
        serde_json::from_str(&content).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let danmaku_banned = data["banned_keywords"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();

    let streaming_banned = data["streaming_banned_keywords"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();

    Ok(Json(BannedKeywordsResponse {
        danmaku_banned_keywords: danmaku_banned,
        streaming_banned_keywords: streaming_banned,
    }))
}

#[derive(Deserialize, Serialize)]
pub struct UpdateBannedKeywordsRequest {
    #[serde(skip_serializing)]
    expected: Option<HashMap<String, serde_json::Value>>,
    danmaku_banned_keywords: Option<Vec<String>>,
    streaming_banned_keywords: Option<Vec<String>>,
}

pub async fn update_banned_keywords(
    Json(payload): Json<UpdateBannedKeywordsRequest>,
) -> Result<ApiResponse<()>, StatusCode> {
    let areas_path = std::env::current_exe()
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .with_file_name("areas.json");

    let patch = serde_json::to_value(&payload).map_err(|_| StatusCode::BAD_REQUEST)?;
    let result = crate::config::mutate_json_file(areas_path, move |data: &mut serde_json::Value| {
        let current = json!({
            "danmaku_banned_keywords": data.get("banned_keywords").cloned().unwrap_or_else(|| json!([])),
            "streaming_banned_keywords": data.get("streaming_banned_keywords").cloned().unwrap_or_else(|| json!([])),
        });
        validate_edit_preconditions(&current, &patch, payload.expected.as_ref())
            .map_err(str::to_owned)?;
        if let Some(keywords) = payload.danmaku_banned_keywords {
            data["banned_keywords"] = serde_json::json!(keywords);
        }
        if let Some(keywords) = payload.streaming_banned_keywords {
            data["streaming_banned_keywords"] = serde_json::json!(keywords);
        }
        Ok(())
    })
    .await;
    if let Err(error) = result {
        return Err(if error == EDIT_CONFLICT {
            StatusCode::CONFLICT
        } else if error == EDIT_INVALID {
            StatusCode::BAD_REQUEST
        } else {
            StatusCode::INTERNAL_SERVER_ERROR
        });
    }
    set_config_updated();

    Ok(ApiResponse {
        success: true,
        data: None,
        message: Some("禁用关键词已更新".to_string()),
    })
}

#[derive(Deserialize)]
pub struct ToggleMonitorRequest {
    enabled: bool,
}

pub async fn toggle_youtube_monitor(
    Json(payload): Json<ToggleMonitorRequest>,
) -> Result<ApiResponse<()>, StatusCode> {
    let mut cfg = load_config()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    if cfg.youtube.enable_monitor == payload.enabled {
        refresh_status_cache_config_from(&cfg);
        return Ok(ApiResponse {
            success: true,
            data: None,
            message: Some(format!(
                "YouTube监控已是{}",
                if payload.enabled { "启用" } else { "禁用" }
            )),
        });
    }

    if payload.enabled && !local_node_can_enable_monitor_toggles(&cfg).await {
        return Ok(monitor_toggle_enable_rejected_response());
    }

    cfg.youtube.enable_monitor = payload.enabled;
    cfg.enable_youtube_monitor = payload.enabled;

    crate::config::save_config(&mut cfg)
        .await
        .map_err(config_save_status)?;

    set_config_updated();
    refresh_status_cache_config_from(&cfg);
    crate::webui::state::request_status_refresh();
    let toggle_sync_message = sync_active_monitor_state_after_toggle_change(&cfg).await;

    Ok(ApiResponse {
        success: true,
        data: None,
        message: Some(format!(
            "YouTube监控已{}{}",
            if payload.enabled { "启用" } else { "禁用" },
            toggle_sync_message
        )),
    })
}

pub async fn toggle_twitch_monitor(
    Json(payload): Json<ToggleMonitorRequest>,
) -> Result<ApiResponse<()>, StatusCode> {
    let mut cfg = load_config()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    if cfg.twitch.enable_monitor == payload.enabled {
        refresh_status_cache_config_from(&cfg);
        return Ok(ApiResponse {
            success: true,
            data: None,
            message: Some(format!(
                "Twitch监控已是{}",
                if payload.enabled { "启用" } else { "禁用" }
            )),
        });
    }

    if payload.enabled && !local_node_can_enable_monitor_toggles(&cfg).await {
        return Ok(monitor_toggle_enable_rejected_response());
    }

    cfg.twitch.enable_monitor = payload.enabled;
    cfg.enable_twitch_monitor = payload.enabled;

    crate::config::save_config(&mut cfg)
        .await
        .map_err(config_save_status)?;

    set_config_updated();
    refresh_status_cache_config_from(&cfg);
    crate::webui::state::request_status_refresh();
    let toggle_sync_message = sync_active_monitor_state_after_toggle_change(&cfg).await;

    Ok(ApiResponse {
        success: true,
        data: None,
        message: Some(format!(
            "Twitch监控已{}{}",
            if payload.enabled { "启用" } else { "禁用" },
            toggle_sync_message
        )),
    })
}
