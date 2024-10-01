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
    async fn get_status(
        &self,
    ) -> Result<(bool, Option<String>, Option<DateTime<Utc>>), Box<dyn Error>>;
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
        "Youtube" => Ok(Box::new(Youtube::new(
            &cfg.youtube.channel_name.as_str(),
            &cfg.youtube.channel_id.as_str(),
        ))),

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
) -> Result<(bool, Option<String>, Option<DateTime<Utc>>), Box<dyn Error>> {
    let mut command = Command::new("yt-dlp");
    command.arg("-g");

    command.arg(format!(
        "https://www.youtube.com/channel/{}/live",
        channel_id
    ));

    let output = command.output()?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    // tracing::info!("yt-dlp -g {}", stdout);
    if stdout.contains("ERROR: [youtube]") {
        // Check for scheduled start time in stderr
        if let Some(captures) =
            Regex::new(r"This live event will begin in (\d+) minutes")?.captures(&stderr)
        {
            let minutes: i64 = captures[1].parse()?;
            let start_time = Utc::now() + chrono::Duration::minutes(minutes);
            return Ok((false, None, Some(start_time))); // Return scheduled start time
        }
        if let Some(captures) =
            Regex::new(r"This live event will begin in (\d+) hours")?.captures(&stderr)
        {
            let hours: i64 = captures[1].parse()?;
            let start_time = Utc::now() + chrono::Duration::hours(hours);
            return Ok((false, None, Some(start_time))); // Return scheduled start time
        }
        if let Some(captures) =
            Regex::new(r"This live event will begin in (\d+) days")?.captures(&stderr)
        {
            let days: i64 = captures[1].parse()?;
            let start_time = Utc::now() + chrono::Duration::days(days);
            return Ok((false, None, Some(start_time))); // Return scheduled start time
        }
        return Ok((false, None, None)); // Channel is not live and no scheduled time
    } else if Regex::new(r"https://.*\.m3u8").unwrap().is_match(&stdout) {
        return Ok((true, Some(stdout.to_string()), None)); // Channel is currently live
    }

    Err("Unexpected output from yt-dlp".into())
}
