#![allow(non_snake_case)]

use crate::config::Config;
use lazy_static::lazy_static;
use md5::{Digest, Md5};
use percent_encoding::{utf8_percent_encode, NON_ALPHANUMERIC};
use qrcode::QrCode;
use reqwest::cookie::{CookieStore, Jar};
use reqwest::Url;
use reqwest_middleware::ClientBuilder;
use reqwest_retry::policies::ExponentialBackoff;
use reqwest_retry::RetryTransientMiddleware;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::error::Error;
use std::fs;
use std::io::Seek;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tracing::warn;
lazy_static! {
    static ref BILISTREAM_PATH: std::path::PathBuf = std::env::current_exe().unwrap();
    static ref CONFIG_PATH: std::path::PathBuf = BILISTREAM_PATH.with_file_name("config.json");
    static ref WBI_CACHE_DIR: std::path::PathBuf = {
        let mut path = BILISTREAM_PATH.clone();
        path.pop(); // Go up one directory from the executable
        path.join(".wbi_cache")
    };
}
const WBI_CACHE_DURATION: u64 = 12 * 60 * 60; // 12 hours in seconds

const MIXIN_KEY_ENC_TAB: [u8; 64] = [
    46, 47, 18, 2, 53, 8, 23, 32, 15, 50, 10, 31, 58, 3, 45, 35, 27, 43, 5, 49, 33, 9, 42, 19, 29,
    28, 14, 39, 12, 38, 41, 13, 37, 48, 7, 16, 24, 55, 40, 61, 26, 17, 0, 1, 60, 51, 30, 4, 22, 25,
    54, 21, 56, 59, 6, 63, 57, 62, 11, 36, 20, 34, 44, 52,
];

fn gen_mixin_key(raw_wbi_key: impl AsRef<[u8]>) -> String {
    let raw_wbi_key = raw_wbi_key.as_ref();
    let mut mixin_key = {
        let binding = MIXIN_KEY_ENC_TAB
            .iter()
            .map(|n| raw_wbi_key[*n as usize])
            .collect::<Vec<u8>>();
        unsafe { String::from_utf8_unchecked(binding) }
    };
    let _ = mixin_key.split_off(32); // 截取前 32 位字符
    mixin_key
}

fn url_encode(s: &str) -> String {
    utf8_percent_encode(s, NON_ALPHANUMERIC)
        .to_string()
        .replace('+', "%20")
}

fn calculate_w_rid(params: &BTreeMap<&str, String>, mixin_key: &str) -> String {
    // Sort parameters by key and encode values
    let encoded_params: Vec<String> = params
        .iter()
        .map(|(k, v)| format!("{}={}", k, url_encode(v)))
        .collect();

    // Join parameters with &
    let param_string = encoded_params.join("&");

    // Append mixin_key
    let string_to_hash = format!("{}{}", param_string, mixin_key);

    // Calculate MD5
    let mut hasher = Md5::new();
    hasher.update(string_to_hash.as_bytes());
    format!("{:x}", hasher.finalize())
}

