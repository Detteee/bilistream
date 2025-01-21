use crate::config::load_config;
use crate::config::Config;
use crate::plugins::ffmpeg;
use regex::Regex;
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
/// Checks if any danmaku lock file exists.
pub fn is_any_danmaku_running() -> bool {
    if Path::new("danmaku.lock-YT").exists() {
        // tracing::info!("一个弹幕命令读取实例已经在YT运行.");
        return true;
    }
    if Path::new("danmaku.lock-TW").exists() {
        // tracing::info!("一个弹幕命令读取实例已经在TW运行.");
        return true;
    }
    false
}

/// Creates the danmaku lock file for the specified platform.
pub fn create_danmaku_lock(platform: &str) -> io::Result<()> {
    let lock_file = format!("danmaku.lock-{}", platform);
    fs::File::create(&lock_file)?;
    tracing::info!("{} created", lock_file);
    Ok(())
}

/// Removes the danmaku lock file for the specified platform.
pub fn remove_danmaku_lock() -> io::Result<()> {
    if Path::new("danmaku.lock-YT").exists() {
        fs::remove_file("danmaku.lock-YT")?;
        tracing::info!("删除弹幕锁文件danmaku.lock-YT成功");
    } else if Path::new("danmaku.lock-TW").exists() {
        fs::remove_file("danmaku.lock-TW")?;
        tracing::info!("删除弹幕锁文件danmaku.lock-TW成功");
    } else {
        tracing::error!("弹幕锁文件不存在");
    }
    Ok(())
}

/// Checks if a channel is in the allowed list and retrieves the channel name.
pub fn get_channel_id(
    platform: &str,
    channel_name: &str,
) -> Result<Option<String>, Box<dyn std::error::Error>> {
    let file_path = format!("./{}/{}_channels.txt", platform, platform);
    let file = fs::File::open(&file_path)?;
    let reader = io::BufReader::new(file);
    // tracing::info!("检查频道: {}", file_path);
    for line in reader.lines() {
        let line = line?;
        if line
            .to_lowercase()
            .contains(&format!("({})", channel_name).to_lowercase())
        {
            // Extract channel name using regex
            let re = Regex::new(r"\[(.*?)\]").unwrap();
            if let Some(captures) = re.captures(&line) {
                return Ok(captures
                    .get(1)
                    .map_or(None, |m| Some(m.as_str().to_string())));
            }
        }
    }
    Ok(None)
}

pub fn get_channel_name(
    platform: &str,
    channel_id: &str,
) -> Result<Option<String>, Box<dyn std::error::Error>> {
    let file_path = format!("./{}/{}_channels.txt", platform, platform);
    let file = fs::File::open(&file_path)?;
    let reader = io::BufReader::new(file);
    for line in reader.lines() {
        let line = line?;
        if line.contains(channel_id) {
            let re = Regex::new(r"\((.*?)\)").unwrap();
            if re.is_match(&line) {
                return Ok(Some(
                    re.captures(&line)
                        .unwrap()
                        .get(1)
                        .unwrap()
                        .as_str()
                        .to_string(),
                ));
            }
        }
    }
    Ok(None)
}

/// Checks live status using the bilistream CLI.
// async fn check_live_status(platform: &str, channel_id: &str) -> io::Result<String> {
//     let output = Command::new("./bilistream")
//         .arg("get-live-status")
//         .arg(platform)
//         .arg(channel_id)
//         .output()?;

//     let stdout = String::from_utf8_lossy(&output.stdout).to_string();
//     Ok(stdout)
// }

