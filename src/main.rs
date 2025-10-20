use bilistream::config::load_config;
use bilistream::plugins::{
    bili_change_live_title, bili_start_live, bili_stop_live, bili_update_area, bilibili,
    check_area_id_with_title, ffmpeg, get_aliases, get_area_name, get_bili_live_status,
    get_channel_name, get_puuid, get_thumbnail, get_twitch_status, get_youtube_status,
    is_danmaku_running, is_ffmpeg_running, run_danmaku, select_live, send_danmaku,
};

use chrono::{DateTime, Local};
use clap::{Arg, Command};
use regex::Regex;
use riven::consts::PlatformRoute;
use riven::RiotApi;
use std::process::Command as StdCommand;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::{error::Error, thread, time::Duration};
use textwrap;
use tracing_subscriber::fmt;
use unicode_width::UnicodeWidthStr;

static NO_LIVE: AtomicBool = AtomicBool::new(false);
static LAST_MESSAGE: Mutex<String> = Mutex::new(String::new());
static LAST_COLLISION: Mutex<Option<(String, i32, String)>> = Mutex::new(None);
static INVALID_ID_DETECTED: AtomicBool = AtomicBool::new(false);
static DANMAKU_KAMITO_APEX: AtomicBool = AtomicBool::new(true);

#[derive(PartialEq)]
enum CollisionResult {
    Continue,
    Proceed,
}

const BANNED_KEYWORDS: [&str; 11] = [
    "どうぶつの森",
    "animal crossing",
    "asmr",
    "dbd",
    "dead by daylight",
    "l4d2",
    "left 4 dead 2",
    "gta",
    "mad town",
    "watchalong",
    "watchparty",
];

fn init_logger() {
    tracing_subscriber::fmt()
        .with_timer(fmt::time::ChronoLocal::new("%H:%M:%S".to_string()))
        .with_target(true)
        .with_span_events(fmt::format::FmtSpan::NONE)
        .init();
}

