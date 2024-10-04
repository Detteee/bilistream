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
    println!("{} created", lock_file);
    Ok(())
}

/// Removes the danmaku lock file for the specified platform.
pub fn remove_danmaku_lock(platform: &str) -> io::Result<()> {
    let lock_file = format!("danmaku.lock-{}", platform);
    fs::remove_file(&lock_file)?;
    println!("{} removed", lock_file);
    Ok(())
}

/// Checks if a channel is in the allowed list and retrieves the channel name.
fn check_channel(platform: &str, channel_id: &str) -> io::Result<String> {
    let file_path = format!("./{}/{}_channels.txt", platform, platform);
    let file = fs::File::open(&file_path)?;
    let reader = io::BufReader::new(file);

    for line in reader.lines() {
        let line = line?;
        if line
            .to_lowercase()
            .contains(&format!("[{}]", channel_id).to_lowercase())
        {
            // Extract channel name using regex
            let re = Regex::new(r"\((.*?)\)").unwrap();
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
    println!("Updated configuration for {}: {}", platform, channel_name);
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
    println!("Processing danmaku: {}", command);

    // Validate danmaku command format: %转播%平台%频道名%分区
    if !command.contains("%转播%") {
        println!("Invalid danmaku command format. Skipping...");
        return;
    }

    // Replace full-width ％ with half-width %
    let normalized_danmaku = command.replace("％", "%");
    let parts: Vec<&str> = normalized_danmaku.split('%').collect();

    if parts.len() < 5 {
        println!("Incomplete danmaku command. Skipping...");
        return;
    }

    let platform = parts[2];
    let channel_name = parts[3];
    let area_name = parts[4];
    println!(
        "Platform: {}, Channel Name: {}, Area Name: {}",
        platform, channel_name, area_name
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
            println!("Unknown area: {}", area_name);
            return;
        }
    };

    // Additional checks for specific area_ids
    if area_id == 240 && channel_name != "kamito" {
        println!("Only 'kamito' is allowed to use Apex area. Skipping...");
        return;
    }

    if platform.eq_ignore_ascii_case("YT") || platform.eq_ignore_ascii_case("TW") {
        let channel_id = match check_channel(platform, channel_name) {
            Ok(id) => id,
            Err(e) => {
                println!("Error checking channel: {}", e);
                return;
            }
        };

        if channel_id.is_empty() {
            println!(
                "Channel {} not found in allowed list for {}",
                channel_name, platform
            );
            return;
        }

        let live_status = match check_live_status(platform, &channel_id).await {
            Ok(status) => status,
            Err(e) => {
                println!("Error checking live status: {}", e);
                return;
            }
        };

        if !live_status.contains("Not Live") {
            println!("area_id: {}", area_id);
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
                        println!("Failed to get live title for YT: {}", e);
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
                        println!("Failed to get live title for TW: {}", e);
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
                println!("Failed to update config: {}", e);
                return;
            }

            println!(
                "Updated {} channel: {} ({})",
                platform, channel_name, channel_id
            );

            // Cooldown for 10 seconds
            thread::sleep(Duration::from_secs(10));

            // Restart bilistream service to trigger streaming process
            // You can implement this as needed, for example:
            // Command::new("systemctl").arg("restart").arg("bilistream.service").spawn().expect("Failed to restart bilistream service");
        } else {
            println!(
                "Channel {} ({}) is not live on {}",
                channel_name, channel_id, platform
            );
        }
    } else {
        println!("Unsupported platform: {}", platform);
    }
}

/// Retrieves the room ID from the configuration.
fn get_room_id() -> String {
    match fs::read_to_string("config.json") {
        Ok(content) => match serde_json::from_str::<Value>(&content) {
            Ok(json) => json["roomId"].as_str().unwrap_or("").to_string(),
            Err(e) => {
                eprintln!("Failed to parse JSON: {}", e);
                "".to_string()
            }
        },
        Err(e) => {
            eprintln!("Failed to read config.json: {}", e);
            "".to_string()
        }
    }
}

/// Main function to execute danmaku processing.
pub fn run_danmaku(platform: &str) {
    // Check if any danmaku is already running
    if is_any_danmaku_running() {
        println!("A danmaku instance is already running. Skipping new instance.");
        return;
    }

    // Create the lock file for the specified platform
    if let Err(e) = create_danmaku_lock(platform) {
        println!("Failed to create danmaku lock file: {}", e);
        return;
    }

    println!("Starting danmaku processing in bilistream-{}", platform);

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

    println!("danmaku-cli has been executed for platform: {}", platform);

    // Monitor Bilibili live status every 300 seconds
    loop {
        thread::sleep(Duration::from_secs(300));

        let room_id = get_room_id();

        if room_id.is_empty() {
            println!("Failed to retrieve room ID from config.json");
            continue;
        }

        println!("Room ID: {}", room_id);
        let bilibili_status = match Command::new("./bilistream")
            .arg("get-live-status")
            .arg("bilibili")
            .arg(room_id)
            .output()
        {
            Ok(output) => String::from_utf8_lossy(&output.stdout).to_string(),
            Err(e) => {
                println!("Error checking Bilibili status: {}", e);
                continue;
            }
        };

        if bilibili_status.contains("Not Live") {
            println!("Bilibili is not live. Continuing danmaku-cli...");
        } else {
            println!("Bilibili is live. Stopping danmaku-cli...");
            // Kill danmaku-cli process
            Command::new("pkill")
                .arg("-f")
                .arg("danmaku-cli")
                .output()
                .expect("Failed to stop danmaku-cli");

            // Remove all danmaku lock files
            if let Err(e) = remove_danmaku_lock("YT") {
                println!("Failed to remove danmaku.lock-YT: {}", e);
            }
            if let Err(e) = remove_danmaku_lock("TW") {
                println!("Failed to remove danmaku.lock-TW: {}", e);
            }

            break;
        }
    }
}
