use crate::plugins::bilibili;
use lazy_static::lazy_static;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::error::Error;
use std::fs;
use std::path::Path;

lazy_static! {
    static ref BILISTREAM_PATH: std::path::PathBuf = std::env::current_exe().unwrap();
    static ref CONFIG_PATH: std::path::PathBuf = BILISTREAM_PATH.with_file_name("config.json");
    static ref LEGACY_CONFIG_PATH: std::path::PathBuf =
        BILISTREAM_PATH.with_file_name("config.yaml");
    static ref COOKIES_PATH: std::path::PathBuf = BILISTREAM_PATH.with_file_name("cookies.json");
}

/// Struct representing the overall configuration.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Config {
    pub auto_cover: bool,
    pub enable_anti_collision: bool,
    pub interval: u64,
    pub bililive: BiliLive,
    pub twitch: Twitch,
    pub youtube: Youtube,
    pub proxy: Option<String>,
    pub holodex_api_key: Option<String>,
    pub riot_api_key: Option<String>,
    pub enable_lol_monitor: bool,
    pub lol_monitor_interval: Option<u64>,
    pub anti_collision_list: HashMap<String, i32>,
}

/// Struct representing BiliLive-specific configuration.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct BiliLive {
    pub enable_danmaku_command: bool,
    pub room: i32,
    pub bili_rtmp_url: String,
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
    #[serde(default = "default_true")]
    pub enable_monitor: bool,
    #[serde(default)]
    pub channel_name: String,
    #[serde(default)]
    pub area_v2: u64,
    #[serde(default)]
    pub channel_id: String,
    #[serde(default)]
    pub proxy_region: String,
    #[serde(default = "default_quality")]
    pub quality: String,
}

/// Struct representing YouTube configuration.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Youtube {
    #[serde(default = "default_true")]
    pub enable_monitor: bool,
    #[serde(default)]
    pub channel_name: String,
    #[serde(default)]
    pub channel_id: String,
    #[serde(default)]
    pub area_v2: u64,
    #[serde(default = "default_quality")]
    pub quality: String,
    #[serde(default)]
    pub cookies_file: Option<String>,
    #[serde(default)]
    pub cookies_from_browser: Option<String>,
}

fn default_quality() -> String {
    "best".to_string()
}

