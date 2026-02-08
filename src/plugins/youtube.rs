use super::danmaku::get_channel_name;
use crate::config::load_config;
use chrono::{DateTime, Local};
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::error::Error; // Ensure this is included
use std::process::Command;

// Holodex API data structures
#[derive(Serialize, Deserialize, Debug)]
pub struct HolodexStream {
    pub id: String,
    pub title: String,
    #[serde(rename = "type")]
    pub stream_type: String,
    pub topic_id: Option<String>,
    pub published_at: Option<String>,
    pub available_at: Option<String>,
    pub status: String,
    pub start_scheduled: Option<String>,
    pub start_actual: Option<String>,
    pub live_viewers: Option<i32>,
    #[serde(default)]
    pub channel: HolodexChannel,
}

#[derive(Serialize, Deserialize, Debug, Default)]
pub struct HolodexChannel {
    pub id: String,
    #[serde(default)]
    pub name: String,
}

// Helper function to get yt-dlp command path
fn get_yt_dlp_command() -> String {
    if cfg!(target_os = "windows") {
        // On Windows, check if yt-dlp.exe exists in the executable directory
        if let Ok(exe_path) = std::env::current_exe() {
            if let Some(exe_dir) = exe_path.parent() {
                let local_yt_dlp = exe_dir.join("yt-dlp.exe");
                if local_yt_dlp.exists() {
                    return local_yt_dlp.to_string_lossy().to_string();
                }
            }
        }
        "yt-dlp.exe".to_string()
    } else {
        "yt-dlp".to_string()
    }
}

// Helper function to create a Command with hidden console on Windows
fn create_hidden_command(program: &str) -> Command {
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        let mut command = Command::new(program);
        // Hide the console window
        command.creation_flags(0x08000000); // CREATE_NO_WINDOW
        command
    }

    #[cfg(not(target_os = "windows"))]
    {
        Command::new(program)
    }
}

// Helper function to add cookies arguments to yt-dlp command
fn add_cookies_args(
    command: &mut Command,
    cookies_file: &Option<String>,
    cookies_from_browser: &Option<String>,
) {
    if let Some(browser) = cookies_from_browser {
        if !browser.is_empty() {
            command.arg("--cookies-from-browser");
            command.arg(browser);
            return;
        }
    }

    if let Some(file_path) = cookies_file {
        if !file_path.is_empty() {
            command.arg("--cookies");
            command.arg(file_path);
        }
    }
}

pub struct Youtube {
    pub channel_name: String,
    pub channel_id: String,
    pub proxy: Option<String>,
}
impl Youtube {
    pub fn new(channel_name: &str, channel_id: &str, proxy: Option<String>) -> Self {
        Youtube {
            channel_name: channel_name.to_string(),
            channel_id: channel_id.to_string(),
            proxy,
        }
    }

    pub async fn get_status(
        &self,
    ) -> Result<
        (
            bool,                    // is_live
            Option<String>,          // topic
            Option<String>,          // title
            Option<String>,          // m3u8_url
            Option<DateTime<Local>>, // start_time
            Option<String>,          // video_id
        ),
        Box<dyn Error>,
    > {
        Ok(get_youtube_status(&self.channel_id).await?)
    }
}

// Get Holodex streams for multiple channels
pub async fn get_holodex_streams(
    channel_ids: Vec<String>,
) -> Result<Vec<HolodexStream>, Box<dyn Error>> {
    let cfg = load_config().await?;

    // Check if Holodex API key is configured
    let api_key = match cfg.holodex_api_key {
        Some(key) if !key.is_empty() => key,
        _ => return Err("Holodex API key not configured".into()),
    };

    if channel_ids.is_empty() {
        return Err("No channel IDs provided".into());
    }

    // Call Holodex API
    let channels_param = channel_ids.join(",");
    let url = format!(
        "https://holodex.net/api/v2/users/live?channels={}",
        channels_param
    );

    let client = reqwest::Client::new();
    let response = client.get(&url).header("X-APIKEY", api_key).send().await?;

    if !response.status().is_success() {
        return Err(format!("Holodex API error: {}", response.status()).into());
    }

    let streams: Vec<HolodexStream> = response.json().await?;
    Ok(streams)
}

