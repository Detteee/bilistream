mod config;
mod plugins;
mod push;

// use crate::push::send_gotify_notification;
use clap::Parser;
use config::{load_config, Config};
use plugins::select_live;
use reqwest::{cookie::Jar, Url};
use reqwest_middleware::ClientBuilder;
use reqwest_retry::policies::ExponentialBackoff;
use reqwest_retry::RetryTransientMiddleware;
// use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;
use tokio;
use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt};
#[derive(Parser)]
#[clap(version = "0.1", author = "Dette")]
struct Opts {
    #[clap(short, long, parse(from_os_str), default_value = "./config.yaml")]
    config: PathBuf,
}
#[tokio::main]
async fn main() {
    let opts: Opts = Opts::parse();

    // let p = Mirai::new(host, target);
    // let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    // 只有注册 subscriber 后， 才能在控制台上看到日志输出
    tracing_subscriber::registry()
        // .with(env_filter)
        .with(fmt::layer())
        .init();
    let mut cfg = load_config(&opts.config).unwrap();
    let live_type = select_live(cfg.clone()).await.unwrap();
    bili_change_live_title(&cfg).await; // 初始化直播标题
    loop {
        let old_cfg = cfg.clone();
        cfg = load_config(&opts.config).unwrap();

        // If configuration changed, stop Bilibili live
        if cfg.bililive.area_v2 != old_cfg.bililive.area_v2 {
            tracing::info!("Configuration changed, stopping Bilibili live");
            bili_stop_live(&old_cfg).await;
        }
        if cfg.bililive.title != old_cfg.bililive.title {
            tracing::info!("Configuration changed, stopping Bilibili live");
            bili_change_live_title(&cfg).await;
        }

        let (is_live, scheduled_start) = live_type.get_status().await.unwrap_or((false, None));

        if is_live {
            tracing::info!("{}", format!("{}直播中", live_type.channel_name()));

            // // 添加Gotify推送
            // if let Some(ref gotify_config) = cfg.gotify {
            //     send_gotify_notification(
            //         &gotify_config,
            //         &format!("{}开始直播", live_type.channel_name()),
            //         "bilistream",
            //     )
            //     .await;
            // }

            if get_bili_live_state(cfg.bililive.room.clone()).await {
                tracing::info!("B站直播中");
                ffmpeg(
                    cfg.bililive.bili_rtmp_url.clone(),
                    cfg.bililive.bili_rtmp_key.clone(),
                    live_type.get_real_m3u8_url().await.unwrap(),
                    cfg.ffmpeg_proxy.clone(),
                );
            } else {
                tracing::info!("B站未直播");
                bili_start_live(&cfg).await;
                tracing::info!("B站已开播");

                tokio::time::sleep(Duration::from_secs(5)).await;
                ffmpeg(
                    cfg.bililive.bili_rtmp_url.clone(),
                    cfg.bililive.bili_rtmp_key.clone(),
                    live_type.get_real_m3u8_url().await.unwrap(),
                    cfg.ffmpeg_proxy.clone(),
                );
                loop {
                    let (is_live, _) = live_type.get_status().await.unwrap();
                    if is_live {
                        ffmpeg(
                            cfg.bililive.bili_rtmp_url.clone(),
                            cfg.bililive.bili_rtmp_key.clone(),
                            live_type.get_real_m3u8_url().await.unwrap(),
                            cfg.ffmpeg_proxy.clone(),
                        );
                    } else {
                        break;
                    }
                    tokio::time::sleep(Duration::from_secs(5)).await;
                }
            }
        } else {
            if scheduled_start.is_some() {
                tracing::info!(
                    "{}未直播，计划于 {} 开始",
                    cfg.youtube.channel_name,
                    scheduled_start.unwrap().format("%Y-%m-%d %H:%M:%S") // Format the start time
                );
            } else {
                tracing::info!("{}", format!("{}未直播", cfg.youtube.channel_name));
            }
            if get_bili_live_state(cfg.bililive.room.clone()).await {
                tracing::info!("B站直播中");
                // bili_stop_live(&cfg).await;
                // tracing::info!("B站已关播");
            }
        }
        // 每60秒检测一下直播状态
        tokio::time::sleep(Duration::from_secs(cfg.interval)).await;
    }
}

// 获取B站直播状态
async fn get_bili_live_state(room: i32) -> bool {
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
    let res:serde_json::Value = client
    .get(format!("https://api.live.bilibili.com/xlive/web-room/v2/index/getRoomPlayInfo?room_id={}&platform=web",room))

    .send()
    .await
    .unwrap()
    .json()
    .await
    .unwrap();
    // println!("{:#?}",res["data"]["live_status"]);
    if res["data"]["live_status"] == 0 {
        return false;
    } else {
        return true;
    }
}

// bilibili开播
async fn bili_start_live(cfg: &Config) {
    let cookie = format!(
        "SESSDATA={};bili_jct={};DedeUserID={};DedeUserID__ckMd5={}",
        cfg.bililive.sessdata,
        cfg.bililive.bili_jct,
        cfg.bililive.dede_user_id,
        cfg.bililive.dede_user_id_ckmd5
    );
    let url = "https://api.live.bilibili.com/".parse::<Url>().unwrap();
    let jar = Jar::default();
    jar.add_cookie_str(cookie.as_str(), &url);
    // 设置最大重试次数为4294967295次
    let retry_policy = ExponentialBackoff::builder().build_with_max_retries(4294967295);
    let raw_client = reqwest::Client::builder()
        .cookie_store(true)
        .cookie_provider(jar.into())
        // 设置超时时间为30秒
        .timeout(Duration::new(30, 0))
        .build()
        .unwrap();
    let client = ClientBuilder::new(raw_client.clone())
        .with(RetryTransientMiddleware::new_with_policy(retry_policy))
        .build();
    let _res: serde_json::Value = client
        .post("https://api.live.bilibili.com/room/v1/Room/startLive")
        .header("Accept", "application/json, text/plain, */*")
        .header(
            "content-type",
            "application/x-www-form-urlencoded; charset=UTF-8",
        )
        .body(format!(
            "room_id={}&platform=pc&area_v2={}&csrf_token={}&csrf={}",
            cfg.bililive.room, cfg.bililive.area_v2, cfg.bililive.bili_jct, cfg.bililive.bili_jct
        ))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    // println!("{:#?}",res);
}

