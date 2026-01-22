use super::twitch::get_twitch_status;
use super::youtube::get_youtube_status;
use crate::config::load_config;
use crate::config::Config;
use crate::plugins::bilibili;
use lazy_static::lazy_static;
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::{fs, io};

static DANMAKU_RUNNING: AtomicBool = AtomicBool::new(false);
static DANMAKU_STOP_SIGNAL: AtomicBool = AtomicBool::new(false);

lazy_static! {
    static ref DANMAKU_COMMANDS_ENABLED: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));
    static ref WARNING_STOP: AtomicBool = AtomicBool::new(false);
    static ref LAST_WARNING_CHANNEL: Mutex<Option<String>> = Mutex::new(None);
    static ref CONFIG_UPDATED: AtomicBool = AtomicBool::new(false);
    static ref WARNING_LOGGED: AtomicBool = AtomicBool::new(false);
}

pub fn is_danmaku_running() -> bool {
    DANMAKU_RUNNING.load(Ordering::Relaxed)
}

pub fn set_danmaku_running(running: bool) {
    DANMAKU_RUNNING.store(running, Ordering::Relaxed);
}

pub fn is_danmaku_commands_enabled() -> bool {
    DANMAKU_COMMANDS_ENABLED.load(Ordering::Relaxed)
}

pub fn set_danmaku_commands_enabled(enabled: bool) {
    DANMAKU_COMMANDS_ENABLED.store(enabled, Ordering::Relaxed);
}

pub fn set_danmaku_stop_signal(stop: bool) {
    DANMAKU_STOP_SIGNAL.store(stop, Ordering::Relaxed);
}

pub fn should_stop_danmaku() -> bool {
    DANMAKU_STOP_SIGNAL.load(Ordering::Relaxed)
}
fn load_banned_keywords() -> Vec<String> {
    let areas_path = match std::env::current_exe() {
        Ok(path) => path.with_file_name("areas.json"),
        Err(e) => {
            tracing::error!("无法获取可执行文件路径: {}", e);
            return Vec::new();
        }
    };

    let content = match std::fs::read_to_string(&areas_path) {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("无法读取 areas.json: {}", e);
            return Vec::new();
        }
    };

    let data: serde_json::Value = match serde_json::from_str(&content) {
        Ok(d) => d,
        Err(e) => {
            tracing::error!("无法解析 areas.json: {}", e);
            return Vec::new();
        }
    };

    if let Some(keywords) = data["banned_keywords"].as_array() {
        keywords
            .iter()
            .filter_map(|k| k.as_str().map(|s| s.to_string()))
            .collect()
    } else {
        tracing::warn!("areas.json 中未找到 banned_keywords");
        Vec::new()
    }
}
#[derive(Serialize, Deserialize, Clone)]
struct Platforms {
    youtube: Option<String>,
    twitch: Option<String>,
}

#[derive(Serialize, Deserialize, Clone)]
struct Channel {
    name: String,
    platforms: Platforms,
    riot_puuid: Option<String>,
    #[serde(default)]
    aliases: Vec<String>,
}

#[derive(Serialize, Deserialize, Clone)]
struct ChannelsConfig {
    channels: Vec<Channel>,
}

// Cache channels config to avoid repeated file reads and parsing
lazy_static! {
    static ref CHANNELS_CACHE: Mutex<Option<(ChannelsConfig, std::time::SystemTime)>> =
        Mutex::new(None);
}

fn load_channels() -> Result<ChannelsConfig, Box<dyn std::error::Error>> {
    let mut cache = CHANNELS_CACHE.lock().unwrap();

    // Check if cache is valid (less than 5 minutes old)
    if let Some((ref config, timestamp)) = *cache {
        if timestamp
            .elapsed()
            .unwrap_or(std::time::Duration::from_secs(301))
            < std::time::Duration::from_secs(300)
        {
            return Ok(config.clone());
        }
    }

    // Load fresh data
    let channels_path = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|p| p.join("channels.json")))
        .ok_or("Failed to get executable path")?;
    let content = fs::read_to_string(channels_path)?;
    let config: ChannelsConfig = serde_json::from_str(&content)?;
    *cache = Some((config.clone(), std::time::SystemTime::now()));
    Ok(config)
}

