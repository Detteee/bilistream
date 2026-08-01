use super::*;

#[derive(Serialize)]
pub struct SetupStatus {
    needs_setup: bool,
    missing_files: Vec<String>,
    setup_command: String,
}

pub async fn check_setup() -> Result<Json<SetupStatus>, StatusCode> {
    let exe_path = std::env::current_exe().map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let config_path = exe_path.with_file_name("config.json");
    let cookies_path = exe_path.with_file_name("cookies.json");

    let mut missing_files = Vec::new();

    if !config_path.exists() {
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
    auto_cover: bool,
    enable_danmaku_command: bool,
    interval: u64,
    anti_collision: bool,
    youtube_channel_name: Option<String>,
    youtube_channel_id: Option<String>,
    youtube_area_v2: Option<u64>,
    youtube_quality: Option<String>,
    youtube_proxy: Option<String>,
    twitch_channel_name: Option<String>,
    twitch_channel_id: Option<String>,
    twitch_area_v2: Option<u64>,
    twitch_proxy_region: Option<String>,
    twitch_quality: Option<String>,
    twitch_proxy: Option<String>,
    holodex_api_key: Option<String>,
    holodex_jwt: Option<String>,
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
                proxy: None,
                crop: None,
                ffmpeg_cache: crate::config::FfmpegCache::default(),
            },
            youtube: crate::config::Youtube {
                enable_monitor: true,
                channel_name: String::new(),
                channel_id: String::new(),
                area_v2: 235,
                quality: "best".to_string(),
                cookies_file: None,
                cookies_from_browser: None,
                proxy: None,
                deno_path: None,
                crop: None,
                ffmpeg_cache: crate::config::FfmpegCache::default(),
            },
            holodex_api_key: None,
            holodex_jwt: None,
            holodex_jwt_refreshed_at: None,
            holodex_username: None,
            holodex_skip_jwt_verify: false,
            riot_api_key: None,
            enable_lol_monitor: false,
            lol_monitor_interval: Some(1),
            anti_collision_list: std::collections::HashMap::new(),
        }
    };
    let previous_cfg = cfg.clone();

    // Update only the fields from payload
    cfg.auto_cover = payload.auto_cover;
    cfg.enable_anti_collision = payload.anti_collision;
    cfg.interval = payload.interval;
    cfg.bililive.enable_danmaku_command = payload.enable_danmaku_command;
    cfg.bililive.room = payload.room;
    cfg.holodex_api_key = payload.holodex_api_key.filter(|key| !key.is_empty());
    if let Some(jwt) = payload.holodex_jwt {
        let jwt = jwt.trim();
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
        }
    }
    cfg.riot_api_key = payload.riot_api_key.filter(|key| !key.is_empty());
    cfg.enable_lol_monitor = payload.enable_lol_monitor;

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
    if let Some(yt_proxy) = payload.youtube_proxy {
        cfg.youtube.proxy = if yt_proxy.is_empty() {
            None
        } else {
            Some(yt_proxy)
        };
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
    if let Some(tw_proxy) = payload.twitch_proxy {
        cfg.twitch.proxy = if tw_proxy.is_empty() {
            None
        } else {
            Some(tw_proxy)
        };
    }

    // Save config
    crate::config::save_config(&cfg)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let youtube_updated = youtube_monitor_reload_needed(&previous_cfg, &cfg);
    let twitch_updated = twitch_monitor_reload_needed(&previous_cfg, &cfg);

    if youtube_updated || twitch_updated {
        set_config_updated();
    }

    // Refresh status cache with updated configuration
    refresh_status_cache_config_from(&cfg);

    // Refresh live status in background only when active monitor targets changed.
    if youtube_updated || twitch_updated {
        tokio::spawn(async move {
            if youtube_updated {
                let _ = refresh_youtube_status().await;
            }
            if twitch_updated {
                let _ = refresh_twitch_status().await;
            }
        });
    }

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

                match schedule_update_restart() {
                    Ok(()) => std::process::exit(0),
                    Err(e) => tracing::error!("❌ 更新后重启调度失败: {}", e),
                }
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

pub(crate) fn current_exe_dir() -> Result<PathBuf, String> {
    let exe = std::env::current_exe().map_err(|e| format!("获取当前程序路径失败: {}", e))?;
    executable_parent_dir(&exe)
}

pub(crate) fn executable_parent_dir(exe: &Path) -> Result<PathBuf, String> {
    exe.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .ok_or_else(|| format!("当前程序路径没有有效父目录: {}", exe.display()))
}

#[cfg(target_os = "windows")]
pub(crate) fn schedule_update_restart() -> Result<(), String> {
    let restart_script = current_exe_dir()?.join("restart_after_update.bat");
    if !restart_script.exists() {
        return Err(format!("重启脚本不存在: {}", restart_script.display()));
    }
    let restart_script = restart_script
        .to_str()
        .ok_or_else(|| format!("重启脚本路径不是有效 UTF-8: {}", restart_script.display()))?;
    std::process::Command::new("cmd")
        .args(["/C", "start", "", restart_script])
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("启动重启脚本失败: {}", e))
}

#[cfg(not(target_os = "windows"))]
pub(crate) fn schedule_update_restart() -> Result<(), String> {
    let exe_dir = current_exe_dir()?;
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

    std::fs::write(&restart_script, script_content)
        .map_err(|e| format!("写入重启脚本失败: {}", e))?;
    let chmod_status = std::process::Command::new("chmod")
        .arg("+x")
        .arg(&restart_script)
        .status()
        .map_err(|e| format!("设置重启脚本权限失败: {}", e))?;
    if !chmod_status.success() {
        return Err(format!("设置重启脚本权限失败: {}", chmod_status));
    }
    std::process::Command::new("sh")
        .arg(&restart_script)
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("启动重启脚本失败: {}", e))
}

// Version endpoint
#[derive(Serialize)]
pub struct VersionInfo {
    version: String,
    is_tauri: bool,
}

pub async fn get_version() -> Result<Json<ApiResponse<VersionInfo>>, StatusCode> {
    Ok(Json(ApiResponse {
        success: true,
        data: Some(VersionInfo {
            version: env!("CARGO_PKG_VERSION").to_string(),
            is_tauri: cfg!(feature = "tauri-build"),
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
