use axum::{
    extract::Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;

use crate::config::load_config;
use crate::plugins::{
    bili_change_live_title, bili_start_live, bili_stop_live, bili_update_area, bilibili,
    get_bili_live_status, get_ffmpeg_speed, send_danmaku as send_danmaku_to_bili,
    set_config_updated,
};
use crate::updater;

// Global log buffer
static LOG_BUFFER: Mutex<Option<VecDeque<String>>> = Mutex::new(None);

// Global status cache updated by main loop
static STATUS_CACHE: Mutex<Option<StatusData>> = Mutex::new(None);

pub fn init_log_buffer() {
    let mut buffer = LOG_BUFFER.lock().unwrap();
    *buffer = Some(VecDeque::with_capacity(500));
}

pub fn add_log_line(line: String) {
    let mut buffer = LOG_BUFFER.lock().unwrap();
    if let Some(ref mut buf) = *buffer {
        buf.push_back(line);
        if buf.len() > 500 {
            buf.pop_front();
        }
    }
}

pub fn get_logs() -> Vec<String> {
    let buffer = LOG_BUFFER.lock().unwrap();
    if let Some(ref buf) = *buffer {
        buf.iter().cloned().collect()
    } else {
        Vec::new()
    }
}

pub fn update_status_cache(status: StatusData) {
    let mut cache = STATUS_CACHE.lock().unwrap();
    *cache = Some(status);
}

pub fn get_status_cache() -> Option<StatusData> {
    let cache = STATUS_CACHE.lock().unwrap();
    cache.clone()
}

// Update status cache with fresh configuration data (config fields only)
pub async fn refresh_status_cache_config() {
    if let Ok(cfg) = load_config().await {
        let mut cached_status = get_status_cache().unwrap_or_default();

        // Update YouTube status with fresh config (preserve live status)
        if !cfg.youtube.channel_id.is_empty() {
            let yt_area_name = crate::plugins::get_area_name(cfg.youtube.area_v2)
                .unwrap_or_else(|| format!("未知分区 (ID: {})", cfg.youtube.area_v2));

            if let Some(ref mut yt_status) = cached_status.youtube {
                // Update only configuration fields, preserve live status
                yt_status.channel_name = cfg.youtube.channel_name.clone();
                yt_status.channel_id = cfg.youtube.channel_id.clone();
                yt_status.area_id = cfg.youtube.area_v2;
                yt_status.area_name = yt_area_name;
                yt_status.quality = cfg.youtube.quality.clone();
                // Keep existing: is_live, title, topic
            } else {
                // Create new status entry with default live status
                cached_status.youtube = Some(YtStatus {
                    is_live: false,
                    title: Some("-".to_string()),
                    channel_name: cfg.youtube.channel_name.clone(),
                    channel_id: cfg.youtube.channel_id.clone(),
                    area_id: cfg.youtube.area_v2,
                    area_name: yt_area_name,
                    topic: Some("-".to_string()),
                    quality: cfg.youtube.quality.clone(),
                });
            }
        }

        // Update Twitch status with fresh config (preserve live status)
        if !cfg.twitch.channel_id.is_empty() {
            let tw_area_name = crate::plugins::get_area_name(cfg.twitch.area_v2)
                .unwrap_or_else(|| format!("未知分区 (ID: {})", cfg.twitch.area_v2));

            if let Some(ref mut tw_status) = cached_status.twitch {
                // Update only configuration fields, preserve live status
                tw_status.channel_name = cfg.twitch.channel_name.clone();
                tw_status.channel_id = cfg.twitch.channel_id.clone();
                tw_status.area_id = cfg.twitch.area_v2;
                tw_status.area_name = tw_area_name;
                tw_status.quality = cfg.twitch.quality.clone();
                // Keep existing: is_live, title, game
            } else {
                // Create new status entry with default live status
                cached_status.twitch = Some(TwStatus {
                    is_live: false,
                    title: Some("-".to_string()),
                    channel_name: cfg.twitch.channel_name.clone(),
                    channel_id: cfg.twitch.channel_id.clone(),
                    area_id: cfg.twitch.area_v2,
                    area_name: tw_area_name,
                    game: Some("-".to_string()),
                    quality: cfg.twitch.quality.clone(),
                });
            }
        }

        update_status_cache(cached_status);
    }
}

// Refresh live status in background (like refresh buttons)
pub async fn refresh_live_status_background() {
    // Spawn background tasks to refresh live status without blocking
    tokio::spawn(async {
        let _ = refresh_youtube_status().await;
    });

    tokio::spawn(async {
        let _ = refresh_twitch_status().await;
    });
}

#[derive(Serialize)]
pub struct ApiResponse<T> {
    success: bool,
    data: Option<T>,
    message: Option<String>,
}

impl<T: Serialize> IntoResponse for ApiResponse<T> {
    fn into_response(self) -> Response {
        Json(self).into_response()
    }
}

#[derive(Serialize, Clone, Default)]
pub struct StatusData {
    pub bilibili: BiliStatus,
    pub youtube: Option<YtStatus>,
    pub twitch: Option<TwStatus>,
}

#[derive(Serialize, Clone, Default)]
pub struct BiliStatus {
    pub is_live: bool,
    pub title: String,
    pub area_id: u64,
    pub area_name: String,
    pub stream_quality: Option<String>,
    pub stream_speed: Option<f32>,
}

#[derive(Serialize, Clone)]
pub struct YtStatus {
    pub is_live: bool,
    pub title: Option<String>,
    pub topic: Option<String>,
    pub channel_name: String,
    pub channel_id: String,
    pub quality: String,
    pub area_id: u64,
    pub area_name: String,
}

#[derive(Serialize, Clone)]
pub struct TwStatus {
    pub is_live: bool,
    pub title: Option<String>,
    pub game: Option<String>,
    pub channel_name: String,
    pub channel_id: String,
    pub quality: String,
    pub area_id: u64,
    pub area_name: String,
}

pub async fn get_status() -> impl IntoResponse {
    // Always fetch fresh Bilibili status for accurate real-time updates
    let cfg = match load_config().await {
        Ok(cfg) => cfg,
        Err(e) => {
            // Only log error if it's not a "file not found" error (expected on first run)
            let is_not_found = e.to_string().contains("No such file");
            if !is_not_found {
                tracing::error!("Failed to load config: {}", e);
            }

            let error_msg = if e.to_string().contains("Permission denied") {
                format!("配置文件权限错误: {}。请确保 config.json 文件存在且有读取权限，或在可执行文件所在目录运行程序。", e)
            } else if is_not_found {
                "配置文件不存在，请完成首次设置".to_string()
            } else {
                format!("配置加载失败: {}", e)
            };
            return (
                StatusCode::OK,
                Json(ApiResponse::<()> {
                    success: false,
                    data: None,
                    message: Some(error_msg),
                }),
            )
                .into_response();
        }
    };

    // Fetch fresh Bilibili status
    let (bili_is_live, bili_title, bili_area_id) =
        match get_bili_live_status(cfg.bililive.room).await {
            Ok(status) => status,
            Err(e) => {
                tracing::error!("Failed to get Bilibili status: {}", e);
                return (
                    StatusCode::OK,
                    Json(ApiResponse::<()> {
                        success: false,
                        data: None,
                        message: Some(format!("获取B站状态失败: {}", e)),
                    }),
                )
                    .into_response();
            }
        };

    let bili_area_name = crate::plugins::get_area_name(bili_area_id)
        .unwrap_or_else(|| format!("未知分区 (ID: {})", bili_area_id));

    // Get ffmpeg speed and calculate stream quality
    let stream_speed = get_ffmpeg_speed().await;
    let stream_quality = if bili_is_live {
        stream_speed.map(|speed| {
            if speed > 0.97 {
                "流畅".to_string()
            } else if speed > 0.94 {
                "波动".to_string()
            } else {
                "卡顿".to_string()
            }
        })
    } else {
        None
    };

    // Get YouTube/Twitch status from cache (updated by main loop)
    // This avoids expensive yt-dlp/streamlink calls on every refresh
    // Note: Individual platform refresh buttons can trigger fresh fetches if needed
    let cached_status = get_status_cache();
    let youtube_status = cached_status.as_ref().and_then(|c| c.youtube.clone());
    let twitch_status = cached_status.as_ref().and_then(|c| c.twitch.clone());

    let status = StatusData {
        bilibili: BiliStatus {
            is_live: bili_is_live,
            title: bili_title,
            area_id: bili_area_id,
            area_name: bili_area_name,
            stream_quality,
            stream_speed,
        },
        youtube: youtube_status,
        twitch: twitch_status,
    };

    (
        StatusCode::OK,
        Json(ApiResponse {
            success: true,
            data: Some(status),
            message: None,
        }),
    )
        .into_response()
}

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
        "proxy": cfg.proxy.clone(),
        "anti_collision_list": cfg.anti_collision_list.clone(),
        "bilibili": {
            "room": cfg.bililive.room,
            "enable_danmaku_command": cfg.bililive.enable_danmaku_command,
        },
        "youtube": {
            "enable_monitor": cfg.youtube.enable_monitor,
            "channel_name": cfg.youtube.channel_name,
            "channel_id": cfg.youtube.channel_id,
            "area_v2": cfg.youtube.area_v2,
        },
        "twitch": {
            "enable_monitor": cfg.twitch.enable_monitor,
            "channel_name": cfg.twitch.channel_name,
            "channel_id": cfg.twitch.channel_id,
            "area_v2": cfg.twitch.area_v2,
            "proxy_region": cfg.twitch.proxy_region,
        }
    });

    Ok(Json(config_json))
}

