use crate::config::Config;
use md5::{Digest, Md5};
use qrcode::QrCode;
use reqwest::cookie::{CookieStore, Jar};
use reqwest::Url;
use reqwest_middleware::ClientBuilder;
use reqwest_retry::policies::ExponentialBackoff;
use reqwest_retry::RetryTransientMiddleware;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::error::Error;
use std::fs;
use std::io::Seek;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

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
            "appkey": "4409e2ce8ffd12b8", // BiliTV appkey
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

    fn sign(&self, param: &str, app_sec: &str) -> String {
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
            None => return Ok(login_info),
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
    fs::write("cookies.json", serde_json::to_string_pretty(&final_info)?)?;
    println!("Login successful! Cookies saved to cookies.json");

    Ok(())
}

/// Renews the authentication tokens using the existing login info
pub async fn renew(user_cookie: PathBuf) -> Result<(), Box<dyn Error>> {
    let credential = Credential::new();
    let mut file = std::fs::File::options()
        .read(true)
        .write(true)
        .open(&user_cookie)?;

    let login_info: LoginInfo = serde_json::from_reader(&file)?;
    let new_info = credential.renew_tokens(login_info).await?;

    file.rewind()?;
    file.set_len(0)?;
    serde_json::to_writer_pretty(std::io::BufWriter::new(&file), &new_info)?;
    tracing::info!("{new_info:?}");

    Ok(())
}
