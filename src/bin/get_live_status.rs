use bilistream::config::load_config;
use bilistream::plugins::{get_bili_live_status, get_youtube_live_status, Live, Twitch};
use clap::Parser;
use reqwest_middleware::ClientBuilder;
use reqwest_retry::policies::ExponentialBackoff;
use reqwest_retry::RetryTransientMiddleware;
use std::path::Path;
use std::time::Duration;

#[derive(Parser)]
#[command(version = "0.1.1", author = "Dette")]
struct Opts {
    platform: String,
    channel_id: String,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let opts: Opts = Opts::parse();

    match opts.platform.as_str() {
        "bilibili" => {
            let room_id: i32 = opts.channel_id.parse()?;
            let is_live = get_bili_live_status(room_id).await?;
            println!(
                "Bilibili live status: {}",
                if is_live { "Live" } else { "Not Live" }
            );
        }
        "YT" => {
            let (is_live, _, scheduled_time) = get_youtube_live_status(&opts.channel_id).await?;
            println!(
                "YouTube live status: {}",
                if is_live { "Live" } else { "Not Live" }
            );
            // if let Some(url) = m3u8_url {
            //     println!("M3U8 URL: {}", url);
            // }
            if let Some(time) = scheduled_time {
                println!("Scheduled start time: {}", time);
            }
        }
        "TW" => {
            let retry_policy = ExponentialBackoff::builder().build_with_max_retries(4294967295);
            let raw_client = reqwest::Client::builder()
                .cookie_store(true)
                .timeout(Duration::new(30, 0))
                .build()?;
            let client = ClientBuilder::new(raw_client.clone())
                .with(RetryTransientMiddleware::new_with_policy(retry_policy))
                .build();

            let cfg = load_config(Path::new("./TW/config.yaml"))?;
            let twitch = Twitch::new(&opts.channel_id, cfg.twitch.oauth_token.clone(), client);

            let (is_live, _, _) = twitch.get_status().await?;
            println!(
                "Twitch live status: {}",
                if is_live { "Live" } else { "Not Live" }
            );
            // if let Some(url) = m3u8_url {
            //     println!("M3U8 URL: {}", url);
            // }
        }
        _ => {
            println!("Unsupported platform: {}", opts.platform);
            return Ok(());
        }
    }

    Ok(())
}