#[derive(Deserialize)]
pub struct UpdateConfigRequest {
    interval: Option<u64>,
    auto_cover: Option<bool>,
    enable_anti_collision: Option<bool>,
    enable_lol_monitor: Option<bool>,
    lol_monitor_interval: Option<u64>,
    riot_api_key: Option<String>,
    holodex_api_key: Option<String>,
    proxy: Option<String>,
    twitch_proxy_region: Option<String>,
    anti_collision_list: Option<HashMap<String, i32>>,
    enable_danmaku_command: Option<bool>,
    youtube_enable_monitor: Option<bool>,
    twitch_enable_monitor: Option<bool>,
    youtube_cookies_from_browser: Option<String>,
    youtube_cookies_file: Option<String>,
}

pub async fn update_config(
    Json(payload): Json<UpdateConfigRequest>,
) -> Result<ApiResponse<()>, StatusCode> {
    // Load current config
    let mut cfg = load_config()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

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
    if let Some(proxy) = payload.proxy {
        if !proxy.is_empty() {
            cfg.proxy = Some(proxy);
        } else {
            cfg.proxy = None;
        }
    }

    // Check if Twitch settings will be updated (before moving values)
    let twitch_settings_updated = payload.twitch_proxy_region.is_some();

    if let Some(anti_collision_list) = payload.anti_collision_list {
        cfg.anti_collision_list = anti_collision_list;
    }
    if let Some(twitch_proxy_region) = payload.twitch_proxy_region {
        cfg.twitch.proxy_region = twitch_proxy_region;
    }
    if let Some(enable_danmaku_command) = payload.enable_danmaku_command {
        cfg.bililive.enable_danmaku_command = enable_danmaku_command;
    }
    if let Some(youtube_enable_monitor) = payload.youtube_enable_monitor {
        cfg.youtube.enable_monitor = youtube_enable_monitor;
    }
    if let Some(twitch_enable_monitor) = payload.twitch_enable_monitor {
        cfg.twitch.enable_monitor = twitch_enable_monitor;
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

    // Save config
    crate::config::save_config(&cfg)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    // Set config updated flag so main loop can detect the change
    set_config_updated();

    // Refresh status cache with updated configuration (for Twitch settings)
    refresh_status_cache_config().await;

    // Refresh Twitch live status in background if Twitch settings were updated
    if twitch_settings_updated {
        tokio::spawn(async {
            let _ = refresh_twitch_status().await;
        });
    }

    Ok(ApiResponse {
        success: true,
        data: None,
        message: Some("配置已更新".to_string()),
    })
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
                        "qr_url": qr_url
                    })),
                    message: Some("需要人脸验证，请扫描二维码完成验证后重试".to_string()),
                })
            } else {
                Err(StatusCode::INTERNAL_SERVER_ERROR)
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
    // Stop current ffmpeg process
    crate::plugins::stop_ffmpeg().await;

    // Clear any warning stops to allow restreaming
    crate::plugins::danmaku::clear_warning_stop();

    // Set manual restart flag to force immediate restart
    crate::plugins::set_manual_restart();

    // Set config updated flag to trigger main loop reload
    set_config_updated();

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

    // Set config updated flag so main loop can detect the change
    set_config_updated();

    // Refresh status cache with updated configuration
    refresh_status_cache_config().await;

    // Refresh live status in background for the specific platform only
    let platform = payload.platform.clone();
    tokio::spawn(async move {
        match platform.as_str() {
            "youtube" => {
                let _ = refresh_youtube_status().await;
            }
            "twitch" => {
                let _ = refresh_twitch_status().await;
            }
            _ => {}
        }
    });

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

    let content =
        std::fs::read_to_string(channels_path).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let channels: serde_json::Value =
        serde_json::from_str(&content).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(Json(channels))
}

pub async fn get_areas() -> Result<Json<serde_json::Value>, StatusCode> {
    let areas_path = std::env::current_exe()
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .with_file_name("areas.json");

    let content =
        std::fs::read_to_string(areas_path).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

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

    let content =
        std::fs::read_to_string(areas_path).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

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

    let content =
        std::fs::read_to_string(&areas_path).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

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

    std::fs::write(&areas_path, updated_content).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

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

    cfg.youtube.enable_monitor = payload.enabled;

    crate::config::save_config(&cfg)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    set_config_updated();

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

    cfg.twitch.enable_monitor = payload.enabled;

    crate::config::save_config(&cfg)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    set_config_updated();

    Ok(ApiResponse {
        success: true,
        data: None,
        message: Some(format!(
            "Twitch监控已{}",
            if payload.enabled { "启用" } else { "禁用" }
        )),
    })
}

