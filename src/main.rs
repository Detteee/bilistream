// Hide console window on Windows in release mode
#![cfg_attr(
    all(target_os = "windows", not(debug_assertions)),
    windows_subsystem = "windows"
)]

use bilistream::config::{load_config, save_config, Config};
use bilistream::plugins::bilibili::get_thumbnail;
use bilistream::plugins::Twitch as TwitchClient;
use bilistream::plugins::Youtube as YoutubeClient;
use bilistream::plugins::{
    bili_change_live_title, bili_start_live, bili_stop_live, bili_update_area, bilibili,
    check_area_id_with_title, clear_config_updated, clear_manual_restart, clear_manual_stop,
    clear_warning_stop, current_game_riot_ids, enable_danmaku_commands, ffmpeg, get_aliases,
    get_area_name, get_bili_live_status, get_bili_live_time, get_puuid, is_config_updated,
    is_danmaku_commands_enabled, is_danmaku_running, is_ffmpeg_running, run_danmaku, send_danmaku,
    should_skip_due_to_warned, should_skip_due_to_warning, stop_danmaku, stop_ffmpeg,
    wait_config_update_or_timeout, wait_ffmpeg, was_manual_restart, was_manual_stop,
    FfmpegCacheOptions, BILI_START_TEMP_BAN_PREFIX,
};
use chrono::{DateTime, Local, NaiveDateTime};
use regex::Regex;
use std::borrow::Cow;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::{error::Error, thread, time::Duration};
use textwrap;
use tracing_subscriber::fmt;
use unicode_width::UnicodeWidthStr;

// Graceful shutdown function
async fn graceful_shutdown() {
    // Stop ffmpeg process
    stop_ffmpeg().await;
}

static NO_LIVE: AtomicBool = AtomicBool::new(false);
// Use compact representation to reduce memory footprint
static LAST_MESSAGE: Mutex<Option<Box<str>>> = Mutex::new(None);
static LAST_COLLISION: Mutex<Option<(Box<str>, i32, Box<str>)>> = Mutex::new(None);
const DUAL_COLLISION_PLATFORM: &str = "双平台";
const MESSAGE_TIME_PATTERN: &str = r"\d{4}-\d{2}-\d{2} \d{2}:\d{2}:\d{2}";
const MESSAGE_TIME_FORMAT: &str = "%Y-%m-%d %H:%M:%S";
const MESSAGE_TIME_PLACEHOLDER: &str = "TIME";
const MESSAGE_UPDATE_TIME_THRESHOLD_MINUTES: i64 = 5;
static INVALID_ID_DETECTED: AtomicBool = AtomicBool::new(false);
// Track last video/stream ID for cover change detection (works across platforms)
static LAST_VIDEO_ID: Mutex<Option<String>> = Mutex::new(None);
// Track last banned keyword warning to prevent spam
static LAST_BANNED_KEYWORD_WARNING: Mutex<Option<String>> = Mutex::new(None);

fn recover_mutex_lock<'a, T>(lock: &'a Mutex<T>, name: &str) -> MutexGuard<'a, T> {
    lock.lock().unwrap_or_else(|poisoned| {
        tracing::warn!("Recovering poisoned {name} mutex");
        poisoned.into_inner()
    })
}

fn normalized_api_key(key: Option<&str>) -> Option<String> {
    key.map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn area_label(area_id: u64) -> String {
    get_area_name(area_id).unwrap_or_else(|| format!("未知分区(ID: {})", area_id))
}

#[derive(PartialEq)]
enum CollisionResult {
    Continue,
    Proceed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StreamPlatform {
    Youtube,
    Twitch,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FfmpegLoopExitReason {
    SourceEnded,
    BiliStopped,
    IntentionalRestart { target_m3u8_available: bool },
}

fn restart_exit_should_skip_end_danmaku(reason: FfmpegLoopExitReason) -> bool {
    matches!(
        reason,
        FfmpegLoopExitReason::IntentionalRestart {
            target_m3u8_available: true
        }
    )
}

impl StreamPlatform {
    fn code(self) -> &'static str {
        match self {
            StreamPlatform::Youtube => "YT",
            StreamPlatform::Twitch => "TW",
        }
    }
}

#[derive(Clone)]
struct StreamCandidate {
    platform: StreamPlatform,
    is_live: bool,
    topic: Option<String>,
    title: Option<String>,
    m3u8_url: Option<String>,
    stream_id: Option<String>,
    channel_name: String,
    channel_id: String,
    area_v2: u64,
}

impl StreamCandidate {
    fn is_playable(&self) -> bool {
        self.is_live && self.m3u8_url.is_some()
    }

    fn stream_title(&self) -> Option<String> {
        match (&self.topic, &self.title) {
            (Some(topic), Some(title)) => Some(format!("{} {}", topic, title)),
            _ => self.title.clone(),
        }
    }

    fn cfg_title(&self) -> String {
        format!("【转播】{}", self.channel_name)
    }
}

fn select_stream(yt: &StreamCandidate, tw: &StreamCandidate) -> Option<StreamCandidate> {
    if yt.is_playable() {
        Some(yt.clone())
    } else if tw.is_playable() {
        Some(tw.clone())
    } else {
        None
    }
}

/// (is_live, topic, title, m3u8_url, scheduled_start, stream_id) as returned by
/// both platform clients' get_status().
type SourceStatus = (
    bool,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<chrono::DateTime<Local>>,
    Option<String>,
);

const OFFLINE_SOURCE_STATUS: SourceStatus = (false, None, None, None, None, None);

/// The live-status client backing the currently selected stream candidate.
enum SourceClient<'a> {
    Youtube(&'a YoutubeClient),
    Twitch(&'a TwitchClient),
}

impl SourceClient<'_> {
    async fn get_status(&self) -> SourceStatus {
        match self {
            SourceClient::Youtube(client) => client.get_status().await,
            SourceClient::Twitch(client) => client.get_status().await,
        }
        .unwrap_or(OFFLINE_SOURCE_STATUS)
    }
}

fn selected_source_client<'a>(
    selected: &StreamCandidate,
    yt: &'a Option<YoutubeClient>,
    tw: &'a Option<TwitchClient>,
) -> Option<SourceClient<'a>> {
    match selected.platform {
        StreamPlatform::Youtube => yt.as_ref().map(SourceClient::Youtube),
        StreamPlatform::Twitch => tw.as_ref().map(SourceClient::Twitch),
    }
}

async fn source_client_status(client: &Option<SourceClient<'_>>) -> SourceStatus {
    match client {
        Some(client) => client.get_status().await,
        None => OFFLINE_SOURCE_STATUS,
    }
}

/// Fetches Bilibili live status, logging failures. Callers decide the fallback.
async fn fetch_bili_live_status_logged(
    room: i32,
) -> Result<(bool, String, u64), Box<dyn std::error::Error>> {
    match get_bili_live_status(room).await {
        Ok(status) => Ok(status),
        Err(e) => {
            tracing::error!("获取B站直播状态失败: {}", e);
            Err(e)
        }
    }
}

/// Marks a candidate offline when its channel was previously stopped due to a
/// warning/cut-off, announcing the skip once via danmaku.
async fn skip_stream_if_previously_warned(stream: &mut StreamCandidate, cfg: &Config) {
    if !stream.is_live || !should_skip_due_to_warning(&stream.channel_name) {
        return;
    }

    if should_skip_due_to_warned(&stream.channel_name) {
        tracing::warn!("⚠️ 跳过频道 {} - 之前因警告/切断停止", stream.channel_name);
        if cfg.bililive.enable_danmaku_command && !is_danmaku_commands_enabled() {
            enable_danmaku_commands(true);
            if let Err(e) = send_danmaku(
                cfg,
                &format!(
                    "⚠️ {} 因警告/切断被跳过，可使用弹幕指令换台",
                    stream.channel_name
                ),
            )
            .await
            {
                tracing::error!("Failed to send danmaku: {}", e);
            }
        }
    }

    stream.is_live = false;
}

/// Marks a candidate offline when its title/topic contains a banned streaming
/// keyword, warning once per (platform, keyword, title) combination.
async fn skip_stream_if_banned_keyword(
    stream: &mut StreamCandidate,
    keywords: &[String],
    cfg: &Config,
) {
    if !stream.is_live {
        return;
    }

    let stream_title = stream.stream_title();
    let default_title = "无标题".to_string();
    let title_str = stream_title.as_ref().unwrap_or(&default_title);
    let Some(keyword) = keywords.iter().find(|k| {
        stream_title
            .as_ref()
            .map_or(false, |t| t.contains(k.as_str()))
    }) else {
        return;
    };

    let platform = stream.platform.code();
    let should_warn = {
        let mut last_warning =
            recover_mutex_lock(&LAST_BANNED_KEYWORD_WARNING, "last banned keyword warning");
        let current_warning = format!("{}:{}:{}", platform, keyword, title_str);
        if last_warning.as_ref() != Some(&current_warning) {
            *last_warning = Some(current_warning);
            true
        } else {
            false
        }
    };

    if should_warn {
        tracing::error!("{}直播标题/分区包含不支持的关键词: {}", platform, keyword);
        if let Err(e) =
            send_danmaku(cfg, &format!("错误：{}标题/分区含:{}", platform, keyword)).await
        {
            tracing::error!("Failed to send danmaku: {}", e);
        }
        if cfg.bililive.enable_danmaku_command {
            if !is_danmaku_commands_enabled() {
                enable_danmaku_commands(true);
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
            if let Err(e) = send_danmaku(cfg, "可使用弹幕指令进行换台").await {
                tracing::error!("Failed to send danmaku: {}", e);
            }
        }
    }

    stream.is_live = false;
}

/// Updates the Bilibili live cover from the source stream's thumbnail in the
/// background.
fn spawn_cover_update(cfg: &Config, platform: &str, channel_id: &str, stream_id: Option<String>) {
    let cfg = cfg.clone();
    let platform = platform.to_string();
    let channel_id = channel_id.to_string();
    tokio::spawn(async move {
        let proxy = if platform == "YT" {
            cfg.youtube.proxy.clone()
        } else {
            cfg.twitch.proxy.clone()
        };
        match get_thumbnail(&platform, &channel_id, stream_id.as_deref(), proxy).await {
            Ok(cover_path) if !cover_path.is_empty() => {
                if let Err(e) = bilibili::bili_change_cover(&cfg, &cover_path).await {
                    tracing::error!("B站直播间封面替换失败: {}", e);
                } else {
                    tracing::info!("B站直播间封面替换成功");
                }
            }
            Ok(_) => {
                tracing::warn!("跳过封面更新：缩略图下载失败");
            }
            Err(e) => {
                tracing::error!("获取缩略图失败: {}", e);
            }
        }
    });
}

fn load_streaming_banned_keywords() -> Vec<String> {
    let areas_path = match std::env::current_exe() {
        Ok(path) => path.with_file_name("areas.json"),
        Err(e) => {
            tracing::error!("Failed to get executable path: {}", e);
            return Vec::new();
        }
    };

    let content = match std::fs::read_to_string(&areas_path) {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("Failed to read areas.json: {}", e);
            return Vec::new();
        }
    };

    let data: serde_json::Value = match serde_json::from_str(&content) {
        Ok(d) => d,
        Err(e) => {
            tracing::error!("Failed to parse areas.json: {}", e);
            return Vec::new();
        }
    };

    if let Some(keywords) = data["streaming_banned_keywords"].as_array() {
        keywords
            .iter()
            .filter_map(|k| k.as_str().map(|s| s.to_string()))
            .collect()
    } else {
        tracing::warn!("areas.json 中未找到 streaming_banned_keywords，使用默认值");
        vec![
            "どうぶつの森".to_string(),
            "animal crossing".to_string(),
            "asmr".to_string(),
            "dbd".to_string(),
            "dead by daylight".to_string(),
            "l4d2".to_string(),
            "left 4 dead 2".to_string(),
            "gta".to_string(),
        ]
    }
}

