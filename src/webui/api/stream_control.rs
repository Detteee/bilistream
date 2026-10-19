//! Stream and danmaku **control** endpoints: start/stop/restart, the priority
//! warning release, the process restart, and the danmaku/cover/area writes.
//!
//! Named `stream_control` rather than `stream`: the bare name collided with the
//! three stream-list modules and with `api::stream_switch` (layout switching),
//! so `grep`-based navigation could not tell them apart. Those are now split by
//! audience — `api::admin_stream_list` (admin HTTP edge), `webui::stream_list`
//! (the shared list service), `public::streams` (the public page's list) — and
//! the pair left here reads as control vs switching: this module starts, stops,
//! restarts and writes, `stream_switch` changes the restream target.

use super::*;

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
        Some("NC") => cfg.niconico.area_v2,
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

    // The room is offline now, so record the session's 下播 cause here:
    // the runtime only observes it later, and this is the deliberate action.
    crate::notify::live_ended(crate::events::BroadcastEndCause::ManualStop);

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
    crate::cluster::clear_local_stream();

    // Stop current ffmpeg process
    crate::plugins::stop_ffmpeg().await;

    Ok(ApiResponse {
        success: true,
        data: None,
        message: Some("已停止当前流并重新加载配置".to_string()),
    })
}

#[derive(Deserialize)]
pub struct ReleasePriorityWarningRequest {
    channel_name: String,
    #[serde(default)]
    platform: Option<String>,
}

pub(crate) async fn release_priority_warning(
    axum::Extension(auth): axum::Extension<std::sync::Arc<crate::webui::listen::AuthState>>,
    generation: Option<axum::Extension<crate::webui::listen::AuthGeneration>>,
    headers: axum::http::HeaderMap,
    Json(payload): Json<ReleasePriorityWarningRequest>,
) -> Result<ApiResponse<serde_json::Value>, StatusCode> {
    if !auth.browser_mutation_allowed(&headers, generation.map(|axum::Extension(g)| g)) {
        return Err(StatusCode::FORBIDDEN);
    }
    use crate::plugins::danmaku::warning::warning_tracker;
    let warnings = warning_tracker();
    let before_action = warnings.snapshot();
    let cfg = load_config()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let platform = payload.platform.as_deref();
    let priority_card = is_priority_warning_card(platform);
    let release = crate::config::with_current_config(&cfg, || {
        release_card_warning_for_config(
            &warnings,
            &before_action,
            &cfg,
            &payload.channel_name,
            platform,
        )
    })
    .ok_or(StatusCode::CONFLICT)?;
    let release = match release {
        Ok(release) => release,
        Err(message) => {
            return Ok(ApiResponse {
                success: false,
                data: None,
                message: Some(message.to_owned()),
            })
        }
    };
    let released = release.is_some();
    if let Some(release) = release {
        release.log();
        // Wake idle selection. This is not a stop/restart request, and with
        // auto-restart off the currently owned source keeps running.
        set_config_updated();
    }
    refresh_status_cache_config_from(&cfg);
    Ok(ApiResponse {
        success: true,
        data: Some(json!({"released": released})),
        message: Some(
            if released {
                if priority_card {
                    "优先频道的临时跳过已解除"
                } else {
                    "临时跳过已解除"
                }
            } else if priority_card {
                "优先频道当前没有临时跳过"
            } else {
                "当前没有临时跳过"
            }
            .to_owned(),
        ),
    })
}

fn is_priority_warning_card(platform: Option<&str>) -> bool {
    matches!(
        platform
            .map(str::trim)
            .filter(|platform| !platform.is_empty()),
        None | Some("priority")
    )
}

#[cfg(test)]
fn release_priority_warning_for_config(
    warnings: &crate::plugins::danmaku::warning::WarningTracker,
    before_action: &crate::plugins::danmaku::warning::WarningSnapshot,
    cfg: &Config,
    requested_name: &str,
) -> Result<Option<crate::plugins::danmaku::warning::WarningRelease>, &'static str> {
    release_card_warning_for_config(warnings, before_action, cfg, requested_name, None)
}

