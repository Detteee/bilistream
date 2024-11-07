use bilistream::config::load_config;
use bilistream::plugins::{
    bili_change_live_title, bili_start_live, bili_stop_live, ffmpeg, get_bili_live_status,
    get_youtube_live_status, run_danmaku, select_live, Live, Twitch, Youtube,
};
use clap::{Arg, Command};
use proctitle::set_title;
use reqwest_middleware::ClientBuilder;
use reqwest_retry::{policies::ExponentialBackoff, RetryTransientMiddleware};
use std::{path::Path, thread, time::Duration};
use tracing_subscriber;

async fn run_bilistream(
    config_path: &str,
    ffmpeg_log_level: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    // Initialize the logger
    tracing_subscriber::fmt::init();
    // tracing::info!("bilistream 正在运行");

    let mut cfg = load_config(Path::new(config_path))?;

    let mut old_cfg = cfg.clone();
    let mut log_once = false;
    let mut log_once_2 = false;
    let mut start_stream = false;
    let mut no_live = false;
    loop {
        // Check if any ffmpeg or danmaku is running
        if ffmpeg::is_any_ffmpeg_running() {
            if log_once == false {
                tracing::info!("ffmpeg lock exists, skipping the loop.");
                log_once = true;
            }
            tokio::time::sleep(Duration::from_secs(cfg.interval)).await;
            continue;
        }
        log_once = false;
        cfg = load_config(Path::new(config_path))?;

        // If configuration changed, stop Bilibili live
        if cfg.bililive.area_v2 != old_cfg.bililive.area_v2 {
            tracing::info!("配置改变, 停止Bilibili直播");
            bili_stop_live(&old_cfg).await?;
            old_cfg.bililive.area_v2 = cfg.bililive.area_v2.clone();
            continue;
        }

        let live_info = select_live(cfg.clone()).await?;
        let (is_live, m3u8_url, scheduled_start) =
            live_info.get_status().await.unwrap_or((false, None, None));
        let platform = if &cfg.platform == "Youtube" {
            "YT"
        } else {
            "TW"
        };
        if is_live {
            tracing::info!(
                "{} 正在直播",
                match platform {
                    "TW" => &cfg.twitch.channel_name,
                    "YT" => &cfg.youtube.channel_name,
                    _ => "Unknown Platform",
                }
            );
            no_live = false;
            log_once_2 = false;
            if !get_bili_live_status(cfg.bililive.room).await? {
                tracing::info!("B站未直播");
                bili_start_live(&cfg).await?;
                tracing::info!(
                    "B站已开播, 标题为 {},分区为 {}",
                    cfg.bililive.title,
                    cfg.bililive.area_v2
                );
                bili_change_live_title(&cfg).await?;
                tracing::info!("标题为 {}", cfg.bililive.title);
                start_stream = true;
            }
            tracing::info!("B站直播中");

            if !start_stream {
                bili_change_live_title(&cfg).await?;
                tracing::info!("B站直播标题变更为 {}", cfg.bililive.title);
            }

            // Execute ffmpeg with platform-specific locks
            ffmpeg(
                cfg.bililive.bili_rtmp_url.clone(),
                cfg.bililive.bili_rtmp_key.clone(),
                m3u8_url.clone().unwrap(),
                cfg.ffmpeg_proxy.clone(),
                ffmpeg_log_level,
                platform,
            );
            // avoid ffmpeg exit errorly and the live is still running, restart ffmpeg
            loop {
                tokio::time::sleep(Duration::from_secs(5)).await;
                let (current_is_live, new_m3u8_url, _) =
                    live_info.get_status().await.unwrap_or((false, None, None));
                if !current_is_live {
                    break;
                }
                ffmpeg(
                    cfg.bililive.bili_rtmp_url.clone(),
                    cfg.bililive.bili_rtmp_key.clone(),
                    new_m3u8_url.clone().unwrap(),
                    cfg.ffmpeg_proxy.clone(),
                    ffmpeg_log_level,
                    platform,
                );
            }

            tracing::info!(
                "{} 直播结束",
                match platform {
                    "TW" => &cfg.twitch.channel_name,
                    "YT" => &cfg.youtube.channel_name,
                    _ => "Unknown Platform",
                }
            );
            if cfg.bililive.enable_danmaku_command {
                thread::spawn(move || run_danmaku(platform));
            }
        } else {
            // 计划直播(预告窗)
            if scheduled_start.is_some() {
                if log_once_2 == false {
                    tracing::info!(
                        "{}未直播，计划于 {} 开始",
                        cfg.youtube.channel_name,
                        scheduled_start.unwrap().format("%Y-%m-%d %H:%M:%S") // Format the start time
                    );
                    log_once_2 = true;
                }
            } else {
                if no_live == false {
                    tracing::info!(
                        "{} 未直播",
                        match platform {
                            "TW" => &cfg.twitch.channel_name,
                            "YT" => &cfg.youtube.channel_name,
                            _ => "Unknown Platform",
                        }
                    );
                    no_live = true;
                };
            }
            if cfg.bililive.enable_danmaku_command {
                thread::spawn(move || run_danmaku(platform));
            }
            old_cfg = cfg.clone();
            tokio::time::sleep(Duration::from_secs(cfg.interval)).await;
        }
    }
}
async fn get_live_status(
    platform: &str,
    channel_id: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    match platform {
        "bilibili" => {
            let room_id: i32 = channel_id.parse()?;
            let is_live = get_bili_live_status(room_id).await?;
            println!(
                "Bilibili live status: {}",
                if is_live { "Live" } else { "Not Live" }
            );
        }
        "YT" => {
            let (is_live, _, scheduled_time) = get_youtube_live_status(channel_id).await?;
            println!(
                "YouTube live status: {}",
                if is_live { "Live" } else { "Not Live" }
            );
            if let Some(time) = scheduled_time {
                println!("Scheduled start time: {}", time);
            }
        }
        "TW" => {
            let retry_policy = ExponentialBackoff::builder().build_with_max_retries(4294967295);
            let raw_client = reqwest::Client::builder()
                .cookie_store(true)
                .timeout(Duration::new(30, 0))
                .build()?;
            let client = ClientBuilder::new(raw_client.clone())
                .with(RetryTransientMiddleware::new_with_policy(retry_policy))
                .build();

            let cfg = load_config(Path::new("./TW/config.yaml"))?;
            let twitch = Twitch::new(
                channel_id,
                cfg.twitch.oauth_token.clone(),
                client,
                cfg.twitch.proxy_region.clone(),
            );

            let (is_live, _, _) = twitch.get_status().await?;
            println!(
                "Twitch live status: {}",
                if is_live { "Live" } else { "Not Live" }
            );
        }
        _ => {
            println!("Unsupported platform: {}", platform);
        }
    }
    Ok(())
}