async fn run_bilistream(ffmpeg_log_level: &str) -> Result<(), Box<dyn std::error::Error>> {
    // Initialize the logger with timestamp format : 2024-11-21 12:00:00
    // Only init if not already initialized (webui mode initializes it earlier)
    if !tracing::dispatcher::has_been_set() {
        init_logger();
    }

    if is_ffmpeg_running().await {
        // Stop any existing ffmpeg process
        stop_ffmpeg().await;
    }

    // Load config to check danmaku command setting
    let initial_cfg = load_config().await?;

    // Start danmaku client in background if not already running and if danmaku commands are enabled
    if !is_danmaku_running() && initial_cfg.bililive.enable_danmaku_command {
        run_danmaku();
        // Give the client a moment to start
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    'outer: loop {
        // Log outer loop restart for debugging channel switch issues
        tracing::debug!("🔄 外层循环开始 - 重新加载配置并检查频道状态");

        // Consume the previous reload signal before loading; updates that arrive during
        // load_config() remain set and will be picked up by the checks below.
        clear_config_updated();
        let mut cfg = load_config().await?;

        // Handle danmaku client based on enable_danmaku_command setting
        if cfg.bililive.enable_danmaku_command {
            // Start danmaku client if not running and commands are enabled
            if !is_danmaku_running() {
                run_danmaku();
            }
        } else {
            // Stop danmaku client if running and commands are disabled
            if is_danmaku_running() {
                tracing::info!("⏸️ 弹幕命令已禁用，停止弹幕客户端");
                stop_danmaku().await;
            }
        }

        // Validate YouTube/Twitch configuration
        if cfg.youtube.channel_id.is_empty() && cfg.twitch.channel_id.is_empty() {
            tracing::error!("❌ YouTube 和 Twitch 配置均为空");
            tracing::error!("请在 WebUI 中配置或手动编辑 config.json 文件");
            tracing::info!("💡 提示: 访问 WebUI 进行配置，或参考 config.json.example");
            // Sleep and continue to allow WebUI configuration
            wait_config_update_or_timeout(Duration::from_secs(cfg.interval)).await;
            continue 'outer;
        }

        if is_config_updated() {
            clear_config_updated();
            tracing::info!("🔄 检测到配置更新，重新加载配置并检查频道状态");
            continue 'outer;
        }

        // Check YouTube status (only if enabled)
        let (yt_live, mut yt_is_live, yt_area, yt_title, yt_m3u8_url, scheduled_start, yt_video_id) =
            if cfg.youtube.enable_monitor && !cfg.youtube.channel_id.is_empty() {
                let yt_live = YoutubeClient::new(
                    &cfg.youtube.channel_name,
                    &cfg.youtube.channel_id,
                    cfg.youtube.proxy.clone(),
                );
                let (yt_is_live, yt_area, yt_title, yt_m3u8_url, mut scheduled_start, yt_video_id) =
                    yt_live
                        .get_status()
                        .await
                        .unwrap_or((false, None, None, None, None, None));
                let max_scheduled_start = Local::now() + Duration::from_secs(2 * 24 * 60 * 60);
                if scheduled_start
                    .as_ref()
                    .is_some_and(|start| start > &max_scheduled_start)
                {
                    scheduled_start = None;
                }
                (
                    Some(yt_live),
                    yt_is_live,
                    yt_area,
                    yt_title,
                    yt_m3u8_url,
                    scheduled_start,
                    yt_video_id,
                )
            } else {
                (None, false, None, None, None, None, None)
            };

        if is_config_updated() {
            clear_config_updated();
            tracing::info!("🔄 YouTube状态检查期间检测到配置更新，重新加载配置并检查频道状态");
            continue 'outer;
        }

        // Check Twitch status (only if enabled)
        let (tw_live, mut tw_is_live, tw_area, tw_title, tw_m3u8_url, tw_stream_id) =
            if cfg.twitch.enable_monitor && !cfg.twitch.channel_id.is_empty() {
                match TwitchClient::new(
                    &cfg.twitch.channel_id,
                    cfg.twitch.proxy_region.clone(),
                    cfg.twitch.proxy.clone(),
                ) {
                    Ok(tw_live) => {
                        let (tw_is_live, tw_area, tw_title, tw_m3u8_url, _, tw_stream_id) = tw_live
                            .get_status()
                            .await
                            .unwrap_or((false, None, None, None, None, None));
                        (
                            Some(tw_live),
                            tw_is_live,
                            tw_area,
                            tw_title,
                            tw_m3u8_url,
                            tw_stream_id,
                        )
                    }
                    Err(e) => {
                        tracing::warn!("Twitch 客户端初始化失败: {}", e);
                        (None, false, None, None, None, None)
                    }
                }
            } else {
                (None, false, None, None, None, None)
            };

        if is_config_updated() {
            clear_config_updated();
            tracing::info!("🔄 Twitch状态检查期间检测到配置更新，重新加载配置并检查频道状态");
            continue 'outer;
        }
        // Get Bilibili status
        let (bili_is_live, bili_title, bili_area_id) =
            match fetch_bili_live_status_logged(cfg.bililive.room).await {
                Ok(status) => status,
                Err(_) => {
                    tracing::warn!("⚠️ 将在下次循环重试");
                    wait_config_update_or_timeout(Duration::from_secs(cfg.interval)).await;
                    continue 'outer;
                }
            };
        let bili_area_name = get_area_name(bili_area_id)
            .unwrap_or_else(|| format!("未知分区 (ID: {})", bili_area_id));

        // Update status cache for WebUI
        bilistream::update_status_cache(bilistream::StatusData {
            bilibili: bilistream::BiliStatus {
                is_live: bili_is_live,
                title: bili_title.clone(),
                area_id: bili_area_id,
                area_name: bili_area_name,
                stream_quality: None,
                stream_speed: None,
                stream_cache_speed: None,
                stream_bitrate_kbps: None,
                stream_cache_bitrate_kbps: None,
                stream_fps: None,
                stream_frame: None,
                stream_total_bytes: 0,
                stream_cache_total_bytes: 0,
                hls_cache_active: false,
                enable_danmaku_command: cfg.bililive.enable_danmaku_command,
            },
            youtube: if cfg.youtube.enable_monitor && !cfg.youtube.channel_id.is_empty() {
                let yt_area_name = get_area_name(cfg.youtube.area_v2)
                    .unwrap_or_else(|| format!("未知分区 (ID: {})", cfg.youtube.area_v2));
                Some(bilistream::YtStatus {
                    is_live: yt_is_live,
                    title: yt_title.clone(),
                    topic: yt_area.clone(),
                    channel_name: cfg.youtube.channel_name.clone(),
                    channel_id: cfg.youtube.channel_id.clone(),
                    quality: cfg.youtube.quality.clone(),
                    area_id: cfg.youtube.area_v2,
                    area_name: yt_area_name,
                    crop_enabled: cfg.youtube.crop.is_some(),
                    ffmpeg_cache_enabled: cfg.youtube.ffmpeg_cache.enabled,
                    ffmpeg_cache_latency_secs: cfg.youtube.ffmpeg_cache.latency_secs,
                })
            } else {
                None
            },
            twitch: if cfg.twitch.enable_monitor && !cfg.twitch.channel_id.is_empty() {
                let tw_area_name = get_area_name(cfg.twitch.area_v2)
                    .unwrap_or_else(|| format!("未知分区 (ID: {})", cfg.twitch.area_v2));
                Some(bilistream::TwStatus {
                    is_live: tw_is_live,
                    title: tw_title.clone(),
                    game: tw_area.clone(),
                    channel_name: cfg.twitch.channel_name.clone(),
                    channel_id: cfg.twitch.channel_id.clone(),
                    quality: cfg.twitch.quality.clone(),
                    area_id: cfg.twitch.area_v2,
                    area_name: tw_area_name,
                    crop_enabled: cfg.twitch.crop.is_some(),
                    ffmpeg_cache_enabled: cfg.twitch.ffmpeg_cache.enabled,
                    ffmpeg_cache_latency_secs: cfg.twitch.ffmpeg_cache.latency_secs,
                })
            } else {
                None
            },
        });

        let mut yt_stream = StreamCandidate {
            platform: StreamPlatform::Youtube,
            is_live: yt_is_live,
            topic: yt_area,
            title: yt_title,
            m3u8_url: yt_m3u8_url,
            stream_id: yt_video_id,
            channel_name: cfg.youtube.channel_name.clone(),
            channel_id: cfg.youtube.channel_id.clone(),
            area_v2: cfg.youtube.area_v2,
        };
        let mut tw_stream = StreamCandidate {
            platform: StreamPlatform::Twitch,
            is_live: tw_is_live,
            topic: tw_area,
            title: tw_title,
            m3u8_url: tw_m3u8_url,
            stream_id: tw_stream_id,
            channel_name: cfg.twitch.channel_name.clone(),
            channel_id: cfg.twitch.channel_id.clone(),
            area_v2: cfg.twitch.area_v2,
        };

        yt_is_live = yt_stream.is_live;
        tw_is_live = tw_stream.is_live;

        if cfg.enable_anti_collision {
            match handle_collisions(&mut yt_is_live, &mut tw_is_live).await? {
                CollisionResult::Continue => continue 'outer,
                CollisionResult::Proceed => (),
            }
            yt_stream.is_live = yt_is_live;
            tw_stream.is_live = tw_is_live;
        }

        if yt_stream.is_live || tw_stream.is_live {
            NO_LIVE.store(false, Ordering::SeqCst);

            // Skip channels previously stopped due to a warning/cut-off.
            skip_stream_if_previously_warned(&mut yt_stream, &cfg).await;
            skip_stream_if_previously_warned(&mut tw_stream, &cfg).await;

            // Check if config was updated by danmaku command after warning filtering
            if is_config_updated() {
                clear_config_updated();
                tracing::info!("🔄 检测到配置更新（弹幕指令），重新加载配置并检查频道状态");
                continue 'outer;
            }

            // If both channels are skipped after filtering, continue to next iteration
            if !yt_stream.is_live && !tw_stream.is_live {
                wait_config_update_or_timeout(Duration::from_secs(cfg.interval)).await;
                continue 'outer;
            }

            let streaming_banned_keywords = load_streaming_banned_keywords();
            skip_stream_if_banned_keyword(&mut yt_stream, &streaming_banned_keywords, &cfg).await;
            skip_stream_if_banned_keyword(&mut tw_stream, &streaming_banned_keywords, &cfg).await;

            if !yt_stream.is_live && !tw_stream.is_live {
                wait_config_update_or_timeout(Duration::from_secs(cfg.interval)).await;
                continue 'outer;
            }

            let Some(mut selected_stream) = select_stream(&yt_stream, &tw_stream) else {
                if yt_stream.is_live && yt_stream.m3u8_url.is_none() {
                    tracing::warn!(
                        "YouTube 直播 {} 缺少可播放的流URL，跳过本轮转播",
                        yt_stream.channel_name
                    );
                }
                if tw_stream.is_live && tw_stream.m3u8_url.is_none() {
                    tracing::warn!(
                        "Twitch 直播 {} 缺少可播放的流URL，跳过本轮转播",
                        tw_stream.channel_name
                    );
                }
                tracing::warn!("未找到可转播的直播候选，等待下一轮检查");
                wait_config_update_or_timeout(Duration::from_secs(cfg.interval)).await;
                continue 'outer;
            };
            let Some(mut m3u8_url) = selected_stream.m3u8_url.take() else {
                tracing::warn!(
                    "{} 直播 {} 缺少可播放的流URL，等待下一轮检查",
                    selected_stream.platform.code(),
                    selected_stream.channel_name
                );
                wait_config_update_or_timeout(Duration::from_secs(cfg.interval)).await;
                continue 'outer;
            };

            // Clear warning stop since we have a playable stream candidate.
            clear_warning_stop();

            let platform = selected_stream.platform.code();
            let channel_name = selected_stream.channel_name.clone();
            let channel_id = selected_stream.channel_id.clone();
            let mut area_v2 = selected_stream.area_v2;
            let cfg_title = selected_stream.cfg_title();
            let current_video_id = selected_stream.stream_id.clone();
            let mut title = selected_stream.title.clone();

            // Check if video/stream ID has changed
            let video_id_changed = {
                let mut last_id = recover_mutex_lock(&LAST_VIDEO_ID, "last video id");
                let changed = last_id.as_ref() != current_video_id.as_ref();
                if changed {
                    *last_id = current_video_id.clone();
                }
                changed
            };
            tracing::info!(
                "{} 正在 {} 直播, 标题:\n          {}",
                channel_name,
                platform,
                title.clone().unwrap_or_else(|| "无标题".to_string())
            );

            if selected_stream.topic.is_some() && title.is_some() {
                title = selected_stream.stream_title();
                selected_stream.title = title.clone();
            }
            let default_title = "无标题".to_string();
            let title_str = title.as_ref().unwrap_or(&default_title);
            area_v2 = check_area_id_with_title(title_str, area_v2);
            if area_v2 == 86 && cfg.enable_lol_monitor {
                let puuid = get_puuid(&channel_name)?;
                if puuid != "" {
                    monitor_lol_game(puuid).await?;
                }
            } else {
                INVALID_ID_DETECTED.store(false, Ordering::SeqCst);
            }

            // Disable danmaku commands only once we are committed to this stream (past skip paths).
            if is_danmaku_commands_enabled() {
                enable_danmaku_commands(false);
            }
            // Reuse bili_is_live, bili_title, bili_area_id from earlier check (line 200)
            if !bili_is_live && (area_v2 != 86 || !INVALID_ID_DETECTED.load(Ordering::SeqCst)) {
                tracing::info!("B站未直播");
                let area_name = area_label(area_v2);

                // Try to start live, but don't crash on error
                match bili_start_live(&mut cfg, area_v2).await {
                    Ok(_) => {
                        if bili_title != cfg_title {
                            if let Err(e) = bili_change_live_title(&cfg, &cfg_title).await {
                                tracing::error!("B站直播标题变更失败: {}", e);
                            }
                        }
                        tracing::info!(
                            "B站已开播，标题为 {}，分区为 {} （ID: {}）",
                            cfg_title,
                            area_name,
                            area_v2
                        );
                        // Clear banned keyword warning when successfully starting a new stream
                        *recover_mutex_lock(
                            &LAST_BANNED_KEYWORD_WARNING,
                            "last banned keyword warning",
                        ) = None;
                    }
                    Err(e) => {
                        let error = e.to_string();
                        let message = error
                            .strip_prefix(BILI_START_TEMP_BAN_PREFIX)
                            .unwrap_or(&error);
                        tracing::error!("B站开播失败: {}", message);
                        if error.starts_with(BILI_START_TEMP_BAN_PREFIX) {
                            let mut config_changed = false;

                            if cfg.youtube.enable_monitor {
                                cfg.youtube.enable_monitor = false;
                                config_changed = true;
                            }

                            if cfg.twitch.enable_monitor {
                                cfg.twitch.enable_monitor = false;
                                config_changed = true;
                            }

                            if config_changed {
                                tracing::warn!(
                                    "检测到B站异常开播限制，已关闭 YouTube 和 Twitch 监控"
                                );
                                if let Err(save_err) = save_config(&cfg).await {
                                    tracing::error!("保存配置失败: {}", save_err);
                                }
                            }
                        }
                        tracing::warn!("⚠️ 将在下次循环重试");
                        wait_config_update_or_timeout(Duration::from_secs(cfg.interval)).await;
                        continue 'outer;
                    }
                }

                // If auto_cover is enabled, update Bilibili live cover in background
                if cfg.auto_cover
                    && (bili_title != cfg_title || bili_area_id != area_v2 || video_id_changed)
                {
                    spawn_cover_update(&cfg, platform, &channel_id, current_video_id.clone());
                }
            } else {
                // 如果target channel改变，则变更B站直播标题
                if bili_title != cfg_title {
                    if let Err(e) = bili_change_live_title(&cfg, &cfg_title).await {
                        tracing::error!("B站直播标题变更失败: {}", e);
                    } else {
                        tracing::info!("B站直播标题变更 （{}->{}）", bili_title, cfg_title);
                        // title is 【转播】频道名
                        let bili_channel_name = bili_title.split("【转播】").last().unwrap();
                        if bili_channel_name != channel_name {
                            tokio::time::sleep(Duration::from_secs(2)).await;
                            if let Err(e) = send_danmaku(
                                &cfg,
                                &format!("换台：{} → {}", bili_channel_name, channel_name),
                            )
                            .await
                            {
                                tracing::error!("发送弹幕失败: {}", e);
                            }
                        }
                    }
                }
                // If area_v2 changed, update Bilibili live area
                if bili_area_id != area_v2 {
                    if let Err(e) = update_area(bili_area_id, area_v2).await {
                        tracing::error!("B站分区更新失败: {}", e);
                    } else {
                        tokio::time::sleep(Duration::from_secs(2)).await;
                        if let Err(e) = bili_change_live_title(&cfg, &cfg_title).await {
                            tracing::error!("B站直播标题变更失败: {}", e);
                        }
                    }
                }
                // If auto_cover is enabled, update Bilibili live cover
                if cfg.auto_cover
                    && (bili_title != cfg_title || bili_area_id != area_v2 || video_id_changed)
                {
                    spawn_cover_update(&cfg, platform, &channel_id, current_video_id.clone());
                }
            }

            let source_client = selected_source_client(&selected_stream, &yt_live, &tw_live);

            // Execute ffmpeg with platform-specific locks
            // Main ffmpeg monitoring loop - blocks until stream ends
            let ffmpeg_loop_exit_reason = loop {
                let proxy = if platform == "YT" {
                    cfg.youtube.proxy.clone()
                } else {
                    cfg.twitch.proxy.clone()
                };

                // Get crop configuration if enabled
                let crop = if platform == "YT" {
                    cfg.youtube
                        .crop
                        .as_ref()
                        .map(|c| (c.width, c.height, c.x, c.y))
                } else {
                    cfg.twitch
                        .crop
                        .as_ref()
                        .map(|c| (c.width, c.height, c.x, c.y))
                };

                let cache = if platform == "YT" {
                    &cfg.youtube.ffmpeg_cache
                } else {
                    &cfg.twitch.ffmpeg_cache
                };

                ffmpeg(
                    cfg.bililive.bili_rtmp_url.clone(),
                    cfg.bililive.bili_rtmp_key.clone(),
                    m3u8_url.clone(),
                    proxy,
                    ffmpeg_log_level.to_string(),
                    crop,
                    FfmpegCacheOptions {
                        enabled: cache.enabled,
                        latency_secs: cache.latency_secs,
                    },
                )
                .await;

                // Wait for ffmpeg to exit (blocking)
                let exit_status = wait_ffmpeg().await;

                if let Some(status) = exit_status {
                    if status.success() {
                        tracing::info!("✅ ffmpeg正常退出");
                    } else {
                        tracing::warn!("⚠️ ffmpeg异常退出: {:?}", status);
                    }
                } else {
                    tracing::warn!("⚠️ ffmpeg进程已停止");
                }

                // Check if stream is still live before restarting
                tokio::time::sleep(Duration::from_secs(2)).await;

                let (current_is_live, _, _, new_m3u8_url, _, _) =
                    source_client_status(&source_client).await;
                // On error, assume still live and retry next iteration.
                let (bili_is_live, _, _) = fetch_bili_live_status_logged(cfg.bililive.room)
                    .await
                    .unwrap_or((true, String::new(), 0));

                if !current_is_live {
                    tracing::info!("直播已结束，停止ffmpeg监控循环");
                    break FfmpegLoopExitReason::SourceEnded;
                }

                if !bili_is_live {
                    tracing::info!("直播已结束，停止ffmpeg监控循环");
                    break FfmpegLoopExitReason::BiliStopped;
                }

                // Check if manual restart was requested (force immediate restart)
                if was_manual_restart() {
                    let exit_reason = FfmpegLoopExitReason::IntentionalRestart {
                        target_m3u8_available: new_m3u8_url.is_some(),
                    };
                    tracing::info!("🔄 检测到手动重启请求，立即退出ffmpeg监控循环");
                    break exit_reason;
                }

                // Check if config was updated (channel switch)
                // Only break if stream has ended, otherwise continue streaming current channel
                if is_config_updated() {
                    tracing::info!("🔄 检测到配置更新请求，但当前流仍在进行，继续转播直到流结束");
                    // Don't break, let the stream continue until it naturally ends
                }

                // Update m3u8 URL if it changed
                if let Some(new_m3u8_url) = new_m3u8_url {
                    if new_m3u8_url != m3u8_url {
                        tracing::info!("🔄 检测到流URL变化，使用新URL重启");
                        m3u8_url = new_m3u8_url;
                    }
                }

                // Stream is still live but ffmpeg exited, restart it
                tracing::info!("🔄 流仍在进行，重启ffmpeg...");
                tokio::time::sleep(Duration::from_secs(1)).await;
            };

            // Check the actual reason for ffmpeg loop exit
            let manual_stop = was_manual_stop();
            let manual_restart = was_manual_restart();
            let warning_skip = should_skip_due_to_warning(&channel_name);
            let restart_exit_should_skip_danmaku =
                restart_exit_should_skip_end_danmaku(ffmpeg_loop_exit_reason);

            if manual_restart {
                clear_manual_restart();
            }
            if manual_stop
                && !restart_exit_should_skip_danmaku
                && !matches!(ffmpeg_loop_exit_reason, FfmpegLoopExitReason::BiliStopped)
            {
                clear_manual_stop();
            }

            // Clear crop settings for both platforms when stream ends
            // Don't clear on manual restart - let it apply for the restarted stream
            if !restart_exit_should_skip_danmaku {
                let mut config_changed = false;

                if cfg.youtube.crop.is_some() {
                    tracing::info!("🔄 清除YouTube裁剪设置");
                    cfg.youtube.crop = None;
                    config_changed = true;
                }

                if cfg.twitch.crop.is_some() {
                    tracing::info!("🔄 清除Twitch裁剪设置");
                    cfg.twitch.crop = None;
                    config_changed = true;
                }

                if config_changed {
                    if let Err(e) = save_config(&cfg).await {
                        tracing::error!("保存配置失败: {}", e);
                    }
                }
            }

            // Check current live status to determine what actually happened
            let (current_is_live, _, _, _, _, _) = source_client_status(&source_client).await;
            // On error, assume still live to avoid incorrect status messages.
            let (bili_is_live, _, _) = fetch_bili_live_status_logged(cfg.bililive.room)
                .await
                .unwrap_or((true, String::new(), 0));

            // Determine what happened and send appropriate message
            if restart_exit_should_skip_danmaku {
                if manual_stop {
                    clear_manual_stop();
                }
                tracing::info!("Stream was manually restarted, skipping end danmaku");
            } else if manual_stop
                && matches!(ffmpeg_loop_exit_reason, FfmpegLoopExitReason::BiliStopped)
            {
                clear_manual_stop();
                tracing::info!("Stream was stopped manually, skipping end danmaku");
            } else if warning_skip {
                tracing::info!("Stream was stopped due to warning/cut off");
            } else if !current_is_live && bili_is_live {
                // Source stream ended but B站 is still live
                tracing::info!("{} 直播结束", channel_name);

                // Check if B站 has been live for more than 8 hours; if so, stop it
                match get_bili_live_time(cfg.bililive.room).await {
                    Ok(Some(live_start)) => {
                        let duration = chrono::Local::now().signed_duration_since(live_start);
                        if duration.num_hours() >= 3 {
                            tracing::info!(
                                "B站直播已超过3小时（{}小时），自动停播",
                                duration.num_hours()
                            );
                            if let Err(e) = bili_stop_live(&cfg).await {
                                tracing::error!("自动停播失败: {}", e);
                            }
                        }
                    }
                    Ok(None) => {}
                    Err(e) => {
                        tracing::warn!("获取B站直播时间失败: {}", e);
                    }
                }

                if cfg.bililive.enable_danmaku_command {
                    enable_danmaku_commands(true);
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    if let Err(e) = send_danmaku(
                        &cfg,
                        &format!("{} 直播结束，可使用弹幕指令进行换台", channel_name),
                    )
                    .await
                    {
                        tracing::error!("Failed to send danmaku: {}", e);
                    }
                } else {
                    if let Err(e) = send_danmaku(&cfg, &format!("{} 直播结束", channel_name)).await
                    {
                        tracing::error!("Failed to send danmaku: {}", e);
                    }
                }
            } else if !bili_is_live {
                // B站 stream was stopped
                tracing::info!("B站直播已停止");
                if cfg.bililive.enable_danmaku_command {
                    enable_danmaku_commands(true);
                }
            } else if current_is_live && bili_is_live {
                // Both streams are still live - this was likely a technical issue
                tracing::info!("流传输中断，但直播仍在进行");
                if cfg.bililive.enable_danmaku_command {
                    enable_danmaku_commands(true);
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    if let Err(e) = send_danmaku(
                        &cfg,
                        &format!("{} 流传输中断，可使用弹幕指令进行换台", channel_name),
                    )
                    .await
                    {
                        tracing::error!("Failed to send danmaku: {}", e);
                    }
                }
            } else {
                // Fallback case
                tracing::info!("流传输已停止");
                if cfg.bililive.enable_danmaku_command {
                    enable_danmaku_commands(true);
                }
            }
        } else {
            // 计划直播(预告窗)
            if let Some(scheduled_start) = scheduled_start {
                let current_message = box_message(
                    &yt_stream.channel_name,
                    cfg.youtube.enable_monitor,
                    Some(scheduled_start),
                    yt_stream.title.as_deref(),
                    &tw_stream.channel_name,
                    cfg.twitch.enable_monitor,
                );
                update_last_status_message(current_message);
            } else {
                if !NO_LIVE.load(Ordering::SeqCst) {
                    let current_message = box_message(
                        &yt_stream.channel_name,
                        cfg.youtube.enable_monitor,
                        None,
                        None, // No title when not streaming
                        &tw_stream.channel_name,
                        cfg.twitch.enable_monitor,
                    );
                    print!("{}", current_message);
                    let mut last = recover_mutex_lock(&LAST_MESSAGE, "last message");
                    *last = Some(current_message.into_boxed_str());
                    NO_LIVE.store(true, Ordering::SeqCst);
                }
            }
            if cfg.bililive.enable_danmaku_command && !is_danmaku_commands_enabled() {
                enable_danmaku_commands(true);
            }

            // Check if config was updated (skip waiting if so)
            if is_config_updated() {
                clear_config_updated();
                tracing::info!("🔄 检测到配置更新，重新加载配置并检查频道状态");
                continue 'outer;
            }

            // Sleep with periodic checks for config updates
            let sleep_duration = cfg.interval;
            let check_interval = 2; // Check every 2 seconds
            let mut elapsed = 0;

            while elapsed < sleep_duration {
                let sleep_time = std::cmp::min(check_interval, sleep_duration - elapsed);
                if wait_config_update_or_timeout(Duration::from_secs(sleep_time)).await {
                    clear_config_updated();
                    tracing::info!("🔄 等待期间检测到配置更新，重新加载配置并检查频道状态");
                    continue 'outer;
                }
                elapsed += sleep_time;

                // Check if config was updated during sleep
                if is_config_updated() {
                    clear_config_updated();
                    tracing::info!("🔄 等待期间检测到配置更新，重新加载配置并检查频道状态");
                    continue 'outer;
                }
            }
        }
    }
}