// bilibili更改直播标题
async fn bili_change_live_title(cfg: &Config) {
    let cookie = format!(
        "SESSDATA={};bili_jct={};DedeUserID={};DedeUserID__ckMd5={}",
        cfg.bililive.sessdata,
        cfg.bililive.bili_jct,
        cfg.bililive.dede_user_id,
        cfg.bililive.dede_user_id_ckmd5
    );
    let url = "https://api.live.bilibili.com/room/v1/Room/update"
        .parse::<Url>()
        .unwrap();
    let jar = Jar::default();
    jar.add_cookie_str(cookie.as_str(), &url);
    // 设置最大重试次数为4294967295次
    let retry_policy = ExponentialBackoff::builder().build_with_max_retries(4294967295);
    let raw_client = reqwest::Client::builder()
        .cookie_store(true)
        .cookie_provider(jar.into())
        // 设置超时时间为30秒
        .timeout(Duration::new(30, 0))
        .build()
        .unwrap();
    let client = ClientBuilder::new(raw_client.clone())
        .with(RetryTransientMiddleware::new_with_policy(retry_policy))
        .build();
    let _res: serde_json::Value = client
        .post("https://api.live.bilibili.com/room/v1/Room/update")
        .header("Accept", "application/json, text/plain, */*")
        .header(
            "content-type",
            "application/x-www-form-urlencoded; charset=UTF-8",
        )
        .body(format!(
            "room_id={}&platform=pc&title={}&csrf_token={}&csrf={}",
            cfg.bililive.room, cfg.bililive.title, cfg.bililive.bili_jct, cfg.bililive.bili_jct
        ))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    // println!("{:#?}",res);
}
// bilibili关播
async fn bili_stop_live(cfg: &Config) {
    let cookie = format!(
        "SESSDATA={};bili_jct={};DedeUserID={};DedeUserID__ckMd5={}",
        cfg.bililive.sessdata,
        cfg.bililive.bili_jct,
        cfg.bililive.dede_user_id,
        cfg.bililive.dede_user_id_ckmd5
    );
    let url = "https://api.live.bilibili.com/".parse::<Url>().unwrap();
    let jar = Jar::default();
    jar.add_cookie_str(cookie.as_str(), &url);
    // 设置最大重试次数为4294967295次
    let retry_policy = ExponentialBackoff::builder().build_with_max_retries(4294967295);
    let raw_client = reqwest::Client::builder()
        .cookie_store(true)
        .cookie_provider(jar.into())
        // 设置超时时间为30秒
        .timeout(Duration::new(30, 0))
        .build()
        .unwrap();
    let client = ClientBuilder::new(raw_client.clone())
        .with(RetryTransientMiddleware::new_with_policy(retry_policy))
        .build();
    let _res: serde_json::Value = client
        .post("https://api.live.bilibili.com/room/v1/Room/stopLive")
        .header("Accept", "application/json, text/plain, */*")
        .header(
            "content-type",
            "application/x-www-form-urlencoded; charset=UTF-8",
        )
        .body(format!(
            "room_id={}&platform=pc&csrf_token={}&csrf={}",
            cfg.bililive.room, cfg.bililive.bili_jct, cfg.bililive.bili_jct
        ))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    // println!("{:#?}",res);
}

pub fn ffmpeg(rtmp_url: String, rtmp_key: String, m3u8_url: String, ffmpeg_proxy: Option<String>) {
    // let cmd = format!("{}&key={}",rtmp_url,rtmp_key);
    let cmd = format!("{}{}", rtmp_url, rtmp_key);
    let mut command = Command::new("ffmpeg");
    // if ffmpeg_proxy.clone()!= "" {
    //     command.arg(ffmpeg_proxy.clone());
    // }
    if ffmpeg_proxy.is_some() {
        command.arg("-http_proxy");
        command.arg(ffmpeg_proxy.clone().unwrap());
    }
    // command.arg("-re");
    command.arg("-i");
    command.arg(m3u8_url.clone());
    // command.arg("-vcodec");
    command.arg("-c");
    command.arg("copy");
    // command.arg("-acodec");
    // command.arg("aac");
    command.arg("-f");
    command.arg("flv");
    command.arg(cmd);
    match command.status().unwrap().code() {
        Some(code) => {
            println!("Exit Status: {}", code);
            if code == 0 {
                println!("Command executed successfully");
            } else {
                ffmpeg(rtmp_url, rtmp_key, m3u8_url, ffmpeg_proxy)
            }
        }
        None => {
            println!("Process terminated.");
        }
    }
}

// #[cfg(test)]
// mod tests {
//     use super::*;
//     use config::GotifyConfig;
//     use tokio;

//     #[tokio::test]
//     async fn test_send_gotify_notification() {
//         let config = GotifyConfig {
//             url: "https://gotify.com".to_string(),
//             token: "".to_string(),
//         };

//         let message = "这是一条测试通知";

//         send_gotify_notification(&config, message, "bilistream测试").await;
//     }
// }