pub async fn get_youtube_status(
    channel_id: &str,
) -> Result<
    (
        bool,                    // is_live
        Option<String>,          // topic
        Option<String>,          // title
        Option<String>,          // m3u8_url
        Option<DateTime<Local>>, // start_time
        Option<String>,          // video_id
    ),
    Box<dyn Error>,
> {
    let cfg = load_config().await?;
    let proxy = cfg.youtube.proxy.clone();
    let quality = cfg.youtube.quality.clone();
    let cookies_file = &cfg.youtube.cookies_file;
    let cookies_from_browser = &cfg.youtube.cookies_from_browser;

    // Check if Holodex API key is available
    match cfg.holodex_api_key.clone() {
        Some(_key) if !_key.is_empty() => {}
        _ => {
            tracing::info!("Holodex API key not configured, using yt-dlp");
            let title = get_youtube_live_title(channel_id).await?;
            return get_status_with_yt_dlp(
                channel_id,
                proxy,
                title,
                Some(&quality),
                cookies_file,
                cookies_from_browser,
            )
            .await;
        }
    };

    // Use the multi-channel function for single channel
    match get_holodex_streams(vec![channel_id.to_string()]).await {
        Ok(streams) => {
            // If streams is empty, it means the API worked but there are no live/scheduled streams
            if streams.is_empty() {
                // tracing::info!(
                //     "No live or scheduled streams found for channel {} in Holodex",
                //     channel_id
                // );
                return Ok((false, None, None, None, None, None));
            }

            // Find streams for this specific channel, prioritizing live over upcoming
            let channel_streams: Vec<_> = streams
                .iter()
                .filter(|s| s.channel.id == channel_id)
                .collect();

            if channel_streams.is_empty() {
                return Ok((false, None, None, None, None, None));
            }

            // First try to find a live stream
            if let Some(live_stream) = channel_streams.iter().find(|s| s.status == "live") {
                let topic = live_stream.topic_id.clone();
                let title = Some(live_stream.title.clone());
                let video_id = Some(live_stream.id.clone());

                let (is_live, _, _, m3u8_url, _, _) = get_status_with_yt_dlp(
                    channel_id,
                    proxy.clone(),
                    title.clone(),
                    Some(&quality),
                    cookies_file,
                    cookies_from_browser,
                )
                .await?;
                return Ok((is_live, topic, title, m3u8_url, None, video_id));
            }

            // No live stream found, check for upcoming streams
            // Filter and sort upcoming streams by scheduled time (earliest first)
            // Also filter out scheduled streams more than 30 hours in the future
            let now = chrono::Utc::now();
            let thirty_hours_later = now + chrono::Duration::hours(30);

            let mut upcoming_streams: Vec<_> = channel_streams
                .iter()
                .filter(|s| {
                    if s.status != "upcoming" {
                        return false;
                    }

                    // Filter by time (within 30 hours)
                    if let Some(ref scheduled_time) = s.start_scheduled {
                        if let Ok(scheduled) = chrono::DateTime::parse_from_rfc3339(scheduled_time)
                        {
                            let scheduled_utc = scheduled.with_timezone(&chrono::Utc);
                            // Only keep if scheduled within next 30 hours
                            return scheduled_utc <= thirty_hours_later;
                        }
                    }
                    // If we can't parse the time, keep it to be safe
                    true
                })
                .collect();

            if !upcoming_streams.is_empty() {
                // Sort by scheduled time (earliest first)
                upcoming_streams.sort_by(|a, b| match (&a.start_scheduled, &b.start_scheduled) {
                    (Some(time_a), Some(time_b)) => time_a.cmp(time_b),
                    (Some(_), None) => std::cmp::Ordering::Less,
                    (None, Some(_)) => std::cmp::Ordering::Greater,
                    (None, None) => std::cmp::Ordering::Equal,
                });

                // Pick the earliest scheduled stream
                let upcoming_stream = upcoming_streams[0];
                let topic = upcoming_stream.topic_id.clone();
                let title = Some(upcoming_stream.title.clone());
                let video_id = Some(upcoming_stream.id.clone());

                let start_time = upcoming_stream.start_scheduled.as_ref().and_then(|t| {
                    DateTime::parse_from_rfc3339(t)
                        .ok()
                        .map(|dt| dt.with_timezone(&Local))
                });

                return Ok((false, topic, title, None, start_time, video_id));
            }

            // No live or upcoming streams found
            Ok((false, None, None, None, None, None))
        }
        Err(e) => {
            tracing::error!("Holodex API failed: {}, using yt-dlp", e);
            let title = get_youtube_live_title(channel_id).await?;
            let (is_live, _, _, m3u8_url, start_time, video_id) = get_status_with_yt_dlp(
                channel_id,
                proxy,
                None,
                Some(&quality),
                cookies_file,
                cookies_from_browser,
            )
            .await?;
            Ok((is_live, None, title, m3u8_url, start_time, video_id))
        }
    }
}