fn box_message(
    yt_channel: &str,
    yt_monitor_enabled: bool,
    scheduled_time: Option<DateTime<Local>>,
    title: Option<&str>,
    tw_channel: &str,
    tw_monitor_enabled: bool,
) -> String {
    // Calculate YouTube line
    let yt_line = if !yt_monitor_enabled {
        format!("YT: 监听已关闭")
    } else if let Some(scheduled_time) = scheduled_time {
        format!(
            "YT: {} 未直播，计划于 {} 开始，",
            yt_channel,
            scheduled_time.format(MESSAGE_TIME_FORMAT)
        )
    } else {
        format!(
            "YT: {} 未直播                                   ",
            yt_channel
        )
    };

    // Calculate Twitch line
    let tw_line = if !tw_monitor_enabled {
        format!("TW: 监听已关闭")
    } else {
        format!("TW: {} 未直播", tw_channel)
    };

    // Calculate width based on the longer line
    let yt_width = yt_line.width() + 2;
    let tw_width = tw_line.width() + 2;
    let width = std::cmp::max(yt_width, tw_width);

    let mut message = format!(
        "\r\x1b[K\x1b[1m┌{:─<width$}┐\n\
         │ {} │\n",
        "",
        yt_line,
        width = width
    );

    // Add padding for YouTube line if needed
    let yt_padding = width - 2 - yt_line.width();
    if yt_padding > 0 {
        // Remove the line we just added and re-add with proper padding
        message = format!(
            "\r\x1b[K\x1b[1m┌{:─<width$}┐\n\
             │ {}{} │\n",
            "",
            yt_line,
            " ".repeat(yt_padding),
            width = width
        );
    }

    if let Some(title_text) = title {
        let wrapped_title = textwrap::fill(title_text, width.saturating_sub(6).max(10));
        for line in wrapped_title.lines() {
            let padding = width.saturating_sub(6).saturating_sub(line.width());
            message.push_str(&format!("│     {}{} │\n", line, " ".repeat(padding)));
        }
    }

    message.push_str(&format!("├{:─<width$}┤\n", "", width = width));

    let tw_padding = width.saturating_sub(2).saturating_sub(tw_line.width());
    message.push_str(&format!(
        "│ {}{} │\n\
         └{:─<width$}┘\x1b[0m\n",
        tw_line,
        " ".repeat(tw_padding),
        "",
        width = width
    ));

    message
}

