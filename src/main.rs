use bilistream::config::load_config;
use bilistream::plugins::{
    bili_change_live_title, bili_start_live, bili_stop_live, check_area_id_with_title, ffmpeg,
    get_area_name, get_bili_live_status, run_danmaku, select_live, Live, Twitch, Youtube,
};
use chrono::{DateTime, Local};
use clap::{Arg, Command};
use proctitle::set_title;
use reqwest_middleware::ClientBuilder;
use reqwest_retry::{policies::ExponentialBackoff, RetryTransientMiddleware};
use std::process::Command as StdCommand;
use std::{path::Path, thread, time::Duration};
use tracing_subscriber;

async fn run_bilistream(
    config_path: &str,
    ffmpeg_log_level: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    // Initialize the logger
    tracing_subscriber::fmt::init();
    // tracing::info!("bilistream 正在运行");
    if !Path::new("cookies.json").exists() {
        tracing::info!("cookies.json 不存在，请登录");
        let mut command = StdCommand::new("./login-biliup");
        command.arg("login");
        command.spawn()?.wait()?;
    } else {
        if Path::new("cookies.json")
            .metadata()?
            .modified()?
            .elapsed()?
            .as_secs()
            > 3600 * 48
        {
            tracing::info!("cookies.json 存在时间超过48小时，刷新cookies");
            let mut command = StdCommand::new("./login-biliup");
            command.arg("renew");
            command.spawn()?.wait()?;
        }
    }

    let mut cfg = load_config(Path::new(config_path), Path::new("cookies.json"))?;

    let mut old_cfg = cfg.clone();
    let mut log_once = false;
    let mut log_once_2 = false;
    let mut no_live = false;
    let platform = if &cfg.platform == "Youtube" {
        "YT"
    } else if &cfg.platform == "Twitch" {
        "TW"
    } else {
        return Err("不支持的平台".into());
    };
    loop {
        // Check if any ffmpeg or danmaku is running
        if ffmpeg::is_any_ffmpeg_running() {
            if log_once == false {
                tracing::info!("一个ffmpeg实例已经在运行。跳过检测循环。");
                log_once = true;
            }
            tokio::time::sleep(Duration::from_secs(cfg.interval)).await;
            continue;
        }
        log_once = false;
        cfg = load_config(Path::new(config_path), Path::new("cookies.json"))?;

        let live_info = select_live(cfg.clone()).await?;
        let (is_live, m3u8_url, scheduled_start) =
            live_info.get_status().await.unwrap_or((false, None, None));

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
            if platform == "YT" {
                let live_topic = if let Ok(topic) =
                    get_live_topic(config_path, platform, &cfg.youtube.channel_id).await
                {
                    topic
                } else {
                    get_live_title(config_path, platform, &cfg.youtube.channel_id).await?
                };
                cfg.bililive.area_v2 = check_area_id_with_title(&live_topic, cfg.bililive.area_v2);
            } else {
                let live_title =
                    get_live_title(config_path, platform, &cfg.twitch.channel_id).await?;
                cfg.bililive.area_v2 = check_area_id_with_title(&live_title, cfg.bililive.area_v2);
            }
            if !get_bili_live_status(cfg.bililive.room).await? {
                tracing::info!("B站未直播");
                let area_name = get_area_name(cfg.bililive.area_v2);
                bili_start_live(&cfg).await?;
                tracing::info!(
                    "B站已开播, 标题为 {},分区为 {} （ID: {}）",
                    cfg.bililive.title,
                    area_name.unwrap(),
                    cfg.bililive.area_v2
                );
                old_cfg.bililive.area_v2 = cfg.bililive.area_v2.clone();
                bili_change_live_title(&cfg).await?;
                tracing::info!("标题为 {}", cfg.bililive.title);
            } else {
                // If configuration changed, stop Bilibili live
                if cfg.bililive.area_v2 != old_cfg.bililive.area_v2 {
                    tracing::info!("分区配置改变, 停止Bilibili直播");
                    bili_stop_live(&old_cfg).await?;
                    old_cfg.bililive.area_v2 = cfg.bililive.area_v2.clone();
                    log_once = false;
                    continue;
                }
                bili_change_live_title(&cfg).await?;
                tracing::info!("B站直播标题变更为 {}", cfg.bililive.title);
            }

            // Execute ffmpeg with platform-specific locks
            ffmpeg(
                cfg.bililive.bili_rtmp_url.clone(),
                cfg.bililive.bili_rtmp_key.clone(),
                m3u8_url.clone().unwrap(),
                cfg.proxy.clone(),
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
                    cfg.proxy.clone(),
                    ffmpeg_log_level,
                    platform,
                );
            }

            tracing::info!(
                "{} 直播结束",
                match platform {
                    "TW" => &cfg.twitch.channel_name,
                    "YT" => &cfg.youtube.channel_name,
                    _ => "未知平台",
                }
            );
            if cfg.bililive.enable_danmaku_command {
                thread::spawn(move || run_danmaku(platform));
            }
        } else {
            // 计划直播(预告窗)
            if scheduled_start.is_some() {
                if log_once_2 == false {
                    let live_title =
                        get_live_title(config_path, platform, &cfg.youtube.channel_id).await?;
                    if live_title != "" {
                        tracing::info!(
                            "{}未直播，计划于 {} 开始\n标题：{}",
                            cfg.youtube.channel_name,
                            scheduled_start.unwrap().format("%Y-%m-%d %H:%M:%S"), // Format the start time
                            live_title
                        );
                    } else {
                        tracing::info!(
                            "{}未直播，计划于 {} 开始",
                            cfg.youtube.channel_name,
                            scheduled_start.unwrap().format("%Y-%m-%d %H:%M:%S")
                        );
                    }
                    log_once_2 = true;
                }
            } else {
                if no_live == false {
                    tracing::info!(
                        "{} 未直播",
                        match platform {
                            "TW" => &cfg.twitch.channel_name,
                            "YT" => &cfg.youtube.channel_name,
                            _ => "未知平台",
                        }
                    );
                    no_live = true;
                };
            }
            if cfg.bililive.enable_danmaku_command {
                thread::spawn(move || run_danmaku(platform));
            }
            if cfg.bililive.title != old_cfg.bililive.title {
                log_once_2 = false;
            }
            old_cfg = cfg.clone();
            tokio::time::sleep(Duration::from_secs(cfg.interval)).await;
        }
    }
}

