use super::twitch::get_twitch_status;
use super::youtube::get_youtube_status;
use crate::config::load_config;
use crate::config::Config;
use crate::plugins::bilibili;
use crate::plugins::ffmpeg;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use serde_yaml;
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;
use std::{
    fs,
    io::{self, BufRead},
    path::Path,
};
pub fn is_danmaku_running() -> bool {
    let mut cmd = Command::new("pgrep");
    cmd.arg("-f").arg("live-danmaku-cli");
    let output = cmd.output().expect("Failed to execute pgrep");
    if output.status.success() {
        return true;
    }
    false
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
}

#[derive(Serialize, Deserialize, Clone)]
struct ChannelsConfig {
    channels: Vec<Channel>,
}

fn load_channels() -> Result<ChannelsConfig, Box<dyn std::error::Error>> {
    let content = fs::read_to_string("channels.json")?;
    let config: ChannelsConfig = serde_json::from_str(&content)?;
    Ok(config)
}

pub fn get_channel_id(
    platform: &str,
    channel_name: &str,
) -> Result<Option<String>, Box<dyn std::error::Error>> {
    let config = load_channels()?;

    for channel in config.channels {
        if channel.name == channel_name {
            match platform {
                "YT" => return Ok(channel.platforms.youtube),
                "TW" => return Ok(channel.platforms.twitch),
                _ => return Ok(None),
            }
        }
    }
    Ok(None)
}