async fn monitor_lol_game(puuid: String) -> Result<(), Box<dyn Error>> {
    let cfg = load_config().await?;

    let interval = cfg.lol_monitor_interval.unwrap_or(1);
    let Some(riot_api_key) = normalized_api_key(cfg.riot_api_key.as_deref()) else {
        tracing::warn!("LOL 监控已启用，但 Riot API Key 未配置，跳过本次检测");
        return Ok(());
    };
    thread::spawn(move || {
        let rt = match tokio::runtime::Runtime::new() {
            Ok(runtime) => runtime,
            Err(e) => {
                tracing::error!("LOL 监控运行时创建失败: {}", e);
                return;
            }
        };
        loop {
            rt.block_on(async {
                if let Ok(Some(riot_ids)) = current_game_riot_ids(&riot_api_key, &puuid).await {
                    let ids = format!("{:?}", riot_ids);
                    // tracing::info!("In game players: {}", ids);
                    let invalid_words_path = std::env::current_exe()
                        .ok()
                        .and_then(|p| p.parent().map(|p| p.join("invalid_words.txt")));
                    if let Some(path) = invalid_words_path {
                        if let Ok(invalid_words) = std::fs::read_to_string(path) {
                            if let Some(word) =
                                invalid_words.lines().find(|word| ids.contains(word))
                            {
                                INVALID_ID_DETECTED.store(true, Ordering::SeqCst);
                                let is_live = match get_bili_live_status(cfg.bililive.room).await {
                                    Ok((is_live, _, _)) => is_live,
                                    Err(e) => {
                                        tracing::error!("获取 B 站直播状态失败: {}", e);
                                        false
                                    }
                                };
                                if is_live {
                                    tracing::error!("检测到非法词汇:{}，停止直播", word);
                                    if let Err(e) = bili_stop_live(&cfg).await {
                                        tracing::error!("停止 B 站直播失败: {}", e);
                                    }
                                    stop_ffmpeg().await;
                                    if let Err(e) =
                                        send_danmaku(&cfg, "检测到玩家ID存在违🈲词汇，停止直播")
                                            .await
                                    {
                                        tracing::error!("Failed to send danmaku: {}", e);
                                    }
                                    if cfg.bililive.enable_danmaku_command
                                        && !is_danmaku_commands_enabled()
                                    {
                                        enable_danmaku_commands(true);
                                        thread::sleep(Duration::from_secs(2));
                                        if let Err(e) =
                                            send_danmaku(&cfg, "可使用弹幕指令进行换台").await
                                        {
                                            tracing::error!("Failed to send danmaku: {}", e);
                                        }
                                    }
                                    return;
                                } else {
                                    tracing::error!("检测到非法词汇:{}，不转播", word);
                                }
                            } else {
                                INVALID_ID_DETECTED.store(false, Ordering::SeqCst);
                            }
                        }
                    }
                }

                // Check if ffmpeg is still running
                if !ffmpeg::is_ffmpeg_running().await {
                    return;
                }
            });

            thread::sleep(Duration::from_secs(interval));
        }
    });
    tokio::time::sleep(Duration::from_secs(interval)).await;

    Ok(())
}

