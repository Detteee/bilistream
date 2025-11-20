use crate::plugins::bilibili;
use lazy_static::lazy_static;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::error::Error;
use std::fs;
use std::path::Path;

lazy_static! {
    static ref BILISTREAM_PATH: std::path::PathBuf = std::env::current_exe().unwrap();
    static ref CONFIG_PATH: std::path::PathBuf = BILISTREAM_PATH.with_file_name("config.yaml");
    static ref COOKIES_PATH: std::path::PathBuf = BILISTREAM_PATH.with_file_name("cookies.json");
}

/// Struct representing the overall configuration.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Config {
    #[serde(rename = "AutoCover")]
    pub auto_cover: bool,
    #[serde(rename = "AntiCollision")]
    pub enable_anti_collision: bool,
    #[serde(rename = "Interval")]
    pub interval: u64,
    #[serde(rename = "BiliLive")]
    pub bililive: BiliLive,
    #[serde(rename = "Twitch")]
    pub twitch: Twitch,
    #[serde(rename = "Youtube")]
    pub youtube: Youtube,
    #[serde(rename = "Proxy")]
    pub proxy: Option<String>,
    #[serde(rename = "HolodexApiKey")]
    pub holodex_api_key: Option<String>,
    #[serde(rename = "RiotApiKey")]
    pub riot_api_key: Option<String>,
    #[serde(rename = "LolMonitorInterval")]
    pub lol_monitor_interval: Option<u64>,
    #[serde(rename = "AntiCollisionList")]
    pub anti_collision: HashMap<String, i32>,
}

/// Struct representing BiliLive-specific configuration.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct BiliLive {
    #[serde(rename = "EnableDanmakuCommand")]
    pub enable_danmaku_command: bool,
    #[serde(rename = "Room")]
    pub room: i32,
    #[serde(rename = "BiliRtmpUrl")]
    pub bili_rtmp_url: String,
    #[serde(rename = "BiliRtmpKey")]
    pub bili_rtmp_key: String,
    #[serde(skip_deserializing)]
    pub credentials: Credentials,
}

/// Struct to hold credential information extracted from cookies.json.
#[derive(Debug, Serialize, Deserialize, Clone, Default)]
pub struct Credentials {
    pub sessdata: String,
    pub bili_jct: String,
    pub dede_user_id: String,
    pub dede_user_id_ckmd5: String,
    pub buvid3: String,
}

/// Struct representing Twitch configuration.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Twitch {
    #[serde(rename = "ChannelName", default)]
    pub channel_name: String,
    #[serde(rename = "Area_v2", default)]
    pub area_v2: u64,
    #[serde(rename = "ChannelId", default)]
    pub channel_id: String,
    #[serde(rename = "OauthToken", default)]
    pub oauth_token: String,
    #[serde(rename = "ProxyRegion", default)]
    pub proxy_region: String,
}

/// Struct representing YouTube configuration.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Youtube {
    #[serde(rename = "ChannelName", default)]
    pub channel_name: String,
    #[serde(rename = "ChannelId", default)]
    pub channel_id: String,
    #[serde(rename = "Area_v2", default)]
    pub area_v2: u64,
}

/// Structs to mirror the structure of cookies.json
#[derive(Debug, Deserialize)]
struct Cookie {
    name: String,
    value: String,
    // Other fields can be added if needed
}
#[derive(Debug, Deserialize)]
struct CookiesFile {
    cookie_info: CookieInfo,
}

#[derive(Debug, Deserialize)]
struct CookieInfo {
    cookies: Vec<Cookie>,
    // domains: Vec<String>, // Included if needed
}
impl Credentials {
    /// Extracts credentials from cookies and initializes a Credentials struct.
    fn from_cookies(cookies: &[Cookie]) -> Result<Self, Box<dyn Error>> {
        let sessdata = cookies
            .iter()
            .find(|cookie| cookie.name == "SESSDATA")
            .map(|cookie| cookie.value.clone())
            .ok_or("SESSDATA cookie not found")?;

        let bili_jct = cookies
            .iter()
            .find(|cookie| cookie.name == "bili_jct")
            .map(|cookie| cookie.value.clone())
            .ok_or("bili_jct cookie not found")?;

        let dede_user_id = cookies
            .iter()
            .find(|cookie| cookie.name == "DedeUserID")
            .map(|cookie| cookie.value.clone())
            .ok_or("DedeUserID cookie not found")?;

        let dede_user_id_ckmd5 = cookies
            .iter()
            .find(|cookie| cookie.name == "DedeUserID__ckMd5")
            .map(|cookie| cookie.value.clone())
            .ok_or("DedeUserID__ckMd5 cookie not found")?;

        let buvid3 = cookies
            .iter()
            .find(|cookie| cookie.name == "buvid3")
            .map(|cookie| cookie.value.clone())
            .unwrap_or_default();

        Ok(Credentials {
            sessdata,
            bili_jct,
            dede_user_id,
            dede_user_id_ckmd5,
            buvid3,
        })
    }
}

/// Loads credentials from the specified cookies.json file.
fn load_credentials<P: AsRef<Path>>(path: P) -> Result<Credentials, Box<dyn Error>> {
    let file_content = fs::read_to_string(path)?;
    let cookies_file: CookiesFile = serde_json::from_str(&file_content)?;
    Credentials::from_cookies(&cookies_file.cookie_info.cookies)
}

/// Loads the configuration along with credentials from cookies.json.
pub async fn load_config() -> Result<Config, Box<dyn Error>> {
    // Read and deserialize config.yaml
    let config_content = fs::read_to_string(&*CONFIG_PATH)?;
    let mut config: Config = serde_yaml::from_str(&config_content)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    // Check cookies
    check_cookies().await?;

    // Load credentials from cookies.json
    let credentials = load_credentials(COOKIES_PATH.as_ref() as &Path);
    config.bililive.credentials = credentials?;

    Ok(config)
}

async fn check_cookies() -> Result<(), Box<dyn std::error::Error>> {
    // Check for the existence of cookies.json
    if !COOKIES_PATH.exists() {
        tracing::info!("cookies.json 不存在，请登录");
        bilibili::login().await?;
    } else {
        // Check if cookies.json is older than 3 days
        if COOKIES_PATH.metadata()?.modified()?.elapsed()?.as_secs() > 3600 * 24 * 3 {
            tracing::info!("cookies.json 已超过3天，正在刷新");
            bilibili::renew().await?;
        }
    }

    Ok(())
}
