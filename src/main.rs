use bilistream::config::load_config;
use bilistream::plugins::{
    bili_change_live_title, bili_start_live, bili_stop_live, check_area_id_with_title, ffmpeg,
    get_area_name, get_bili_live_status, get_channel_id, get_channel_name, get_twitch_live_status,
    get_twitch_live_title, get_youtube_live_title, run_danmaku, select_live,
};
use chrono::{DateTime, Local};
use clap::{Arg, Command};
use proctitle::set_title;
use regex::Regex;
use reqwest_middleware::ClientBuilder;
use riven::consts::PlatformRoute;
use riven::RiotApi;
use std::process::Command as StdCommand;
use std::{error::Error, fs, io, io::BufRead, path::Path, thread, time::Duration};
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
    let mut old_cfg_title = "".to_string();
    let mut log_once = false;
    let mut no_live = false;
    let mut old_scheduled_start = None;
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
        let (is_live, m3u8_url, title, scheduled_start) = live_info
            .get_status()
            .await
            .unwrap_or((false, None, None, None));
        if is_live {
            tracing::info!(
                "{} 正在 {} 直播, 标题:\n          {}",
                match platform {
                    "TW" => &cfg.twitch.channel_name,
                    "YT" => &cfg.youtube.channel_name,
                    _ => "Unknown Platform",
                },
                cfg.platform,
                title.unwrap()
            );
            no_live = false;
            if platform == "YT" {
                let live_topic = if let Ok(topic) =
                    get_live_topic(platform, Some(&cfg.youtube.channel_id)).await
                {
                    topic
                } else {
                    get_live_title(platform, Some(&cfg.youtube.channel_id)).await?
                };
                cfg.bililive.area_v2 = check_area_id_with_title(&live_topic, cfg.bililive.area_v2);
                if cfg.bililive.area_v2 == 240 && !cfg.youtube.channel_id.contains("Kamito") {
                    cfg.bililive.area_v2 = 0
                };
            } else {
                let live_title = get_live_title(platform, Some(&cfg.twitch.channel_id)).await?;
                cfg.bililive.area_v2 = check_area_id_with_title(&live_title, cfg.bililive.area_v2);
                if cfg.bililive.area_v2 == 240 && !cfg.twitch.channel_id.contains("Kamito") {
                    cfg.bililive.area_v2 = 0
                };
            }
            if cfg.bililive.area_v2 == 0 {
                tracing::info!("标题包含的直播分区不支持,等待10min后重新检测");
                // 等待10min后重新检测
                tokio::time::sleep(Duration::from_secs(600)).await;
                continue;
            }
            let (is_live, title, area_id) = get_bili_live_status(cfg.bililive.room).await?;
            if !is_live {
                tracing::info!("B站未直播");
                let area_name = get_area_name(cfg.bililive.area_v2);
                bili_start_live(&cfg).await?;
                if title != cfg.bililive.title {
                    bili_change_live_title(&cfg).await?;
                }
                tracing::info!(
                    "B站已开播，标题为 {}，分区为 {} （ID: {}）",
                    cfg.bililive.title,
                    area_name.unwrap(),
                    cfg.bililive.area_v2
                );
            } else {
                // If configuration changed, stop Bilibili live
                if cfg.bililive.area_v2 != area_id {
                    let to_area_name = get_area_name(cfg.bililive.area_v2);
                    let area_name = get_area_name(area_id);
                    if area_name.is_some() && to_area_name.is_some() {
                        tracing::info!(
                            "分区改变（{}->{}），请调整分区",
                            area_name.unwrap(),
                            to_area_name.unwrap()
                        );
                    }
                    // bili_stop_live(&cfg).await?;
                    // bili_start_live(&cfg).await?;
                    bili_change_live_title(&cfg).await?;
                    tracing::info!("已更换转播频道，标题：{}", cfg.bililive.title);
                    log_once = false;
                }
                // 如果标题改变，则变更B站直播标题
                if title != cfg.bililive.title {
                    bili_change_live_title(&cfg).await?;
                    tracing::info!("B站直播标题变更 （{}->{}）", title, cfg.bililive.title);
                }
            }

            if cfg.bililive.area_v2 == 86 {
                let puuid = get_puuid_from_file(&cfg.youtube.channel_name)?;
                monitor_lol_game(puuid).await?;
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
                tokio::time::sleep(Duration::from_secs(1)).await;
                if cfg.bililive.area_v2 == 86 {
                    let puuid = get_puuid_from_file(&cfg.youtube.channel_name)?;
                    monitor_lol_game(puuid).await?;
                }
                let (current_is_live, new_m3u8_url, _, _) = live_info
                    .get_status()
                    .await
                    .unwrap_or((false, None, None, None));
                if !current_is_live {
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
                let mut diff = chrono::Duration::seconds(0);
                if old_scheduled_start.is_some() {
                    diff = old_scheduled_start.unwrap() - scheduled_start.unwrap();
                }
                if !old_cfg_title.contains(&cfg.bililive.title) || diff.num_hours() > 2 {
                    let live_title =
                        get_live_title(platform, Some(&cfg.youtube.channel_id)).await?;
                    if live_title != "" && live_title != "空" {
                        tracing::info!(
                            "{} 未直播，计划于 {} 开始，标题：\n          {}",
                            cfg.youtube.channel_name,
                            scheduled_start.unwrap().format("%Y-%m-%d %H:%M:%S"), // Format the start time
                            live_title
                        );
                    } else {
                        tracing::info!(
                            "{} 未直播，计划于 {} 开始",
                            cfg.youtube.channel_name,
                            scheduled_start.unwrap().format("%Y-%m-%d %H:%M:%S")
                        );
                    }
                    old_scheduled_start = scheduled_start;
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
            old_cfg_title = cfg.bililive.title.clone();
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
            let channel_name = get_channel_name("YT", channel_id).unwrap();
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
            if !videos.is_empty() {
                let mut vid = videos.last().unwrap();
                let mut flag = false;
                for video in videos.iter().rev() {
                    let cname = video.get("channel");
                    if cname
                        .unwrap()
                        .get("name")
                        .unwrap()
                        .as_str()
                        .unwrap()
                        .replace(" ", "")
                        .contains(channel_name.as_ref().unwrap())
                    {
                        vid = video;
                        flag = true;
                        break;
                    }
                }
                if flag {
                    let topic_id = vid.get("topic_id");
                    if let Some(topic) = topic_id {
                        if let Some(topic_str) = topic.as_str() {
                            return Ok(topic_str.to_string());
                        }
                    }
                    tracing::info!("当前YT直播没有分区");
                    Err("当前YT直播没有分区".into())
                } else {
                    tracing::info!("当前频道没有直播");
                    Err("当前频道没有直播".into())
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
            let (is_live, title, area_id) = get_bili_live_status(cfg.bililive.room).await?;
            if is_live {
                let area_name = get_area_name(area_id);
                println!(
                    "B站直播状态: 直播中, 标题: {}, 分区: {} （ID: {}）",
                    title,
                    area_name.unwrap(),
                    area_id
                );
            } else {
                println!("B站直播状态: 未直播");
            }
        }
        "YT" => {
            let cfg = load_config(Path::new("YT/config.yaml"), Path::new("cookies.json"))?;
            let channel_id = if let Some(id) = channel_id {
                id
            } else {
                &cfg.youtube.channel_id
            };
            let mut channel_name = get_channel_name("YT", channel_id).unwrap();
            if channel_name.is_none() {
                channel_name = Some(cfg.youtube.channel_name.clone());
            }
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
                // println!("{:?}", videos);
                if !videos.is_empty() {
                    let mut vid = videos.last().unwrap();
                    let mut flag = false;
                    for video in videos.iter().rev() {
                        let cname = video.get("channel");
                        if cname
                            .unwrap()
                            .get("name")
                            .unwrap()
                            .as_str()
                            .unwrap()
                            .replace(" ", "")
                            .contains(channel_name.as_ref().unwrap())
                        {
                            let topic_id = video.get("topic_id").unwrap();
                            if topic_id.as_str().unwrap().contains("membersonly") {
                                // tracing::info!("频道 {} 正在进行会限直播", channel_name);
                                println!(
                                    "频道 {} 正在进行会限直播",
                                    channel_name.as_ref().unwrap()
                                );
                            } else {
                                vid = video;
                                flag = true;
                                break;
                            }
                        }
                    }
                    if flag {
                        let status = vid.get("status").unwrap();
                        if status == "upcoming" {
                            let start_time_str = vid
                                .get("start_scheduled")
                                .and_then(|v| v.as_str())
                                .ok_or("start_scheduled 不存在")?;
                            // 将时间字符串转换为DateTime<Local>
                            let start_time = DateTime::parse_from_rfc3339(&start_time_str)?
                                .with_timezone(&Local);
                            let title = vid.get("title").unwrap();
                            if title != "" {
                                println!(
                                    "{} 计划于 {} 开始 YouTube 直播, 标题: {}",
                                    channel_name.as_ref().unwrap(),
                                    start_time,
                                    title
                                );
                            } else {
                                println!(
                                    "{} 计划于 {} 开始 YouTube 直播",
                                    channel_name.as_ref().unwrap(),
                                    start_time
                                );
                            }
                        } else if status == "live" {
                            let title = vid.get("title").unwrap();
                            let channel_id =
                                get_channel_id("TW", channel_name.as_ref().unwrap()).unwrap();
                            if channel_id.is_some() {
                                if !get_twitch_live_status(channel_id.as_ref().unwrap())
                                    .await
                                    .unwrap()
                                {
                                    println!(
                                        "{} 在 YouTube 直播中, 标题: {}",
                                        channel_name.as_ref().unwrap(),
                                        title
                                    );
                                } else {
                                    println!(
                                        "{} 在 Twitch 直播中, 标题: {}",
                                        channel_name.as_ref().unwrap(),
                                        title
                                    );
                                }
                            } else {
                                println!(
                                    "{} 在 YouTube 直播中, 标题: {}",
                                    channel_name.as_ref().unwrap(),
                                    title
                                );
                            }
                        } else {
                            let channel_name = cfg.youtube.channel_name;
                            println!("{} 未直播", channel_name);
                        }
                    } else {
                        let channel_name = cfg.youtube.channel_name;
                        println!("{} 未直播", channel_name)
                    }
                } else {
                    println!("{} 未直播", cfg.youtube.channel_name);
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
            let mut channel_name = get_channel_name("TW", channel_id).unwrap();
            if channel_name.is_none() {
                channel_name = Some(channel_id.to_string());
            }

            if get_twitch_live_status(channel_id).await? {
                println!("{} 在 Twitch 直播中", channel_name.unwrap());
            } else {
                println!("{} 未在 Twitch 直播", channel_name.unwrap());
            }
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

            let title_str = get_youtube_live_title(channel_id).await?;
            if let Some(title) = title_str {
                // title end with date time like 2024-11-21 01:59 remove it
                let title = title.split(" 202").next().unwrap_or(&title).to_string();
                // tracing::info!("YouTube 直播标题: {}", title);
                Ok(title)
            } else {
                // tracing::info!("YouTube 直播标题: 空");
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
            let client = ClientBuilder::new(reqwest::Client::new()).build();

            let title = get_twitch_live_title(channel_id, client).await?;
            if title != "" {
                // println!("Twitch直播标题: {}", title);
                tracing::info!("Twitch 直播标题: {}", title);
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

async fn monitor_lol_game(puuid: Option<String>) -> Result<(), Box<dyn Error>> {
    if let Some(puuid_str) = puuid {
        let cfg = load_config(Path::new("YT/config.yaml"), Path::new("cookies.json"))?;
        let interval = cfg.lol_monitor_interval.unwrap_or(1);
        let riot_api = RiotApi::new(cfg.riot_api_key.clone().unwrap());
        thread::spawn(move || {
            let rt = tokio::runtime::Runtime::new().unwrap();
            loop {
                rt.block_on(async {
                    if let Ok(game_data) = riot_api
                        .spectator_v5()
                        .get_current_game_info_by_puuid(PlatformRoute::JP1, &puuid_str)
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
                            if let Ok(invalid_words) = fs::read_to_string("invalid_words.txt") {
                                if let Some(word) =
                                    invalid_words.lines().find(|word| ids.contains(word))
                                {
                                    bili_stop_live(&cfg).await.unwrap();
                                    tracing::info!("检测到非法词汇:{}，停止直播", word);
                                    return;
                                }
                            }
                        }
                    }
                });

                if !ffmpeg::is_any_ffmpeg_running() {
                    return;
                }
                thread::sleep(Duration::from_secs(interval));
            }
        });
    }
    Ok(())
}

fn get_puuid_from_file(channel_name: &str) -> Result<Option<String>, Box<dyn Error>> {
    let file = fs::File::open("./puuid.txt")?;
    let reader = io::BufReader::new(file);
    let mut puuid = None;

    for line in reader.lines() {
        let line = line?;
        if line
            .to_lowercase()
            .contains(&format!("({})", channel_name).to_lowercase())
        {
            let re = Regex::new(r"\[(.*?)\]").unwrap();
            if let Some(captures) = re.captures(&line) {
                puuid = captures.get(1).map(|m| m.as_str().to_string());
            }
        }
    }
    Ok(puuid)
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