fn default_true() -> bool {
    true
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
    // Try to load config.json first
    let mut config = if CONFIG_PATH.exists() {
        let config_content = fs::read_to_string(&*CONFIG_PATH)?;
        serde_json::from_str(&config_content)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?
    } else if LEGACY_CONFIG_PATH.exists() {
        // Migrate from config.yaml to config.json
        tracing::info!("Migrating config.yaml to config.json...");
        let config_content = fs::read_to_string(&*LEGACY_CONFIG_PATH)?;

        // Parse YAML with old field names
        #[derive(Deserialize)]
        struct LegacyConfig {
            #[serde(rename = "AutoCover")]
            auto_cover: bool,
            #[serde(rename = "AntiCollision")]
            enable_anti_collision: bool,
            #[serde(rename = "Interval")]
            interval: u64,
            #[serde(rename = "BiliLive")]
            bililive: LegacyBiliLive,
            #[serde(rename = "Twitch")]
            twitch: LegacyTwitch,
            #[serde(rename = "Youtube")]
            youtube: LegacyYoutube,
            #[serde(rename = "Proxy")]
            proxy: Option<String>,
            #[serde(rename = "HolodexApiKey")]
            holodex_api_key: Option<String>,
            #[serde(rename = "RiotApiKey")]
            riot_api_key: Option<String>,
            #[serde(rename = "EnableLolMonitor")]
            enable_lol_monitor: bool,
            #[serde(rename = "LolMonitorInterval")]
            lol_monitor_interval: Option<u64>,
            #[serde(rename = "AntiCollisionList")]
            anti_collision_list: HashMap<String, i32>,
        }

        #[derive(Deserialize)]
        struct LegacyBiliLive {
            #[serde(rename = "EnableDanmakuCommand")]
            enable_danmaku_command: bool,
            #[serde(rename = "Room")]
            room: i32,
            #[serde(rename = "BiliRtmpUrl")]
            bili_rtmp_url: String,
            #[serde(rename = "BiliRtmpKey")]
            bili_rtmp_key: String,
        }

        #[derive(Deserialize)]
        struct LegacyTwitch {
            #[serde(rename = "ChannelName", default)]
            channel_name: String,
            #[serde(rename = "Area_v2", default)]
            area_v2: u64,
            #[serde(rename = "ChannelId", default)]
            channel_id: String,
            #[serde(rename = "ProxyRegion", default)]
            proxy_region: String,
            #[serde(rename = "Quality", default = "default_quality")]
            quality: String,
        }

        #[derive(Deserialize)]
        struct LegacyYoutube {
            #[serde(rename = "ChannelName", default)]
            channel_name: String,
            #[serde(rename = "ChannelId", default)]
            channel_id: String,
            #[serde(rename = "Area_v2", default)]
            area_v2: u64,
            #[serde(rename = "Quality", default = "default_quality")]
            quality: String,
            #[serde(default)]
            cookies_file: Option<String>,
            #[serde(default)]
            cookies_from_browser: Option<String>,
        }

        let legacy: LegacyConfig = serde_yaml::from_str(&config_content)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;

        // Convert to new format
        let new_config = Config {
            auto_cover: legacy.auto_cover,
            enable_anti_collision: legacy.enable_anti_collision,
            interval: legacy.interval,
            bililive: BiliLive {
                enable_danmaku_command: legacy.bililive.enable_danmaku_command,
                room: legacy.bililive.room,
                bili_rtmp_url: legacy.bililive.bili_rtmp_url,
                bili_rtmp_key: legacy.bililive.bili_rtmp_key,
                credentials: Credentials::default(),
            },
            twitch: Twitch {
                enable_monitor: true, // Default to enabled for migration
                channel_name: legacy.twitch.channel_name,
                area_v2: legacy.twitch.area_v2,
                channel_id: legacy.twitch.channel_id,
                proxy_region: legacy.twitch.proxy_region,
                quality: legacy.twitch.quality,
            },
            youtube: Youtube {
                enable_monitor: true, // Default to enabled for migration
                channel_name: legacy.youtube.channel_name,
                channel_id: legacy.youtube.channel_id,
                area_v2: legacy.youtube.area_v2,
                quality: legacy.youtube.quality,
                cookies_file: legacy.youtube.cookies_file,
                cookies_from_browser: legacy.youtube.cookies_from_browser,
            },
            proxy: legacy.proxy,
            holodex_api_key: legacy.holodex_api_key,
            riot_api_key: legacy.riot_api_key,
            enable_lol_monitor: legacy.enable_lol_monitor,
            lol_monitor_interval: legacy.lol_monitor_interval,
            anti_collision_list: legacy.anti_collision_list,
        };

        // Save as JSON
        save_config(&new_config).await?;

        // Backup old config
        let backup_path = LEGACY_CONFIG_PATH.with_extension("yaml.backup");
        fs::rename(&*LEGACY_CONFIG_PATH, backup_path)?;
        tracing::info!("Migration complete! config.yaml backed up as config.yaml.backup");

        new_config
    } else {
        return Err("No config file found. Please run setup first.".into());
    };

    // Check cookies
    check_cookies().await?;

    // Load credentials from cookies.json
    let credentials = load_credentials(COOKIES_PATH.as_ref() as &Path);
    config.bililive.credentials = credentials?;

    Ok(config)
}

/// Saves the configuration to config.json
pub async fn save_config(config: &Config) -> Result<(), Box<dyn Error>> {
    let json = serde_json::to_string_pretty(config)?;
    fs::write(&*CONFIG_PATH, json)?;
    Ok(())
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
