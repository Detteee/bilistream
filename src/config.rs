use crate::plugins::bilibili;
use lazy_static::lazy_static;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::error::Error;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::SystemTime;

lazy_static! {
    static ref BILISTREAM_PATH: PathBuf = executable_path();
    static ref CONFIG_PATH: PathBuf = sibling_file_path(&BILISTREAM_PATH, "config.json");
    static ref COOKIES_PATH: PathBuf = sibling_file_path(&BILISTREAM_PATH, "cookies.json");
    static ref CONFIG_CACHE: RwLock<Option<ConfigCacheEntry>> = RwLock::new(None);
}

const COOKIE_REFRESH_AGE_SECS: u64 = 3600 * 24 * 3;
static PERSISTENCE_LOCK: Mutex<()> = Mutex::new(());
static CONFIG_PUBLICATION_LOCK: RwLock<()> = RwLock::new(());
static CONFIG_REVISION: AtomicU64 = AtomicU64::new(0);

#[derive(Debug)]
pub(crate) struct ConfigSnapshot {
    json: serde_json::Value,
    revision: u64,
    source_keys: Vec<FileCacheKey>,
}

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

fn cached_config(source_keys: &[FileCacheKey]) -> Option<Config> {
    if source_keys.first()?.is_none() {
        return None;
    }

    let cache = CONFIG_CACHE.read().ok()?;
    let entry = cache.as_ref()?;
    (entry.source_keys == source_keys
        && entry
            .config
            .snapshot
            .as_ref()
            .is_some_and(|snapshot| snapshot.revision == CONFIG_REVISION.load(Ordering::Acquire)))
    .then(|| entry.config.clone())
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
    /// The read snapshot travels with clones, so all save_config callers get
    /// conflict detection without persisting runtime metadata in config.json.
    #[serde(skip)]
    pub(crate) snapshot: Option<Arc<ConfigSnapshot>>,
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
    /// When true (default), YouTube monitor/status asks Holodex first if an API
    /// key is configured. When false, yt-dlp queries YouTube directly.
    #[serde(default = "default_true")]
    pub holodex_monitor_gate: bool,
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
    #[serde(skip)]
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
    let revision = CONFIG_REVISION.load(Ordering::Acquire);
    // The keys are captured before reading: a file swapped mid-load produces a
    // key mismatch on the next call, which forces a fresh parse.
    let source_keys = config_source_keys();
    if let Some(config) = cached_config(&source_keys) {
        return Ok(config);
    }

    let mut config = read_config(&CONFIG_PATH, &COOKIES_PATH).await?;
    config.snapshot = Some(Arc::new(ConfigSnapshot {
        json: serde_json::to_value(&config)?,
        revision,
        source_keys: source_keys.clone(),
    }));

    store_cached_config(source_keys, &config);

    Ok(config)
}

async fn read_config(config_path: &Path, cookies_path: &Path) -> Result<Config, Box<dyn Error>> {
    let content = tokio::fs::read(config_path).await?;
    let mut config: Config = serde_json::from_slice(&content)?;
    // Settings reads never trigger login or renewal. Missing/invalid cookies
    // keep the setup interface available; authentication remains explicit.
    config.bililive.credentials = load_credentials(cookies_path).await.unwrap_or_default();
    Ok(config)
}

/// Saves the configuration to config.json
pub async fn save_config(config: &mut Config) -> Result<(), Box<dyn Error>> {
    save_config_inner(config, false).await
}

/// Commit an automated decision only if every input source is still current.
/// Unlike user field edits, an automated plan must not be rebased over newer
/// settings that may have disabled or redirected the operation.
pub async fn save_config_if_current(config: &mut Config) -> Result<(), Box<dyn Error>> {
    save_config_inner(config, true).await
}

