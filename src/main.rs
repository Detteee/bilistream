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
use tracing_subscriber::fmt;
fn init_logger() {
    tracing_subscriber::fmt()
        .with_timer(fmt::time::ChronoLocal::new("%H:%M:%S".to_string()))
        .with_span_events(fmt::format::FmtSpan::NONE)
        .init();
}
async fn run_bilistream(
    config_path: &str,
    ffmpeg_log_level: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    // Initialize the logger with timestamp format : 2024-11-21 12:00:00
    init_logger();
    // tracing::info!("bilistream 正在运行");

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
            check_cookies().await?;
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
                    get_live_topic(platform, Some(&cfg.youtube.channel_id)).await
                {
                    topic
                } else {
                    get_live_title(platform, Some(&cfg.youtube.channel_id)).await?
                };
                cfg.bililive.area_v2 = check_area_id_with_title(&live_topic, cfg.bililive.area_v2);
            } else {
                let live_title = get_live_title(platform, Some(&cfg.twitch.channel_id)).await?;
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
            } else {
                // If configuration changed, stop Bilibili live
                if cfg.bililive.area_v2 != old_cfg.bililive.area_v2 {
                    tracing::info!("分区配置改变, 停止Bilibili直播");
                    bili_stop_live(&old_cfg).await?;
                    old_cfg.bililive.area_v2 = cfg.bililive.area_v2.clone();
                    log_once = false;
                    continue;
                }
                if cfg.bililive.title != old_cfg.bililive.title {
                    bili_change_live_title(&cfg).await?;
                    tracing::info!("B站直播标题变更为 {}", cfg.bililive.title);
                    old_cfg.bililive.title = cfg.bililive.title.clone();
                }
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
            if cfg.bililive.title != old_cfg.bililive.title {
                log_once_2 = false;
                old_cfg = cfg.clone();
            }
            if scheduled_start.is_some() {
                if log_once_2 == false {
                    let live_title =
                        get_live_title(platform, Some(&cfg.youtube.channel_id)).await?;
                    if live_title != "" && live_title != "空" {
                        tracing::info!(
                            "{}未直播，计划于 {} 开始, 标题：{}",
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

            tokio::time::sleep(Duration::from_secs(cfg.interval)).await;
        }
    }
}

async fn get_live_topic(
    platform: &str,
    channel_id: Option<&str>,
) -> Result<String, Box<dyn std::error::Error>> {
    match platform {
        "YT" => {
            let config_path = Path::new("YT/config.yaml");
            let client = reqwest::Client::new();
            let cfg = load_config(Path::new(config_path), Path::new("cookies.json"))?;
            let channel_id = if let Some(id) = channel_id {
                id
            } else {
                &cfg.youtube.channel_id
            };
            let url = format!(
                "https://holodex.net/api/v2/users/live?channels={}",
                channel_id
            );
            let response = client
                .get(&url)
                .header("X-APIKEY", cfg.holodex_api_key.clone().unwrap())
                .send()
                .await?;

            let videos: Vec<serde_json::Value> = response.json().await?;
            if let Some(video) = videos.last() {
                if let Some(topic_id) = video.get("topic_id") {
                    tracing::info!("YouTube直播分区: {}", topic_id.to_string());
                    return Ok(topic_id.to_string());
                } else {
                    tracing::info!("当前YT直播没有分区");
                    Err("当前YT直播没有分区".into())
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
    platform: &str,
    channel_id: Option<&str>,
) -> Result<(), Box<dyn std::error::Error>> {
    match platform {
        "bilibili" => {
            let cfg = load_config(Path::new("YT/config.yaml"), Path::new("cookies.json"))?;
            let is_live = get_bili_live_status(cfg.bililive.room).await?;
            println!("B站直播状态: {}", if is_live { "直播中" } else { "未直播" });
        }
        "YT" => {
            let cfg = load_config(Path::new("YT/config.yaml"), Path::new("cookies.json"))?;
            let channel_id = if let Some(id) = channel_id {
                id
            } else {
                &cfg.youtube.channel_id
            };
            let client = reqwest::Client::new();
            let url = format!(
                "https://holodex.net/api/v2/users/live?channels={}",
                channel_id
            );
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
            let cfg = load_config(Path::new("TW/config.yaml"), Path::new("cookies.json"))?;
            let channel_id = if let Some(id) = channel_id {
                id
            } else {
                &cfg.twitch.channel_id
            };
            let retry_policy = ExponentialBackoff::builder().build_with_max_retries(5);
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

async fn get_live_title(
    platform: &str,
    channel_id: Option<&str>,
) -> Result<String, Box<dyn std::error::Error>> {
    match platform {
        "YT" => {
            let config = load_config(Path::new("YT/config.yaml"), Path::new("cookies.json"))?;
            let channel_id = if let Some(id) = channel_id {
                id
            } else {
                &config.youtube.channel_id
            };
            let youtube = Youtube::new(
                &config.youtube.channel_name,
                channel_id,
                config.proxy.clone(),
            );
            let title_str = youtube.get_title().await?;
            if title_str != "" {
                // title end with date time like 2024-11-21 01:59 remove it
                let title = title_str
                    .split(" 202")
                    .next()
                    .unwrap_or(&title_str)
                    .to_string();
                // tracing::info!("YouTube直播标题: {}", title);
                Ok(title)
            } else {
                // tracing::info!("YouTube直播标题: 空");
                Ok("空".to_string())
            }
        }
        "TW" => {
            let config = load_config(Path::new("TW/config.yaml"), Path::new("cookies.json"))?;
            let channel_id = if let Some(id) = channel_id {
                id
            } else {
                &config.twitch.channel_id
            };
            let twitch = Twitch::new(
                channel_id,
                config.twitch.oauth_token.clone(),
                ClientBuilder::new(reqwest::Client::new()).build(),
                config.twitch.proxy_region.clone(),
            );
            let title = twitch.get_title().await?;
            if title != "" {
                // println!("Twitch直播标题: {}", title);
                tracing::info!("Twitch直播标题: {}", title);
            }
            Ok(title)
        }
        _ => {
            tracing::info!("不支持的平台: {}", platform);
            Err(format!("不支持的平台: {}", platform).into())
        }
    }
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

async fn check_cookies() -> Result<(), Box<dyn std::error::Error>> {
    // Retrieve live information
    // Check for the existence of cookies.json
    if !Path::new("cookies.json").exists() {
        tracing::info!("cookies.json 不存在，请登录");
        let mut command = StdCommand::new("./login-biliup");
        command.arg("login");
        command.spawn()?.wait()?;
    } else {
        // Check if cookies.json is older than 48 hours
        if Path::new("cookies.json")
            .metadata()?
            .modified()?
            .elapsed()?
            .as_secs()
            > 3600 * 24 * 3
        {
            tracing::info!("cookies.json 已超过3天，正在刷新");
            let mut command = StdCommand::new("./login-biliup");
            command.arg("renew");
            command.spawn()?.wait()?;
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
                .arg(Arg::new("channel_id").required(false).help("检查的频道ID")),
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
                .arg(Arg::new("channel_id").required(false).help("获取的频道ID")),
        )
        .subcommand(
            Command::new("get-live-topic")
                .about("获取直播topic_id")
                .arg(
                    Arg::new("platform")
                        .required(true)
                        .help("获取的平台 (仅支持YT)"),
                )
                .arg(Arg::new("channel_id").required(false).help("获取的频道ID")),
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
            let channel_id = sub_m.get_one::<String>("channel_id");
            if channel_id.is_none() {
                get_live_status(platform, None).await?;
            } else {
                get_live_status(platform, Some(channel_id.unwrap())).await?;
            }
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
            let channel_id = sub_m.get_one::<String>("channel_id");
            if channel_id.is_none() {
                // tracing::info!("直播标题: {}", get_live_title(platform, None).await?);
                println!("直播标题: {}", get_live_title(platform, None).await?);
            } else {
                // tracing::info!("直播标题: {}", get_live_title(platform, Some(channel_id.unwrap())).await?);
                println!(
                    "直播标题: {}",
                    get_live_title(platform, Some(channel_id.unwrap())).await?
                );
            }
        }
        Some(("get-live-topic", sub_m)) => {
            let platform = sub_m.get_one::<String>("platform").unwrap();
            let channel_id = sub_m.get_one::<String>("channel_id");
            if channel_id.is_none() {
                println!("YouTube直播分区: {}", get_live_topic(platform, None).await?);
            } else {
                println!(
                    "YouTube直播分区: {}",
                    get_live_topic(platform, Some(channel_id.unwrap())).await?
                );
            }
        }
        Some(("login", _)) => {
            let mut command = StdCommand::new("./login-biliup");
            command.arg("login");
            command.spawn()?.wait()?;
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