pub fn get_channel_id(
    platform: &str,
    channel_name: &str,
) -> Result<Option<String>, Box<dyn std::error::Error>> {
    let config = load_channels()?;

    for channel in &config.channels {
        // Check both name and aliases without cloning whole channel (case-insensitive)
        if channel.name.to_lowercase() == channel_name.to_lowercase()
            || channel
                .aliases
                .iter()
                .any(|a| a.to_lowercase() == channel_name.to_lowercase())
        {
            return Ok(match platform {
                "YT" => channel.platforms.youtube.as_ref().map(|s| s.to_string()),
                "TW" => channel.platforms.twitch.as_ref().map(|s| s.to_string()),
                _ => None,
            });
        }
    }
    Ok(None)
}

pub fn get_channel_name(
    platform: &str,
    channel_id: &str,
) -> Result<Option<String>, Box<dyn std::error::Error>> {
    let config = load_channels()?;

    for channel in &config.channels {
        match platform {
            "YT" => {
                if let Some(id) = &channel.platforms.youtube {
                    if id == channel_id {
                        return Ok(Some(channel.name.clone()));
                    }
                }
            }
            "TW" => {
                if let Some(id) = &channel.platforms.twitch {
                    if id == channel_id {
                        return Ok(Some(channel.name.clone()));
                    }
                }
            }
            _ => return Ok(None),
        }
    }
    Ok(None)
}

pub fn get_puuid(channel_name: &str) -> Result<String, Box<dyn std::error::Error>> {
    let config = load_channels()?;

    for channel in &config.channels {
        // Check both name and aliases (case-insensitive)
        if channel.name.to_lowercase() == channel_name.to_lowercase()
            || channel
                .aliases
                .iter()
                .any(|a| a.to_lowercase() == channel_name.to_lowercase())
        {
            return Ok(channel
                .riot_puuid
                .as_ref()
                .map(|s| s.to_string())
                .unwrap_or_default());
        }
    }
    tracing::error!("PUUID not found for channel: {}", channel_name);
    Ok(String::new())
}

// Optional: Helper function to get all channels for a platform
pub fn get_all_channels(
    platform: &str,
) -> Result<Vec<(String, String)>, Box<dyn std::error::Error>> {
    let config = load_channels()?;
    let mut channels = Vec::new();

    for channel in &config.channels {
        match platform {
            "YT" => {
                if let Some(id) = &channel.platforms.youtube {
                    channels.push((channel.name.clone(), id.clone()));
                }
            }
            "TW" => {
                if let Some(id) = &channel.platforms.twitch {
                    channels.push((channel.name.clone(), id.clone()));
                }
            }
            _ => (),
        }
    }

    Ok(channels)
}

/// Updates the configuration JSON file with new values.
fn update_config(
    platform: &str,
    channel_name: &str,
    channel_id: &str,
    area_id: u64,
) -> io::Result<bool> {
    // Use the same config.json path as the executable (matches config.rs behavior)
    let exe_path = std::env::current_exe()?;
    let config_path = exe_path.with_file_name("config.json");

    // Read the existing config.json
    let config_content = fs::read_to_string(&config_path)?;

    // Deserialize JSON into Config struct
    let mut config: Config = serde_json::from_str(&config_content)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

    // Update the fields directly (no need to check again since we already checked earlier)
    if platform == "YT" {
        config.youtube.channel_id = channel_id.to_string();
        config.youtube.channel_name = channel_name.to_string();
        config.youtube.area_v2 = area_id;
    } else if platform == "TW" {
        config.twitch.channel_id = channel_id.to_string();
        config.twitch.channel_name = channel_name.to_string();
        config.twitch.area_v2 = area_id;
    }

    // Serialize Config struct back to JSON
    let updated_json = serde_json::to_string_pretty(&config)
        .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;

    // Write the updated JSON back to config.json (this also updates file mtime)
    fs::write(&config_path, updated_json)?;

    Ok(true)
}