async fn get_live_topic(
    config_path: &str,
    platform: &str,
    channel_id: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    match platform {
        "YT" => {
            let client = reqwest::Client::new();
            let url = format!(
                "https://holodex.net/api/v2/users/live?channels={}",
                channel_id
            );
            let cfg = load_config(Path::new(config_path), Path::new("cookies.json"))?;
            let response = client
                .get(&url)
                .header("X-APIKEY", cfg.holodex_api_key.clone().unwrap())
                .send()
                .await?;

            let videos: Vec<serde_json::Value> = response.json().await?;
            if let Some(video) = videos.last() {
                if let Some(topic_id) = video.get("topic_id") {
                    // tracing::info!("YouTube live topic_id: {:?}", &topic_id);
                    println!("YouTube live topic_id: {:?}", &topic_id);
                    return Ok(topic_id.to_string());
                } else {
                    tracing::info!("当前YT直播没有topic_id");
                    Err("当前YT直播没有topic_id".into())
                }
            } else {
                tracing::info!("当前频道没有直播");
                Err("当前频道没有直播".into())
            }
        }
        _ => {
            tracing::info!("不支持的平台: {}", platform);
            Err(format!("不支持的平台: {}", platform).into())
        }
    }
}

async fn get_live_status(
    config_path: &str,
    platform: &str,
    channel_id: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let cfg = load_config(Path::new(config_path), Path::new("cookies.json"))?;
    match platform {
        "bilibili" => {
            let room_id: i32 = channel_id.parse()?;
            let is_live = get_bili_live_status(room_id).await?;
            println!("B站直播状态: {}", if is_live { "直播中" } else { "未直播" });
        }
        "YT" => {
            let client = reqwest::Client::new();
            let url = format!(
                "https://holodex.net/api/v2/users/live?channels={}",
                channel_id
            );
            let cfg = load_config(Path::new("YT/config.yaml"), Path::new("cookies.json"))?;
            let response = client
                .get(&url)
                .header("X-APIKEY", cfg.holodex_api_key.clone().unwrap())
                .send()
                .await?;
            if response.status().is_success() {
                let videos: Vec<serde_json::Value> = response.json().await?;
                if let Some(video) = videos.last() {
                    let status = video.get("status").unwrap();
                    if status == "upcoming" {
                        let start_time_str = video
                            .get("start_scheduled")
                            .and_then(|v| v.as_str())
                            .ok_or("start_scheduled 不存在")?;
                        // 将时间字符串转换为DateTime<Local>
                        let start_time =
                            DateTime::parse_from_rfc3339(&start_time_str)?.with_timezone(&Local);
                        println!("计划开始时间: {}", start_time);
                    } else if status == "live" {
                        println!("YouTube直播状态: 直播中");
                    } else {
                        println!("YouTube直播状态: 未直播");
                    }
                } else {
                    println!("YouTube直播状态: 未直播");
                }
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

            let twitch = Twitch::new(
                channel_id,
                cfg.twitch.oauth_token.clone(),
                client,
                cfg.twitch.proxy_region.clone(),
            );

            let (is_live, _, _) = twitch.get_status().await?;
            println!(
                "Twitch直播状态: {}",
                if is_live { "直播中" } else { "未直播" }
            );
        }
        _ => {
            println!("不支持的平台: {}", platform);
        }
    }
    Ok(())
}

async fn start_live(config_path: &str) -> Result<(), Box<dyn std::error::Error>> {
    let cfg = load_config(Path::new(config_path), Path::new("cookies.json"))?;
    bili_start_live(&cfg).await?;
    println!("直播开始成功");
    Ok(())
}

async fn stop_live(config_path: &str) -> Result<(), Box<dyn std::error::Error>> {
    let cfg = load_config(Path::new(config_path), Path::new("cookies.json"))?;
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
        return Err(format!("配置文件不存在: {}", config_path).into());
    }
    let mut cfg = load_config(config_file, Path::new("cookies.json"))?;
    cfg.bililive.title = new_title.to_string();
    bili_change_live_title(&cfg).await?;
    println!("直播标题改变成功");
    Ok(())
}

