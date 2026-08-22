// Hide console window on Windows in release mode
#![cfg_attr(
    all(target_os = "windows", not(debug_assertions)),
    windows_subsystem = "windows"
)]

use bilistream::config::{
    load_config, save_config, BiliLive, Config, Credentials, Twitch, Youtube,
};
use bilistream::plugins::bilibili::get_thumbnail;
use bilistream::plugins::Twitch as TwitchClient;
use bilistream::plugins::Youtube as YoutubeClient;
use bilistream::plugins::{
    bili_change_live_title, bili_start_live, bili_stop_live, bili_update_area, bilibili,
    check_area_id_with_title, clear_config_updated, clear_manual_restart, clear_manual_stop,
    clear_warning_stop, current_game_riot_ids, enable_danmaku_commands, ffmpeg, get_aliases,
    get_area_name, get_bili_live_status, get_bili_live_time, get_channel_name, get_puuid,
    is_config_updated, is_danmaku_commands_enabled, is_danmaku_running, is_ffmpeg_running,
    run_danmaku, send_danmaku, should_skip_due_to_warned, should_skip_due_to_warning, stop_danmaku,
    stop_ffmpeg, wait_config_update_or_timeout, wait_ffmpeg, was_manual_restart, was_manual_stop,
    FfmpegCacheOptions, BILI_START_TEMP_BAN_PREFIX,
};
use qrcode::QrCode;

use chrono::{DateTime, Local, NaiveDateTime};
use clap::{Arg, Command};
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

async fn get_live_status(
    platform: &str,
    channel_id: Option<&str>,
) -> Result<(), Box<dyn std::error::Error>> {
    match platform {
        "bilibili" => {
            let cfg = load_config().await?;
            let (is_live, title, area_id) = get_bili_live_status(cfg.bililive.room).await?;
            if is_live {
                println!(
                    "B站直播中, 标题: {}, 分区: {} （ID: {}）",
                    title,
                    area_label(area_id),
                    area_id,
                );
            } else {
                println!("B站未直播");
            }
            Ok(())
        }
        "YT" => {
            let cfg = load_config().await?;
            let channel_id = if let Some(id) = channel_id {
                id
            } else {
                &cfg.youtube.channel_id
            };
            let channel_name =
                get_channel_name("YT", channel_id)?.unwrap_or_else(|| channel_id.to_string());
            let yt_client =
                YoutubeClient::new(&channel_name, channel_id, cfg.youtube.proxy.clone());
            let (is_live, topic, title, _, start_time, _) = yt_client.get_status().await?;
            if is_live {
                println!(
                    "{} 在 YouTube 直播中, 分区: {}, 标题: {}",
                    channel_name,
                    topic.as_deref().unwrap_or("未知分区"),
                    title.as_deref().unwrap_or("无标题")
                );
            } else if let Some(start_time) = start_time {
                println!(
                    "{} 未在 YouTube 直播, {}计划于 {} 开始, 标题: {}",
                    channel_name,
                    if let Some(t) = &topic {
                        format!("分区: {}, ", t)
                    } else {
                        String::new()
                    },
                    start_time.format(MESSAGE_TIME_FORMAT),
                    title.as_deref().unwrap_or("无标题")
                );
            } else {
                println!("{} 未在 YouTube 直播", channel_name);
            }
            Ok(())
        }
        "TW" => {
            let cfg = load_config().await?;
            let channel_id = if let Some(id) = channel_id {
                id
            } else {
                &cfg.twitch.channel_id
            };
            let channel_name =
                get_channel_name("TW", channel_id)?.unwrap_or_else(|| channel_id.to_string());
            let tw_client = TwitchClient::new(
                channel_id,
                cfg.twitch.proxy_region.clone(),
                cfg.twitch.proxy.clone(),
            )?;
            let (is_live, game_name, title, _, _, _) = tw_client.get_status().await?;
            if is_live {
                println!(
                    "{} 在 Twitch 直播中, 分区: {}, 标题: {}",
                    channel_name,
                    game_name.as_deref().unwrap_or("未知分区"),
                    title.as_deref().unwrap_or("无标题")
                );
            } else {
                println!("{} 未在 Twitch 直播", channel_name);
            }
            Ok(())
        }
        // all 平台 output all platform
        "all" => {
            let cfg = load_config().await?;
            let (is_live, title, area_id) = get_bili_live_status(cfg.bililive.room).await?;
            if is_live {
                println!(
                    "B站直播中, 标题: {}, 分区: {} （ID: {}）",
                    title,
                    area_label(area_id),
                    area_id,
                );
            } else {
                println!("B站未直播");
            }
            let channel_id = cfg.youtube.channel_id;
            let channel_name = cfg.youtube.channel_name;

            let yt_client =
                YoutubeClient::new(&channel_name, &channel_id, cfg.youtube.proxy.clone());
            let (is_live, topic, title, _, start_time, _) = yt_client.get_status().await?;
            if is_live {
                if let Some(topic) = topic.as_deref() {
                    println!(
                        "{} 在 YouTube 直播中, 分区: {}, 标题: {}",
                        channel_name,
                        topic,
                        title.as_deref().unwrap_or("无标题")
                    );
                } else {
                    println!(
                        "{} 在 YouTube 直播中, 标题: {}",
                        channel_name,
                        title.as_deref().unwrap_or("无标题")
                    );
                }
            } else if let Some(start_time) = start_time {
                println!(
                    "{} 未在 YouTube 直播, {}计划于 {} 开始, 标题: {}",
                    channel_name,
                    if let Some(t) = &topic {
                        format!("分区: {}, ", t)
                    } else {
                        String::new()
                    },
                    start_time.format(MESSAGE_TIME_FORMAT),
                    title.as_deref().unwrap_or("无标题")
                );
            } else {
                println!("{} 未在 YouTube 直播", channel_name);
            }
            let channel_id = cfg.twitch.channel_id;
            let channel_name = cfg.twitch.channel_name;
            let tw_client = TwitchClient::new(
                &channel_id,
                cfg.twitch.proxy_region.clone(),
                cfg.twitch.proxy.clone(),
            )?;
            let (is_live, game_name, title, _, _, _) = tw_client.get_status().await?;
            if is_live {
                println!(
                    "{} 在 Twitch 直播中, 分区: {}, 标题: {}",
                    channel_name,
                    game_name.as_deref().unwrap_or("未知分区"),
                    title.as_deref().unwrap_or("无标题")
                );
            } else {
                println!("{} 未在 Twitch 直播", channel_name);
            }
            Ok(())
        }
        _ => {
            println!("不支持的平台: {}", platform);
            Err(format!("不支持的平台: {}", platform).into())
        }
    }
}

