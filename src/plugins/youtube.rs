use super::danmaku::get_channel_name;
use super::twitch::get_twitch_status;
use super::Live;
use crate::config::load_config;
use async_trait::async_trait;
use chrono::{DateTime, Local};
use regex::Regex;
use std::error::Error; // Ensure this is included
use std::process::Command;
pub struct Youtube {
    pub channel_name: String,
    pub channel_id: String,
    pub proxy: Option<String>,
}
#[async_trait]
impl Live for Youtube {
    async fn get_status(
        &self,
    ) -> Result<
        (
            bool,                    // is_live
            Option<String>,          // topic
            Option<String>,          // title
            Option<String>,          // m3u8_url
            Option<DateTime<Local>>, // start_time
        ),
        Box<dyn Error>,
    > {
        Ok(get_youtube_status(&self.channel_id).await?)
    }
}

impl Youtube {
    pub fn new(channel_name: &str, channel_id: &str, proxy: Option<String>) -> impl Live {
        Youtube {
            channel_name: channel_name.to_string(),
            channel_id: channel_id.to_string(),
            proxy,
        }
    }
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
    ),
    Box<dyn Error>,
> {
    let client = reqwest::Client::new();
    let cfg = load_config().await?;
    let proxy = cfg.proxy.clone();
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
    if !response.status().is_success() {
        tracing::error!("Holodex获取直播状态失败，使用yt-dlp获取");
        return get_status_with_yt_dlp(channel_id, proxy, None).await;
    }

    let videos: Vec<serde_json::Value> = response.json().await?;
    if videos.is_empty() {
        return get_status_with_yt_dlp(channel_id, proxy, None).await;
    }

    for video in videos.iter().rev() {
        if let Some(cname) = video.get("channel") {
            if cname
                .get("name")
                .unwrap()
                .as_str()
                .unwrap()
                .replace(" ", "")
                .contains(channel_name.as_deref().unwrap_or(""))
            {
                let status = video
                    .get("status")
                    .and_then(|s| s.as_str())
                    .unwrap_or("none");
                let topic = video
                    .get("topic_id")
                    .and_then(|t| t.as_str())
                    .map(|s| s.to_string());

                let title = video
                    .get("title")
                    .and_then(|t| t.as_str())
                    .map(|s| s.to_string());

                if status == "live" {
                    let tw_channel_name =
                        get_channel_name("TW", channel_name.as_deref().unwrap()).unwrap();
                    if tw_channel_name.is_some() {
                        let (is_tw_live, _, _) =
                            get_twitch_status(tw_channel_name.as_deref().unwrap()).await?;
                        if is_tw_live {
                            return Ok((false, None, None, None, None));
                        }
                    } else {
                        let (is_live, _, _, m3u8_url, _) =
                            get_status_with_yt_dlp(channel_id, proxy, title.clone()).await?;
                        return Ok((is_live, topic, title, m3u8_url, None));
                    }
                }

                let start_time = if status == "upcoming" {
                    video
                        .get("start_scheduled")
                        .and_then(|v| v.as_str())
                        .map(|t| {
                            DateTime::parse_from_rfc3339(t)
                                .unwrap()
                                .with_timezone(&Local)
                        })
                } else {
                    None
                };

                return Ok((false, topic, title, None, start_time));
            }
        }
    }
    let title = get_youtube_live_title(channel_id).await?;
    let (is_live, _, _, m3u8_url, start_time) =
        get_status_with_yt_dlp(channel_id, proxy, None).await?;
    Ok((is_live, None, title, m3u8_url, start_time))
}

// Update get_status_with_yt_dlp to match the new order
async fn get_status_with_yt_dlp(
    channel_id: &str,
    proxy: Option<String>,
    title: Option<String>,
) -> Result<
    (
        bool,                    // is_live
        Option<String>,          // topic
        Option<String>,          // title
        Option<String>,          // m3u8_url
        Option<DateTime<Local>>, // start_time
    ),
    Box<dyn Error>,
> {
    let mut command = Command::new("yt-dlp");
    if let Some(proxy) = proxy.clone() {
        command.arg("--proxy");
        command.arg(proxy);
    }
    command.arg("-g");

    command.arg(format!(
        "https://www.youtube.com/channel/{}/live",
        channel_id
    ));
    let output = command.output()?;
    // println!("{:?}", output);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    if stderr.contains("ERROR: [youtube") {
        // Check for scheduled start time in stderr
        if let Some(captures) =
            Regex::new(r"This live event will begin in (\d+) minutes")?.captures(&stderr)
        {
            let minutes: i64 = captures[1].parse()?;
            let start_time = chrono::Local::now() + chrono::Duration::minutes(minutes);
            return Ok((false, None, title, None, Some(start_time)));
        }
        if let Some(captures) =
            Regex::new(r"This live event will begin in (\d+) hours")?.captures(&stderr)
        {
            let hours: i64 = captures[1].parse()?;
            let start_time = chrono::Local::now() + chrono::Duration::hours(hours);
            if title.is_some() {
                return Ok((false, None, title, None, Some(start_time))); // Return scheduled start time
            } else {
                let title = get_youtube_live_title(channel_id).await?;
                return Ok((false, None, title, None, Some(start_time))); // Return scheduled start time
            }
        }
        if let Some(captures) =
            Regex::new(r"This live event will begin in (\d+) days")?.captures(&stderr)
        {
            let days: i64 = captures[1].parse()?;
            let start_time = chrono::Local::now() + chrono::Duration::days(days);
            if title.is_some() {
                return Ok((false, None, title, None, Some(start_time))); // Return scheduled start time
            } else {
                let title = get_youtube_live_title(channel_id).await?;
                return Ok((false, None, title, None, Some(start_time))); // Return scheduled start time
            }
        }
        return Ok((false, None, None, None, None)); // Channel is not live and no scheduled time
    } else if Regex::new(r"https://.*\.m3u8").unwrap().is_match(&stdout) {
        return Ok((true, None, title, Some(stdout.to_string()), None));
    }

    Err("Unexpected output from yt-dlp".into())
}

pub async fn get_youtube_live_title(channel_id: &str) -> Result<Option<String>, Box<dyn Error>> {
    let cfg = load_config().await?;
    let proxy = cfg.proxy.clone();
    let channel_name = get_channel_name("YT", channel_id).unwrap();
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
        let mut command = Command::new("yt-dlp");
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
