use super::{Twitch, Youtube};
use crate::config::Config;
use async_trait::async_trait;
use chrono::{DateTime, Utc}; // Add this import
use regex::Regex;
use reqwest_middleware::ClientBuilder;
use reqwest_retry::policies::ExponentialBackoff;
use reqwest_retry::RetryTransientMiddleware;
use std::error::Error;
use std::process::Command;
use std::time::Duration;

#[allow(dead_code)]
/// Status of the live stream
pub enum Status {
    /// Stream is online.
    Online,
    /// Stream is offline.
    Offline,
    /// The status of the stream could not be determined.
    Unknown,
}

#[async_trait]
pub trait Live {
    fn channel_name(&self) -> &str;
    async fn get_status(&self) -> Result<(bool, Option<DateTime<Utc>>), Box<dyn Error>>;
    async fn get_real_m3u8_url(&self) -> Result<String, Box<dyn Error>>;
    // fn set_room(&mut self, room: &str);
}

pub async fn select_live(cfg: Config) -> Result<Box<dyn Live>, Box<dyn Error>> {
    // 设置最大重试次数为4294967295次
    let retry_policy = ExponentialBackoff::builder().build_with_max_retries(4294967295);
    let raw_client = reqwest::Client::builder()
        .cookie_store(true)
        // 设置超时时间为30秒
        .timeout(Duration::new(30, 0))
        .build()
        .unwrap();
    let client = ClientBuilder::new(raw_client.clone())
        .with(RetryTransientMiddleware::new_with_policy(retry_policy))
        .build();
    match cfg.platform.as_str() {
        "Youtube" => {
            let channel_name = cfg.youtube.channel_name.as_str();
            let channel_id = cfg.youtube.channel_id.as_str(); // Use room as channel_id
            Ok(Box::new(Youtube::new(channel_name, channel_id))) // Create Youtube instance
        }
        "Twitch" => Ok(Box::new(Twitch::new(
            &cfg.twitch.channel_id.as_str(),
            cfg.twitch.oauth_token,
            client.clone(),
        ))),
        _ => Err("unknown platform".into()),
    }
}

pub async fn get_youtube_live_status(
    channel_id: &str,
) -> Result<(bool, Option<DateTime<Utc>>), Box<dyn Error>> {
    let output = Command::new("yt-dlp")
        .arg("-g")
        .arg(format!("https://www.youtube.com/@{}/live", channel_id))
        .output()?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    if stderr.contains("The channel is not currently live") {
        // Check for scheduled start time in stderr
        if let Some(captures) =
            Regex::new(r"This live event will begin in (\d+) minutes")?.captures(&stderr)
        {
            let minutes: i64 = captures[1].parse()?;
            let start_time = Utc::now() + chrono::Duration::minutes(minutes);
            return Ok((false, Some(start_time))); // Return scheduled start time
        }
        return Ok((false, None)); // Channel is not live and no scheduled time
    } else if stdout.contains("https://") {
        return Ok((true, None)); // Channel is currently live
    }

    Err("Unexpected output from yt-dlp".into())
}
