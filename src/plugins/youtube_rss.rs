//! Roster discovery from YouTube's channel Atom feeds and uploads playlists.
//!
//! Holodex sometimes never returns a stream at all, and without a video ID the
//! `videos.list` overlay has nothing to correct. The public
//! `feeds/videos.xml?channel_id=` feed lists each channel's latest uploads,
//! waiting rooms included, at no quota cost. A background worker reads the
//! roster's feeds, has `videos.list` classify the IDs, and keeps the ones that
//! are live or upcoming so they can be merged into Holodex results.
//!
//! The feed is best effort: it is cached for up to 15 min, has no published
//! rate limit, and goes down across many channels for hours at a time. So the
//! worker backs off per feed, pauses on repeated 429s, and treats a mostly
//! failing pass as an outage. The uploads playlist (`playlistItems.list`,
//! 1 unit) is polled when the key pool's budget allows a faster cadence than
//! RSS, and always while RSS is down or paused.
//!
//! Atom carries no live status, so the worker only runs while a YouTube Data
//! API key is configured.

use super::holodex::{HolodexChannel, HolodexStream};
use super::http::{pooled_client, response_bytes_limited};
use super::youtube_data::{google_get, usable_key_count, DAILY_UNIT_BUDGET};
use crate::config::{load_config, Config};
use futures_util::stream::{self, StreamExt};
use regex::Regex;
use reqwest::header::{ETAG, IF_MODIFIED_SINCE, IF_NONE_MATCH, LAST_MODIFIED, RETRY_AFTER};
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

const FEED_URL: &str = "https://www.youtube.com/feeds/videos.xml?channel_id=";
const PLAYLIST_ITEMS_URL: &str = "https://www.googleapis.com/youtube/v3/playlistItems";
/// Base loop. RSS, playlist polls and classification keep their own schedules.
const LOOP: Duration = Duration::from_secs(30);
const RSS_TICK: Duration = Duration::from_secs(180);
const FEED_CONCURRENCY: usize = 2;
const FEED_JITTER_MS: u64 = 500;
const PLAYLIST_CONCURRENCY: usize = 4;
/// A 429 without `Retry-After` backs the feed off this long, doubling.
const RATE_LIMIT_BACKOFF: Duration = Duration::from_secs(30 * 60);
/// A single failing feed (404/5xx) backs off from one RSS tick, doubling.
const FEED_ERROR_BACKOFF: Duration = RSS_TICK;
const MAX_FEED_BACKOFF: Duration = Duration::from_secs(60 * 60);
/// A `Retry-After` beyond this is treated as bogus.
const MAX_RETRY_AFTER: Duration = Duration::from_secs(6 * 60 * 60);
/// This many 429s in one pass pause RSS as a whole.
const PAUSE_AFTER_429S: usize = 3;
const OUTAGE_PROBE_EVERY: Duration = Duration::from_secs(10 * 60);
/// Fewer attempted feeds than this cannot tell an outage from a few bad channels.
const OUTAGE_MIN_FEEDS: usize = 4;
/// Daily units per pool kept for classification, the overlays and the monitor.
const RESERVED_UNITS: u64 = 3_000;
const MIN_PLAYLIST_INTERVAL: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, PartialEq)]
pub struct FeedEntry {
    pub video_id: String,
    pub channel_id: String,
    pub channel_name: String,
    pub title: String,
    pub published: Option<String>,
}

fn entry_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?s)<entry>(.*?)</entry>").expect("valid regex"))
}

fn tag_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"(?s)<yt:videoId>(?P<vid>[^<]*)</yt:videoId>|<yt:channelId>(?P<cid>[^<]*)</yt:channelId>|<name>(?P<name>[^<]*)</name>|<published>(?P<pub>[^<]*)</published>|<title>(?P<title>[^<]*)</title>",
        )
        .expect("valid regex")
    })
}

fn unescape(text: &str) -> String {
    text.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&amp;", "&")
}

/// Entries of one channel feed. Only the first of each tag inside an entry
/// counts, so `<media:title>` and nested descriptions cannot override it.
pub fn parse_feed(xml: &str) -> Vec<FeedEntry> {
    entry_regex()
        .captures_iter(xml)
        .filter_map(|entry| {
            let body = entry.get(1)?.as_str();
            let mut parsed = FeedEntry {
                video_id: String::new(),
                channel_id: String::new(),
                channel_name: String::new(),
                title: String::new(),
                published: None,
            };
            for tag in tag_regex().captures_iter(body) {
                let set = |slot: &mut String, name: &str| {
                    if slot.is_empty() {
                        if let Some(value) = tag.name(name) {
                            *slot = unescape(value.as_str().trim());
                        }
                    }
                };
                set(&mut parsed.video_id, "vid");
                set(&mut parsed.channel_id, "cid");
                set(&mut parsed.channel_name, "name");
                set(&mut parsed.title, "title");
                if parsed.published.is_none() {
                    parsed.published = tag.name("pub").map(|m| m.as_str().trim().to_string());
                }
            }
            (!parsed.video_id.is_empty() && !parsed.channel_id.is_empty()).then_some(parsed)
        })
        .collect()
}