#[derive(Serialize)]
pub struct SetupStatus {
    needs_setup: bool,
    missing_files: Vec<String>,
    setup_command: String,
}

pub async fn check_setup() -> Result<Json<SetupStatus>, StatusCode> {
    let exe_path = std::env::current_exe().map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let config_path = exe_path.with_file_name("config.json");
    let legacy_config_path = exe_path.with_file_name("config.yaml");
    let cookies_path = exe_path.with_file_name("cookies.json");

    let mut missing_files = Vec::new();

    // Check for config.json or config.yaml
    if !config_path.exists() && !legacy_config_path.exists() {
        missing_files.push("config.json".to_string());
    }

    if !cookies_path.exists() {
        missing_files.push("cookies.json".to_string());
    }

    let needs_setup = !missing_files.is_empty();

    // Detect platform and set appropriate command
    let setup_command = if cfg!(target_os = "windows") {
        "bilistream.exe setup".to_string()
    } else {
        "./bilistream setup".to_string()
    };

    Ok(Json(SetupStatus {
        needs_setup,
        missing_files,
        setup_command,
    }))
}

#[derive(Serialize)]
pub struct LogsResponse {
    success: bool,
    logs: String,
}

pub async fn get_logs_endpoint() -> Result<Json<LogsResponse>, StatusCode> {
    let logs = get_logs();
    let logs_text = logs.join("\n");

    Ok(Json(LogsResponse {
        success: true,
        logs: logs_text,
    }))
}

#[derive(Deserialize)]
pub struct SetupConfigRequest {
    room: i32,
    proxy: Option<String>,
    auto_cover: bool,
    enable_danmaku_command: bool,
    interval: u64,
    anti_collision: bool,
    youtube_channel_name: Option<String>,
    youtube_channel_id: Option<String>,
    youtube_area_v2: Option<u64>,
    youtube_quality: Option<String>,
    twitch_channel_name: Option<String>,
    twitch_channel_id: Option<String>,
    twitch_area_v2: Option<u64>,
    twitch_proxy_region: Option<String>,
    twitch_quality: Option<String>,
    holodex_api_key: Option<String>,
    riot_api_key: Option<String>,
    enable_lol_monitor: bool,
}

pub async fn save_setup_config(
    Json(payload): Json<SetupConfigRequest>,
) -> Result<ApiResponse<()>, StatusCode> {
    // Load existing config or create default
    let mut cfg = if let Ok(existing_cfg) = load_config().await {
        existing_cfg
    } else {
        // Create new config with defaults
        crate::config::Config {
            auto_cover: true,
            enable_anti_collision: false,
            interval: 60,
            bililive: crate::config::BiliLive {
                enable_danmaku_command: true,
                room: 0,
                bili_rtmp_url: "rtmp://live-push.bilivideo.com/live-bvc/".to_string(),
                bili_rtmp_key: String::new(),
                credentials: crate::config::Credentials::default(),
            },
            twitch: crate::config::Twitch {
                enable_monitor: true,
                channel_name: String::new(),
                area_v2: 235,
                channel_id: String::new(),
                proxy_region: "as".to_string(),
                quality: "best".to_string(),
            },
            youtube: crate::config::Youtube {
                enable_monitor: true,
                channel_name: String::new(),
                channel_id: String::new(),
                area_v2: 235,
                quality: "best".to_string(),
                cookies_file: None,
                cookies_from_browser: None,
            },
            proxy: None,
            holodex_api_key: None,
            riot_api_key: None,
            enable_lol_monitor: false,
            lol_monitor_interval: Some(1),
            anti_collision_list: std::collections::HashMap::new(),
        }
    };

    // Update only the fields from payload
    cfg.auto_cover = payload.auto_cover;
    cfg.enable_anti_collision = payload.anti_collision;
    cfg.interval = payload.interval;
    cfg.bililive.enable_danmaku_command = payload.enable_danmaku_command;
    cfg.bililive.room = payload.room;
    cfg.proxy = payload.proxy;
    cfg.holodex_api_key = payload.holodex_api_key;
    cfg.riot_api_key = payload.riot_api_key;
    cfg.enable_lol_monitor = payload.enable_lol_monitor;

    // Track which platforms were updated
    let youtube_updated = payload.youtube_channel_name.is_some()
        || payload.youtube_channel_id.is_some()
        || payload.youtube_area_v2.is_some()
        || payload.youtube_quality.is_some();

    let twitch_updated = payload.twitch_channel_name.is_some()
        || payload.twitch_channel_id.is_some()
        || payload.twitch_area_v2.is_some()
        || payload.twitch_proxy_region.is_some()
        || payload.twitch_quality.is_some();

    // Update YouTube config if provided
    if let Some(yt_name) = payload.youtube_channel_name {
        cfg.youtube.channel_name = yt_name;
    }
    if let Some(yt_id) = payload.youtube_channel_id {
        cfg.youtube.channel_id = yt_id;
    }
    if let Some(yt_area) = payload.youtube_area_v2 {
        cfg.youtube.area_v2 = yt_area;
    }
    if let Some(yt_quality) = payload.youtube_quality {
        cfg.youtube.quality = yt_quality;
    }

    // Update Twitch config if provided
    if let Some(tw_name) = payload.twitch_channel_name {
        cfg.twitch.channel_name = tw_name;
    }
    if let Some(tw_id) = payload.twitch_channel_id {
        cfg.twitch.channel_id = tw_id;
    }
    if let Some(tw_area) = payload.twitch_area_v2 {
        cfg.twitch.area_v2 = tw_area;
    }
    if let Some(tw_region) = payload.twitch_proxy_region {
        cfg.twitch.proxy_region = tw_region;
    }
    if let Some(tw_quality) = payload.twitch_quality {
        cfg.twitch.quality = tw_quality;
    }

    // Save config
    crate::config::save_config(&cfg)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    // Set config updated flag so main loop can detect the change
    set_config_updated();

    // Refresh status cache with updated configuration
    refresh_status_cache_config().await;

    // Refresh live status in background for only the updated platforms
    tokio::spawn(async move {
        if youtube_updated {
            let _ = refresh_youtube_status().await;
        }
        if twitch_updated {
            let _ = refresh_twitch_status().await;
        }
    });

    Ok(ApiResponse {
        success: true,
        data: None,
        message: Some("配置已保存".to_string()),
    })
}

#[derive(Serialize)]
pub struct LoginStatusResponse {
    logged_in: bool,
    message: String,
}

pub async fn check_login_status() -> Result<Json<LoginStatusResponse>, StatusCode> {
    let cookies_path = std::env::current_exe()
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .with_file_name("cookies.json");

    let logged_in = cookies_path.exists();
    let message = if logged_in {
        "已登录".to_string()
    } else {
        "未登录".to_string()
    };

    Ok(Json(LoginStatusResponse { logged_in, message }))
}