// Update get_status_with_yt_dlp to match the new order
async fn get_status_with_yt_dlp(
    channel_id: &str,
    proxy: Option<String>,
    title: Option<String>,
    quality: Option<&str>,
    cookies_file: &Option<String>,
    cookies_from_browser: &Option<String>,
) -> Result<
    (
        bool,                    // is_live
        Option<String>,          // topic
        Option<String>,          // title
        Option<String>,          // m3u8_url
        Option<DateTime<Local>>, // start_time
        Option<String>,          // video_id
    ),
    Box<dyn Error>,
> {
    let quality = quality.unwrap_or("best");

    let mut command = create_hidden_command(&get_yt_dlp_command());
    if let Some(proxy) = proxy.clone() {
        command.arg("--proxy");
        command.arg(proxy);
    }

    // Add cookies arguments
    add_cookies_args(&mut command, cookies_file, cookies_from_browser);

    command.arg("-f");
    command.arg(quality);
    command.arg("--print").arg("id");
    command.arg("-g");

    command.arg(format!(
        "https://www.youtube.com/channel/{}/live",
        channel_id
    ));
    let output = command.output()?;
    // println!("{:?}", output);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    // Extract video ID from stdout (first line when using --print id)
    let lines: Vec<&str> = stdout.lines().collect();
    let video_id = if lines.len() >= 2 {
        Some(lines[0].to_string()) // First line is the video ID
    } else {
        None
    };

    if stderr.contains("ERROR: [youtube") {
        // Check for scheduled start time in stderr
        if let Some(captures) =
            Regex::new(r"This live event will begin in (\d+) minutes")?.captures(&stderr)
        {
            let minutes: i64 = captures[1].parse()?;
            let start_time = chrono::Local::now() + chrono::Duration::minutes(minutes);
            return Ok((false, None, title, None, Some(start_time), video_id));
        }
        if let Some(captures) =
            Regex::new(r"This live event will begin in (\d+) hours")?.captures(&stderr)
        {
            let hours: i64 = captures[1].parse()?;
            let start_time = chrono::Local::now() + chrono::Duration::hours(hours);
            let title = if title.is_some() {
                title
            } else {
                get_youtube_live_title(channel_id).await?
            };
            return Ok((false, None, title, None, Some(start_time), video_id)); // Return scheduled start time
        }
        if let Some(captures) =
            Regex::new(r"This live event will begin in (\d+) days")?.captures(&stderr)
        {
            let days: i64 = captures[1].parse()?;
            let start_time = chrono::Local::now() + chrono::Duration::days(days);
            let title = if title.is_some() {
                title
            } else {
                get_youtube_live_title(channel_id).await?
            };
            return Ok((false, None, title, None, Some(start_time), video_id)); // Return scheduled start time
        }
        return Ok((false, None, None, None, None, video_id)); // Channel is not live and no scheduled time
    } else if Regex::new(r"https://.*\.m3u8")?.is_match(&stdout) {
        let regex = Regex::new(r"(https://.*\.m3u8.*)")?;
        let matches: Vec<&str> = regex.find_iter(&stdout).map(|m| m.as_str()).collect();

        if matches.len() > 1 {
            tracing::warn!(
                "Multiple m3u8 URLs found (likely separate video and audio streams): {} URLs",
                matches.len()
            );
            tracing::warn!("Using first URL: {}", matches[0]);
        }

        let m3u8_url = matches[0].to_string();
        return Ok((true, None, title, Some(m3u8_url), None, video_id));
    }

    Err("Unexpected output from yt-dlp".into())
}