async fn start_live(optional_platform: Option<&str>) -> Result<(), Box<dyn std::error::Error>> {
    let mut cfg = load_config().await?;
    let area_v2 = if optional_platform == Some("YT") {
        cfg.youtube.area_v2
    } else if optional_platform == Some("TW") {
        cfg.twitch.area_v2
    } else {
        235 // default area_v2 (其他单机)
    };

    match bili_start_live(&mut cfg, area_v2).await {
        Ok(_) => {
            println!("直播开始成功");
            println!("url：{}", cfg.bililive.bili_rtmp_url);
            println!("key：{}", cfg.bililive.bili_rtmp_key);
            Ok(())
        }
        Err(e) => {
            let error_msg = e.to_string();
            if error_msg.starts_with("FACE_AUTH_REQUIRED:") {
                let qr_url = error_msg.strip_prefix("FACE_AUTH_REQUIRED:").unwrap_or("");
                eprintln!("❌ 需要人脸认证");

                if let Ok(qr) = QrCode::new(qr_url) {
                    let qr_string = qr
                        .render::<char>()
                        .quiet_zone(false)
                        .module_dimensions(2, 1)
                        .build();
                    eprintln!("📱 请扫描二维码完成认证:\n{}", qr_string);
                } else {
                    eprintln!("📱 请访问以下链接完成认证: {}", qr_url);
                }
            } else {
                eprintln!("❌ 开播失败: {}", error_msg);
            }
            Err(e)
        }
    }
}

async fn stop_live() -> Result<(), Box<dyn std::error::Error>> {
    let cfg = load_config().await?;
    bili_stop_live(&cfg).await?;
    println!("直播停止成功");
    Ok(())
}