async fn run_bilistream(ffmpeg_log_level: &str) -> Result<(), Box<dyn std::error::Error>> {
    // Initialize the logger with timestamp format : 2024-11-21 12:00:00
    init_logger();

    if is_ffmpeg_running() {
        //pkill ffmpeg;
        let mut cmd = StdCommand::new("pkill");
        cmd.arg("ffmpeg");
        cmd.spawn()?;
    }
    if is_danmaku_running() {
        let mut cmd = StdCommand::new("pkill");
        cmd.arg("live-danmaku-cli");
        cmd.spawn()?;
    }

    'outer: loop {
        let mut cfg = load_config().await?;
        // Check YouTube status
        let yt_live = select_live(cfg.clone(), "YT").await?;
        let (mut yt_is_live, yt_area, yt_title, yt_m3u8_url, mut scheduled_start) = yt_live
            .get_status()
            .await
            .unwrap_or((false, None, None, None, None));
        if scheduled_start.is_some() {
            if scheduled_start.unwrap() > Local::now() + Duration::from_secs(2 * 24 * 60 * 60) {
                scheduled_start = None;
            }
        }
        // Check Twitch status
        let tw_live = select_live(cfg.clone(), "TW").await?;
        let (mut tw_is_live, tw_area, tw_title, tw_m3u8_url, _) = tw_live
            .get_status()
            .await
            .unwrap_or((false, None, None, None, None));

        // Modified main code section
        if cfg.enable_anti_collision {
            match handle_collisions(&mut yt_is_live, &mut tw_is_live).await? {
                CollisionResult::Continue => continue 'outer,
                CollisionResult::Proceed => (),
            }
        }

        if yt_is_live || tw_is_live {
            NO_LIVE.store(false, Ordering::SeqCst);
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
            let m3u8_url = if yt_is_live { yt_m3u8_url } else { tw_m3u8_url };
            tracing::info!(
                "{} 正在 {} 直播, 标题:\n          {}",
                channel_name,
                platform,
                title.clone().unwrap()
            );

            if yot_area.is_some() {
                title = Some(format!("{} {}", yot_area.unwrap(), title.unwrap()));
            }
            area_v2 = check_area_id_with_title(&title.as_ref().unwrap(), area_v2);
            if area_v2 == 86 {
                let puuid = get_puuid(&channel_name)?;
                if puuid != "" {
                    monitor_lol_game(puuid).await?;
                }
            } else {
                INVALID_ID_DETECTED.store(false, Ordering::SeqCst);
            }
            if area_v2 == 240
                && !channel_name.contains("Kamito")
                && DANMAKU_KAMITO_APEX.load(Ordering::SeqCst)
            {
                send_danmaku(&cfg, &format!("Apex分区只转播 Kamito")).await?;
                DANMAKU_KAMITO_APEX.store(false, Ordering::SeqCst);
                if cfg.bililive.enable_danmaku_command && !is_danmaku_running() {
                    thread::spawn(move || run_danmaku());
                    thread::sleep(Duration::from_secs(2));
                    send_danmaku(&cfg, "可使用弹幕指令进行换台").await?;
                }
                tokio::time::sleep(Duration::from_secs(cfg.interval)).await;
                continue;
            } else if area_v2 == 240
                && !channel_name.contains("Kamito")
                && !DANMAKU_KAMITO_APEX.load(Ordering::SeqCst)
            {
                tokio::time::sleep(Duration::from_secs(cfg.interval)).await;
                continue;
            } else {
                DANMAKU_KAMITO_APEX.store(true, Ordering::SeqCst);
            }
            if let Some(keyword) = BANNED_KEYWORDS
                .iter()
                .find(|k| title.as_ref().unwrap().contains(*k))
            {
                tracing::error!("直播标题/分区包含不支持的关键词:\n{}", keyword);
                send_danmaku(&cfg, &format!("错误：标题/分区含:{}", keyword)).await?;
                if cfg.bililive.enable_danmaku_command && !is_danmaku_running() {
                    thread::spawn(move || run_danmaku());
                    thread::sleep(Duration::from_secs(2));
                    send_danmaku(&cfg, "可使用弹幕指令进行换台").await?;
                }
                tokio::time::sleep(Duration::from_secs(cfg.interval)).await;
                continue;
            }
            let (bili_is_live, bili_title, bili_area_id) =
                get_bili_live_status(cfg.bililive.room).await?;
            if !bili_is_live && (area_v2 != 86 || !INVALID_ID_DETECTED.load(Ordering::SeqCst)) {
                tracing::info!("B站未直播");
                let area_name = get_area_name(area_v2);
                bili_start_live(&mut cfg, area_v2).await?;
                if bili_title != cfg_title {
                    bili_change_live_title(&cfg, &cfg_title).await?;
                }
                tracing::info!(
                    "B站已开播，标题为 {}，分区为 {} （ID: {}）",
                    cfg_title,
                    area_name.unwrap(),
                    area_v2
                );
                // If auto_cover is enabled, update Bilibili live cover
                if cfg.auto_cover && (bili_title != cfg_title || bili_area_id != area_v2) {
                    let cover_path =
                        get_thumbnail(platform, &channel_id, cfg.proxy.clone()).await?;
                    if let Err(e) = bilibili::bili_change_cover(&cfg, &cover_path).await {
                        tracing::error!("B站直播间封面替换失败: {}", e);
                    } else {
                        tracing::info!("B站直播间封面替换成功");
                    }
                }
            } else {
                // 如果target channel改变，则变更B站直播标题
                if bili_title != cfg_title {
                    bili_change_live_title(&cfg, &cfg_title).await?;
                    tracing::info!("B站直播标题变更 （{}->{}）", bili_title, cfg_title);
                    // title is 【转播】频道名
                    let bili_channel_name = bili_title.split("【转播】").last().unwrap();
                    if bili_channel_name != channel_name {
                        tokio::time::sleep(Duration::from_secs(2)).await;
                        send_danmaku(
                            &cfg,
                            &format!("换台：{} → {}", bili_channel_name, channel_name),
                        )
                        .await?;
                    }
                }
                // If area_v2 changed, update Bilibili live area
                if bili_area_id != area_v2 {
                    update_area(bili_area_id, area_v2).await?;
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    bili_change_live_title(&cfg, &cfg_title).await?;
                }
                // If auto_cover is enabled, update Bilibili live cover
                if cfg.auto_cover && (bili_title != cfg_title || bili_area_id != area_v2) {
                    let cover_path =
                        get_thumbnail(platform, &channel_id, cfg.proxy.clone()).await?;
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    if let Err(e) = bilibili::bili_change_cover(&cfg, &cover_path).await {
                        tracing::error!("B站直播间封面替换失败: {}", e);
                    } else {
                        tracing::info!("B站直播间封面替换成功");
                    }
                }
            }

            // Execute ffmpeg with platform-specific locks
            ffmpeg(
                cfg.bililive.bili_rtmp_url.clone(),
                cfg.bililive.bili_rtmp_key.clone(),
                m3u8_url.clone().unwrap(),
                cfg.proxy.clone(),
                ffmpeg_log_level,
            );
            // avoid ffmpeg exit errorly and the live is still running, restart ffmpeg
            loop {
                tokio::time::sleep(Duration::from_secs(1)).await;
                if area_v2 == 86 {
                    let puuid = get_puuid(&channel_name)?;
                    if puuid != "" {
                        monitor_lol_game(puuid).await?;
                    }
                }
                let (current_is_live, _, _, new_m3u8_url, _) = if yt_is_live {
                    yt_live
                        .get_status()
                        .await
                        .unwrap_or((false, None, None, None, None))
                } else {
                    tw_live
                        .get_status()
                        .await
                        .unwrap_or((false, None, None, None, None))
                };
                let (bili_is_live, _, _) = get_bili_live_status(cfg.bililive.room).await?;
                if !current_is_live || !bili_is_live {
                    break;
                }
                // let (is_live, _, _) = get_bili_live_status(cfg.bililive.room).await?;
                // if !is_live {
                //     bili_start_live(&cfg).await?;
                // }
                ffmpeg(
                    cfg.bililive.bili_rtmp_url.clone(),
                    cfg.bililive.bili_rtmp_key.clone(),
                    new_m3u8_url.clone().unwrap(),
                    cfg.proxy.clone(),
                    ffmpeg_log_level,
                );
            }

            tracing::info!("{} 直播结束", channel_name);
            if cfg.bililive.enable_danmaku_command {
                if !is_danmaku_running() {
                    thread::spawn(move || run_danmaku());
                }
                send_danmaku(
                    &cfg,
                    &format!("{} 直播结束，可使用弹幕指令进行换台", channel_name),
                )
                .await?;
            } else {
                send_danmaku(&cfg, &format!("{} 直播结束", channel_name)).await?;
            }
        } else {
            // 计划直播(预告窗)
            if scheduled_start.is_some() {
                if yt_title.is_some() {
                    let current_message = box_message(
                        &cfg.youtube.channel_name,
                        Some(scheduled_start.unwrap()),
                        Some(&yt_title.unwrap()),
                        &cfg.twitch.channel_name,
                    );

                    let mut last = LAST_MESSAGE.lock().unwrap();
                    if *last != current_message {
                        // Only update if message content changed significantly
                        let time_diff = if let Some(last_time) = extract_time(&last) {
                            if let Some(current_time) = extract_time(&current_message) {
                                (current_time - last_time).num_minutes().abs()
                            } else {
                                i64::MAX
                            }
                        } else {
                            i64::MAX
                        };

                        // Only update if time difference is more than 5 minutes or other content changed
                        if time_diff > 5 || remove_time(&last) != remove_time(&current_message) {
                            tracing::info!("{}", current_message);
                            *last = current_message;
                        }
                    }
                } else {
                    let current_message = box_message(
                        &cfg.youtube.channel_name,
                        None,
                        None,
                        &cfg.twitch.channel_name,
                    );

                    let mut last = LAST_MESSAGE.lock().unwrap();
                    if *last != current_message {
                        // Only update if message content changed significantly
                        let time_diff = if let Some(last_time) = extract_time(&last) {
                            if let Some(current_time) = extract_time(&current_message) {
                                (current_time - last_time).num_minutes().abs()
                            } else {
                                i64::MAX
                            }
                        } else {
                            i64::MAX
                        };

                        // Only update if time difference is more than 5 minutes or other content changed
                        if time_diff > 5 || remove_time(&last) != remove_time(&current_message) {
                            tracing::info!("{}", current_message);
                            *last = current_message;
                        }
                    }
                }
            } else {
                if !NO_LIVE.load(Ordering::SeqCst) {
                    let current_message = box_message(
                        &cfg.youtube.channel_name,
                        None,
                        None, // No title when not streaming
                        &cfg.twitch.channel_name,
                    );
                    tracing::info!("{}", current_message);
                    NO_LIVE.store(true, Ordering::SeqCst);
                }
            }
            if cfg.bililive.enable_danmaku_command && !is_danmaku_running() {
                thread::spawn(move || run_danmaku());
            }
            tokio::time::sleep(Duration::from_secs(cfg.interval)).await;
        }
    }
}

