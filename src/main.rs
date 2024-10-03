mod config;
mod plugins;
mod push;

// Re-export anything that needs to be public
pub use config::{load_config, Config};

use clap::Parser;
use plugins::{ffmpeg, select_live};

use std::path::PathBuf;
use std::time::Duration;
use tokio;
// Import the Bilibili functions
use plugins::{bili_change_live_title, bili_start_live, bili_stop_live, get_bili_live_state};

#[derive(Parser)]
#[command(version = "0.1.1", author = "Dette")]
struct Opts {
    #[arg(short, long, value_name = "FILE", default_value = "./config.yaml")]
    config: PathBuf,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let opts: Opts = Opts::parse();

    // Initialize the logger
    tracing_subscriber::fmt::init();

    let mut cfg = load_config(&opts.config)?;
    let live_type = select_live(cfg.clone()).await?;

    loop {
        let old_cfg = cfg.clone();
        cfg = load_config(&opts.config)?;

        // If configuration changed, stop Bilibili live
        if cfg.bililive.area_v2 != old_cfg.bililive.area_v2 {
            tracing::info!("Configuration changed, stopping Bilibili live");
            bili_stop_live(&old_cfg).await?;
        }
        if cfg.bililive.title != old_cfg.bililive.title {
            tracing::info!("Configuration changed, updating Bilibili live title");
            bili_change_live_title(&cfg).await?;
        }

        let (is_live, m3u8_url, scheduled_start) =
            live_type.get_status().await.unwrap_or((false, None, None));

        if is_live {
            tracing::info!("{} 直播中", live_type.channel_name());

            // // 添加Gotify推送
            // if let Some(ref gotify_config) = cfg.gotify {
            //     send_gotify_notification(
            //         &gotify_config,
            //         &format!("{}开始直播", live_type.channel_name()),
            //         "bilistream",
            //     )
            //     .await;
            // }

            if get_bili_live_state(cfg.bililive.room).await? {
                tracing::info!("B站直播中");
                bili_change_live_title(&cfg).await?;

                // Start ffmpeg if not already running
                // ffmpeg may stop, so we need to start it again by check if is_live still = true

                ffmpeg(
                    cfg.bililive.bili_rtmp_url.clone(),
                    cfg.bililive.bili_rtmp_key.clone(),
                    m3u8_url.clone().unwrap_or_default(),
                    cfg.ffmpeg_proxy.clone(),
                );
                let current_is_live = is_live;
                while current_is_live {
                    let (current_is_live, new_m3u8_url, _) =
                        live_type.get_status().await.unwrap_or((false, None, None));

                    if current_is_live {
                        ffmpeg(
                            cfg.bililive.bili_rtmp_url.clone(),
                            cfg.bililive.bili_rtmp_key.clone(),
                            new_m3u8_url.clone().unwrap_or_default(),
                            cfg.ffmpeg_proxy.clone(),
                        );
                    }
                }
                tracing::info!("{} 直播已结束", live_type.channel_name());
            } else {
                tracing::info!("B站未直播");
                bili_start_live(&cfg).await?;
                tracing::info!("B站已开播");
                bili_change_live_title(&cfg).await?;

                tokio::time::sleep(Duration::from_secs(5)).await;

                // Start ffmpeg if not already running
                // ffmpeg may stop, so we need to start it again by check if is_live still = true
                let current_is_live = is_live;
                while current_is_live {
                    let (current_is_live, new_m3u8_url, _) =
                        live_type.get_status().await.unwrap_or((false, None, None));

                    if current_is_live {
                        ffmpeg(
                            cfg.bililive.bili_rtmp_url.clone(),
                            cfg.bililive.bili_rtmp_key.clone(),
                            new_m3u8_url.clone().unwrap_or_default(),
                            cfg.ffmpeg_proxy.clone(),
                        );
                    }
                }
                tracing::info!("{} 直播已结束", live_type.channel_name());
            }
        } else {
            if scheduled_start.is_some() {
                tracing::info!(
                    "{}未直播，计划于 {} 开始",
                    cfg.youtube.channel_name,
                    scheduled_start.unwrap().format("%Y-%m-%d %H:%M:%S") // Format the start time
                );
            } else {
                tracing::info!("{}未直播", live_type.channel_name());
            }
            if get_bili_live_state(cfg.bililive.room.clone()).await? {
                tracing::info!("B站直播中");
                // bili_stop_live(&cfg).await;
                // tracing::info!("B站已关播");
            }
        }
        // 每60秒检测一下直播状态
        tokio::time::sleep(Duration::from_secs(cfg.interval)).await;
    }
}
