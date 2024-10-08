use crate::config::Config;
use reqwest::{cookie::Jar, Url};
use reqwest_middleware::ClientBuilder;
use reqwest_retry::policies::ExponentialBackoff;
use reqwest_retry::RetryTransientMiddleware;
use serde_json::Value;
use std::error::Error;
use std::time::Duration;

/// Retrieves the live status of a Bilibili room.
///
/// # Arguments
///
/// * `room` - The room ID to check.
///
/// # Returns
///
/// * `bool` - Returns `true` if the room is live, otherwise `false`.
pub async fn get_bili_live_status(room: i32) -> Result<bool, Box<dyn Error>> {
    // Define the retry policy with a very high number of retries
    let retry_policy = ExponentialBackoff::builder().build_with_max_retries(4294967295);

    // Build the raw HTTP client with cookie storage and timeout
    let raw_client = reqwest::Client::builder()
        .cookie_store(true)
        .timeout(Duration::new(30, 0))
        .build()?;

    // Wrap the client with retry middleware
    let client = ClientBuilder::new(raw_client.clone())
        .with(RetryTransientMiddleware::new_with_policy(retry_policy))
        .build();

    // Make the GET request to check the live status
    let res: Value = client
        .get(&format!(
            "https://api.live.bilibili.com/xlive/web-room/v2/index/getRoomPlayInfo?room_id={}&platform=web",
            room
        ))
        .send()
        .await?
        .json()
        .await?;

    // Determine live status based on the response
    Ok(res["data"]["live_status"] != 0)
}

/// Starts a Bilibili live stream.
///
/// # Arguments
///
/// * `cfg` - Reference to the application configuration.
///
/// # Returns
///
/// * `Result<(), Box<dyn Error>>` - Returns `Ok` if successful, otherwise an error.
pub async fn bili_start_live(cfg: &Config) -> Result<(), Box<dyn Error>> {
    let cookie = format!(
        "SESSDATA={};bili_jct={};DedeUserID={};DedeUserID__ckMd5={}",
        cfg.bililive.sessdata,
        cfg.bililive.bili_jct,
        cfg.bililive.dede_user_id,
        cfg.bililive.dede_user_id_ckmd5
    );
    let url = Url::parse("https://api.live.bilibili.com/")?;
    let jar = Jar::default();
    jar.add_cookie_str(&cookie, &url);

    // Define the retry policy
    let retry_policy = ExponentialBackoff::builder().build_with_max_retries(4294967295);

    // Build the HTTP client with retry middleware
    let raw_client = reqwest::Client::builder()
        .cookie_store(true)
        .cookie_provider(jar.into())
        .timeout(Duration::new(30, 0))
        .build()?;
    let client = ClientBuilder::new(raw_client.clone())
        .with(RetryTransientMiddleware::new_with_policy(retry_policy))
        .build();

    // Make the POST request to start the live stream
    let _res: Value = client
        .post("https://api.live.bilibili.com/room/v1/Room/startLive")
        .header("Accept", "application/json, text/plain, */*")
        .header(
            "content-type",
            "application/x-www-form-urlencoded; charset=UTF-8",
        )
        .body(format!(
            "room_id={}&platform=android_link&area_v2={}&csrf_token={}&csrf={}",
            cfg.bililive.room, cfg.bililive.area_v2, cfg.bililive.bili_jct, cfg.bililive.bili_jct
        ))
        .send()
        .await?
        .json()
        .await?;
    // tracing::info!("{:#?}", _res);
    // Optionally, handle the response if needed
    // println!("{:#?}", res);

    Ok(())
}

/// Updates the live stream title on Bilibili.
///
/// # Arguments
///
/// * `cfg` - Reference to the application configuration.
///
/// # Returns
///
/// * `Result<(), Box<dyn Error>>` - Returns `Ok` if successful, otherwise an error.
pub async fn bili_change_live_title(cfg: &Config) -> Result<(), Box<dyn Error>> {
    let cookie = format!(
        "SESSDATA={};bili_jct={};DedeUserID={};DedeUserID__ckMd5={}",
        cfg.bililive.sessdata,
        cfg.bililive.bili_jct,
        cfg.bililive.dede_user_id,
        cfg.bililive.dede_user_id_ckmd5
    );
    let url = Url::parse("https://api.live.bilibili.com/room/v1/Room/update")?;
    let jar = Jar::default();
    jar.add_cookie_str(&cookie, &url);

    // Define the retry policy
    let retry_policy = ExponentialBackoff::builder().build_with_max_retries(4294967295);

    // Build the HTTP client with retry middleware
    let raw_client = reqwest::Client::builder()
        .cookie_store(true)
        .cookie_provider(jar.into())
        .timeout(Duration::new(30, 0))
        .build()?;
    let client = ClientBuilder::new(raw_client.clone())
        .with(RetryTransientMiddleware::new_with_policy(retry_policy))
        .build();

    // Make the POST request to update the live title
    let _res: Value = client
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
        .await?
        .json()
        .await?;

    // Optionally, handle the response if needed
    // println!("{:#?}", res);

    Ok(())
}

/// Stops the Bilibili live stream.
///
/// # Arguments
///
/// * `cfg` - Reference to the application configuration.
///
/// # Returns
///
/// * `Result<(), Box<dyn Error>>` - Returns `Ok` if successful, otherwise an error.
pub async fn bili_stop_live(cfg: &Config) -> Result<(), Box<dyn Error>> {
    let cookie = format!(
        "SESSDATA={};bili_jct={};DedeUserID={};DedeUserID__ckMd5={}",
        cfg.bililive.sessdata,
        cfg.bililive.bili_jct,
        cfg.bililive.dede_user_id,
        cfg.bililive.dede_user_id_ckmd5
    );
    let url = Url::parse("https://api.live.bilibili.com/")?;
    let jar = Jar::default();
    jar.add_cookie_str(&cookie, &url);

    // Define the retry policy
    let retry_policy = ExponentialBackoff::builder().build_with_max_retries(4294967295);

    // Build the HTTP client with retry middleware
    let raw_client = reqwest::Client::builder()
        .cookie_store(true)
        .cookie_provider(jar.into())
        .timeout(Duration::new(30, 0))
        .build()?;
    let client = ClientBuilder::new(raw_client.clone())
        .with(RetryTransientMiddleware::new_with_policy(retry_policy))
        .build();

    // Make the POST request to stop the live stream
    let _res: Value = client
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
        .await?
        .json()
        .await?;
    // tracing::info!("{:#?}", _res);
    // Optionally, handle the response if needed
    // println!("{:#?}", res);

    Ok(())
}
