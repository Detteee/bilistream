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
    let proxy = cfg.proxy.clone();
    let quality = cfg.youtube.quality.clone();

    // Check if Holodex API key is available
    match cfg.holodex_api_key.clone() {
        Some(_key) if !_key.is_empty() => {}
        _ => {
            tracing::info!("Holodex API key not configured, using yt-dlp");
            return get_status_with_yt_dlp(channel_id, proxy, None, Some(&quality)).await;
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

            // Find the stream for this specific channel
            for stream in streams.iter().rev() {
                if stream.channel.id == channel_id {
                    let status = &stream.status;
                    let topic = stream.topic_id.clone();
                    let title = Some(stream.title.clone());
                    let video_id = Some(stream.id.clone());

                    if status == "live" {
                        let (is_live, _, _, m3u8_url, _, _) = get_status_with_yt_dlp(
                            channel_id,
                            proxy,
                            title.clone(),
                            Some(&quality),
                        )
                        .await?;
                        return Ok((is_live, topic, title, m3u8_url, None, video_id));
                    }

                    let start_time = if status == "upcoming" {
                        stream.start_scheduled.as_ref().and_then(|t| {
                            DateTime::parse_from_rfc3339(t)
                                .ok()
                                .map(|dt| dt.with_timezone(&Local))
                        })
                    } else {
                        None
                    };

                    return Ok((false, topic, title, None, start_time, video_id));
                }
            }

            // If streams exist but none match our channel ID, no live/scheduled streams for this channel
            // tracing::info!(
            //     "No streams found for channel {} in Holodex response",
            //     channel_id
            // );
            Ok((false, None, None, None, None, None))
        }
        Err(e) => {
            tracing::error!("Holodex API failed: {}, using yt-dlp", e);
            let title = get_youtube_live_title(channel_id).await?;
            let (is_live, _, _, m3u8_url, start_time, video_id) =
                get_status_with_yt_dlp(channel_id, proxy, None, Some(&quality)).await?;
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
    let proxy = cfg.proxy.clone();
    let channel_name = get_channel_name("YT", channel_id).unwrap();
    let client = reqwest::Client::new();

    // Check if Holodex API key is available
    let holodex_api_key = match cfg.holodex_api_key.clone() {
        Some(key) if !key.is_empty() => key,
        _ => {
            // Fallback to yt-dlp for title
            let mut command = create_hidden_command(&get_yt_dlp_command());
            if let Some(proxy) = proxy {
                command.arg("--proxy").arg(proxy);
            }
            command.arg("-e");
            command.arg(format!(
                "https://www.youtube.com/channel/{}/live",
                channel_id
            ));
            let output = command.output()?;
            let title_str = String::from_utf8_lossy(&output.stdout);
            if let Some(title) = title_str.split(" 202").next() {
                return Ok(Some(title.to_string()));
            } else {
                return Ok(Some("空".to_string()));
            }
        }
    };

    let url = format!(
        "https://holodex.net/api/v2/users/live?channels={}",
        channel_id
    );
    let response = client
        .get(&url)
        .header("X-APIKEY", holodex_api_key)
        .send()
        .await?;
    if response.status().is_success() {
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
                    .contains(channel_name.as_deref().unwrap_or(""))
                {
                    if let Some(topic_id) = video.get("topic_id") {
                        if topic_id.as_str().unwrap().contains("membersonly") {
                            // tracing::info!("频道 {} 正在进行会限直播", channel_name);
                        } else {
                            vid = video;
                            flag = true;
                            break;
                        }
                    } else {
                        vid = video;
                        flag = true;
                        break;
                    }
                    // let live_topic = video.get("topic_id").unwrap();
                    // if !live_topic.as_str().unwrap().contains("membersonly") {
                    //     vid = video;
                    //     flag = true;
                    //     break;
                    // }
                }
            }
            if flag {
                let title = vid
                    .get("title")
                    .and_then(|t| t.as_str())
                    .map(|s| s.split(" 202").next().unwrap_or(s).to_string());
                return Ok(title);
            } else {
                return Ok(None);
            }
        } else {
            Ok(None)
        }
    } else {
        let mut command = create_hidden_command(&get_yt_dlp_command());
        if let Some(proxy) = proxy {
            command.arg("--proxy").arg(proxy);
        }
        command.arg("-e");
        command.arg(format!(
            "https://www.youtube.com/channel/{}/live",
            channel_id
        ));
        let output = command.output()?;
        let title_str = String::from_utf8_lossy(&output.stdout);
        if let Some(title) = title_str.split(" 202").next() {
            Ok(Some(title.to_string()))
        } else {
            Ok(Some("空".to_string()))
        }
    }
}