pub async fn trigger_login() -> Result<ApiResponse<String>, StatusCode> {
    // Trigger Bilibili login
    match bilibili::login().await {
        Ok(_) => Ok(ApiResponse {
            success: true,
            data: Some("登录成功".to_string()),
            message: Some("Bilibili 登录成功".to_string()),
        }),
        Err(e) => Ok(ApiResponse {
            success: false,
            data: None,
            message: Some(format!("登录失败: {}", e)),
        }),
    }
}

#[derive(Serialize)]
pub struct QrCodeResponse {
    qr_url: String,
    auth_code: String,
}

pub async fn get_qr_code() -> Result<Json<ApiResponse<QrCodeResponse>>, StatusCode> {
    match bilibili::get_login_qrcode().await {
        Ok((qr_url, auth_code)) => Ok(Json(ApiResponse {
            success: true,
            data: Some(QrCodeResponse { qr_url, auth_code }),
            message: None,
        })),
        Err(e) => Ok(Json(ApiResponse {
            success: false,
            data: None,
            message: Some(format!("获取二维码失败: {}", e)),
        })),
    }
}

#[derive(Deserialize)]
pub struct PollLoginRequest {
    auth_code: String,
}

#[derive(Serialize)]
pub struct PollLoginResponse {
    status: String, // "waiting", "success", "expired", "error"
    message: String,
}

pub async fn poll_login(
    Json(payload): Json<PollLoginRequest>,
) -> Result<Json<ApiResponse<PollLoginResponse>>, StatusCode> {
    match bilibili::poll_login_status(&payload.auth_code).await {
        Ok(status) => {
            let (status_str, message) = match status.as_str() {
                "success" => ("success", "登录成功"),
                "waiting" => ("waiting", "等待扫码..."),
                "expired" => ("expired", "二维码已过期"),
                _ => ("error", "未知状态"),
            };
            Ok(Json(ApiResponse {
                success: true,
                data: Some(PollLoginResponse {
                    status: status_str.to_string(),
                    message: message.to_string(),
                }),
                message: None,
            }))
        }
        Err(e) => Ok(Json(ApiResponse {
            success: false,
            data: None,
            message: Some(format!("轮询登录状态失败: {}", e)),
        })),
    }
}

// Update check endpoint
pub async fn check_updates() -> Result<Json<ApiResponse<updater::UpdateInfo>>, StatusCode> {
    match updater::check_for_updates().await {
        Ok(update_info) => Ok(Json(ApiResponse {
            success: true,
            data: Some(update_info),
            message: None,
        })),
        Err(e) => Ok(Json(ApiResponse {
            success: false,
            data: None,
            message: Some(format!("检查更新失败: {}", e)),
        })),
    }
}

#[derive(Deserialize)]
pub struct DownloadUpdateRequest {
    download_url: String,
}

#[derive(Serialize)]
pub struct DownloadProgress {
    downloaded: u64,
    total: u64,
    percentage: f32,
    status: String,
}

// Download and install update endpoint
pub async fn download_update(
    Json(payload): Json<DownloadUpdateRequest>,
) -> Result<Json<ApiResponse<String>>, StatusCode> {
    let download_url = payload.download_url;

    tracing::info!("开始下载更新: {}", download_url);

    // Spawn update task in background
    tokio::spawn(async move {
        match updater::download_and_install_update(&download_url, None).await {
            Ok(_) => {
                tracing::info!("✅ 更新安装成功！程序将在 3 秒后重启...");

                // Perform graceful shutdown before restarting
                tracing::info!("🛑 执行优雅关闭...");
                crate::plugins::stop_ffmpeg().await;

                tokio::time::sleep(tokio::time::Duration::from_secs(3)).await;

                // Restart the program
                #[cfg(target_os = "windows")]
                {
                    let exe_dir = std::env::current_exe()
                        .unwrap()
                        .parent()
                        .unwrap()
                        .to_path_buf();
                    let restart_script = exe_dir.join("restart_after_update.bat");
                    if restart_script.exists() {
                        let _ = std::process::Command::new("cmd")
                            .args(&["/C", "start", "", restart_script.to_str().unwrap()])
                            .spawn();
                    }
                }

                #[cfg(not(target_os = "windows"))]
                {
                    // Create a restart script that kills old process and starts new one
                    let exe_dir = std::env::current_exe()
                        .unwrap()
                        .parent()
                        .unwrap()
                        .to_path_buf();
                    let restart_script = exe_dir.join("restart_after_update.sh");
                    let new_exe = exe_dir.join("bilistream");
                    let old_exe = exe_dir.join("bilistream.old");

                    let script_content = format!(
                        r#"#!/bin/bash
# Wait for current process to exit
sleep 2

# Kill any remaining old process (but keep the file as backup)
if [ -f "{}" ]; then
    pkill -f "{}" 2>/dev/null || true
fi

# Wait for port to be released
sleep 1

# Start new version
"{}" &

# Clean up this script
rm "$0"
"#,
                        old_exe.display(),
                        old_exe.display(),
                        new_exe.display()
                    );

                    if let Ok(_) = std::fs::write(&restart_script, script_content) {
                        let _ = std::process::Command::new("chmod")
                            .arg("+x")
                            .arg(&restart_script)
                            .output();
                        let _ = std::process::Command::new("sh")
                            .arg(&restart_script)
                            .spawn();
                    }
                }

                std::process::exit(0);
            }
            Err(e) => {
                tracing::error!("❌ 更新安装失败: {}", e);
            }
        }
    });

    Ok(Json(ApiResponse {
        success: true,
        data: Some("更新下载已开始，请查看日志了解进度".to_string()),
        message: Some("更新将在后台下载并自动安装".to_string()),
    }))
}

// Version endpoint
#[derive(Serialize)]
pub struct VersionInfo {
    version: String,
}

pub async fn get_version() -> Result<Json<ApiResponse<VersionInfo>>, StatusCode> {
    Ok(Json(ApiResponse {
        success: true,
        data: Some(VersionInfo {
            version: env!("CARGO_PKG_VERSION").to_string(),
        }),
        message: None,
    }))
}

// Get dependency download status
pub async fn get_deps_status() -> impl IntoResponse {
    let (progress, total, message) = crate::deps::get_download_progress();
    let in_progress = crate::deps::is_download_in_progress();
    let complete = crate::deps::is_download_complete();

    Json(json!({
        "in_progress": in_progress,
        "complete": complete,
        "progress": progress,
        "total": total,
        "message": message
    }))
}

// Holodex API - Get live/upcoming streams

#[derive(Serialize, Debug)]
pub struct HolodexStreamWithArea {
    pub id: String,
    pub title: String,
    pub stream_type: String,
    pub topic_id: Option<String>,
    pub status: String,
    pub start_scheduled: Option<String>,
    pub start_actual: Option<String>,
    pub live_viewers: Option<i32>,
    pub channel_id: String,
    pub channel_name: String,
    pub suggested_area_id: Option<u64>,
    pub suggested_area_name: Option<String>,
}