async fn update_area(current_area: u64, new_area: u64) -> Result<(), Box<dyn Error>> {
    if current_area != new_area {
        tracing::info!(
            "分区改变（{}->{})",
            area_label(current_area),
            area_label(new_area)
        );
        let cfg = load_config().await?;
        bili_update_area(&cfg, new_area).await?;
    }
    Ok(())
}

fn message_time_regex() -> Option<&'static Regex> {
    static MESSAGE_TIME_RE: OnceLock<Result<Regex, regex::Error>> = OnceLock::new();

    MESSAGE_TIME_RE
        .get_or_init(|| Regex::new(MESSAGE_TIME_PATTERN))
        .as_ref()
        .ok()
}

fn extract_time(message: &str) -> Option<NaiveDateTime> {
    let re = message_time_regex()?;
    re.find(message)
        .and_then(|m| NaiveDateTime::parse_from_str(m.as_str(), MESSAGE_TIME_FORMAT).ok())
}

fn message_without_time(message: &str) -> Cow<'_, str> {
    match message_time_regex() {
        Some(re) => re.replace_all(message, MESSAGE_TIME_PLACEHOLDER),
        None => Cow::Borrowed(message),
    }
}

fn should_update_status_message(last_message: &str, current_message: &str) -> bool {
    if last_message == current_message {
        return false;
    }

    let time_diff = if let Some(last_time) = extract_time(last_message) {
        if let Some(current_time) = extract_time(current_message) {
            (current_time - last_time).num_minutes().abs()
        } else {
            i64::MAX
        }
    } else {
        i64::MAX
    };

    time_diff > MESSAGE_UPDATE_TIME_THRESHOLD_MINUTES
        || message_without_time(last_message) != message_without_time(current_message)
}

fn update_last_status_message(current_message: String) {
    let message_to_print = {
        let mut last = recover_mutex_lock(&LAST_MESSAGE, "last message");
        let should_update = last
            .as_deref()
            .map(|last_msg| should_update_status_message(last_msg, &current_message))
            .unwrap_or(true);

        if should_update {
            *last = Some(current_message.clone().into_boxed_str());
            Some(current_message)
        } else {
            None
        }
    };

    if let Some(message) = message_to_print {
        print!("{}", message);
    }
}

async fn check_collision(
    target_name: &str,
    aliases: &[String],
) -> Result<
    Option<(
        String, // room name
        i32,    // room id
        String, // target channel name
    )>,
    Box<dyn std::error::Error>,
> {
    let cfg = load_config().await?;
    for (room_name, room_id) in cfg.anti_collision_list {
        match get_bili_live_status(room_id).await {
            Ok((true, title, _)) => {
                // Check if title contains the target name or aliases
                let contains_target = title.contains(target_name)
                    || aliases.iter().any(|alias| title.contains(alias));

                if contains_target {
                    // Check if this is a multi-channel stream (contains multiple channels)
                    let is_multi_channel = is_multi_channel_stream(&title);

                    if is_multi_channel {
                        tracing::debug!(
                            "📺 检测到多频道转播，跳过撞车检测: {} - {}",
                            room_name,
                            title
                        );
                        continue; // Skip collision detection for multi-channel streams
                    }

                    // This appears to be a single-channel stream, flag as collision
                    tracing::debug!(
                        "🚨 检测到撞车: {} ({}) 正在转播 {}",
                        room_name,
                        room_id,
                        target_name
                    );
                    return Ok(Some((room_name.clone(), room_id, target_name.to_string())));
                }
            }
            Err(e) => tracing::error!("获取防撞直播间 {} 状态失败: {}", room_id, e),
            _ => (),
        }
    }
    Ok(None)
}

/// Check if a stream title indicates multiple channels (not exclusive to target)
fn is_multi_channel_stream(title: &str) -> bool {
    // Load channels.json and check if multiple channels appear in the title
    if let Ok(is_multi) = has_multiple_channels_in_title(title) {
        return is_multi;
    }

    false
}

/// Check if multiple channel names from channels.json appear in the title
fn has_multiple_channels_in_title(title: &str) -> Result<bool, Box<dyn std::error::Error>> {
    // Get channels.json path
    let channels_path = std::env::current_exe()?.with_file_name("channels.json");

    if !channels_path.exists() {
        return Ok(false);
    }

    let channels_content = std::fs::read_to_string(&channels_path)?;
    let channels_json: serde_json::Value = serde_json::from_str(&channels_content)?;

    let mut found_channels = 0;
    let title_lower = title.to_lowercase();

    // Check current format: channels[].name and channels[].aliases
    if let Some(channels) = channels_json.get("channels").and_then(|v| v.as_array()) {
        for channel in channels {
            let mut channel_found = false;

            // Check main channel name
            if let Some(name) = channel.get("name").and_then(|v| v.as_str()) {
                if title_lower.contains(&name.to_lowercase()) {
                    channel_found = true;
                }
            }

            // Check aliases if main name not found
            if !channel_found {
                if let Some(aliases) = channel.get("aliases").and_then(|v| v.as_array()) {
                    for alias in aliases {
                        if let Some(alias_str) = alias.as_str() {
                            if title_lower.contains(&alias_str.to_lowercase()) {
                                channel_found = true;
                                break;
                            }
                        }
                    }
                }
            }

            if channel_found {
                found_channels += 1;
                // Early return: if we found 2+ channels, it's definitely multi-channel
                if found_channels >= 2 {
                    return Ok(true);
                }
            }
        }
    }

    // Legacy support: Check old format YT_channels[].channel_name
    if let Some(yt_channels) = channels_json.get("YT_channels").and_then(|v| v.as_array()) {
        for channel in yt_channels {
            if let Some(name) = channel.get("channel_name").and_then(|v| v.as_str()) {
                if title_lower.contains(&name.to_lowercase()) {
                    found_channels += 1;
                    // Early return for legacy format too
                    if found_channels >= 2 {
                        return Ok(true);
                    }
                }
            }
        }
    }

    // Legacy support: Check old format TW_channels[].channel_name
    if let Some(tw_channels) = channels_json.get("TW_channels").and_then(|v| v.as_array()) {
        for channel in tw_channels {
            if let Some(name) = channel.get("channel_name").and_then(|v| v.as_str()) {
                if title_lower.contains(&name.to_lowercase()) {
                    found_channels += 1;
                    // Early return for legacy format too
                    if found_channels >= 2 {
                        return Ok(true);
                    }
                }
            }
        }
    }

    // Return false if we found 0 or 1 channels
    Ok(false)
}