/// Load areas configuration from areas.json
fn load_areas_config() -> Option<serde_json::Value> {
    let areas_path = std::env::current_exe().ok()?.parent()?.join("areas.json");

    let content = std::fs::read_to_string(areas_path).ok()?;
    serde_json::from_str(&content).ok()
}

/// Determines the area id based on the live title by checking keywords from areas.json
pub fn check_area_id_with_title(live_title: &str, current_area_id: u64) -> u64 {
    let title = live_title.to_lowercase().replace("_", " ");

    // Load areas configuration
    let areas_config = match load_areas_config() {
        Some(config) => config,
        None => return current_area_id,
    };

    // Check each area's title keywords
    if let Some(areas) = areas_config["areas"].as_array() {
        for area in areas {
            if let (Some(id), Some(keywords)) =
                (area["id"].as_u64(), area["title_keywords"].as_array())
            {
                for keyword in keywords {
                    if let Some(kw) = keyword.as_str() {
                        if title.contains(&kw.to_lowercase()) {
                            return id;
                        }
                    }
                }
            }
        }
    }

    current_area_id
}

/// Resolve area alias to area name using areas.json
fn resolve_area_alias(alias: &str) -> &str {
    let alias_lower = alias.to_lowercase();

    // Load areas configuration
    let areas_config = match load_areas_config() {
        Some(config) => config,
        None => return alias,
    };

    // Check each area's aliases
    if let Some(areas) = areas_config["areas"].as_array() {
        for area in areas {
            if let (Some(name), Some(aliases)) = (area["name"].as_str(), area["aliases"].as_array())
            {
                for area_alias in aliases {
                    if let Some(a) = area_alias.as_str() {
                        if alias_lower == a.to_lowercase() {
                            // Return a static string by leaking memory (acceptable for small config)
                            return Box::leak(name.to_string().into_boxed_str());
                        }
                    }
                }
            }
        }
    }

    alias
}

/// Processes a single danmaku command.
pub async fn process_danmaku(command: &str) {
    process_danmaku_with_owner(command, false).await;
}

