//! YouTube Data API overlay for Holodex rows.
//!
//! Holodex keeps rows `upcoming` after go-live, keeps finished or never-started
//! rows `live`, and lags YouTube in both directions. `videos.list` returns
//! YouTube's own `liveStreamingDetails` for up to 50 known video IDs per unit,
//! so the rows Holodex already sent are re-classified from that instead.
//!
//! `youtube_api_key` may hold several keys (from different Google projects);
//! calls draw on a pool with a daily budget per key.
//!
//! Everything here degrades to the Holodex rows unchanged: no key, every
//! budget spent, or any API error.

use super::holodex::HolodexStream;
use super::http::{pooled_client, response_json_limited};
use crate::config::load_config;
use chrono::{Datelike, TimeZone};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::error::Error;
use std::sync::Mutex;
use std::time::{Duration, Instant};

const VIDEOS_URL: &str = "https://www.googleapis.com/youtube/v3/videos";
pub(crate) const MAX_IDS_PER_CALL: usize = 50;
/// Per key. Google's default is 10,000 units/day; stop short so retries and
/// calls made before the local count noticed a new day still fit.
pub(crate) const DAILY_UNIT_BUDGET: u32 = 9_000;

#[derive(Deserialize, Debug, Clone, Default)]
#[serde(rename_all = "camelCase")]
pub struct YtLiveDetails {
    pub actual_start_time: Option<String>,
    pub actual_end_time: Option<String>,
    pub scheduled_start_time: Option<String>,
    pub concurrent_viewers: Option<String>,
}

#[derive(Deserialize, Debug, Clone, Default)]
pub struct YtSnippet {
    #[serde(default)]
    pub title: String,
    #[serde(rename = "channelId", default)]
    pub channel_id: String,
}

#[derive(Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct YtVideo {
    pub id: String,
    #[serde(default)]
    pub snippet: YtSnippet,
    pub live_streaming_details: Option<YtLiveDetails>,
}

#[derive(Deserialize)]
struct VideosResponse {
    #[serde(default)]
    items: Vec<YtVideo>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum YtLiveState {
    Live {
        start_actual: String,
        viewers: Option<i32>,
    },
    Upcoming {
        scheduled: Option<String>,
    },
    Ended,
    /// Ordinary upload with no live details.
    Vod,
}

fn non_empty(value: &Option<String>) -> Option<&String> {
    value.as_ref().filter(|value| !value.is_empty())
}

pub fn classify(video: &YtVideo) -> YtLiveState {
    let Some(details) = &video.live_streaming_details else {
        return YtLiveState::Vod;
    };
    if non_empty(&details.actual_end_time).is_some() {
        return YtLiveState::Ended;
    }
    if let Some(start) = non_empty(&details.actual_start_time) {
        return YtLiveState::Live {
            start_actual: start.clone(),
            viewers: details
                .concurrent_viewers
                .as_deref()
                .and_then(|viewers| viewers.parse().ok()),
        };
    }
    YtLiveState::Upcoming {
        scheduled: non_empty(&details.scheduled_start_time).cloned(),
    }
}

/// Re-classify Holodex rows from YouTube's answer.
///
/// Ended and plain uploads are dropped. IDs YouTube did not return (private,
/// members-only, deleted) and placeholder rows keep their Holodex values,
/// except rows YouTube itself confirmed earlier (discovered or recently live):
/// those only ever came from YouTube, so an omitted one is dropped.
pub fn overlay(
    streams: Vec<HolodexStream>,
    videos: &HashMap<String, YtVideo>,
) -> Vec<HolodexStream> {
    streams
        .into_iter()
        .filter_map(|mut stream| {
            if stream.stream_type == "placeholder" {
                return Some(stream);
            }
            let Some(video) = videos.get(&stream.id) else {
                return (!stream.yt_confirmed).then_some(stream);
            };
            match classify(video) {
                YtLiveState::Ended | YtLiveState::Vod => return None,
                YtLiveState::Live {
                    start_actual,
                    viewers,
                } => {
                    stream.status = "live".to_string();
                    stream.start_actual = Some(start_actual);
                    if viewers.is_some() {
                        stream.live_viewers = viewers;
                    }
                }
                YtLiveState::Upcoming { scheduled } => {
                    stream.status = "upcoming".to_string();
                    stream.start_actual = None;
                    if scheduled.is_some() {
                        stream.start_scheduled = scheduled;
                    }
                }
            }
            if !video.snippet.title.is_empty() {
                stream.title = video.snippet.title.clone();
            }
            stream.yt_confirmed = true;
            Some(stream)
        })
        .collect()
}

/// Hours to subtract from UTC to get US Pacific local time. PDT is 7, PST is 8.
/// DST: 10:00 UTC on the second Sunday of March through 09:00 UTC on the first
/// Sunday of November (02:00 local, which is Google's "Pacific Time").
fn pacific_offset_hours(now: chrono::DateTime<chrono::Utc>) -> i64 {
    let year = now.year();
    if now >= us_pacific_dst_switch(year, 3, 2, 10) && now < us_pacific_dst_switch(year, 11, 1, 9) {
        7
    } else {
        8
    }
}

fn us_pacific_dst_switch(
    year: i32,
    month: u32,
    nth_sunday: u32,
    hour_utc: u32,
) -> chrono::DateTime<chrono::Utc> {
    let first = chrono::NaiveDate::from_ymd_opt(year, month, 1).expect("month");
    let to_sunday = (7 - first.weekday().num_days_from_sunday()) % 7;
    let date = first + chrono::Days::new(u64::from(to_sunday + 7 * (nth_sunday - 1)));
    chrono::Utc.from_utc_datetime(&date.and_hms_opt(hour_utc, 0, 0).expect("hour"))
}

fn pacific_local_date(now: chrono::DateTime<chrono::Utc>) -> chrono::NaiveDate {
    (now - chrono::Duration::hours(pacific_offset_hours(now))).date_naive()
}

/// UTC instant of midnight Pacific on `date`.
fn pacific_midnight(date: chrono::NaiveDate) -> chrono::DateTime<chrono::Utc> {
    let midnight = date.and_hms_opt(0, 0, 0).expect("midnight");
    for hours in [7_i64, 8] {
        let utc = chrono::Utc.from_utc_datetime(&(midnight + chrono::Duration::hours(hours)));
        if pacific_offset_hours(utc) == hours && pacific_local_date(utc) == date {
            return utc;
        }
    }
    chrono::Utc.from_utc_datetime(&(midnight + chrono::Duration::hours(8)))
}

/// Day index that rolls over at midnight Pacific Time, including DST.
fn pacific_day(now: chrono::DateTime<chrono::Utc>) -> i64 {
    (now - chrono::Duration::hours(pacific_offset_hours(now)))
        .timestamp()
        .div_euclid(86_400)
}

/// Share of the current Pacific day (as `pacific_day` counts it) still ahead.
pub(crate) fn pacific_day_left(now: chrono::DateTime<chrono::Utc>) -> f64 {
    let elapsed = (now - chrono::Duration::hours(pacific_offset_hours(now)))
        .timestamp()
        .rem_euclid(86_400);
    1.0 - elapsed as f64 / 86_400.0
}

/// When `pacific_day` next rolls over, i.e. when the local counts reset.
fn pacific_day_end(now: chrono::DateTime<chrono::Utc>) -> chrono::DateTime<chrono::Utc> {
    pacific_midnight(pacific_local_date(now) + chrono::Days::new(1))
}

/// Which key pays for a call. Each key has its own daily budget; the one with
/// the most budget left is used, so load spreads and a new key takes over.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Bench {
    /// Google said the key's quota is spent; usable again on this Pacific day.
    UntilDay(i64),
    /// Google rejected the key; usable again once the key list changes.
    UntilConfigChange,
}