async fn handle_collisions(
    yt_is_live: &mut bool,
    tw_is_live: &mut bool,
) -> Result<CollisionResult, Box<dyn Error>> {
    let cfg = load_config().await?;

    let mut yt_collision = None;
    let mut tw_collision = None;

    // YouTube collision check
    if *yt_is_live {
        let target_name = &cfg.youtube.channel_name;
        let aliases = get_aliases(target_name)?;
        yt_collision = check_collision(target_name, &aliases).await?;
    }

    // Twitch collision check
    if *tw_is_live {
        let target_name = &cfg.twitch.channel_name;
        let aliases = get_aliases(target_name)?;
        tw_collision = check_collision(target_name, &aliases).await?;
    }

    match (yt_collision, tw_collision) {
        (Some(yt_collision), Some(tw_collision)) => {
            let (yt_room_name, yt_room_id, yt_target_name) = yt_collision;
            let (tw_room_name, tw_room_id, tw_target_name) = tw_collision;

            // Check if we're already in a dual-platform collision state (regardless of specific room)
            let already_in_dual_collision = {
                let last_collision = recover_mutex_lock(&LAST_COLLISION, "last collision");
                last_collision
                    .as_ref()
                    .map(|(_, _, platform)| platform.as_ref() == DUAL_COLLISION_PLATFORM)
                    .unwrap_or(false)
            };

            if !already_in_dual_collision {
                {
                    let mut last_collision = recover_mutex_lock(&LAST_COLLISION, "last collision");
                    *last_collision = Some((
                        yt_room_name.clone().into_boxed_str(),
                        yt_room_id,
                        DUAL_COLLISION_PLATFORM.into(),
                    ));
                }

                tracing::warn!("YouTube和Twitch均检测到撞车，跳过本次转播");
                // send_danmaku(&cfg, "🚨YT和TW双平台撞车").await?;
                // tokio::time::sleep(Duration::from_secs(2)).await;
                if let Err(e) = send_danmaku(
                    &cfg,
                    &format!("{}({})正在转{}", yt_room_name, yt_room_id, yt_target_name),
                )
                .await
                {
                    tracing::error!("Failed to send danmaku: {}", e);
                }
                if yt_room_name != tw_room_name {
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    if let Err(e) = send_danmaku(
                        &cfg,
                        &format!("{}({})正在转{}", tw_room_name, tw_room_id, tw_target_name),
                    )
                    .await
                    {
                        tracing::error!("Failed to send danmaku: {}", e);
                    }
                }
                tokio::time::sleep(Duration::from_secs(2)).await;
                if cfg.bililive.enable_danmaku_command && !is_danmaku_commands_enabled() {
                    enable_danmaku_commands(true);
                }
                if cfg.bililive.enable_danmaku_command {
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    if let Err(e) = send_danmaku(&cfg, "撞车：可使用弹幕指令进行换台").await
                    {
                        tracing::error!("Failed to send danmaku: {}", e);
                    }
                }
                tokio::time::sleep(Duration::from_secs(30)).await;
            }
            Ok(CollisionResult::Continue)
        }
        (Some(collision), None) | (None, Some(collision)) => {
            let (room_name, room_id, target_name) = collision;
            let is_youtube_collision = target_name == cfg.youtube.channel_name;
            let other_live = if is_youtube_collision {
                let ol = *tw_is_live;
                *yt_is_live = false;
                ol
            } else {
                let ol = *yt_is_live;
                *tw_is_live = false;
                ol
            };

            // Check if we're already in a collision state for this platform
            let already_in_collision = {
                let last_collision = recover_mutex_lock(&LAST_COLLISION, "last collision");
                last_collision
                    .as_ref()
                    .map(|(_, _, platform)| platform.as_ref() == target_name.as_str())
                    .unwrap_or(false)
            };

            if !other_live && !already_in_collision {
                {
                    let mut last_collision = recover_mutex_lock(&LAST_COLLISION, "last collision");
                    *last_collision = Some((
                        room_name.clone().into_boxed_str(),
                        room_id,
                        target_name.clone().into_boxed_str(),
                    ));
                }

                tracing::warn!(
                    "{}（{}）撞车，{}（{}）未开播",
                    room_name,
                    room_id,
                    if is_youtube_collision {
                        "Twitch"
                    } else {
                        "YouTube"
                    },
                    if is_youtube_collision {
                        cfg.twitch.channel_name.clone()
                    } else {
                        cfg.youtube.channel_name.clone()
                    }
                );
                if let Err(e) = send_danmaku(
                    &cfg,
                    &format!("{}({})正在转{}", room_name, room_id, target_name,),
                )
                .await
                {
                    tracing::error!("Failed to send danmaku: {}", e);
                }
                tokio::time::sleep(Duration::from_secs(2)).await;
                if cfg.bililive.enable_danmaku_command && !is_danmaku_commands_enabled() {
                    enable_danmaku_commands(true);
                }
                if cfg.bililive.enable_danmaku_command {
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    if let Err(e) = send_danmaku(&cfg, "撞车：可使用弹幕指令进行换台").await
                    {
                        tracing::error!("Failed to send danmaku: {}", e);
                    }
                }
                tokio::time::sleep(Duration::from_secs(30)).await;
                Ok(CollisionResult::Continue)
            } else {
                Ok(CollisionResult::Proceed)
            }
        }
        (None, None) => Ok(CollisionResult::Proceed),
    }
}

#[derive(Debug)]
struct LaunchArgs {
    bind: String,
    password: Option<String>,
    port: u16,
    ffmpeg_log_level: String,
    tray: bool,
}

#[derive(Debug)]
enum ParseOutcome {
    Launch(LaunchArgs),
    Help,
    Version,
}

