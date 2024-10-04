use regex::Regex;
use serde_json::Value;
use std::fs;
use std::io::{self, BufRead};
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

/// Checks if any danmaku lock file exists.
pub fn is_any_danmaku_running() -> bool {
    Path::new("danmaku.lock-YT").exists() || Path::new("danmaku.lock-TW").exists()
}

/// Creates the danmaku lock file for the specified platform.
pub fn create_danmaku_lock(platform: &str) -> io::Result<()> {
    let lock_file = format!("danmaku.lock-{}", platform);
    fs::File::create(&lock_file)?;
    tracing::info!("{} created", lock_file);
    Ok(())
}

/// Removes the danmaku lock file for the specified platform.
pub fn remove_danmaku_lock(platform: &str) -> io::Result<()> {
    let lock_file = format!("danmaku.lock-{}", platform);
    fs::remove_file(&lock_file)?;
    tracing::info!("{} removed", lock_file);
    Ok(())
}

/// Checks if a channel is in the allowed list and retrieves the channel name.
fn check_channel(platform: &str, channel_name: &str) -> io::Result<String> {
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
                    .map_or(String::new(), |m| m.as_str().to_string()));
            }
        }
    }

    Ok(String::new())
}

/// Checks live status using the bilistream CLI.
async fn check_live_status(platform: &str, channel_id: &str) -> io::Result<String> {
    let output = Command::new("./bilistream")
        .arg("get-live-status")
        .arg(platform)
        .arg(channel_id)
        .output()?;

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    Ok(stdout)
}

