use bilistream::config::load_config;
use bilistream::plugins::{
    bili_change_live_title, bili_start_live, bili_stop_live, ffmpeg, get_bili_live_status,
    get_youtube_live_status, select_live, Live, Twitch,
};
use clap::{Arg, Command};
use reqwest_middleware::ClientBuilder;
use reqwest_retry::policies::ExponentialBackoff;
use reqwest_retry::RetryTransientMiddleware;
use std::path::Path;
use std::time::Duration;
use tracing_subscriber;

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
        .get_matches();

    let config_path = matches
        .get_one::<String>("config")
        .map(|s| s.as_str())
        .unwrap_or("./YT/config.yaml");
    // default config path is ./YT/config.yaml to prevent error

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
        _ => {
            // Default behavior: run bilistream with the provided config
            run_bilistream(config_path).await?;
        }
    }
    Ok(())
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
            let twitch = Twitch::new(channel_id, cfg.twitch.oauth_token.clone(), client);

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

async fn run_bilistream(config_path: &str) -> Result<(), Box<dyn std::error::Error>> {
    // Initialize the logger
    tracing_subscriber::fmt::init();

    let mut cfg = load_config(Path::new(config_path))?;
    let live_type = select_live(cfg.clone()).await?;

    loop {
        // if ffmpeg.lock exists skip the loop
        if std::path::Path::new("./ffmpeg.lock").exists() {
            tracing::info!("ffmpeg.lock exists, skipping the loop");
            tokio::time::sleep(Duration::from_secs(cfg.interval)).await;
            continue;
        }
        let old_cfg = cfg.clone();
        cfg = load_config(Path::new(config_path))?;

        // If configuration changed, stop Bilibili live
        if cfg.bililive.area_v2 != old_cfg.bililive.area_v2 {
            tracing::info!("Configuration changed, stopping Bilibili live");
            bili_stop_live(&old_cfg).await?;
        }
        if cfg.bililive.title != old_cfg.bililive.title {
            tracing::info!("Configuration changed, updating Bilibili live title");
            bili_change_live_title(&cfg).await?;
        }

        let (is_live, m3u8_url, scheduled_start) =
            live_type.get_status().await.unwrap_or((false, None, None));

        if is_live {
            tracing::info!("{} 直播中", live_type.channel_name());

            if get_bili_live_status(cfg.bililive.room).await? {
                tracing::info!("B站直播中");
                bili_change_live_title(&cfg).await?;

                ffmpeg(
                    cfg.bililive.bili_rtmp_url.clone(),
                    cfg.bililive.bili_rtmp_key.clone(),
                    m3u8_url.clone().unwrap_or_default(),
                    cfg.ffmpeg_proxy.clone(),
                );
                let current_is_live = is_live;
                while current_is_live {
                    let (current_is_live, new_m3u8_url, _) =
                        live_type.get_status().await.unwrap_or((false, None, None));

                    if current_is_live {
                        ffmpeg(
                            cfg.bililive.bili_rtmp_url.clone(),
                            cfg.bililive.bili_rtmp_key.clone(),
                            new_m3u8_url.clone().unwrap_or_default(),
                            cfg.ffmpeg_proxy.clone(),
                        );
                    }
                }
                tracing::info!("{} 直播已结束", live_type.channel_name());
            } else {
                tracing::info!("B站未直播");
                bili_start_live(&cfg).await?;
                tracing::info!("B站已开播");
                bili_change_live_title(&cfg).await?;

                let current_is_live = is_live;
                while current_is_live {
                    let (current_is_live, new_m3u8_url, _) =
                        live_type.get_status().await.unwrap_or((false, None, None));

                    if current_is_live {
                        ffmpeg(
                            cfg.bililive.bili_rtmp_url.clone(),
                            cfg.bililive.bili_rtmp_key.clone(),
                            new_m3u8_url.clone().unwrap_or_default(),
                            cfg.ffmpeg_proxy.clone(),
                        );
                    }
                }
                tracing::info!("{} 直播已结束", live_type.channel_name());
            }
        } else {
            if cfg.bililive.enable_danmaku_command {
                if !std::path::Path::new("./danmaku.lock").exists() {
                    // make a file named danmaku.lock
                    let _ = std::fs::File::create("./danmaku.lock");

                    tracing::info!("执行弹幕命令");
                    let _ = std::process::Command::new("bash")
                        .arg("./danmaku.sh")
                        .output()
                        .expect("Failed to execute danmaku.sh");
                }
            }
            if scheduled_start.is_some() {
                tracing::info!(
                    "{}未直播，计划于 {} 开始",
                    cfg.youtube.channel_name,
                    scheduled_start.unwrap().format("%Y-%m-%d %H:%M:%S") // Format the start time
                );
            } else {
                tracing::info!("{}未直播", live_type.channel_name());
            }
            if get_bili_live_status(cfg.bililive.room.clone()).await? {
                tracing::info!("B站直播中");
                // bili_stop_live(&cfg).await;
                // tracing::info!("B站已关播");
            }
        }
        tokio::time::sleep(Duration::from_secs(cfg.interval)).await;
    }
}

async fn start_live(config_path: &str) -> Result<(), Box<dyn std::error::Error>> {
    let cfg = load_config(Path::new(config_path))?;
    bili_start_live(&cfg).await?;
    println!("Live stream started successfully");
    Ok(())
}

async fn stop_live(config_path: &str) -> Result<(), Box<dyn std::error::Error>> {
    let cfg = load_config(Path::new(config_path))?;
    bili_stop_live(&cfg).await?;
    println!("Live stream stopped successfully");
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
    println!("Live stream title changed successfully");
    Ok(())
}