async fn get_wbi_keys(agent: &reqwest::Client) -> Result<(String, String), Box<dyn Error>> {
    // Create cache directory if it doesn't exist
    fs::create_dir_all(&*WBI_CACHE_DIR)?;

    let img_key_path = WBI_CACHE_DIR.join("img_key");
    let sub_key_path = WBI_CACHE_DIR.join("sub_key");
    let timestamp_path = WBI_CACHE_DIR.join("timestamp");

    // Check if we have cached keys and if they're still valid
    if img_key_path.exists() && sub_key_path.exists() && timestamp_path.exists() {
        let timestamp = fs::read_to_string(&timestamp_path)?
            .parse::<u64>()
            .unwrap_or(0);
        let current_time = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();

        if current_time - timestamp < WBI_CACHE_DURATION {
            // Cache is still valid, read the keys
            let img_key = fs::read_to_string(&img_key_path)?;
            let sub_key = fs::read_to_string(&sub_key_path)?;
            return Ok((img_key, sub_key));
        }
    }

    // Cache is invalid or doesn't exist, get new keys
    let nav_data: Value = agent
        .get("https://api.bilibili.com/x/web-interface/nav")
        .header("User-Agent", "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/58.0.3029.110 Safari/537.3")
        .header("Referer", "https://www.bilibili.com/")
        .send()
        .await?
        .json()
        .await?;

    let wbi_img = nav_data
        .get("data")
        .and_then(|d| d.get("wbi_img"))
        .ok_or_else(|| "Missing wbi_img in nav response")?;

    let img_url = wbi_img
        .get("img_url")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "Missing img_url in wbi_img")?;

    let sub_url = wbi_img
        .get("sub_url")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "Missing sub_url in wbi_img")?;

    let img_key = img_url
        .split('/')
        .last()
        .unwrap_or("")
        .split('.')
        .next()
        .unwrap_or("");
    let sub_key = sub_url
        .split('/')
        .last()
        .unwrap_or("")
        .split('.')
        .next()
        .unwrap_or("");

    // Save the new keys and timestamp
    fs::write(&img_key_path, img_key)?;
    fs::write(&sub_key_path, sub_key)?;
    fs::write(
        &timestamp_path,
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
            .to_string(),
    )?;

    Ok((img_key.to_string(), sub_key.to_string()))
}

enum AppKeyStore {
    BiliTV,
    Android,
}