fn release_card_warning_for_config(
    warnings: &crate::plugins::danmaku::warning::WarningTracker,
    before_action: &crate::plugins::danmaku::warning::WarningSnapshot,
    cfg: &Config,
    requested_name: &str,
    platform: Option<&str>,
) -> Result<Option<crate::plugins::danmaku::warning::WarningRelease>, &'static str> {
    use crate::plugins::danmaku::warning::{BroadcastSource, WarningReleaseReason};
    let reason = WarningReleaseReason::AdminCard;
    if is_priority_warning_card(platform) {
        if requested_name.is_empty() || requested_name != cfg.priority_channel.channel_name {
            return Err("优先频道已变更，请刷新后重试");
        }
        let priority = &cfg.priority_channel;
        let priority_source = |platform, channel_id: &str| {
            BroadcastSource::new(platform, requested_name, channel_id, None)
        };
        return Ok(warnings
            .release_name(before_action, requested_name, reason)
            .or_else(|| {
                warnings.release_source(
                    before_action,
                    &priority_source("YT", &priority.youtube_channel_id),
                    reason,
                )
            })
            .or_else(|| {
                warnings.release_source(
                    before_action,
                    &priority_source("TW", &priority.twitch_channel_id),
                    reason,
                )
            }));
    }
    // A source card releases exactly the identity it renders: `release_source`
    // also matches display names, so a Twitch hold must not clear from the
    // YouTube card of the same talent. That gate also makes the release
    // unconditional here — a matched identity always matches `release_source`.
    let code = platform.map(str::trim).unwrap_or_default();
    let source = crate::plugins::danmaku::configured_broadcast_source(cfg, code)
        .ok_or("频道已变更，请刷新后重试")?;
    if requested_name.is_empty() || requested_name != source.channel_name {
        return Err("频道已变更，请刷新后重试");
    }
    if !before_action.holds_identity(&source) {
        return Ok(None);
    }
    Ok(warnings.release_source(before_action, &source, reason))
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
    let restart_command = match crate::webui::restart::restart_command_line() {
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

    if let Err(e) = schedule_screen_restart(
        &screen_session,
        restart_command.as_str(),
        std::process::id(),
    ) {
        tracing::error!("Failed to schedule screen restart: {}", e);
        return Ok(ApiResponse {
            success: false,
            data: None,
            message: Some(format!("发送 screen 重启命令失败: {}", e)),
        });
    }
    restart_command.handed_off();

    Ok(ApiResponse {
        success: true,
        data: None,
        message: Some(format!("程序将在 screen 会话 {} 中重启", screen_session)),
    })
}

pub(crate) fn current_screen_session() -> Option<String> {
    inherited_screen_session().or_else(find_named_screen_session)
}

pub(crate) fn inherited_screen_session() -> Option<String> {
    non_empty_env("BILISTREAM_SCREEN_SESSION").or_else(|| non_empty_env("STY"))
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
        .env_remove("BILISTREAM_PASSWORD")
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
    // Historical field name; only application cache names are accepted.
    image_path: String,
}

async fn read_cached_cover(path: &Path) -> std::io::Result<Vec<u8>> {
    use tokio::io::AsyncReadExt;
    let mut options = tokio::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    #[cfg(windows)]
    options.custom_flags(0x0020_0000); // FILE_FLAG_OPEN_REPARSE_POINT
    let file = options.open(path).await?;
    let metadata = file.metadata().await?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(std::io::Error::other("封面必须是普通图片文件"));
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if metadata.file_attributes() & 0x400 != 0 {
            // FILE_ATTRIBUTE_REPARSE_POINT
            return Err(std::io::Error::other("封面不能是链接"));
        }
    }
    const MAX_BYTES: u64 = 16 * 1024 * 1024;
    let mut bytes = Vec::new();
    file.take(MAX_BYTES + 1).read_to_end(&mut bytes).await?;
    if bytes.is_empty() || bytes.len() as u64 > MAX_BYTES {
        return Err(std::io::Error::other("封面为空或过大"));
    }
    Ok(bytes)
}

