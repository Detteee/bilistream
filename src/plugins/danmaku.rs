use super::twitch::get_twitch_status;
use super::youtube::{get_youtube_area_topic, get_youtube_status};
use crate::config::load_config;
use crate::config::Config;
use crate::plugins::banned_keywords::{
    banned_keyword_hit, danmaku_banned_keywords, danmaku_haystack,
};
use crate::plugins::bilibili;
use lazy_static::lazy_static;
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use std::{fs, io};
use tokio::sync::Notify;

static DANMAKU_RUNNING: AtomicBool = AtomicBool::new(false);
static DANMAKU_STOP_SIGNAL: AtomicBool = AtomicBool::new(false);

lazy_static! {
    static ref DANMAKU_COMMANDS_ENABLED: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));
    static ref DANMAKU_STOP_NOTIFY: Notify = Notify::new();
    static ref WARNING_STOP: AtomicBool = AtomicBool::new(false);
    static ref LAST_WARNING_CHANNEL: Mutex<Option<String>> = Mutex::new(None);
    static ref CONFIG_UPDATED: AtomicBool = AtomicBool::new(false);
    static ref CONFIG_UPDATE_NOTIFY: Notify = Notify::new();
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
    if stop {
        DANMAKU_STOP_NOTIFY.notify_waiters();
    }
}

pub fn should_stop_danmaku() -> bool {
    DANMAKU_STOP_SIGNAL.load(Ordering::Relaxed)
}

pub async fn wait_danmaku_stop_signal() {
    if should_stop_danmaku() {
        return;
    }

    let notified = DANMAKU_STOP_NOTIFY.notified();
    if should_stop_danmaku() {
        return;
    }
    notified.await;
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
    let mut cache = CHANNELS_CACHE.lock().unwrap_or_else(|poisoned| {
        tracing::warn!("Recovering poisoned danmaku channels cache");
        poisoned.into_inner()
    });

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
async fn update_config(
    platform: &str,
    channel_name: &str,
    channel_id: &str,
    area_id: u64,
) -> io::Result<bool> {
    let mut config = load_config()
        .await
        .map_err(|error| io::Error::other(error.to_string()))?;
    if !apply_danmaku_target(&mut config, platform, channel_name, channel_id, area_id)? {
        return Ok(false);
    }
    crate::config::save_config(&mut config)
        .await
        .map_err(|error| io::Error::other(error.to_string()))?;
    Ok(true)
}

fn apply_danmaku_target(
    config: &mut Config,
    platform: &str,
    channel_name: &str,
    channel_id: &str,
    area_id: u64,
) -> io::Result<bool> {
    let (current_name, current_id, current_area) = match platform {
        "YT" => (
            &mut config.youtube.channel_name,
            &mut config.youtube.channel_id,
            &mut config.youtube.area_v2,
        ),
        "TW" => (
            &mut config.twitch.channel_name,
            &mut config.twitch.channel_id,
            &mut config.twitch.area_v2,
        ),
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "unknown platform",
            ))
        }
    };
    // This comparison receives the corrected area, after live metadata has
    // been checked. A repeated command can therefore repair a saved wrong area.
    if current_name == channel_name && current_id == channel_id && *current_area == area_id {
        return Ok(false);
    }
    *current_name = channel_name.to_owned();
    *current_id = channel_id.to_owned();
    *current_area = area_id;
    Ok(true)
}

/// Load areas configuration from areas.json
fn load_areas_config() -> Option<serde_json::Value> {
    let areas_path = std::env::current_exe().ok()?.parent()?.join("areas.json");

    let content = std::fs::read_to_string(areas_path).ok()?;
    serde_json::from_str(&content).ok()
}

/// Bilibili's catch-all 其他单机. The restream loop starts here when nothing
/// else has been chosen yet. A Holodex card still suggests it when a keyword
/// actually matched.
pub const DEFAULT_AREA_ID: u64 = 235;

/// Topic plus title, which is what Holodex cards and the restream loop both
/// feed into [`check_area_id_with_title`]. Underscores become spaces so a
/// Holodex `just_chatting` topic can hit a `just chatting` keyword.
pub fn stream_area_haystack(topic: Option<&str>, title: &str) -> String {
    let title = title.trim();
    match topic.map(str::trim).filter(|topic| !topic.is_empty()) {
        Some(topic) => format!("{topic} {title}"),
        None => title.to_string(),
    }
}

