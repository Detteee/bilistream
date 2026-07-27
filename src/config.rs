use crate::plugins::bilibili;
use lazy_static::lazy_static;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::error::Error;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::RwLock;
use std::time::SystemTime;

lazy_static! {
    static ref BILISTREAM_PATH: PathBuf = executable_path();
    static ref CONFIG_PATH: PathBuf = sibling_file_path(&BILISTREAM_PATH, "config.json");
    static ref LEGACY_CONFIG_PATH: PathBuf = sibling_file_path(&BILISTREAM_PATH, "config.yaml");
    static ref COOKIES_PATH: PathBuf = sibling_file_path(&BILISTREAM_PATH, "cookies.json");
    static ref CONFIG_CACHE: RwLock<Option<ConfigCacheEntry>> = RwLock::new(None);
}

const COOKIE_REFRESH_AGE_SECS: u64 = 3600 * 24 * 3;

type FileCacheKey = Option<(SystemTime, u64)>;

/// Parsed-config cache keyed on the source files' (mtime, len). load_config()
/// is called on every WebUI request and worker cycle; re-reading and re-parsing
/// the JSON sources each time is wasted work while nothing changed.
struct ConfigCacheEntry {
    source_keys: Vec<FileCacheKey>,
    config: Config,
}

fn file_cache_key(path: &Path) -> FileCacheKey {
    let metadata = fs::metadata(path).ok()?;
    Some((metadata.modified().ok()?, metadata.len()))
}

/// Keys of every file load_config() derives the Config from, in fixed order.
fn config_source_keys() -> Vec<FileCacheKey> {
    vec![file_cache_key(&CONFIG_PATH), file_cache_key(&COOKIES_PATH)]
}

fn cookies_need_refresh() -> bool {
    match file_cache_key(&COOKIES_PATH) {
        // Refresh is due when the file is older than the renewal window.
        Some((modified, _)) => modified
            .elapsed()
            .map(|age| age.as_secs() > COOKIE_REFRESH_AGE_SECS)
            .unwrap_or(false),
        // Missing cookies file requires the login flow in the full load path.
        None => true,
    }
}

fn cached_config(source_keys: &[FileCacheKey]) -> Option<Config> {
    if source_keys.first()?.is_none() || cookies_need_refresh() {
        return None;
    }

    let cache = CONFIG_CACHE.read().ok()?;
    let entry = cache.as_ref()?;
    (entry.source_keys == source_keys).then(|| entry.config.clone())
}

fn store_cached_config(source_keys: Vec<FileCacheKey>, config: &Config) {
    if let Ok(mut cache) = CONFIG_CACHE.write() {
        *cache = Some(ConfigCacheEntry {
            source_keys,
            config: config.clone(),
        });
    }
}

fn invalidate_config_cache() {
    if let Ok(mut cache) = CONFIG_CACHE.write() {
        *cache = None;
    }
}

static CONFIG_TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

fn executable_path() -> PathBuf {
    std::env::current_exe().unwrap_or_else(|_| PathBuf::from("bilistream"))
}

fn sibling_file_path(base: &Path, file_name: &str) -> PathBuf {
    base.with_file_name(file_name)
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
    pub holodex_api_key: Option<String>,
    #[serde(default)]
    pub holodex_jwt: Option<String>,
    /// Unix timestamp of last successful Holodex `/user/refresh` call.
    #[serde(default)]
    pub holodex_jwt_refreshed_at: Option<u64>,
    #[serde(default)]
    pub holodex_username: Option<String>,
    /// When true, use stored JWT as-is without expiry checks or auto-refresh.
    #[serde(default)]
    pub holodex_skip_jwt_verify: bool,
    pub riot_api_key: Option<String>,
    pub enable_lol_monitor: bool,
    pub lol_monitor_interval: Option<u64>,
    pub anti_collision_list: HashMap<String, i32>,
}

/// FFmpeg HLS timeshift cache settings.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct FfmpegCache {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_ffmpeg_cache_latency_secs")]
    pub latency_secs: u64,
}

impl Default for FfmpegCache {
    fn default() -> Self {
        Self {
            enabled: false,
            latency_secs: default_ffmpeg_cache_latency_secs(),
        }
    }
}

fn default_ffmpeg_cache_latency_secs() -> u64 {
    8
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
    #[serde(default)]
    pub proxy: Option<String>,
    #[serde(default)]
    pub crop: Option<CropConfig>,
    #[serde(default)]
    pub ffmpeg_cache: FfmpegCache,
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
    #[serde(default)]
    pub proxy: Option<String>,
    #[serde(default)]
    pub deno_path: Option<String>,
    #[serde(default)]
    pub crop: Option<CropConfig>,
    #[serde(default)]
    pub ffmpeg_cache: FfmpegCache,
}

