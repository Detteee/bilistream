use axum::{
    extract::Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::VecDeque;
use std::sync::Mutex;

use crate::config::load_config;
use crate::plugins::{
    bili_start_live, bili_stop_live, bili_update_area, bilibili, get_bili_live_status,
    get_ffmpeg_speed, send_danmaku as send_danmaku_to_bili, set_config_updated,
};

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

#[derive(Serialize, Clone)]
pub struct StatusData {
    pub bilibili: BiliStatus,
    pub youtube: Option<YtStatus>,
    pub twitch: Option<TwStatus>,
}

#[derive(Serialize, Clone)]
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
}

#[derive(Serialize, Clone)]
pub struct TwStatus {
    pub is_live: bool,
    pub title: Option<String>,
    pub game: Option<String>,
    pub channel_name: String,
    pub channel_id: String,
    pub quality: String,
}

fn get_area_name(area_id: u64) -> String {
    let areas_path = match std::env::current_exe() {
        Ok(path) => path.with_file_name("areas.json"),
        Err(_) => return format!("未知分区 (ID: {})", area_id),
    };

    let content = match std::fs::read_to_string(areas_path) {
        Ok(c) => c,
        Err(_) => return format!("未知分区 (ID: {})", area_id),
    };

    let areas: serde_json::Value = match serde_json::from_str(&content) {
        Ok(a) => a,
        Err(_) => return format!("未知分区 (ID: {})", area_id),
    };

    if let Some(areas_array) = areas["areas"].as_array() {
        for area in areas_array {
            if let (Some(id), Some(name)) = (area["id"].as_u64(), area["name"].as_str()) {
                if id == area_id {
                    return name.to_string();
                }
            }
        }
    }

    format!("未知分区 (ID: {})", area_id)
}

pub async fn get_status() -> impl IntoResponse {
    // Always fetch fresh Bilibili status for accurate real-time updates
    let cfg = match load_config().await {
        Ok(cfg) => cfg,
        Err(e) => {
            tracing::error!("Failed to load config: {}", e);
            let error_msg = if e.to_string().contains("Permission denied") {
                format!("配置文件权限错误: {}。请确保 config.yaml 文件存在且有读取权限，或在可执行文件所在目录运行程序。", e)
            } else if e.to_string().contains("No such file") {
                format!(
                    "配置文件不存在: {}。请先运行 'bilistream setup' 创建配置文件。",
                    e
                )
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

    let bili_area_name = get_area_name(bili_area_id);

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
        "anti_collision": cfg.enable_anti_collision,
        "enable_lol_monitor": cfg.enable_lol_monitor,
        "riot_api_key": cfg.riot_api_key.clone().unwrap_or_default(),
        "bilibili": {
            "room": cfg.bililive.room,
            "enable_danmaku_command": cfg.bililive.enable_danmaku_command,
        },
        "youtube": {
            "channel_name": cfg.youtube.channel_name,
            "channel_id": cfg.youtube.channel_id,
            "area_v2": cfg.youtube.area_v2,
        },
        "twitch": {
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
    anti_collision: Option<bool>,
    enable_lol_monitor: Option<bool>,
    riot_api_key: Option<String>,
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
    if let Some(anti_collision) = payload.anti_collision {
        cfg.enable_anti_collision = anti_collision;
    }
    if let Some(enable_lol_monitor) = payload.enable_lol_monitor {
        cfg.enable_lol_monitor = enable_lol_monitor;
    }
    if let Some(riot_api_key) = payload.riot_api_key {
        if !riot_api_key.is_empty() {
            cfg.riot_api_key = Some(riot_api_key);
        }
    }

    // Save config
    let config_path = std::env::current_exe()
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .with_file_name("config.yaml");
    let yaml = serde_yaml::to_string(&cfg).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    std::fs::write(config_path, yaml).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    // Set config updated flag so main loop can detect the change
    set_config_updated();

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
) -> Result<ApiResponse<()>, StatusCode> {
    let mut cfg = load_config()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let area_v2 = match payload.platform.as_deref() {
        Some("YT") => cfg.youtube.area_v2,
        Some("TW") => cfg.twitch.area_v2,
        _ => 235,
    };

    bili_start_live(&mut cfg, area_v2)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(ApiResponse {
        success: true,
        data: None,
        message: Some("直播已开始".to_string()),
    })
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

#[derive(Deserialize)]
pub struct SendDanmakuRequest {
    message: String,
}

pub async fn send_danmaku(
    Json(payload): Json<SendDanmakuRequest>,
) -> Result<ApiResponse<()>, StatusCode> {
    let cfg = load_config()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    send_danmaku_to_bili(&cfg, &payload.message)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(ApiResponse {
        success: true,
        data: None,
        message: Some("弹幕已发送".to_string()),
    })
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
pub struct UpdateChannelRequest {
    platform: String, // "youtube" or "twitch"
    channel_id: String,
    channel_name: String,
    area_id: Option<u64>,
    quality: Option<String>,
    riot_api_key: Option<String>,
}

pub async fn update_channel(
    Json(payload): Json<UpdateChannelRequest>,
) -> Result<ApiResponse<()>, StatusCode> {
    let mut cfg = load_config()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    match payload.platform.as_str() {
        "youtube" => {
            cfg.youtube.channel_id = payload.channel_id;
            cfg.youtube.channel_name = payload.channel_name;
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
        }
        "twitch" => {
            cfg.twitch.channel_id = payload.channel_id;
            cfg.twitch.channel_name = payload.channel_name;
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
    let config_path = std::env::current_exe()
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .with_file_name("config.yaml");
    let yaml = serde_yaml::to_string(&cfg).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    std::fs::write(config_path, yaml).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    // Set config updated flag so main loop can detect the change
    set_config_updated();

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
pub struct SetupStatus {
    needs_setup: bool,
    missing_files: Vec<String>,
    setup_command: String,
}

pub async fn check_setup() -> Result<Json<SetupStatus>, StatusCode> {
    let exe_path = std::env::current_exe().map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let config_path = exe_path.with_file_name("config.yaml");
    let cookies_path = exe_path.with_file_name("cookies.json");

    let mut missing_files = Vec::new();

    if !config_path.exists() {
        missing_files.push("config.yaml".to_string());
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