#[derive(Debug, Default)]
struct KeyState {
    used: u32,
    benched: Option<Bench>,
}

#[derive(Default)]
struct KeyPool {
    day: i64,
    keys: HashMap<String, KeyState>,
}

impl KeyPool {
    /// Follow the configured list and the Pacific day. Spend on keys that stay
    /// in the list is kept; any change to the list lifts rejected-key benches.
    fn sync(&mut self, keys: &[String], day: i64) {
        let changed =
            keys.len() != self.keys.len() || keys.iter().any(|key| !self.keys.contains_key(key));
        if changed {
            self.keys.retain(|key, _| keys.contains(key));
            for key in keys {
                self.keys.entry(key.clone()).or_default();
            }
            for state in self.keys.values_mut() {
                if state.benched == Some(Bench::UntilConfigChange) {
                    state.benched = None;
                }
            }
        }
        if self.day != day {
            self.day = day;
            for state in self.keys.values_mut() {
                state.used = 0;
                if matches!(state.benched, Some(Bench::UntilDay(until)) if until <= day) {
                    state.benched = None;
                }
            }
        }
    }

    fn usable(&self, key: &str, units: u32) -> bool {
        self.keys
            .get(key)
            .is_some_and(|state| state.benched.is_none() && state.used + units <= DAILY_UNIT_BUDGET)
    }

    /// Spend `units` on the usable key with the most budget left, skipping
    /// `tried`.
    fn pick(&mut self, keys: &[String], units: u32, day: i64, tried: &[String]) -> Option<String> {
        self.sync(keys, day);
        let key = keys
            .iter()
            .filter(|key| !tried.contains(key) && self.usable(key, units))
            .min_by_key(|key| self.keys[*key].used)?
            .clone();
        if let Some(state) = self.keys.get_mut(&key) {
            state.used += units;
        }
        Some(key)
    }

    fn bench(&mut self, key: &str, bench: Bench) {
        if let Some(state) = self.keys.get_mut(key) {
            state.benched = Some(bench);
        }
    }

    fn usable_count(&mut self, keys: &[String], day: i64) -> usize {
        self.sync(keys, day);
        keys.iter().filter(|key| self.usable(key, 1)).count()
    }

    /// Unspent units on keys Google has not benched, as a share of every
    /// key's full budget.
    fn remaining_fraction(&mut self, keys: &[String], day: i64) -> f64 {
        self.sync(keys, day);
        if keys.is_empty() {
            return 0.0;
        }
        let left: u32 = keys
            .iter()
            .filter_map(|key| self.keys.get(key))
            .filter(|state| state.benched.is_none())
            .map(|state| DAILY_UNIT_BUDGET.saturating_sub(state.used))
            .sum();
        f64::from(left) / (keys.len() as f64 * f64::from(DAILY_UNIT_BUDGET))
    }