fn env_nonempty(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn default_tray() -> bool {
    cfg!(target_os = "windows")
}

fn print_help() {
    println!(
        "bilistream {}\n\n\
Start the Web UI and stream monitor. Setup and controls are in the browser.\n\n\
Usage: bilistream [OPTIONS]\n\n\
Options:\n\
  --bind ADDR                 Listen address (default 127.0.0.1, or BILISTREAM_BIND)\n\
  -p, --port PORT             Web UI port (default 3150, or BILISTREAM_PORT)\n\
  --password PASSWORD         Web UI login password (or BILISTREAM_PASSWORD)\n\
  --ffmpeg-log-level LEVEL    error, info, or debug (default error)\n\
  --tray                      System tray (default on Windows)\n\
  --webui                     Console Web UI (default on Linux/macOS)\n\
  -h, --help                  Print help\n\
  -V, --version               Print version",
        env!("CARGO_PKG_VERSION")
    );
}

fn take_value(
    argv: &[String],
    i: &mut usize,
    inline: Option<&str>,
    name: &str,
) -> Result<String, String> {
    if let Some(value) = inline {
        return Ok(value.to_string());
    }
    *i += 1;
    argv.get(*i)
        .cloned()
        .ok_or_else(|| format!("missing value for {name}"))
}

fn parse_launch_args(argv: &[String]) -> Result<ParseOutcome, String> {
    parse_launch_args_with(
        argv,
        env_nonempty("BILISTREAM_BIND"),
        env_nonempty("BILISTREAM_PASSWORD"),
        env_nonempty("BILISTREAM_PORT"),
        env_nonempty("BILISTREAM_FFMPEG_LOG_LEVEL"),
        default_tray(),
    )
}

fn parse_launch_args_with(
    argv: &[String],
    env_bind: Option<String>,
    env_password: Option<String>,
    env_port: Option<String>,
    env_ffmpeg: Option<String>,
    mut tray: bool,
) -> Result<ParseOutcome, String> {
    let mut bind = env_bind.unwrap_or_else(|| "127.0.0.1".to_string());
    let mut password = env_password;
    let mut port: u16 = match env_port {
        Some(value) => value
            .parse()
            .map_err(|_| format!("invalid BILISTREAM_PORT: {value}"))?,
        None => 3150,
    };
    let mut ffmpeg_log_level = match env_ffmpeg {
        Some(value) if matches!(value.as_str(), "error" | "info" | "debug") => value,
        Some(value) => {
            return Err(format!(
                "invalid BILISTREAM_FFMPEG_LOG_LEVEL: {value} (error, info, debug)"
            ));
        }
        None => "error".to_string(),
    };

    let mut i = 1;
    while i < argv.len() {
        let arg = argv[i].as_str();
        let (key, inline) = match arg.split_once('=') {
            Some((key, value)) => (key, Some(value)),
            None => (arg, None),
        };

        match key {
            "-h" | "--help" => return Ok(ParseOutcome::Help),
            "-V" | "--version" => return Ok(ParseOutcome::Version),
            "--bind" => bind = take_value(argv, &mut i, inline, "--bind")?,
            "--password" => {
                password = Some(take_value(argv, &mut i, inline, "--password")?);
            }
            "-p" | "--port" => {
                let value = take_value(argv, &mut i, inline, "--port")?;
                port = value
                    .parse()
                    .map_err(|_| format!("invalid port: {value}"))?;
            }
            "--ffmpeg-log-level" => {
                let value = take_value(argv, &mut i, inline, "--ffmpeg-log-level")?;
                if !matches!(value.as_str(), "error" | "info" | "debug") {
                    return Err(format!(
                        "invalid --ffmpeg-log-level: {value} (error, info, debug)"
                    ));
                }
                ffmpeg_log_level = value;
            }
            "--tray" => {
                if inline.is_some() {
                    return Err("unexpected value for --tray".into());
                }
                tray = true;
            }
            "--webui" => {
                if inline.is_some() {
                    return Err("unexpected value for --webui".into());
                }
                tray = false;
            }
            other => return Err(format!("unknown argument: {other}")),
        }
        i += 1;
    }

    if bind.trim().is_empty() {
        bind = "127.0.0.1".to_string();
    }
    let password = password.filter(|value| !value.trim().is_empty());

    Ok(ParseOutcome::Launch(LaunchArgs {
        bind,
        password,
        port,
        ffmpeg_log_level,
        tray,
    }))
}

#[cfg(target_os = "windows")]
fn windows_needs_console(argv: &[String]) -> bool {
    let mut i = 1;
    while i < argv.len() {
        let raw = argv[i].as_str();
        let key = raw.split_once('=').map(|(k, _)| k).unwrap_or(raw);
        match key {
            "-h" | "--help" | "-V" | "--version" | "--webui" => return true,
            "--tray" => i += 1,
            "--bind" | "--password" | "--port" | "-p" | "--ffmpeg-log-level" => {
                if !raw.contains('=') {
                    i += 1;
                }
                i += 1;
            }
            _ => return true,
        }
    }
    false
}

#[cfg(target_os = "windows")]
fn allocate_windows_console() {
    unsafe {
        use std::ffi::CString;
        use winapi::um::consoleapi::AllocConsole;
        use winapi::um::fileapi::{CreateFileA, OPEN_EXISTING};
        use winapi::um::processenv::SetStdHandle;
        use winapi::um::winbase::{STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE};
        use winapi::um::winnt::{FILE_SHARE_READ, FILE_SHARE_WRITE, GENERIC_READ, GENERIC_WRITE};

        AllocConsole();

        let stdout_handle = CreateFileA(
            CString::new("CONOUT$").unwrap().as_ptr(),
            GENERIC_WRITE,
            FILE_SHARE_WRITE,
            std::ptr::null_mut(),
            OPEN_EXISTING,
            0,
            std::ptr::null_mut(),
        );

        let stderr_handle = CreateFileA(
            CString::new("CONOUT$").unwrap().as_ptr(),
            GENERIC_WRITE,
            FILE_SHARE_WRITE,
            std::ptr::null_mut(),
            OPEN_EXISTING,
            0,
            std::ptr::null_mut(),
        );

        let stdin_handle = CreateFileA(
            CString::new("CONIN$").unwrap().as_ptr(),
            GENERIC_READ,
            FILE_SHARE_READ,
            std::ptr::null_mut(),
            OPEN_EXISTING,
            0,
            std::ptr::null_mut(),
        );

        SetStdHandle(STD_OUTPUT_HANDLE, stdout_handle);
        SetStdHandle(STD_ERROR_HANDLE, stderr_handle);
        SetStdHandle(STD_INPUT_HANDLE, stdin_handle);
    }
}

fn apply_webui_listen(
    bind: &str,
    password: Option<String>,
) -> Result<(), Box<dyn std::error::Error>> {
    let bind = if bind.trim().is_empty() {
        "127.0.0.1"
    } else {
        bind
    };
    let password = password.filter(|value| !value.trim().is_empty());
    bilistream::webui::install_listen(bind, password)?;
    Ok(())
}

fn spawn_webui(port: u16) {
    tokio::spawn(async move {
        if let Err(e) = bilistream::webui::server::start_webui(port).await {
            tracing::error!("Web UI 服务器错误: {}", e);
        }
    });
}

fn spawn_deps_download() {
    tokio::spawn(async {
        tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
        if let Err(e) = bilistream::deps::ensure_all_dependencies().await {
            tracing::error!("⚠️ 下载依赖项失败: {}", e);
            tracing::error!("请手动从 GitHub 下载必需文件");
        }
    });
}

fn config_paths() -> Result<(std::path::PathBuf, std::path::PathBuf), Box<dyn std::error::Error>> {
    let exe = std::env::current_exe()?;
    Ok((
        exe.with_file_name("config.json"),
        exe.with_file_name("cookies.json"),
    ))
}

async fn wait_until_config_ready() {
    let Ok((config_path, cookies_path)) = config_paths() else {
        return;
    };

    if !config_path.exists() {
        tracing::warn!("⚠️ 配置文件不存在，等待用户配置...");
        tracing::info!("💡 请访问 Web UI 进行配置");
        loop {
            tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
            if config_path.exists() {
                tracing::info!("✅ 检测到配置文件，开始监控");
                break;
            }
        }
    }

    if !cookies_path.exists() {
        tracing::warn!("⚠️ 登录凭证不存在，等待用户登录...");
        tracing::info!("💡 请访问 Web UI 进行登录");
        loop {
            tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
            if cookies_path.exists() {
                tracing::info!("✅ 检测到登录凭证，开始监控");
                break;
            }
        }
    }
}

fn spawn_monitor_loop(log_level: String) {
    tracing::info!("🔄 监控循环已启动");
    std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async move {
            tracing::info!("🔄 进入监控循环...");
            wait_until_config_ready().await;
            loop {
                match run_bilistream(&log_level).await {
                    Ok(_) => {
                        tracing::info!("监控循环正常结束");
                        break;
                    }
                    Err(e) => {
                        tracing::error!("监控循环错误: {}", e);
                        tracing::info!("⏳ 5秒后重试...");
                        tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
                    }
                }
            }
        });
    });
}

fn install_shutdown_handler() {
    #[cfg(unix)]
    {
        use tokio::signal;
        tokio::spawn(async {
            let mut sigterm = match signal::unix::signal(signal::unix::SignalKind::terminate()) {
                Ok(signal) => signal,
                Err(e) => {
                    tracing::error!("设置 SIGTERM 处理器失败: {}", e);
                    return;
                }
            };
            let mut sigint = match signal::unix::signal(signal::unix::SignalKind::interrupt()) {
                Ok(signal) => signal,
                Err(e) => {
                    tracing::error!("设置 SIGINT 处理器失败: {}", e);
                    return;
                }
            };

            tokio::select! {
                _ = sigterm.recv() => {
                    tracing::info!("收到 SIGTERM 信号");
                    graceful_shutdown().await;
                    std::process::exit(0);
                }
                _ = sigint.recv() => {
                    tracing::info!("收到 SIGINT 信号 (Ctrl+C)");
                    graceful_shutdown().await;
                    std::process::exit(0);
                }
            }
        });
    }

    #[cfg(windows)]
    {
        use tokio::signal;
        tokio::spawn(async {
            match signal::ctrl_c().await {
                Ok(_) => {
                    tracing::info!("收到 Ctrl+C 信号");
                    graceful_shutdown().await;
                    std::process::exit(0);
                }
                Err(e) => {
                    tracing::error!("设置 Ctrl+C 处理器失败: {}", e);
                }
            }
        });
    }
}

async fn run_tray_app(
    port: u16,
    ffmpeg_log_level: &str,
    is_first_run: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    if is_first_run {
        tracing::info!("🚀 欢迎使用 Bilistream！");
        tracing::info!("   检测到首次运行，启动设置向导...");
    } else {
        tracing::info!("🚀 启动 Bilistream 系统托盘模式");
    }
    tracing::info!("   Web UI 端口: {}", port);

    spawn_webui(port);
    spawn_deps_download();
    spawn_monitor_loop(ffmpeg_log_level.to_string());

    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
    tracing::info!("✅ 后台服务已启动");
    bilistream::tray::run_tray(port).await?;
    Ok(())
}

async fn run_webui_app(
    port: u16,
    ffmpeg_log_level: &str,
    is_first_run: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    if is_first_run {
        tracing::info!("🚀 欢迎使用 Bilistream！");
        tracing::info!("   检测到首次运行，启动 Web 设置向导...");
        tracing::info!("");
        tracing::info!("📋 请在浏览器中完成设置：");
        tracing::info!("   1. 打开浏览器访问 http://localhost:{}", port);
        tracing::info!("   2. 按照向导完成 Bilibili 登录和配置");
        tracing::info!("   3. 配置完成后即可开始使用");
        tracing::info!("");
    } else {
        tracing::info!("🚀 启动 Web UI 和自动监控模式");
        tracing::info!("   Web UI 将在后台运行");
        tracing::info!("   访问 http://localhost:{} 查看控制面板", port);
    }

    #[cfg(target_os = "windows")]
    {
        tracing::info!("⚠️ 请勿关闭此窗口 ⚠️");
        if let Err(e) = show_windows_notification(port) {
            eprintln!("无法显示通知: {}", e);
        }
    }

    spawn_webui(port);
    spawn_deps_download();
    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
    tracing::info!("✅ Web UI 已启动");

    if !is_first_run {
        run_bilistream(ffmpeg_log_level).await?;
    } else {
        tracing::info!("⏳ 等待配置完成...");
        tracing::info!("   配置完成后将自动开始监控");
        let (config_path, cookies_path) = config_paths()?;
        loop {
            tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
            if config_path.exists() && cookies_path.exists() {
                tracing::info!("✅ 检测到配置文件已创建！");
                tracing::info!("🚀 正在启动监控...");
                tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
                run_bilistream(ffmpeg_log_level).await?;
                break;
            }
        }
    }
    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    bilistream::install_crypto_provider();

    let args: Vec<String> = std::env::args().collect();

    #[cfg(target_os = "windows")]
    {
        if windows_needs_console(&args) {
            allocate_windows_console();
        }
    }

    let launch = match parse_launch_args(&args) {
        Ok(ParseOutcome::Help) => {
            print_help();
            return Ok(());
        }
        Ok(ParseOutcome::Version) => {
            println!("bilistream {}", env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
        Ok(ParseOutcome::Launch(launch)) => launch,
        Err(err) => {
            #[cfg(target_os = "windows")]
            allocate_windows_console();
            eprintln!("error: {err}");
            eprintln!("Try 'bilistream --help' for more information.");
            std::process::exit(2);
        }
    };

    init_logger_with_capture();
    apply_webui_listen(&launch.bind, launch.password.clone())?;
    install_shutdown_handler();

    let (config_path, cookies_path) = config_paths()?;
    let is_first_run = !config_path.exists() || !cookies_path.exists();

    if launch.tray {
        run_tray_app(launch.port, &launch.ffmpeg_log_level, is_first_run).await
    } else {
        run_webui_app(launch.port, &launch.ffmpeg_log_level, is_first_run).await
    }
}

fn init_logger() {
    tracing_subscriber::fmt()
        .with_timer(fmt::time::ChronoLocal::new("%H:%M:%S".to_string()))
        .with_target(true)
        .with_span_events(fmt::format::FmtSpan::NONE)
        .with_writer(|| LogWriter)
        .with_max_level(tracing::Level::INFO)
        .init();
}

struct LogWriter;

impl std::io::Write for LogWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        write_log_bytes(buf)?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        std::io::Write::flush(&mut std::io::stdout())
    }
}

