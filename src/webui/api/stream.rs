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
                        "qr_url": qr_url
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
    crate::config::save_config(&cfg)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

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

    Ok(ApiResponse {
        success: true,
        data: None,
        message: Some(format!("{} 频道已更新", payload.platform)),
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

#[derive(Deserialize)]
pub struct UpdateBannedKeywordsRequest {
    danmaku_banned_keywords: Option<Vec<String>>,
    streaming_banned_keywords: Option<Vec<String>>,
}

pub async fn update_banned_keywords(
    Json(payload): Json<UpdateBannedKeywordsRequest>,
) -> Result<ApiResponse<()>, StatusCode> {
    let areas_path = std::env::current_exe()
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .with_file_name("areas.json");

    let content = tokio::fs::read_to_string(&areas_path)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let mut data: serde_json::Value =
        serde_json::from_str(&content).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    if let Some(danmaku_keywords) = payload.danmaku_banned_keywords {
        data["banned_keywords"] = serde_json::json!(danmaku_keywords);
    }

    if let Some(streaming_keywords) = payload.streaming_banned_keywords {
        data["streaming_banned_keywords"] = serde_json::json!(streaming_keywords);
    }

    let updated_content =
        serde_json::to_string_pretty(&data).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    tokio::fs::write(&areas_path, updated_content)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

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

    cfg.youtube.enable_monitor = payload.enabled;

    crate::config::save_config(&cfg)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    set_config_updated();
    refresh_status_cache_config_from(&cfg);
    crate::webui::state::request_status_refresh();

    Ok(ApiResponse {
        success: true,
        data: None,
        message: Some(format!(
            "YouTube监控已{}",
            if payload.enabled { "启用" } else { "禁用" }
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

    cfg.twitch.enable_monitor = payload.enabled;

    crate::config::save_config(&cfg)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    set_config_updated();
    refresh_status_cache_config_from(&cfg);
    crate::webui::state::request_status_refresh();

    Ok(ApiResponse {
        success: true,
        data: None,
        message: Some(format!(
            "Twitch监控已{}",
            if payload.enabled { "启用" } else { "禁用" }
        )),
    })
}