/// Updates the configuration YAML file with new values.
fn update_config(
    platform: &str,
    channel_name: &str,
    channel_id: &str,
    new_title: &str,
    area_id: u32,
) -> io::Result<()> {
    let config_path = format!("./{}/config.yaml", platform);

    // Update ChannelId
    let channel_id_regex = Regex::new(r"(?m)^ChannelId:.*$").unwrap();
    let config_content = fs::read_to_string(&config_path)?;
    let updated_content = channel_id_regex
        .replace(&config_content, format!("ChannelId: {}", channel_id))
        .to_string();

    // Update ChannelName
    let channel_name_regex = Regex::new(r#"(?m)^ChannelName: .*"#).unwrap();
    let updated_content = channel_name_regex
        .replace(
            &updated_content,
            format!("ChannelName: \"{}\"", channel_name),
        )
        .to_string();

    // Update Title
    let title_regex = Regex::new(r#"(?m)^Title: .*"#).unwrap();
    let updated_content = title_regex
        .replace(&updated_content, format!("Title: \"{}\"", new_title))
        .to_string();

    // Update Area_v2
    let area_regex = Regex::new(r"(?m)^Area_v2:.*$").unwrap();
    let updated_content = area_regex
        .replace(&updated_content, format!("Area_v2: {}", area_id))
        .to_string();

    fs::write(&config_path, updated_content)?;
    // tracing::info!("Updated configuration for {}: {}", platform, channel_name);
    Ok(())
}

/// Determines the area ID based on the live title.
fn check_area_id_with_title(live_title: &str, current_area_id: u32) -> u32 {
    let title = live_title.to_lowercase();
    if title.contains("valorant") {
        329
    } else if title.contains("league of legends")
        || title.contains("lol")
        || title.contains("k4sen")
    {
        86
    } else if title.contains("minecraft") || title.contains("マイクラ") {
        216
    } else if title.contains("overwatch") {
        87
    } else if title.contains("deadlock") {
        927
    } else if title.contains("漆黒メインクエ") || title.contains("ff14") {
        102
    } else {
        current_area_id
    }
}

/// Processes a single danmaku command.
async fn process_danmaku(command: &str) {
    // only line start with : is danmaku
    if !command.starts_with(" :") {
        return;
    }

    // Validate danmaku command format: %转播%平台%频道名%分区
    if !command.contains("%转播%") {
        // tracing::error!("弹幕命令格式错误. Skipping...");
        return;
    }
    let danmaku_command = command.replace(" :", "");
    tracing::info!("弹幕:{}", danmaku_command);

    // Replace full-width ％ with half-width %
    let normalized_danmaku = danmaku_command.replace("％", "%");
    let parts: Vec<&str> = normalized_danmaku.split('%').collect();

    if parts.len() < 5 {
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
        _ => {
            tracing::error!("未知的分区: {}", area_name);
            return;
        }
    };

    // Additional checks for specific area_ids
    if area_id == 240 && channel_name != "kamito" {
        tracing::error!("只有'kamito'可以使用Apex分区. Skipping...");
        return;
    }

    if platform.eq("YT") || platform.eq("TW") {
        let channel_id = match check_channel(platform, channel_name) {
            Ok(id) => id,
            Err(e) => {
                tracing::error!("检查频道时出错: {}", e);
                return;
            }
        };

        if channel_id.is_empty() {
            tracing::error!(
                "Channel {} not found in allowed list for {}",
                channel_name,
                platform
            );
            return;
        }

        let live_status = match check_live_status(platform, &channel_id).await {
            Ok(status) => status,
            Err(e) => {
                tracing::error!("获取直播状态时出错: {}", e);
                return;
            }
        };

        if !live_status.contains("Not Live") {
            // tracing::info!("area_id: {}", area_id);
            // let config_path = format!("./{}/config.yaml", platform);
            let new_title = format!("【转播】{}", channel_name);

            let live_title = if platform.eq_ignore_ascii_case("YT") {
                match Command::new("yt-dlp")
                    .arg("-e")
                    .arg(&format!(
                        "https://www.youtube.com/channel/{}/live",
                        channel_id
                    ))
                    .output()
                {
                    Ok(output) => String::from_utf8_lossy(&output.stdout).trim().to_string(),
                    Err(e) => {
                        tracing::error!("获取YT直播标题时出错: {}", e);
                        return;
                    }
                }
            } else {
                // TW
                match Command::new("./bilistream")
                    .arg("get-live-title")
                    .arg("TW")
                    .arg(&channel_id)
                    .output()
                {
                    Ok(output) => String::from_utf8_lossy(&output.stdout).trim().to_string(),
                    Err(e) => {
                        tracing::error!("获取TW直播标题时出错: {}", e);
                        return;
                    }
                }
            };

            let updated_area_id = check_area_id_with_title(&live_title, area_id);

            if let Err(e) = update_config(
                platform,
                channel_name,
                &channel_id,
                &new_title,
                updated_area_id,
            ) {
                tracing::error!("更新配置时出错: {}", e);
                return;
            }

            tracing::info!(
                "更新 {} 频道: {} 分区: {}",
                platform,
                channel_name,
                area_name
            );
        } else {
            tracing::info!(
                "频道 {} ({}) 未在 {} 直播",
                channel_name,
                channel_id,
                platform
            );
        }
    } else {
        tracing::error!("不支持的平台: {}", platform);
    }
}

/// Retrieves the room ID from the configuration.
fn get_room_id() -> String {
    match fs::read_to_string("config.json") {
        Ok(content) => match serde_json::from_str::<Value>(&content) {
            Ok(json) => json["roomId"].as_str().unwrap_or("").to_string(),
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
        tracing::info!("一个弹幕命令读取实例已经在运行. 跳过新实例.");
        return;
    }

    // Create the lock file for the specified platform
    if let Err(e) = create_danmaku_lock(platform) {
        tracing::error!("创建弹幕锁文件时出错: {}", e);
        return;
    }

    tracing::info!("在bilistream-{} 中启动弹幕命令读取", platform);

    // Start danmaku-cli in background
    let danmaku_cli = Command::new("./danmaku-cli")
        .arg("--config")
        .arg("config.json")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("Failed to start danmaku-cli");

    let stdout = danmaku_cli.stdout.expect("Failed to capture stdout");
    let stderr = danmaku_cli.stderr.expect("Failed to capture stderr");

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
                eprintln!("Danmaku stderr: {}", line);
            }
        }
    });

    tracing::info!("弹幕命令读取已在进程bilistream-{} 中执行", platform);

    // Monitor Bilibili live status every 300 seconds
    loop {
        tracing::info!("检查Bilibili直播间状态...");
        thread::sleep(Duration::from_secs(60));

        let room_id = get_room_id();

        if room_id.is_empty() {
            tracing::error!("从config.json中获取房间ID失败");
            continue;
        }

        tracing::info!("Room ID: {}", room_id);
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

        if bilibili_status.contains("Not Live") {
            tracing::info!("Bilibili 未直播. 继续弹幕命令读取...");
        } else {
            tracing::info!("Bilibili 正在直播. 停止弹幕命令读取...");
            // Kill danmaku-cli process
            Command::new("pkill")
                .arg("-f")
                .arg("danmaku-cli")
                .output()
                .expect("Failed to stop danmaku-cli");

            // Remove all danmaku lock files
            if let Err(e) = remove_danmaku_lock("YT") {
                tracing::error!("删除 danmaku.lock-YT 时出错: {}", e);
            }
            if let Err(e) = remove_danmaku_lock("TW") {
                tracing::error!("删除 danmaku.lock-TW 时出错: {}", e);
            }

            break;
        }
    }
}
