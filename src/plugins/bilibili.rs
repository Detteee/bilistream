#![allow(non_snake_case)]

use crate::config::{save_config, Config, Credentials};
use chrono::TimeZone;
use lazy_static::lazy_static;
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
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tracing::warn;

lazy_static! {
    static ref BILI_PLAIN_CLIENT: reqwest::Client = reqwest::Client::new();
}

/// Shared (raw, retrying) client pair for credential-free live-status calls.
static BILI_STATUS_CLIENTS: OnceLock<(reqwest::Client, reqwest_middleware::ClientWithMiddleware)> =
    OnceLock::new();

fn bili_status_clients(
) -> Result<(reqwest::Client, reqwest_middleware::ClientWithMiddleware), reqwest::Error> {
    if let Some((raw, retrying)) = BILI_STATUS_CLIENTS.get() {
        return Ok((raw.clone(), retrying.clone()));
    }

    let retry_policy = ExponentialBackoff::builder().build_with_max_retries(5);
    let raw = reqwest::Client::builder()
        .cookie_store(true)
        .timeout(Duration::new(30, 0))
        .build()?;
    let retrying = ClientBuilder::new(raw.clone())
        .with(RetryTransientMiddleware::new_with_policy(retry_policy))
        .build();
    let (raw, retrying) = BILI_STATUS_CLIENTS.get_or_init(|| (raw, retrying));
    Ok((raw.clone(), retrying.clone()))
}

/// Reuse the shared plain HTTP client (no cookie store, default timeouts).
pub(crate) fn bili_plain_http_client() -> reqwest::Client {
    BILI_PLAIN_CLIENT.clone()
}

/// Shared (raw, retrying) client pair for credentialed live-room calls.
///
/// The account cookies ride in an explicit `Cookie` header per request rather
/// than a per-call cookie jar, so one client serves every caller and keeps its
/// connection pool warm across stream lifecycles.
static BILI_ROOM_CLIENTS: OnceLock<(reqwest::Client, reqwest_middleware::ClientWithMiddleware)> =
    OnceLock::new();

fn bili_room_clients(
) -> Result<(reqwest::Client, reqwest_middleware::ClientWithMiddleware), reqwest::Error> {
    if let Some((raw, retrying)) = BILI_ROOM_CLIENTS.get() {
        return Ok((raw.clone(), retrying.clone()));
    }

    let retry_policy = ExponentialBackoff::builder().build_with_max_retries(5);
    let raw = reqwest::Client::builder()
        .timeout(Duration::new(30, 0))
        .build()?;
    let retrying = ClientBuilder::new(raw.clone())
        .with(RetryTransientMiddleware::new_with_policy(retry_policy))
        .build();
    let (raw, retrying) = BILI_ROOM_CLIENTS.get_or_init(|| (raw, retrying));
    Ok((raw.clone(), retrying.clone()))
}

/// `Cookie` header value carrying the configured Bilibili account credentials.
fn bili_credential_cookie(credentials: &Credentials) -> String {
    format!(
        "SESSDATA={};bili_jct={};DedeUserID={};DedeUserID__ckMd5={}",
        credentials.sessdata,
        credentials.bili_jct,
        credentials.dede_user_id,
        credentials.dede_user_id_ckmd5
    )
}

/// Both history endpoints read the logged-in account's room. An empty cookie
/// or a different local room must not consume a shared summary claim.
pub(crate) fn history_account_ready(cfg: &Config, room: u64) -> bool {
    room != 0
        && cfg.bililive.room.max(0) as u64 == room
        && !cfg.bililive.credentials.sessdata.trim().is_empty()
}

/// The live-client (version, build) pair changes rarely; cache successful
/// probes for a day instead of fetching on every stream start.
static BILI_LIVE_VERSION_CACHE: Mutex<Option<(Instant, (String, i64))>> = Mutex::new(None);
const BILI_LIVE_VERSION_TTL: Duration = Duration::from_secs(24 * 60 * 60);

async fn bili_live_version() -> Result<(String, i64), Box<dyn Error>> {
    if let Ok(cache) = BILI_LIVE_VERSION_CACHE.lock() {
        if let Some((fetched_at, version)) = cache.as_ref() {
            if fetched_at.elapsed() < BILI_LIVE_VERSION_TTL {
                return Ok(version.clone());
            }
        }
    }

    let version_api =
        "https://api.live.bilibili.com/xlive/app-blink/v1/liveVersionInfo/getHomePageLiveVersion";
    let version_ts = chrono::Utc::now().timestamp().to_string();
    let version_query = format!(
        "system_version=2&ts={}&appKey=aae92bc66f3edfab&sign=",
        version_ts
    );
    let version_url = format!("{}?{}", version_api, version_query);

    let version_resp: serde_json::Value = BILI_PLAIN_CLIENT
        .get(&version_url)
        .send()
        .await?
        .json()
        .await?;

    if version_resp["code"].as_i64() == Some(0) {
        let data = &version_resp["data"];
        let version = (
            data["curr_version"]
                .as_str()
                .unwrap_or("7.19.0.9432")
                .to_string(),
            data["build"].as_i64().unwrap_or(9432),
        );
        if let Ok(mut cache) = BILI_LIVE_VERSION_CACHE.lock() {
            *cache = Some((Instant::now(), version.clone()));
        }
        Ok(version)
    } else {
        Ok(("7.19.0.9432".to_string(), 9432))
    }
}
pub const BILI_START_TEMP_BAN_PREFIX: &str = "BILI_START_TEMP_BAN:";

fn unix_time_secs(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

fn current_unix_time_secs() -> u64 {
    unix_time_secs(SystemTime::now())
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

/// Room snapshot from `room/v1/Room/get_info`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct BiliLiveStatus {
    pub is_live: bool,
    pub title: String,
    pub area_id: u64,
    pub area_name: String,
    pub online: u64,
    pub live_start_ts: Option<i64>,
}

/// Room punishment snapshot from `room/v1/Room/room_init`.
///
/// `get_info` does not carry the ban. `lock_till` is the Unix second the lock
/// ends; `0` means the response did not give an end time.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BiliRoomLock {
    pub is_locked: bool,
    pub lock_till: i64,
    pub hidden_till: i64,
    pub live_status: i64,
}

/// One extra second so startLive is not attempted in the same second the lock lifts.
const ROOM_LOCK_GRACE_SECS: i64 = 1;

static ANNOUNCED_LOCK_TILL: AtomicI64 = AtomicI64::new(-1);

fn json_string(value: &Value) -> String {
    value.as_str().unwrap_or_default().to_string()
}

