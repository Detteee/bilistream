use async_trait::async_trait;
// use reqwest_middleware::ClientWithMiddleware;
use super::Live;
use crate::config::load_config;
use chrono::{DateTime, Local};
use regex::Regex;
use std::error::Error; // Ensure this is included
use std::path::Path;
use std::process::Command;
pub struct Youtube {
    pub channel_name: String,
    pub channel_id: String,
    pub proxy: Option<String>,
    // pub access_token: String,
    // pub client: ClientWithMiddleware,
}
#[async_trait]
impl Live for Youtube {
    // fn channel_name(&self) -> &str {
    //     &self.channel_name // Return channel_id instead of room
    // }
    async fn get_status(
        &self,
    ) -> Result<
        (
            bool,
            Option<String>,
            Option<String>,
            Option<DateTime<Local>>,
        ),
        Box<dyn Error>,
    > {
        Ok(get_youtube_live_status(&self.channel_id, self.proxy.clone()).await?)
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

pub async fn get_youtube_live_status(
    channel_id: &str,
    proxy: Option<String>,
) -> Result<
    (
        bool,
        Option<String>,
        Option<String>,
        Option<DateTime<Local>>,
    ),
    Box<dyn Error>,
> {
    let client = reqwest::Client::new();
    let url = format!(
        "https://holodex.net/api/v2/users/live?channels={}",
        channel_id
    );
    let cfg = load_config(Path::new("YT/config.yaml"), Path::new("cookies.json"))?;
    let channel_name = &cfg.youtube.channel_name;
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
                // println!("{:?}", cname.unwrap().get("name"));
                if cname
                    .unwrap()
                    .get("name")
                    .unwrap()
                    .as_str()
                    .unwrap()
                    .contains(channel_name)
                {
                    vid = video;
                    flag = true;
                    break;
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
                    let start_time =
                        DateTime::parse_from_rfc3339(&start_time_str)?.with_timezone(&Local);
                    if vid.get("title").is_some() {
                        let title = vid.get("title").unwrap();
                        // println!("计划开始时间: {}", start_time);
                        return Ok((false, None, Some(title.to_string()), Some(start_time)));
                    } else {
                        return Ok((false, None, None, Some(start_time)));
                    }
                } else if status == "live" {
                    if let Some(title) = vid.get("title").and_then(|v| v.as_str()) {
                        // println!("title: {}", title);
                        return get_status_with_yt_dlp(channel_id, proxy, Some(title.to_string()))
                            .await;
                    } else {
                        return get_status_with_yt_dlp(channel_id, proxy, None).await;
                    }
                } else {
                    return Ok((false, None, None, None));
                }
            } else {
                return Ok((false, None, None, None));
            }
        } else {
            return Ok((false, None, None, None));
        }
    } else {
        tracing::error!("Holodex获取直播状态失败，使用yt-dlp获取");
        return get_status_with_yt_dlp(channel_id, proxy, None).await;
    }
}

pub async fn get_youtube_live_title(
    channel_id: &str,
    proxy: Option<String>,
) -> Result<Option<String>, Box<dyn Error>> {
    let cfg = load_config(Path::new("YT/config.yaml"), Path::new("cookies.json"))?;
    let channel_name = &cfg.youtube.channel_name;
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
                // println!("{:?}", cname.unwrap().get("name"));
                if cname
                    .unwrap()
                    .get("name")
                    .unwrap()
                    .as_str()
                    .unwrap()
                    .contains(channel_name)
                {
                    vid = video;
                    flag = true;
                    break;
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
        let live_title = String::from_utf8_lossy(&output.stdout);
        // println!("live_title: {}", live_title);
        Ok(Some(live_title.to_string()))
    }
}

async fn get_status_with_yt_dlp(
    channel_id: &str,
    proxy: Option<String>,
    title: Option<String>,
) -> Result<
    (
        bool,
        Option<String>,
        Option<String>,
        Option<DateTime<Local>>,
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
    // println!("yt-dlp -g {}", stderr);
    if stderr.contains("ERROR: [youtube]") {
        // Check for scheduled start time in stderr
        if let Some(captures) =
            Regex::new(r"This live event will begin in (\d+) minutes")?.captures(&stderr)
        {
            let minutes: i64 = captures[1].parse()?;
            let start_time = chrono::Local::now() + chrono::Duration::minutes(minutes);
            if title.is_some() {
                return Ok((false, None, title, Some(start_time))); // Return scheduled start time
            } else {
                let title = get_youtube_live_title(channel_id, proxy).await?;
                return Ok((false, None, title, Some(start_time))); // Return scheduled start time
            }
        }
        if let Some(captures) =
            Regex::new(r"This live event will begin in (\d+) hours")?.captures(&stderr)
        {
            let hours: i64 = captures[1].parse()?;
            let start_time = chrono::Local::now() + chrono::Duration::hours(hours);
            if title.is_some() {
                return Ok((false, None, title, Some(start_time))); // Return scheduled start time
            } else {
                let title = get_youtube_live_title(channel_id, proxy).await?;
                return Ok((false, None, title, Some(start_time))); // Return scheduled start time
            }
        }
        if let Some(captures) =
            Regex::new(r"This live event will begin in (\d+) days")?.captures(&stderr)
        {
            let days: i64 = captures[1].parse()?;
            let start_time = chrono::Local::now() + chrono::Duration::days(days);
            if title.is_some() {
                return Ok((false, None, title, Some(start_time))); // Return scheduled start time
            } else {
                let title = get_youtube_live_title(channel_id, proxy).await?;
                return Ok((false, None, title, Some(start_time))); // Return scheduled start time
            }
        }
        return Ok((false, None, None, None)); // Channel is not live and no scheduled time
    } else if Regex::new(r"https://.*\.m3u8").unwrap().is_match(&stdout) {
        if title.is_some() {
            return Ok((true, Some(stdout.to_string()), title, None)); // Channel is currently live
        } else {
            let title = get_youtube_live_title(channel_id, proxy).await?;
            return Ok((true, Some(stdout.to_string()), title, None)); // Channel is currently live
        }
    }

    Err("Unexpected output from yt-dlp".into())
}