async fn change_live_title(new_title: &str) -> Result<(), Box<dyn std::error::Error>> {
    let cfg = load_config().await?;

    match bili_change_live_title(&cfg, new_title).await {
        Ok(_) => {
            println!("✅ 直播标题改变成功");
            Ok(())
        }
        Err(e) => {
            eprintln!("❌ 直播标题改变失败: {}", e);

            // Provide helpful suggestions for common issues
            if e.to_string().contains("审核") {
                eprintln!("💡 建议:");
                eprintln!("   - 尝试使用更通用的标题，如 '【转播】游戏直播'");
                eprintln!("   - 避免使用特定的VTuber名称");
                eprintln!("   - 使用英文或数字代替敏感词汇");
            }

            Err(e)
        }
    }
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

async fn setup_wizard() -> Result<(), Box<dyn std::error::Error>> {
    use std::io::{self, Write};

    println!("=== Bilistream 初始化设置向导 ===\n");

    // Step 1: Check if config.json already exists
    let config_path = std::env::current_exe()?.with_file_name("config.json");
    if config_path.exists() {
        print!("检测到已存在的 config.json，是否覆盖? (y/N): ");
        io::stdout().flush()?;
        let mut input = String::new();
        io::stdin().read_line(&mut input)?;
        if !input.trim().eq_ignore_ascii_case("y") {
            println!("已取消设置");
            return Ok(());
        }
    }

    // Step 2: Login to Bilibili
    println!("\n步骤 1/2: 登录 Bilibili");
    println!("----------------------------------------");
    let cookies_path = std::env::current_exe()?.with_file_name("cookies.json");
    if cookies_path.exists() {
        print!("检测到已存在的 cookies.json，是否重新登录? (y/N): ");
        io::stdout().flush()?;
        let mut input = String::new();
        io::stdin().read_line(&mut input)?;
        if input.trim().eq_ignore_ascii_case("y") {
            bilibili::login().await?;
        } else {
            println!("使用现有登录凭证");
        }
    } else {
        bilibili::login().await?;
    }

    // Step 3: Configure config.json
    println!("\n步骤 2/2: 配置 config.json");
    println!("----------------------------------------");

    // Get room number
    print!("请输入你的B站直播间号: ");
    io::stdout().flush()?;
    let mut room = String::new();
    io::stdin().read_line(&mut room)?;
    let room: i32 = room.trim().parse().unwrap_or(0);
    if room == 0 {
        return Err("无效的直播间号".into());
    }

    // Get YouTube channel info
    print!("\n是否配置 YouTube 频道? (Y/n): ");
    io::stdout().flush()?;
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    let configure_youtube = !input.trim().eq_ignore_ascii_case("n");

    let (yt_channel_name, yt_channel_id, yt_area_v2, yt_quality, yt_proxy) = if configure_youtube {
        print!("YouTube 频道名称: ");
        io::stdout().flush()?;
        let mut name = String::new();
        io::stdin().read_line(&mut name)?;
        let name = name.trim().to_string();

        print!("YouTube 频道ID: ");
        io::stdout().flush()?;
        let mut id = String::new();
        io::stdin().read_line(&mut id)?;
        let id = id.trim().to_string();

        print!("B站分区ID (默认 235-其他单机): ");
        io::stdout().flush()?;
        let mut area = String::new();
        io::stdin().read_line(&mut area)?;
        let area: u64 = area.trim().parse().unwrap_or(235);

        println!("\n流质量设置 (用于网络受限用户):");
        println!("  best - 最佳质量 (推荐)");
        println!("  worst - 最低质量");
        println!("  720p/480p - 指定分辨率");
        print!("请选择质量 (默认 best): ");
        io::stdout().flush()?;
        let mut quality = String::new();
        io::stdin().read_line(&mut quality)?;
        let quality = if quality.trim().is_empty() {
            "best".to_string()
        } else {
            quality.trim().to_string()
        };

        print!("\n是否需要为 YouTube 配置代理? (y/N): ");
        io::stdout().flush()?;
        let mut proxy_input = String::new();
        io::stdin().read_line(&mut proxy_input)?;
        let yt_proxy = if proxy_input.trim().eq_ignore_ascii_case("y") {
            print!("YouTube 代理: ");
            io::stdout().flush()?;
            let mut proxy = String::new();
            io::stdin().read_line(&mut proxy)?;
            let proxy_str = proxy.trim().to_string();
            if proxy_str.is_empty() {
                None
            } else {
                Some(proxy_str)
            }
        } else {
            None
        };

        (name, id, area, quality, yt_proxy)
    } else {
        (
            "".to_string(),
            "".to_string(),
            235,
            "best".to_string(),
            None,
        )
    };

    // Get Twitch channel info
    print!("\n是否配置 Twitch 频道? (Y/n): ");
    io::stdout().flush()?;
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    let configure_twitch = !input.trim().eq_ignore_ascii_case("n");

    let (tw_channel_name, tw_channel_id, tw_area_v2, tw_proxy_region, tw_quality, tw_proxy) =
        if configure_twitch {
            print!("Twitch 频道名称: ");
            io::stdout().flush()?;
            let mut name = String::new();
            io::stdin().read_line(&mut name)?;
            let name = name.trim().to_string();

            print!("Twitch 频道ID (用户名): ");
            io::stdout().flush()?;
            let mut id = String::new();
            io::stdin().read_line(&mut id)?;
            let id = id.trim().to_string();

            print!("B站分区ID (默认 235-其他单机): ");
            io::stdout().flush()?;
            let mut area = String::new();
            io::stdin().read_line(&mut area)?;
            let area: u64 = area.trim().parse().unwrap_or(235);

            print!("Twitch 代理区域 (默认 as): ");
            io::stdout().flush()?;
            let mut region = String::new();
            io::stdin().read_line(&mut region)?;
            let region = if region.trim().is_empty() {
                "as".to_string()
            } else {
                region.trim().to_string()
            };

            println!("\n流质量设置 (用于网络受限用户):");
            println!("  best - 最佳质量 (推荐)");
            println!("  worst - 最低质量");
            println!("  720p/480p - 指定分辨率");
            print!("请选择质量 (默认 best): ");
            io::stdout().flush()?;
            let mut quality = String::new();
            io::stdin().read_line(&mut quality)?;
            let quality = if quality.trim().is_empty() {
                "best".to_string()
            } else {
                quality.trim().to_string()
            };

            print!("\n是否需要为 Twitch 配置代理? (y/N): ");
            io::stdout().flush()?;
            let mut proxy_input = String::new();
            io::stdin().read_line(&mut proxy_input)?;
            let tw_proxy = if proxy_input.trim().eq_ignore_ascii_case("y") {
                print!("Twitch 代理: ");
                io::stdout().flush()?;
                let mut proxy = String::new();
                io::stdin().read_line(&mut proxy)?;
                let proxy_str = proxy.trim().to_string();
                if proxy_str.is_empty() {
                    None
                } else {
                    Some(proxy_str)
                }
            } else {
                None
            };

            (name, id, area, region, quality, tw_proxy)
        } else {
            (
                "".to_string(),
                "".to_string(),
                235,
                "as".to_string(),
                "best".to_string(),
                None,
            )
        };

    // Optional settings
    print!("\n是否启用自动封面更换? (Y/n): ");
    io::stdout().flush()?;
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    let auto_cover = !input.trim().eq_ignore_ascii_case("n");

    print!("是否启用弹幕指令? (Y/n): ");
    io::stdout().flush()?;
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    let enable_danmaku_command = !input.trim().eq_ignore_ascii_case("n");

    print!("检测间隔 (秒，默认 60): ");
    io::stdout().flush()?;
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    let interval: u64 = input.trim().parse().unwrap_or(60);

    // Anti-collision settings
    print!("\n是否启用撞车监控? (y/N): ");
    io::stdout().flush()?;
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    let anti_collision = input.trim().eq_ignore_ascii_case("y");

    let mut collision_rooms = Vec::new();
    if anti_collision {
        println!("\n配置撞车监控直播间");
        println!("提示: 输入需要监控的B站直播间信息，用于检测是否有其他人在转播相同频道");
        loop {
            print!("\n输入监控直播间名称 (直接回车结束添加): ");
            io::stdout().flush()?;
            let mut name = String::new();
            io::stdin().read_line(&mut name)?;
            let name = name.trim();

            if name.is_empty() {
                break;
            }

            print!("输入直播间号: ");
            io::stdout().flush()?;
            let mut room_id = String::new();
            io::stdin().read_line(&mut room_id)?;
            let room_id: i32 = match room_id.trim().parse() {
                Ok(id) => id,
                Err(_) => {
                    println!("⚠️  无效的直播间号，已跳过");
                    continue;
                }
            };

            collision_rooms.push((name.to_string(), room_id));
            println!("✅ 已添加: {} ({})", name, room_id);
        }

        if collision_rooms.is_empty() {
            println!("⚠️  未添加任何监控直播间，撞车监控将不会生效");
        }
    }

    // Advanced optional settings
    print!("\n是否配置高级选项 (API密钥等)? (y/N): ");
    io::stdout().flush()?;
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    let configure_advanced = input.trim().eq_ignore_ascii_case("y");

    let (holodex_api_key, holodex_jwt, riot_api_key, enable_lol_monitor) = if configure_advanced {
        println!("\n高级选项配置");
        println!("----------------------------------------");

        println!("\nHolodex API Key (用于YouTube直播状态检测，可选)");
        println!("获取方法: https://holodex.net/login");
        print!("请输入 (直接回车跳过): ");
        io::stdout().flush()?;
        let mut holodex = String::new();
        io::stdin().read_line(&mut holodex)?;
        let holodex = holodex.trim().to_string();

        println!("\nHolodex JWT (用于收藏夹直播监控，可选)");
        println!("留空则 Holodex 监控使用 channels.json，可稍后在 Web UI 配置");
        println!("如何获取 JWT:");
        println!("  1. 打开 https://holodex.net/login 并完成登录");
        println!("  2. 浏览器 F12 → Application（应用程序）→ Cookies → https://holodex.net");
        println!("  3. 找到 HOLODEX_JWT，复制其 Value（值）");
        println!("  (JWT 将在到期前 30 天内自动续期并写回 config.json)");
        print!("请输入 (直接回车跳过): ");
        io::stdout().flush()?;
        let mut holodex_jwt = String::new();
        io::stdin().read_line(&mut holodex_jwt)?;
        let holodex_jwt = holodex_jwt
            .trim()
            .trim_start_matches("BEARER ")
            .trim_start_matches("bearer ")
            .to_string();

        println!("\n英雄联盟玩家ID监控 (用于检测游戏内违禁词汇)");
        print!("是否启用? (y/N): ");
        io::stdout().flush()?;
        let mut input = String::new();
        io::stdin().read_line(&mut input)?;
        let enable_lol = input.trim().eq_ignore_ascii_case("y");

        let riot = if enable_lol {
            println!("\nRiot API Key (用于英雄联盟玩家ID监控)");
            println!("获取方法: https://developer.riotgames.com/");
            print!("请输入 (直接回车跳过): ");
            io::stdout().flush()?;
            let mut riot = String::new();
            io::stdin().read_line(&mut riot)?;
            riot.trim().to_string()
        } else {
            String::new()
        };

        (holodex, holodex_jwt, riot, enable_lol)
    } else {
        (String::new(), String::new(), String::new(), false)
    };

    // Create config structure
    let mut collision_map = std::collections::HashMap::new();
    for (name, room_id) in &collision_rooms {
        collision_map.insert(name.clone(), *room_id);
    }

    let config = Config {
        auto_cover,
        enable_anti_collision: anti_collision,
        interval,
        bililive: BiliLive {
            enable_danmaku_command,
            room,
            bili_rtmp_url: "rtmp://live-push.bilivideo.com/live-bvc/".to_string(),
            bili_rtmp_key: String::new(),
            credentials: Credentials::default(),
        },
        twitch: Twitch {
            enable_monitor: true,
            channel_name: tw_channel_name,
            area_v2: tw_area_v2,
            channel_id: tw_channel_id,
            proxy_region: tw_proxy_region,
            quality: tw_quality,
            proxy: tw_proxy,
            crop: None,
            ffmpeg_cache: Default::default(),
        },
        youtube: Youtube {
            enable_monitor: true,
            channel_name: yt_channel_name,
            channel_id: yt_channel_id,
            area_v2: yt_area_v2,
            quality: yt_quality,
            cookies_file: None,
            cookies_from_browser: None,
            proxy: yt_proxy,
            deno_path: None,
            crop: None,
            ffmpeg_cache: Default::default(),
        },
        holodex_api_key: if holodex_api_key.is_empty() {
            None
        } else {
            Some(holodex_api_key)
        },
        holodex_jwt: if holodex_jwt.is_empty() {
            None
        } else {
            Some(holodex_jwt)
        },
        holodex_jwt_refreshed_at: None,
        holodex_username: None,
        holodex_skip_jwt_verify: false,
        riot_api_key: if riot_api_key.is_empty() {
            None
        } else {
            Some(riot_api_key)
        },
        enable_lol_monitor,
        lol_monitor_interval: Some(1),
        anti_collision_list: collision_map,
    };

    // Write config file as JSON
    let config_json = serde_json::to_string_pretty(&config)?;
    std::fs::write(&config_path, config_json)?;
    println!("\n✅ 配置文件已创建: {}", config_path.display());

    // Try to start live to get RTMP info
    println!("\n正在获取推流地址...");
    match load_config().await {
        Ok(mut cfg) => {
            if let Err(e) = bili_start_live(&mut cfg, yt_area_v2).await {
                println!("⚠️  获取推流地址失败: {}", e);
                println!("你可以稍后手动开播获取推流地址");
            } else {
                println!("✅ 推流地址已更新到配置文件");
                // Stop the live immediately
                let _ = bili_stop_live(&cfg).await;
            }
        }
        Err(e) => {
            println!("⚠️  加载配置失败: {}", e);
        }
    }

    println!("\n=== 设置完成 ===");
    println!("你现在可以运行 'bilistream' 开始转播");
    println!("配置文件位置: {}", config_path.display());
    println!("登录凭证位置: {}", cookies_path.display());

    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    bilistream::install_crypto_provider();

    // On Windows, allocate a console for CLI and WebUI modes
    #[cfg(target_os = "windows")]
    {
        // Check if we're running CLI or WebUI mode (or other console commands)
        let args: Vec<String> = std::env::args().collect();
        let needs_console = args.len() > 1 && !matches!(args[1].as_str(), "tray");

        if needs_console {
            unsafe {
                use std::ffi::CString;
                use winapi::um::consoleapi::AllocConsole;
                use winapi::um::fileapi::{CreateFileA, OPEN_EXISTING};
                use winapi::um::processenv::SetStdHandle;
                use winapi::um::winbase::{STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE};
                use winapi::um::winnt::{
                    FILE_SHARE_READ, FILE_SHARE_WRITE, GENERIC_READ, GENERIC_WRITE,
                };

                // Allocate a console
                AllocConsole();

                // Redirect stdout, stdin, stderr to console
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

                // Set the handles
                SetStdHandle(STD_OUTPUT_HANDLE, stdout_handle);
                SetStdHandle(STD_ERROR_HANDLE, stderr_handle);
                SetStdHandle(STD_INPUT_HANDLE, stdin_handle);
            }
        }
    }

    let matches = Command::new("bilistream")
        .version(env!("CARGO_PKG_VERSION"))
        .arg(
            Arg::new("ffmpeg-log-level")
                .long("ffmpeg-log-level")
                .value_name("LEVEL")
                .help("设置ffmpeg日志级别 (error, info, debug)")
                .default_value("error")
                .value_parser(["error", "info", "debug"]),
        )
        .arg(
            Arg::new("bind")
                .long("bind")
                .value_name("ADDR")
                .help("Web UI listen address (default 127.0.0.1)")
                .global(true),
        )
        .arg(
            Arg::new("password")
                .long("password")
                .value_name("PASSWORD")
                .help("Web UI login password. Also BILISTREAM_PASSWORD.")
                .global(true),
        )
        .subcommand(
            Command::new("cli")
                .about("以命令行模式运行（无 Web UI）"),
        )
        .subcommand(
            Command::new("get-live-status")
                .about("获取直播状态、标题和分区")
                .arg(
                    Arg::new("platform")
                        .required(false)
                        .value_parser(["YT", "TW", "bilibili", "all"])
                        .default_value("all")
                        .help("获取的平台 (YT, TW, bilibili, all)"),
                )
                .arg(Arg::new("channel_id").required(false).help("获取的频道ID")),
        )
        .subcommand(
            Command::new("start-live").about("开始直播").arg(
                Arg::new("platform")
                    .required(false)
                    .help("开始直播的分区来源 (YT, TW)，未指定则默认为其他单机分区开播"),
            ),
        )
        .subcommand(Command::new("stop-live").about("停止直播"))
        .subcommand(
            Command::new("change-live-title")
                .about("改变直播标题")
                .arg(Arg::new("title").required(true).help("新直播标题")),
        )

        .subcommand(
            Command::new("login")
                .about("通过二维码登录Bilibili")
                .long_about("在终端显示一个二维码，你可以用Bilibili移动应用扫描登录。将登录凭证保存到cookies.json"),
        )
        .subcommand(
            Command::new("send-danmaku")
                .about("发送弹幕到直播间")
                .arg(Arg::new("message").required(true).help("弹幕内容")),
        )
        .subcommand(
            Command::new("replace-cover").about("更换直播间封面").arg(
                Arg::new("image_path")
                    .required(true)
                    .help("封面图片路径 (支持jpg/png格式)"),
            ),
        )
        .subcommand(
            Command::new("update-area")
                .about("更新Bilibili直播间分区")
                .arg(
                    Arg::new("area_id")
                        .help("新分区ID")
                        .required(true)
                        .value_parser(clap::value_parser!(u64)),
                ),
        )
        .subcommand(
            Command::new("renew")
                .about("更新Bilibili登录令牌")

        )
        .subcommand(
            Command::new("completion")
                .about("生成shell自动补全脚本")
                .arg(
                    Arg::new("shell")
                        .required(true)
                        .help("目标shell (bash, zsh, fish)")
                        .value_parser(["bash", "zsh", "fish"]),
                ),
        )
        .subcommand(
            Command::new("setup")
                .about("初始化配置：登录Bilibili并配置config.json")
                .long_about("交互式设置向导，帮助你登录Bilibili并创建config.json配置文件"),
        )
        .subcommand(
            Command::new("webui")
                .about("启动 Web UI 控制面板")
                .arg(
                    Arg::new("port")
                        .short('p')
                        .long("port")
                        .value_name("PORT")
                        .help("Web UI 端口")
                        .default_value("3150")
                        .value_parser(clap::value_parser!(u16)),
                ),
        )
        .subcommand(
            Command::new("tray")
                .about("启动系统托盘模式")
                .arg(
                    Arg::new("port")
                        .short('p')
                        .long("port")
                        .value_name("PORT")
                        .help("Web UI 端口")
                        .default_value("3150")
                        .value_parser(clap::value_parser!(u16)),
                ),
        )
        .get_matches();

    let ffmpeg_log_level = matches
        .get_one::<String>("ffmpeg-log-level")
        .map(String::as_str)
        .unwrap_or("error");

    // Set up graceful shutdown handler
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

    match matches.subcommand() {
        Some(("get-live-status", sub_m)) => {
            let platform = sub_m
                .get_one::<String>("platform")
                .map(String::as_str)
                .unwrap_or("all");
            let channel_id = sub_m.get_one::<String>("channel_id");
            get_live_status(platform, channel_id.map(String::as_str)).await?;
        }
        Some(("start-live", sub_m)) => {
            let platform = sub_m.get_one::<String>("platform");
            if platform.is_none() {
                start_live(None).await?;
            } else {
                start_live(Some(platform.unwrap())).await?;
            }
        }
        Some(("stop-live", _)) => {
            stop_live().await?;
        }
        Some(("change-live-title", sub_m)) => {
            let new_title = sub_m.get_one::<String>("title").unwrap();
            change_live_title(new_title).await?;
        }

        Some(("login", _)) => {
            tracing::info!("Starting Bilibili login process...");
            bilibili::login().await?;
        }
        Some(("send-danmaku", sub_m)) => {
            let message = sub_m.get_one::<String>("message").unwrap();
            let cfg = load_config().await?;
            match bilibili::send_danmaku(&cfg, message).await {
                Ok(_) => println!("弹幕发送成功"),
                Err(e) => {
                    // Check if it's a rate limit error
                    if e.to_string().contains("频率过快") {
                        eprintln!("⚠️ 弹幕发送失败: 发送频率过快，请稍后再试");
                    } else {
                        eprintln!("❌ 弹幕发送失败: {}", e);
                    }
                }
            }
        }
        Some(("replace-cover", sub_m)) => {
            let image_path = sub_m.get_one::<String>("image_path").unwrap();
            let cfg = load_config().await?;
            bilibili::bili_change_cover(&cfg, image_path).await?;
            println!("直播间封面更换成功");
        }
        Some(("update-area", sub_matches)) => {
            let cfg = load_config().await?;
            let area_id = sub_matches
                .get_one::<u64>("area_id")
                .expect("Required argument");

            let (_, _, current_area) = get_bili_live_status(cfg.bililive.room).await?;
            if current_area != *area_id {
                update_area(current_area, *area_id).await?;
                let (_, _, current_area) = get_bili_live_status(cfg.bililive.room).await?;
                if current_area != *area_id {
                    println!("直播间分区更新失败");
                } else {
                    println!(
                        "直播间分区更新成功, {} -> {}",
                        area_label(current_area),
                        area_label(*area_id)
                    );
                }
            } else {
                println!("分区相同，无须更新");
            }
        }
        Some(("renew", _)) => {
            bilibili::renew().await?;
        }
        Some(("setup", _)) => {
            setup_wizard().await?;
        }
        Some(("webui", sub_m)) => {
            // Initialize logger with capture for webui mode
            init_logger_with_capture();
            apply_webui_listen(&matches)?;

            let port = sub_m.get_one::<u16>("port").copied().unwrap_or(3150);
            tracing::info!("🚀 启动 Web UI 和自动监控模式");
            tracing::info!("   Web UI 将在后台运行");
            tracing::info!("   访问 http://localhost:{} 查看控制面板", port);

            // Spawn WebUI server in background
            tokio::spawn(async move {
                if let Err(e) = bilistream::webui::server::start_webui(port).await {
                    tracing::error!("Web UI 服务器错误: {}", e);
                }
            });

            // Give WebUI time to start
            tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
            tracing::info!("✅ Web UI 已启动");

            // Run monitoring loop in foreground (this will block)
            run_bilistream(ffmpeg_log_level).await?;
        }
        Some(("cli", _)) => {
            // Initialize logger for CLI mode
            init_logger();

            // CLI mode: Check if setup is needed
            let config_path = std::env::current_exe()?.with_file_name("config.json");
            let cookies_path = std::env::current_exe()?.with_file_name("cookies.json");
            let needs_setup = !config_path.exists() || !cookies_path.exists();

            if needs_setup {
                println!("⚠️  检测到缺少配置文件，启动设置向导...\n");
                setup_wizard().await?;
                return Ok(());
            }

            // CLI mode: run normal monitoring
            run_bilistream(ffmpeg_log_level).await?;
        }
        Some(("tray", sub_m)) => {
            // Initialize logger with capture for tray mode
            init_logger_with_capture();
            apply_webui_listen(&matches)?;

            let port = sub_m.get_one::<u16>("port").copied().unwrap_or(3150);
            let log_level = ffmpeg_log_level.to_string(); // Clone to owned String

            tracing::info!("🚀 启动系统托盘模式");
            tracing::info!("   Web UI 端口: {}", port);

            // Spawn WebUI server in background
            tokio::spawn(async move {
                if let Err(e) = bilistream::webui::server::start_webui(port).await {
                    tracing::error!("Web UI 服务器错误: {}", e);
                }
            });

            // Spawn monitoring loop in separate thread with its own runtime
            tracing::info!("🔄 监控循环已启动");
            std::thread::spawn(move || {
                let rt = tokio::runtime::Runtime::new().unwrap();
                rt.block_on(async move {
                    tracing::info!("🔄 进入监控循环...");

                    // Check if config exists before starting
                    let config_path = std::env::current_exe()
                        .unwrap()
                        .with_file_name("config.json");
                    let cookies_path = std::env::current_exe()
                        .unwrap()
                        .with_file_name("cookies.json");

                    if !config_path.exists() {
                        tracing::warn!("⚠️ 配置文件不存在，等待用户配置...");
                        tracing::info!("💡 请访问 Web UI 进行配置");

                        // Wait for config to be created
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

                        // Wait for cookies to be created
                        loop {
                            tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
                            if cookies_path.exists() {
                                tracing::info!("✅ 检测到登录凭证，开始监控");
                                break;
                            }
                        }
                    }

                    // Now start the actual monitoring loop
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

            // Give WebUI time to start
            tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
            tracing::info!("✅ 后台服务已启动");

            // Download dependencies in background
            tokio::spawn(async move {
                if let Err(e) = bilistream::deps::ensure_all_dependencies().await {
                    tracing::warn!("⚠️ 下载依赖项失败: {}", e);
                }
            });

            // Run system tray (this will block until quit)
            bilistream::tray::run_tray(port).await?;
        }
        Some(("completion", sub_m)) => {
            let shell = sub_m.get_one::<String>("shell").unwrap();
            let mut cmd = Command::new("bilistream")
                .version("0.2.1")
                .arg(
                    Arg::new("config")
                        .short('c')
                        .long("config")
                        .value_name("FILE")
                        .help("设置自定义配置文件")
                        .global(true),
                )
                .arg(
                    Arg::new("ffmpeg-log-level")
                        .long("ffmpeg-log-level")
                        .value_name("LEVEL")
                        .help("设置ffmpeg日志级别 (error, info, debug)")
                        .default_value("error")
                        .value_parser(["error", "info", "debug"]),
                )
                .subcommand(
                    Command::new("get-live-status")
                        .about("检查频道直播状态")
                        .visible_alias("get-status")
                        .arg(
                            Arg::new("platform")
                                .required(true)
                                .value_parser(["YT", "TW", "bilibili", "all"])
                                .help("检查的平台 (YT, TW, bilibili, all)"),
                        ),
                )
                .subcommand(Command::new("login").about("登录"))
                .subcommand(
                    Command::new("send-danmaku")
                        .about("发送弹幕到直播间")
                        .arg(Arg::new("message").required(true).help("弹幕内容")),
                )
                .subcommand(
                    Command::new("replace-cover").about("更换直播间封面").arg(
                        Arg::new("image_path")
                            .required(true)
                            .help("封面图片路径 (支持jpg/png格式)"),
                    ),
                )
                .subcommand(
                    Command::new("update-area")
                        .about("更新Bilibili直播间分区")
                        .arg(
                            Arg::new("area_id")
                                .help("新分区ID")
                                .required(true)
                                .value_parser(clap::value_parser!(u64)),
                        ),
                )
                .subcommand(
                    Command::new("completion")
                        .about("Generate shell completion scripts")
                        .arg(
                            Arg::new("shell")
                                .required(true)
                                .help("Target shell (bash, zsh, fish)")
                                .value_parser(["bash", "zsh", "fish"]),
                        ),
                );

            match shell.as_str() {
                "bash" => {
                    clap_complete::generate(
                        clap_complete::shells::Bash,
                        &mut cmd,
                        "bilistream",
                        &mut std::io::stdout(),
                    );
                }
                "zsh" => {
                    clap_complete::generate(
                        clap_complete::shells::Zsh,
                        &mut cmd,
                        "bilistream",
                        &mut std::io::stdout(),
                    );
                }
                "fish" => {
                    clap_complete::generate(
                        clap_complete::shells::Fish,
                        &mut cmd,
                        "bilistream",
                        &mut std::io::stdout(),
                    );
                }
                _ => unreachable!(),
            }
        }
        _ => {
            {
                // Check if this is first run
                let config_path = std::env::current_exe()?.with_file_name("config.json");
                let cookies_path = std::env::current_exe()?.with_file_name("cookies.json");
                let is_first_run = !config_path.exists() || !cookies_path.exists();

                // Initialize logger with capture for webui mode
                init_logger_with_capture();
                apply_webui_listen(&matches)?;

                // On Windows, default to tray mode
                // On Linux, default to WebUI mode
                #[cfg(target_os = "windows")]
                let use_tray_mode = true;
                #[cfg(not(target_os = "windows"))]
                let use_tray_mode = false;

                if use_tray_mode {
                    // Windows tray mode: system tray + auto-open browser
                    let port = 3150u16;

                    if is_first_run {
                        tracing::info!("🚀 欢迎使用 Bilistream！");
                        tracing::info!("   检测到首次运行，启动设置向导...");
                    } else {
                        tracing::info!("🚀 启动 Bilistream 系统托盘模式");
                    }

                    tracing::info!("   Web UI 端口: {}", port);

                    // Spawn WebUI server in background
                    tokio::spawn(async move {
                        if let Err(e) = bilistream::webui::server::start_webui(port).await {
                            tracing::error!("Web UI 服务器错误: {}", e);
                        }
                    });

                    // Download dependencies in background after WebUI starts
                    tokio::spawn(async move {
                        tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
                        if let Err(e) = bilistream::deps::ensure_all_dependencies().await {
                            tracing::error!("⚠️ 下载依赖项失败: {}", e);
                            tracing::error!("请手动从 GitHub 下载必需文件");
                        }
                    });

                    // Spawn monitoring loop in separate thread with its own runtime
                    tracing::info!("🔄 监控循环已启动");
                    let log_level = ffmpeg_log_level.to_string();
                    std::thread::spawn(move || {
                        let rt = tokio::runtime::Runtime::new().unwrap();
                        rt.block_on(async move {
                            tracing::info!("🔄 进入监控循环...");

                            // Check if config exists before starting
                            let config_path = std::env::current_exe()
                                .unwrap()
                                .with_file_name("config.json");
                            let cookies_path = std::env::current_exe()
                                .unwrap()
                                .with_file_name("cookies.json");

                            if !config_path.exists() {
                                tracing::warn!("⚠️ 配置文件不存在，等待用户配置...");
                                tracing::info!("💡 请访问 Web UI 进行配置");

                                // Wait for config to be created
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

                                // Wait for cookies to be created
                                loop {
                                    tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
                                    if cookies_path.exists() {
                                        tracing::info!("✅ 检测到登录凭证，开始监控");
                                        break;
                                    }
                                }
                            }

                            // Now start the actual monitoring loop
                            loop {
                                match run_bilistream(&log_level).await {
                                    Ok(_) => {
                                        tracing::info!("监控循环正常结束");
                                        break;
                                    }
                                    Err(e) => {
                                        tracing::error!("监控循环错误: {}", e);
                                        tracing::info!("⏳ 5秒后重试...");
                                        tokio::time::sleep(tokio::time::Duration::from_secs(5))
                                            .await;
                                    }
                                }
                            }
                        });
                    });

                    // Give WebUI time to start
                    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
                    tracing::info!("✅ 后台服务已启动");

                    // Run system tray (this will block until quit)
                    bilistream::tray::run_tray(port).await?;
                } else {
                    // Default: Start Web UI (Linux or non-tray build)
                    use bilistream::webui::start_webui;

                    if is_first_run {
                        tracing::info!("🚀 欢迎使用 Bilistream！");
                        tracing::info!("   检测到首次运行，启动 Web 设置向导...");
                        tracing::info!("");
                        tracing::info!("📋 请在浏览器中完成设置：");
                        tracing::info!("   1. 打开浏览器访问 http://localhost:3150");
                        tracing::info!("   2. 按照向导完成 Bilibili 登录和配置");
                        tracing::info!("   3. 配置完成后即可开始使用");
                        tracing::info!("");
                    } else {
                        tracing::info!("🚀 启动 Web UI 和自动监控模式");
                    }

                    #[cfg(target_os = "windows")]
                    {
                        tracing::info!("⚠️ 请勿关闭此窗口 ⚠️");
                        // Show notification about where the service is hosted
                        if let Err(e) = show_windows_notification() {
                            eprintln!("无法显示通知: {}", e);
                        }
                    }

                    #[cfg(not(target_os = "windows"))]
                    {
                        tracing::info!("💡 提示: 使用 --cli 以命令行模式运行");
                    }

                    // Spawn WebUI server in background
                    tokio::spawn(async move {
                        if let Err(e) = start_webui(3150).await {
                            tracing::error!("Web UI 服务器错误: {}", e);
                        }
                    });

                    // Download dependencies in background after WebUI starts
                    tokio::spawn(async move {
                        tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
                        if let Err(e) = bilistream::deps::ensure_all_dependencies().await {
                            tracing::error!("⚠️ 下载依赖项失败: {}", e);
                            tracing::error!("请手动从 GitHub 下载必需文件");
                        }
                    });

                    // Give WebUI time to start
                    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
                    tracing::info!("✅ Web UI 已启动");

                    // Only run monitoring loop if config exists (not first run)
                    if !is_first_run {
                        // Run monitoring loop in foreground (this will block)
                        run_bilistream(ffmpeg_log_level).await?;
                    } else {
                        // First run: wait for config to be created, then start monitoring
                        tracing::info!("⏳ 等待配置完成...");
                        tracing::info!("   配置完成后将自动开始监控");

                        // Poll for config file creation
                        loop {
                            tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;

                            // Check if config was created
                            if config_path.exists() && cookies_path.exists() {
                                tracing::info!("✅ 检测到配置文件已创建！");
                                tracing::info!("🚀 正在启动监控...");
                                tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

                                // Start monitoring loop
                                run_bilistream(ffmpeg_log_level).await?;
                                break;
                            }
                        }
                    }
                }
            }
        }
    }
    Ok(())
}

fn apply_webui_listen(matches: &clap::ArgMatches) -> Result<(), Box<dyn std::error::Error>> {
    let bind = matches
        .get_one::<String>("bind")
        .cloned()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| {
            std::env::var("BILISTREAM_BIND")
                .ok()
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
        })
        .unwrap_or_else(|| "127.0.0.1".to_string());
    let password = matches
        .get_one::<String>("password")
        .cloned()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| {
            std::env::var("BILISTREAM_PASSWORD")
                .ok()
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
        });
    bilistream::webui::install_listen(&bind, password)?;
    Ok(())
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
fn show_windows_notification() -> Result<(), Box<dyn std::error::Error>> {
    use std::process::Command as StdCommand;

    // Build notification message
    let mut message = String::from("🌐 Web UI 服务已启动\n");
    message.push_str("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━\n");
    message.push_str("📍 本地访问: http://localhost:3150\n");
    message.push_str("📍 本地访问: http://127.0.0.1:3150\n");

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
}