pub async fn api_get_holodex_streams() -> impl IntoResponse {
    let cfg = match load_config().await {
        Ok(c) => c,
        Err(e) => {
            return Json(json!({
                "success": false,
                "message": format!("Failed to load config: {}", e)
            }));
        }
    };

    // Collect all channel IDs from channels.json
    let mut channel_ids = Vec::new();

    // Load channels.json for all channels
    let channels_path = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|p| p.join("channels.json")));

    if let Some(path) = channels_path {
        if let Ok(channels_content) = tokio::fs::read_to_string(path).await {
            if let Ok(channels_json) = serde_json::from_str::<serde_json::Value>(&channels_content)
            {
                // Try new format: channels[].platforms.youtube
                if let Some(channels) = channels_json.get("channels").and_then(|v| v.as_array()) {
                    for channel in channels {
                        if let Some(platforms) = channel.get("platforms") {
                            if let Some(yt_id) = platforms.get("youtube").and_then(|v| v.as_str()) {
                                if !yt_id.is_empty() && !channel_ids.contains(&yt_id.to_string()) {
                                    channel_ids.push(yt_id.to_string());
                                }
                            }
                        }
                    }
                }
                // Try old format: YT_channels[].channel_id (for backward compatibility)
                else if let Some(yt_channels) =
                    channels_json.get("YT_channels").and_then(|v| v.as_array())
                {
                    for channel in yt_channels {
                        if let Some(id) = channel.get("channel_id").and_then(|v| v.as_str()) {
                            if !id.is_empty() && !channel_ids.contains(&id.to_string()) {
                                channel_ids.push(id.to_string());
                            }
                        }
                    }
                }
            }
        }
    }

    // Also add the currently configured channel if not already in list
    if !cfg.youtube.channel_id.is_empty() && !channel_ids.contains(&cfg.youtube.channel_id) {
        channel_ids.push(cfg.youtube.channel_id.clone());
    }

    if channel_ids.is_empty() {
        return Json(json!({
            "success": false,
            "message": "No YouTube channels configured"
        }));
    }

    // Call the new Holodex function from youtube.rs
    let streams = match crate::plugins::youtube::get_holodex_streams(channel_ids.clone()).await {
        Ok(s) => s,
        Err(e) => {
            return Json(json!({
                "success": false,
                "message": format!("Failed to fetch from Holodex: {}", e)
            }));
        }
    };

    // Filter: if a channel is live, omit its scheduled streams
    use std::collections::HashSet;
    let mut live_channels: HashSet<String> = HashSet::new();

    // Create a set of queried channel IDs for filtering collab streams
    let queried_channels: HashSet<String> = channel_ids.iter().cloned().collect();

    // First pass: collect all channels that are currently live
    for stream in &streams {
        if stream.status == "live" {
            live_channels.insert(stream.channel.id.clone());
        }
    }

    // Second pass: filter out scheduled streams for channels that are live
    // Also filter out scheduled streams more than 30 hours in the future
    // Also filter out collab streams (where the channel is not in our query list)
    let now = chrono::Utc::now();
    let thirty_hours_later = now + chrono::Duration::hours(30);

    let filtered_streams: Vec<_> = streams
        .into_iter()
        .filter(|stream| {
            // Only keep streams from channels we explicitly queried
            // This filters out collab streams where our channel appears as a guest
            if !queried_channels.contains(&stream.channel.id) {
                return false;
            }

            // Keep live streams
            if stream.status == "live" {
                return true;
            }

            // Keep scheduled streams only if channel is not currently live
            if live_channels.contains(&stream.channel.id) {
                return false;
            }

            // Filter scheduled streams by time (within 30 hours)
            if stream.status == "upcoming" {
                if let Some(ref scheduled_time) = stream.start_scheduled {
                    if let Ok(scheduled) = chrono::DateTime::parse_from_rfc3339(scheduled_time) {
                        let scheduled_utc = scheduled.with_timezone(&chrono::Utc);
                        // Only keep if scheduled within next 30 hours
                        return scheduled_utc <= thirty_hours_later;
                    }
                }
                // If we can't parse the time, keep it to be safe
                return true;
            }

            true
        })
        .collect();

    // Add area detection for each stream
    let streams_with_area: Vec<HolodexStreamWithArea> = filtered_streams
        .into_iter()
        .map(|stream| {
            let title_for_detection = if let Some(ref topic) = stream.topic_id {
                format!("{} {}", topic, stream.title)
            } else {
                stream.title.clone()
            };

            // Check if topic_id suggests 萌宅领域 (530)
            let mut suggested_area_id = 235; // Default to 其他单机
            if let Some(ref topic) = stream.topic_id {
                let topic_lower = topic.to_lowercase();
                if topic_lower.contains("freechat")
                    || topic_lower.contains("talk")
                    || topic_lower.contains("singing")
                {
                    suggested_area_id = 530; // 萌宅领域
                }
            }

            // If not matched by topic, check title
            if suggested_area_id == 235 {
                suggested_area_id =
                    crate::plugins::check_area_id_with_title(&title_for_detection, 235);
            }

            let suggested_area_name = if suggested_area_id != 235 {
                crate::plugins::get_area_name(suggested_area_id)
            } else {
                None
            };

            HolodexStreamWithArea {
                id: stream.id,
                title: stream.title,
                stream_type: stream.stream_type,
                topic_id: stream.topic_id,
                status: stream.status,
                start_scheduled: stream.start_scheduled,
                start_actual: stream.start_actual,
                live_viewers: stream.live_viewers,
                channel_id: stream.channel.id,
                channel_name: stream.channel.name,
                suggested_area_id: if suggested_area_id != 235 {
                    Some(suggested_area_id)
                } else {
                    None
                },
                suggested_area_name,
            }
        })
        .collect();

    Json(json!({
        "success": true,
        "data": streams_with_area
    }))
}

// Switch to a Holodex stream
#[derive(Deserialize)]
pub struct SwitchToHolodexStream {
    pub channel_id: String,
    pub area_id: Option<u64>,
    pub title: Option<String>,
    pub topic_id: Option<String>,
    pub status: Option<String>,
}