/// Processes a single danmaku command with owner flag.
pub async fn process_danmaku_with_owner(command: &str, is_owner: bool) {
    // only line start with : is danmaku
    if command.contains("WARN  [init] Connection closed by server") {
        tracing::info!("B站cookie过期，无法启动弹幕指令，请更新配置文件:./biliup login");
        return;
    }
    if !command.starts_with(" :") {
        return;
    }
    // tracing::info!("弹幕:{}", &command[2..]);
    let command = command.replace(" ", "").replace("　", "");
    let normalized_danmaku = command.replace("％", "%");

    let cfg = load_config().await.unwrap();
    // Add check for 查询 command
    if normalized_danmaku.contains("%查询") {
        // tracing::info!("🔍 查询命令收到");
        let channel_name = cfg.youtube.channel_name.clone();
        let area_name = get_area_name(cfg.youtube.area_v2);
        let _ = bilibili::send_danmaku(
            &cfg,
            &format!("YT: {} - {}", channel_name, area_name.unwrap()),
        )
        .await;
        let channel_name = cfg.twitch.channel_name.clone();
        let area_name = get_area_name(cfg.twitch.area_v2);
        // bilibili 发送弹幕cooldown > 1秒
        tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
        let _ = bilibili::send_danmaku(
            &cfg,
            &format!("TW: {} - {}", channel_name, area_name.unwrap()),
        )
        .await;
        return;
    }

    // Continue with existing command processing for %转播% commands
    if !normalized_danmaku.contains("%转播%") {
        // Not a command, ignore silently
        return;
    }

    // tracing::info!("📺 转播命令收到: {}", normalized_danmaku);
    let danmaku_command = normalized_danmaku.replace(" :", "");

    // Replace full-width ％ with half-width %
    let parts: Vec<&str> = danmaku_command.split('%').collect();
    // tracing::info!("弹幕:{:?}", parts);
    if parts.len() < 5 {
        tracing::error!("弹幕命令格式错误. Skipping...");
        let _ = bilibili::send_danmaku(&cfg, "错误：弹幕命令格式错误").await;
        return;
    }

    let platform = parts[2].to_uppercase();
    if platform.to_uppercase() != "YT" && platform.to_uppercase() != "TW" {
        tracing::error!("平台错误. Skipping... : {}", platform);
        let _ = bilibili::send_danmaku(&cfg, "错误：弹幕命令格式错误").await;
        return;
    }
    let channel_name = parts[3];
    let area_alias = parts[4];

    if area_alias.is_empty() {
        tracing::error!("分区不能为空. Skipping...");
        let _ = bilibili::send_danmaku(&cfg, "错误：分区不能为空").await;
        return;
    }

    let area_name = resolve_area_alias(area_alias);
    let area_id = match get_area_id(area_name) {
        Ok(id) => id,
        Err(e) => {
            tracing::error!("{}", e);
            let _ = bilibili::send_danmaku(&cfg, &format!("错误：{}", e)).await;
            return;
        }
    };

    tracing::info!(
        "平台: {}, 频道: {}, 分区: {}",
        platform,
        channel_name,
        area_name
    );

    if platform.eq("YT") || platform.eq("TW") {
        let channel_id = match get_channel_id(&platform, channel_name) {
            Ok(id) => id,
            Err(e) => {
                tracing::error!("检查频道时出错: {}", e);
                let _ = bilibili::send_danmaku(&cfg, &format!("错误：检查频道时出错 {}", e)).await;
                return;
            }
        };

        if channel_id.is_none() {
            tracing::error!("频道 {} 未在{}列表中", channel_name, platform);
            let _ = bilibili::send_danmaku(
                &cfg,
                &format!("错误：频道 {} 未在{}列表中", channel_name, platform),
            )
            .await;
            return;
        }

        // Use a reference to the String inside channel_id without moving it
        let channel_id_str = channel_id.as_ref().unwrap();
        let channel_name = match get_channel_name(&platform, channel_id_str) {
            Ok(name) => name,
            Err(e) => {
                tracing::error!("获取频道名称时出错: {}", e);
                return;
            }
        };

        // Early config check to avoid expensive live status API calls
        let exe_path = std::env::current_exe().map_err(|e| {
            tracing::error!("无法获取可执行文件路径: {}", e);
        });
        if let Ok(exe_path) = exe_path {
            let config_path = exe_path.with_file_name("config.json");

            // Read the existing config.json
            if let Ok(config_content) = fs::read_to_string(&config_path) {
                // Deserialize JSON into Config struct
                if let Ok(config) = serde_json::from_str::<Config>(&config_content) {
                    // Check if update is needed
                    let needs_update = if platform == "YT" {
                        &config.youtube.channel_id != channel_id_str
                            || &config.youtube.channel_name != channel_name.as_deref().unwrap()
                            || config.youtube.area_v2 != area_id
                    } else if platform == "TW" {
                        &config.twitch.channel_id != channel_id_str
                            || &config.twitch.channel_name != channel_name.as_deref().unwrap()
                            || config.twitch.area_v2 != area_id
                    } else {
                        false
                    };

                    if !needs_update {
                        let area_name = match get_area_name(area_id) {
                            Some(name) => name,
                            None => {
                                tracing::error!("无法获取分区名称");
                                return;
                            }
                        };
                        let _ = bilibili::send_danmaku(
                            &cfg,
                            &format!(
                                "{} 监听对象已是：{} - {}",
                                platform,
                                channel_name.as_deref().unwrap(),
                                area_name
                            ),
                        )
                        .await;
                        tracing::info!(
                            "{} 监听对象已是：{} - {}",
                            platform,
                            channel_name.as_deref().unwrap(),
                            area_name
                        );
                        return;
                    }
                }
            }
        }

        let (live_title, live_topic) = if platform.eq_ignore_ascii_case("YT") {
            // get youtube live status
            match get_youtube_status(channel_id_str).await {
                Ok((_, topic, title, _, _, _)) => {
                    let t = match title {
                        Some(t) => t,
                        None => {
                            if is_owner {
                                // Owner can force switch even without title
                                tracing::warn!("主播强制切换到无标题的YT频道");
                                "无标题直播".to_string()
                            } else {
                                tracing::error!("获取YT直播标题失败");
                                let _ =
                                    bilibili::send_danmaku(&cfg, "错误：获取YT直播标题失败").await;
                                tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
                                let _ =
                                    bilibili::send_danmaku(&cfg, "请确认是否已开（预告）窗").await;
                                return;
                            }
                        }
                    };
                    (t, topic.unwrap_or_default())
                }
                Err(e) => {
                    tracing::error!("获取YT直播标题时出错: {}", e);
                    let _ =
                        bilibili::send_danmaku(&cfg, &format!("错误：获取YT直播标题时出错 {}", e))
                            .await;
                    return;
                }
            }
        } else {
            // TW
            match get_twitch_status(channel_id_str).await {
                Ok((is_live, topic, title, _)) => {
                    if !is_live {
                        tracing::error!("TW频道 {:?} 未在直播", channel_name.clone().unwrap());
                        let _ = bilibili::send_danmaku(
                            &cfg,
                            &format!("错误: {:?} 未在直播", channel_name.unwrap()),
                        )
                        .await;
                        return;
                    }

                    let t = match title {
                        Some(t) => t,
                        None => {
                            if is_owner {
                                // Owner can force switch even without title
                                tracing::warn!("主播强制切换到无标题的TW频道");
                                "无标题直播".to_string()
                            } else {
                                tracing::error!("获取TW直播标题失败");
                                let _ =
                                    bilibili::send_danmaku(&cfg, "错误：获取TW直播标题失败").await;
                                return;
                            }
                        }
                    };
                    (t, topic.unwrap_or_default())
                }
                Err(e) => {
                    tracing::error!("获取TW状态时出错: {}", e);
                    let _ =
                        bilibili::send_danmaku(&cfg, &format!("错误：获取TW直播标题时出错 {}", e))
                            .await;
                    return;
                }
            }
        };
        let live_topic_title = format!("{} {}", live_topic, live_title).to_lowercase();

        let banned_keywords = load_banned_keywords();
        if let Some(keyword) = banned_keywords
            .iter()
            .find(|keyword| live_topic_title.contains(keyword.as_str()))
        {
            tracing::error!("直播标题/分区包含不支持的关键词:\n{}", live_topic_title);
            let _ = bilibili::send_danmaku(
                &cfg,
                &format!("错误：{} 的标题/分区含:{}", platform, keyword),
            )
            .await;
            return;
        }

        // Now you can use channel_id_str where needed without moving channel_id
        // let new_title = format!("【转播】{}", channel_name);
        let updated_area_id = check_area_id_with_title(&live_topic_title, area_id);

        let updated_area_name = match get_area_name(updated_area_id) {
            Some(name) => name,
            None => {
                let _ = bilibili::send_danmaku(&cfg, "错误：无法获取更新后的分区名称").await;
                return;
            }
        };

        match update_config(
            &platform,
            channel_name.as_deref().unwrap(),
            &channel_id_str,
            updated_area_id,
        ) {
            Ok(_) => {
                // Clear warning flag when user manually changes channel
                clear_warning_stop();

                // Set config updated flag to skip waiting interval
                set_config_updated();

                // Send success notification
                let _ = bilibili::send_danmaku(
                    &cfg,
                    &format!(
                        "更新：{} - {} - {}",
                        platform,
                        channel_name.as_deref().unwrap(),
                        updated_area_name
                    ),
                )
                .await;
                tracing::info!(
                    "✅ 更新成功 {} 频道: {} 分区: {} (ID: {} )",
                    platform,
                    channel_name.as_deref().unwrap(),
                    updated_area_name,
                    updated_area_id
                );
            }
            Err(e) => {
                tracing::error!("更新配置时出错: {}", e);
                let _ = bilibili::send_danmaku(&cfg, &format!("错误：更新配置时出错 {}", e)).await;
                return;
            }
        };
    } else {
        tracing::error!("指令错误: {}", danmaku_command);
        let _ = bilibili::send_danmaku(&cfg, &format!("错误：不支持的平台 {}", platform)).await;
    }
}

