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
/// * `(bool, String, u64)` - Returns `true` if the room is live, otherwise `false`.
/// * `String` - The title of the room.
/// * `u64` - The area ID of the room.
pub async fn get_bili_live_status(room: i32) -> Result<(bool, String, u64), Box<dyn Error>> {
    // Define the retry policy with a very high number of retries
    let retry_policy = ExponentialBackoff::builder().build_with_max_retries(5);

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
            "https://api.live.bilibili.com/room/v1/Room/get_info?room_id={}",
            room
        ))
        .send()
        .await?
        .json()
        .await?;
    let title = res["data"]["title"].to_string();
    let title = title.trim_matches('"');
    // Determine live status based on the response
    Ok((
        res["data"]["live_status"] == 1,
        title.to_string(),
        res["data"]["area_id"].as_u64().unwrap(),
    ))
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
pub async fn bili_start_live(cfg: &Config, area_v2: u64) -> Result<(), Box<dyn Error>> {
    let cookie = format!(
        "SESSDATA={};bili_jct={};DedeUserID={};DedeUserID__ckMd5={}",
        cfg.bililive.credentials.sessdata,
        cfg.bililive.credentials.bili_jct,
        cfg.bililive.credentials.dede_user_id,
        cfg.bililive.credentials.dede_user_id_ckmd5
    );
    let url = Url::parse("https://api.live.bilibili.com/")?;
    let jar = Jar::default();
    jar.add_cookie_str(&cookie, &url);

    // Define the retry policy
    let retry_policy = ExponentialBackoff::builder().build_with_max_retries(5);

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
            cfg.bililive.room,
            area_v2,
            cfg.bililive.credentials.bili_jct,
            cfg.bililive.credentials.bili_jct
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
pub async fn bili_change_live_title(cfg: &Config, title: &str) -> Result<(), Box<dyn Error>> {
    let cookie = format!(
        "SESSDATA={};bili_jct={};DedeUserID={};DedeUserID__ckMd5={}",
        cfg.bililive.credentials.sessdata,
        cfg.bililive.credentials.bili_jct,
        cfg.bililive.credentials.dede_user_id,
        cfg.bililive.credentials.dede_user_id_ckmd5
    );
    let url = Url::parse("https://api.live.bilibili.com/room/v1/Room/update")?;
    let jar = Jar::default();
    jar.add_cookie_str(&cookie, &url);

    // Define the retry policy
    let retry_policy = ExponentialBackoff::builder().build_with_max_retries(5);

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
            cfg.bililive.room,
            title,
            cfg.bililive.credentials.bili_jct,
            cfg.bililive.credentials.bili_jct
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
        cfg.bililive.credentials.sessdata,
        cfg.bililive.credentials.bili_jct,
        cfg.bililive.credentials.dede_user_id,
        cfg.bililive.credentials.dede_user_id_ckmd5
    );
    let url = Url::parse("https://api.live.bilibili.com/")?;
    let jar = Jar::default();
    jar.add_cookie_str(&cookie, &url);

    // Define the retry policy
    let retry_policy = ExponentialBackoff::builder().build_with_max_retries(5);

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
            cfg.bililive.room, cfg.bililive.credentials.bili_jct, cfg.bililive.credentials.bili_jct
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

pub async fn send_danmaku(
    cfg: &Config,
    message: &str,
) -> Result<Value, Box<dyn std::error::Error>> {
    let client = reqwest::Client::new();
    let cookie = format!(
        "SESSDATA={};bili_jct={};DedeUserID={};DedeUserID__ckMd5={}",
        cfg.bililive.credentials.sessdata,
        cfg.bililive.credentials.bili_jct,
        cfg.bililive.credentials.dede_user_id,
        cfg.bililive.credentials.dede_user_id_ckmd5
    );
    let resp: Value = client
        .post("https://api.live.bilibili.com/msg/send")
        .header("Cookie", &cookie)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(format!(
            "bubble=0&msg={}&color=16777215&mode=1&fontsize=25&rnd={}&roomid={}&csrf_token={}&csrf={}",
            message,
            chrono::Local::now().timestamp(),
            cfg.bililive.room,
            cfg.bililive.credentials.bili_jct,
            cfg.bililive.credentials.bili_jct
        ))
        .send()
        .await?
        .json()
        .await?;

    if resp["code"].as_i64() != Some(0) {
        return Err(format!("Failed to send danmaku: {}", resp["message"]).into());
    }

    Ok(resp)
}

/// Updates the live stream cover on Bilibili.
///
/// # Arguments
///
/// * `cfg` - Reference to the application configuration.
/// * `image_path` - Path to the new cover image.
///
/// # Returns
///
/// * `Result<(), Box<dyn Error>>` - Returns `Ok` if successful, otherwise an error.
pub async fn bili_change_cover(cfg: &Config, image_path: &str) -> Result<(), Box<dyn Error>> {
    let cookie = format!(
        "SESSDATA={};bili_jct={};DedeUserID={};DedeUserID__ckMd5={}",
        cfg.bililive.credentials.sessdata,
        cfg.bililive.credentials.bili_jct,
        cfg.bililive.credentials.dede_user_id,
        cfg.bililive.credentials.dede_user_id_ckmd5
    );
    let url = Url::parse("https://api.bilibili.com/x/upload/web/image")?;
    let jar = Jar::default();
    jar.add_cookie_str(&cookie, &url);

    let client = reqwest::Client::builder()
        .cookie_store(true)
        .cookie_provider(jar.into())
        .timeout(Duration::new(30, 0))
        .build()?;

    // Step 1: Upload image
    let file_content = tokio::fs::read(image_path).await?;
    let form = reqwest::multipart::Form::new()
        .text("csrf", cfg.bililive.credentials.bili_jct.clone())
        .text("bucket", "live")
        .text("dir", "new_room_cover")
        .part(
            "file",
            reqwest::multipart::Part::bytes(file_content)
                .file_name(image_path.to_string())
                .mime_str("image/jpeg")?,
        );

    let upload_res: Value = client
        .post(format!(
            "https://api.bilibili.com/x/upload/web/image?csrf={}",
            cfg.bililive.credentials.bili_jct
        ))
        .header("Cookie", &cookie)
        .multipart(form)
        .send()
        .await?
        .json()
        .await?;

    if upload_res["code"].as_i64() != Some(0) {
        return Err(format!("Failed to upload image: {}", upload_res["message"]).into());
    }

    let image_url = upload_res["data"]["location"]
        .as_str()
        .ok_or("Failed to get image URL from upload response")?;

    // Step 2: Update cover
    let update_res: Value = client
        .post("https://api.live.bilibili.com/xlive/app-blink/v1/preLive/UpdatePreLiveInfo")
        .header("Cookie", &cookie)
        .header("Accept", "application/json, text/plain, */*")
        .header(
            "content-type",
            "application/x-www-form-urlencoded; charset=UTF-8",
        )
        .form(&[
            ("platform", "web"),
            ("mobi_app", "web"),
            ("build", "1"),
            ("cover", image_url),
            ("coverVertical", ""),
            ("liveDirectionType", "1"),
            ("csrf_token", cfg.bililive.credentials.bili_jct.as_str()),
            ("csrf", cfg.bililive.credentials.bili_jct.as_str()),
            ("visit_id", ""),
        ])
        .send()
        .await?
        .json()
        .await?;

    if update_res["code"].as_i64() != Some(0) {
        println!("Request parameters:");
        println!("cover: {}", image_url);
        println!("csrf_token: {}", cfg.bililive.credentials.bili_jct);
        return Err(format!(
            "Failed to update cover: {} (Response: {})",
            update_res["message"],
            serde_json::to_string_pretty(&update_res).unwrap_or_default()
        ))?;
    }

    Ok(())
}

/// Updates the area of a Bilibili live room.
///
/// # Arguments
///
/// * `cfg` - Reference to the application configuration
/// * `area_id` - The new area ID to set
///
/// # Returns
///
/// * `Result<(), Box<dyn Error>>` - Returns `Ok` if successful, otherwise an error
pub async fn bili_update_area(cfg: &Config, area_id: u64) -> Result<(), Box<dyn Error>> {
    let cookie = format!(
        "SESSDATA={};bili_jct={};DedeUserID={};DedeUserID__ckMd5={}",
        cfg.bililive.credentials.sessdata,
        cfg.bililive.credentials.bili_jct,
        cfg.bililive.credentials.dede_user_id,
        cfg.bililive.credentials.dede_user_id_ckmd5
    );
    let url = Url::parse("https://api.live.bilibili.com/")?;
    let jar = Jar::default();
    jar.add_cookie_str(&cookie, &url);

    // Define the retry policy
    let retry_policy = ExponentialBackoff::builder().build_with_max_retries(5);

    // Build the HTTP client with retry middleware
    let raw_client = reqwest::Client::builder()
        .cookie_store(true)
        .cookie_provider(jar.into())
        .timeout(Duration::new(30, 0))
        .build()?;
    let client = ClientBuilder::new(raw_client.clone())
        .with(RetryTransientMiddleware::new_with_policy(retry_policy))
        .build();

    let form_data = [
        ("room_id", cfg.bililive.room.to_string()),
        ("area_id", area_id.to_string()),
        ("activity_id", "0".to_string()),
        ("platform", "pc".to_string()),
        ("csrf_token", cfg.bililive.credentials.bili_jct.clone()),
        ("csrf", cfg.bililive.credentials.bili_jct.clone()),
        ("visit_id", "".to_string()),
    ];

    let res: Value = client
        .post("https://api.live.bilibili.com/room/v1/Room/update")
        .header("Cookie", &cookie)
        .form(&form_data)
        .send()
        .await?
        .json()
        .await?;

    if res["code"].as_i64() != Some(0) {
        return Err(format!(
            "Failed to update room area: {} (Response: {})",
            res["message"],
            serde_json::to_string_pretty(&res).unwrap_or_default()
        ))?;
    }

    Ok(())
}