pub fn get_channel_name(
    platform: &str,
    channel_id: &str,
) -> Result<Option<String>, Box<dyn std::error::Error>> {
    let config = load_channels()?;

    for channel in config.channels {
        match platform {
            "YT" => {
                if let Some(id) = channel.platforms.youtube {
                    if id == channel_id {
                        return Ok(Some(channel.name));
                    }
                }
            }
            "TW" => {
                if let Some(id) = channel.platforms.twitch {
                    if id == channel_id {
                        return Ok(Some(channel.name));
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

    for channel in config.channels {
        if channel.name == channel_name {
            if let Some(puuid) = channel.riot_puuid {
                return Ok(puuid);
            }
        }
    }
    Err("PUUID not found for channel".into())
}

// Optional: Helper function to get all channels for a platform
pub fn get_all_channels(
    platform: &str,
) -> Result<Vec<(String, String)>, Box<dyn std::error::Error>> {
    let config = load_channels()?;
    let mut channels = Vec::new();

    for channel in config.channels {
        match platform {
            "YT" => {
                if let Some(id) = channel.platforms.youtube {
                    channels.push((channel.name, id));
                }
            }
            "TW" => {
                if let Some(id) = channel.platforms.twitch {
                    channels.push((channel.name, id));
                }
            }
            _ => (),
        }
    }

    Ok(channels)
}

// Optional: Helper function to get all PUUIDs
pub fn get_all_puuids() -> Result<Vec<(String, String)>, Box<dyn std::error::Error>> {
    let config = load_channels()?;
    let mut puuids = Vec::new();

    for channel in config.channels {
        if let Some(puuid) = channel.riot_puuid {
            puuids.push((channel.name, puuid));
        }
    }

    Ok(puuids)
}

/// Updates the configuration YAML file with new values.
fn update_config(
    platform: &str,
    channel_name: &str,
    channel_id: &str,
    area_id: u64,
) -> io::Result<()> {
    let config_path = Path::new("config.yaml");

    // Read the existing config.yaml
    let config_content = fs::read_to_string(config_path)?;

    // Deserialize YAML into Config struct
    let mut config: Config = serde_yaml::from_str(&config_content)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

    // Update the fields
    if platform == "YT" {
        config.youtube.channel_id = channel_id.to_string();
        config.youtube.channel_name = channel_name.to_string();
        config.youtube.area_v2 = area_id;
    } else if platform == "TW" {
        config.twitch.channel_id = channel_id.to_string();
        config.twitch.channel_name = channel_name.to_string();
        config.twitch.area_v2 = area_id;
    }

    // Serialize Config struct back to YAML
    let updated_yaml =
        serde_yaml::to_string(&config).map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;

    // Write the updated YAML back to config.yaml
    fs::write(config_path, updated_yaml)?;

    // tracing::info!("Updated configuration for {}: {}", platform, channel_name);
    Ok(())
}

/// determines the area id based on the live title.
pub fn check_area_id_with_title(live_title: &str, current_area_id: u64) -> u64 {
    let title = live_title.to_lowercase();
    let title = title.replace("_", " ");

    if title.contains("valorant") || title.contains("ヴァロ") {
        329
    } else if title.contains("league of legends")
        || title.contains("lol")
        || title.contains("ろる")
        || title.contains("ろ、る")
        || title.contains("TFT")
    {
        86
    } else if title.contains("minecraft") || title.contains("マイクラ") {
        216
    } else if title.contains("overwatch") {
        87
    } else if title.contains("deadlock") {
        927
    } else if title.contains("final fantasy")
        || title.contains("漆黒メインクエ")
        || title.contains("ff14")
    {
        102
    } else if title.contains("apex") {
        240
    } else if title.contains("スト６") || title.contains("street fighter") {
        433
    } else if title.contains("yu-gi-oh") || title.contains("遊戯王") {
        407
    } else if title.contains("splatoon") || title.contains("スプラトゥーン3") {
        694
    } else if title.contains("原神") {
        321
    } else if title.contains("monhun")
        || title.contains("モンハン")
        || title.contains("monster hunter")
    {
        578
    } else if title.contains("pokemon")
        || title.contains("core keeper")
        || title.contains("terraria")
        || title.contains("tgc card shop simulator")
        || title.contains("stardew valley")
        || title.contains("gta")
    {
        235
    } else if title.contains("tarkov") || title.contains("タルコフ") {
        252
    } else if title.contains("call of duty") || title.contains("BO6") {
        318
    } else if title.contains("elden ring") || title.contains("エルデンリング") {
        555
    } else {
        current_area_id
    }
}

/// Processes a single danmaku command.
async fn process_danmaku(command: &str) {
    // only line start with : is danmaku
    if command.contains("WARN  [init] Connection closed by server") {
        tracing::info!("B站cookie过期，无法启动弹幕指令，请更新配置文件:./biliup login");
        return;
    }
    if !command.starts_with(" :") {
        return;
    }
    // tracing::info!("弹幕:{}", &command[2..]);
    let command = command.replace(" ", "").replace("　", "");
    let normalized_danmaku = command.replace("％", "%");

    let cfg = load_config().await.unwrap();
    // Add check for 查询 command
    if normalized_danmaku.contains("%查询") {
        // tracing::info!("查询弹幕");
        let channel_name = cfg.youtube.channel_name.clone();
        let area_name = get_area_name(cfg.youtube.area_v2);
        let _ = bilibili::send_danmaku(
            &cfg,
            &format!("YT: {} - {}", channel_name, area_name.unwrap()),
        )
        .await;
        let channel_name = cfg.twitch.channel_name.clone();
        let area_name = get_area_name(cfg.twitch.area_v2);
        // bilibili 发送弹幕cooldown > 1秒
        tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
        let _ = bilibili::send_danmaku(
            &cfg,
            &format!("TW: {} - {}", channel_name, area_name.unwrap()),
        )
        .await;
        return;
    }

    // Continue with existing command processing for %转播% commands
    if !normalized_danmaku.contains("%转播%") {
        // tracing::error!("弹幕命令格式错误. Skipping...");
        return;
    }
    let danmaku_command = normalized_danmaku.replace(" :", "");
    // tracing::info!("{}", danmaku_command);

    // Replace full-width ％ with half-width %
    let parts: Vec<&str> = danmaku_command.split('%').collect();
    // tracing::info!("弹幕:{:?}", parts);
    if parts.len() < 4 {
        tracing::error!("弹幕命令格式错误. Skipping...");
        let _ = bilibili::send_danmaku(&cfg, "错误：弹幕命令格式错误").await;
        return;
    }

    let platform = parts[2];
    let channel_name = parts[3];
    let area_name = parts[4];
    tracing::info!(
        "平台: {}, 频道: {}, 分区: {}",
        platform,
        channel_name,
        area_name
    );

    // Determine area_id based on area_name
    let area_id = match get_area_id(area_name) {
        Ok(id) => id,
        Err(e) => {
            tracing::error!("{}", e);
            let _ = bilibili::send_danmaku(&cfg, &format!("错误：{}", e)).await;
            return;
        }
    };

    if platform.eq("YT") || platform.eq("TW") {
        let channel_id = match get_channel_id(platform, channel_name) {
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

        // Use a reference to the String inside channel_id without moving it
        let channel_id_str = channel_id.as_ref().unwrap();

        let (live_title, live_topic) = if platform.eq_ignore_ascii_case("YT") {
            // get youtube live status
            match get_youtube_status(channel_id_str).await {
                Ok((_, topic, title, _, _)) => {
                    let t = match title {
                        Some(t) => t,
                        None => {
                            tracing::error!("获取YT直播标题失败");
                            let _ = bilibili::send_danmaku(&cfg, "错误：获取YT直播标题失败").await;
                            return;
                        }
                    };
                    (t, topic.unwrap_or_default())
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
                Ok((_, topic, title)) => {
                    let t = match title {
                        Some(t) => t,
                        None => {
                            tracing::error!("获取TW直播标题失败");
                            let _ = bilibili::send_danmaku(&cfg, "错误：获取TW直播标题失败").await;
                            return;
                        }
                    };
                    (t, topic.unwrap_or_default())
                }
                Err(e) => {
                    tracing::error!("获取TW直播标题时出错: {}", e);
                    let _ =
                        bilibili::send_danmaku(&cfg, &format!("错误：获取TW直播标题时出错 {}", e))
                            .await;
                    return;
                }
            }
        };
        let live_topic_title = format!("{} {}", live_topic, live_title).to_lowercase();
        if live_topic_title.contains("ウォッチパ")
            || live_topic_title.contains("watchalong")
            || live_topic_title.contains("talk")
            || live_topic_title.contains("zatsudan")
            || live_topic_title.contains("雑談")
            || live_topic_title.contains("marshmallow")
            || live_topic_title.contains("morning")
            || live_topic_title.contains("freechat")
            || live_topic_title.contains("どうぶつの森")
            || live_topic_title.contains("animal crossing")
            || live_topic_title.contains("just chatting")
        {
            tracing::error!("直播标题/topic包含不支持的关键词:\n{}", live_topic_title);
            let _ = bilibili::send_danmaku(&cfg, "错误：目标直播标题/分区包含不支持的关键词").await;
            return;
        }
        // Now you can use channel_id_str where needed without moving channel_id
        // let new_title = format!("【转播】{}", channel_name);
        let updated_area_id = check_area_id_with_title(&live_topic_title, area_id);
        // Additional checks for specific area_ids
        if (updated_area_id == 240 || updated_area_id == 318 || updated_area_id == 252)
            && channel_name != "Kamito"
        {
            tracing::error!("只有'Kamito'可以使用 Apex, COD or Tarkov 分区. Skipping...");
            let _ =
                bilibili::send_danmaku(&cfg, "错误：只有'Kamito'可以使用 Apex, COD or Tarkov 分区")
                    .await;
            return;
        }

        if let Err(e) = update_config(platform, channel_name, &channel_id_str, updated_area_id) {
            tracing::error!("更新配置时出错: {}", e);
            let _ = bilibili::send_danmaku(&cfg, &format!("错误：更新配置时出错 {}", e)).await;
            return;
        }

        let updated_area_name = match get_area_name(updated_area_id) {
            Some(name) => name,
            None => {
                let _ = bilibili::send_danmaku(&cfg, "错误：无法获取更新后的分区名称").await;
                return;
            }
        };

        tracing::info!(
            "更新 {} 频道: {} 分区: {} (ID: {} )",
            platform,
            channel_name,
            updated_area_name,
            updated_area_id
        );

        // Send success notification
        let _ = bilibili::send_danmaku(
            &cfg,
            &format!(
                "更新：{} - {} - {}",
                platform, channel_name, updated_area_name
            ),
        )
        .await;
    } else {
        tracing::error!("指令错误: {}", danmaku_command);
        let _ = bilibili::send_danmaku(&cfg, &format!("错误：不支持的平台 {}", platform)).await;
    }
}

/// Retrieves the room ID from the configuration.
fn get_room_id() -> String {
    match fs::read_to_string("config.json") {
        Ok(content) => match serde_json::from_str::<Value>(&content) {
            Ok(json) => json["roomId"].to_string(),
            Err(e) => {
                tracing::error!("解析JSON时出错: {}", e);
                "".to_string()
            }
        },
        Err(e) => {
            tracing::error!("读取config.json时出错: {}", e);
            "".to_string()
        }
    }
}

/// Main function to execute danmaku processing.
pub fn run_danmaku() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        // 更新config.json中的sessdata 为cfg.bililive.credentials.sessdata
        let cfg = load_config().await.unwrap();
        Command::new("sed")
            .arg("-i")
            .arg(format!(
                r#"s|\"sessdata\": \".*\"|\"sessdata\": \"{}\"|"#,
                cfg.bililive.credentials.sessdata
            ))
            .arg("config.json")
            .output()
            .expect("更新sessdata失败");
        // Start danmaku-cli in background
        let danmaku_cli = Command::new("./live-danmaku-cli")
            .arg("--config")
            .arg("config.json")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("启动弹幕命令读取失败");

        let stdout = danmaku_cli.stdout.expect("捕获stdout失败");
        let stderr = danmaku_cli.stderr.expect("捕获stderr失败");

        // Handle stdout in a separate thread
        thread::spawn(move || {
            let reader = io::BufReader::new(stdout);
            for line in reader.lines() {
                if let Ok(line) = line {
                    // Process each danmaku command
                    tokio::runtime::Runtime::new()
                        .unwrap()
                        .block_on(process_danmaku(&line));
                }
            }
        });

        // Handle stderr in a separate thread
        thread::spawn(move || {
            let reader = io::BufReader::new(stderr);
            for line in reader.lines() {
                if let Ok(line) = line {
                    eprintln!("弹幕stderr: {}", line);
                }
            }
        });
        tracing::info!("弹幕命令读取启动");
        // Monitor Bilibili live status every 300 seconds
        loop {
            thread::sleep(Duration::from_secs(60));

            let room_id = get_room_id();

            if room_id.is_empty() {
                tracing::error!("从config.json中获取房间ID失败");
                continue;
            }

            // tracing::info!("Room ID: {}", room_id);
            let bilibili_status = match Command::new("./bilistream")
                .arg("get-live-status")
                .arg("bilibili")
                .arg(room_id)
                .output()
            {
                Ok(output) => String::from_utf8_lossy(&output.stdout).to_string(),
                Err(e) => {
                    tracing::error!("检查Bilibili直播间状态时出错: {}", e);
                    continue;
                }
            };

            if !bilibili_status.contains("未直播") {
                if ffmpeg::is_ffmpeg_running() {
                    tracing::info!("ffmpeg 正在运行. 停止弹幕命令读取...");
                    // Kill danmaku-cli process
                    Command::new("pkill")
                        .arg("-f")
                        .arg("danmaku-cli")
                        .output()
                        .expect("停止弹幕命令读取失败");

                    break;
                }
            }
        }
    });
}

pub fn get_area_name(area_id: u64) -> Option<&'static str> {
    match area_id {
        86 => Some("英雄联盟"),
        329 => Some("无畏契约"),
        240 => Some("APEX英雄"),
        87 => Some("守望先锋"),
        235 => Some("其他单机"),
        107 => Some("其他网游"),
        530 => Some("萌宅领域"),
        236 => Some("主机游戏"),
        321 => Some("原神"),
        694 => Some("斯普拉遁3"),
        407 => Some("游戏王：决斗链接"),
        433 => Some("格斗游戏"),
        927 => Some("DeadLock"),
        216 => Some("我的世界"),
        646 => Some("UP主日常"),
        102 => Some("最终幻想14"),
        252 => Some("逃离塔科夫"),
        318 => Some("使命召唤:战区"),
        555 => Some("艾尔登法环"),
        578 => Some("怪物猎人"),
        _ => {
            tracing::error!("未知的分区ID: {}", area_id);
            None
        }
    }
}

fn get_area_id(area_name: &str) -> Result<u64, Box<dyn std::error::Error>> {
    match area_name {
        "英雄联盟" => Ok(86),
        "无畏契约" => Ok(329),
        "APEX英雄" => Ok(240),
        "守望先锋" => Ok(87),
        "萌宅领域" => Ok(530),
        "其他单机" => Ok(235),
        "其他网游" => Ok(107),
        "UP主日常" => Ok(646),
        "最终幻想14" => Ok(102),
        "格斗游戏" => Ok(433),
        "我的世界" => Ok(216),
        "DeadLock" => Ok(927),
        "主机游戏" => Ok(236),
        "原神" => Ok(321),
        "斯普拉遁3" => Ok(694),
        "游戏王：决斗链接" => Ok(407),
        "逃离塔科夫" => Ok(252),
        "使命召唤:战区" => Ok(318),
        "艾尔登法环" => Ok(555),
        "怪物猎人" => Ok(578),
        _ => Err(format!("未知的分区: {}", area_name).into()),
    }
}