pub async fn switch_to_holodex_stream(
    Json(payload): Json<SwitchToHolodexStream>,
) -> Result<ApiResponse<()>, StatusCode> {
    tracing::info!(
        "Switching to Holodex channel: {} (area: {:?})",
        payload.channel_id,
        payload.area_id
    );

    let mut cfg = match load_config().await {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("Failed to load config: {}", e);
            return Ok(ApiResponse {
                success: false,
                data: None,
                message: Some(format!("Failed to load config: {}", e)),
            });
        }
    };

    // Get channel info from channels.json
    let channels_path = std::env::current_exe()
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .with_file_name("channels.json");

    let channels_content = match tokio::fs::read_to_string(&channels_path).await {
        Ok(c) => c,
        Err(e) => {
            return Ok(ApiResponse {
                success: false,
                data: None,
                message: Some(format!("Failed to read channels.json: {}", e)),
            });
        }
    };

    let channels_json: serde_json::Value = match serde_json::from_str(&channels_content) {
        Ok(j) => j,
        Err(e) => {
            return Ok(ApiResponse {
                success: false,
                data: None,
                message: Some(format!("Failed to parse channels.json: {}", e)),
            });
        }
    };

    // Find channel name - try both new and old formats
    let mut channel_name = None;

    // Try new format: channels[].platforms.youtube
    if let Some(channels) = channels_json.get("channels").and_then(|v| v.as_array()) {
        for channel in channels {
            if let Some(platforms) = channel.get("platforms") {
                if let Some(yt_id) = platforms.get("youtube").and_then(|v| v.as_str()) {
                    if yt_id == payload.channel_id {
                        channel_name = channel
                            .get("name")
                            .and_then(|v| v.as_str())
                            .map(|s| s.to_string());
                        break;
                    }
                }
            }
        }
    }

    // Try old format if not found: YT_channels[].channel_id
    if channel_name.is_none() {
        if let Some(yt_channels) = channels_json.get("YT_channels").and_then(|v| v.as_array()) {
            for channel in yt_channels {
                if let Some(id) = channel.get("channel_id").and_then(|v| v.as_str()) {
                    if id == payload.channel_id {
                        channel_name = channel
                            .get("channel_name")
                            .and_then(|v| v.as_str())
                            .map(|s| s.to_string());
                        break;
                    }
                }
            }
        }
    }

    // If channel name not found in channels.json, fetch from Holodex API
    if channel_name.is_none() {
        if let Some(ref api_key) = cfg.holodex_api_key {
            if !api_key.is_empty() {
                let url = format!("https://holodex.net/api/v2/channels/{}", payload.channel_id);
                let client = reqwest::Client::new();
                if let Ok(response) = client.get(&url).header("X-APIKEY", api_key).send().await {
                    if response.status().is_success() {
                        if let Ok(channel_data) = response.json::<serde_json::Value>().await {
                            channel_name = channel_data
                                .get("name")
                                .and_then(|v| v.as_str())
                                .map(|s| s.to_string());
                        }
                    }
                }
            }
        }
    }

    let channel_name = channel_name.unwrap_or_else(|| payload.channel_id.clone());

    // Update config
    cfg.youtube.channel_id = payload.channel_id.clone();
    cfg.youtube.channel_name = channel_name;

    if let Some(area_id) = payload.area_id {
        cfg.youtube.area_v2 = area_id;
    }

    // Save config as JSON
    if let Err(e) = crate::config::save_config(&cfg).await {
        tracing::error!("Failed to save config: {}", e);
        return Ok(ApiResponse {
            success: false,
            data: None,
            message: Some(format!("Failed to save config: {}", e)),
        });
    }

    tracing::info!(
        "Successfully switched to channel: {} ({})",
        cfg.youtube.channel_name,
        cfg.youtube.channel_id
    );

    // Notify main loop to reload config
    set_config_updated();

    // Use stream data from Holodex monitor (passed from frontend)
    let is_live = payload
        .status
        .as_ref()
        .filter(|s| !s.is_empty()) // Filter out empty strings
        .map(|s| s.to_lowercase() == "live")
        .unwrap_or(false);
    let stream_title = payload.title.unwrap_or_else(|| "未知标题".to_string());
    let stream_topic = payload.topic_id.unwrap_or_else(|| "未知".to_string());

    // Update YouTube status cache immediately with stream data from Holodex monitor
    let mut current_cache = get_status_cache().unwrap_or_default();

    let yt_area_name = crate::plugins::get_area_name(cfg.youtube.area_v2)
        .unwrap_or_else(|| format!("未知分区 (ID: {})", cfg.youtube.area_v2));

    current_cache.youtube = Some(YtStatus {
        is_live, // From Holodex monitor data
        title: Some(stream_title.clone()),
        topic: Some(stream_topic),
        channel_name: cfg.youtube.channel_name.clone(),
        channel_id: cfg.youtube.channel_id.clone(),
        quality: cfg.youtube.quality.clone(),
        area_id: cfg.youtube.area_v2,
        area_name: yt_area_name,
    });

    update_status_cache(current_cache);

    Ok(ApiResponse {
        success: true,
        data: Some(()),
        message: Some(format!(
            "已切换到 {} (分区: {}) - {}",
            cfg.youtube.channel_name,
            cfg.youtube.area_v2,
            if is_live { "直播中" } else { "预定直播" }
        )),
    })
}

// Refresh YouTube status (fetch fresh data and update cache)
pub async fn refresh_youtube_status() -> Json<ApiResponse<()>> {
    let cfg = match load_config().await {
        Ok(c) => c,
        Err(e) => {
            return Json(ApiResponse {
                success: false,
                data: None,
                message: Some(format!("Failed to load config: {}", e)),
            });
        }
    };

    if cfg.youtube.channel_id.is_empty() {
        return Json(ApiResponse {
            success: false,
            data: None,
            message: Some("YouTube channel not configured".to_string()),
        });
    }

    // Fetch fresh YouTube status using Holodex API directly
    let streams =
        match crate::plugins::youtube::get_holodex_streams(vec![cfg.youtube.channel_id.clone()])
            .await
        {
            Ok(s) => s,
            Err(e) => {
                return Json(ApiResponse {
                    success: false,
                    data: None,
                    message: Some(format!("Failed to get YouTube status: {}", e)),
                });
            }
        };

    // Find the stream for this channel, prioritizing live streams over upcoming ones
    let (yt_is_live, yt_area, yt_title) = {
        let channel_streams: Vec<_> = streams
            .iter()
            .filter(|s| s.channel.id == cfg.youtube.channel_id)
            .collect();

        if channel_streams.is_empty() {
            // No streams found for this channel
            (false, None, None)
        } else {
            // First try to find a live stream
            if let Some(live_stream) = channel_streams.iter().find(|s| s.status == "live") {
                let topic = live_stream.topic_id.clone();
                let title = Some(live_stream.title.clone());
                (true, topic, title)
            } else {
                // No live stream, check for upcoming streams
                if let Some(upcoming_stream) =
                    channel_streams.iter().find(|s| s.status == "upcoming")
                {
                    let topic = upcoming_stream.topic_id.clone();
                    let title = Some(upcoming_stream.title.clone());
                    (false, topic, title)
                } else {
                    // No live or upcoming streams
                    (false, None, None)
                }
            }
        }
    };

    // Get current cache and update only YouTube part
    let mut current_cache = get_status_cache().unwrap_or_else(|| {
        // Create default cache if none exists
        StatusData {
            bilibili: BiliStatus {
                is_live: false,
                title: String::new(),
                area_id: 0,
                area_name: String::new(),
                stream_quality: None,
                stream_speed: None,
            },
            youtube: None,
            twitch: None,
        }
    });

    current_cache.youtube = Some(YtStatus {
        is_live: yt_is_live,
        title: yt_title,
        topic: yt_area,
        channel_name: cfg.youtube.channel_name.clone(),
        channel_id: cfg.youtube.channel_id.clone(),
        quality: cfg.youtube.quality.clone(),
        area_id: cfg.youtube.area_v2,
        area_name: crate::plugins::get_area_name(cfg.youtube.area_v2)
            .unwrap_or_else(|| format!("未知分区 (ID: {})", cfg.youtube.area_v2)),
    });

    update_status_cache(current_cache);

    Json(ApiResponse {
        success: true,
        data: Some(()),
        message: Some("YouTube status refreshed".to_string()),
    })
}