async fn save_config_inner(
    config: &mut Config,
    require_current: bool,
) -> Result<(), Box<dyn Error>> {
    let edited = serde_json::to_value(&config)?;
    let snapshot = config.snapshot.clone();
    let credentials = config.bililive.credentials.clone();
    // A blocking transaction owns its lock through read/merge/fsync/rename even
    // if its HTTP caller disconnects. Unrelated edits merge; conflicting edits
    // fail explicitly instead of silently overwriting newer settings.
    let mut saved = tokio::task::spawn_blocking(move || -> std::io::Result<Config> {
        let _guard = PERSISTENCE_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if require_current
            && snapshot
                .as_ref()
                .is_none_or(|snapshot| !snapshot_is_current(snapshot))
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::WouldBlock,
                "configuration changed while planning the operation",
            ));
        }
        let json =
            commit_config_snapshot(&CONFIG_PATH, snapshot.as_deref().map(|s| &s.json), &edited)?;
        let mut saved: Config = serde_json::from_value(json.clone())?;
        let _publication = CONFIG_PUBLICATION_LOCK
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let revision = CONFIG_REVISION.fetch_add(1, Ordering::AcqRel) + 1;
        saved.snapshot = Some(Arc::new(ConfigSnapshot {
            json,
            revision,
            source_keys: config_source_keys(),
        }));
        invalidate_config_cache();
        crate::webui::events::publish(crate::webui::events::CONFIG);
        crate::webui::state::request_status_refresh();
        Ok(saved)
    })
    .await
    .map_err(|error| -> Box<dyn Error> { error.to_string().into() })??;
    saved.bililive.credentials = load_credentials(&*COOKIES_PATH)
        .await
        .unwrap_or(credentials);
    *config = saved;
    Ok(())
}

pub fn config_is_current(config: &Config) -> bool {
    config
        .snapshot
        .as_ref()
        .is_some_and(|snapshot| snapshot_is_current(snapshot))
}

fn snapshot_is_current(snapshot: &ConfigSnapshot) -> bool {
    snapshot.revision == CONFIG_REVISION.load(Ordering::Acquire)
        && snapshot.source_keys == config_source_keys()
}

/// Apply a synchronous runtime update only while its source configuration is
/// current, excluding a concurrent commit between validation and publication.
pub fn with_current_config<T>(config: &Config, update: impl FnOnce() -> T) -> Option<T> {
    let _guard = CONFIG_PUBLICATION_LOCK
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    config_is_current(config).then(update)
}

fn commit_config_snapshot(
    path: &Path,
    base: Option<&serde_json::Value>,
    edited: &serde_json::Value,
) -> std::io::Result<serde_json::Value> {
    let current = match fs::read(path) {
        Ok(bytes) => {
            let parsed: Config = serde_json::from_slice(&bytes)?;
            Some(serde_json::to_value(parsed)?)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error),
    };
    let merged = merge_config_changes(base, Some(edited), current.as_ref(), "config")?
        .ok_or_else(|| std::io::Error::other("configuration was removed during edit"))?;
    write_file_atomic(path, &serde_json::to_vec_pretty(&merged)?)?;
    Ok(merged)
}

fn merge_config_changes(
    base: Option<&serde_json::Value>,
    edited: Option<&serde_json::Value>,
    current: Option<&serde_json::Value>,
    path: &str,
) -> std::io::Result<Option<serde_json::Value>> {
    if base == edited {
        return Ok(current.cloned());
    }
    if current == base || current == edited {
        return Ok(edited.cloned());
    }
    if let (
        Some(serde_json::Value::Object(base)),
        Some(serde_json::Value::Object(edited)),
        Some(serde_json::Value::Object(current)),
    ) = (base, edited, current)
    {
        let mut merged = current.clone();
        let keys: std::collections::BTreeSet<_> = base.keys().chain(edited.keys()).collect();
        for key in keys {
            match merge_config_changes(
                base.get(key),
                edited.get(key),
                current.get(key),
                &format!("{path}.{key}"),
            )? {
                Some(value) => {
                    merged.insert(key.clone(), value);
                }
                None => {
                    merged.remove(key);
                }
            }
        }
        return Ok(Some(serde_json::Value::Object(merged)));
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::WouldBlock,
        format!("{path} changed during edit; reload and retry"),
    ))
}

