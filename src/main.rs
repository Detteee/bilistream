// Hide console window on Windows in release mode
#![cfg_attr(
    all(target_os = "windows", not(debug_assertions)),
    windows_subsystem = "windows"
)]

use bilistream::config::{load_config, BiliLive, Config, Credentials, Twitch, Youtube};
use bilistream::plugins::bilibili::get_thumbnail;
use bilistream::plugins::Twitch as TwitchClient;
use bilistream::plugins::Youtube as YoutubeClient;
use bilistream::plugins::{
    bili_change_live_title, bili_start_live, bili_stop_live, bili_update_area, bilibili,
    check_area_id_with_title, clear_config_updated, clear_manual_restart, clear_manual_stop,
    clear_warning_stop, enable_danmaku_commands, ffmpeg, get_aliases, get_area_name,
    get_bili_live_status, get_channel_name, get_puuid, is_config_updated,
    is_danmaku_commands_enabled, is_danmaku_running, is_ffmpeg_running, run_danmaku, send_danmaku,
    should_skip_due_to_warned, should_skip_due_to_warning, stop_danmaku, stop_ffmpeg, wait_ffmpeg,
    was_manual_restart, was_manual_stop,
};

use chrono::{DateTime, Local};
use clap::{Arg, Command};
use regex::Regex;
use riven::consts::PlatformRoute;
use riven::RiotApi;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
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
static INVALID_ID_DETECTED: AtomicBool = AtomicBool::new(false);
// Track last video/stream ID for cover change detection (works across platforms)
static LAST_VIDEO_ID: Mutex<Option<String>> = Mutex::new(None);
// Track last banned keyword warning to prevent spam
static LAST_BANNED_KEYWORD_WARNING: Mutex<Option<String>> = Mutex::new(None);