// Refresh Twitch status (fetch fresh data and update cache)
pub async fn refresh_twitch_status() -> Json<ApiResponse<()>> {
    let cfg = match load_config().await {
        Ok(c) => c,
        Err(e) => {
            return Json(ApiResponse {
                success: false,
                data: None,
                message: Some(format!("Failed to load config: {}", e)),
            });
        }
    };

    if cfg.twitch.channel_id.is_empty() {
        return Json(ApiResponse {
            success: false,
            data: None,
            message: Some("Twitch channel not configured".to_string()),
        });
    }

    // Fetch fresh Twitch status using get_twitch_status
    let (tw_is_live, tw_area, tw_title, _) =
        match crate::plugins::get_twitch_status(&cfg.twitch.channel_id).await {
            Ok(status) => status,
            Err(e) => {
                return Json(ApiResponse {
                    success: false,
                    data: None,
                    message: Some(format!("Failed to get Twitch status: {}", e)),
                });
            }
        };

    // Get current cache and update only Twitch part
    let mut current_cache = get_status_cache().unwrap_or_else(|| {
        // Create default cache if none exists
        StatusData {
            bilibili: BiliStatus {
                is_live: false,
                title: String::new(),
                area_id: 0,
                area_name: String::new(),
                stream_quality: None,
                stream_speed: None,
            },
            youtube: None,
            twitch: None,
        }
    });

    current_cache.twitch = Some(TwStatus {
        is_live: tw_is_live,
        title: tw_title,
        game: tw_area,
        channel_name: cfg.twitch.channel_name.clone(),
        channel_id: cfg.twitch.channel_id.clone(),
        quality: cfg.twitch.quality.clone(),
        area_id: cfg.twitch.area_v2,
        area_name: crate::plugins::get_area_name(cfg.twitch.area_v2)
            .unwrap_or_else(|| format!("未知分区 (ID: {})", cfg.twitch.area_v2)),
    });

    update_status_cache(current_cache);

    Json(ApiResponse {
        success: true,
        data: Some(()),
        message: Some("Twitch status refreshed".to_string()),
    })
}

