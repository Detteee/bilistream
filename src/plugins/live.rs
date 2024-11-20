use super::{Twitch, Youtube};
use crate::config::Config;
use crate::load_config;
use async_trait::async_trait;
use chrono::{DateTime, Local}; // Add this import
use regex::Regex;
use reqwest_middleware::ClientBuilder;
use reqwest_retry::policies::ExponentialBackoff;
use reqwest_retry::RetryTransientMiddleware;
use std::error::Error;
use std::path::Path;
use std::process::Command;
use std::time::Duration;
#[async_trait]
pub trait Live {
    async fn get_title(&self) -> Result<String, Box<dyn Error>>;
    // fn channel_name(&self) -> &str;
    async fn get_status(
        &self,
    ) -> Result<(bool, Option<String>, Option<DateTime<Local>>), Box<dyn Error>>;
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
            cfg.proxy,
        ))),

        "Twitch" => Ok(Box::new(Twitch::new(
            &cfg.twitch.channel_id.as_str(),
            cfg.twitch.oauth_token,
            client.clone(),
            cfg.twitch.proxy_region,
        ))),
        _ => Err("不支持的平台".into()),
    }
}

pub async fn get_youtube_live_status(
    channel_id: &str,
    proxy: Option<String>,
) -> Result<(bool, Option<String>, Option<DateTime<Local>>), Box<dyn Error>> {
    let client = reqwest::Client::new();
    let url = format!(
        "https://holodex.net/api/v2/users/live?channels={}",
        channel_id
    );
    let cfg = load_config(Path::new("YT/config.yaml"), Path::new("cookies.json"))?;
    let response = client
        .get(&url)
        .header("X-APIKEY", cfg.holodex_api_key.clone().unwrap())
        .send()
        .await?;
    if response.status().is_success() {
        let videos: Vec<serde_json::Value> = response.json().await?;
        if let Some(video) = videos.last() {
            let status = video.get("status").unwrap();
            if status == "upcoming" {
                let start_time_str = video
                    .get("start_scheduled")
                    .and_then(|v| v.as_str())
                    .ok_or("start_scheduled 不存在")?;
                // 将时间字符串转换为DateTime<Local>
                let start_time =
                    DateTime::parse_from_rfc3339(&start_time_str)?.with_timezone(&Local);
                // println!("计划开始时间: {}", start_time);
                return Ok((false, None, Some(start_time)));
            } else if status == "live" {
                return get_status_with_yt_dlp(channel_id, proxy);
            } else {
                tracing::info!("Holodex获取直播状态失败，使用yt-dlp获取");
                return get_status_with_yt_dlp(channel_id, proxy);
            }
        } else {
            return Ok((false, None, None));
        }
    } else {
        tracing::info!("Holodex获取直播状态失败，使用yt-dlp获取");
        return get_status_with_yt_dlp(channel_id, proxy);
    }
}

fn get_status_with_yt_dlp(
    channel_id: &str,
    proxy: Option<String>,
) -> Result<(bool, Option<String>, Option<DateTime<Local>>), Box<dyn Error>> {
    let mut command = Command::new("yt-dlp");
    if let Some(proxy) = proxy {
        command.arg("--proxy");
        command.arg(proxy);
    }
    command.arg("-g");

    command.arg(format!(
        "https://www.youtube.com/channel/{}/live",
        channel_id
    ));
    let output = command.output()?;
    println!("{:?}", output);
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
            return Ok((false, None, Some(start_time))); // Return scheduled start time
        }
        if let Some(captures) =
            Regex::new(r"This live event will begin in (\d+) hours")?.captures(&stderr)
        {
            let hours: i64 = captures[1].parse()?;
            let start_time = chrono::Local::now() + chrono::Duration::hours(hours);
            return Ok((false, None, Some(start_time))); // Return scheduled start time
        }
        if let Some(captures) =
            Regex::new(r"This live event will begin in (\d+) days")?.captures(&stderr)
        {
            let days: i64 = captures[1].parse()?;
            let start_time = chrono::Local::now() + chrono::Duration::days(days);
            return Ok((false, None, Some(start_time))); // Return scheduled start time
        }
        return Ok((false, None, None)); // Channel is not live and no scheduled time
    } else if Regex::new(r"https://.*\.m3u8").unwrap().is_match(&stdout) {
        return Ok((true, Some(stdout.to_string()), None)); // Channel is currently live
    }

    Err("Unexpected output from yt-dlp".into())
}