fn json_u64(value: &Value) -> u64 {
    value
        .as_u64()
        .or_else(|| value.as_i64().and_then(|n| u64::try_from(n).ok()))
        .unwrap_or(0)
}

/// Parses Bilibili's naive `live_time` (`YYYY-MM-DD HH:MM:SS`, local wall clock).
/// `"0000-00-00 00:00:00"` means the room is not live.
fn parse_bili_live_time_str(live_time_str: &str) -> Option<chrono::DateTime<chrono::Local>> {
    if live_time_str.is_empty() || live_time_str.starts_with("0000") || live_time_str == "0" {
        return None;
    }
    let naive = chrono::NaiveDateTime::parse_from_str(live_time_str, "%Y-%m-%d %H:%M:%S").ok()?;
    chrono::Local.from_local_datetime(&naive).single()
}

pub(crate) fn parse_bili_live_status(res: &Value) -> Result<BiliLiveStatus, Box<dyn Error>> {
    let data = &res["data"];
    if data.is_null() {
        return Err("直播状态接口返回缺少 data".into());
    }

    let area_id = data["area_id"]
        .as_u64()
        .ok_or("Missing or invalid area_id in API response")?;

    Ok(BiliLiveStatus {
        is_live: data["live_status"] == 1,
        title: json_string(&data["title"]),
        area_id,
        area_name: json_string(&data["area_name"]),
        online: json_u64(&data["online"]),
        live_start_ts: parse_bili_live_time_str(&json_string(&data["live_time"]))
            .map(|dt| dt.timestamp()),
    })
}

fn json_i64(value: &Value) -> i64 {
    value
        .as_i64()
        .or_else(|| value.as_u64().and_then(|n| i64::try_from(n).ok()))
        .or_else(|| value.as_str().and_then(|text| text.parse().ok()))
        .unwrap_or(0)
}

/// Parses `room/v1/Room/room_init`. A locked room's `lock_till` is the ban end.
pub fn parse_bili_room_lock(res: &Value) -> Result<BiliRoomLock, Box<dyn Error>> {
    let code = res["code"].as_i64().unwrap_or(0);
    if code != 0 {
        let message = res["message"]
            .as_str()
            .or_else(|| res["msg"].as_str())
            .unwrap_or("Unknown error");
        return Err(format!("room_init 失败（错误码 {code}）：{message}").into());
    }
    let data = &res["data"];
    if data.is_null() {
        return Err("room_init 接口返回缺少 data".into());
    }
    Ok(BiliRoomLock {
        is_locked: data["is_locked"].as_bool().unwrap_or(false),
        lock_till: json_i64(&data["lock_till"]),
        hidden_till: json_i64(&data["hidden_till"]),
        live_status: json_i64(&data["live_status"]),
    })
}

/// How long to wait before the next start attempt.
///
/// `None` when the room is not locked, or the ban has already ended, or the
/// response did not include an end time (`lock_till <= 0`).
pub fn timed_room_lock_wait_at(lock: &BiliRoomLock, now_unix: i64) -> Option<Duration> {
    if !lock.is_locked || lock.lock_till <= 0 || lock.lock_till <= now_unix {
        return None;
    }
    let secs = (lock.lock_till - now_unix).saturating_add(ROOM_LOCK_GRACE_SECS);
    Some(Duration::from_secs(secs as u64))
}

pub fn timed_room_lock_wait(lock: &BiliRoomLock) -> Option<Duration> {
    timed_room_lock_wait_at(lock, current_unix_time_secs() as i64)
}

fn format_lock_end(lock_till: i64) -> Option<String> {
    chrono::Local
        .timestamp_opt(lock_till, 0)
        .single()
        .map(|dt| dt.format("%Y-%m-%d %H:%M:%S").to_string())
}

pub fn room_lock_log_line(room: i32, lock: &BiliRoomLock) -> String {
    let end = format_lock_end(lock.lock_till).unwrap_or_else(|| "未知".to_string());
    format!(
        "B站房间 {} 封禁中，解封时间 {}，解封前不再尝试开播（room_init is_locked={} lock_till={} hidden_till={} live_status={}）",
        room, end, lock.is_locked, lock.lock_till, lock.hidden_till, lock.live_status
    )
}

/// Logs the room_init lock once per `lock_till`. A later ban with a new end time logs again.
pub fn announce_room_lock(room: i32, lock: &BiliRoomLock) {
    if ANNOUNCED_LOCK_TILL.swap(lock.lock_till, Ordering::Relaxed) == lock.lock_till {
        return;
    }
    tracing::error!("{}", room_lock_log_line(room, lock));
    crate::history::emit(crate::history::EventKind::RoomLocked {
        room: room.max(0) as u64,
        until: lock.lock_till,
    });
}

/// Retrieves the room lock from public `room_init`. No credentials.
pub async fn get_bili_room_lock(room: i32) -> Result<BiliRoomLock, Box<dyn Error>> {
    // One attempt. The retrying client would stall the monitor for every pass.
    let (client, _) = bili_status_clients()?;
    let response = client
        .get(format!(
            "https://api.live.bilibili.com/room/v1/Room/room_init?id={room}"
        ))
        .send()
        .await?;
    let res: Value = crate::plugins::http::response_json_limited(response).await?;
    parse_bili_room_lock(&res)
}

/// Retrieves the live status of a Bilibili room from `get_info`.
pub async fn get_bili_live_status(room: i32) -> Result<BiliLiveStatus, Box<dyn Error>> {
    // Reuse the shared clients; this runs every monitor cycle.
    let (raw_client, client) = bili_status_clients()?;
    let url = bili_live_status_url(&raw_client, room).await?;

    let observed_at = crate::storage::now();
    let res: Value = client.get(url).send().await?.json().await?;
    let status = parse_bili_live_status(&res)?;
    observe_live_status_reply(room, &res, observed_at);
    Ok(status)
}

/// Cluster health sampling must not wait for the monitor's five retries.
/// The caller bounds the entire attempt, including signing-key lookup and body.
pub(crate) async fn get_bili_live_status_once(room: i32) -> Result<BiliLiveStatus, Box<dyn Error>> {
    let (client, _) = bili_status_clients()?;
    let url = bili_live_status_url(&client, room).await?;
    let observed_at = crate::storage::now();
    let res: Value = client
        .get(url)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let status = parse_bili_live_status(&res)?;
    observe_live_status_reply(room, &res, observed_at);
    Ok(status)
}

