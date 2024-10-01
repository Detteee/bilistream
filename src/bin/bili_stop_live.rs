use bilistream::config::{load_config, Config};
use clap::Parser;
use reqwest::{cookie::Jar, Url};
use reqwest_middleware::ClientBuilder;
use reqwest_retry::policies::ExponentialBackoff;
use reqwest_retry::RetryTransientMiddleware;
use std::path::PathBuf;
use std::time::Duration;

#[derive(Parser)]
#[clap(version = "0.1.0", author = "Dette")]
struct Opts {
    #[clap(short, long, parse(from_os_str), default_value = "./config.yaml")]
    config: PathBuf,
}
#[tokio::main]
async fn main() {
    let opts: Opts = Opts::parse();
    let cfg = load_config(&opts.config).unwrap();
    bili_stop_live(&cfg).await;
}

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

    let retry_policy = ExponentialBackoff::builder().build_with_max_retries(4294967295);
    let raw_client = reqwest::Client::builder()
        .cookie_store(true)
        .cookie_provider(jar.into())
        .timeout(Duration::new(30, 0))
        .build()
        .unwrap();
    let client = ClientBuilder::new(raw_client.clone())
        .with(RetryTransientMiddleware::new_with_policy(retry_policy))
        .build();

    let res: serde_json::Value = client
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

    println!("Stop live response: {:?}", res);
}