/// YouTube channel IDs from channels.json plus the active channel.
async fn roster_channel_ids(cfg: &Config) -> Vec<String> {
    let mut ids = Vec::new();
    let mut seen = HashSet::new();
    let mut push = |id: &str| {
        let id = id.trim();
        if !id.is_empty() && seen.insert(id.to_string()) {
            ids.push(id.to_string());
        }
    };

    let path = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join("channels.json")));
    if let Some(path) = path {
        if let Ok(content) = tokio::fs::read_to_string(path).await {
            if let Ok(json) = serde_json::from_str::<serde_json::Value>(&content) {
                if let Some(channels) = json.get("channels").and_then(|v| v.as_array()) {
                    for channel in channels {
                        if let Some(id) = channel
                            .get("platforms")
                            .and_then(|p| p.get("youtube"))
                            .and_then(|v| v.as_str())
                        {
                            push(id);
                        }
                    }
                } else if let Some(channels) = json.get("YT_channels").and_then(|v| v.as_array()) {
                    for channel in channels {
                        if let Some(id) = channel.get("channel_id").and_then(|v| v.as_str()) {
                            push(id);
                        }
                    }
                }
            }
        }
    }
    push(&cfg.youtube.channel_id);
    ids
}

#[derive(Default)]
struct Discovery {
    /// Live or upcoming rows, already classified by YouTube.
    found: HashMap<String, HolodexStream>,
    /// IDs YouTube called a plain upload, ended, or omitted. Pruned to the IDs
    /// still in some feed or playlist, so it stays bounded.
    ignored: HashSet<String>,
}

static DISCOVERY: Mutex<Option<Discovery>> = Mutex::new(None);

fn base_row(entry: &FeedEntry) -> HolodexStream {
    HolodexStream {
        id: entry.video_id.clone(),
        title: entry.title.clone(),
        stream_type: "stream".to_string(),
        topic_id: None,
        published_at: entry.published.clone(),
        available_at: None,
        status: "upcoming".to_string(),
        start_scheduled: None,
        start_actual: None,
        live_viewers: None,
        channel: HolodexChannel {
            id: entry.channel_id.clone(),
            name: entry.channel_name.clone(),
            photo: None,
        },
        link: None,
        thumbnail: None,
        placeholder_type: None,
        yt_confirmed: false,
    }
}

#[derive(Debug, Clone, PartialEq)]
enum FeedOutcome {
    Fresh {
        entries: Vec<FeedEntry>,
        etag: Option<String>,
        last_modified: Option<String>,
    },
    NotModified,
    RateLimited {
        retry_after: Option<Duration>,
    },
    /// 404, 5xx, another status, or no response at all.
    Failed,
}

#[derive(Debug, Default)]
struct FeedState {
    failures: u32,
    retry_at: Option<Instant>,
    etag: Option<String>,
    last_modified: Option<String>,
    /// What the feed last listed, kept while it fails or is skipped.
    entries: Vec<FeedEntry>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RssMode {
    Up,
    /// Too many 429s: no feed is read until then.
    Paused {
        until: Instant,
    },
    /// Most feeds failing: read one rotating probe feed at a time.
    Down {
        probe_at: Instant,
    },
}

/// RSS health as a pure state machine: `plan` says which feeds to read,
/// `record` takes what came back.
struct RssHealth {
    mode: RssMode,
    feeds: HashMap<String, FeedState>,
    probe_cursor: usize,
}

impl Default for RssHealth {
    fn default() -> Self {
        Self {
            mode: RssMode::Up,
            feeds: HashMap::new(),
            probe_cursor: 0,
        }
    }
}

/// `base`, doubled for each failure after the first, capped at an hour.
fn backoff(base: Duration, failures: u32) -> Duration {
    base.saturating_mul(1 << failures.saturating_sub(1).min(8))
        .min(MAX_FEED_BACKOFF)
}

impl RssHealth {
    fn plan(&mut self, roster: &[String], now: Instant) -> Vec<String> {
        self.feeds.retain(|id, _| roster.contains(id));
        match self.mode {
            RssMode::Paused { until } if now < until => Vec::new(),
            RssMode::Down { probe_at } => {
                if now < probe_at || roster.is_empty() {
                    return Vec::new();
                }
                let probe = roster[self.probe_cursor % roster.len()].clone();
                self.probe_cursor += 1;
                vec![probe]
            }
            RssMode::Paused { .. } | RssMode::Up => {
                if self.mode != RssMode::Up {
                    tracing::info!("YouTube RSS 暂停结束，恢复读取");
                    self.mode = RssMode::Up;
                }
                roster
                    .iter()
                    .filter(|id| {
                        self.feeds
                            .get(*id)
                            .and_then(|feed| feed.retry_at)
                            .is_none_or(|at| at <= now)
                    })
                    .cloned()
                    .collect()
            }
        }
    }

    fn validators(&self, channel_id: &str) -> (Option<String>, Option<String>) {
        self.feeds
            .get(channel_id)
            .map(|feed| (feed.etag.clone(), feed.last_modified.clone()))
            .unwrap_or_default()
    }