    fn status(&mut self, keys: &[String], day: i64) -> Vec<KeyStatus> {
        self.sync(keys, day);
        keys.iter()
            .filter_map(|key| {
                let state = self.keys.get(key)?;
                Some(KeyStatus {
                    fingerprint: fingerprint(key),
                    used: state.used,
                    state: match state.benched {
                        Some(Bench::UntilConfigChange) => KeyUse::Rejected,
                        Some(Bench::UntilDay(_)) => KeyUse::Exhausted,
                        None if !self.usable(key, 1) => KeyUse::Exhausted,
                        None => KeyUse::Usable,
                    },
                })
            })
            .collect()
    }
}

/// One key as the settings view shows it. Never carries the key itself.
#[derive(Serialize)]
struct KeyStatus {
    fingerprint: String,
    used: u32,
    state: KeyUse,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
enum KeyUse {
    Usable,
    /// Today's budget is spent; usable again after the Pacific reset.
    Exhausted,
    /// Google rejected the key; usable again once the key list changes.
    Rejected,
}

static POOL: Mutex<Option<KeyPool>> = Mutex::new(None);

fn with_pool<T>(f: impl FnOnce(&mut KeyPool) -> T) -> T {
    let mut guard = POOL.lock().unwrap_or_else(|e| e.into_inner());
    f(guard.get_or_insert_with(KeyPool::default))
}

/// Keys whose daily budget is not spent and that Google has not rejected.
pub(crate) fn usable_key_count(keys: &[String]) -> usize {
    with_pool(|pool| pool.usable_count(keys, pacific_day(chrono::Utc::now())))
}

/// How much of the pool's daily budget is left, from 0 to 1.
pub(crate) fn budget_remaining_fraction(keys: &[String]) -> f64 {
    with_pool(|pool| pool.remaining_fraction(keys, pacific_day(chrono::Utc::now())))
}

pub(crate) const MAX_STRETCH: u32 = 8;
/// Remaining-budget share the pool may trail the day by before intervals
/// stretch. The first videos.list after Pacific midnight spends a few units
/// while `day_left` is still ~1, so without slack `ceil` jumps to 2× and then
/// flips back as the clock catches up. 1% is about 15 minutes of even pace.
const STRETCH_SLACK: f64 = 0.01;

/// Interval multiplier: 1 while the pool has at least as large a share of its
/// daily budget left as of the day (plus a small slack), otherwise enough to
/// fall back in step.
pub(crate) fn stretch(remaining: f64, day_left: f64) -> u32 {
    if remaining <= 0.0 {
        MAX_STRETCH
    } else if remaining + STRETCH_SLACK >= day_left {
        1
    } else {
        ((day_left / remaining).ceil() as u32).min(MAX_STRETCH)
    }
}

/// The pool for the settings view: per-key spend and state, the share left
/// and when the local counts reset.
pub(crate) fn key_pool_status(keys: &[String]) -> serde_json::Value {
    let now = chrono::Utc::now();
    let (keys, remaining) = with_pool(|pool| {
        let day = pacific_day(now);
        (pool.status(keys, day), pool.remaining_fraction(keys, day))
    });
    serde_json::json!({
        "keys": keys,
        "budget_per_key": DAILY_UNIT_BUDGET,
        "remaining_fraction": remaining,
        "resets_at": pacific_day_end(now).to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
    })
}

/// Short stable tag for logs; the key itself must never be logged.
fn fingerprint(key: &str) -> String {
    // FNV-1a
    let hash = key.bytes().fold(0x811c_9dc5_u32, |hash, byte| {
        (hash ^ u32::from(byte)).wrapping_mul(0x0100_0193)
    });
    format!("{:06x}", hash & 0x00ff_ffff)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KeyFault {
    /// Daily quota spent: bench until tomorrow, try another key.
    Exhausted,
    /// Short-term rate limit: try another key, no bench.
    RateLimited,
    /// Bad, blocked, or API-disabled key: bench until the config changes.
    Rejected,
}

/// Google puts the reason in `error.errors[].reason` (legacy) and
/// `error.details[].reason` (ErrorInfo); an invalid key only shows up in the
/// latter (`badRequest` / `API_KEY_INVALID`).
fn key_fault(body: &serde_json::Value) -> Option<KeyFault> {
    let error = body.get("error")?;
    let reasons = ["errors", "details"]
        .iter()
        .filter_map(|field| error.get(*field)?.as_array())
        .flatten()
        .filter_map(|item| item.get("reason")?.as_str());
    let mut fault = None;
    for reason in reasons {
        let this = match reason {
            "quotaExceeded" | "dailyLimitExceeded" => KeyFault::Exhausted,
            "rateLimitExceeded" | "userRateLimitExceeded" => KeyFault::RateLimited,
            "keyInvalid"
            | "keyExpired"
            | "API_KEY_INVALID"
            | "API_KEY_SERVICE_BLOCKED"
            | "API_KEY_HTTP_REFERRER_BLOCKED"
            | "API_KEY_IP_ADDRESS_BLOCKED"
            | "accessNotConfigured"
            | "SERVICE_DISABLED" => KeyFault::Rejected,
            _ => continue,
        };
        // A rejected key outranks a quota reason reported alongside it.
        if fault != Some(KeyFault::Rejected) {
            fault = Some(this);
        }
    }
    fault
}

/// GET a Data API endpoint with a key from the pool. A key-specific failure
/// (quota, rate limit, rejected key) benches that key as needed and retries
/// once on another. `Ok(None)` is a 404, e.g. `playlistNotFound`.
pub(crate) async fn google_get<T: serde::de::DeserializeOwned>(
    keys: &[String],
    proxy: Option<&str>,
    url: &str,
    query: &[(&str, &str)],
    units: u32,
) -> Result<Option<T>, Box<dyn Error>> {
    let client = pooled_client(proxy)?;
    let day = pacific_day(chrono::Utc::now());
    let mut tried: Vec<String> = Vec::new();
    while tried.len() < 2 {
        let Some(key) = with_pool(|pool| pool.pick(keys, units, day, &tried)) else {
            break;
        };
        tried.push(key.clone());
        let mut pairs = query.to_vec();
        pairs.push(("key", &key));
        let response = client
            .get(format!("{url}?{}", serde_urlencoded::to_string(&pairs)?))
            .timeout(Duration::from_secs(10))
            .send()
            .await
            // The URL carries the key; keep it out of error messages and logs.
            .map_err(reqwest::Error::without_url)?;
        let status = response.status();
        if status.is_success() {
            return Ok(Some(response_json_limited(response).await?));
        }
        if status == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let body: serde_json::Value = response_json_limited(response).await.unwrap_or_default();
        let tag = fingerprint(&key);
        match key_fault(&body) {
            Some(KeyFault::Exhausted) => {
                tracing::warn!("YouTube API key {} 今日配额已用尽，换用其他 key", tag);
                with_pool(|pool| pool.bench(&key, Bench::UntilDay(day + 1)));
            }
            Some(KeyFault::Rejected) => {
                tracing::warn!(
                    "YouTube API key {} 被拒绝 ({})，修改配置前不再使用",
                    tag,
                    status
                );
                with_pool(|pool| pool.bench(&key, Bench::UntilConfigChange));
            }
            Some(KeyFault::RateLimited) => {
                tracing::debug!("YouTube API key {} 触发短时限流，换用其他 key", tag);
            }
            None => return Err(format!("YouTube Data API error: {status}").into()),
        }
    }
    Err(if tried.is_empty() {
        "YouTube Data API daily budget used up".into()
    } else {
        "YouTube Data API: no usable key left".into()
    })
}

pub(crate) async fn fetch_videos(
    keys: &[String],
    proxy: Option<&str>,
    ids: &[String],
) -> Result<Vec<YtVideo>, Box<dyn Error>> {
    let mut videos = Vec::new();
    for chunk in ids.chunks(MAX_IDS_PER_CALL) {
        let ids = chunk.join(",");
        let query = [
            ("part", "snippet,liveStreamingDetails"),
            ("id", ids.as_str()),
        ];
        let body: Option<VideosResponse> = google_get(keys, proxy, VIDEOS_URL, &query, 1).await?;
        videos.extend(body.map(|body| body.items).unwrap_or_default());
    }
    Ok(videos)
}

pub(crate) async fn videos_for(
    keys: &[String],
    proxy: Option<&str>,
    ids: Vec<String>,
) -> Result<HashMap<String, YtVideo>, Box<dyn Error>> {
    videos_within(keys, proxy, ids, None).await
}

/// `max_age`: re-fetch answers older than this instead of waiting for their tier.
pub(crate) async fn videos_within(
    keys: &[String],
    proxy: Option<&str>,
    ids: Vec<String>,
    max_age: Option<Duration>,
) -> Result<HashMap<String, YtVideo>, Box<dyn Error>> {
    super::youtube_index::store_videos(keys, proxy, ids, max_age).await
}

/// Whether YouTube can answer right now: a key with budget left.
pub(crate) fn youtube_answers_available(cfg: &crate::config::Config) -> bool {
    usable_key_count(&cfg.youtube_api_keys()) > 0
}

/// A dropped encoder can reconnect under the same video ID while YouTube holds
/// the broadcast open (2–5 min), and Holodex often drops the row meanwhile.
/// A stream seen live is re-added for this long after Holodex and discovery
/// last listed it, and kept only while YouTube still answers live.
const RECENT_LIVE_HOLD: Duration = Duration::from_secs(10 * 60);

/// Video ID → (last listed by Holodex or discovery, last live row).
type RecentLive = HashMap<String, (Instant, HolodexStream)>;
static RECENT_LIVE: Mutex<Option<RecentLive>> = Mutex::new(None);

fn with_recent_live<T>(f: impl FnOnce(&mut RecentLive) -> T) -> T {
    let mut guard = RECENT_LIVE.lock().unwrap_or_else(|e| e.into_inner());
    f(guard.get_or_insert_with(HashMap::new))
}

/// Recently live rows missing from `present`, to be re-checked by the overlay.
/// Rows still in `present` count as listed now.
pub(crate) fn recent_live_rows(present: &HashSet<String>) -> Vec<HolodexStream> {
    with_recent_live(|recent| recent_live_rows_in(recent, present, Instant::now()))
}

fn recent_live_rows_in(
    recent: &mut RecentLive,
    present: &HashSet<String>,
    now: Instant,
) -> Vec<HolodexStream> {
    for (id, (listed, _)) in recent.iter_mut() {
        if present.contains(id) {
            *listed = now;
        }
    }
    recent.retain(|_, (listed, _)| now.duration_since(*listed) < RECENT_LIVE_HOLD);
    recent
        .iter()
        .filter(|(id, _)| !present.contains(*id))
        .map(|(_, (_, row))| row.clone())
        .collect()
}

#[cfg(test)]
pub(crate) fn remember_live_for_test(row: HolodexStream) {
    with_recent_live(|recent| {
        recent.insert(row.id.clone(), (Instant::now(), row));
    });
}

/// Remember rows YouTube answered live; forget asked IDs it did not.
fn record_recent_live(
    recent: &mut RecentLive,
    asked: &[String],
    result: &[HolodexStream],
    now: Instant,
) {
    let live: HashMap<&str, &HolodexStream> = result
        .iter()
        .filter(|stream| stream.yt_confirmed && stream.status == "live")
        .map(|stream| (stream.id.as_str(), stream))
        .collect();
    for id in asked {
        match live.get(id.as_str()) {
            Some(row) => {
                let listed = recent.get(id).map_or(now, |(listed, _)| *listed);
                recent.insert(id.clone(), (listed, (*row).clone()));
            }
            None => {
                recent.remove(id);
            }
        }
    }
}

/// Holodex rows corrected by YouTube, or unchanged when the overlay is off or fails.
pub async fn apply_youtube_overlay(streams: Vec<HolodexStream>) -> Vec<HolodexStream> {
    apply_youtube_overlay_within(streams, None).await
}

/// `apply_youtube_overlay` that re-fetches answers older than `max_age`.
pub async fn apply_youtube_overlay_within(
    streams: Vec<HolodexStream>,
    max_age: Option<Duration>,
) -> Vec<HolodexStream> {
    let Ok(cfg) = load_config().await else {
        return streams;
    };
    let keys = cfg.youtube_api_keys();
    if keys.is_empty() {
        // Rows YouTube confirmed earlier cannot be re-checked without a key.
        with_recent_live(|recent| recent.clear());
        return streams
            .into_iter()
            .filter(|stream| !stream.yt_confirmed)
            .collect();
    }

    let mut seen = HashSet::new();
    let ids: Vec<String> = streams
        .iter()
        .filter(|stream| stream.stream_type != "placeholder" && !stream.id.is_empty())
        .filter(|stream| seen.insert(stream.id.clone()))
        .map(|stream| stream.id.clone())
        .collect();
    if ids.is_empty() {
        return streams;
    }

    match videos_within(&keys, cfg.youtube.proxy.as_deref(), ids.clone(), max_age).await {
        Ok(videos) => {
            let result = overlay(streams, &videos);
            with_recent_live(|recent| record_recent_live(recent, &ids, &result, Instant::now()));
            result
        }
        Err(e) => {
            tracing::warn!("YouTube Data API 校正失败，沿用 Holodex 状态: {}", e);
            streams
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugins::holodex::HolodexChannel;

    fn row(id: &str, status: &str, stream_type: &str) -> HolodexStream {
        HolodexStream {
            id: id.to_string(),
            title: "holodex title".to_string(),
            stream_type: stream_type.to_string(),
            topic_id: Some("minecraft".to_string()),
            published_at: None,
            available_at: None,
            status: status.to_string(),
            start_scheduled: Some("2026-09-24T10:00:00Z".to_string()),
            start_actual: None,
            live_viewers: None,
            channel: HolodexChannel::default(),
            link: None,
            thumbnail: None,
            placeholder_type: None,
            yt_confirmed: false,
        }
    }

    fn video(id: &str, details: Option<YtLiveDetails>) -> YtVideo {
        YtVideo {
            id: id.to_string(),
            snippet: YtSnippet {
                title: "youtube title".to_string(),
                ..YtSnippet::default()
            },
            live_streaming_details: details,
        }
    }

    fn details(start: Option<&str>, end: Option<&str>, scheduled: Option<&str>) -> YtLiveDetails {
        YtLiveDetails {
            actual_start_time: start.map(str::to_string),
            actual_end_time: end.map(str::to_string),
            scheduled_start_time: scheduled.map(str::to_string),
            concurrent_viewers: Some("1234".to_string()),
        }
    }

    fn by_id(videos: Vec<YtVideo>) -> HashMap<String, YtVideo> {
        videos.into_iter().map(|v| (v.id.clone(), v)).collect()
    }

    #[test]
    fn classify_follows_live_streaming_details() {
        let ended = video("a", Some(details(Some("s"), Some("e"), Some("x"))));
        assert_eq!(classify(&ended), YtLiveState::Ended);
        let live = video("a", Some(details(Some("s"), None, Some("x"))));
        assert_eq!(
            classify(&live),
            YtLiveState::Live {
                start_actual: "s".to_string(),
                viewers: Some(1234)
            }
        );
        let upcoming = video("a", Some(details(None, None, Some("x"))));
        assert_eq!(
            classify(&upcoming),
            YtLiveState::Upcoming {
                scheduled: Some("x".to_string())
            }
        );
        assert_eq!(classify(&video("a", None)), YtLiveState::Vod);
    }

    #[test]
    fn an_upcoming_row_youtube_sees_live_flips_to_live() {
        let videos = by_id(vec![video(
            "v",
            Some(details(Some("2026-09-24T10:02:00Z"), None, None)),
        )]);
        let out = overlay(vec![row("v", "upcoming", "stream")], &videos);
        assert_eq!(out[0].status, "live");
        assert_eq!(out[0].start_actual.as_deref(), Some("2026-09-24T10:02:00Z"));
        assert_eq!(out[0].live_viewers, Some(1234));
        assert_eq!(out[0].title, "youtube title");
        assert_eq!(out[0].topic_id.as_deref(), Some("minecraft"));
        assert!(out[0].yt_confirmed);
    }

    #[test]
    fn a_hung_live_row_without_a_youtube_start_becomes_upcoming() {
        let videos = by_id(vec![video("v", Some(details(None, None, Some("later"))))]);
        let mut hung = row("v", "live", "stream");
        hung.start_actual = Some(String::new());
        let out = overlay(vec![hung], &videos);
        assert_eq!(out[0].status, "upcoming");
        assert_eq!(out[0].start_actual, None);
        assert_eq!(out[0].start_scheduled.as_deref(), Some("later"));
    }

    #[test]
    fn ended_and_plain_uploads_are_dropped() {
        let videos = by_id(vec![
            video("ended", Some(details(Some("s"), Some("e"), None))),
            video("vod", None),
        ]);
        let out = overlay(
            vec![
                row("ended", "live", "stream"),
                row("vod", "upcoming", "stream"),
            ],
            &videos,
        );
        assert!(out.is_empty());
    }

    #[test]
    fn ids_youtube_omits_and_placeholders_are_left_alone() {
        let videos = by_id(vec![video("ph", None)]);
        let out = overlay(
            vec![
                row("members", "live", "stream"),
                row("ph", "live", "placeholder"),
            ],
            &videos,
        );
        assert_eq!(out.len(), 2);
        assert!(out.iter().all(|s| s.status == "live" && !s.yt_confirmed));
        assert!(out.iter().all(|s| s.title == "holodex title"));
    }

    #[test]
    fn a_row_youtube_confirmed_earlier_is_dropped_once_omitted() {
        let mut confirmed = row("gone", "live", "stream");
        confirmed.yt_confirmed = true;
        assert!(overlay(vec![confirmed], &HashMap::new()).is_empty());
    }

    fn live_row(id: &str) -> HolodexStream {
        let mut row = row(id, "live", "stream");
        row.yt_confirmed = true;
        row
    }

    fn ids(list: &[&str]) -> Vec<String> {
        list.iter().map(|id| id.to_string()).collect()
    }

    fn set(list: &[&str]) -> HashSet<String> {
        ids(list).into_iter().collect()
    }

    #[test]
    fn a_dropped_live_stream_is_re_added_until_youtube_ends_it() {
        let t0 = Instant::now();
        let mut recent = RecentLive::new();
        record_recent_live(&mut recent, &ids(&["v"]), &[live_row("v")], t0);

        // Holodex still lists it: nothing to re-add.
        assert!(recent_live_rows_in(&mut recent, &set(&["v"]), t0).is_empty());
        // Holodex dropped it during the encoder outage: re-added.
        let t1 = t0 + Duration::from_secs(60);
        let re_added = recent_live_rows_in(&mut recent, &set(&[]), t1);
        assert_eq!(re_added.len(), 1);
        assert_eq!(re_added[0].id, "v");

        // YouTube still answers live during its hold: the row survives the
        // overlay and stays remembered.
        let videos = by_id(vec![video("v", Some(details(Some("s"), None, None)))]);
        let result = overlay(re_added, &videos);
        assert_eq!(result.len(), 1);
        record_recent_live(&mut recent, &ids(&["v"]), &result, t1);
        assert!(recent.contains_key("v"));

        // YouTube archived it: dropped and forgotten.
        let videos = by_id(vec![video("v", Some(details(Some("s"), Some("e"), None)))]);
        let result = overlay(recent_live_rows_in(&mut recent, &set(&[]), t1), &videos);
        assert!(result.is_empty());
        record_recent_live(&mut recent, &ids(&["v"]), &result, t1);
        assert!(recent.is_empty());
    }

    #[test]
    fn a_re_added_row_youtube_omits_is_dropped_and_forgotten() {
        let t0 = Instant::now();
        let mut recent = RecentLive::new();
        record_recent_live(&mut recent, &ids(&["v"]), &[live_row("v")], t0);
        let result = overlay(
            recent_live_rows_in(&mut recent, &set(&[]), t0),
            &HashMap::new(),
        );
        assert!(result.is_empty());
        record_recent_live(&mut recent, &ids(&["v"]), &result, t0);
        assert!(recent.is_empty());
    }

    #[test]
    fn the_hold_counts_from_when_holodex_last_listed_the_stream() {
        let t0 = Instant::now();
        let mut recent = RecentLive::new();
        record_recent_live(&mut recent, &ids(&["v"]), &[live_row("v")], t0);
        // Still listed at +5 min, so the hold restarts there.
        let t5 = t0 + Duration::from_secs(5 * 60);
        recent_live_rows_in(&mut recent, &set(&["v"]), t5);
        // YouTube answering live for a re-added row does not extend it.
        record_recent_live(
            &mut recent,
            &ids(&["v"]),
            &[live_row("v")],
            t5 + RECENT_LIVE_HOLD / 2,
        );
        let before = t5 + RECENT_LIVE_HOLD - Duration::from_secs(1);
        assert_eq!(recent_live_rows_in(&mut recent, &set(&[]), before).len(), 1);
        assert!(recent_live_rows_in(&mut recent, &set(&[]), t5 + RECENT_LIVE_HOLD).is_empty());
        assert!(recent.is_empty());
    }

    #[test]
    fn a_stream_that_goes_back_to_upcoming_is_forgotten() {
        let t0 = Instant::now();
        let mut recent = RecentLive::new();
        record_recent_live(&mut recent, &ids(&["v"]), &[live_row("v")], t0);
        let mut upcoming = live_row("v");
        upcoming.status = "upcoming".to_string();
        record_recent_live(&mut recent, &ids(&["v"]), &[upcoming], t0);
        assert!(recent.is_empty());
    }

    fn keys(list: &[&str]) -> Vec<String> {
        list.iter().map(|key| key.to_string()).collect()
    }

    #[test]
    fn one_key_stops_at_the_budget_and_resets_each_pacific_day() {
        let one = keys(&["a"]);
        let mut pool = KeyPool::default();
        assert_eq!(
            pool.pick(&one, DAILY_UNIT_BUDGET, 1, &[]).as_deref(),
            Some("a")
        );
        assert_eq!(pool.pick(&one, 1, 1, &[]), None);
        assert_eq!(pool.pick(&one, 1, 2, &[]).as_deref(), Some("a"));
    }

    #[test]
    fn the_pool_spreads_spend_and_skips_tried_and_benched_keys() {
        let two = keys(&["a", "b"]);
        let mut pool = KeyPool::default();
        let picks: Vec<String> = (0..4).filter_map(|_| pool.pick(&two, 1, 1, &[])).collect();
        assert_eq!(picks, keys(&["a", "b", "a", "b"]));
        assert_eq!(pool.pick(&two, 1, 1, &keys(&["a"])).as_deref(), Some("b"));

        pool.bench("a", Bench::UntilDay(2));
        assert_eq!(pool.usable_count(&two, 1), 1);
        assert_eq!(pool.pick(&two, 1, 1, &keys(&["b"])), None);
        assert_eq!(pool.usable_count(&two, 2), 2);
    }

    #[test]
    fn a_rejected_key_waits_for_a_key_list_change() {
        let two = keys(&["a", "b"]);
        let mut pool = KeyPool::default();
        pool.sync(&two, 1);
        pool.bench("a", Bench::UntilConfigChange);
        assert_eq!(pool.usable_count(&two, 5), 1);
        pool.pick(&two, 7, 5, &[]);

        let three = keys(&["a", "b", "c"]);
        assert_eq!(pool.usable_count(&three, 5), 3);
        assert_eq!(
            pool.keys["b"].used, 7,
            "spend on kept keys survives the change"
        );
    }

    #[test]
    fn the_remaining_budget_counts_only_unbenched_keys() {
        let two = keys(&["a", "b"]);
        let mut pool = KeyPool::default();
        assert_eq!(pool.remaining_fraction(&two, 1), 1.0);
        pool.pick(&keys(&["a"]), DAILY_UNIT_BUDGET / 2, 1, &[]);
        assert_eq!(pool.remaining_fraction(&two, 1), 0.75);
        pool.bench("b", Bench::UntilDay(2));
        assert_eq!(pool.remaining_fraction(&two, 1), 0.25);
        assert_eq!(
            pool.remaining_fraction(&two, 2),
            1.0,
            "a new day resets both"
        );
        assert_eq!(pool.remaining_fraction(&[], 2), 0.0);
    }

    #[test]
    fn intervals_stretch_when_the_budget_runs_ahead_of_the_day() {
        assert_eq!(stretch(0.5, 0.5), 1);
        assert_eq!(stretch(0.9, 0.5), 1);
        assert_eq!(stretch(0.4, 0.5), 2);
        assert_eq!(stretch(0.1, 0.5), 5);
        assert_eq!(stretch(0.01, 0.5), MAX_STRETCH);
        assert_eq!(stretch(0.0, 0.5), MAX_STRETCH);
        // 47 units across 3 keys, 6s after Pacific midnight: the first
        // videos.list pass after local counts zero. Slack keeps this at 1×.
        let remaining = 1.0 - 47.0 / (3.0 * f64::from(DAILY_UNIT_BUDGET));
        let day_left = 1.0 - 6.0 / 86_400.0;
        assert_eq!(stretch(remaining, day_left), 1);
        // 16:00 in China during PDT: Google's day is already an hour in.
        assert_eq!(stretch(remaining, 1.0 - 3_600.0 / 86_400.0), 1);
        assert_eq!(
            stretch(0.8, 1.0 - 60.0 / 86_400.0),
            2,
            "20% of the pool in the first minute is actually ahead"
        );
    }

    #[test]
    fn key_status_names_keys_by_fingerprint_and_state_only() {
        let secret = keys(&[
            "AIzaSyUsable",
            "AIzaSySpent",
            "AIzaSyRejected",
            "AIzaSyFull",
        ]);
        let mut pool = KeyPool::default();
        pool.sync(&secret, 1);
        pool.keys.get_mut("AIzaSyUsable").unwrap().used = 1_234;
        pool.bench("AIzaSySpent", Bench::UntilDay(2));
        pool.bench("AIzaSyRejected", Bench::UntilConfigChange);
        pool.keys.get_mut("AIzaSyFull").unwrap().used = DAILY_UNIT_BUDGET;
        let json = serde_json::to_value(pool.status(&secret, 1)).unwrap();
        assert_eq!(
            json,
            serde_json::json!([
                {"fingerprint": fingerprint("AIzaSyUsable"), "used": 1_234, "state": "usable"},
                {"fingerprint": fingerprint("AIzaSySpent"), "used": 0, "state": "exhausted"},
                {"fingerprint": fingerprint("AIzaSyRejected"), "used": 0, "state": "rejected"},
                {"fingerprint": fingerprint("AIzaSyFull"), "used": DAILY_UNIT_BUDGET, "state": "exhausted"},
            ])
        );
        assert!(!json.to_string().contains("AIzaSy"));
    }

    #[test]
    fn google_error_reasons_map_to_key_faults() {
        // Google's documented 403 body for a spent daily quota, verbatim.
        let quota = serde_json::json!({"error": {
            "code": 403,
            "message": "The request cannot be completed because you have exceeded your quota.",
            "errors": [{
                "domain": "youtube.quota",
                "reason": "quotaExceeded",
                "message": "The request cannot be completed because you have exceeded your quota."
            }]
        }});
        assert_eq!(key_fault(&quota), Some(KeyFault::Exhausted));
        let legacy = |reason: &str| serde_json::json!({"error": {"code": 403, "errors": [{"reason": reason}]}});
        assert_eq!(
            key_fault(&legacy("dailyLimitExceeded")),
            Some(KeyFault::Exhausted)
        );
        assert_eq!(
            key_fault(&legacy("rateLimitExceeded")),
            Some(KeyFault::RateLimited)
        );
        assert_eq!(key_fault(&legacy("videoNotFound")), None);
        let invalid = serde_json::json!({"error": {
            "code": 400,
            "message": "API key not valid. Please pass a valid API key.",
            "errors": [{"reason": "badRequest"}],
            "details": [{"@type": "type.googleapis.com/google.rpc.ErrorInfo", "reason": "API_KEY_INVALID"}]
        }});
        assert_eq!(key_fault(&invalid), Some(KeyFault::Rejected));
        assert_eq!(key_fault(&serde_json::json!({})), None);
    }

    #[test]
    fn key_fingerprints_are_short_and_stable() {
        assert_eq!(fingerprint("AIzaSyExample").len(), 6);
        assert_eq!(fingerprint("AIzaSyExample"), fingerprint("AIzaSyExample"));
        assert_ne!(fingerprint("AIzaSyExample"), fingerprint("AIzaSyExamplf"));
    }

    #[test]
    fn the_pacific_day_follows_us_dst() {
        let at = |s: &str| {
            chrono::DateTime::parse_from_rfc3339(s)
                .unwrap()
                .with_timezone(&chrono::Utc)
        };
        // Late September is PDT: Google resets at 07:00 UTC (15:00 in China).
        let before = pacific_day(at("2026-09-24T06:59:59Z"));
        assert_eq!(pacific_day(at("2026-09-24T07:00:00Z")), before + 1);
        assert_eq!(pacific_day(at("2026-09-24T08:00:00Z")), before + 1);
        assert_eq!(pacific_day_left(at("2026-09-24T07:00:00Z")), 1.0);
        assert_eq!(
            pacific_day_left(at("2026-09-24T08:00:00Z")),
            1.0 - 3_600.0 / 86_400.0
        );
        assert_eq!(pacific_day_left(at("2026-09-24T19:00:00Z")), 0.5);
        assert_eq!(
            pacific_day_end(at("2026-09-24T06:59:59Z")),
            at("2026-09-24T07:00:00Z")
        );
        assert_eq!(
            pacific_day_end(at("2026-09-24T08:00:00Z")),
            at("2026-09-25T07:00:00Z")
        );

        let jan = pacific_day(at("2026-01-15T07:59:59Z"));
        assert_eq!(pacific_day(at("2026-01-15T08:00:00Z")), jan + 1);
        assert_eq!(pacific_day_left(at("2026-01-15T08:00:00Z")), 1.0);
        assert_eq!(
            pacific_day_end(at("2026-01-15T08:00:00Z")),
            at("2026-01-16T08:00:00Z")
        );

        assert_eq!(
            pacific_day_end(at("2026-03-07T08:00:00Z")),
            at("2026-03-08T08:00:00Z")
        );
        assert_eq!(
            pacific_day_end(at("2026-03-08T08:00:00Z")),
            at("2026-03-09T07:00:00Z")
        );
        assert_eq!(
            pacific_day_end(at("2026-10-31T07:00:00Z")),
            at("2026-11-01T07:00:00Z")
        );
        assert_eq!(
            pacific_day_end(at("2026-11-01T07:00:00Z")),
            at("2026-11-02T08:00:00Z")
        );
    }
}
