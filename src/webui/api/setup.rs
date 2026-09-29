use super::setup_data::{save_setup_bundle, setup_channel, SetupArea, SetupChannel};
use super::*;
use crate::plugins::holodex::HolodexFavoriteChannel;

#[derive(Serialize)]
pub struct SetupStatus {
    needs_setup: bool,
    missing_files: Vec<String>,
    setup_command: String,
    storage_error: Option<String>,
}

pub async fn check_setup() -> Result<Json<SetupStatus>, StatusCode> {
    let store = match tokio::task::spawn_blocking(crate::storage::global).await {
        Ok(Ok(store)) => store,
        error => {
            return Ok(Json(SetupStatus {
                needs_setup: false,
                missing_files: Vec::new(),
                setup_command: String::new(),
                storage_error: Some(match error {
                    Ok(Err(error)) => error.to_string(),
                    _ => "数据存储不可用".into(),
                }),
            }))
        }
    };
    let mut missing_files = Vec::new();
    if store
        .revision("config.json")
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .is_none()
    {
        missing_files.push("config.json".into());
    }
    if store
        .revision("cookies.json")
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .is_none()
    {
        missing_files.push("cookies.json".into());
    }

    let needs_setup = !missing_files.is_empty();

    let setup_command = if cfg!(target_os = "windows") {
        "bilistream.exe".to_string()
    } else {
        "./bilistream".to_string()
    };

    Ok(Json(SetupStatus {
        needs_setup,
        missing_files,
        setup_command,
        storage_error: None,
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
    youtube_enable_monitor: Option<bool>,
    youtube_channel_name: Option<String>,
    youtube_channel_id: Option<String>,
    youtube_area_v2: Option<u64>,
    youtube_quality: Option<String>,
    youtube_proxy: Option<String>,
    twitch_enable_monitor: Option<bool>,
    twitch_channel_name: Option<String>,
    twitch_channel_id: Option<String>,
    twitch_area_v2: Option<u64>,
    twitch_proxy_region: Option<String>,
    twitch_quality: Option<String>,
    twitch_proxy: Option<String>,
    niconico_enable_monitor: Option<bool>,
    niconico_channel_name: Option<String>,
    niconico_channel_id: Option<String>,
    niconico_area_v2: Option<u64>,
    #[serde(default)]
    selected_areas: Vec<SetupArea>,
    #[serde(default)]
    selected_youtube_channels: Vec<HolodexFavoriteChannel>,
    holodex_api_key: Option<String>,
    holodex_jwt: Option<String>,
    riot_api_key: Option<String>,
    enable_lol_monitor: bool,
}

/// Selected roster additions and monitor targets have deliberately separate paths.
fn setup_targets(payload: &SetupConfigRequest) -> Result<Vec<SetupChannel>, String> {
    let mut channels = Vec::new();
    for (platform, enabled, name, id) in [
        (
            "youtube",
            payload.youtube_enable_monitor,
            payload.youtube_channel_name.as_deref(),
            payload.youtube_channel_id.as_deref(),
        ),
        (
            "twitch",
            payload.twitch_enable_monitor,
            payload.twitch_channel_name.as_deref(),
            payload.twitch_channel_id.as_deref(),
        ),
        (
            "niconico",
            payload.niconico_enable_monitor,
            payload.niconico_channel_name.as_deref(),
            payload.niconico_channel_id.as_deref(),
        ),
    ] {
        if enabled == Some(false) {
            continue;
        }
        match setup_channel(platform, name, id)? {
            Some(channel) => channels.push(channel),
            None if enabled == Some(true) => {
                return Err(format!("{platform} 请选择转播频道，或选择不转播"))
            }
            None => {}
        }
    }
    Ok(channels)
}

async fn resolve_setup_targets(
    payload: &mut SetupConfigRequest,
) -> Result<Vec<SetupChannel>, String> {
    if let Some(input) = payload
        .youtube_channel_id
        .as_deref()
        .filter(|s| payload.youtube_enable_monitor != Some(false) && !s.trim().is_empty())
    {
        payload.youtube_channel_id = Some(
            crate::plugins::youtube_channel::resolve_channel_id(
                input,
                payload.youtube_proxy.as_deref(),
            )
            .await?,
        );
    }
    setup_targets(payload)
}

fn setup_roster(
    payload: &SetupConfigRequest,
    targets: &[SetupChannel],
) -> Result<Vec<SetupChannel>, String> {
    let mut channels = targets.to_vec();
    for selected in &payload.selected_youtube_channels {
        channels.push(
            setup_channel("youtube", Some(&selected.name), Some(&selected.id))?
                .ok_or("导入频道缺少 YouTube ID")?,
        );
    }
    Ok(channels)
}

fn apply_setup_targets(
    cfg: &mut Config,
    payload: &SetupConfigRequest,
    targets: &[SetupChannel],
    new_install: bool,
) {
    let requested = |platform, enabled: Option<bool>| {
        enabled.or_else(|| {
            new_install.then(|| targets.iter().any(|channel| channel.platform == platform))
        })
    };
    if let Some(enabled) = requested("youtube", payload.youtube_enable_monitor) {
        cfg.youtube.enable_monitor = enabled;
        cfg.enable_youtube_monitor = enabled;
    }
    if let Some(enabled) = requested("twitch", payload.twitch_enable_monitor) {
        cfg.twitch.enable_monitor = enabled;
        cfg.enable_twitch_monitor = enabled;
    }
    if let Some(enabled) = requested("niconico", payload.niconico_enable_monitor) {
        cfg.niconico.enable_monitor = enabled;
    }
    for channel in targets {
        match channel.platform {
            "youtube" => {
                cfg.youtube.channel_name = channel.name.clone();
                cfg.youtube.channel_id = channel.id.clone();
            }
            "twitch" => {
                cfg.twitch.channel_name = channel.name.clone();
                cfg.twitch.channel_id = channel.id.clone();
            }
            "niconico" => {
                cfg.niconico.channel_name = channel.name.clone();
                cfg.niconico.channel_id = channel.id.clone();
                cfg.niconico.live_id.clear();
                if new_install {
                    cfg.show_niconico = true;
                }
            }
            _ => unreachable!(),
        }
    }
    // A disabled selector can carry stale values from an earlier edit. Ignore
    // every target preference in that case, including quality/area/proxy.
    if payload.youtube_enable_monitor != Some(false) {
        if let Some(area) = payload.youtube_area_v2 {
            cfg.youtube.area_v2 = area;
        }
        if let Some(quality) = &payload.youtube_quality {
            cfg.youtube.quality = quality.clone();
        }
        if let Some(proxy) = &payload.youtube_proxy {
            cfg.youtube.proxy = (!proxy.is_empty()).then(|| proxy.clone());
        }
    }
    if payload.twitch_enable_monitor != Some(false) {
        if let Some(area) = payload.twitch_area_v2 {
            cfg.twitch.area_v2 = area;
        }
        if let Some(quality) = &payload.twitch_quality {
            cfg.twitch.quality = quality.clone();
        }
        if let Some(region) = &payload.twitch_proxy_region {
            cfg.twitch.proxy_region = region.clone();
        }
        if let Some(proxy) = &payload.twitch_proxy {
            cfg.twitch.proxy = (!proxy.is_empty()).then(|| proxy.clone());
        }
    }
    if payload.niconico_enable_monitor != Some(false) {
        if let Some(area) = payload.niconico_area_v2 {
            cfg.niconico.area_v2 = area;
        }
    }
}

fn setup_enables_monitor(
    previous: &Config,
    current: &Config,
    payload: &SetupConfigRequest,
) -> bool {
    payload.youtube_enable_monitor == Some(true)
        || payload.twitch_enable_monitor == Some(true)
        || payload.niconico_enable_monitor == Some(true)
        || (!previous.youtube.enable_monitor && current.youtube.enable_monitor)
        || (!previous.twitch.enable_monitor && current.twitch.enable_monitor)
        || (!previous.niconico.enable_monitor && current.niconico.enable_monitor)
        || (!previous.bililive.enable_danmaku_command && current.bililive.enable_danmaku_command)
}

pub async fn save_setup_config(
    Json(mut payload): Json<SetupConfigRequest>,
) -> Result<ApiResponse<()>, StatusCode> {
    let invalid = |message| ApiResponse {
        success: false,
        data: None,
        message: Some(message),
    };
    if payload.room <= 0 || payload.interval == 0 {
        return Ok(invalid("直播间号和检测间隔必须大于 0".into()));
    }
    let targets = match resolve_setup_targets(&mut payload).await {
        Ok(targets) => targets,
        Err(error) => return Ok(invalid(error)),
    };
    let channels = match setup_roster(&payload, &targets) {
        Ok(channels) => channels,
        Err(error) => return Ok(invalid(error)),
    };
    let new_install =
        !crate::storage::contains("config.json").map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    // Load existing config or create default
    let mut cfg = if let Ok(existing_cfg) = load_config().await {
        existing_cfg
    } else {
        if !new_install {
            return Ok(invalid(
                "已有 config.json 无法读取，请先修复文件，避免覆盖原配置".into(),
            ));
        }
        // Create new config with defaults
        crate::config::Config {
            snapshot: None,
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
                enable_monitor: false,
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
                enable_monitor: false,
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
            holodex_monitor_gate: true,
            youtube_api_key: None,
            youtube_rss_enabled: true,
            show_priority_channel: false,
            show_twitch: true,
            show_niconico: false,
            youtube_websub_callback_url: None,
            youtube_websub_port: crate::config::default_websub_port(),
            riot_api_key: None,
            enable_lol_monitor: false,
            lol_monitor_interval: Some(1),
            anti_collision_list: std::collections::HashMap::new(),
            priority_channel: crate::config::PriorityChannel::default(),
            enable_youtube_monitor: false,
            enable_twitch_monitor: false,
            niconico: crate::config::Niconico::default(),
            cluster: crate::config::ClusterConfig::default(),
        }
    };
    let previous_cfg = cfg.clone();

    // Update only the fields from payload
    cfg.auto_cover = payload.auto_cover;
    cfg.enable_anti_collision = payload.anti_collision;
    cfg.interval = payload.interval;
    cfg.bililive.enable_danmaku_command = payload.enable_danmaku_command;
    cfg.bililive.room = payload.room;
    if let Some(key) = &payload.holodex_api_key {
        cfg.holodex_api_key = (!key.trim().is_empty()).then(|| key.trim().to_string());
    }
    if let Some(jwt) = &payload.holodex_jwt {
        let jwt = crate::plugins::holodex::normalize_holodex_jwt(jwt);
        if jwt.is_empty() {
            cfg.holodex_jwt = None;
            cfg.holodex_jwt_refreshed_at = None;
            cfg.holodex_username = None;
        } else {
            cfg.holodex_jwt = Some(jwt.to_string());
            cfg.holodex_username = None;
        }
    }
    if let Some(key) = &payload.riot_api_key {
        cfg.riot_api_key = (!key.is_empty()).then(|| key.clone());
    }
    cfg.enable_lol_monitor = payload.enable_lol_monitor;

    apply_setup_targets(&mut cfg, &payload, &targets, new_install);
    if setup_enables_monitor(&previous_cfg, &cfg, &payload)
        && !local_node_can_enable_monitor_toggles(&previous_cfg)
    {
        return Ok(monitor_toggle_enable_rejected_response());
    }
    save_setup_bundle(&mut cfg, channels, payload.selected_areas)
        .await
        .map_err(config_save_status)?;

    let youtube_updated = youtube_monitor_reload_needed(&previous_cfg, &cfg);
    let twitch_updated = twitch_monitor_reload_needed(&previous_cfg, &cfg);
    let niconico_updated = niconico_monitor_reload_needed(&previous_cfg, &cfg);

    if youtube_updated || twitch_updated || niconico_updated {
        set_config_updated();
    }

    // Refresh status cache with updated configuration
    refresh_status_cache_config_from(&cfg);

    let toggle_sync_message = schedule_active_monitor_state_sync_after_toggle_change(&cfg);
    let target_sync_message =
        if monitored_config_version(&previous_cfg) != monitored_config_version(&cfg) {
            sync_monitored_config_after_change(&cfg).await
        } else {
            String::new()
        };

    // Refresh live status in background only when active monitor targets changed.
    if youtube_updated || twitch_updated || niconico_updated {
        tokio::spawn(async move {
            if youtube_updated {
                let _ = refresh_youtube_status().await;
            }
            if twitch_updated {
                let _ = refresh_twitch_status().await;
            }
            if niconico_updated {
                let _ = refresh_niconico_status().await;
            }
        });
    }

    Ok(ApiResponse {
        success: true,
        data: None,
        message: Some(format!(
            "配置已保存{target_sync_message}{toggle_sync_message}"
        )),
    })
}

#[derive(Serialize)]
pub struct LoginStatusResponse {
    logged_in: bool,
    message: String,
}

pub async fn check_login_status() -> Result<Json<LoginStatusResponse>, StatusCode> {
    let logged_in =
        crate::storage::contains("cookies.json").map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
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
    qr_image: String,
    auth_code: String,
}

pub(crate) fn qr_code_data_url(text: &str) -> Result<String, qrcode::types::QrError> {
    use base64::Engine as _;
    let svg = qrcode::QrCode::new(text.as_bytes())?
        .render::<qrcode::render::svg::Color>()
        .min_dimensions(256, 256)
        .build();
    Ok(format!(
        "data:image/svg+xml;base64,{}",
        base64::engine::general_purpose::STANDARD.encode(svg)
    ))
}

pub async fn get_qr_code() -> Result<Json<ApiResponse<QrCodeResponse>>, StatusCode> {
    match bilibili::get_login_qrcode().await {
        Ok((qr_url, auth_code)) => Ok(Json(ApiResponse {
            success: true,
            data: Some(QrCodeResponse {
                qr_image: qr_code_data_url(&qr_url)
                    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?,
                qr_url,
                auth_code,
            }),
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
    if !updater::begin_update() {
        return Ok(Json(ApiResponse {
            success: false,
            data: None,
            message: Some("更新正在进行中".into()),
        }));
    }

    tracing::info!("开始下载更新: {}", download_url);

    // Spawn update task in background
    tokio::spawn(async move {
        match updater::download_and_install_update(&download_url, None).await {
            Ok(_) => {
                updater::set_update_status("restarting", "安装完成，正在重启");
                tracing::info!("✅ 更新安装成功！程序将在 3 秒后重启...");

                // Perform graceful shutdown before restarting
                tracing::info!("🛑 执行优雅关闭...");
                crate::cluster::clear_local_stream();
                crate::plugins::stop_ffmpeg().await;

                tokio::time::sleep(tokio::time::Duration::from_secs(3)).await;

                match schedule_update_restart() {
                    Ok(()) => std::process::exit(0),
                    Err(e) => {
                        updater::set_update_status(
                            "failed",
                            "安装完成，但自动重启失败，请手动重启",
                        );
                        tracing::error!("❌ 更新后重启调度失败: {}", e);
                    }
                }
            }
            Err(e) => {
                updater::set_update_status("failed", format!("更新失败: {e}"));
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

pub async fn update_status() -> Json<updater::UpdateStatus> {
    Json(updater::update_status())
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
    std::fs::write(
        &restart_script,
        crate::webui::restart::windows_restart_bat()?,
    )
    .map_err(|e| format!("写入重启脚本失败: {}", e))?;
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
    let restart_command = crate::webui::restart::restart_command_line()?;

    let script_content = format!("#!/bin/sh\nsleep 3\n{} &\nrm -- \"$0\"\n", restart_command);
    // This script contains the existing login arguments, so create it privately.
    let _ = std::fs::remove_file(&restart_script);
    crate::storage::paths::write_private(&restart_script, script_content.as_bytes())
        .map_err(|e| format!("写入重启脚本失败: {e}"))?;
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

#[cfg(test)]
mod setup_tests {
    use super::*;

    fn payload(changes: serde_json::Value) -> SetupConfigRequest {
        let mut value = json!({"room":10000,"interval":60,"auto_cover":true,
            "enable_danmaku_command":false,"anti_collision":false,"enable_lol_monitor":false});
        value
            .as_object_mut()
            .unwrap()
            .extend(changes.as_object().unwrap().clone());
        serde_json::from_value(value).unwrap()
    }

    fn config() -> Config {
        let mut cfg = crate::cluster::tests::test_config("setup", 0);
        cfg.cluster.enabled = false;
        cfg
    }

    #[tokio::test]
    async fn none_skips_stale_validation_and_preserves_all_platform_preferences() {
        let mut request = payload(json!({
            "youtube_enable_monitor":false,"twitch_enable_monitor":false,"niconico_enable_monitor":false,
            "youtube_channel_id":"https://invalid.example/stale","youtube_channel_name":"ignored",
            "twitch_channel_id":"https://invalid.example/stale","niconico_channel_id":"lv-invalid",
            "youtube_quality":"worst","twitch_quality":"worst","youtube_proxy":"stale",
            "twitch_proxy":"stale","twitch_proxy_region":"stale","youtube_area_v2":999,
            "twitch_area_v2":999,"niconico_area_v2":999
        }));
        let targets = resolve_setup_targets(&mut request).await.unwrap();
        assert!(targets.is_empty());
        let mut cfg = config();
        cfg.youtube.enable_monitor = true;
        cfg.twitch.enable_monitor = true;
        cfg.niconico.enable_monitor = true;
        cfg.niconico.live_id = "lv123".into();
        let mut expected = cfg.clone();
        expected.youtube.enable_monitor = false;
        expected.enable_youtube_monitor = false;
        expected.twitch.enable_monitor = false;
        expected.enable_twitch_monitor = false;
        expected.niconico.enable_monitor = false;
        apply_setup_targets(&mut cfg, &request, &targets, false);
        assert_eq!(
            serde_json::to_value(cfg).unwrap(),
            serde_json::to_value(expected).unwrap()
        );
    }

    #[test]
    fn selected_imports_do_not_assign_targets_and_import_only_is_valid() {
        let request = payload(json!({
            "youtube_enable_monitor":false,"twitch_enable_monitor":false,"niconico_enable_monitor":false,
            "selected_youtube_channels":[
                {"id":"UCabcdefghijklmnopqrstuv","name":"Offline favorite"},
                {"id":"UC1234567890123456789012","name":"Another favorite"}
            ]
        }));
        let targets = setup_targets(&request).unwrap();
        let roster = setup_roster(&request, &targets).unwrap();
        assert!(targets.is_empty());
        assert_eq!(roster.len(), 2);
        assert_eq!(roster[0].name, "Offline favorite");
        let mut cfg = config();
        let previous_id = cfg.youtube.channel_id.clone();
        apply_setup_targets(&mut cfg, &request, &targets, true);
        assert_eq!(cfg.youtube.channel_id, previous_id);
        assert!(!cfg.youtube.enable_monitor && !cfg.enable_youtube_monitor);
        assert!(!cfg.twitch.enable_monitor && !cfg.enable_twitch_monitor);
        assert!(!cfg.niconico.enable_monitor);
    }

    #[test]
    fn explicit_targets_enable_mirrored_flags_and_ignore_last_import() {
        let request = payload(json!({
            "youtube_enable_monitor":true,"twitch_enable_monitor":true,"niconico_enable_monitor":true,
            "youtube_channel_id":"UCabcdefghijklmnopqrstuv","youtube_channel_name":"Target",
            "twitch_channel_id":"DEMO_TW","niconico_channel_id":"https://ch.nicovideo.jp/demo",
            "selected_youtube_channels":[{"id":"UC1234567890123456789012","name":"Last import"}]
        }));
        let targets = setup_targets(&request).unwrap();
        assert_eq!(setup_roster(&request, &targets).unwrap().len(), 4);
        let mut cfg = config();
        cfg.youtube.enable_monitor = false;
        cfg.twitch.enable_monitor = false;
        cfg.niconico.enable_monitor = false;
        cfg.niconico.live_id = "lv123".into();
        let previous = cfg.clone();
        apply_setup_targets(&mut cfg, &request, &targets, false);
        assert!(cfg.youtube.enable_monitor && cfg.enable_youtube_monitor);
        assert!(cfg.twitch.enable_monitor && cfg.enable_twitch_monitor);
        assert!(cfg.niconico.enable_monitor);
        assert_eq!(cfg.youtube.channel_name, "Target");
        assert_eq!(cfg.youtube.channel_id, "UCabcdefghijklmnopqrstuv");
        assert_eq!(cfg.twitch.channel_id, "demo_tw");
        assert_eq!(cfg.niconico.channel_id, "demo");
        assert!(cfg.niconico.live_id.is_empty());
        assert!(setup_enables_monitor(&previous, &cfg, &request));
    }

    #[test]
    fn enabled_targets_and_selected_imports_require_valid_ids() {
        for platform in ["youtube", "twitch", "niconico"] {
            let mut changes = json!({});
            changes[format!("{platform}_enable_monitor")] = json!(true);
            assert!(setup_targets(&payload(changes.clone())).is_err());
            changes[format!("{platform}_channel_id")] = json!("invalid input!");
            assert!(setup_targets(&payload(changes)).is_err());
        }
        let request =
            payload(json!({"selected_youtube_channels":[{"id":"video-id","name":"Bad"}]}));
        assert!(setup_roster(&request, &[]).is_err());
    }

    #[test]
    fn older_clients_preserve_existing_monitors_and_fresh_empty_targets_stay_off() {
        let request = payload(json!({"youtube_channel_id":"UCabcdefghijklmnopqrstuv"}));
        assert_eq!(request.youtube_enable_monitor, None);
        assert!(request.selected_youtube_channels.is_empty());
        let targets = setup_targets(&request).unwrap();
        let mut cfg = config();
        cfg.youtube.enable_monitor = false;
        cfg.enable_youtube_monitor = false;
        cfg.twitch.enable_monitor = true;
        cfg.enable_twitch_monitor = true;
        apply_setup_targets(&mut cfg, &request, &targets, false);
        assert!(!cfg.youtube.enable_monitor && !cfg.enable_youtube_monitor);
        assert!(cfg.twitch.enable_monitor && cfg.enable_twitch_monitor);
        apply_setup_targets(&mut cfg, &request, &targets, true);
        assert!(cfg.youtube.enable_monitor && cfg.enable_youtube_monitor);
        assert!(!cfg.twitch.enable_monitor && !cfg.enable_twitch_monitor);
        let empty = payload(json!({}));
        apply_setup_targets(&mut cfg, &empty, &[], true);
        assert!(
            !cfg.youtube.enable_monitor
                && !cfg.twitch.enable_monitor
                && !cfg.niconico.enable_monitor
        );
    }

    #[test]
    fn setup_enable_guard_covers_niconico_and_explicit_enable_but_allows_disabling() {
        let previous = config();
        for platform in ["youtube", "twitch", "niconico"] {
            let mut changes = json!({});
            changes[format!("{platform}_enable_monitor")] = json!(true);
            assert!(setup_enables_monitor(
                &previous,
                &previous,
                &payload(changes)
            ));
        }
        let request = payload(
            json!({"youtube_enable_monitor":false,"twitch_enable_monitor":false,"niconico_enable_monitor":false}),
        );
        let mut cfg = previous.clone();
        apply_setup_targets(&mut cfg, &request, &[], false);
        assert!(!setup_enables_monitor(&previous, &cfg, &request));
    }
}