/// Determines the area id based on the live title by checking keywords from areas.json
pub fn check_area_id_with_title(live_title: &str, current_area_id: u64) -> u64 {
    matched_area_id_from_title(live_title).unwrap_or(current_area_id)
}

fn matched_area_id_from_title(live_title: &str) -> Option<u64> {
    area_id_matching_keywords(live_title, &load_areas_config()?)
}

fn area_id_matching_keywords(live_title: &str, areas_config: &serde_json::Value) -> Option<u64> {
    let title = live_title.to_lowercase().replace("_", " ");
    let areas = areas_config["areas"].as_array()?;
    for area in areas {
        let (Some(id), Some(keywords)) = (area["id"].as_u64(), area["title_keywords"].as_array())
        else {
            continue;
        };
        for keyword in keywords {
            let Some(kw) = keyword.as_str() else {
                continue;
            };
            let kw = kw.trim().to_lowercase();
            if !kw.is_empty() && title.contains(&kw) {
                return Some(id);
            }
        }
    }
    None
}

/// Suggested Holodex 切换 area from the current `areas.json` keywords.
///
/// Reads the file on every call so a keyword edit shows up on the next Holodex
/// refresh instead of waiting for a process restart. A keyword hit on 235
/// (其他单机) is still a suggestion; only a miss leaves the picker to open.
pub fn suggest_area_from_stream(topic: Option<&str>, title: &str) -> (Option<u64>, Option<String>) {
    match matched_area_id_from_title(&stream_area_haystack(topic, title)) {
        Some(id) => (Some(id), get_area_name(id)),
        None => (None, None),
    }
}

/// Resolve area alias to area name using areas.json
fn resolve_area_alias(alias: &str) -> String {
    let alias_trimmed = alias.trim();
    let alias_lower = alias_trimmed.to_lowercase();

    // Load areas configuration
    let areas_config = match load_areas_config() {
        Some(config) => config,
        None => {
            tracing::warn!("无法加载 areas.json 配置");
            return alias_trimmed.to_string();
        }
    };

    // Check each area's aliases
    if let Some(areas) = areas_config["areas"].as_array() {
        for area in areas {
            if let Some(name) = area["name"].as_str() {
                let name_lower = name.to_lowercase();

                // First check if the input matches the area name itself
                if alias_lower == name_lower {
                    tracing::debug!("分区别名 '{}' 匹配到分区名称: {}", alias_trimmed, name);
                    return name.to_string();
                }

                // Then check aliases
                if let Some(aliases) = area["aliases"].as_array() {
                    for area_alias in aliases {
                        if let Some(a) = area_alias.as_str() {
                            if alias_lower == a.to_lowercase() {
                                tracing::debug!(
                                    "分区别名 '{}' 匹配到别名 '{}', 返回分区: {}",
                                    alias_trimmed,
                                    a,
                                    name
                                );
                                return name.to_string();
                            }
                        }
                    }
                }
            }
        }
    }

    tracing::warn!("未找到分区别名 '{}' 的匹配项", alias_trimmed);
    alias_trimmed.to_string()
}

/// Processes a single danmaku command.
pub async fn process_danmaku(command: &str) {
    process_danmaku_with_owner(command, false).await;
}