/// Main function to start the danmaku client in the background.
/// The client runs continuously and monitors for WARNING/CUT_OFF messages.
/// Danmaku commands are only processed when enabled via set_danmaku_commands_enabled().
pub fn run_danmaku() {
    if is_danmaku_running() {
        tracing::warn!("弹幕客户端已在运行");
        return;
    }

    std::thread::spawn(|| {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            // Set running flag inside the async task to avoid race conditions
            set_danmaku_running(true);
            tracing::info!("🚀 启动弹幕客户端");

            let cfg = load_config().await.unwrap();
            let room_id = cfg.bililive.room;

            // Create danmaku client config
            let danmaku_config = crate::plugins::danmaku_client::DanmakuConfig {
                room_id: room_id as u64,
                sessdata: cfg.bililive.credentials.sessdata.clone(),
                bili_jct: cfg.bililive.credentials.bili_jct.clone(),
                dede_user_id: cfg.bililive.credentials.dede_user_id.clone(),
                dede_user_id_ckmd5: cfg.bililive.credentials.dede_user_id_ckmd5.clone(),
                buvid3: cfg.bililive.credentials.buvid3.clone(),
            };

            // Wrap config in Arc for sharing across tasks
            let cfg_arc = Arc::new(cfg);
            // Use the global DANMAKU_COMMANDS_ENABLED Arc
            let enable_commands = DANMAKU_COMMANDS_ENABLED.clone();

            // Run danmaku client - it will keep running
            if let Err(e) = crate::plugins::danmaku_client::run_native_danmaku_client(
                danmaku_config,
                cfg_arc,
                enable_commands,
            )
            .await
            {
                tracing::error!("弹幕客户端错误: {}", e);
            }

            set_danmaku_running(false);
            tracing::info!("弹幕客户端已停止");
        });
    });
}