// Data structures for area and channel management
#[derive(Serialize, Deserialize, Debug)]
pub struct Area {
    pub id: u32,
    pub name: String,
    pub title_keywords: Vec<String>,
    pub aliases: Vec<String>,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct AreasData {
    pub banned_keywords: Vec<String>,
    pub areas: Vec<Area>,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct Channel {
    pub name: String,
    pub aliases: Vec<String>,
    pub platforms: HashMap<String, String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub riot_puuid: Option<String>,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct ChannelsData {
    pub channels: Vec<Channel>,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct AddAreaRequest {
    pub id: u32,
    pub name: String,
    pub title_keywords: Vec<String>,
    pub aliases: Vec<String>,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct AddChannelRequest {
    pub name: String,
    pub aliases: Vec<String>,
    pub platforms: HashMap<String, String>,
    pub riot_puuid: Option<String>,
}

// Get all areas
pub async fn get_areas_manage() -> Json<ApiResponse<AreasData>> {
    match std::fs::read_to_string("areas.json") {
        Ok(data) => match serde_json::from_str::<AreasData>(&data) {
            Ok(areas) => Json(ApiResponse {
                success: true,
                data: Some(areas),
                message: None,
            }),
            Err(e) => Json(ApiResponse {
                success: false,
                data: None,
                message: Some(format!("Failed to parse areas.json: {}", e)),
            }),
        },
        Err(e) => Json(ApiResponse {
            success: false,
            data: None,
            message: Some(format!("Failed to read areas.json: {}", e)),
        }),
    }
}

// Add new area
pub async fn add_area(Json(payload): Json<AddAreaRequest>) -> Json<ApiResponse<()>> {
    // Read current areas
    let mut areas_data = match std::fs::read_to_string("areas.json") {
        Ok(data) => match serde_json::from_str::<AreasData>(&data) {
            Ok(areas) => areas,
            Err(e) => {
                return Json(ApiResponse {
                    success: false,
                    data: None,
                    message: Some(format!("Failed to parse areas.json: {}", e)),
                });
            }
        },
        Err(e) => {
            return Json(ApiResponse {
                success: false,
                data: None,
                message: Some(format!("Failed to read areas.json: {}", e)),
            });
        }
    };

    // Check if area ID already exists
    if areas_data.areas.iter().any(|a| a.id == payload.id) {
        return Json(ApiResponse {
            success: false,
            data: None,
            message: Some(format!("Area with ID {} already exists", payload.id)),
        });
    }

    // Add new area
    areas_data.areas.push(Area {
        id: payload.id,
        name: payload.name,
        title_keywords: payload.title_keywords,
        aliases: payload.aliases,
    });

    // Sort areas by ID
    areas_data.areas.sort_by_key(|a| a.id);

    // Write back to file
    match serde_json::to_string_pretty(&areas_data) {
        Ok(json_str) => match std::fs::write("areas.json", json_str) {
            Ok(_) => Json(ApiResponse {
                success: true,
                data: Some(()),
                message: Some("Area added successfully".to_string()),
            }),
            Err(e) => Json(ApiResponse {
                success: false,
                data: None,
                message: Some(format!("Failed to write areas.json: {}", e)),
            }),
        },
        Err(e) => Json(ApiResponse {
            success: false,
            data: None,
            message: Some(format!("Failed to serialize areas data: {}", e)),
        }),
    }
}

// Get all channels
pub async fn get_channels_manage() -> Json<ApiResponse<ChannelsData>> {
    match std::fs::read_to_string("channels.json") {
        Ok(data) => match serde_json::from_str::<ChannelsData>(&data) {
            Ok(channels) => Json(ApiResponse {
                success: true,
                data: Some(channels),
                message: None,
            }),
            Err(e) => Json(ApiResponse {
                success: false,
                data: None,
                message: Some(format!("Failed to parse channels.json: {}", e)),
            }),
        },
        Err(e) => Json(ApiResponse {
            success: false,
            data: None,
            message: Some(format!("Failed to read channels.json: {}", e)),
        }),
    }
}

// Add new channel
pub async fn add_channel(Json(payload): Json<AddChannelRequest>) -> Json<ApiResponse<()>> {
    // Validate that at least one platform is provided
    if payload.platforms.is_empty() {
        return Json(ApiResponse {
            success: false,
            data: None,
            message: Some(
                "At least one platform (YouTube or Twitch) must be specified".to_string(),
            ),
        });
    }

    // Read current channels
    let mut channels_data = match std::fs::read_to_string("channels.json") {
        Ok(data) => match serde_json::from_str::<ChannelsData>(&data) {
            Ok(channels) => channels,
            Err(e) => {
                return Json(ApiResponse {
                    success: false,
                    data: None,
                    message: Some(format!("Failed to parse channels.json: {}", e)),
                });
            }
        },
        Err(e) => {
            return Json(ApiResponse {
                success: false,
                data: None,
                message: Some(format!("Failed to read channels.json: {}", e)),
            });
        }
    };

    // Check if channel name already exists
    if channels_data
        .channels
        .iter()
        .any(|c| c.name == payload.name)
    {
        return Json(ApiResponse {
            success: false,
            data: None,
            message: Some(format!("Channel '{}' already exists", payload.name)),
        });
    }

    // Add new channel
    channels_data.channels.push(Channel {
        name: payload.name,
        aliases: payload.aliases,
        platforms: payload.platforms,
        riot_puuid: payload.riot_puuid,
    });

    // Write back to file
    match serde_json::to_string_pretty(&channels_data) {
        Ok(json_str) => match std::fs::write("channels.json", json_str) {
            Ok(_) => Json(ApiResponse {
                success: true,
                data: Some(()),
                message: Some("Channel added successfully".to_string()),
            }),
            Err(e) => Json(ApiResponse {
                success: false,
                data: None,
                message: Some(format!("Failed to write channels.json: {}", e)),
            }),
        },
        Err(e) => Json(ApiResponse {
            success: false,
            data: None,
            message: Some(format!("Failed to serialize channels data: {}", e)),
        }),
    }
}

// Update existing channel
pub async fn update_channel_manage(
    Json(payload): Json<AddChannelRequest>,
) -> Json<ApiResponse<()>> {
    // Validate that at least one platform is provided
    if payload.platforms.is_empty() {
        return Json(ApiResponse {
            success: false,
            data: None,
            message: Some(
                "At least one platform (YouTube or Twitch) must be specified".to_string(),
            ),
        });
    }

    // Read current channels
    let mut channels_data = match std::fs::read_to_string("channels.json") {
        Ok(data) => match serde_json::from_str::<ChannelsData>(&data) {
            Ok(channels) => channels,
            Err(e) => {
                return Json(ApiResponse {
                    success: false,
                    data: None,
                    message: Some(format!("Failed to parse channels.json: {}", e)),
                });
            }
        },
        Err(e) => {
            return Json(ApiResponse {
                success: false,
                data: None,
                message: Some(format!("Failed to read channels.json: {}", e)),
            });
        }
    };

    // Find and update the channel
    if let Some(channel) = channels_data
        .channels
        .iter_mut()
        .find(|c| c.name == payload.name)
    {
        channel.aliases = payload.aliases;
        channel.platforms = payload.platforms;
        channel.riot_puuid = payload.riot_puuid;

        // Write back to file
        match serde_json::to_string_pretty(&channels_data) {
            Ok(json_str) => match std::fs::write("channels.json", json_str) {
                Ok(_) => Json(ApiResponse {
                    success: true,
                    data: Some(()),
                    message: Some("Channel updated successfully".to_string()),
                }),
                Err(e) => Json(ApiResponse {
                    success: false,
                    data: None,
                    message: Some(format!("Failed to write channels.json: {}", e)),
                }),
            },
            Err(e) => Json(ApiResponse {
                success: false,
                data: None,
                message: Some(format!("Failed to serialize channels data: {}", e)),
            }),
        }
    } else {
        Json(ApiResponse {
            success: false,
            data: None,
            message: Some(format!("Channel '{}' not found", payload.name)),
        })
    }
}
// Delete area by ID
pub async fn delete_area(
    axum::extract::Path(id): axum::extract::Path<u32>,
) -> Json<ApiResponse<()>> {
    // Read current areas
    let mut areas_data = match std::fs::read_to_string("areas.json") {
        Ok(data) => match serde_json::from_str::<AreasData>(&data) {
            Ok(areas) => areas,
            Err(e) => {
                return Json(ApiResponse {
                    success: false,
                    data: None,
                    message: Some(format!("Failed to parse areas.json: {}", e)),
                });
            }
        },
        Err(e) => {
            return Json(ApiResponse {
                success: false,
                data: None,
                message: Some(format!("Failed to read areas.json: {}", e)),
            });
        }
    };

    // Find and remove the area
    let initial_len = areas_data.areas.len();
    areas_data.areas.retain(|area| area.id != id);

    if areas_data.areas.len() == initial_len {
        return Json(ApiResponse {
            success: false,
            data: None,
            message: Some(format!("Area with ID {} not found", id)),
        });
    }

    // Write back to file
    match serde_json::to_string_pretty(&areas_data) {
        Ok(json_str) => match std::fs::write("areas.json", json_str) {
            Ok(_) => Json(ApiResponse {
                success: true,
                data: Some(()),
                message: Some("Area deleted successfully".to_string()),
            }),
            Err(e) => Json(ApiResponse {
                success: false,
                data: None,
                message: Some(format!("Failed to write areas.json: {}", e)),
            }),
        },
        Err(e) => Json(ApiResponse {
            success: false,
            data: None,
            message: Some(format!("Failed to serialize areas data: {}", e)),
        }),
    }
}

// Delete channel by name
pub async fn delete_channel(
    axum::extract::Path(name): axum::extract::Path<String>,
) -> Json<ApiResponse<()>> {
    // Read current channels
    let mut channels_data = match std::fs::read_to_string("channels.json") {
        Ok(data) => match serde_json::from_str::<ChannelsData>(&data) {
            Ok(channels) => channels,
            Err(e) => {
                return Json(ApiResponse {
                    success: false,
                    data: None,
                    message: Some(format!("Failed to parse channels.json: {}", e)),
                });
            }
        },
        Err(e) => {
            return Json(ApiResponse {
                success: false,
                data: None,
                message: Some(format!("Failed to read channels.json: {}", e)),
            });
        }
    };

    // Find and remove the channel
    let initial_len = channels_data.channels.len();
    channels_data
        .channels
        .retain(|channel| channel.name != name);

    if channels_data.channels.len() == initial_len {
        return Json(ApiResponse {
            success: false,
            data: None,
            message: Some(format!("Channel '{}' not found", name)),
        });
    }

    // Write back to file
    match serde_json::to_string_pretty(&channels_data) {
        Ok(json_str) => match std::fs::write("channels.json", json_str) {
            Ok(_) => Json(ApiResponse {
                success: true,
                data: Some(()),
                message: Some("Channel deleted successfully".to_string()),
            }),
            Err(e) => Json(ApiResponse {
                success: false,
                data: None,
                message: Some(format!("Failed to write channels.json: {}", e)),
            }),
        },
        Err(e) => Json(ApiResponse {
            success: false,
            data: None,
            message: Some(format!("Failed to serialize channels data: {}", e)),
        }),
    }
}
