use super::{Twitch, Youtube};
use crate::config::Config;
use async_trait::async_trait;
use chrono::{DateTime, Local};
use reqwest_middleware::ClientBuilder;
use reqwest_retry::policies::ExponentialBackoff;
use reqwest_retry::RetryTransientMiddleware;
use std::error::Error;
use std::process::Command;
use std::time::Duration;
use tracing::warn;

#[async_trait]
pub trait Live {
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
    >;
}

pub async fn select_live(cfg: Config, platform: &str) -> Result<Box<dyn Live>, Box<dyn Error>> {
    // 设置最大重试次数为5次
    let retry_policy = ExponentialBackoff::builder().build_with_max_retries(5);
    let raw_client = reqwest::Client::builder()
        .cookie_store(true)
        // 设置超时时间为30秒
        .timeout(Duration::new(30, 0))
        .build()
        .unwrap();
    let client = ClientBuilder::new(raw_client.clone())
        .with(RetryTransientMiddleware::new_with_policy(retry_policy))
        .build();
    match platform {
        "YT" => Ok(Box::new(Youtube::new(
            &cfg.youtube.channel_name.as_str(),
            &cfg.youtube.channel_id.as_str(),
            cfg.proxy,
        ))),

        "TW" => Ok(Box::new(Twitch::new(
            &cfg.twitch.channel_id.as_str(),
            cfg.twitch.oauth_token,
            client.clone(),
            cfg.twitch.proxy_region,
        ))),
        _ => Err("不支持的平台".into()),
    }
}

pub async fn get_thumbnail(
    platform: &str,
    channel_id: &str,
    proxy: Option<String>,
) -> Result<String, Box<dyn Error>> {
    let mut command = Command::new("yt-dlp");

    if let Some(proxy_url) = proxy {
        command.arg("--proxy").arg(proxy_url);
    }

    command
        .arg("--write-thumbnail")
        .arg("--skip-download")
        .arg("--convert-thumbnails")
        .arg("jpg")
        .arg(match platform {
            "YT" => format!("https://www.youtube.com/channel/{}/live", channel_id),
            "TW" => format!("https://www.twitch.tv/{}", channel_id),
            _ => return Err("Unsupported platform".into()),
        })
        .arg("--output")
        .arg("thumbnail");

    let output = match command.output() {
        Ok(output) => output,
        Err(e) => {
            warn!("Failed to execute yt-dlp for thumbnail: {}", e);
            return Ok(String::new()); // Return empty string to skip thumbnail
        }
    };

    if !output.status.success() {
        warn!(
            "yt-dlp failed to download thumbnail: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        return Ok(String::new()); // Return empty string to skip thumbnail
    }

    // Process the downloaded thumbnail with ImageMagick
    let convert_output = match Command::new("convert")
        .arg("thumbnail.jpg")
        .arg("-resize")
        .arg("640x480") // Force resize to exact dimensions
        .arg("-quality")
        .arg("95")
        .arg("cover.jpg")
        .output()
    {
        Ok(output) => output,
        Err(e) => {
            warn!("Failed to execute ImageMagick convert: {}", e);
            return Ok(String::new()); // Return empty string to skip thumbnail
        }
    };

    if !convert_output.status.success() {
        warn!(
            "ImageMagick failed to convert thumbnail: {}",
            String::from_utf8_lossy(&convert_output.stderr)
        );
        return Ok(String::new()); // Return empty string to skip thumbnail
    }

    // Remove the original thumbnail
    if let Err(e) = std::fs::remove_file("thumbnail.jpg") {
        warn!("Failed to remove original thumbnail file: {}", e);
        // Continue anyway, not critical
    }

    Ok("cover.jpg".to_string())
}