fn box_message(
    yt_channel: &str,
    scheduled_time: Option<DateTime<Local>>,
    title: Option<&str>,
    tw_channel: &str,
) -> String {
    // Initialize variables first
    let (yt_line, width) = if scheduled_time.is_some() {
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

    let tw_line = format!("TW: {} 未直播", tw_channel);
    let padding = width - 2 - tw_line.width();
    message.push_str(&format!(
        "│ {}{} │\n\
         └{:─<width$}┘\x1b[0m",
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
            let (is_live, topic, title, _, start_time) = get_youtube_status(channel_id).await?;
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
            let (is_live, game_name, title) = get_twitch_status(channel_id).await?;
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

            let (is_live, topic, title, _, start_time) = get_youtube_status(&channel_id).await?;
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
            let (is_live, game_name, title) = get_twitch_status(&channel_id).await?;
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
    bili_change_live_title(&cfg, new_title).await?;
    println!("直播标题改变成功");
    Ok(())
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
                        if let Ok(invalid_words) = std::fs::read_to_string("invalid_words.txt") {
                            if let Some(word) =
                                invalid_words.lines().find(|word| ids.contains(word))
                            {
                                INVALID_ID_DETECTED.store(true, Ordering::SeqCst);
                                let is_live =
                                    get_bili_live_status(cfg.bililive.room).await.unwrap().0;
                                if is_live {
                                    tracing::error!("检测到非法词汇:{}，停止直播", word);
                                    bili_stop_live(&cfg).await.unwrap();
                                    let mut cmd = StdCommand::new("pkill");
                                    cmd.arg("ffmpeg");
                                    cmd.spawn().unwrap();
                                    send_danmaku(&cfg, "检测到玩家ID存在违🈲词汇，停止直播")
                                        .await
                                        .unwrap();
                                    if cfg.bililive.enable_danmaku_command && !is_danmaku_running()
                                    {
                                        thread::spawn(move || run_danmaku());
                                        thread::sleep(Duration::from_secs(2));
                                        send_danmaku(&cfg, "可使用弹幕指令进行换台").await.unwrap();
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
            });

            if !ffmpeg::is_ffmpeg_running() {
                return;
            }
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
    for (room_name, room_id) in cfg.anti_collision {
        match get_bili_live_status(room_id).await {
            Ok((true, title, _)) => {
                let contains_collision = title.contains(target_name)
                    || aliases.iter().any(|alias| title.contains(alias));

                if contains_collision {
                    return Ok(Some((room_name.clone(), room_id, target_name.to_string())));
                }
            }
            Err(e) => tracing::error!("获取防撞直播间 {} 状态失败: {}", room_id, e),
            _ => (),
        }
    }
    Ok(None)
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
        let current = (
            yt_collision.as_ref().unwrap().0.clone(),
            yt_collision.as_ref().unwrap().1,
            "双平台".to_string(),
        );

        if last_collision.as_ref() != Some(&current) {
            tracing::warn!("YouTube和Twitch均检测到撞车，跳过本次转播");
            // send_danmaku(&cfg, "🚨YT和TW双平台撞车").await?;
            // tokio::time::sleep(Duration::from_secs(2)).await;
            send_danmaku(
                &cfg,
                &format!(
                    "{}({})正在转{}",
                    yt_collision.as_ref().unwrap().0,
                    yt_collision.as_ref().unwrap().1,
                    yt_collision.as_ref().unwrap().2,
                ),
            )
            .await?;
            if yt_collision.as_ref().unwrap().0 != tw_collision.as_ref().unwrap().0 {
                tokio::time::sleep(Duration::from_secs(2)).await;
                send_danmaku(
                    &cfg,
                    &format!(
                        "{}({})正在转{}",
                        tw_collision.as_ref().unwrap().0,
                        tw_collision.as_ref().unwrap().1,
                        tw_collision.as_ref().unwrap().2,
                    ),
                )
                .await?;
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
            if cfg.bililive.enable_danmaku_command && !is_danmaku_running() {
                thread::spawn(move || run_danmaku());
            }
            if cfg.bililive.enable_danmaku_command {
                tokio::time::sleep(Duration::from_secs(2)).await;
                send_danmaku(&cfg, "撞车：可使用弹幕指令进行换台").await?;
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

        if !other_live && last_collision.as_ref() != Some(&collision) {
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
            send_danmaku(
                &cfg,
                &format!("{}({})正在转{}", collision.0, collision.1, collision.2,),
            )
            .await?;
            tokio::time::sleep(Duration::from_secs(2)).await;
            if cfg.bililive.enable_danmaku_command && !is_danmaku_running() {
                thread::spawn(move || run_danmaku());
            }
            if cfg.bililive.enable_danmaku_command {
                send_danmaku(&cfg, "撞车：可使用弹幕指令进行换台").await?;
            }
            tokio::time::sleep(Duration::from_secs(30)).await;
            *last_collision = Some(collision);
            Ok(CollisionResult::Continue)
        } else {
            Ok(CollisionResult::Proceed)
        }
    } else {
        Ok(CollisionResult::Proceed)
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let matches = Command::new("bilistream")
        .version("0.2.5")
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
        .get_matches();

    let ffmpeg_log_level = matches
        .get_one::<String>("ffmpeg-log-level")
        .map(String::as_str)
        .unwrap_or("error");

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
            bilibili::send_danmaku(&cfg, message).await?;
            println!("弹幕发送成功");
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
            // Default behavior: run bilistream with the provided config
            run_bilistream(ffmpeg_log_level).await?;
        }
    }
    Ok(())
}