/// One serialized read/edit/atomic-write operation for managed JSON files.
pub async fn mutate_json_file<T, R, F>(path: PathBuf, edit: F) -> Result<R, String>
where
    T: serde::de::DeserializeOwned + Serialize + Send + 'static,
    R: Send + 'static,
    F: FnOnce(&mut T) -> Result<R, String> + Send + 'static,
{
    tokio::task::spawn_blocking(move || {
        let _guard = PERSISTENCE_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut data: T = serde_json::from_slice(&fs::read(&path).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?;
        let result = edit(&mut data)?;
        let bytes = serde_json::to_vec_pretty(&data).map_err(|e| e.to_string())?;
        write_file_atomic(&path, &bytes).map_err(|e| e.to_string())?;
        let _publication = CONFIG_PUBLICATION_LOCK
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        CONFIG_REVISION.fetch_add(1, Ordering::AcqRel);
        invalidate_config_cache();
        crate::plugins::set_config_updated();
        crate::webui::state::request_status_refresh();
        Ok(result)
    })
    .await
    .map_err(|e| e.to_string())?
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

pub async fn refresh_credentials() -> Result<(), Box<dyn std::error::Error>> {
    // Check for the existence of cookies.json
    if !COOKIES_PATH.exists() {
        return Err("cookies.json is missing; complete login in the setup UI".into());
    } else {
        // Check if cookies.json is older than 3 days
        if COOKIES_PATH
            .metadata()?
            .modified()?
            .elapsed()
            .unwrap_or_default()
            .as_secs()
            > COOKIE_REFRESH_AGE_SECS
        {
            tracing::info!("cookies.json 已超过3天，正在刷新");
            bilibili::renew().await?;
        }
    }

    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;

    fn test_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "bilistream-config-review-{}-{}",
            std::process::id(),
            CONFIG_TMP_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn fixture_json() -> serde_json::Value {
        let config: Config = serde_json::from_value(serde_json::json!({
            "auto_cover": false, "enable_anti_collision": false, "interval": 15,
            "bililive": { "enable_danmaku_command": false, "room": 1, "bili_rtmp_url": "", "bili_rtmp_key": "" },
            "youtube": {}, "twitch": {}, "enable_lol_monitor": false, "anti_collision_list": {}
        })).unwrap();
        serde_json::to_value(config).unwrap()
    }

    #[tokio::test]
    async fn settings_are_readable_without_credentials() {
        let dir = test_dir();
        let path = dir.join("config.json");
        write_file_atomic(&path, &serde_json::to_vec(&fixture_json()).unwrap()).unwrap();
        let config = read_config(&path, &dir.join("cookies.json")).await.unwrap();
        assert_eq!(config.interval, 15);
        assert!(config.bililive.credentials.sessdata.is_empty());
        fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn concurrent_config_edits_preserve_unrelated_fields() {
        let dir = test_dir();
        let path = dir.join("config.json");
        let base = fixture_json();
        write_file_atomic(&path, &serde_json::to_vec(&base).unwrap()).unwrap();
        let mut tasks = Vec::new();
        for id in 0..10 {
            let (path, base) = (path.clone(), base.clone());
            tasks.push(tokio::task::spawn_blocking(move || {
                let mut edited = base.clone();
                edited["anti_collision_list"][format!("room-{id}")] = serde_json::json!(id);
                let _guard = PERSISTENCE_LOCK.lock().unwrap();
                commit_config_snapshot(&path, Some(&base), &edited).unwrap();
            }));
        }
        for task in tasks {
            task.await.unwrap();
        }
        let current: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(
            current["anti_collision_list"].as_object().unwrap().len(),
            10
        );
        let mut first = base.clone();
        first["interval"] = serde_json::json!(20);
        let mut stale = base.clone();
        stale["interval"] = serde_json::json!(30);
        commit_config_snapshot(&path, Some(&base), &first).unwrap();
        assert_eq!(
            commit_config_snapshot(&path, Some(&base), &stale)
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::WouldBlock
        );
        let current: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(current["interval"], 20);
        assert_eq!(
            current["anti_collision_list"].as_object().unwrap().len(),
            10
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn merge_rejects_conflicting_deletion_and_preserves_other_keys() {
        let base = serde_json::json!({"a": 1, "b": [1]});
        let edited = serde_json::json!({"b": [1]});
        let current = serde_json::json!({"a": 2, "b": [1]});
        assert!(
            merge_config_changes(Some(&base), Some(&edited), Some(&current), "config").is_err()
        );
        let current = serde_json::json!({"a": 1, "b": [2], "c": true});
        assert_eq!(
            merge_config_changes(Some(&base), Some(&edited), Some(&current), "config").unwrap(),
            Some(serde_json::json!({"b": [2], "c": true}))
        );
    }

    #[tokio::test]
    async fn cancelled_caller_does_not_abandon_a_managed_transaction() {
        let dir = test_dir();
        let path = dir.join("counter.json");
        write_file_atomic(&path, b"0").unwrap();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let first_path = path.clone();
        let caller = tokio::spawn(async move {
            mutate_json_file(first_path, move |count: &mut u64| {
                started_tx.send(()).unwrap();
                release_rx
                    .recv_timeout(std::time::Duration::from_secs(5))
                    .unwrap();
                *count += 1;
                Ok(())
            })
            .await
        });
        started_rx.await.unwrap();
        caller.abort();
        assert!(caller.await.unwrap_err().is_cancelled());
        release_tx.send(()).unwrap();
        mutate_json_file(path.clone(), |count: &mut u64| {
            *count += 1;
            Ok(())
        })
        .await
        .unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "2");
        fs::remove_dir_all(dir).unwrap();
    }

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