pub async fn get_youtube_live_title(channel_id: &str) -> Result<Option<String>, Box<dyn Error>> {
    let cfg = load_config().await?;
    let proxy = cfg.youtube.proxy.clone();
    let cookies_file = &cfg.youtube.cookies_file;
    let cookies_from_browser = &cfg.youtube.cookies_from_browser;
    let channel_name = get_channel_name("YT", channel_id).unwrap();

    // Helper function to get title using yt-dlp
    let get_title_with_ytdlp = || -> Result<Option<String>, Box<dyn Error>> {
        let mut command = create_hidden_command(&get_yt_dlp_command());
        if let Some(ref p) = proxy {
            command.arg("--proxy").arg(p);
        }
        add_cookies_args(&mut command, cookies_file, cookies_from_browser);
        command.arg("-e").arg(format!(
            "https://www.youtube.com/channel/{}/live",
            channel_id
        ));

        let output = command.output()?;
        let title_str = String::from_utf8_lossy(&output.stdout);

        let title = title_str
            .lines()
            .filter(|line| {
                !line.trim().is_empty()
                    && !line.starts_with("WARNING")
                    && !line.starts_with("ERROR")
            })
            .last()
            .map(|line| {
                let re = regex::Regex::new(r"\s+\d{4}-\d{2}-\d{2}\s+\d{2}:\d{2}$").unwrap();
                re.replace(line, "").trim().to_string()
            })
            .filter(|s| !s.is_empty());

        Ok(title)
    };

    // Try Holodex API if key is configured
    if let Some(key) = cfg.holodex_api_key.clone().filter(|k| !k.is_empty()) {
        let client = reqwest::Client::new();
        let url = format!(
            "https://holodex.net/api/v2/users/live?channels={}",
            channel_id
        );

        match client.get(&url).header("X-APIKEY", key).send().await {
            Ok(response) if response.status().is_success() => {
                if let Ok(videos) = response.json::<Vec<serde_json::Value>>().await {
                    if !videos.is_empty() {
                        for video in videos.iter().rev() {
                            if let Some(cname) = video
                                .get("channel")
                                .and_then(|c| c.get("name"))
                                .and_then(|n| n.as_str())
                            {
                                if cname
                                    .replace(" ", "")
                                    .contains(channel_name.as_deref().unwrap_or(""))
                                {
                                    if let Some(topic_id) =
                                        video.get("topic_id").and_then(|t| t.as_str())
                                    {
                                        if topic_id.contains("membersonly") {
                                            continue;
                                        }
                                    }
                                    let title = video
                                        .get("title")
                                        .and_then(|t| t.as_str())
                                        .map(|s| s.to_string());
                                    return Ok(title);
                                }
                            }
                        }
                    }
                }
                return Ok(None);
            }
            _ => {
                tracing::warn!("Holodex API failed, falling back to yt-dlp");
            }
        }
    } else {
        tracing::info!("Holodex API key not configured, using yt-dlp");
    }

    // Fallback to yt-dlp
    get_title_with_ytdlp()
}