/// Enable or disable danmaku command processing.
/// The client continues to monitor for WARNING/CUT_OFF regardless of this setting.
pub fn enable_danmaku_commands(enabled: bool) {
    set_danmaku_commands_enabled(enabled);
    if enabled {
        tracing::info!("✅ 弹幕命令已启用");
    } else {
        tracing::info!("⏸️ 弹幕命令已禁用");
    }
}

/// Stop the danmaku client
pub fn stop_danmaku() {
    if !is_danmaku_running() {
        tracing::warn!("弹幕客户端未在运行");
        return;
    }

    tracing::info!("🛑 停止弹幕客户端");
    set_danmaku_stop_signal(true);

    // Wait for the client to stop gracefully (check status periodically)
    let mut attempts = 0;
    while is_danmaku_running() && attempts < 20 {
        std::thread::sleep(std::time::Duration::from_millis(100));
        attempts += 1;
    }

    if is_danmaku_running() {
        tracing::warn!("弹幕客户端停止超时，但继续执行");
    } else {
        tracing::info!("✅ 弹幕客户端已成功停止");
    }

    // Reset the stop signal for next time
    set_danmaku_stop_signal(false);
}

/// Set the warning stop flag and store the channel that was stopped
pub fn set_warning_stop(channel_name: String) {
    WARNING_STOP.store(true, Ordering::SeqCst);
    WARNING_LOGGED.store(false, Ordering::SeqCst); // Reset logged flag for new warning
    if let Ok(mut last) = LAST_WARNING_CHANNEL.lock() {
        *last = Some(channel_name);
    }
}

