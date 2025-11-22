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
            // Only log error if it's not a "file not found" error (expected on first run)
            let is_not_found = e.to_string().contains("No such file");
            if !is_not_found {
                tracing::error!("Failed to load config: {}", e);
            }

            let error_msg = if e.to_string().contains("Permission denied") {
                format!("配置文件权限错误: {}。请确保 config.yaml 文件存在且有读取权限，或在可执行文件所在目录运行程序。", e)
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
    twitch_oauth_token: Option<String>,
    twitch_proxy_region: Option<String>,
    twitch_quality: Option<String>,
    holodex_api_key: Option<String>,
    riot_api_key: Option<String>,
    enable_lol_monitor: bool,
}

pub async fn save_setup_config(
    Json(payload): Json<SetupConfigRequest>,
) -> Result<ApiResponse<()>, StatusCode> {
    let proxy_line = payload.proxy.unwrap_or_default();
    let holodex_line = payload.holodex_api_key.unwrap_or_default();
    let riot_line = payload.riot_api_key.unwrap_or_default();

    let yt_channel_name = payload.youtube_channel_name.unwrap_or_default();
    let yt_channel_id = payload.youtube_channel_id.unwrap_or_default();
    let yt_area_v2 = payload.youtube_area_v2.unwrap_or(235);
    let yt_quality = payload
        .youtube_quality
        .unwrap_or_else(|| "best".to_string());

    let tw_channel_name = payload.twitch_channel_name.unwrap_or_default();
    let tw_channel_id = payload.twitch_channel_id.unwrap_or_default();
    let tw_area_v2 = payload.twitch_area_v2.unwrap_or(235);
    let tw_oauth = payload.twitch_oauth_token.unwrap_or_default();
    let tw_proxy_region = payload
        .twitch_proxy_region
        .unwrap_or_else(|| "as".to_string());
    let tw_quality = payload.twitch_quality.unwrap_or_else(|| "best".to_string());

    let config_content = format!(
        r#"Interval: {} # 检测直播间隔
AutoCover: {} # 自动更换封面
AntiCollision: {} # 撞车监控
Proxy: {} # 代理地址,无需代理可以不填此项或者留空
HolodexApiKey: {} # Holodex Api Key from https://holodex.net/login
RiotApiKey: {} # Riot API Key from https://developer.riotgames.com/
EnableLolMonitor: {} # 启用英雄联盟玩家ID监控 (true/false)
LolMonitorInterval: 1 # 监控LOL局内玩家ID时间间隔(秒)
BiliLive:
  EnableDanmakuCommand: {} # true or false
  Room: {}
  BiliRtmpUrl: rtmp://live-push.bilivideo.com/live-bvc/
  BiliRtmpKey: ""
Youtube:
  ChannelName: {} # 频道名称 (将出现于转播标题)
  ChannelId: {} # Youtube Channel ID
  AreaV2: {} # B站分区ID https://api.live.bilibili.com/room/v1/Area/getList
  Quality: {} # 流质量: best(推荐), worst, 720p, 480p, 360p, 或 yt-dlp 格式字符串
Twitch:
  ChannelName: {} # 频道名称 (将出现于转播标题)
  ChannelId: {} # the string followed after https://www.twitch.tv/
  AreaV2: {} # B站分区ID https://api.live.bilibili.com/room/v1/Area/getList
  OauthToken: {} # check https://streamlink.github.io/cli/plugins/twitch.html#authentication
  ProxyRegion: {} # na, eu, eu2, eu3, eu4, eu5, as, sa, eul, eu2l, asl, all, perf
  Quality: {} # 流质量: best(推荐), worst, 720p, 480p, 360p, 或 streamlink 质量选项

AntiCollisionList:
  # B站ID1: 房间号1  # ID仅用于弹幕提醒撞车
  # B站ID2: 房间号2  # 房间号用于检测撞车
"#,
        payload.interval,
        payload.auto_cover,
        payload.anti_collision,
        proxy_line,
        holodex_line,
        riot_line,
        payload.enable_lol_monitor,
        payload.enable_danmaku_command,
        payload.room,
        yt_channel_name,
        yt_channel_id,
        yt_area_v2,
        yt_quality,
        tw_channel_name,
        tw_channel_id,
        tw_area_v2,
        tw_oauth,
        tw_proxy_region,
        tw_quality,
    );

    // Write config file
    let config_path = std::env::current_exe()
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .with_file_name("config.yaml");
    std::fs::write(config_path, config_content).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

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
                    let current_exe = std::env::current_exe().unwrap();
                    let _ = std::process::Command::new(current_exe).spawn();
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