    fn record(&mut self, results: Vec<(String, FeedOutcome)>, now: Instant) {
        let attempted = results.len();
        let retry_afters: Vec<Option<Duration>> = results
            .iter()
            .filter_map(|(_, outcome)| match outcome {
                FeedOutcome::RateLimited { retry_after } => Some(*retry_after),
                _ => None,
            })
            .collect();
        let failed = results
            .iter()
            .filter(|(_, outcome)| *outcome == FeedOutcome::Failed)
            .count();
        let answered = results.iter().any(|(_, outcome)| {
            matches!(
                outcome,
                FeedOutcome::Fresh { .. } | FeedOutcome::NotModified
            )
        });

        let mut penalize = true;
        if let RssMode::Down { .. } = self.mode {
            // Probes of a known outage never penalize the probed channel.
            penalize = false;
            if answered {
                tracing::info!("YouTube RSS 已恢复");
                self.mode = RssMode::Up;
            } else if attempted > 0 {
                self.mode = RssMode::Down {
                    probe_at: now + OUTAGE_PROBE_EVERY,
                };
            }
        } else if retry_afters.len() >= PAUSE_AFTER_429S {
            let longest = retry_afters.iter().flatten().max().copied();
            let pause = RATE_LIMIT_BACKOFF.max(longest.unwrap_or_default());
            tracing::warn!(
                "YouTube RSS 本轮 {} 个 feed 返回 429，暂停 {} 分钟",
                retry_afters.len(),
                pause.as_secs() / 60
            );
            self.mode = RssMode::Paused { until: now + pause };
        } else if attempted >= OUTAGE_MIN_FEEDS && failed * 2 >= attempted {
            tracing::warn!(
                "YouTube RSS 本轮 {}/{} 个 feed 失败，视为 RSS 故障，改为每 {} 分钟探测一次",
                failed,
                attempted,
                OUTAGE_PROBE_EVERY.as_secs() / 60
            );
            self.mode = RssMode::Down {
                probe_at: now + OUTAGE_PROBE_EVERY,
            };
            penalize = false;
        }

        for (id, outcome) in results {
            let feed = self.feeds.entry(id).or_default();
            match outcome {
                FeedOutcome::Fresh {
                    entries,
                    etag,
                    last_modified,
                } => {
                    *feed = FeedState {
                        entries,
                        etag,
                        last_modified,
                        ..FeedState::default()
                    };
                }
                FeedOutcome::NotModified => {
                    feed.failures = 0;
                    feed.retry_at = None;
                }
                FeedOutcome::RateLimited { retry_after } => {
                    feed.failures += 1;
                    let wait =
                        retry_after.unwrap_or_else(|| backoff(RATE_LIMIT_BACKOFF, feed.failures));
                    feed.retry_at = Some(now + wait);
                }
                FeedOutcome::Failed if penalize => {
                    feed.failures += 1;
                    feed.retry_at = Some(now + backoff(FEED_ERROR_BACKOFF, feed.failures));
                }
                FeedOutcome::Failed => {}
            }
        }
    }

    /// RSS cannot be relied on right now, so the playlist backstop runs.
    fn degraded(&self, now: Instant) -> bool {
        match self.mode {
            RssMode::Up => false,
            RssMode::Paused { until } => now < until,
            RssMode::Down { .. } => true,
        }
    }

    fn entries(&self) -> impl Iterator<Item = &FeedEntry> {
        self.feeds.values().flat_map(|feed| feed.entries.iter())
    }
}

/// Seconds or an HTTP date, capped at `MAX_RETRY_AFTER`.
fn parse_retry_after(value: &str, now: chrono::DateTime<chrono::Utc>) -> Option<Duration> {
    let value = value.trim();
    let wait = match value.parse::<u64>() {
        Ok(secs) => Duration::from_secs(secs),
        Err(_) => (chrono::DateTime::parse_from_rfc2822(value)
            .ok()?
            .with_timezone(&chrono::Utc)
            - now)
            .to_std()
            .unwrap_or_default(),
    };
    Some(wait.min(MAX_RETRY_AFTER))
}

fn header_string(
    response: &reqwest::Response,
    name: reqwest::header::HeaderName,
) -> Option<String> {
    response
        .headers()
        .get(name)?
        .to_str()
        .ok()
        .map(str::to_string)
}

fn unix_now() -> Duration {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
}

async fn fetch_feed(
    client: &reqwest::Client,
    channel_id: &str,
    etag: Option<String>,
    last_modified: Option<String>,
) -> FeedOutcome {
    // Spread the two concurrent requests a little.
    let jitter = u64::from(unix_now().subsec_nanos()) % FEED_JITTER_MS;
    tokio::time::sleep(Duration::from_millis(jitter)).await;

    // The feed is served with max-age=900; a unique query skips shared caches.
    let url = format!("{FEED_URL}{channel_id}&_={}", unix_now().as_secs());
    let mut request = client.get(url).timeout(Duration::from_secs(15));
    if let Some(etag) = etag {
        request = request.header(IF_NONE_MATCH, etag);
    }
    if let Some(last_modified) = last_modified {
        request = request.header(IF_MODIFIED_SINCE, last_modified);
    }
    let response = match request.send().await {
        Ok(response) => response,
        Err(e) => {
            tracing::debug!("YouTube RSS {} 读取失败: {}", channel_id, e);
            return FeedOutcome::Failed;
        }
    };
    let status = response.status();
    if status == reqwest::StatusCode::NOT_MODIFIED {
        return FeedOutcome::NotModified;
    }
    if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
        let retry_after = header_string(&response, RETRY_AFTER)
            .and_then(|value| parse_retry_after(&value, chrono::Utc::now()));
        tracing::debug!(
            "YouTube RSS {} 429, Retry-After {:?}",
            channel_id,
            retry_after
        );
        return FeedOutcome::RateLimited { retry_after };
    }
    if !status.is_success() {
        tracing::debug!("YouTube RSS {} 读取失败: {}", channel_id, status);
        return FeedOutcome::Failed;
    }
    let etag = header_string(&response, ETAG);
    let last_modified = header_string(&response, LAST_MODIFIED);
    match response_bytes_limited(response, 2 * 1024 * 1024).await {
        Ok(bytes) => FeedOutcome::Fresh {
            entries: parse_feed(&String::from_utf8_lossy(&bytes)),
            etag,
            last_modified,
        },
        Err(e) => {
            tracing::debug!("YouTube RSS {} 读取失败: {}", channel_id, e);
            FeedOutcome::Failed
        }
    }
}