fn observe_live_status_reply(room: i32, reply: &Value, observed_at: u64) {
    if reply.get("code").and_then(Value::as_i64) != Some(0) {
        return;
    }
    let live = match reply.pointer("/data/live_status").and_then(Value::as_i64) {
        Some(1) => true,
        Some(0 | 2) => false,
        _ => return,
    };
    crate::history::bili::observe_room_status_at(room.max(0) as u64, live, observed_at);
}

async fn bili_live_status_url(
    client: &reqwest::Client,
    room: i32,
) -> Result<String, Box<dyn Error>> {
    let mut params = BTreeMap::new();
    params.insert("room_id", room.to_string());
    let query_string = super::wbi::signed_query(client, params).await?;
    Ok(format!(
        "https://api.live.bilibili.com/room/v1/Room/get_info?{}",
        query_string
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
    let seq = crate::history::open_session_seq(cfg.bililive.room.max(0) as u64);
    let result = bili_start_live_inner(cfg, area_v2).await;
    record_start_live_outcome(seq, &result);
    result
}

fn record_start_live_outcome(seq: Option<u64>, result: &Result<(), Box<dyn Error>>) {
    if let Err(error) = result {
        crate::history::emit(crate::history::EventKind::BiliStartFailed {
            seq,
            error: crate::logging::safe_diagnostic(&error.to_string()),
        });
    }
}

async fn bili_start_live_inner(cfg: &mut Config, area_v2: u64) -> Result<(), Box<dyn Error>> {
    let secret = "af125a0d5279fd576c1b4418a3e8276d";
    let appkey = "aae92bc66f3edfab"; // BiliTV appkey
    let platform = "pc_link";
    let ts = chrono::Utc::now().timestamp().to_string();

    // 获取直播姬版本号和 build（缓存有效期内直接复用）
    let (version, build) = bili_live_version().await?;

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
    let sign = super::wbi::md5_hex(&format!("{}{}", query_string, secret));

    // Add sign to params
    params.insert("sign", sign.clone());

    // Build the final query string with all parameters including sign
    let query_string = params
        .iter()
        .map(|(k, v)| format!("{}={}", k, v))
        .collect::<Vec<_>>()
        .join("&");

    // Prepare cookies
    let cookie = bili_credential_cookie(&cfg.bililive.credentials);
    let (client, _) = bili_room_clients()?;

    // POST to the endpoint with query parameters in URL
    let response: Value = client
        .post(format!(
            "https://api.live.bilibili.com/room/v1/Room/startLive?{}",
            query_string
        ))
        .header("Accept", "application/json, text/plain, */*")
        .header("Cookie", &cookie)
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
            60024 | 60043 => {
                // Face verification required
                let face_auth_url = response["data"]["qr"]
                    .as_str()
                    .map(str::trim)
                    .filter(|url| !url.is_empty())
                    .map(str::to_string)
                    .or_else(|| {
                        if code == 60043 {
                            let mid = cfg.bililive.credentials.dede_user_id.trim();
                            if !mid.is_empty() {
                                Some(format!(
                                    "https://www.bilibili.com/blackboard/live/face-auth-middle.html?source_event=400&mid={}",
                                    mid
                                ))
                            } else {
                                Some(
                                    "https://www.bilibili.com/blackboard/live/face-auth-middle.html?source_event=400"
                                        .to_string(),
                                )
                            }
                        } else {
                            None
                        }
                    });

                tracing::error!("Bilibili 开播失败（错误码 {}）：{}", code, message);

                if let Some(face_auth_url) = face_auth_url {
                    return Err(format!("FACE_AUTH_REQUIRED:{}", face_auth_url).into());
                }

                return Err(format!("开播失败（错误码 {}）：{}", code, message).into());
            }
            60031 => {
                // Temporary ban. The end time is not in this body; room_init.lock_till is.
                tracing::error!("Bilibili 开播失败（错误码 {}）", code);
                tracing::error!("{}", message);
                return Err(format!("{}{}", BILI_START_TEMP_BAN_PREFIX, message).into());
            }
            _ => {
                tracing::error!("Bilibili 开播失败（错误码 {}）：{}", code, message);
                return Err(format!("开播失败（错误码 {}）：{}", code, message).into());
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

                    // Save the updated config through the shared atomic path.
                    save_config(cfg).await?;

                    // tracing::info!("已更新配置中的 RTMP 推流信息");
                }
            }
        }
    }

    // 统计: the B站 live session id goes into the cluster live book only
    // (never logged).
    if response["code"].as_i64() == Some(0) {
        // Even when startLive omits its key, remember this live transition
        // before the relay's asynchronous GetInfo fallback captures it.
        crate::history::bili::observe_room_status(cfg.bililive.room.max(0) as u64, true);
        if let Some(key) = crate::history::enrich::parse_live_key(&response["data"]["live_key"]) {
            crate::history::on_bili_started(cfg, &key);
        }
    }

    // Reflect the new live state in the WebUI immediately; the periodic
    // refresh fills in title/area details afterwards.
    crate::webui::state::update_status_cache_with(|status| {
        status.bilibili.is_live = true;
    });
    crate::webui::state::request_status_refresh();

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
    let cookie = bili_credential_cookie(&cfg.bililive.credentials);
    let (_, client) = bili_room_clients()?;

    // Make the POST request to update the live title
    let _res: Value = client
        .post("https://api.live.bilibili.com/room/v1/Room/update")
        .header("Accept", "application/json, text/plain, */*")
        .header("Cookie", &cookie)
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
                    "标题审核失败：{} - 可能包含敏感词汇，请尝试其他标题",
                    message
                )
                .into());
            }

            return Err(format!("API 调用失败（code {}）：{}", code, message).into());
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
    let cookie = bili_credential_cookie(&cfg.bililive.credentials);
    let (_, client) = bili_room_clients()?;

    // Make the POST request to stop the live stream
    let response = client
        .post("https://api.live.bilibili.com/room/v1/Room/stopLive")
        .header("Accept", "application/json, text/plain, */*")
        .header("Cookie", &cookie)
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
        .error_for_status()?;
    let reply: Value = crate::plugins::http::response_json_limited(response).await?;
    validate_stop_live_reply(&reply)?;

    // Reflect the new live state in the WebUI immediately.
    crate::webui::state::update_status_cache_with(|status| {
        status.bilibili.is_live = false;
    });
    crate::webui::state::request_status_refresh();

    crate::history::on_bili_stopped(cfg);
    Ok(())
}

fn validate_stop_live_reply(reply: &Value) -> Result<(), Box<dyn Error>> {
    match reply.get("code").and_then(Value::as_i64) {
        Some(0) => Ok(()),
        Some(code) => Err(format!("B站下播失败（错误码 {code}）").into()),
        None => Err("B站下播响应缺少状态码".into()),
    }
}