pub async fn update_cover(
    Json(payload): Json<UpdateCoverRequest>,
) -> Result<ApiResponse<()>, StatusCode> {
    // Resolve the name before loading credentials or contacting Bilibili.
    if !matches!(
        payload.image_path.as_str(),
        "cover.jpg" | "pic_for_crop.jpg"
    ) {
        return Err(StatusCode::BAD_REQUEST);
    }
    let path = crate::storage::cache_file(&payload.image_path)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let bytes = read_cached_cover(&path).await.map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            StatusCode::NOT_FOUND
        } else {
            StatusCode::BAD_REQUEST
        }
    })?;
    let cfg = load_config()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    bilibili::bili_change_cover_bytes(&cfg, bytes)
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
    platform: String, // "youtube", "twitch", or "niconico"
    channel_id: Option<String>,
    channel_name: Option<String>,
    live_id: Option<String>,
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
        "niconico" => {
            if let Some(channel_id) = payload.channel_id {
                cfg.niconico.channel_id = crate::plugins::normalize_channel_id(&channel_id);
            }
            if let Some(channel_name) = payload.channel_name {
                cfg.niconico.channel_name = channel_name;
            }
            if let Some(live_id) = payload.live_id {
                cfg.niconico.live_id = crate::plugins::normalize_live_id(&live_id);
            }
            if let Some(area_id) = payload.area_id {
                cfg.niconico.area_v2 = area_id;
                if area_id == 86 {
                    if let Some(riot_api_key) = payload.riot_api_key {
                        if !riot_api_key.is_empty() {
                            cfg.riot_api_key = Some(riot_api_key);
                        }
                    }
                }
            }
            if let Some(quality) = payload.quality {
                cfg.niconico.quality = quality;
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
    let refresh_niconico = niconico_monitor_reload_needed(&previous_cfg, &cfg);

    if refresh_youtube || refresh_twitch || refresh_niconico {
        set_config_updated();
    }

    // Refresh status cache with updated configuration
    refresh_status_cache_config_from(&cfg);

    // Refresh live status in background only when the active monitor target changed.
    if refresh_youtube || refresh_twitch || refresh_niconico {
        tokio::spawn(async move {
            if refresh_youtube {
                let _ = refresh_youtube_status().await;
            }
            if refresh_twitch {
                let _ = refresh_twitch_status().await;
            }
            if refresh_niconico {
                let _ = refresh_niconico_status().await;
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
    crate::storage::read_document("channels.json")
        .map(Json)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

pub async fn get_areas() -> Result<Json<serde_json::Value>, StatusCode> {
    crate::storage::read_document("areas.json")
        .map(Json)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

#[derive(Serialize)]
pub struct BannedKeywordsResponse {
    danmaku_banned_keywords: Vec<String>,
    streaming_banned_keywords: Vec<String>,
}

pub async fn get_banned_keywords() -> Result<Json<BannedKeywordsResponse>, StatusCode> {
    let data: serde_json::Value = crate::storage::read_document("areas.json")
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

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
    let patch = serde_json::to_value(&payload).map_err(|_| StatusCode::BAD_REQUEST)?;
    let result = crate::config::mutate_document("areas.json", move |data: &mut serde_json::Value| {
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

    crate::webui::public::remap_after_areas_change();
    let sync_message = schedule_managed_document_sync_after_change("areas.json");

    Ok(ApiResponse {
        success: true,
        data: None,
        message: Some(format!("禁用关键词已更新{sync_message}")),
    })
}

#[derive(Deserialize)]
pub struct ToggleMonitorRequest {
    enabled: bool,
}

pub(crate) fn youtube_monitor_toggle_matches(cfg: &Config, enabled: bool) -> bool {
    cfg.youtube.enable_monitor == enabled && cfg.enable_youtube_monitor == enabled
}

pub(crate) fn twitch_monitor_toggle_matches(cfg: &Config, enabled: bool) -> bool {
    cfg.twitch.enable_monitor == enabled && cfg.enable_twitch_monitor == enabled
}

pub async fn toggle_youtube_monitor(
    Json(payload): Json<ToggleMonitorRequest>,
) -> Result<ApiResponse<()>, StatusCode> {
    let mut cfg = load_config()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    if payload.enabled && !local_node_can_enable_monitor_toggles(&cfg) {
        return Ok(monitor_toggle_enable_rejected_response());
    }

    if youtube_monitor_toggle_matches(&cfg, payload.enabled) {
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

    cfg.youtube.enable_monitor = payload.enabled;
    cfg.enable_youtube_monitor = payload.enabled;

    crate::config::save_config(&mut cfg)
        .await
        .map_err(config_save_status)?;

    set_config_updated();
    refresh_status_cache_config_from(&cfg);
    crate::webui::state::request_status_refresh();
    let toggle_sync_message = schedule_active_monitor_state_sync_after_toggle_change(&cfg);

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

    if payload.enabled && !local_node_can_enable_monitor_toggles(&cfg) {
        return Ok(monitor_toggle_enable_rejected_response());
    }

    if twitch_monitor_toggle_matches(&cfg, payload.enabled) {
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

    cfg.twitch.enable_monitor = payload.enabled;
    cfg.enable_twitch_monitor = payload.enabled;

    crate::config::save_config(&mut cfg)
        .await
        .map_err(config_save_status)?;

    set_config_updated();
    refresh_status_cache_config_from(&cfg);
    crate::webui::state::request_status_refresh();
    let toggle_sync_message = schedule_active_monitor_state_sync_after_toggle_change(&cfg);

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

pub(crate) fn niconico_monitor_toggle_matches(cfg: &Config, enabled: bool) -> bool {
    cfg.niconico.enable_monitor == enabled
}

pub async fn toggle_niconico_monitor(
    Json(payload): Json<ToggleMonitorRequest>,
) -> Result<ApiResponse<()>, StatusCode> {
    let mut cfg = load_config()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    if payload.enabled && !local_node_can_enable_monitor_toggles(&cfg) {
        return Ok(monitor_toggle_enable_rejected_response());
    }

    if niconico_monitor_toggle_matches(&cfg, payload.enabled) {
        refresh_status_cache_config_from(&cfg);
        return Ok(ApiResponse {
            success: true,
            data: None,
            message: Some(format!(
                "Niconico监控已是{}",
                if payload.enabled { "启用" } else { "禁用" }
            )),
        });
    }

    cfg.niconico.enable_monitor = payload.enabled;

    crate::config::save_config(&mut cfg)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    set_config_updated();
    refresh_status_cache_config_from(&cfg);
    crate::webui::state::request_status_refresh();
    let toggle_sync_message = schedule_active_monitor_state_sync_after_toggle_change(&cfg);

    Ok(ApiResponse {
        success: true,
        data: None,
        message: Some(format!(
            "Niconico监控已{}{}",
            if payload.enabled { "启用" } else { "禁用" },
            toggle_sync_message
        )),
    })
}

#[cfg(test)]
mod priority_warning_tests {
    use super::*;
    use crate::plugins::danmaku::warning::{BroadcastSource, WarningTracker};
    use std::sync::Arc;

    fn priority_config(auto_restart: bool) -> Config {
        let mut cfg = crate::test_support::config("priority-admin-test", 0);
        cfg.priority_channel.enabled = true;
        cfg.priority_channel.auto_restart = auto_restart;
        cfg.priority_channel.channel_name = "示例频道 A".into();
        cfg.priority_channel.youtube_channel_id = "UC-held".into();
        cfg.priority_channel.twitch_channel_id = "held_login".into();
        cfg
    }

    #[test]
    fn priority_release_preserves_current_source_and_saved_targets_in_both_restart_modes() {
        for auto_restart in [false, true] {
            let cfg = priority_config(auto_restart);
            let saved = serde_json::to_value(&cfg).unwrap();
            let warnings = Arc::new(WarningTracker::default());
            let held = BroadcastSource::new("YT", "示例频道 A", "UC-held", Some("episode-a"));
            let _held_owner = warnings.own_source(71, held.clone());
            let mut event = warnings.capture_event();
            warnings.mark_event(&mut event, None).unwrap();
            let running =
                BroadcastSource::new("TW", "示例频道 B", "other_login", Some("episode-b"));
            let _running_owner = warnings.own_source(72, running.clone());

            assert!(release_priority_warning_for_config(
                &warnings,
                &warnings.snapshot(),
                &cfg,
                &cfg.priority_channel.channel_name,
            )
            .unwrap()
            .is_some());
            assert!(!warnings.snapshot().holds(&held));
            assert_eq!(warnings.active_source(), Some(running));
            assert_eq!(serde_json::to_value(&cfg).unwrap(), saved);
            assert!(
                release_priority_warning_for_config(
                    &warnings,
                    &warnings.snapshot(),
                    &cfg,
                    &cfg.priority_channel.channel_name,
                )
                .unwrap()
                .is_none(),
                "repeating the release is harmless"
            );
        }
    }

    #[test]
    fn priority_release_rejects_stale_card_names_and_cannot_clear_a_newer_warning() {
        let mut cfg = priority_config(false);
        let warnings = Arc::new(WarningTracker::default());
        let held = BroadcastSource::new("YT", "示例频道 A", "UC-held", Some("episode-a"));
        let _owner = warnings.own_source(81, held.clone());
        let mut event = warnings.capture_event();
        warnings.mark_event(&mut event, None).unwrap();
        let old_click = warnings.snapshot();

        for requested in ["", "已经更换的频道"] {
            assert!(
                release_priority_warning_for_config(&warnings, &old_click, &cfg, requested)
                    .is_err()
            );
            assert!(warnings.snapshot().holds(&held));
        }
        cfg.priority_channel.channel_name = "示例频道 B".into();
        cfg.priority_channel.youtube_channel_id = "UC-other".into();
        cfg.priority_channel.twitch_channel_id = "other_login".into();
        assert!(
            release_priority_warning_for_config(&warnings, &old_click, &cfg, "示例频道 A",)
                .is_err()
        );
        assert!(
            release_priority_warning_for_config(&warnings, &old_click, &cfg, "示例频道 B",)
                .unwrap()
                .is_none(),
            "the current card cannot clear an unrelated hold"
        );
        assert!(warnings.snapshot().holds(&held));

        let cfg = priority_config(false);
        let mut newer = warnings.capture_event();
        warnings.mark_event(&mut newer, None).unwrap();
        assert!(
            release_priority_warning_for_config(&warnings, &old_click, &cfg, "示例频道 A",)
                .unwrap()
                .is_none()
        );
        assert!(warnings.snapshot().holds(&held));
        assert!(release_priority_warning_for_config(
            &warnings,
            &warnings.snapshot(),
            &cfg,
            "示例频道 A",
        )
        .unwrap()
        .is_some());
    }

    #[test]
    fn priority_release_matches_a_changed_alias_by_canonical_provider_id() {
        for (platform, id) in [("YT", "UC-held"), ("TW", "HELD_LOGIN")] {
            let cfg = priority_config(false);
            let warnings = Arc::new(WarningTracker::default());
            let held = BroadcastSource::new(platform, "频道旧显示名", id, Some("episode-a"));
            let _owner = warnings.own_source(91, held.clone());
            let mut event = warnings.capture_event();
            warnings.mark_event(&mut event, None).unwrap();
            assert!(release_priority_warning_for_config(
                &warnings,
                &warnings.snapshot(),
                &cfg,
                "示例频道 A",
            )
            .unwrap()
            .is_some());
            assert!(!warnings.snapshot().holds(&held));
            assert_eq!(warnings.active_source(), Some(held));
        }
    }

    #[test]
    fn ordinary_twitch_card_releases_that_live_id_without_lighting_youtube() {
        let mut cfg = priority_config(false);
        cfg.youtube.channel_name = "示例频道 A".into();
        cfg.youtube.channel_id = "UC-other".into();
        cfg.twitch.channel_name = "示例频道 A".into();
        cfg.twitch.channel_id = "held_login".into();
        let warnings = Arc::new(WarningTracker::default());
        let held = BroadcastSource::new("TW", "示例频道 A", "held_login", Some("tw-stream-1"));
        let _owner = warnings.own_source(93, held.clone());
        let mut event = warnings.capture_event();
        warnings.mark_event(&mut event, None).unwrap();
        assert!(!warnings.snapshot().holds_identity(&BroadcastSource::new(
            "YT",
            "示例频道 A",
            "UC-other",
            None,
        )));
        assert!(
            release_card_warning_for_config(
                &warnings,
                &warnings.snapshot(),
                &cfg,
                "示例频道 A",
                Some("YT"),
            )
            .unwrap()
            .is_none(),
            "the YouTube card is not this hold"
        );
        assert!(warnings.snapshot().holds(&held));
        assert!(release_card_warning_for_config(
            &warnings,
            &warnings.snapshot(),
            &cfg,
            "示例频道 A",
            Some("TW"),
        )
        .unwrap()
        .is_some());
        assert!(!warnings.snapshot().holds(&held));
    }

    #[test]
    fn a_card_platform_resolves_to_the_source_its_config_names() {
        let mut cfg = priority_config(false);
        cfg.youtube.channel_name = "示例频道 A".into();
        cfg.youtube.channel_id = "UC-other".into();
        cfg.twitch.channel_name = "示例频道 A".into();
        cfg.twitch.channel_id = "Held_Login".into();
        let yt = crate::plugins::danmaku::configured_broadcast_source(&cfg, "YT").unwrap();
        let tw = crate::plugins::danmaku::configured_broadcast_source(&cfg, "TW").unwrap();
        assert_eq!(
            (yt.platform.as_str(), yt.channel_id.as_str()),
            ("YT", "UC-other")
        );
        // Twitch ids compare case-insensitively, so the card still matches the hold.
        assert_eq!(tw.channel_id, "held_login");
        assert!(crate::plugins::danmaku::configured_broadcast_source(&cfg, "priority").is_none());
        assert!(crate::plugins::danmaku::configured_broadcast_source(&cfg, "").is_none());
    }

    #[tokio::test]
    async fn priority_release_route_requires_browser_auth_and_same_origin() {
        use crate::webui::listen::AuthState;
        use axum::body::Body;
        use axum::http::{header, Request};
        use tower::ServiceExt;

        crate::install_crypto_provider();
        let dir = std::env::temp_dir().join(format!(
            "bilistream-priority-auth-{}",
            crate::cluster::peer_auth::random_id().unwrap(),
        ));
        {
            let store =
                crate::storage::Store::open(dir.join("data"), dir.join("key"), None).unwrap();
            let auth = AuthState::open(store, Some("synthetic-priority-password".into())).unwrap();
            let app = crate::webui::server::build_app(crate::AppState::new(), auth);
            let login = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/api/login")
                        .header(header::HOST, "panel.invalid")
                        .header(header::ORIGIN, "http://panel.invalid")
                        .header(header::CONTENT_TYPE, "application/json")
                        .body(Body::from(r#"{"password":"synthetic-priority-password"}"#))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(login.status(), StatusCode::OK);
            let cookie = login.headers()[header::SET_COOKIE]
                .to_str()
                .unwrap()
                .split(';')
                .next()
                .unwrap()
                .to_owned();
            drop(login);
            for (cookie, origin, bearer, expected) in [
                (
                    None,
                    Some("http://panel.invalid"),
                    false,
                    StatusCode::UNAUTHORIZED,
                ),
                (
                    Some("bilistream_session=invalid"),
                    Some("http://panel.invalid"),
                    false,
                    StatusCode::UNAUTHORIZED,
                ),
                (
                    Some(cookie.as_str()),
                    Some("http://panel.invalid"),
                    true,
                    StatusCode::UNAUTHORIZED,
                ),
                (
                    Some(cookie.as_str()),
                    Some("http://other.invalid"),
                    false,
                    StatusCode::FORBIDDEN,
                ),
                (Some(cookie.as_str()), None, false, StatusCode::FORBIDDEN),
            ] {
                let mut request = Request::builder()
                    .method("POST")
                    .uri("/api/priority-channel/release-warning")
                    .header(header::HOST, "panel.invalid")
                    .header(header::CONTENT_TYPE, "application/json");
                if let Some(cookie) = cookie {
                    request = request.header(header::COOKIE, cookie);
                }
                if let Some(origin) = origin {
                    request = request.header(header::ORIGIN, origin);
                }
                if bearer {
                    request = request.header(header::AUTHORIZATION, "Bearer synthetic-peer-token");
                }
                let response = app
                    .clone()
                    .oneshot(
                        request
                            .body(Body::from(r#"{"channel_name":"示例频道 A"}"#))
                            .unwrap(),
                    )
                    .await
                    .unwrap();
                assert_eq!(
                    response.status(),
                    expected,
                    "origin={origin:?}, bearer={bearer}"
                );
            }
            // Authenticated same-origin traffic reaches the real typed route.
            // An invalid body stops before process-wide config/hold mutation.
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/api/priority-channel/release-warning")
                        .header(header::HOST, "panel.invalid")
                        .header(header::ORIGIN, "http://panel.invalid")
                        .header(header::COOKIE, &cookie)
                        .header(header::CONTENT_TYPE, "application/json")
                        .body(Body::from(r#"{"channel_name":42}"#))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
            let response = app
                .oneshot(
                    Request::builder()
                        .method("GET")
                        .uri("/api/priority-channel/release-warning")
                        .header(header::COOKIE, &cookie)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn priority_release_has_no_public_route() {
        use axum::body::Body;
        use axum::http::Request;
        use tower::ServiceExt;

        let app = crate::webui::public::server::public_router();
        for path in [
            "/api/priority-channel/release-warning",
            "/api/public/priority-channel/release-warning",
        ] {
            for (method, expected) in [
                ("GET", StatusCode::NOT_FOUND),
                ("POST", StatusCode::METHOD_NOT_ALLOWED),
            ] {
                let response = app
                    .clone()
                    .oneshot(
                        Request::builder()
                            .method(method)
                            .uri(path)
                            .body(Body::empty())
                            .unwrap(),
                    )
                    .await
                    .unwrap();
                assert_eq!(response.status(), expected, "{method} {path}");
            }
        }
    }

    #[tokio::test]
    async fn priority_release_router_success_and_stale_credentials_are_isolated() {
        use std::process::Stdio;

        // The real handler uses process-global config, warning and stop state.
        // Run it alone so this assertion cannot alter parallel unit tests.
        let child = tokio::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "webui::api::stream_control::priority_warning_tests::priority_release_router_fixture",
                "--ignored",
                "--nocapture",
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let pid = child.id().unwrap();
        let output = tokio::time::timeout(Duration::from_secs(20), child.wait_with_output())
            .await
            .expect("isolated admin route fixture timed out")
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success(),
            "{stdout}\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            stdout.contains("priority release router fixture verified"),
            "fixture did not run: {stdout}"
        );
        std::fs::remove_dir_all(std::env::temp_dir().join(format!("bilistream-unit-store-{pid}")))
            .unwrap();
    }

    #[tokio::test]
    #[ignore = "subprocess fixture for priority_release_router_success_and_stale_credentials_are_isolated"]
    async fn priority_release_router_fixture() {
        use crate::plugins::danmaku::warning::{warning_tracker, WarningReleaseReason};
        use crate::webui::listen::{AuthGeneration, AuthState};
        use axum::body::{to_bytes, Body};
        use axum::http::{header, HeaderMap, Request};
        use tower::ServiceExt;

        crate::install_crypto_provider();
        let store = crate::storage::global().unwrap();
        store
            .write(
                "channels.json",
                json!({"channels":[{
                    "name":"示例频道 A", "aliases":[], "riot_puuid":null,
                    "platforms":{"youtube":"UC-held","twitch":"held_login"}
                }]}),
            )
            .unwrap();
        let auth =
            AuthState::open(store.clone(), Some("synthetic-router-password".into())).unwrap();
        let app = crate::webui::server::build_app(crate::AppState::current(), auth.clone());
        let login = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/login")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"password":"synthetic-router-password"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(login.status(), StatusCode::OK);
        let cookie = login.headers()[header::SET_COOKIE]
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_owned();
        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, "panel.invalid".parse().unwrap());
        headers.insert(header::ORIGIN, "http://panel.invalid".parse().unwrap());
        headers.insert(header::COOKIE, cookie.parse().unwrap());

        let warnings = warning_tracker();
        let held = BroadcastSource::new("YT", "示例频道 A", "UC-held", Some("episode-a"));
        let running = BroadcastSource::new("TW", "示例频道 B", "other_login", Some("episode-b"));
        for auto_restart in [false, true] {
            let mut cfg = priority_config(auto_restart);
            cfg.cluster.enabled = false;
            store
                .write("config.json", serde_json::to_value(cfg).unwrap())
                .unwrap();
            let cfg = load_config().await.unwrap();
            let saved = store.read("config.json").unwrap().unwrap();
            let version = monitored_config_version(&cfg);
            let _ = warnings.clear(WarningReleaseReason::ManualRestart);
            let _held_owner = warnings.own_source(101, held.clone());
            let mut event = warnings.capture_event();
            warnings.mark_event(&mut event, None).unwrap();
            let _running_owner = warnings.own_source(102, running.clone());
            crate::plugins::danmaku::clear_config_updated();
            crate::plugins::ffmpeg::clear_manual_restart();
            crate::plugins::ffmpeg::clear_manual_stop();
            let stop_generation = crate::plugins::ffmpeg::stop_restart_generation();

            let mut request = Request::builder()
                .method("POST")
                .uri("/api/priority-channel/release-warning")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"channel_name":"示例频道 A"}"#))
                .unwrap();
            request.headers_mut().extend(headers.clone());
            let response = app.clone().oneshot(request).await.unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let response: serde_json::Value =
                serde_json::from_slice(&to_bytes(response.into_body(), 64 * 1024).await.unwrap())
                    .unwrap();
            assert_eq!(response["success"], true);
            assert_eq!(response["data"]["released"], true);
            assert!(!warnings.snapshot().holds(&held));
            assert_eq!(warnings.active_source(), Some(running.clone()));
            assert!(
                crate::plugins::danmaku::is_config_updated(),
                "idle selection is woken"
            );
            assert!(!crate::plugins::ffmpeg::was_manual_restart());
            assert!(!crate::plugins::ffmpeg::was_manual_stop());
            assert_eq!(
                crate::plugins::ffmpeg::stop_restart_generation(),
                stop_generation
            );
            let after = store.read("config.json").unwrap().unwrap();
            assert_eq!(after.value, saved.value);
            assert_eq!(after.revision, saved.revision);
            assert_eq!(
                monitored_config_version(&load_config().await.unwrap()),
                version
            );
        }

        // Simulate a request admitted before a credential change: its captured
        // generation is now stale even though its Origin still matches.
        let admitted = auth.sessions.snapshot().unwrap().revision;
        let _held_owner = warnings.own_source(103, held.clone());
        let mut event = warnings.capture_event();
        warnings.mark_event(&mut event, None).unwrap();
        let _running_owner = warnings.own_source(104, running.clone());
        crate::webui::sessions::reset_password(store.clone()).unwrap();
        assert_ne!(auth.sessions.snapshot().unwrap().revision, admitted);
        for generation in [None, Some(axum::Extension(AuthGeneration(admitted)))] {
            let result = release_priority_warning(
                axum::Extension(auth.clone()),
                generation,
                headers.clone(),
                Json(ReleasePriorityWarningRequest {
                    channel_name: "示例频道 A".into(),
                    platform: None,
                }),
            )
            .await;
            assert_eq!(result.err(), Some(StatusCode::FORBIDDEN));
            assert!(warnings.snapshot().holds(&held));
            assert_eq!(warnings.active_source(), Some(running.clone()));
        }
        println!("priority release router fixture verified");
    }
}

#[cfg(test)]
mod cover_access_tests {
    use super::*;

    #[tokio::test]
    async fn cover_api_rejects_paths_before_loading_configuration() {
        for image_path in [
            "/etc/passwd",
            "../../etc/passwd",
            "data/bilistream.db",
            "data/cache/cover.jpg",
            "C:\\Users\\user\\secret.txt",
            "cover.jpg/../cookies.json",
        ] {
            let result = update_cover(Json(UpdateCoverRequest {
                image_path: image_path.into(),
            }))
            .await;
            assert_eq!(result.err(), Some(StatusCode::BAD_REQUEST), "{image_path}");
        }
    }

    #[tokio::test]
    async fn cached_cover_reads_a_regular_file_and_refuses_a_link() {
        let root =
            std::env::temp_dir().join(format!("bilistream-cover-test-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let image = root.join("cover.jpg");
        std::fs::write(&image, b"synthetic-cover-bytes").unwrap();
        assert_eq!(
            read_cached_cover(&image).await.unwrap(),
            b"synthetic-cover-bytes"
        );
        assert!(read_cached_cover(&root).await.is_err());
        #[cfg(unix)]
        {
            let link = root.join("pic_for_crop.jpg");
            std::os::unix::fs::symlink(&image, &link).unwrap();
            assert!(read_cached_cover(&link).await.is_err());
        }
        std::fs::remove_dir_all(root).unwrap();
    }
}