/// Processes a single danmaku command with owner flag.
pub async fn process_danmaku_with_owner(command: &str, is_owner: bool) {
    if !command.starts_with(" :") {
        return;
    }
    // tracing::info!("弹幕:{}", &command[2..]);
    let command = command.replace(" ", "").replace("　", "");
    let normalized_danmaku = command.replace("％", "%");

    let cfg = match load_config().await {
        Ok(cfg) => cfg,
        Err(e) => {
            tracing::error!("加载配置失败，忽略弹幕命令: {}", e);
            return;
        }
    };
    // Add check for 查询 command
    if normalized_danmaku.contains("%查询") {
        // tracing::info!("🔍 查询命令收到");
        let channel_name = cfg.youtube.channel_name.clone();
        let area_name = area_name_or_unknown(cfg.youtube.area_v2);
        let _ =
            bilibili::send_danmaku(&cfg, &format!("YT: {} - {}", channel_name, area_name)).await;
        let channel_name = cfg.twitch.channel_name.clone();
        let area_name = area_name_or_unknown(cfg.twitch.area_v2);
        // bilibili 发送弹幕cooldown > 1秒
        tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
        let _ =
            bilibili::send_danmaku(&cfg, &format!("TW: {} - {}", channel_name, area_name)).await;
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

    tracing::info!("原始分区输入: '{}'", area_alias);
    let area_name = resolve_area_alias(area_alias);
    tracing::info!("解析后的分区名称: '{}'", area_name);

    let area_id = match get_area_id(&area_name) {
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

        let Some(channel_id_str) = channel_id.as_deref() else {
            tracing::error!("频道 {} 未在{}列表中", channel_name, platform);
            let _ = bilibili::send_danmaku(
                &cfg,
                &format!("错误：频道 {} 未在{}列表中", channel_name, platform),
            )
            .await;
            return;
        };
        let resolved_channel_name = match get_channel_name(&platform, channel_id_str) {
            Ok(Some(name)) => name,
            Ok(None) => {
                tracing::error!("频道 ID {} 未在{}列表中", channel_id_str, platform);
                let _ = bilibili::send_danmaku(
                    &cfg,
                    &format!("错误：频道 ID {} 未在{}列表中", channel_id_str, platform),
                )
                .await;
                return;
            }
            Err(e) => {
                tracing::error!("获取频道名称时出错: {}", e);
                return;
            }
        };

        let (live_title, live_topic, youtube_video_id) = if platform.eq_ignore_ascii_case("YT") {
            // get youtube live status
            match get_youtube_status(channel_id_str).await {
                Ok((_, topic, title, _, _, video_id)) => {
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
                    (t, topic.unwrap_or_default(), video_id)
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
                        tracing::error!("TW频道 {} 未在直播", resolved_channel_name);
                        let _ = bilibili::send_danmaku(
                            &cfg,
                            &format!("错误: {} 未在直播", resolved_channel_name),
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
                    (t, topic.unwrap_or_default(), None)
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
        let live_topic_title = danmaku_haystack(&live_topic, &live_title);

        if let Some(keyword) = banned_keyword_hit(&live_topic_title, &danmaku_banned_keywords()) {
            tracing::error!(
                "直播标题/分区包含不支持的关键词: {} | {}",
                keyword,
                live_topic_title
            );
            let _ = bilibili::send_danmaku(
                &cfg,
                &format!("错误：{} 的标题/分区含:{}", platform, keyword),
            )
            .await;
            return;
        }

        // Area matching needs the same game/topic that powers WebUI suggestions.
        // Keep this optional lookup separate from command validation and liveness.
        let area_topic = if platform == "YT" && live_topic.trim().is_empty() {
            get_youtube_area_topic(channel_id_str, youtube_video_id.as_deref()).await
        } else {
            Some(live_topic)
        };
        let updated_area_id = check_area_id_with_title(
            &stream_area_haystack(area_topic.as_deref(), &live_title),
            area_id,
        );

        let updated_area_name = match get_area_name(updated_area_id) {
            Some(name) => name,
            None => {
                let _ = bilibili::send_danmaku(&cfg, "错误：无法获取更新后的分区名称").await;
                return;
            }
        };

        if updated_area_id != area_id {
            tracing::info!(
                "🔄 按直播标题/游戏关键词修正弹幕分区: {} -> {} (ID: {})",
                area_name,
                updated_area_name,
                updated_area_id
            );
        }

        match update_config(
            &platform,
            &resolved_channel_name,
            channel_id_str,
            updated_area_id,
        )
        .await
        {
            Ok(false) => {
                let message = format!(
                    "{} 监听对象已是：{} - {}",
                    platform, resolved_channel_name, updated_area_name
                );
                tracing::info!("{}", message);
                let _ = bilibili::send_danmaku(&cfg, &message).await;
            }
            Ok(true) => {
                // Clear warning flag when user manually changes channel
                clear_warning_stop();

                // Set config updated flag to skip waiting interval
                set_config_updated();

                // Send success notification
                let _ = bilibili::send_danmaku(
                    &cfg,
                    &format!(
                        "更新：{} - {} - {}",
                        platform, resolved_channel_name, updated_area_name
                    ),
                )
                .await;
                tracing::info!(
                    "✅ 更新成功 {} 频道: {} 分区: {} (ID: {} )",
                    platform,
                    resolved_channel_name,
                    updated_area_name,
                    updated_area_id
                );
            }
            Err(e) => {
                tracing::error!("更新配置时出错: {}", e);
                let _ = bilibili::send_danmaku(&cfg, &format!("错误：更新配置时出错 {}", e)).await;
            }
        }
    } else {
        tracing::error!("指令错误: {}", danmaku_command);
        let _ = bilibili::send_danmaku(&cfg, &format!("错误：不支持的平台 {}", platform)).await;
    }
}

/// Main function to start the danmaku client in the background.
/// The client runs continuously and monitors for WARNING/CUT_OFF messages.
/// Danmaku commands are only processed when enabled via set_danmaku_commands_enabled().
pub fn run_danmaku() {
    if DANMAKU_RUNNING
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        tracing::warn!("弹幕客户端已在运行");
        return;
    }

    set_danmaku_stop_signal(false);

    std::thread::spawn(|| {
        let rt = match tokio::runtime::Runtime::new() {
            Ok(rt) => rt,
            Err(e) => {
                tracing::error!("创建弹幕客户端运行时失败: {}", e);
                return;
            }
        };
        rt.block_on(async {
            tracing::info!("🚀 启动弹幕客户端");

            let cfg = match load_config().await {
                Ok(cfg) => cfg,
                Err(e) => {
                    tracing::error!("加载弹幕客户端配置失败: {}", e);
                    set_danmaku_running(false);
                    return;
                }
            };
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
pub async fn stop_danmaku() {
    if !is_danmaku_running() {
        tracing::warn!("弹幕客户端未在运行");
        return;
    }

    tracing::info!("🛑 停止弹幕客户端");
    set_danmaku_stop_signal(true);

    // Wait for the client to stop gracefully without blocking the runtime.
    let mut attempts = 0;
    while is_danmaku_running() && attempts < 20 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        attempts += 1;
    }

    if is_danmaku_running() {
        tracing::warn!("弹幕客户端停止超时，保持停止信号等待后台退出");
    } else {
        tracing::info!("✅ 弹幕客户端已成功停止");
        set_danmaku_stop_signal(false);
    }
}

/// Set the warning stop flag and store the channel that was stopped
pub fn set_warning_stop(channel_name: String) {
    if let Ok(mut last) = LAST_WARNING_CHANNEL.lock() {
        *last = Some(channel_name);
    }
    WARNING_LOGGED.store(false, Ordering::SeqCst); // Reset logged flag for new warning
    WARNING_STOP.store(true, Ordering::SeqCst);
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
    CONFIG_UPDATE_NOTIFY.notify_waiters();
}

/// Check if config was updated (to skip waiting)
pub fn is_config_updated() -> bool {
    CONFIG_UPDATED.load(Ordering::SeqCst)
}

/// Clear the config updated flag
pub fn clear_config_updated() {
    CONFIG_UPDATED.store(false, Ordering::SeqCst);
}

/// Wait until either config changes or the timeout expires.
pub async fn wait_config_update_or_timeout(timeout: Duration) -> bool {
    if is_config_updated() {
        return true;
    }

    let notified = CONFIG_UPDATE_NOTIFY.notified();
    if is_config_updated() {
        return true;
    }

    tokio::select! {
        _ = notified => true,
        _ = tokio::time::sleep(timeout) => is_config_updated(),
    }
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

fn area_name_or_unknown(area_id: u64) -> String {
    get_area_name(area_id).unwrap_or_else(|| format!("未知分区({})", area_id))
}

fn get_area_id(area_name: &str) -> Result<u64, Box<dyn std::error::Error>> {
    let area_name_trimmed = area_name.trim();
    let areas_path = std::env::current_exe()
        .map_err(|e| format!("无法获取可执行文件路径: {}", e))?
        .with_file_name("areas.json");

    let content =
        std::fs::read_to_string(&areas_path).map_err(|e| format!("无法读取 areas.json: {}", e))?;

    let areas: serde_json::Value =
        serde_json::from_str(&content).map_err(|e| format!("无法解析 areas.json: {}", e))?;

    let area_name_lower = area_name_trimmed.to_lowercase();

    if let Some(areas_array) = areas["areas"].as_array() {
        for area in areas_array {
            if let (Some(id), Some(name)) = (area["id"].as_u64(), area["name"].as_str()) {
                if name.to_lowercase() == area_name_lower {
                    tracing::debug!("找到分区 '{}' 的ID: {}", area_name_trimmed, id);
                    return Ok(id);
                }
            }
        }
    }

    tracing::error!("未知的分区: '{}' (已尝试匹配)", area_name_trimmed);
    Err(format!("未知的分区: {}", area_name_trimmed).into())
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gta_topic_corrects_a_repeated_other_online_games_request_to_235() {
        let title =
            "【#SURGETown 】Cafe ニャイトメア素敵な悪夢よ届け!【 #折咲もしゅ #MOSHULIVE #REJECT】";
        let areas = serde_json::json!({"areas": [
            { "id": 107, "name": "其他网游", "title_keywords": [] },
            { "id": 235, "name": "其他单机", "title_keywords": [
                "pokemon", "core keeper", "terraria", "tgc card shop simulator",
                "stardew valley", "rust", "gta"
            ], "aliases": ["单机", "其他游戏"] }
        ]});
        let requested_area = 107;
        assert_eq!(area_id_matching_keywords(title, &areas), None);
        let corrected_area =
            area_id_matching_keywords(&stream_area_haystack(Some("GTA"), title), &areas)
                .unwrap_or(requested_area);
        assert_eq!(
            corrected_area, 235,
            "a keyword hit on 235 is a correction, not a fallback"
        );

        let mut cfg: Config = serde_json::from_value(serde_json::json!({
            "auto_cover": false, "enable_anti_collision": false, "interval": 15,
            "bililive": { "enable_danmaku_command": true, "room": 1, "bili_rtmp_url": "", "bili_rtmp_key": "" },
            "youtube": { "channel_id": "moshu", "channel_name": "折咲もしゅ", "area_v2": requested_area },
            "twitch": { "channel_id": "moshu", "channel_name": "折咲もしゅ", "area_v2": requested_area },
            "enable_lol_monitor": false, "anti_collision_list": {}
        })).unwrap();
        for platform in ["YT", "TW"] {
            assert!(apply_danmaku_target(
                &mut cfg,
                platform,
                "折咲もしゅ",
                "moshu",
                corrected_area
            )
            .unwrap());
            assert!(!apply_danmaku_target(
                &mut cfg,
                platform,
                "折咲もしゅ",
                "moshu",
                corrected_area
            )
            .unwrap());
        }
        assert_eq!(cfg.youtube.area_v2, 235);
        assert_eq!(cfg.twitch.area_v2, 235);
    }

    #[test]
    fn unmatched_metadata_keeps_the_requested_area() {
        let areas = serde_json::json!({"areas": [
            { "id": 235, "title_keywords": ["gta", "rust"] }
        ]});
        for topic in [None, Some("unmapped game")] {
            let area =
                area_id_matching_keywords(&stream_area_haystack(topic, "初めてのゲーム"), &areas)
                    .unwrap_or(107);
            assert_eq!(area, 107);
        }
    }

    #[test]
    fn a_keyword_hit_on_the_catchall_is_still_a_match() {
        let areas = serde_json::json!({
            "areas": [
                { "id": 235, "title_keywords": ["rust"] },
                { "id": 252, "title_keywords": ["tarkov", "タルコフ"] }
            ]
        });
        assert_eq!(
            area_id_matching_keywords("RUST】レオラス最終日", &areas),
            Some(DEFAULT_AREA_ID)
        );
        assert_eq!(
            area_id_matching_keywords("【Escape from Tarkov】タルコフ", &areas),
            Some(252)
        );
        assert_eq!(area_id_matching_keywords("雑談します", &areas), None);
    }

    #[tokio::test]
    async fn config_update_wait_wakes_on_signal() {
        clear_config_updated();

        let waiter = tokio::spawn(wait_config_update_or_timeout(Duration::from_secs(5)));
        tokio::time::sleep(Duration::from_millis(10)).await;
        set_config_updated();

        assert!(waiter.await.unwrap());
        clear_config_updated();
    }
}
