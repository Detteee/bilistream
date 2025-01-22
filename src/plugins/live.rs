use super::{Twitch, Youtube};
use crate::config::Config;
use async_trait::async_trait;
use chrono::{DateTime, Local};
use reqwest_middleware::ClientBuilder;
use reqwest_retry::policies::ExponentialBackoff;
use reqwest_retry::RetryTransientMiddleware;
use std::error::Error;
use std::time::Duration;
#[async_trait]
pub trait Live {
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