/// Error text for history fetches: never the URL (it carries the live key).
fn safe_fetch_error(error: reqwest::Error) -> String {
    if error.is_timeout() {
        "timeout".into()
    } else if error.is_decode() {
        "invalid json".into()
    } else if error.is_status() {
        format!("http {}", error.status().map_or(0, |s| s.as_u16()))
    } else {
        "request failed".into()
    }
}

/// 统计: `StopLiveData` for one B站 live session (read-only GET, 10 s).
/// Returns the raw reply; `history::enrich` applies the allowlist.
pub(crate) async fn bili_stop_live_data(cfg: &Config, live_key: &str) -> Result<Value, String> {
    if !history_account_ready(cfg, cfg.bililive.room.max(0) as u64) {
        return Err("未配置可用的B站登录凭据".into());
    }
    let cookie = bili_credential_cookie(&cfg.bililive.credentials);
    let (client, _) = bili_room_clients().map_err(safe_fetch_error)?;
    let url = reqwest::Url::parse_with_params(
        "https://api.live.bilibili.com/xlive/app-blink/v1/live/StopLiveData",
        &[("live_key", live_key)],
    )
    .map_err(|_| "invalid key".to_string())?;
    history_json(
        client
            .get(url)
            .header("Accept", "application/json, text/plain, */*")
            .header("Cookie", &cookie),
    )
    .await
}

/// 统计: read-only `GetInfo` for the account's room, the fallback source
/// of `live_key` when the cluster live book has none (10 s timeout).
pub(crate) async fn bili_room_info(cfg: &Config) -> Result<Value, String> {
    if !history_account_ready(cfg, cfg.bililive.room.max(0) as u64) {
        return Err("未配置可用的B站登录凭据".into());
    }
    let cookie = bili_credential_cookie(&cfg.bililive.credentials);
    let (client, _) = bili_room_clients().map_err(safe_fetch_error)?;
    history_json(
        client
            .get("https://api.live.bilibili.com/xlive/app-blink/v1/room/GetInfo?platform=pc")
            .header("Accept", "application/json, text/plain, */*")
            .header("Cookie", &cookie),
    )
    .await
}

/// The request timeout also covers streaming the bounded response body.
/// Error classes deliberately omit the URL, cookie, response and live key.
async fn history_json(request: reqwest::RequestBuilder) -> Result<Value, String> {
    let response = request
        .timeout(crate::history::enrich::FETCH_TIMEOUT)
        .send()
        .await
        .map_err(safe_fetch_error)?
        .error_for_status()
        .map_err(safe_fetch_error)?;
    crate::plugins::http::response_json_limited(response)
        .await
        .map_err(|_| "invalid json".to_string())
}

/// `msg/send` codes for "sending too fast". 10031 is documented; 10030 is the
/// older code for the same cooldown.
pub const DANMAKU_RATE_LIMIT_CODES: [i64; 2] = [10030, 10031];

/// Wait before the single retry after a rate-limit rejection; the live chat
/// cooldown is a little over one second.
const DANMAKU_RATE_LIMIT_BACKOFF: Duration = Duration::from_secs(2);

/// A danmaku that Bilibili answered with a non-zero code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DanmakuRejected {
    pub code: i64,
    pub message: String,
}

impl std::fmt::Display for DanmakuRejected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Failed to send danmaku: code {}: {}",
            self.code, self.message
        )
    }
}

impl Error for DanmakuRejected {}

/// The Bilibili rejection code inside a `send_danmaku` error, if it was one.
pub fn danmaku_rejection_code(error: &(dyn Error + 'static)) -> Option<i64> {
    error.downcast_ref::<DanmakuRejected>().map(|r| r.code)
}

/// Only a rate-limit rejection is worth one more try; e.g. 1003212 (too long)
/// can never succeed on a resend.
pub fn danmaku_should_retry(code: i64) -> bool {
    DANMAKU_RATE_LIMIT_CODES.contains(&code)
}

/// The `msg/send` body, form-encoded so `&`, `+`, `%` and `=` in the text
/// cannot split or truncate it.
pub fn danmaku_form_body(message: &str, room: i32, csrf: &str, rnd: i64) -> String {
    let room = room.to_string();
    let rnd = rnd.to_string();
    serde_urlencoded::to_string([
        ("bubble", "0"),
        ("msg", message),
        ("color", "16777215"),
        ("mode", "1"),
        ("fontsize", "25"),
        ("rnd", rnd.as_str()),
        ("roomid", room.as_str()),
        ("csrf_token", csrf),
        ("csrf", csrf),
    ])
    .expect("string pairs always form-encode")
}

async fn send_danmaku_once(cfg: &Config, message: &str) -> Result<Value, Box<dyn Error>> {
    let client = bili_plain_http_client();
    let cookie = bili_credential_cookie(&cfg.bililive.credentials);
    let body = danmaku_form_body(
        message,
        cfg.bililive.room,
        &cfg.bililive.credentials.bili_jct,
        chrono::Local::now().timestamp(),
    );
    let resp: Value = client
        .post("https://api.live.bilibili.com/msg/send")
        .header("Cookie", &cookie)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(body)
        .send()
        .await?
        .json()
        .await?;

    match resp["code"].as_i64() {
        Some(0) => Ok(resp),
        code => Err(Box::new(DanmakuRejected {
            code: code.unwrap_or(-1),
            message: resp["message"]
                .as_str()
                .or_else(|| resp["msg"].as_str())
                .unwrap_or("")
                .to_string(),
        })),
    }
}

/// Runs `send`, and once more after `backoff` only when the first attempt was
/// rejected for sending too fast. Other failures are returned as they are.
pub(crate) async fn send_with_one_retry<F, Fut>(
    mut send: F,
    backoff: Duration,
) -> Result<Value, Box<dyn Error>>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<Value, Box<dyn Error>>>,
{
    let code = match send().await {
        Ok(value) => return Ok(value),
        Err(error) => match danmaku_rejection_code(error.as_ref()) {
            Some(code) if danmaku_should_retry(code) => code,
            _ => return Err(error),
        },
    };
    tracing::info!(
        "弹幕发送过快（code {code}），{} 秒后重试一次",
        backoff.as_secs()
    );
    tokio::time::sleep(backoff).await;
    send().await
}

pub async fn send_danmaku(
    cfg: &Config,
    message: &str,
) -> Result<Value, Box<dyn std::error::Error>> {
    send_with_one_retry(
        || send_danmaku_once(cfg, message),
        DANMAKU_RATE_LIMIT_BACKOFF,
    )
    .await
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
    let file_content = tokio::fs::read(image_path).await?;
    bili_change_cover_bytes(cfg, file_content).await
}

pub(crate) async fn bili_change_cover_bytes(
    cfg: &Config,
    file_content: Vec<u8>,
) -> Result<(), Box<dyn Error>> {
    let cookie = bili_credential_cookie(&cfg.bililive.credentials);
    let (client, _) = bili_room_clients()?;

    // Step 1: Upload image
    let form = reqwest::multipart::Form::new()
        .text("csrf", cfg.bililive.credentials.bili_jct.clone())
        .text("bucket", "live")
        .text("dir", "new_room_cover")
        .part(
            "file",
            reqwest::multipart::Part::bytes(file_content)
                .file_name("cover.jpg")
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
    let cookie = bili_credential_cookie(&cfg.bililive.credentials);
    let (_, client) = bili_room_clients()?;

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

/// A failed QR-login poll names only B站's code and message. The response
/// body can carry login tokens and cookies, so it is never formatted.
fn login_failed<T>(res: &ResponseData<T>) -> String {
    format!(
        "Login failed: code {} ({})",
        res.code,
        crate::logging::safe_text(&res.message, 120)
    )
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(untagged)]
pub enum ResponseValue {
    Login(LoginInfo),
    OAuth(OAuthInfo),
    Value(Value),
}

/// `Debug` is manual: cookies and tokens show only whether they are set.
#[derive(Clone, Deserialize, Serialize)]
pub struct LoginInfo {
    pub cookie_info: Value,
    pub sso: Vec<String>,
    pub token_info: TokenInfo,
    pub platform: Option<String>,
}

/// `Debug` is manual: tokens show only whether they are set.
#[derive(Clone, Deserialize, Serialize)]
pub struct TokenInfo {
    pub access_token: String,
    pub expires_in: u32,
    pub mid: u64,
    pub refresh_token: String,
}

/// `Debug` is manual: the token shows only whether it is set.
#[derive(Clone, Deserialize, Serialize)]
pub struct OAuthInfo {
    pub mid: u64,
    pub access_token: String,
    pub expires_in: u32,
    pub refresh: bool,
}

impl std::fmt::Debug for LoginInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LoginInfo")
            .field(
                "cookie_info",
                &if self.cookie_info.is_null() {
                    "[unset]"
                } else {
                    "[redacted]"
                },
            )
            .field("sso", &self.sso)
            .field("token_info", &self.token_info)
            .field("platform", &self.platform)
            .finish()
    }
}

impl std::fmt::Debug for TokenInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        use crate::config::secret;
        f.debug_struct("TokenInfo")
            .field("access_token", &secret(&self.access_token))
            .field("expires_in", &self.expires_in)
            .field("mid", &self.mid)
            .field("refresh_token", &secret(&self.refresh_token))
            .finish()
    }
}