#[derive(PartialEq)]
enum CollisionResult {
    Continue,
    Proceed,
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
                stop_danmaku();
            }
        }

        // Validate YouTube/Twitch configuration
        if cfg.youtube.channel_id.is_empty() && cfg.twitch.channel_id.is_empty() {
            tracing::error!("❌ YouTube 和 Twitch 配置均为空");
            tracing::error!("请在 WebUI 中配置或手动编辑 config.json 文件");
            tracing::info!("💡 提示: 访问 WebUI 进行配置，或参考 config.json.example");
            // Sleep and continue to allow WebUI configuration
            tokio::time::sleep(Duration::from_secs(cfg.interval)).await;
            continue 'outer;
        }

        // Check YouTube status (only if enabled)
        let (yt_live, mut yt_is_live, yt_area, yt_title, yt_m3u8_url, scheduled_start, yt_video_id) =
            if cfg.youtube.enable_monitor && !cfg.youtube.channel_id.is_empty() {
                let yt_live = YoutubeClient::new(
                    &cfg.youtube.channel_name,
                    &cfg.youtube.channel_id,
                    cfg.proxy.clone(),
                );
                let (yt_is_live, yt_area, yt_title, yt_m3u8_url, mut scheduled_start, yt_video_id) =
                    yt_live
                        .get_status()
                        .await
                        .unwrap_or((false, None, None, None, None, None));
                if scheduled_start.is_some() {
                    if scheduled_start.unwrap()
                        > Local::now() + Duration::from_secs(2 * 24 * 60 * 60)
                    {
                        scheduled_start = None;
                    }
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

        // Check Twitch status (only if enabled)
        let (tw_live, mut tw_is_live, tw_area, tw_title, tw_m3u8_url, tw_stream_id) =
            if cfg.twitch.enable_monitor && !cfg.twitch.channel_id.is_empty() {
                let tw_live = TwitchClient::new(
                    &cfg.twitch.channel_id,
                    cfg.twitch.oauth_token.clone(),
                    cfg.twitch.proxy_region.clone(),
                );
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
            } else {
                (None, false, None, None, None, None)
            };

        // Get Bilibili status
        let (bili_is_live, bili_title, bili_area_id) =
            match get_bili_live_status(cfg.bililive.room).await {
                Ok(status) => status,
                Err(e) => {
                    tracing::error!("获取B站直播状态失败: {}", e);
                    tracing::warn!("⚠️ 将在下次循环重试");
                    tokio::time::sleep(Duration::from_secs(cfg.interval)).await;
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
                })
            } else {
                None
            },
        });

        // Modified main code section
        if cfg.enable_anti_collision {
            match handle_collisions(&mut yt_is_live, &mut tw_is_live).await? {
                CollisionResult::Continue => continue 'outer,
                CollisionResult::Proceed => (),
            }
        }

        if yt_is_live || tw_is_live {
            NO_LIVE.store(false, Ordering::SeqCst);

            // Check if YouTube channel should be skipped due to warning
            if yt_is_live && should_skip_due_to_warning(&cfg.youtube.channel_name) {
                // Only log warning message once
                if should_skip_due_to_warned(&cfg.youtube.channel_name) {
                    tracing::warn!(
                        "⚠️ 跳过频道 {} - 之前因警告/切断停止",
                        &cfg.youtube.channel_name
                    );
                    if cfg.bililive.enable_danmaku_command && !is_danmaku_commands_enabled() {
                        enable_danmaku_commands(true);
                        if let Err(e) = send_danmaku(
                            &cfg,
                            &format!(
                                "⚠️ {} 因警告/切断被跳过，可使用弹幕指令换台",
                                &cfg.youtube.channel_name
                            ),
                        )
                        .await
                        {
                            tracing::error!("Failed to send danmaku: {}", e);
                        }
                    }
                }
                // Set YouTube as not live so Twitch can be used if available
                yt_is_live = false;
            }

            // Check if Twitch channel should be skipped due to warning
            if tw_is_live && should_skip_due_to_warning(&cfg.twitch.channel_name) {
                // Only log warning message once
                if should_skip_due_to_warned(&cfg.twitch.channel_name) {
                    tracing::warn!(
                        "⚠️ 跳过频道 {} - 之前因警告/切断停止",
                        &cfg.twitch.channel_name
                    );
                    if cfg.bililive.enable_danmaku_command && !is_danmaku_commands_enabled() {
                        enable_danmaku_commands(true);
                        if let Err(e) = send_danmaku(
                            &cfg,
                            &format!(
                                "⚠️ {} 因警告/切断被跳过，可使用弹幕指令换台",
                                &cfg.twitch.channel_name
                            ),
                        )
                        .await
                        {
                            tracing::error!("Failed to send danmaku: {}", e);
                        }
                    }
                }
                // Set Twitch as not live
                tw_is_live = false;
            }

            // Check if config was updated by danmaku command after warning filtering
            if is_config_updated() {
                clear_config_updated();
                tracing::info!("🔄 检测到配置更新（弹幕指令），重新加载配置并检查频道状态");
                continue 'outer;
            }

            // If both channels are skipped after filtering, continue to next iteration
            if !yt_is_live && !tw_is_live {
                tokio::time::sleep(Duration::from_secs(cfg.interval)).await;
                continue 'outer;
            }

            // Clear warning stop since we have a valid channel to stream
            clear_warning_stop();

            // Disable danmaku commands when streaming
            if is_danmaku_commands_enabled() {
                enable_danmaku_commands(false);
            }
            let (platform, channel_name, channel_id, mut area_v2, cfg_title) = if yt_is_live {
                (
                    "YT",
                    cfg.youtube.channel_name.clone(),
                    cfg.youtube.channel_id.clone(),
                    cfg.youtube.area_v2,
                    format!("【转播】{}", cfg.youtube.channel_name),
                )
            } else {
                (
                    "TW",
                    cfg.twitch.channel_name.clone(),
                    cfg.twitch.channel_id.clone(),
                    cfg.twitch.area_v2,
                    format!("【转播】{}", cfg.twitch.channel_name),
                )
            };
            let yot_area = if yt_is_live { yt_area } else { tw_area };
            let mut title = if yt_is_live { yt_title } else { tw_title };
            let mut m3u8_url = if yt_is_live { yt_m3u8_url } else { tw_m3u8_url };
            let current_video_id = if yt_is_live {
                yt_video_id
            } else {
                tw_stream_id
            };

            // Check if video/stream ID has changed
            let video_id_changed = {
                let mut last_id = LAST_VIDEO_ID.lock().unwrap();
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

            if yot_area.is_some() && title.is_some() {
                title = Some(format!("{} {}", yot_area.unwrap(), title.unwrap()));
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
            if let Some(keyword) = load_streaming_banned_keywords()
                .iter()
                .find(|k| title.as_ref().map_or(false, |t| t.contains(k.as_str())))
            {
                // Check if we already warned about this keyword for this stream
                let should_warn = {
                    let mut last_warning = LAST_BANNED_KEYWORD_WARNING.lock().unwrap();
                    let current_warning = format!("{}:{}", keyword, title_str);
                    if last_warning.as_ref() != Some(&current_warning) {
                        *last_warning = Some(current_warning);
                        true
                    } else {
                        false
                    }
                };

                if should_warn {
                    tracing::error!("直播标题/分区包含不支持的关键词: {}", keyword);
                    if let Err(e) =
                        send_danmaku(&cfg, &format!("错误：标题/分区含:{}", keyword)).await
                    {
                        tracing::error!("Failed to send danmaku: {}", e);
                    }
                    if cfg.bililive.enable_danmaku_command && !is_danmaku_commands_enabled() {
                        enable_danmaku_commands(true);
                        thread::sleep(Duration::from_secs(2));
                        if let Err(e) = send_danmaku(&cfg, "可使用弹幕指令进行换台").await
                        {
                            tracing::error!("Failed to send danmaku: {}", e);
                        }
                    }
                }
                tokio::time::sleep(Duration::from_secs(cfg.interval)).await;
                continue 'outer;
            }
            // Reuse bili_is_live, bili_title, bili_area_id from earlier check (line 200)
            if !bili_is_live && (area_v2 != 86 || !INVALID_ID_DETECTED.load(Ordering::SeqCst)) {
                tracing::info!("B站未直播");
                let area_name = get_area_name(area_v2);

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
                            area_name.unwrap(),
                            area_v2
                        );
                        // Clear banned keyword warning when successfully starting a new stream
                        *LAST_BANNED_KEYWORD_WARNING.lock().unwrap() = None;
                    }
                    Err(e) => {
                        tracing::error!("B站开播失败: {}", e);
                        tracing::warn!("⚠️ 将在下次循环重试");
                        tokio::time::sleep(Duration::from_secs(cfg.interval)).await;
                        continue 'outer;
                    }
                }

                // If auto_cover is enabled, update Bilibili live cover
                if cfg.auto_cover
                    && (bili_title != cfg_title || bili_area_id != area_v2 || video_id_changed)
                {
                    match get_thumbnail(platform, &channel_id, cfg.proxy.clone()).await {
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
                    match get_thumbnail(platform, &channel_id, cfg.proxy.clone()).await {
                        Ok(cover_path) if !cover_path.is_empty() => {
                            tokio::time::sleep(Duration::from_secs(2)).await;
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
                }
            }

            // Execute ffmpeg with platform-specific locks
            // Main ffmpeg monitoring loop - blocks until stream ends
            loop {
                ffmpeg(
                    cfg.bililive.bili_rtmp_url.clone(),
                    cfg.bililive.bili_rtmp_key.clone(),
                    m3u8_url.clone().unwrap(),
                    cfg.proxy.clone(),
                    ffmpeg_log_level.to_string(),
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

                let (current_is_live, _, _, new_m3u8_url, _, _) = if yt_is_live {
                    if let Some(ref client) = yt_live {
                        client
                            .get_status()
                            .await
                            .unwrap_or((false, None, None, None, None, None))
                    } else {
                        (false, None, None, None, None, None)
                    }
                } else {
                    if let Some(ref client) = tw_live {
                        client
                            .get_status()
                            .await
                            .unwrap_or((false, None, None, None, None, None))
                    } else {
                        (false, None, None, None, None, None)
                    }
                };
                let (bili_is_live, _, _) = match get_bili_live_status(cfg.bililive.room).await {
                    Ok(status) => status,
                    Err(e) => {
                        tracing::error!("获取B站直播状态失败: {}", e);
                        // Assume still live and continue, will retry next iteration
                        (true, String::new(), 0)
                    }
                };

                if !current_is_live || !bili_is_live {
                    tracing::info!("直播已结束，停止ffmpeg监控循环");
                    break;
                }

                // Check if manual restart was requested (force immediate restart)
                if was_manual_restart() {
                    tracing::info!("🔄 检测到手动重启请求，立即退出ffmpeg监控循环");
                    clear_manual_restart();
                    break;
                }

                // Check if config was updated (channel switch)
                // Only break if stream has ended, otherwise continue streaming current channel
                if is_config_updated() {
                    tracing::info!("🔄 检测到配置更新请求，但当前流仍在进行，继续转播直到流结束");
                    // Don't break, let the stream continue until it naturally ends
                }

                // Update m3u8 URL if it changed
                if new_m3u8_url.is_some() && new_m3u8_url != m3u8_url {
                    tracing::info!("🔄 检测到流URL变化，使用新URL重启");
                    m3u8_url = new_m3u8_url;
                }

                // Stream is still live but ffmpeg exited, restart it
                tracing::info!("🔄 流仍在进行，重启ffmpeg...");
                tokio::time::sleep(Duration::from_secs(1)).await;
            }

            // Check the actual reason for ffmpeg loop exit
            let manual_stop = was_manual_stop();
            let config_updated = is_config_updated();
            let warning_skip = should_skip_due_to_warning(&channel_name);

            // Check current live status to determine what actually happened
            let (current_is_live, _, _, _, _, _) = if yt_is_live {
                if let Some(ref client) = yt_live {
                    client
                        .get_status()
                        .await
                        .unwrap_or((false, None, None, None, None, None))
                } else {
                    (false, None, None, None, None, None)
                }
            } else {
                if let Some(ref client) = tw_live {
                    client
                        .get_status()
                        .await
                        .unwrap_or((false, None, None, None, None, None))
                } else {
                    (false, None, None, None, None, None)
                }
            };
            let (bili_is_live, _, _) = match get_bili_live_status(cfg.bililive.room).await {
                Ok(status) => status,
                Err(e) => {
                    tracing::error!("获取B站直播状态失败: {}", e);
                    // Assume still live to avoid incorrect status messages
                    (true, String::new(), 0)
                }
            };

            // Determine what happened and send appropriate message
            if manual_stop {
                clear_manual_stop();
                tracing::info!("Stream was stopped manually, skipping end danmaku");
            } else if config_updated {
                tracing::info!("Stream stopped due to config update/restart, skipping end danmaku");
            } else if warning_skip {
                tracing::info!("Stream was stopped due to warning/cut off");
            } else if !current_is_live && bili_is_live {
                // Source stream ended but B站 is still live
                tracing::info!("{} 直播结束", channel_name);
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
            if scheduled_start.is_some() {
                if yt_title.is_some() {
                    let current_message = box_message(
                        &cfg.youtube.channel_name,
                        cfg.youtube.enable_monitor,
                        Some(scheduled_start.unwrap()),
                        Some(&yt_title.unwrap()),
                        &cfg.twitch.channel_name,
                        cfg.twitch.enable_monitor,
                    );

                    let mut last = LAST_MESSAGE.lock().unwrap();
                    let should_update = match last.as_ref() {
                        Some(last_msg) if last_msg.as_ref() == current_message.as_str() => false,
                        Some(last_msg) => {
                            // Only update if message content changed significantly
                            let time_diff = if let Some(last_time) = extract_time(last_msg) {
                                if let Some(current_time) = extract_time(&current_message) {
                                    (current_time - last_time).num_minutes().abs()
                                } else {
                                    i64::MAX
                                }
                            } else {
                                i64::MAX
                            };
                            time_diff > 5 || remove_time(last_msg) != remove_time(&current_message)
                        }
                        None => true,
                    };

                    if should_update {
                        print!("{}", current_message);
                        *last = Some(current_message.into_boxed_str());
                    }
                } else {
                    let current_message = box_message(
                        &cfg.youtube.channel_name,
                        cfg.youtube.enable_monitor,
                        None,
                        None,
                        &cfg.twitch.channel_name,
                        cfg.twitch.enable_monitor,
                    );

                    let mut last = LAST_MESSAGE.lock().unwrap();
                    let should_update = match last.as_ref() {
                        Some(last_msg) if last_msg.as_ref() == current_message.as_str() => false,
                        Some(last_msg) => {
                            let time_diff = if let Some(last_time) = extract_time(last_msg) {
                                if let Some(current_time) = extract_time(&current_message) {
                                    (current_time - last_time).num_minutes().abs()
                                } else {
                                    i64::MAX
                                }
                            } else {
                                i64::MAX
                            };
                            time_diff > 5 || remove_time(last_msg) != remove_time(&current_message)
                        }
                        None => true,
                    };

                    if should_update {
                        print!("{}", current_message);
                        *last = Some(current_message.into_boxed_str());
                    }
                }
            } else {
                if !NO_LIVE.load(Ordering::SeqCst) {
                    let current_message = box_message(
                        &cfg.youtube.channel_name,
                        cfg.youtube.enable_monitor,
                        None,
                        None, // No title when not streaming
                        &cfg.twitch.channel_name,
                        cfg.twitch.enable_monitor,
                    );
                    print!("{}", current_message);
                    let mut last = LAST_MESSAGE.lock().unwrap();
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
                tokio::time::sleep(Duration::from_secs(sleep_time)).await;
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
    // Initialize variables first
    let (yt_line, width) = if !yt_monitor_enabled {
        let line = format!("YT: 监听已关闭");
        (line.clone(), line.width() + 2)
    } else if scheduled_time.is_some() {
        let line = format!(
            "YT: {} 未直播，计划于 {} 开始，",
            yt_channel,
            scheduled_time.unwrap().format("%Y-%m-%d %H:%M:%S")
        );
        (line.clone(), line.width() + 2)
    } else {
        let line = format!(
            "YT: {} 未直播                                   ",
            yt_channel
        );
        (line.clone(), line.width() + 2)
    };

    let mut message = format!(
        "\r\x1b[K\x1b[1m┌{:─<width$}┐\n\
         │ {} │\n",
        "",
        yt_line,
        width = width
    );

    if let Some(title_text) = title {
        let wrapped_title = textwrap::fill(title_text, width - 6);
        for line in wrapped_title.lines() {
            let padding = width - 6 - line.width();
            message.push_str(&format!("│     {}{} │\n", line, " ".repeat(padding)));
        }
    }

    message.push_str(&format!("├{:─<width$}┤\n", "", width = width));

    let tw_line = if !tw_monitor_enabled {
        format!("TW: 监听已关闭")
    } else {
        format!("TW: {} 未直播", tw_channel)
    };
    let padding = width - 2 - tw_line.width();
    message.push_str(&format!(
        "│ {}{} │\n\
         └{:─<width$}┘\x1b[0m\n",
        tw_line,
        " ".repeat(padding),
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
                let area_name = get_area_name(area_id);
                println!(
                    "B站直播中, 标题: {}, 分区: {} （ID: {}）",
                    title,
                    area_name.unwrap(),
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
            let mut channel_name = get_channel_name("YT", channel_id).unwrap();
            if channel_name.is_none() {
                channel_name = Some(channel_id.to_string());
            }
            let yt_client = YoutubeClient::new(
                channel_name.as_deref().unwrap_or(channel_id),
                channel_id,
                cfg.proxy.clone(),
            );
            let (is_live, topic, title, _, start_time, _) = yt_client.get_status().await?;
            if is_live {
                println!(
                    "{} 在 YouTube 直播中, 分区: {}, 标题: {}",
                    channel_name.unwrap(),
                    topic.unwrap(),
                    title.unwrap()
                );
            } else {
                if start_time.is_some() {
                    println!(
                        "{} 未在 YouTube 直播, {}计划于 {} 开始, 标题: {}",
                        channel_name.unwrap(),
                        if let Some(t) = &topic {
                            format!("分区: {}, ", t)
                        } else {
                            String::new()
                        },
                        start_time.unwrap().format("%Y-%m-%d %H:%M:%S"),
                        title.unwrap()
                    );
                } else {
                    println!("{} 未在 YouTube 直播", channel_name.unwrap());
                }
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
            let mut channel_name = get_channel_name("TW", channel_id).unwrap();
            if channel_name.is_none() {
                channel_name = Some(channel_id.to_string());
            }
            let tw_client = TwitchClient::new(
                channel_id,
                cfg.twitch.oauth_token.clone(),
                cfg.twitch.proxy_region.clone(),
            );
            let (is_live, game_name, title, _, _, _) = tw_client.get_status().await?;
            if is_live {
                println!(
                    "{} 在 Twitch 直播中, 分区: {}, 标题: {}",
                    channel_name.unwrap(),
                    game_name.unwrap(),
                    title.unwrap()
                );
            } else {
                println!("{} 未在 Twitch 直播", channel_name.unwrap());
            }
            Ok(())
        }
        // all 平台 output all platform
        "all" => {
            let cfg = load_config().await?;
            let (is_live, title, area_id) = get_bili_live_status(cfg.bililive.room).await?;
            if is_live {
                let area_name = get_area_name(area_id);
                println!(
                    "B站直播中, 标题: {}, 分区: {} （ID: {}）",
                    title,
                    area_name.unwrap(),
                    area_id,
                );
            } else {
                println!("B站未直播");
            }
            let channel_id = cfg.youtube.channel_id;
            let channel_name = cfg.youtube.channel_name;

            let yt_client = YoutubeClient::new(&channel_name, &channel_id, cfg.proxy.clone());
            let (is_live, topic, title, _, start_time, _) = yt_client.get_status().await?;
            if is_live {
                if topic.is_some() {
                    println!(
                        "{} 在 YouTube 直播中, 分区: {}, 标题: {}",
                        channel_name,
                        topic.unwrap(),
                        title.unwrap()
                    );
                } else {
                    println!(
                        "{} 在 YouTube 直播中, 标题: {}",
                        channel_name,
                        title.unwrap()
                    );
                }
            } else {
                if start_time.is_some() {
                    println!(
                        "{} 未在 YouTube 直播, {}计划于 {} 开始, 标题: {}",
                        channel_name,
                        if let Some(t) = &topic {
                            format!("分区: {}, ", t)
                        } else {
                            String::new()
                        },
                        start_time.unwrap().format("%Y-%m-%d %H:%M:%S"),
                        title.unwrap()
                    );
                } else {
                    println!("{} 未在 YouTube 直播", channel_name);
                }
            }
            let channel_id = cfg.twitch.channel_id;
            let channel_name = cfg.twitch.channel_name;
            let tw_client = TwitchClient::new(
                &channel_id,
                cfg.twitch.oauth_token.clone(),
                cfg.twitch.proxy_region.clone(),
            );
            let (is_live, game_name, title, _, _, _) = tw_client.get_status().await?;
            if is_live {
                println!(
                    "{} 在 Twitch 直播中, 分区: {}, 标题: {}",
                    channel_name,
                    game_name.unwrap(),
                    title.unwrap()
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
    bili_start_live(&mut cfg, area_v2).await?;
    println!("直播开始成功");
    println!("url：{}", cfg.bililive.bili_rtmp_url);
    println!("key：{}", cfg.bililive.bili_rtmp_key);
    Ok(())
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
    let riot_api = RiotApi::new(cfg.riot_api_key.clone().unwrap());
    thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().unwrap();
        loop {
            rt.block_on(async {
                if let Ok(game_data) = riot_api
                    .spectator_v5()
                    .get_current_game_info_by_puuid(PlatformRoute::JP1, &puuid)
                    .await
                {
                    if game_data.is_some() {
                        let riot_ids: Vec<String> = game_data
                            .unwrap()
                            .participants
                            .iter()
                            .filter_map(|p| p.riot_id.clone())
                            .collect();
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
                                    let is_live =
                                        get_bili_live_status(cfg.bililive.room).await.unwrap().0;
                                    if is_live {
                                        tracing::error!("检测到非法词汇:{}，停止直播", word);
                                        bili_stop_live(&cfg).await.unwrap();
                                        // Stop ffmpeg using supervisor
                                        rt.block_on(stop_ffmpeg());
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
                }

                // Check if ffmpeg is still running
                if !rt.block_on(ffmpeg::is_ffmpeg_running()) {
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
        let to_area_name = get_area_name(new_area);
        let area_name = get_area_name(current_area);
        if area_name.is_some() && to_area_name.is_some() {
            tracing::info!(
                "分区改变（{}->{})",
                area_name.unwrap(),
                to_area_name.unwrap()
            );
            let cfg = load_config().await?;
            bili_update_area(&cfg, new_area).await?;
        }
    }
    Ok(())
}

fn extract_time(message: &str) -> Option<DateTime<Local>> {
    let re = Regex::new(r"\d{4}-\d{2}-\d{2} \d{2}:\d{2}:\d{2}").ok()?;
    re.find(message)
        .and_then(|m| DateTime::parse_from_str(m.as_str(), "%Y-%m-%d %H:%M:%S").ok())
        .map(|dt| dt.with_timezone(&Local))
}

fn remove_time(message: &str) -> String {
    let re = Regex::new(r"\d{4}-\d{2}-\d{2} \d{2}:\d{2}:\d{2}").unwrap();
    re.replace_all(message, "TIME").to_string()
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

    // Collision handling logic
    let mut last_collision = LAST_COLLISION.lock().unwrap();
    if yt_collision.is_some() && tw_collision.is_some() {
        let yt_col = yt_collision.as_ref().unwrap();
        let current = (
            yt_col.0.clone().into_boxed_str(),
            yt_col.1,
            "双平台".to_string().into_boxed_str(),
        );

        // Check if we're already in a dual-platform collision state (regardless of specific room)
        let already_in_dual_collision = last_collision
            .as_ref()
            .map(|(_, _, platform)| platform.as_ref() == "双平台")
            .unwrap_or(false);

        if !already_in_dual_collision {
            tracing::warn!("YouTube和Twitch均检测到撞车，跳过本次转播");
            // send_danmaku(&cfg, "🚨YT和TW双平台撞车").await?;
            // tokio::time::sleep(Duration::from_secs(2)).await;
            if let Err(e) = send_danmaku(
                &cfg,
                &format!(
                    "{}({})正在转{}",
                    yt_collision.as_ref().unwrap().0,
                    yt_collision.as_ref().unwrap().1,
                    yt_collision.as_ref().unwrap().2,
                ),
            )
            .await
            {
                tracing::error!("Failed to send danmaku: {}", e);
            }
            if yt_collision.as_ref().unwrap().0 != tw_collision.as_ref().unwrap().0 {
                tokio::time::sleep(Duration::from_secs(2)).await;
                if let Err(e) = send_danmaku(
                    &cfg,
                    &format!(
                        "{}({})正在转{}",
                        tw_collision.as_ref().unwrap().0,
                        tw_collision.as_ref().unwrap().1,
                        tw_collision.as_ref().unwrap().2,
                    ),
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
            *last_collision = Some(current);
            Ok(CollisionResult::Continue)
        } else {
            Ok(CollisionResult::Continue)
        }
    } else if let Some(collision) = yt_collision.or(tw_collision) {
        let other_live = if collision.2 == cfg.youtube.channel_name {
            let ol = *tw_is_live;
            *yt_is_live = false;
            ol
        } else {
            let ol = *yt_is_live;
            *tw_is_live = false;
            ol
        };

        // Check if we're already in a collision state for this platform
        let already_in_collision = last_collision
            .as_ref()
            .map(|(_, _, platform)| platform.as_ref() == collision.2.as_str())
            .unwrap_or(false);

        if !other_live && !already_in_collision {
            tracing::warn!(
                "{}（{}）撞车，{}（{}）未开播",
                collision.0,
                collision.1,
                if collision.2 == cfg.youtube.channel_name {
                    "Twitch"
                } else {
                    "YouTube"
                },
                if collision.2 == cfg.youtube.channel_name {
                    cfg.twitch.channel_name.clone()
                } else {
                    cfg.youtube.channel_name.clone()
                }
            );
            if let Err(e) = send_danmaku(
                &cfg,
                &format!("{}({})正在转{}", collision.0, collision.1, collision.2,),
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
            *last_collision = Some((
                collision.0.into_boxed_str(),
                collision.1,
                collision.2.into_boxed_str(),
            ));
            Ok(CollisionResult::Continue)
        } else {
            Ok(CollisionResult::Proceed)
        }
    } else {
        Ok(CollisionResult::Proceed)
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

    // Proxy setting (may be needed for YouTube/Twitch access)
    print!("\n是否需要配置代理? (y/N): ");
    io::stdout().flush()?;
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    let proxy = if input.trim().eq_ignore_ascii_case("y") {
        print!("代理地址 (格式: http://host:port): ");
        io::stdout().flush()?;
        let mut proxy_input = String::new();
        io::stdin().read_line(&mut proxy_input)?;
        proxy_input.trim().to_string()
    } else {
        String::new()
    };

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

    let (yt_channel_name, yt_channel_id, yt_area_v2, yt_quality) = if configure_youtube {
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

        (name, id, area, quality)
    } else {
        ("".to_string(), "".to_string(), 235, "best".to_string())
    };

    // Get Twitch channel info
    print!("\n是否配置 Twitch 频道? (Y/n): ");
    io::stdout().flush()?;
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    let configure_twitch = !input.trim().eq_ignore_ascii_case("n");

    let (tw_channel_name, tw_channel_id, tw_area_v2, tw_oauth, tw_proxy_region, tw_quality) =
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

            println!("Twitch OAuth Token (可选，用于streamlink认证)");
            println!(
                "获取方法: https://streamlink.github.io/cli/plugins/twitch.html#authentication"
            );
            print!("请输入 (直接回车跳过): ");
            io::stdout().flush()?;
            let mut oauth = String::new();
            io::stdin().read_line(&mut oauth)?;
            let oauth = oauth.trim().to_string();

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

            (name, id, area, oauth, region, quality)
        } else {
            (
                "".to_string(),
                "".to_string(),
                235,
                "".to_string(),
                "as".to_string(),
                "best".to_string(),
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

    let (holodex_api_key, riot_api_key, enable_lol_monitor) = if configure_advanced {
        println!("\n高级选项配置");
        println!("----------------------------------------");

        println!("\nHolodex API Key (用于YouTube直播状态检测)");
        println!("获取方法: https://holodex.net/login");
        print!("请输入 (直接回车跳过): ");
        io::stdout().flush()?;
        let mut holodex = String::new();
        io::stdin().read_line(&mut holodex)?;
        let holodex = holodex.trim().to_string();

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

        (holodex, riot, enable_lol)
    } else {
        (String::new(), String::new(), false)
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
            oauth_token: tw_oauth,
            proxy_region: tw_proxy_region,
            quality: tw_quality,
        },
        youtube: Youtube {
            enable_monitor: true,
            channel_name: yt_channel_name,
            channel_id: yt_channel_id,
            area_v2: yt_area_v2,
            quality: yt_quality,
            cookies_file: None,
            cookies_from_browser: None,
        },
        proxy: if proxy.is_empty() { None } else { Some(proxy) },
        holodex_api_key: if holodex_api_key.is_empty() {
            None
        } else {
            Some(holodex_api_key)
        },
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
            let mut sigterm = signal::unix::signal(signal::unix::SignalKind::terminate()).unwrap();
            let mut sigint = signal::unix::signal(signal::unix::SignalKind::interrupt()).unwrap();

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
                    let current_area_name = get_area_name(current_area);
                    let area_name = get_area_name(*area_id);
                    if current_area_name.is_some() && area_name.is_some() {
                        println!(
                            "直播间分区更新成功, {} -> {}",
                            current_area_name.unwrap(),
                            area_name.unwrap()
                        );
                    } else {
                        println!("直播间分区更新成功, {} -> {}", current_area, area_id);
                    }
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
            let legacy_config_path = std::env::current_exe()?.with_file_name("config.yaml");
            let cookies_path = std::env::current_exe()?.with_file_name("cookies.json");
            let needs_setup =
                (!config_path.exists() && !legacy_config_path.exists()) || !cookies_path.exists();

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
                    let legacy_config_path = std::env::current_exe()
                        .unwrap()
                        .with_file_name("config.yaml");
                    let cookies_path = std::env::current_exe()
                        .unwrap()
                        .with_file_name("cookies.json");

                    if !config_path.exists() && !legacy_config_path.exists() {
                        tracing::warn!("⚠️ 配置文件不存在，等待用户配置...");
                        tracing::info!("💡 请访问 Web UI 进行配置");

                        // Wait for config to be created
                        loop {
                            tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
                            if config_path.exists() || legacy_config_path.exists() {
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
            bilistream::tray::run_tray(port)?;
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
                let legacy_config_path = std::env::current_exe()?.with_file_name("config.yaml");
                let cookies_path = std::env::current_exe()?.with_file_name("cookies.json");
                let is_first_run = (!config_path.exists() && !legacy_config_path.exists())
                    || !cookies_path.exists();

                // Initialize logger with capture for webui mode
                init_logger_with_capture();

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
                            let legacy_config_path = std::env::current_exe()
                                .unwrap()
                                .with_file_name("config.yaml");
                            let cookies_path = std::env::current_exe()
                                .unwrap()
                                .with_file_name("cookies.json");

                            if !config_path.exists() && !legacy_config_path.exists() {
                                tracing::warn!("⚠️ 配置文件不存在，等待用户配置...");
                                tracing::info!("💡 请访问 Web UI 进行配置");

                                // Wait for config to be created
                                loop {
                                    tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
                                    if config_path.exists() || legacy_config_path.exists() {
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
                    bilistream::tray::run_tray(port)?;
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

fn init_logger() {
    tracing_subscriber::fmt()
        .with_timer(fmt::time::ChronoLocal::new("%H:%M:%S".to_string()))
        .with_target(true)
        .with_span_events(fmt::format::FmtSpan::NONE)
        .with_writer(std::io::stdout)
        .with_max_level(tracing::Level::INFO)
        .init();
}

fn init_logger_with_capture() {
    use tracing_subscriber::filter::LevelFilter;
    use tracing_subscriber::layer::SubscriberExt;

    // Create a custom writer that captures logs
    struct LogCapture;

    impl std::io::Write for LogCapture {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            if let Ok(s) = std::str::from_utf8(buf) {
                // Also write to stdout
                print!("{}", s);
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

    // Get local IP address
    let local_ip = if let Ok(socket) = std::net::UdpSocket::bind("0.0.0.0:0") {
        if socket.connect("8.8.8.8:80").is_ok() {
            if let Ok(local_addr) = socket.local_addr() {
                let ip = local_addr.ip();
                if !ip.is_loopback() {
                    Some(ip.to_string())
                } else {
                    None
                }
            } else {
                None
            }
        } else {
            None
        }
    } else {
        None
    };

    // Build notification message
    let mut message = String::from("🌐 Web UI 服务已启动\n");
    message.push_str("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━\n");
    message.push_str("📍 本地访问: http://localhost:3150\n");
    message.push_str("📍 本地访问: http://127.0.0.1:3150\n");
    if let Some(ip) = local_ip {
        message.push_str(&format!("📍 局域网访问: http://{}:3150", ip));
    }

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