impl AppKeyStore {
    fn app_key(&self) -> &'static str {
        match self {
            Self::BiliTV => "4409e2ce8ffd12b8",
            Self::Android => "1d8b6e7d45233436",
        }
    }

    fn appsec(&self) -> &'static str {
        match self {
            Self::BiliTV => "59b43e04ad6965f34319062b478f83dd",
            Self::Android => "560c52ccd288fed045859ed18bffd973",
        }
    }
}

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

    // Get WBI keys
    let (img_key, sub_key) = get_wbi_keys(&raw_client).await?;
    let raw_wbi_key = format!("{}{}", img_key, sub_key);
    let mixin_key = gen_mixin_key(raw_wbi_key.as_bytes());

    // Get wts
    let wts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
        .to_string();

    // Create sorted parameters map
    let mut params = BTreeMap::new();
    params.insert("room_id", room.to_string());
    params.insert("wts", wts.clone());

    // Calculate w_rid
    let w_rid = calculate_w_rid(&params, &mixin_key);

    // Build final query string
    let query_string = format!("room_id={}&wts={}&w_rid={}", room, wts, w_rid);

    // Make the GET request to check the live status
    let res: Value = client
        .get(&format!(
            "https://api.live.bilibili.com/room/v1/Room/get_info?{}",
            query_string
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
pub async fn bili_start_live(cfg: &mut Config, area_v2: u64) -> Result<(), Box<dyn Error>> {
    let secret = "af125a0d5279fd576c1b4418a3e8276d";
    let appkey = "aae92bc66f3edfab"; // BiliTV appkey
    let platform = "pc_link";
    let ts = chrono::Utc::now().timestamp().to_string();

    // 获取直播姬版本号和 build
    let version_api =
        "https://api.live.bilibili.com/xlive/app-blink/v1/liveVersionInfo/getHomePageLiveVersion";
    let version_appkey = "aae92bc66f3edfab";
    let version_ts = chrono::Utc::now().timestamp().to_string();

    let version_query = format!(
        "system_version=2&ts={}&appKey={}&sign=",
        version_ts, version_appkey
    );

    let version_url = format!("{}?{}", version_api, version_query);

    let version_resp: serde_json::Value = reqwest::Client::new()
        .get(&version_url)
        .send()
        .await?
        .json()
        .await?;

    let (version, build) = if version_resp["code"].as_i64() == Some(0) {
        let data = &version_resp["data"];
        (
            data["curr_version"]
                .as_str()
                .unwrap_or("7.19.0.9432")
                .to_string(),
            data["build"].as_i64().unwrap_or(9432),
        )
    } else {
        ("7.19.0.9432".to_string(), 9432)
    };

    // 构造开播参数
    let mut params = BTreeMap::new();
    params.insert("appkey", appkey.to_string());
    params.insert("area_v2", area_v2.to_string());
    params.insert("backup_stream", "0".to_string());
    params.insert("build", build.to_string());
    params.insert("csrf", cfg.bililive.credentials.bili_jct.clone());
    params.insert("csrf_token", cfg.bililive.credentials.bili_jct.clone());
    params.insert("platform", platform.to_string());
    params.insert("room_id", cfg.bililive.room.to_string());
    params.insert("ts", ts.clone());
    params.insert("version", version.clone());

    // Build the query string (sorted by key)
    let query_string = params
        .iter()
        .map(|(k, v)| format!("{}={}", k, v))
        .collect::<Vec<_>>()
        .join("&");

    // Sign the query string with appsec
    let mut hasher = Md5::new();
    hasher.update(format!("{}{}", query_string, secret));
    let sign = format!("{:x}", hasher.finalize());

    // Add sign to params
    params.insert("sign", sign.clone());

    // Build the final query string with all parameters including sign
    let query_string = params
        .iter()
        .map(|(k, v)| format!("{}={}", k, v))
        .collect::<Vec<_>>()
        .join("&");

    // Prepare cookies
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

    // Build the HTTP client
    let client = reqwest::Client::builder()
        .cookie_store(true)
        .cookie_provider(jar.into())
        .timeout(Duration::new(30, 0))
        .build()?;

    // POST to the endpoint with query parameters in URL
    let response: Value = client
        .post(format!(
            "https://api.live.bilibili.com/room/v1/Room/startLive?{}",
            query_string
        ))
        .header("Accept", "application/json, text/plain, */*")
        .send()
        .await?
        .json()
        .await?;

    // Check response code and provide clear error messages
    let code = response["code"].as_i64().unwrap_or(-1);

    if code != 0 {
        let message = response["message"]
            .as_str()
            .or_else(|| response["msg"].as_str())
            .unwrap_or("Unknown error");

        match code {
            60024 => {
                // Face verification required
                if let Some(qr_url) = response["data"]["qr"].as_str() {
                    return Err(format!("FACE_AUTH_REQUIRED:{}", qr_url).into());
                }
            }
            60031 => {
                // Abnormal streaming behavior - temporary ban
                tracing::error!("❌ Bilibili 开播失败 (错误码: {})", code);
                tracing::error!("📛 {}", message);
                return Err(format!("Bilibili 暂时无法开播: {}", message).into());
            }
            _ => {
                tracing::error!("❌ Bilibili 开播失败 (错误码: {}): {}", code, message);
                tracing::debug!("完整响应: {:#?}", response);
                return Err(format!("开播失败 (错误码 {}): {}", code, message).into());
            }
        }
    }

    // Extract RTMP information from the response
    if response["code"].as_i64() == Some(0) {
        if let Some(rtmp_data) = response["data"]["rtmp"].as_object() {
            if let (Some(addr), Some(code)) = (rtmp_data.get("addr"), rtmp_data.get("code")) {
                if let (Some(rtmp_url), Some(rtmp_key)) = (addr.as_str(), code.as_str()) {
                    // Update config with new RTMP info
                    cfg.bililive.bili_rtmp_url = rtmp_url.to_string();
                    cfg.bililive.bili_rtmp_key = rtmp_key.to_string();

                    // Save the updated config to file
                    let updated_json = serde_json::to_string_pretty(&cfg)?;
                    std::fs::write(&*CONFIG_PATH, updated_json)?;

                    // tracing::info!("Updated RTMP information in config");
                }
            }
        }
    }

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

    // Check if the API call was successful
    if let Some(code) = _res.get("code").and_then(|v| v.as_i64()) {
        if code != 0 {
            let message = _res
                .get("message")
                .or_else(|| _res.get("msg"))
                .and_then(|v| v.as_str())
                .unwrap_or("未知错误");

            // Check for content moderation failure
            if message.contains("未能通过审核") {
                return Err(format!(
                    "⚠️ 标题审核失败: {} - 可能包含敏感词汇，请尝试其他标题",
                    message
                )
                .into());
            }

            return Err(format!("API调用失败 (code: {}): {}", code, message).into());
        }
    }

    // Optionally, print the response for debugging
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

#[derive(Debug, Deserialize, Serialize)]
pub struct ResponseData<T> {
    code: i64,
    message: String,
    data: Option<T>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(untagged)]
pub enum ResponseValue {
    Login(LoginInfo),
    OAuth(OAuthInfo),
    Value(Value),
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct LoginInfo {
    pub cookie_info: Value,
    pub sso: Vec<String>,
    pub token_info: TokenInfo,
    pub platform: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TokenInfo {
    pub access_token: String,
    pub expires_in: u32,
    pub mid: u64,
    pub refresh_token: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct OAuthInfo {
    pub mid: u64,
    pub access_token: String,
    pub expires_in: u32,
    pub refresh: bool,
}

#[derive(Clone)]
struct StatefulClient {
    client: reqwest::Client,
    cookie_store: Arc<Jar>,
}

impl StatefulClient {
    fn new(headers: reqwest::header::HeaderMap) -> Self {
        let cookie_store = Arc::new(Jar::default());
        let client = reqwest::Client::builder()
            .cookie_provider(cookie_store.clone())
            .default_headers(headers)
            .build()
            .unwrap();

        Self {
            client,
            cookie_store,
        }
    }
}

pub struct Credential(StatefulClient);

impl Credential {
    pub fn new() -> Self {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            "Referer",
            reqwest::header::HeaderValue::from_static("https://www.bilibili.com/"),
        );
        Self(StatefulClient::new(headers))
    }

    pub async fn get_qrcode(&self) -> Result<Value, Box<dyn Error>> {
        let mut form = json!({
            "appkey": "4409e2ce8ffd12b8",
            "local_id": "0",
            "ts": SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs()
        });

        let urlencoded = serde_urlencoded::to_string(&form)?;
        let sign = self.sign(&urlencoded, "59b43e04ad6965f34319062b478f83dd"); // BiliTV appsec
        form["sign"] = Value::from(sign);

        Ok(self
            .0
            .client
            .post("http://passport.bilibili.com/x/passport-tv-login/qrcode/auth_code")
            .form(&form)
            .send()
            .await?
            .json()
            .await?)
    }

    pub fn sign(&self, param: &str, app_sec: &str) -> String {
        let mut hasher = Md5::new();
        hasher.update(format!("{}{}", param, app_sec));
        format!("{:x}", hasher.finalize())
    }

    async fn login_by_qrcode(&self, value: Value) -> Result<LoginInfo, Box<dyn Error>> {
        let mut form = json!({
            "appkey": "4409e2ce8ffd12b8",
            "auth_code": value["data"]["auth_code"],
            "local_id": "0",
            "ts": SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs()
        });

        let urlencoded = serde_urlencoded::to_string(&form)?;
        let sign = self.sign(&urlencoded, "59b43e04ad6965f34319062b478f83dd");
        form["sign"] = Value::from(sign);

        loop {
            tokio::time::sleep(Duration::from_secs(1)).await;
            let res: ResponseData<ResponseValue> = self
                .0
                .client
                .post("http://passport.bilibili.com/x/passport-tv-login/qrcode/poll")
                .form(&form)
                .send()
                .await?
                .json()
                .await?;

            match res {
                ResponseData {
                    code: 0,
                    data: Some(ResponseValue::Login(info)),
                    ..
                } => {
                    // Save cookies from response
                    if let Some(cookies) = info.cookie_info.get("cookies") {
                        let base_url = Url::parse("https://bilibili.com")?;
                        for cookie in cookies.as_array().unwrap_or(&Vec::new()) {
                            let cookie_str = format!(
                                "{}={}",
                                cookie["name"].as_str().unwrap_or(""),
                                cookie["value"].as_str().unwrap_or("")
                            );
                            self.0.cookie_store.add_cookie_str(&cookie_str, &base_url);
                        }
                    }
                    return Ok(LoginInfo {
                        platform: Some("BiliTV".to_string()),
                        ..info
                    });
                }
                ResponseData { code: 86039, .. } => {
                    print!("\rWaiting for QR code scan...");
                }
                _ => {
                    return Err(format!("Login failed: {:#?}", res).into());
                }
            }
        }
    }

    pub async fn renew_tokens(&self, login_info: LoginInfo) -> Result<LoginInfo, Box<dyn Error>> {
        let keypair = match login_info.platform.as_deref() {
            Some("BiliTV") => AppKeyStore::BiliTV,
            Some("Android") => AppKeyStore::Android,
            Some(_) => return Err("Unknown platform".into()),
            None => return Err("Unknown platform".into()),
        };

        let mut payload = json!({
            "access_key": login_info.token_info.access_token,
            "actionKey": "appkey",
            "appkey": keypair.app_key(),
            "refresh_token": login_info.token_info.refresh_token,
            "ts": SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs(),
        });

        let urlencoded = serde_urlencoded::to_string(&payload)?;
        let sign = self.sign(&urlencoded, keypair.appsec());
        payload["sign"] = Value::from(sign);

        let response: ResponseData<ResponseValue> = self
            .0
            .client
            .post("https://passport.bilibili.com/x/passport-login/oauth2/refresh_token")
            .form(&payload)
            .send()
            .await?
            .json()
            .await?;

        match response.data {
            Some(ResponseValue::Login(info)) if !info.cookie_info.is_null() => {
                if let Some(cookies) = info.cookie_info.get("cookies") {
                    let base_url = Url::parse("https://bilibili.com")?;
                    for cookie in cookies.as_array().unwrap_or(&Vec::new()) {
                        let cookie_str = format!(
                            "{}={}",
                            cookie["name"].as_str().unwrap_or(""),
                            cookie["value"].as_str().unwrap_or("")
                        );
                        self.0.cookie_store.add_cookie_str(&cookie_str, &base_url);
                    }
                }
                Ok(LoginInfo {
                    platform: login_info.platform,
                    ..info
                })
            }
            _ => Err("Failed to renew tokens".into()),
        }
    }
}

/// Get QR code for web-based login
pub async fn get_login_qrcode() -> Result<(String, String), Box<dyn Error>> {
    let credential = Credential::new();
    let qrcode_res = credential.get_qrcode().await?;

    let qr_url = qrcode_res["data"]["url"]
        .as_str()
        .ok_or("Failed to get QR code URL")?
        .to_string();

    let auth_code = qrcode_res["data"]["auth_code"]
        .as_str()
        .ok_or("Failed to get auth code")?
        .to_string();

    Ok((qr_url, auth_code))
}

/// Poll login status for web-based login
pub async fn poll_login_status(auth_code: &str) -> Result<String, Box<dyn Error>> {
    let credential = Credential::new();

    let mut form = json!({
        "appkey": "4409e2ce8ffd12b8",
        "auth_code": auth_code,
        "local_id": "0",
        "ts": SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs()
    });

    let urlencoded = serde_urlencoded::to_string(&form)?;
    let sign = credential.sign(&urlencoded, "59b43e04ad6965f34319062b478f83dd");
    form["sign"] = Value::from(sign);

    let res: ResponseData<ResponseValue> = credential
        .0
        .client
        .post("http://passport.bilibili.com/x/passport-tv-login/qrcode/poll")
        .form(&form)
        .send()
        .await?
        .json()
        .await?;

    match res {
        ResponseData {
            code: 0,
            data: Some(ResponseValue::Login(info)),
            ..
        } => {
            // Save login info
            save_login_info(&credential, info).await?;
            Ok("success".to_string())
        }
        ResponseData { code: 86039, .. } => Ok("waiting".to_string()),
        ResponseData { code: 86038, .. } => Ok("expired".to_string()),
        _ => Err(format!("Login failed: {:#?}", res).into()),
    }
}

async fn save_login_info(credential: &Credential, info: LoginInfo) -> Result<(), Box<dyn Error>> {
    // Save cookies from response
    if let Some(cookies) = info.cookie_info.get("cookies") {
        let base_url = Url::parse("https://bilibili.com")?;
        for cookie in cookies.as_array().unwrap_or(&Vec::new()) {
            let cookie_str = format!(
                "{}={}",
                cookie["name"].as_str().unwrap_or(""),
                cookie["value"].as_str().unwrap_or("")
            );
            credential
                .0
                .cookie_store
                .add_cookie_str(&cookie_str, &base_url);
        }
    }

    // Create cookie info structure
    let mut cookies = Vec::new();
    let base_url = Url::parse("https://bilibili.com")?;

    if let Some(cookie_header) = credential.0.cookie_store.cookies(&base_url) {
        let cookie_str = cookie_header.to_str().unwrap_or_default();
        for cookie_part in cookie_str.split("; ") {
            if let Some((name, value)) = cookie_part.split_once('=') {
                let expires = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_secs() as i64
                    + 15552000; // 180 days

                cookies.push(json!({
                    "name": name,
                    "value": value,
                    "expires": expires,
                    "http_only": 0,
                    "secure": 0
                }));
            }
        }
    }

    let cookie_info = json!({
        "cookies": cookies,
        "domains": [
            ".bilibili.com",
            ".biligame.com",
            ".bigfun.cn",
            ".bigfunapp.cn",
            ".dreamcast.hk"
        ]
    });

    // Create final login info structure
    let final_info = json!({
        "cookie_info": cookie_info,
        "sso": [
            "https://passport.bilibili.com/api/v2/sso",
            "https://passport.biligame.com/api/v2/sso",
            "https://passport.bigfunapp.cn/api/v2/sso"
        ],
        "token_info": info.token_info,
        "platform": "BiliTV"
    });

    // Save to file
    let bilistream_dir = std::env::var("BILISTREAM_DIR").unwrap_or_else(|_| {
        std::env::current_exe()
            .unwrap()
            .to_string_lossy()
            .to_string()
    });
    let cookies_path = Path::new(&bilistream_dir).with_file_name("cookies.json");
    fs::write(cookies_path, serde_json::to_string_pretty(&final_info)?)?;

    Ok(())
}

/// Login to Bilibili using QR code and save cookies
pub async fn login() -> Result<(), Box<dyn Error>> {
    let credential = Credential::new();

    // Get QR code
    let qrcode_res = credential.get_qrcode().await?;

    // Generate and display QR code
    let qr_url = qrcode_res["data"]["url"]
        .as_str()
        .ok_or("Failed to get QR code URL")?;

    let qr = QrCode::new(qr_url)?;
    let qr_string = qr
        .render::<char>()
        .quiet_zone(false)
        .module_dimensions(2, 1)
        .build();
    println!("Please scan the QR code to login:\n{}", qr_string);

    // Wait for scan and get login info
    let login_info = credential.login_by_qrcode(qrcode_res).await?;

    // Create cookie info structure
    let mut cookies = Vec::new();
    let base_url = Url::parse("https://bilibili.com")?;

    if let Some(cookie_header) = credential.0.cookie_store.cookies(&base_url) {
        let cookie_str = cookie_header.to_str().unwrap_or_default();
        for cookie_part in cookie_str.split("; ") {
            if let Some((name, value)) = cookie_part.split_once('=') {
                let expires = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_secs() as i64
                    + 15552000; // 180 days

                cookies.push(json!({
                    "name": name,
                    "value": value,
                    "expires": expires,
                    "http_only": 0,
                    "secure": 0
                }));
            }
        }
    }

    let cookie_info = json!({
        "cookies": cookies,
        "domains": [
            ".bilibili.com",
            ".biligame.com",
            ".bigfun.cn",
            ".bigfunapp.cn",
            ".dreamcast.hk"
        ]
    });

    // Create final login info structure
    let final_info = json!({
        "cookie_info": cookie_info,
        "sso": [
            "https://passport.bilibili.com/api/v2/sso",
            "https://passport.biligame.com/api/v2/sso",
            "https://passport.bigfunapp.cn/api/v2/sso"
        ],
        "token_info": login_info.token_info,
        "platform": "BiliTV"
    });

    // Save to file
    let bilistream_dir = std::env::var("BILISTREAM_DIR").unwrap_or_else(|_| {
        std::env::current_exe()
            .unwrap()
            .to_string_lossy()
            .to_string()
    });
    let cookies_path = Path::new(&bilistream_dir).with_file_name("cookies.json");
    fs::write(cookies_path, serde_json::to_string_pretty(&final_info)?)?;
    println!("Login successful! Cookies saved to cookies.json");

    Ok(())
}

/// Renews the authentication tokens using the existing login info
pub async fn renew() -> Result<(), Box<dyn Error>> {
    let bilistream_dir = std::env::var("BILISTREAM_DIR").unwrap_or_else(|_| {
        std::env::current_exe()
            .unwrap()
            .to_string_lossy()
            .to_string()
    });
    let cookies_path = Path::new(&bilistream_dir).with_file_name("cookies.json");
    let credential = Credential::new();
    let mut file = std::fs::File::options()
        .read(true)
        .write(true)
        .open(&cookies_path)?;

    let login_info: LoginInfo = serde_json::from_reader(&file)?;
    let new_info = credential.renew_tokens(login_info).await?;

    file.rewind()?;
    file.set_len(0)?;
    serde_json::to_writer_pretty(std::io::BufWriter::new(&file), &new_info)?;
    // tracing::info!("{new_info:?}");

    Ok(())
}

// Helper function to create a Command with hidden console on Windows
fn create_hidden_command(program: &str) -> Command {
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        let mut command = Command::new(program);
        // Hide the console window
        command.creation_flags(0x08000000); // CREATE_NO_WINDOW
        command
    }

    #[cfg(not(target_os = "windows"))]
    {
        Command::new(program)
    }
}

// Helper function to get yt-dlp command path
fn get_yt_dlp_command() -> String {
    if cfg!(target_os = "windows") {
        // On Windows, check if yt-dlp.exe exists in the executable directory
        if let Ok(exe_path) = std::env::current_exe() {
            if let Some(exe_dir) = exe_path.parent() {
                let local_yt_dlp = exe_dir.join("yt-dlp.exe");
                if local_yt_dlp.exists() {
                    return local_yt_dlp.to_string_lossy().to_string();
                }
            }
        }
        "yt-dlp.exe".to_string()
    } else {
        "yt-dlp".to_string()
    }
}

// Helper function to get ImageMagick command
fn get_imagemagick_command() -> String {
    if cfg!(target_os = "windows") {
        // On Windows, use convert.exe (renamed from ImageMagick installer)
        if let Ok(exe_path) = std::env::current_exe() {
            if let Some(exe_dir) = exe_path.parent() {
                let local_convert = exe_dir.join("convert.exe");
                if local_convert.exists() {
                    return local_convert.to_string_lossy().to_string();
                }
            }
        }
        // Try system-installed convert
        "convert".to_string()
    } else {
        // On Linux/macOS, use convert
        "convert".to_string()
    }
}

/// Downloads and processes thumbnail for live streams
pub async fn get_thumbnail(
    platform: &str,
    channel_id: &str,
    proxy: Option<String>,
) -> Result<String, Box<dyn Error>> {
    let mut command = create_hidden_command(&get_yt_dlp_command());

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
    let convert_cmd = get_imagemagick_command();

    let convert_output = match create_hidden_command(&convert_cmd)
        .arg("thumbnail.jpg")
        .arg("-resize")
        .arg("640x480")
        .arg("-quality")
        .arg("95")
        .arg("cover.jpg")
        .output()
    {
        Ok(output) => output,
        Err(e) => {
            warn!("Failed to execute ImageMagick: {}", e);
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