async fn start_live(config_path: &str) -> Result<(), Box<dyn std::error::Error>> {
    let cfg = load_config(Path::new(config_path))?;
    bili_start_live(&cfg).await?;
    println!("直播开始成功");
    Ok(())
}

async fn stop_live(config_path: &str) -> Result<(), Box<dyn std::error::Error>> {
    let cfg = load_config(Path::new(config_path))?;
    bili_stop_live(&cfg).await?;
    println!("直播停止成功");
    Ok(())
}

async fn change_live_title(
    config_path: &str,
    new_title: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let config_file = Path::new(config_path);
    if !config_file.exists() {
        return Err(format!("Config file not found: {}", config_path).into());
    }
    let mut cfg = load_config(config_file)?;
    cfg.bililive.title = new_title.to_string();
    bili_change_live_title(&cfg).await?;
    println!("直播标题改变成功");
    Ok(())
}

async fn get_live_title(
    config_path: &str,
    platform: &str,
    channel_id: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let cfg = load_config(Path::new(config_path))?;

    match platform {
        "YT" => {
            let youtube = Youtube::new(channel_id, channel_id);
            let title = youtube.get_title().await?;
            println!("YouTube live title: {}", title);
        }
        "TW" => {
            let twitch = Twitch::new(
                channel_id,
                cfg.twitch.oauth_token.clone(),
                ClientBuilder::new(reqwest::Client::new()).build(),
                cfg.twitch.proxy_region.clone(),
            );
            let title = twitch.get_title().await?;
            println!("Twitch live title: {}", title);
        }
        _ => {
            println!("Unsupported platform: {}", platform);
        }
    }

    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let matches = Command::new("bilistream")
        .arg(
            Arg::new("config")
                .short('c')
                .long("config")
                .value_name("FILE")
                .help("Sets a custom config file")
                .global(true),
        )
        .arg(
            Arg::new("ffmpeg-log-level")
                .long("ffmpeg-log-level")
                .value_name("LEVEL")
                .help("Sets ffmpeg log level (error, info, debug)")
                .default_value("error")
                .value_parser(["error", "info", "debug"]),
        )
        .subcommand(
            Command::new("get-live-status")
                .about("Check live status of a channel")
                .arg(
                    Arg::new("platform")
                        .required(true)
                        .help("Platform to check (YT, TW, bilibili)"),
                )
                .arg(
                    Arg::new("channel_id")
                        .required(true)
                        .help("Channel ID to check"),
                ),
        )
        .subcommand(Command::new("start-live").about("Start a live stream"))
        .subcommand(Command::new("stop-live").about("Stop a live stream"))
        .subcommand(
            Command::new("change-live-title")
                .about("Change the title of a live stream")
                .arg(
                    Arg::new("title")
                        .required(true)
                        .help("New title for the live stream"),
                ),
        )
        .subcommand(
            Command::new("get-live-title")
                .about("Get the title of a live stream")
                .arg(
                    Arg::new("platform")
                        .required(true)
                        .help("Platform to check (YT, TW)"),
                )
                .arg(
                    Arg::new("channel_id")
                        .required(true)
                        .help("Channel ID to check"),
                ),
        )
        .get_matches();

    let config_path = matches
        .get_one::<String>("config")
        .map(|s| s.as_str())
        .unwrap_or("./TW/config.yaml");
    // default config path is ./YT/config.yaml to prevent error

    let ffmpeg_log_level = matches
        .get_one::<String>("ffmpeg-log-level")
        .map(String::as_str)
        .unwrap_or("error");

    match matches.subcommand() {
        Some(("get-live-status", sub_m)) => {
            let platform = sub_m.get_one::<String>("platform").unwrap();
            let channel_id = sub_m.get_one::<String>("channel_id").unwrap();
            get_live_status(platform, channel_id).await?;
        }
        Some(("start-live", _)) => {
            start_live(config_path).await?;
        }
        Some(("stop-live", _)) => {
            stop_live(config_path).await?;
        }
        Some(("change-live-title", sub_m)) => {
            let new_title = sub_m.get_one::<String>("title").unwrap();
            change_live_title(config_path, new_title).await?;
        }
        Some(("get-live-title", sub_m)) => {
            let platform = sub_m.get_one::<String>("platform").unwrap();
            let channel_id = sub_m.get_one::<String>("channel_id").unwrap();
            get_live_title(config_path, platform, channel_id).await?;
        }
        _ => {
            let file_name = Path::new(config_path)
                .parent()
                .and_then(|p| p.file_name())
                .and_then(|s| s.to_str())
                .unwrap_or("default");
            let process_name = format!("bilistream-{}", file_name);
            set_title(&process_name);
            // Default behavior: run bilistream with the provided config
            run_bilistream(config_path, ffmpeg_log_level).await?;
        }
    }
    Ok(())
}