/// Struct representing crop configuration for video filtering
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct CropConfig {
    pub width: u32,
    pub height: u32,
    pub x: u32,
    pub y: u32,
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
async fn load_credentials<P: AsRef<Path>>(path: P) -> Result<Credentials, Box<dyn Error>> {
    let file_content = tokio::fs::read_to_string(path.as_ref()).await?;
    let cookies_file: CookiesFile = serde_json::from_str(&file_content)?;
    Credentials::from_cookies(&cookies_file.cookie_info.cookies)
}

/// Loads the configuration along with credentials from cookies.json.
pub async fn load_config() -> Result<Config, Box<dyn Error>> {
    // The keys are captured before reading: a file swapped mid-load produces a
    // key mismatch on the next call, which forces a fresh parse.
    let source_keys = config_source_keys();
    if let Some(config) = cached_config(&source_keys) {
        return Ok(config);
    }

    // Try to load config.json first
    let mut config = if CONFIG_PATH.exists() {
        let config_content = tokio::fs::read_to_string(&*CONFIG_PATH).await?;
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
                proxy: None,
                crop: None,
                ffmpeg_cache: FfmpegCache::default(),
            },
            youtube: Youtube {
                enable_monitor: true, // Default to enabled for migration
                channel_name: legacy.youtube.channel_name,
                channel_id: legacy.youtube.channel_id,
                area_v2: legacy.youtube.area_v2,
                quality: legacy.youtube.quality,
                cookies_file: legacy.youtube.cookies_file,
                cookies_from_browser: legacy.youtube.cookies_from_browser,
                proxy: legacy.proxy,
                deno_path: None,
                crop: None,
                ffmpeg_cache: FfmpegCache::default(),
            },
            holodex_api_key: legacy.holodex_api_key,
            holodex_jwt: None,
            holodex_jwt_refreshed_at: None,
            holodex_username: None,
            holodex_skip_jwt_verify: false,
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
    let credentials = load_credentials(COOKIES_PATH.as_ref() as &Path).await;
    config.bililive.credentials = credentials?;

    store_cached_config(source_keys, &config);

    Ok(config)
}

/// Saves the configuration to config.json
pub async fn save_config(config: &Config) -> Result<(), Box<dyn Error>> {
    let json = serde_json::to_string_pretty(config)?;
    // write_file_atomic fsyncs; run it off the async runtime.
    let path: &'static Path = &CONFIG_PATH;
    tokio::task::spawn_blocking(move || write_file_atomic(path, json.as_bytes()))
        .await
        .map_err(|e| -> Box<dyn Error> { e.to_string().into() })??;
    invalidate_config_cache();
    crate::webui::events::publish(crate::webui::events::CONFIG);
    Ok(())
}

fn write_file_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let (tmp_path, mut tmp_file) = create_unique_tmp_file(path)?;
    let write_result = tmp_file.write_all(bytes).and_then(|_| tmp_file.sync_all());
    drop(tmp_file);

    let result = write_result.and_then(|_| fs::rename(&tmp_path, path));
    if result.is_err() {
        let _ = fs::remove_file(&tmp_path);
    }
    result
}

fn create_unique_tmp_file(path: &Path) -> std::io::Result<(PathBuf, fs::File)> {
    const MAX_ATTEMPTS: usize = 16;
    for _ in 0..MAX_ATTEMPTS {
        let tmp_path = unique_tmp_path(path);
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp_path)
        {
            Ok(file) => return Ok((tmp_path, file)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        "failed to reserve unique config temporary file",
    ))
}

fn unique_tmp_path(path: &Path) -> PathBuf {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("config.json");
    let suffix = CONFIG_TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    path.with_file_name(format!(
        "{}.tmp-{}-{}",
        file_name,
        std::process::id(),
        suffix
    ))
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
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sibling_file_path_keeps_executable_directory() {
        assert_eq!(
            sibling_file_path(Path::new("/opt/bilistream/bilistream"), "config.json"),
            PathBuf::from("/opt/bilistream/config.json")
        );
    }

    #[test]
    fn sibling_file_path_falls_back_to_relative_file_for_relative_binary() {
        assert_eq!(
            sibling_file_path(Path::new("bilistream"), "config.json"),
            PathBuf::from("config.json")
        );
    }

    #[test]
    fn unique_tmp_path_stays_next_to_target() {
        let path = PathBuf::from("/opt/bilistream/config.json");

        let first = unique_tmp_path(&path);
        let second = unique_tmp_path(&path);

        assert_ne!(first, second);
        assert_eq!(first.parent(), path.parent());
        assert_eq!(second.parent(), path.parent());
        assert!(first
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("config.json.tmp-")));
    }

    #[test]
    fn atomic_write_replaces_target_without_leftover_tmp() {
        let dir = std::env::temp_dir().join(format!(
            "bilistream-config-write-test-{}-{}",
            std::process::id(),
            CONFIG_TMP_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");

        write_file_atomic(&path, br#"{"old":true}"#).unwrap();
        write_file_atomic(&path, br#"{"new":true}"#).unwrap();

        assert_eq!(fs::read_to_string(&path).unwrap(), r#"{"new":true}"#);
        let entries = fs::read_dir(&dir)
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].path(), path);

        fs::remove_dir_all(dir).unwrap();
    }
}