/// Check if we should skip streaming due to a recent warning
pub fn should_skip_due_to_warning(channel_name: &str) -> bool {
    if !WARNING_STOP.load(Ordering::SeqCst) {
        return false;
    }

    if let Ok(last) = LAST_WARNING_CHANNEL.lock() {
        if let Some(ref last_channel) = *last {
            return last_channel == channel_name;
        }
    }
    false
}

/// Check if we should skip streaming due to a recent warning (returns true only on first check for logging)
pub fn should_skip_due_to_warned(channel_name: &str) -> bool {
    if !WARNING_STOP.load(Ordering::SeqCst) {
        return false;
    }

    if let Ok(last) = LAST_WARNING_CHANNEL.lock() {
        if let Some(ref last_channel) = *last {
            if last_channel == channel_name {
                // Only return true for logging on first check
                if !WARNING_LOGGED.load(Ordering::SeqCst) {
                    WARNING_LOGGED.store(true, Ordering::SeqCst);
                    return true; // First time - should log
                }
                return false; // Subsequent times - don't log
            }
        }
    }
    false
}

/// Clear the warning stop flag (call when user manually changes channel)
pub fn clear_warning_stop() {
    WARNING_STOP.store(false, Ordering::SeqCst);
    if let Ok(mut last) = LAST_WARNING_CHANNEL.lock() {
        *last = None;
    }
}

/// Set the config updated flag to skip waiting interval
pub fn set_config_updated() {
    CONFIG_UPDATED.store(true, Ordering::SeqCst);
}

/// Check if config was updated (to skip waiting)
pub fn is_config_updated() -> bool {
    CONFIG_UPDATED.load(Ordering::SeqCst)
}

/// Clear the config updated flag
pub fn clear_config_updated() {
    CONFIG_UPDATED.store(false, Ordering::SeqCst);
}

pub fn get_area_name(area_id: u64) -> Option<String> {
    let areas_path = std::env::current_exe().ok()?.with_file_name("areas.json");

    let content = std::fs::read_to_string(areas_path).ok()?;
    let areas: serde_json::Value = serde_json::from_str(&content).ok()?;

    if let Some(areas_array) = areas["areas"].as_array() {
        for area in areas_array {
            if let (Some(id), Some(name)) = (area["id"].as_u64(), area["name"].as_str()) {
                if id == area_id {
                    return Some(name.to_string());
                }
            }
        }
    }

    tracing::error!("未知的分区ID: {}", area_id);
    None
}

fn get_area_id(area_name: &str) -> Result<u64, Box<dyn std::error::Error>> {
    let areas_path = std::env::current_exe()
        .map_err(|e| format!("无法获取可执行文件路径: {}", e))?
        .with_file_name("areas.json");

    let content =
        std::fs::read_to_string(&areas_path).map_err(|e| format!("无法读取 areas.json: {}", e))?;

    let areas: serde_json::Value =
        serde_json::from_str(&content).map_err(|e| format!("无法解析 areas.json: {}", e))?;

    if let Some(areas_array) = areas["areas"].as_array() {
        for area in areas_array {
            if let (Some(id), Some(name)) = (area["id"].as_u64(), area["name"].as_str()) {
                if name == area_name {
                    return Ok(id);
                }
            }
        }
    }

    Err(format!("未知的分区: {}", area_name).into())
}

pub fn get_aliases(target_name: &str) -> Result<Vec<String>, Box<dyn std::error::Error>> {
    let channels = load_channels()?;
    Ok(channels
        .channels
        .iter()
        .find(|c| c.name == target_name)
        .map(|c| c.aliases.clone())
        .unwrap_or_default())
}