#[derive(Deserialize)]
struct PlaylistResponse {
    #[serde(default)]
    items: Vec<PlaylistItem>,
}

#[derive(Deserialize)]
struct PlaylistItem {
    #[serde(default)]
    snippet: PlaylistSnippet,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct PlaylistSnippet {
    #[serde(default)]
    title: String,
    #[serde(default)]
    channel_title: String,
    published_at: Option<String>,
    #[serde(default)]
    resource_id: PlaylistResource,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct PlaylistResource {
    #[serde(default)]
    video_id: String,
}

/// A channel's uploads playlist is its ID with `UC` swapped for `UU`.
fn uploads_playlist_id(channel_id: &str) -> Option<String> {
    channel_id
        .strip_prefix("UC")
        .filter(|rest| !rest.is_empty())
        .map(|rest| format!("UU{rest}"))
}

/// Uploads-playlist items in the same shape as feed entries.
fn playlist_entries(channel_id: &str, response: PlaylistResponse) -> Vec<FeedEntry> {
    response
        .items
        .into_iter()
        .filter(|item| !item.snippet.resource_id.video_id.is_empty())
        .map(|item| FeedEntry {
            video_id: item.snippet.resource_id.video_id,
            channel_id: channel_id.to_string(),
            channel_name: item.snippet.channel_title,
            title: item.snippet.title,
            published: item.snippet.published_at,
        })
        .collect()
}

/// Per-channel poll interval the pool's budget allows, never under a minute.
fn playlist_interval(channels: usize, usable_keys: usize) -> Option<Duration> {
    let budget = (usable_keys as u64 * u64::from(DAILY_UNIT_BUDGET)).checked_sub(RESERVED_UNITS)?;
    if channels == 0 || budget == 0 {
        return None;
    }
    let secs = (86_400 * channels as u64).div_ceil(budget);
    Some(Duration::from_secs(secs).max(MIN_PLAYLIST_INTERVAL))
}

enum PlaylistOutcome {
    Entries(Vec<FeedEntry>),
    NotFound,
    Failed,
}

async fn fetch_playlist(
    keys: Vec<String>,
    proxy: Option<String>,
    channel_id: String,
) -> (String, PlaylistOutcome) {
    let Some(playlist) = uploads_playlist_id(&channel_id) else {
        return (channel_id, PlaylistOutcome::NotFound);
    };
    let query = [
        ("part", "snippet"),
        ("playlistId", playlist.as_str()),
        ("maxResults", "5"),
    ];
    let result =
        google_get::<PlaylistResponse>(&keys, proxy.as_deref(), PLAYLIST_ITEMS_URL, &query, 1)
            .await
            .map_err(|e| e.to_string());
    let outcome = match result {
        Ok(Some(response)) => PlaylistOutcome::Entries(playlist_entries(&channel_id, response)),
        Ok(None) => PlaylistOutcome::NotFound,
        Err(e) => {
            tracing::debug!("YouTube 上传列表 {} 读取失败: {}", channel_id, e);
            PlaylistOutcome::Failed
        }
    };
    (channel_id, outcome)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Source {
    Rss,
    Playlist,
}

impl Source {
    fn label(self) -> &'static str {
        match self {
            Source::Rss => "rss",
            Source::Playlist => "playlist",
        }
    }
}

/// Seconds since YouTube's `actualStartTime`.
fn seconds_since(start: &str) -> Option<i64> {
    let start = chrono::DateTime::parse_from_rfc3339(start).ok()?;
    Some((chrono::Utc::now() - start.with_timezone(&chrono::Utc)).num_seconds())
}

#[derive(Default)]
struct Worker {
    roster: Vec<String>,
    rss: RssHealth,
    next_rss: Option<Instant>,
    /// Re-check of known rows and retry after a failed classification.
    next_recheck: Option<Instant>,
    playlist_on: bool,
    /// Next uploads-playlist poll per channel.
    playlist_due: HashMap<String, Instant>,
    /// What each uploads playlist last listed.
    playlist_seen: HashMap<String, Vec<FeedEntry>>,
    /// Channels without an uploads playlist, until the roster changes.
    no_playlist: HashSet<String>,
}

impl Worker {
    async fn tick(&mut self) -> Result<(), String> {
        let cfg = load_config().await.map_err(|e| e.to_string())?;
        let keys = cfg.youtube_api_keys();
        if keys.is_empty() {
            // Nothing can be classified without a key; drop stale rows too.
            *DISCOVERY.lock().unwrap_or_else(|e| e.into_inner()) = None;
            *self = Worker::default();
            return Ok(());
        }
        let roster = roster_channel_ids(&cfg).await;
        if roster != self.roster {
            self.no_playlist.clear();
            self.playlist_due.retain(|id, _| roster.contains(id));
            self.playlist_seen.retain(|id, _| roster.contains(id));
            self.roster = roster.clone();
        }
        let proxy = cfg.youtube.proxy.clone();
        let now = Instant::now();
        // IDs listed by a fetch made in this loop.
        let mut fresh: HashMap<String, Source> = HashMap::new();

        if self.next_rss.is_none_or(|at| at <= now) {
            self.next_rss = Some(now + RSS_TICK);
            self.read_feeds(&roster, proxy.as_deref(), &mut fresh)
                .await?;
        }
        self.poll_playlists(&roster, &keys, proxy, &mut fresh, now)
            .await;
        self.classify(&keys, cfg.youtube.proxy.as_deref(), &fresh, now)
            .await
    }

    async fn read_feeds(
        &mut self,
        roster: &[String],
        proxy: Option<&str>,
        fresh: &mut HashMap<String, Source>,
    ) -> Result<(), String> {
        let client = pooled_client(proxy).map_err(|e| e.to_string())?;
        let jobs: Vec<(String, Option<String>, Option<String>)> = self
            .rss
            .plan(roster, Instant::now())
            .into_iter()
            .map(|id| {
                let (etag, last_modified) = self.rss.validators(&id);
                (id, etag, last_modified)
            })
            .collect();
        // Owned items: futures borrowing the roster make tick() unspawnable.
        let results: Vec<(String, FeedOutcome)> = stream::iter(jobs)
            .map(|(id, etag, last_modified)| {
                let client = client.clone();
                async move {
                    let outcome = fetch_feed(&client, &id, etag, last_modified).await;
                    (id, outcome)
                }
            })
            .buffer_unordered(FEED_CONCURRENCY)
            .collect()
            .await;
        for (_, outcome) in &results {
            if let FeedOutcome::Fresh { entries, .. } = outcome {
                for entry in entries {
                    fresh.entry(entry.video_id.clone()).or_insert(Source::Rss);
                }
            }
        }
        self.rss.record(results, Instant::now());
        Ok(())
    }

    async fn poll_playlists(
        &mut self,
        roster: &[String],
        keys: &[String],
        proxy: Option<String>,
        fresh: &mut HashMap<String, Source>,
        now: Instant,
    ) {
        let interval = playlist_interval(roster.len(), usable_key_count(keys));
        let degraded = self.rss.degraded(now);
        let active = interval.filter(|interval| *interval < RSS_TICK || degraded);
        if active.is_some() != self.playlist_on {
            self.playlist_on = active.is_some();
            match active {
                Some(interval) => tracing::info!(
                    "YouTube 上传列表轮询开启: 每频道 {}s{}",
                    interval.as_secs(),
                    if degraded { " (RSS 不可用)" } else { "" }
                ),
                None => tracing::info!("YouTube 上传列表轮询关闭"),
            }
        }
        let Some(interval) = active else {
            self.playlist_due.clear();
            self.playlist_seen.clear();
            return;
        };

        // Spread first polls evenly over one interval.
        let count = roster.len().max(1) as u32;
        for (index, id) in roster.iter().enumerate() {
            self.playlist_due
                .entry(id.clone())
                .or_insert(now + interval * index as u32 / count);
        }
        let live_channels: HashSet<String> = {
            let guard = DISCOVERY.lock().unwrap_or_else(|e| e.into_inner());
            guard
                .iter()
                .flat_map(|state| state.found.values())
                .filter(|row| row.status == "live")
                .map(|row| row.channel.id.clone())
                .collect()
        };
        let due: Vec<String> = roster
            .iter()
            .filter(|id| !self.no_playlist.contains(*id) && !live_channels.contains(*id))
            .filter(|id| self.playlist_due.get(*id).is_some_and(|at| *at <= now))
            .cloned()
            .collect();
        for id in &due {
            self.playlist_due.insert(id.clone(), now + interval);
        }

        let results: Vec<(String, PlaylistOutcome)> = stream::iter(due)
            .map(|id| fetch_playlist(keys.to_vec(), proxy.clone(), id))
            .buffer_unordered(PLAYLIST_CONCURRENCY)
            .collect()
            .await;
        for (id, outcome) in results {
            match outcome {
                PlaylistOutcome::Entries(entries) => {
                    for entry in &entries {
                        fresh
                            .entry(entry.video_id.clone())
                            .or_insert(Source::Playlist);
                    }
                    self.playlist_seen.insert(id, entries);
                }
                PlaylistOutcome::NotFound => {
                    tracing::info!("YouTube 频道 {} 没有上传列表，停止轮询", id);
                    self.no_playlist.insert(id);
                }
                PlaylistOutcome::Failed => {}
            }
        }
    }

    async fn classify(
        &mut self,
        keys: &[String],
        proxy: Option<&str>,
        fresh: &HashMap<String, Source>,
        now: Instant,
    ) -> Result<(), String> {
        let mut entries: HashMap<String, (FeedEntry, Source)> = HashMap::new();
        for entry in self.rss.entries() {
            entries.insert(entry.video_id.clone(), (entry.clone(), Source::Rss));
        }
        for entry in self.playlist_seen.values().flatten() {
            entries
                .entry(entry.video_id.clone())
                .or_insert_with(|| (entry.clone(), Source::Playlist));
        }
        let recheck = self.next_recheck.is_none_or(|at| at <= now);

        let (candidates, known) = {
            let mut guard = DISCOVERY.lock().unwrap_or_else(|e| e.into_inner());
            let state = guard.get_or_insert_with(Discovery::default);
            state.ignored.retain(|id| entries.contains_key(id));
            let known: Vec<HolodexStream> = if recheck {
                state.found.values().cloned().collect()
            } else {
                Vec::new()
            };
            let mut candidates: Vec<String> = entries
                .keys()
                .filter(|id| !state.found.contains_key(*id) && !state.ignored.contains(*id))
                .cloned()
                .collect();
            candidates.extend(known.iter().map(|row| row.id.clone()));
            (candidates, known)
        };
        if candidates.is_empty() {
            return Ok(());
        }

        let videos = match super::youtube_data::videos_for(keys, proxy, candidates.clone())
            .await
            .map_err(|e| e.to_string())
        {
            Ok(videos) => videos,
            Err(e) => {
                // Unclassified candidates wait for the next re-check instead
                // of retrying every loop.
                self.next_recheck = Some(now + RSS_TICK);
                return Err(e);
            }
        };
        if recheck {
            self.next_recheck = Some(now + RSS_TICK);
        }

        let known_ids: HashSet<String> = known.iter().map(|row| row.id.clone()).collect();
        let new_rows: Vec<HolodexStream> = candidates
            .iter()
            .filter(|id| !known_ids.contains(*id))
            .filter_map(|id| entries.get(id))
            .map(|(entry, _)| base_row(entry))
            .collect();
        // Rows YouTube omitted would pass through the overlay unchanged; they are
        // private or deleted, so drop them here instead.
        let rows: Vec<HolodexStream> = known
            .into_iter()
            .chain(new_rows)
            .filter(|row| videos.contains_key(&row.id))
            .collect();
        let classified = super::youtube_data::overlay(rows, &videos);

        let mut guard = DISCOVERY.lock().unwrap_or_else(|e| e.into_inner());
        let state = guard.get_or_insert_with(Discovery::default);
        // Re-checked rows are replaced by their new answer, or dropped.
        state.found.retain(|id, _| !known_ids.contains(id));
        for row in classified {
            if row.status == "live" && !known_ids.contains(&row.id) {
                let source = fresh
                    .get(&row.id)
                    .or_else(|| entries.get(&row.id).map(|(_, source)| source))
                    .copied()
                    .unwrap_or(Source::Rss);
                let since = row.start_actual.as_deref().and_then(seconds_since);
                tracing::info!(
                    "发现直播 {} via {}, 开播后 {}s",
                    row.id,
                    source.label(),
                    since.map_or_else(|| "?".to_string(), |secs| secs.to_string())
                );
            }
            state.found.insert(row.id.clone(), row);
        }
        for id in candidates {
            if !state.found.contains_key(&id) {
                state.ignored.insert(id);
            }
        }
        Ok(())
    }
}

/// Holodex rows plus discovered live/upcoming rows Holodex did not send.
/// Callers filter by channel and horizon afterwards as usual.
pub fn merge_discovered(streams: Vec<HolodexStream>) -> Vec<HolodexStream> {
    let guard = DISCOVERY.lock().unwrap_or_else(|e| e.into_inner());
    let Some(state) = guard.as_ref() else {
        return streams;
    };
    merge_rows(streams, state.found.values())
}

fn merge_rows<'a>(
    mut streams: Vec<HolodexStream>,
    found: impl Iterator<Item = &'a HolodexStream>,
) -> Vec<HolodexStream> {
    let present: HashSet<String> = streams.iter().map(|s| s.id.clone()).collect();
    streams.extend(found.filter(|row| !present.contains(&row.id)).cloned());
    streams
}

static RSS_WORKER_STARTED: AtomicBool = AtomicBool::new(false);

pub struct RssDiscoveryWorker(tokio::task::JoinHandle<()>);

impl Drop for RssDiscoveryWorker {
    fn drop(&mut self) {
        self.0.abort();
        RSS_WORKER_STARTED.store(false, Ordering::SeqCst);
    }
}

pub fn start_rss_discovery_worker() -> Option<RssDiscoveryWorker> {
    if RSS_WORKER_STARTED.swap(true, Ordering::SeqCst) {
        return None;
    }
    Some(RssDiscoveryWorker(tokio::spawn(async {
        let mut worker = Worker::default();
        loop {
            if let Err(e) = worker.tick().await {
                tracing::warn!("YouTube 直播发现失败: {}", e);
            }
            tokio::time::sleep(LOOP).await;
        }
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    const FEED: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<feed xmlns:yt="http://www.youtube.com/xml/schemas/2015" xmlns="http://www.w3.org/2005/Atom">
 <yt:channelId>gYCMluaLpERsyNXlPOvBtA</yt:channelId>
 <title>Channel</title>
 <entry>
  <id>yt:video:BnV1AEeqBR4</id>
  <yt:videoId>BnV1AEeqBR4</yt:videoId>
  <yt:channelId>UCgYCMluaLpERsyNXlPOvBtA</yt:channelId>
  <title>Bomb &amp; defuse</title>
  <author><name>かみと</name></author>
  <published>2026-09-24T10:59:38+00:00</published>
  <media:group><media:title>other</media:title></media:group>
 </entry>
 <entry>
  <yt:videoId>second</yt:videoId>
  <yt:channelId>UCgYCMluaLpERsyNXlPOvBtA</yt:channelId>
  <title>Two</title>
 </entry>
</feed>"#;

    #[test]
    fn parses_each_entry_with_its_full_channel_id() {
        let entries = parse_feed(FEED);
        assert_eq!(entries.len(), 2);
        assert_eq!(
            entries[0],
            FeedEntry {
                video_id: "BnV1AEeqBR4".to_string(),
                channel_id: "UCgYCMluaLpERsyNXlPOvBtA".to_string(),
                channel_name: "かみと".to_string(),
                title: "Bomb & defuse".to_string(),
                published: Some("2026-09-24T10:59:38+00:00".to_string()),
            }
        );
        assert_eq!(entries[1].video_id, "second");
        assert_eq!(entries[1].published, None);
    }

    #[test]
    fn merging_adds_only_ids_holodex_did_not_send() {
        let entry = |id: &str| FeedEntry {
            video_id: id.to_string(),
            channel_id: "UC1".to_string(),
            channel_name: String::new(),
            title: format!("{id} rss"),
            published: None,
        };
        let mut holodex = base_row(&entry("shared"));
        holodex.title = "holodex".to_string();
        let found = [base_row(&entry("shared")), base_row(&entry("missing"))];

        let merged = merge_rows(vec![holodex], found.iter());
        let ids: Vec<&str> = merged.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, vec!["shared", "missing"]);
        assert_eq!(merged[0].title, "holodex");
    }

    fn roster(n: usize) -> Vec<String> {
        (0..n).map(|i| format!("UC{i}")).collect()
    }

    fn pass(ids: &[String], outcome: FeedOutcome) -> Vec<(String, FeedOutcome)> {
        ids.iter().map(|id| (id.clone(), outcome.clone())).collect()
    }

    fn fresh_feed() -> FeedOutcome {
        FeedOutcome::Fresh {
            entries: Vec::new(),
            etag: Some("\"e1\"".to_string()),
            last_modified: None,
        }
    }

    #[test]
    fn feed_backoff_doubles_from_its_base_and_caps_at_an_hour() {
        assert_eq!(backoff(FEED_ERROR_BACKOFF, 1), Duration::from_secs(180));
        assert_eq!(backoff(FEED_ERROR_BACKOFF, 2), Duration::from_secs(360));
        assert_eq!(backoff(FEED_ERROR_BACKOFF, 5), Duration::from_secs(2880));
        assert_eq!(backoff(FEED_ERROR_BACKOFF, 6), MAX_FEED_BACKOFF);
        assert_eq!(backoff(RATE_LIMIT_BACKOFF, 1), Duration::from_secs(1800));
        assert_eq!(backoff(RATE_LIMIT_BACKOFF, 2), MAX_FEED_BACKOFF);
        assert_eq!(backoff(RATE_LIMIT_BACKOFF, 40), MAX_FEED_BACKOFF);
    }

    #[test]
    fn a_single_failing_feed_backs_off_alone_and_success_resets_it() {
        let ids = roster(6);
        let now = Instant::now();
        let mut health = RssHealth::default();
        let mut results = pass(&ids, fresh_feed());
        results[0].1 = FeedOutcome::Failed;
        health.record(results, now);
        assert_eq!(health.mode, RssMode::Up);
        assert_eq!(health.plan(&ids, now).len(), 5);
        assert_eq!(health.validators("UC1").0.as_deref(), Some("\"e1\""));

        let later = now + FEED_ERROR_BACKOFF;
        assert_eq!(health.plan(&ids, later).len(), 6);
        health.record(vec![(ids[0].clone(), FeedOutcome::NotModified)], later);
        assert_eq!(health.feeds["UC0"].failures, 0);
    }

    #[test]
    fn a_429_honors_retry_after_and_three_pause_every_feed() {
        let ids = roster(6);
        let now = Instant::now();
        let mut health = RssHealth::default();
        let limited = |secs: Option<u64>| FeedOutcome::RateLimited {
            retry_after: secs.map(Duration::from_secs),
        };
        let mut results = pass(&ids, fresh_feed());
        results[0].1 = limited(Some(90));
        results[1].1 = limited(None);
        health.record(results, now);
        assert_eq!(health.mode, RssMode::Up);
        assert_eq!(
            health.feeds["UC0"].retry_at,
            Some(now + Duration::from_secs(90))
        );
        assert_eq!(health.feeds["UC1"].retry_at, Some(now + RATE_LIMIT_BACKOFF));

        let mut results = pass(&ids, fresh_feed());
        results[2].1 = limited(Some(7200));
        results[3].1 = limited(None);
        results[4].1 = limited(Some(60));
        health.record(results, now);
        let until = now + Duration::from_secs(7200);
        assert_eq!(health.mode, RssMode::Paused { until });
        assert!(health.degraded(now));
        assert!(health.plan(&ids, now).is_empty());
        assert!(!health.plan(&ids, until).is_empty());
        assert_eq!(health.mode, RssMode::Up);
    }

    #[test]
    fn a_mostly_failing_pass_is_an_outage_probed_one_feed_at_a_time() {
        let ids = roster(6);
        let now = Instant::now();
        let mut health = RssHealth::default();
        let mut results = pass(&ids, FeedOutcome::Failed);
        results[5].1 = fresh_feed();
        health.record(results, now);
        let probe_at = now + OUTAGE_PROBE_EVERY;
        assert_eq!(health.mode, RssMode::Down { probe_at });
        assert!(health.degraded(now));
        assert!(
            health.feeds.values().all(|feed| feed.failures == 0),
            "outage passes do not penalize channels"
        );

        assert!(health.plan(&ids, now).is_empty());
        assert_eq!(health.plan(&ids, probe_at), vec!["UC0".to_string()]);
        health.record(pass(&roster(1), FeedOutcome::Failed), probe_at);
        let next = probe_at + OUTAGE_PROBE_EVERY;
        assert_eq!(health.mode, RssMode::Down { probe_at: next });
        assert_eq!(health.plan(&ids, next), vec!["UC1".to_string()]);
        health.record(vec![("UC1".to_string(), fresh_feed())], next);
        assert_eq!(health.mode, RssMode::Up);
        assert!(!health.degraded(next));
        assert_eq!(health.feeds["UC0"].failures, 0);
    }

    #[test]
    fn a_small_roster_is_never_called_an_outage() {
        let ids = roster(OUTAGE_MIN_FEEDS - 1);
        let mut health = RssHealth::default();
        health.record(pass(&ids, FeedOutcome::Failed), Instant::now());
        assert_eq!(health.mode, RssMode::Up);
        assert!(health.feeds.values().all(|feed| feed.failures == 1));
    }

    #[test]
    fn retry_after_takes_seconds_or_an_http_date() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-09-24T07:28:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        assert_eq!(
            parse_retry_after(" 120 ", now),
            Some(Duration::from_secs(120))
        );
        assert_eq!(
            parse_retry_after("Thu, 24 Sep 2026 07:30:00 GMT", now),
            Some(Duration::from_secs(120))
        );
        assert_eq!(
            parse_retry_after("Thu, 24 Sep 2026 07:00:00 GMT", now),
            Some(Duration::ZERO)
        );
        assert_eq!(parse_retry_after("999999", now), Some(MAX_RETRY_AFTER));
        assert_eq!(parse_retry_after("soon", now), None);
    }

    #[test]
    fn playlist_interval_follows_the_key_count() {
        let secs = |keys| playlist_interval(37, keys).map(|d| d.as_secs());
        assert_eq!(secs(0), None);
        assert_eq!(secs(1), Some(640));
        assert_eq!(secs(2), Some(246));
        assert_eq!(secs(3), Some(153));
        assert_eq!(secs(4), Some(111));
        assert_eq!(secs(6), Some(72));
        assert_eq!(playlist_interval(3, 6), Some(MIN_PLAYLIST_INTERVAL));
        assert_eq!(playlist_interval(0, 2), None);
    }

    #[test]
    fn the_uploads_playlist_swaps_uc_for_uu() {
        assert_eq!(
            uploads_playlist_id("UCgYCMluaLpERsyNXlPOvBtA").as_deref(),
            Some("UUgYCMluaLpERsyNXlPOvBtA")
        );
        assert_eq!(uploads_playlist_id("UC"), None);
        assert_eq!(uploads_playlist_id("HCabc"), None);
    }

    #[test]
    fn playlist_items_become_feed_entries() {
        let response: PlaylistResponse = serde_json::from_value(serde_json::json!({
            "items": [
                {"snippet": {
                    "title": "Waiting room",
                    "channelTitle": "かみと",
                    "publishedAt": "2026-09-24T10:59:38Z",
                    "resourceId": {"kind": "youtube#video", "videoId": "BnV1AEeqBR4"}
                }},
                {"snippet": {"title": "no id"}}
            ]
        }))
        .unwrap();
        let entries = playlist_entries("UC1", response);
        assert_eq!(
            entries,
            vec![FeedEntry {
                video_id: "BnV1AEeqBR4".to_string(),
                channel_id: "UC1".to_string(),
                channel_name: "かみと".to_string(),
                title: "Waiting room".to_string(),
                published: Some("2026-09-24T10:59:38Z".to_string()),
            }]
        );
    }
}