async fn get_live_title(
    config_path: &str,
    platform: &str,
    channel_id: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    let cfg = load_config(Path::new(config_path), Path::new("cookies.json"))?;

    match platform {
        "YT" => {
            let youtube = Youtube::new(channel_id, channel_id, cfg.proxy.clone());
            let title = youtube.get_title().await?;
            if title != "" {
                tracing::info!("YouTube直播标题: {}", title);
            }
            Ok(title)
        }
        "TW" => {
            let twitch = Twitch::new(
                channel_id,
                cfg.twitch.oauth_token.clone(),
                ClientBuilder::new(reqwest::Client::new()).build(),
                cfg.twitch.proxy_region.clone(),
            );
            let title = twitch.get_title().await?;
            tracing::info!("Twitch直播标题: {}", title);
            Ok(title)
        }
        _ => {
            tracing::info!("不支持的平台: {}", platform);
            Err(format!("不支持的平台: {}", platform).into())
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let matches = Command::new("bilistream")
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
                .arg(
                    Arg::new("platform")
                        .required(true)
                        .help("检查的平台 (YT, TW, bilibili)"),
                )
                .arg(Arg::new("channel_id").required(true).help("检查的频道ID")),
        )
        .subcommand(Command::new("start-live").about("开始直播"))
        .subcommand(Command::new("stop-live").about("停止直播"))
        .subcommand(
            Command::new("change-live-title")
                .about("改变直播标题")
                .arg(Arg::new("title").required(true).help("新直播标题")),
        )
        .subcommand(
            Command::new("get-live-title")
                .about("获取直播标题")
                .arg(
                    Arg::new("platform")
                        .required(true)
                        .help("获取的平台 (YT, TW)"),
                )
                .arg(Arg::new("channel_id").required(true).help("获取的频道ID")),
        )
        .subcommand(
            Command::new("get-live-topic")
                .about("获取直播topic_id")
                .arg(
                    Arg::new("platform")
                        .required(true)
                        .help("获取的平台 (仅支持YT)"),
                )
                .arg(Arg::new("channel_id").required(true).help("获取的频道ID")),
        )
        .subcommand(Command::new("login").about("登录"))
        .get_matches();

    let config_path = matches
        .get_one::<String>("config")
        .map(|s| s.as_str())
        .unwrap_or("./TW/config.yaml");
    // 默认配置文件路径为./YT/config.yaml，防止错误

    let ffmpeg_log_level = matches
        .get_one::<String>("ffmpeg-log-level")
        .map(String::as_str)
        .unwrap_or("error");

    match matches.subcommand() {
        Some(("get-live-status", sub_m)) => {
            let platform = sub_m.get_one::<String>("platform").unwrap();
            let channel_id = sub_m.get_one::<String>("channel_id").unwrap();
            get_live_status(config_path, platform, channel_id).await?;
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
        Some(("get-live-topic", sub_m)) => {
            let platform = sub_m.get_one::<String>("platform").unwrap();
            let channel_id = sub_m.get_one::<String>("channel_id").unwrap();
            get_live_topic(config_path, platform, channel_id).await?;
        }
        Some(("login", _)) => {}
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