impl std::fmt::Debug for OAuthInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OAuthInfo")
            .field("mid", &self.mid)
            .field("access_token", &crate::config::secret(&self.access_token))
            .field("expires_in", &self.expires_in)
            .field("refresh", &self.refresh)
            .finish()
    }
}

#[derive(Clone)]
struct StatefulClient {
    client: reqwest::Client,
    cookie_store: Arc<Jar>,
}

impl StatefulClient {
    fn new(headers: reqwest::header::HeaderMap) -> Result<Self, reqwest::Error> {
        let cookie_store = Arc::new(Jar::default());
        let client = reqwest::Client::builder()
            .cookie_provider(cookie_store.clone())
            .default_headers(headers)
            .build()?;

        Ok(Self {
            client,
            cookie_store,
        })
    }
}

pub struct Credential(StatefulClient);

impl Credential {
    pub fn new() -> Result<Self, reqwest::Error> {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            "Referer",
            reqwest::header::HeaderValue::from_static("https://www.bilibili.com/"),
        );
        StatefulClient::new(headers).map(Self)
    }

    pub async fn get_qrcode(&self) -> Result<Value, Box<dyn Error>> {
        let mut form = json!({
            "appkey": "4409e2ce8ffd12b8",
            "local_id": "0",
            "ts": current_unix_time_secs()
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
        super::wbi::md5_hex(&format!("{}{}", param, app_sec))
    }

    async fn login_by_qrcode(&self, value: Value) -> Result<LoginInfo, Box<dyn Error>> {
        let mut form = json!({
            "appkey": "4409e2ce8ffd12b8",
            "auth_code": value["data"]["auth_code"],
            "local_id": "0",
            "ts": current_unix_time_secs()
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
                    return Err(login_failed(&res).into());
                }
            }
        }
    }

    pub async fn renew_tokens(&self, login_info: LoginInfo) -> Result<LoginInfo, Box<dyn Error>> {
        let keypair = match login_info.platform.as_deref() {
            Some("BiliTV") => AppKeyStore::BiliTV,
            Some("Android") => AppKeyStore::Android,
            Some(_) => return Err("未知的登录平台".into()),
            None => return Err("登录信息缺少平台字段".into()),
        };

        let mut payload = json!({
            "access_key": login_info.token_info.access_token,
            "actionKey": "appkey",
            "appkey": keypair.app_key(),
            "refresh_token": login_info.token_info.refresh_token,
            "ts": current_unix_time_secs(),
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
            _ => Err("刷新B站令牌失败".into()),
        }
    }
}

/// Get QR code for web-based login
pub async fn get_login_qrcode() -> Result<(String, String), Box<dyn Error>> {
    let credential = Credential::new()?;
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
    let credential = Credential::new()?;

    let mut form = json!({
        "appkey": "4409e2ce8ffd12b8",
        "auth_code": auth_code,
        "local_id": "0",
        "ts": current_unix_time_secs()
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
        _ => Err(login_failed(&res).into()),
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
                let expires = current_unix_time_secs() as i64 + 15552000; // 180 days

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

    let store = crate::storage::global()?;
    tokio::task::spawn_blocking(move || store.write("cookies.json", final_info)).await??;
    crate::webui::state::request_status_refresh();

    Ok(())
}

/// Login to Bilibili using QR code and save cookies
pub async fn login() -> Result<(), Box<dyn Error>> {
    let credential = Credential::new()?;

    // Get QR code
    let qrcode_res = credential.get_qrcode().await?;

    // Generate and display QR code
    let qr_url = qrcode_res["data"]["url"]
        .as_str()
        .ok_or("获取二维码 URL 失败")?;

    let qr = QrCode::new(qr_url)?;
    let qr_string = qr
        .render::<char>()
        .quiet_zone(false)
        .module_dimensions(2, 1)
        .build();
    println!("请扫描二维码登录：\n{}", qr_string);

    // Wait for scan and get login info
    let login_info = credential.login_by_qrcode(qrcode_res).await?;

    // Create cookie info structure
    let mut cookies = Vec::new();
    let base_url = Url::parse("https://bilibili.com")?;

    if let Some(cookie_header) = credential.0.cookie_store.cookies(&base_url) {
        let cookie_str = cookie_header.to_str().unwrap_or_default();
        for cookie_part in cookie_str.split("; ") {
            if let Some((name, value)) = cookie_part.split_once('=') {
                let expires = current_unix_time_secs() as i64 + 15552000; // 180 days

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

    let store = crate::storage::global()?;
    tokio::task::spawn_blocking(move || store.write("cookies.json", final_info)).await??;
    crate::webui::state::request_status_refresh();
    println!("登录成功，凭据已加密保存");

    Ok(())
}

/// Renews the authentication tokens using the existing login info
pub async fn renew() -> Result<(), Box<dyn Error>> {
    let store = crate::storage::global()?;
    let snapshot = store.read("cookies.json")?.ok_or("尚未登录哔哩哔哩")?;
    let credential = Credential::new()?;
    let login_info: LoginInfo = serde_json::from_value(snapshot.value)?;
    let new_info = credential.renew_tokens(login_info).await?;
    let value = serde_json::to_value(new_info)?;
    tokio::task::spawn_blocking(move || {
        store.compare_exchange("cookies.json", snapshot.revision, value)
    })
    .await??;
    crate::webui::state::request_status_refresh();

    Ok(())
}

/// Downloads thumbnail for live streams using platform CDN URLs.
pub async fn get_thumbnail(
    platform: &str,
    channel_id: &str,
    stream_id: Option<&str>,
    proxy: Option<String>,
) -> Result<String, anyhow::Error> {
    let proxy_url = proxy.filter(|p| !p.is_empty());
    let thumbnail_url = if platform == "NC" {
        let Some(live_id) = stream_id.filter(|id| !id.is_empty()) else {
            warn!(
                "无法构建缩略图 URL: platform={}, channel_id={}, stream_id={:?}",
                platform, channel_id, stream_id
            );
            return Ok(String::new());
        };
        match super::niconico::niconico_cover_thumbnail_url(live_id, proxy_url.as_deref()).await {
            Ok(Some(url)) => url,
            Ok(None) => {
                warn!("Niconico 节目没有可用的封面缩略图 (live_id={})", live_id);
                return Ok(String::new());
            }
            Err(e) => {
                warn!("获取 Niconico 封面缩略图失败：{}", e);
                return Ok(String::new());
            }
        }
    } else {
        let Some(url) = stream_thumbnail_url(platform, channel_id, stream_id, None) else {
            warn!(
                "无法构建缩略图 URL: platform={}, channel_id={}, stream_id={:?}",
                platform, channel_id, stream_id
            );
            return Ok(String::new());
        };
        url
    };

    let client = super::http::pooled_client(proxy_url.as_deref())?;

    let mut request = client.get(&thumbnail_url);
    if platform == "NC" {
        request = request
            .header("User-Agent", super::niconico::CHANNEL_PAGE_USER_AGENT)
            .header("Referer", "https://live.nicovideo.jp/")
            .header("Accept", "image/jpeg,image/*,*/*");
    }
    let response = match request.send().await {
        Ok(response) => response,
        Err(e) => {
            warn!("下载封面失败：{}", e);
            return Ok(String::new());
        }
    };

    if !response.status().is_success() {
        warn!(
            "下载封面失败：HTTP {} from {}",
            response.status(),
            thumbnail_url
        );
        return Ok(String::new());
    }

    let bytes = match super::http::response_bytes_limited(response, 8 * 1024 * 1024).await {
        Ok(bytes) => bytes,
        Err(e) => {
            warn!("读取封面失败：{}", e);
            return Ok(String::new());
        }
    };

    let cover_path = crate::storage::cache_file("cover.jpg")?;
    if let Err(e) = tokio::fs::write(&cover_path, &bytes).await {
        warn!("保存封面失败：{}", e);
        return Ok(String::new());
    }

    Ok(cover_path.to_string_lossy().into_owned())
}

/// Build a direct CDN thumbnail URL for YT/TW streams (same logic as holodex webui).
/// Niconico listing thumbnails are not a static CDN path; see
/// `niconico::niconico_cover_thumbnail_url`.
pub fn stream_thumbnail_url(
    platform: &str,
    channel_id: &str,
    stream_id: Option<&str>,
    holodex_thumbnail: Option<&str>,
) -> Option<String> {
    if let Some(url) = holodex_thumbnail.filter(|url| !url.is_empty()) {
        return Some(url.to_string());
    }

    match platform {
        "YT" => stream_id
            .filter(|id| !id.is_empty())
            .map(|id| format!("https://i.ytimg.com/vi/{id}/sddefault.jpg")),
        "TW" if !channel_id.is_empty() => Some(format!(
            "https://static-cdn.jtvnw.net/previews-ttv/live_user_{channel_id}-640x360.jpg"
        )),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn login_and_token_debug_never_contains_a_token_or_cookie() {
        let token_info = TokenInfo {
            access_token: "synthetic-access-token-b1".into(),
            expires_in: 60,
            mid: 1,
            refresh_token: "synthetic-refresh-token-b2".into(),
        };
        let login = LoginInfo {
            cookie_info: serde_json::json!({"cookies": [{"name": "SESSDATA", "value": "synthetic-sessdata-b3"}]}),
            sso: Vec::new(),
            token_info: token_info.clone(),
            platform: None,
        };
        let oauth = OAuthInfo {
            mid: 1,
            access_token: "synthetic-access-token-b4".into(),
            expires_in: 60,
            refresh: false,
        };
        for text in [
            format!("{login:#?}"),
            format!("{token_info:?}"),
            format!("{oauth:?}"),
            format!("{:?}", ResponseValue::Login(login.clone())),
            format!("{:?}", ResponseValue::OAuth(oauth.clone())),
        ] {
            for value in [
                "synthetic-access-token-b1",
                "synthetic-refresh-token-b2",
                "synthetic-sessdata-b3",
                "synthetic-access-token-b4",
            ] {
                assert!(!text.contains(value), "{value} leaked into Debug output");
            }
            assert!(text.contains("[redacted]"));
        }
    }

    #[tokio::test]
    async fn history_requests_require_a_cookie_and_the_configured_room() {
        let mut cfg = crate::test_support::config("a", 0);
        assert!(!history_account_ready(&cfg, 1));
        assert_eq!(
            bili_room_info(&cfg).await.unwrap_err(),
            "未配置可用的B站登录凭据"
        );
        assert_eq!(
            bili_stop_live_data(&cfg, "synthetic-key")
                .await
                .unwrap_err(),
            "未配置可用的B站登录凭据"
        );
        cfg.bililive.credentials.sessdata = "synthetic-cookie".into();
        assert!(history_account_ready(&cfg, 1));
        assert!(!history_account_ready(&cfg, 2));
        cfg.bililive.room = 0;
        assert!(!history_account_ready(&cfg, 0));
        cfg.bililive.room = 1;
        cfg.bililive.credentials.sessdata = " \n".into();
        assert!(!history_account_ready(&cfg, 1));
    }

    #[test]
    fn start_live_outcome_records_failures_from_every_caller_once_with_safe_text() {
        let bus = std::sync::Arc::new(crate::events::Bus::new());
        let mut receiver = bus.register("start-live", 8, |_| true);
        bus.seal();
        crate::events::with_sync_test_bus(bus, || {
            record_start_live_outcome(None, &Ok(()));
            record_start_live_outcome(
                Some(7),
                &Err("request failed https://example.invalid?token=SYNTHETIC".into()),
            );
        });
        let event = receiver.try_recv().unwrap();
        assert!(matches!(
            &event.kind,
            crate::history::EventKind::BiliStartFailed { seq: Some(7), error }
                if error.contains("request failed") && !error.contains("SYNTHETIC")
        ));
        assert!(receiver.try_recv().is_err());
    }

    #[test]
    fn stop_response_must_confirm_success_before_room_is_marked_offline() {
        assert!(validate_stop_live_reply(&serde_json::json!({"code":0})).is_ok());
        for reply in [
            serde_json::json!({"code":-101,"message":"SYNTHETIC-cookie"}),
            serde_json::json!({}),
        ] {
            let error = validate_stop_live_reply(&reply).unwrap_err().to_string();
            assert!(!error.contains("SYNTHETIC-cookie"));
        }
    }

    #[tokio::test]
    async fn history_http_retry_uses_allowlisted_totals_and_redacted_error_classes() {
        use axum::{routing::get, Json, Router};
        use std::sync::atomic::AtomicUsize;
        use std::sync::Arc;
        crate::install_crypto_provider();
        let requests = Arc::new(AtomicUsize::new(0));
        let count = Arc::clone(&requests);
        let app = Router::new()
            .route("/summary", get(move || {
                let count = Arc::clone(&count);
                async move {
                    let fixture = if count.fetch_add(1, Ordering::Relaxed) == 0 {
                        include_str!("../history/fixtures/stoplivedata_unknown_key.json")
                    } else { include_str!("../history/fixtures/stoplivedata_live_session.json") };
                    Json(serde_json::from_str::<Value>(fixture).unwrap())
                }
            }))
            .route("/unavailable", get(|| async { (axum::http::StatusCode::SERVICE_UNAVAILABLE, "SYNTHETIC-private-body") }))
            .route("/info", get(|| async { Json(serde_json::json!({"code":0,"data":{"live_status":1,"up_session":"synthetic-up-session"}})) }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let url = format!("http://{address}/summary?live_key=SYNTHETIC-KEY");
        let totals = crate::history::enrich::fetch_with_retry_guarded(
            || history_json(client.get(&url)),
            Duration::ZERO,
            Duration::from_millis(1),
            || true,
        )
        .await;
        let crate::history::enrich::FetchResult::Finished(Some(totals)) = totals else {
            panic!("retry did not yield totals");
        };
        assert_eq!(totals.watched_count, Some(469));
        assert_eq!(requests.load(Ordering::Relaxed), 2);
        let unavailable = history_json(client.get(format!(
            "http://{address}/unavailable?live_key=SYNTHETIC-KEY"
        )))
        .await
        .unwrap_err();
        assert_eq!(unavailable, "http 503");
        let info = history_json(client.get(format!("http://{address}/info")))
            .await
            .unwrap();
        assert_eq!(
            crate::history::enrich::parse_room_live_key(&info).as_deref(),
            Some("synthetic-up-session")
        );
        let safe_info = crate::history::enrich::redact_room_info(&info).to_string();
        assert!(!safe_info.contains("synthetic-up-session"));
        assert_eq!(
            serde_json::to_value(totals)
                .unwrap()
                .as_object()
                .unwrap()
                .len(),
            6
        );
        server.abort();
    }
    use chrono::TimeZone;
    use std::sync::atomic::AtomicUsize;

    #[test]
    fn credential_cookie_carries_every_configured_field() {
        let credentials = Credentials {
            sessdata: "sess".to_string(),
            bili_jct: "jct".to_string(),
            dede_user_id: "uid".to_string(),
            dede_user_id_ckmd5: "md5".to_string(),
            ..Credentials::default()
        };

        assert_eq!(
            bili_credential_cookie(&credentials),
            "SESSDATA=sess;bili_jct=jct;DedeUserID=uid;DedeUserID__ckMd5=md5"
        );
    }

    #[test]
    fn parse_bili_live_status_keeps_room_stats_and_quoted_titles() {
        let res = serde_json::json!({
            "code": 0,
            "data": {
                "title": "Foo \"bar\"",
                "live_status": 1,
                "area_id": 371,
                "area_name": "虚拟Gamer",
                "parent_area_name": "虚拟主播",
                "online": 89012,
                "attention": 1234567,
                "live_time": "2024-01-01 12:00:00"
            }
        });

        let status = parse_bili_live_status(&res).expect("parse live room");
        assert!(status.is_live);
        assert_eq!(status.title, "Foo \"bar\"");
        assert_eq!(status.area_id, 371);
        assert_eq!(status.area_name, "虚拟Gamer");
        assert_eq!(status.online, 89012);
        assert!(status.live_start_ts.is_some());
    }

    #[test]
    fn parse_bili_live_status_treats_zero_live_time_as_offline_clock() {
        let res = serde_json::json!({
            "data": {
                "title": "idle",
                "live_status": 0,
                "area_id": 235,
                "area_name": "其他单机",
                "online": 0,
                "live_time": "0000-00-00 00:00:00"
            }
        });

        let status = parse_bili_live_status(&res).expect("parse idle room");
        assert!(!status.is_live);
        assert_eq!(status.title, "idle");
        assert_eq!(status.online, 0);
        assert_eq!(status.live_start_ts, None);
    }

    #[test]
    fn room_init_lock_till_is_the_ban_end_and_sets_the_wait() {
        let res = serde_json::json!({
            "code": 0,
            "message": "ok",
            "data": {
                "room_id": 370627,
                "is_locked": true,
                "is_hidden": false,
                "hidden_till": 0,
                "lock_till": 1790881195,
                "live_status": 0
            }
        });

        let lock = parse_bili_room_lock(&res).expect("parse room lock");
        let now = 1790881195 - 140;
        let wait = timed_room_lock_wait_at(&lock, now).expect("ban still running");

        assert!(lock.is_locked);
        assert_eq!(lock.lock_till, 1790881195);
        assert_eq!(wait, Duration::from_secs(141));
        let line = room_lock_log_line(370627, &lock);
        assert!(line.contains("lock_till=1790881195"));
        assert!(line.contains("is_locked=true"));
        assert!(line.contains("hidden_till=0"));
        let end = format_lock_end(lock.lock_till).expect("local end time");
        let parsed = chrono::NaiveDateTime::parse_from_str(&end, "%Y-%m-%d %H:%M:%S")
            .expect("formatted end");
        let back = chrono::Local
            .from_local_datetime(&parsed)
            .single()
            .expect("local end");
        assert_eq!(back.timestamp(), lock.lock_till);
    }

    #[test]
    fn an_unlocked_or_expired_room_does_not_wait() {
        let unlocked = BiliRoomLock {
            is_locked: false,
            lock_till: 1790881195,
            hidden_till: 0,
            live_status: 0,
        };
        assert!(timed_room_lock_wait_at(&unlocked, 1790880000).is_none());

        let expired = BiliRoomLock {
            is_locked: true,
            lock_till: 1790881195,
            hidden_till: 0,
            live_status: 0,
        };
        assert!(timed_room_lock_wait_at(&expired, 1790881195).is_none());
        assert!(timed_room_lock_wait_at(&expired, 1790881196).is_none());

        let untimed = BiliRoomLock {
            is_locked: true,
            lock_till: 0,
            hidden_till: 0,
            live_status: 0,
        };
        assert!(timed_room_lock_wait_at(&untimed, 1_700_000_000).is_none());
    }

    // ---- danmaku sending: encoding and retry ----

    fn decoded_fields(body: &str) -> Vec<(String, String)> {
        serde_urlencoded::from_str(body).expect("body decodes")
    }

    #[test]
    fn danmaku_body_round_trips_reserved_cjk_and_emoji() {
        let msg = "A&B+C%D=E 频道，直播结束📺👨‍👩‍👧 #?&msg=x";
        let body = danmaku_form_body(msg, 12345, "jct&+%", 1_700_000_000);
        let fields = decoded_fields(&body);
        assert_eq!(fields.len(), 9, "{body}");
        let get = |key: &str| {
            fields
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.as_str())
        };
        assert_eq!(get("msg"), Some(msg));
        assert_eq!(get("roomid"), Some("12345"));
        assert_eq!(get("rnd"), Some("1700000000"));
        assert_eq!(get("csrf"), Some("jct&+%"));
        assert_eq!(get("csrf_token"), Some("jct&+%"));
        // The injected "&msg=x" stays inside the one msg value.
        assert_eq!(fields.iter().filter(|(k, _)| k == "msg").count(), 1);
        assert!(!body.contains("A&B"), "{body}");
        assert!(body.contains("A%26B%2BC%25D%3DE"), "{body}");
    }

    #[test]
    fn danmaku_body_encodes_name_with_ampersand_and_plus() {
        let msg = "Tom & Jerry+ 直播结束，可使用弹幕指令进行换台";
        let body = danmaku_form_body(msg, 1, "jct", 0);
        assert!(body.contains("Tom+%26+Jerry%2B"), "{body}");
        let fields = decoded_fields(&body);
        assert_eq!(fields[1], ("msg".to_string(), msg.to_string()));
    }

    type ScriptedSend = std::future::Ready<Result<Value, Box<dyn Error>>>;

    /// A sender that plays `script` in order: `Ok`, a rejection code, or
    /// -999 for a transport error.
    fn scripted_sender(
        script: Vec<Result<Value, i64>>,
    ) -> (Arc<AtomicUsize>, impl FnMut() -> ScriptedSend) {
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = calls.clone();
        let sender = move || {
            let n = counter.fetch_add(1, Ordering::SeqCst);
            std::future::ready(script[n].clone().map_err(|code| {
                if code == -999 {
                    Box::<dyn Error>::from("connection reset")
                } else {
                    Box::new(DanmakuRejected {
                        code,
                        message: "x".to_string(),
                    }) as Box<dyn Error>
                }
            }))
        };
        (calls, sender)
    }

    #[tokio::test]
    async fn rate_limited_danmaku_is_retried_once() {
        let (calls, send) = scripted_sender(vec![Err(10031), Ok(json!({"code": 0}))]);
        assert!(send_with_one_retry(send, Duration::ZERO).await.is_ok());
        assert_eq!(calls.load(Ordering::SeqCst), 2);

        let (calls, send) = scripted_sender(vec![Err(10030), Ok(json!({"code": 0}))]);
        assert!(send_with_one_retry(send, Duration::ZERO).await.is_ok());
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn danmaku_retry_happens_only_once() {
        let (calls, send) = scripted_sender(vec![Err(10031), Err(10031), Ok(json!({}))]);
        let error = send_with_one_retry(send, Duration::ZERO).await.unwrap_err();
        assert_eq!(danmaku_rejection_code(error.as_ref()), Some(10031));
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn too_long_danmaku_is_not_retried() {
        let (calls, send) = scripted_sender(vec![Err(1003212), Ok(json!({}))]);
        let error = send_with_one_retry(send, Duration::ZERO).await.unwrap_err();
        assert_eq!(danmaku_rejection_code(error.as_ref()), Some(1003212));
        assert!(error.to_string().contains("Failed to send danmaku"));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(!danmaku_should_retry(1003212));
    }

    #[tokio::test]
    async fn transport_errors_and_other_codes_are_not_retried() {
        let (calls, send) = scripted_sender(vec![Err(-999), Ok(json!({}))]);
        let error = send_with_one_retry(send, Duration::ZERO).await.unwrap_err();
        assert_eq!(danmaku_rejection_code(error.as_ref()), None);
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        let (calls, send) = scripted_sender(vec![Err(-111), Ok(json!({}))]);
        assert!(send_with_one_retry(send, Duration::ZERO).await.is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
}