/// Updates the configuration YAML file with new values.
fn update_config(
    platform: &str,
    channel_name: &str,
    channel_id: &str,
    new_title: &str,
    area_id: u64,
) -> io::Result<()> {
    let config_path = format!("./{}/config.yaml", platform);
    let config_path = Path::new(&config_path);

    // Read the existing config.yaml
    let config_content = fs::read_to_string(config_path)?;

    // Deserialize YAML into Config struct
    let mut config: Config = serde_yaml::from_str(&config_content)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

    // Update the fields
    if platform == "YT" {
        config.youtube.channel_id = channel_id.to_string();
        config.youtube.channel_name = channel_name.to_string();
    } else if platform == "TW" {
        config.twitch.channel_id = channel_id.to_string();
        config.twitch.channel_name = channel_name.to_string();
    }

    config.bililive.title = new_title.to_string();
    config.bililive.area_v2 = area_id;

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
    {
        86
    } else if title.contains("minecraft") || title.contains("マイクラ") {
        216
    } else if title.contains("overwatch") {
        87
    } else if title.contains("deadlock") {
        927
    } else if title.contains("final fantasy online")
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
    } else if title.contains("monhun") || title.contains("モンハン") {
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
    let command = command.replace(" ", "");
    let normalized_danmaku = command.replace("％", "%");
    // Validate danmaku command format: %转播%平台%频道名%分区
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
    let area_id = match area_name {
        "英雄联盟" => 86,
        "无畏契约" => 329,
        "APEX英雄" => 240,
        "守望先锋" => 87,
        "萌宅领域" => 530,
        "其他单机" => 235,
        "其他网游" => 107,
        "UP主日常" => 646,
        "最终幻想14" => 102,
        "格斗游戏" => 433,
        "我的世界" => 216,
        "DeadLock" => 927,
        "主机游戏" => 236,
        "原神" => 321,
        "斯普拉遁3" => 694,
        "游戏王：决斗链接" => 407,
        "逃离塔科夫" => 252,
        "使命召唤:战区" => 318,
        "艾尔登法环" => 555,
        "怪物猎人" => 578,
        _ => {
            tracing::error!("未知的分区: {}", area_name);
            return;
        }
    };

    if platform.eq("YT") || platform.eq("TW") {
        let channel_id = match get_channel_id(platform, channel_name) {
            Ok(id) => id,
            Err(e) => {
                tracing::error!("检查频道时出错: {}", e);
                return;
            }
        };

        if channel_id.is_none() {
            tracing::error!("频道 {} 未在{}列表中", channel_name, platform);
            return;
        }

        // Use a reference to the String inside channel_id without moving it
        let channel_id_str = channel_id.as_ref().unwrap();

        let live_title = if platform.eq_ignore_ascii_case("YT") {
            // get youtube live topic
            match Command::new("./bilistream")
                .arg("get-live-topic")
                .arg("YT")
                .arg(channel_id_str)
                .output()
            {
                Ok(output) => String::from_utf8_lossy(&output.stdout).trim().to_string(),
                Err(e) => {
                    tracing::error!("获取YT直播分区时出错: {}", e);
                    match Command::new("yt-dlp")
                        .arg("-e")
                        .arg(&format!(
                            "https://www.youtube.com/channel/{}/live",
                            channel_id_str
                        ))
                        .output()
                    {
                        Ok(output) => String::from_utf8_lossy(&output.stdout).trim().to_string(),
                        Err(e) => {
                            tracing::error!("获取YT直播标题时出错: {}", e);
                            return;
                        }
                    }
                }
            }
            // match Command::new("yt-dlp")
            //     .arg("-e")
            //     .arg(&format!(
            //         "https://www.youtube.com/channel/{}/live",
            //         channel_id_str
            //     ))
            //     .output()
            // {
            //     Ok(output) => String::from_utf8_lossy(&output.stdout).trim().to_string(),
            //     Err(e) => {
            //         tracing::error!("获取YT直播标题时出错: {}", e);
            //         return;
            //     }
            // }
        } else {
            // TW
            match Command::new("./bilistream")
                .arg("get-live-title")
                .arg("TW")
                .arg(channel_id_str)
                .output()
            {
                Ok(output) => String::from_utf8_lossy(&output.stdout).trim().to_string(),
                Err(e) => {
                    tracing::error!("获取TW直播标题时出错: {}", e);
                    return;
                }
            }
        };
        println!("{}", live_title);
        let live_title = live_title.to_lowercase();
        if live_title.contains("ウォッチパ")
            || live_title.contains("watchalong")
            || live_title.contains("talk")
            || live_title.contains("zatsudan")
            || live_title.contains("雑談")
            || live_title.contains("marshmallow")
            || live_title.contains("morning")
            || live_title.contains("freechat")
        {
            tracing::error!("直播标题/topic包含不支持的关键词");
            return;
        }
        // Now you can use channel_id_str where needed without moving channel_id
        let new_title = format!("【转播】{}", channel_name);
        let updated_area_id = check_area_id_with_title(&live_title, area_id);
        // Additional checks for specific area_ids
        if (updated_area_id == 240 || updated_area_id == 318 || updated_area_id == 252)
            && channel_name != "Kamito"
        {
            tracing::error!("只有'Kamito'可以使用 Apex, COD or Tarkov 分区. Skipping...");
            return;
        }
        if let Err(e) = update_config(
            platform,
            channel_name,
            &channel_id_str,
            &new_title,
            updated_area_id,
        ) {
            tracing::error!("更新配置时出错: {}", e);
            return;
        }
        let updated_area_name = match get_area_name(updated_area_id) {
            Some(name) => name,
            None => return, // Early return if the area ID is unknown
        };
        tracing::info!(
            "更新 {} 频道: {} 分区: {} (ID: {} )",
            platform,
            channel_name,
            updated_area_name,
            updated_area_id
        );
    } else {
        tracing::error!("指令错误: {}", danmaku_command);
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
pub fn run_danmaku(platform: &str) {
    // Check if any danmaku is already running
    if is_any_danmaku_running() {
        return;
    }

    // Create the lock file for the specified platform
    if let Err(e) = create_danmaku_lock(platform) {
        tracing::error!("创建弹幕锁文件时出错: {}", e);
        return;
    }
    // 更新config.json中的sessdata 为cfg.bililive.credentials.sessdata
    let cfg = load_config(Path::new("YT/config.yaml"), Path::new("cookies.json")).unwrap();
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

    tracing::info!("弹幕命令读取已在进程 bilistream-{} 中执行", platform);

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
            if ffmpeg::is_any_ffmpeg_running() {
                tracing::info!("ffmpeg 正在运行. 停止弹幕命令读取...");
                // Kill danmaku-cli process
                Command::new("pkill")
                    .arg("-f")
                    .arg("danmaku-cli")
                    .output()
                    .expect("停止弹幕命令读取失败");

                // Try to remove both lock files, logging any errors
                remove_danmaku_lock().expect("删除弹幕锁文件失败");
                break;
            }
        }
    }
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