fn write_log_bytes(buf: &[u8]) -> std::io::Result<()> {
    bilistream::plugins::clear_ffmpeg_stats_display();
    std::io::Write::write_all(&mut std::io::stdout(), buf)
}

fn init_logger_with_capture() {
    use tracing_subscriber::filter::LevelFilter;
    use tracing_subscriber::layer::SubscriberExt;

    // Create a custom writer that captures logs
    struct LogCapture;

    impl std::io::Write for LogCapture {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            if let Ok(s) = std::str::from_utf8(buf) {
                write_log_bytes(buf)?;
                // Capture for web UI (strip ANSI codes)
                // First strip ANSI codes from the entire string
                let clean_str = strip_ansi_codes(s);
                // Then split into lines
                let lines: Vec<&str> = clean_str.lines().collect();
                for line in lines {
                    // Skip pure box drawing lines (borders only)
                    let trimmed = line.trim();
                    if trimmed.starts_with('┌')
                        || trimmed.starts_with('├')
                        || trimmed.starts_with('└')
                    {
                        continue;
                    }

                    // For lines with content, strip the box borders but keep the content
                    let content = if line.contains('│') {
                        // Extract content between │ characters
                        line.split('│')
                            .filter(|s| !s.trim().is_empty())
                            .collect::<Vec<_>>()
                            .join(" ")
                            .trim()
                            .to_string()
                    } else {
                        line.to_string()
                    };

                    // Only add non-empty content
                    if !content.is_empty() {
                        bilistream::add_log_line(content);
                    }
                }
            }
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            std::io::stdout().flush()
        }
    }

    // Helper function to strip ANSI escape codes
    fn strip_ansi_codes(s: &str) -> String {
        let mut result = String::new();
        let mut chars = s.chars();

        while let Some(ch) = chars.next() {
            if ch == '\x1b' {
                // Skip escape sequence
                if chars.next() == Some('[') {
                    // Skip until we find a letter (end of escape sequence)
                    for c in chars.by_ref() {
                        if c.is_ascii_alphabetic() {
                            break;
                        }
                    }
                }
            } else if ch == '\r' {
                // Skip carriage return
                continue;
            } else {
                result.push(ch);
            }
        }

        result
    }

    let fmt_layer = tracing_subscriber::fmt::layer()
        .with_timer(fmt::time::ChronoLocal::new("%H:%M:%S".to_string()))
        .with_target(true)
        .with_span_events(fmt::format::FmtSpan::NONE)
        .with_writer(|| LogCapture);

    let subscriber = tracing_subscriber::registry()
        .with(fmt_layer)
        .with(LevelFilter::INFO);
    tracing::subscriber::set_global_default(subscriber).expect("Failed to set tracing subscriber");
}

#[cfg(target_os = "windows")]
fn show_windows_notification(port: u16) -> Result<(), Box<dyn std::error::Error>> {
    use std::process::Command as StdCommand;

    // Build notification message
    let mut message = String::from("🌐 Web UI 服务已启动\n");
    message.push_str("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━\n");
    message.push_str(&format!("📍 本地访问: http://localhost:{}\n", port));
    message.push_str(&format!("📍 本地访问: http://127.0.0.1:{}\n", port));

    // Escape the message for PowerShell
    let escaped_message = message.replace("`", "``").replace("\"", "`\"");

    // Try to show a Windows notification using PowerShell
    let script = format!(
        r#"
        Add-Type -AssemblyName System.Windows.Forms
        $notification = New-Object System.Windows.Forms.NotifyIcon
        $notification.Icon = [System.Drawing.SystemIcons]::Information
        $notification.Visible = $true
        $notification.ShowBalloonTip(10000, "Bilistream Web UI", "{}", [System.Windows.Forms.ToolTipIcon]::Info)
        Start-Sleep -Seconds 11
        $notification.Dispose()
    "#,
        escaped_message
    );

    StdCommand::new("powershell")
        .args(&["-NoProfile", "-Command", &script])
        .spawn()?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_stream_candidate(platform: StreamPlatform, is_live: bool) -> StreamCandidate {
        StreamCandidate {
            platform,
            is_live,
            topic: None,
            title: Some("title".to_string()),
            m3u8_url: Some("https://example.com/live.m3u8".to_string()),
            stream_id: Some("stream-id".to_string()),
            channel_name: platform.code().to_string(),
            channel_id: "channel-id".to_string(),
            area_v2: 235,
        }
    }

    #[test]
    fn select_stream_prefers_youtube_when_both_live() {
        let yt = test_stream_candidate(StreamPlatform::Youtube, true);
        let tw = test_stream_candidate(StreamPlatform::Twitch, true);

        let selected = select_stream(&yt, &tw).expect("live stream should be selected");

        assert_eq!(selected.platform, StreamPlatform::Youtube);
    }

    #[test]
    fn select_stream_returns_none_when_no_live_candidate() {
        let yt = test_stream_candidate(StreamPlatform::Youtube, false);
        let tw = test_stream_candidate(StreamPlatform::Twitch, false);

        assert!(select_stream(&yt, &tw).is_none());
    }

    #[test]
    fn select_stream_skips_live_candidate_without_stream_url() {
        let mut yt = test_stream_candidate(StreamPlatform::Youtube, true);
        yt.m3u8_url = None;
        let tw = test_stream_candidate(StreamPlatform::Twitch, true);

        let selected = select_stream(&yt, &tw).expect("playable fallback should be selected");

        assert_eq!(selected.platform, StreamPlatform::Twitch);
    }

    #[test]
    fn select_stream_returns_none_when_live_candidates_have_no_stream_url() {
        let mut yt = test_stream_candidate(StreamPlatform::Youtube, true);
        yt.m3u8_url = None;
        let mut tw = test_stream_candidate(StreamPlatform::Twitch, true);
        tw.m3u8_url = None;

        assert!(select_stream(&yt, &tw).is_none());
    }

    #[test]
    fn restart_skip_requires_replacement_m3u8() {
        assert!(restart_exit_should_skip_end_danmaku(
            FfmpegLoopExitReason::IntentionalRestart {
                target_m3u8_available: true,
            }
        ));

        assert!(!restart_exit_should_skip_end_danmaku(
            FfmpegLoopExitReason::IntentionalRestart {
                target_m3u8_available: false,
            }
        ));
        assert!(!restart_exit_should_skip_end_danmaku(
            FfmpegLoopExitReason::SourceEnded
        ));
    }

    #[test]
    fn recover_mutex_lock_returns_inner_after_poison() {
        let lock = Mutex::new(1_u32);
        let _ = std::panic::catch_unwind(|| {
            let mut guard = lock.lock().unwrap();
            *guard = 2;
            panic!("poison test mutex");
        });

        {
            let mut guard = recover_mutex_lock(&lock, "test mutex");
            assert_eq!(*guard, 2);
            *guard = 3;
        }

        assert_eq!(*recover_mutex_lock(&lock, "test mutex"), 3);
    }
    #[test]
    fn normalized_api_key_trims_and_rejects_empty_values() {
        assert_eq!(normalized_api_key(Some("  key  ")).as_deref(), Some("key"));
        assert_eq!(normalized_api_key(Some("   ")), None);
        assert_eq!(normalized_api_key(None), None);
    }

    #[test]
    fn area_label_falls_back_to_area_id() {
        assert_eq!(area_label(u64::MAX), format!("未知分区(ID: {})", u64::MAX));
    }

    #[test]
    fn status_message_update_ignores_small_time_only_changes() {
        let last = "YT: channel 未直播，计划于 2026-07-04 12:00:00 开始，";
        let current = "YT: channel 未直播，计划于 2026-07-04 12:04:00 开始，";

        assert!(!should_update_status_message(last, current));
    }

    #[test]
    fn status_message_update_keeps_non_time_changes() {
        let last = "YT: channel 未直播，计划于 2026-07-04 12:00:00 开始，";
        let current = "YT: channel 未直播，计划于 2026-07-04 12:04:00 开始，new title";

        assert!(should_update_status_message(last, current));
    }

    fn parse_test_args(args: &[&str]) -> Result<ParseOutcome, String> {
        let argv: Vec<String> = std::iter::once("bilistream".to_string())
            .chain(args.iter().map(|s| (*s).to_string()))
            .collect();
        parse_launch_args_with(&argv, None, None, None, None, false)
    }

    #[test]
    fn launch_defaults_without_flags() {
        let ParseOutcome::Launch(launch) = parse_test_args(&[]).unwrap() else {
            panic!("expected launch");
        };
        assert_eq!(launch.bind, "127.0.0.1");
        assert_eq!(launch.port, 3150);
        assert_eq!(launch.ffmpeg_log_level, "error");
        assert!(!launch.tray);
        assert!(launch.password.is_none());
    }

    #[test]
    fn launch_parses_bind_port_and_webui() {
        let ParseOutcome::Launch(launch) =
            parse_test_args(&["--bind=0.0.0.0", "-p", "8080", "--webui"]).unwrap()
        else {
            panic!("expected launch");
        };
        assert_eq!(launch.bind, "0.0.0.0");
        assert_eq!(launch.port, 8080);
        assert!(!launch.tray);
    }

    #[test]
    fn launch_tray_flag_overrides_default() {
        let ParseOutcome::Launch(launch) = parse_test_args(&["--tray"]).unwrap() else {
            panic!("expected launch");
        };
        assert!(launch.tray);
    }

    #[test]
    fn launch_rejects_unknown_subcommand() {
        let err = parse_test_args(&["setup"]).unwrap_err();
        assert!(err.contains("unknown argument"));
    }

    #[test]
    fn launch_help_and_version() {
        assert!(matches!(
            parse_test_args(&["--help"]).unwrap(),
            ParseOutcome::Help
        ));
        assert!(matches!(
            parse_test_args(&["-V"]).unwrap(),
            ParseOutcome::Version
        ));
    }
}
