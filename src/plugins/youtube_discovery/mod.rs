//! YouTube stream discovery: RSS feeds, uploads playlists and WebSub pushes.
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
//! RSS, and while RSS is disabled, down or paused.
//!
//! Atom carries no live status, so the worker only runs while a YouTube Data
//! API key is configured.

mod rss;

use rss::{fetch_feed, FeedOutcome, RssHealth};
pub use rss::{parse_feed, FeedEntry};

use super::holodex::{HolodexChannel, HolodexStream};
use super::http::pooled_client;
use super::youtube_data::{
    google_get, lane_book, set_target_reserved, usable_key_count, Lane, LaneBook,
    DAILY_UNIT_BUDGET, RESERVED_UNITS, TARGET_RESERVED_UNITS,
};
use crate::config::{load_config, Config};
use chrono::{DateTime, Timelike, Utc};
use futures_util::stream::{self, StreamExt};
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

const PLAYLIST_ITEMS_URL: &str = "https://www.googleapis.com/youtube/v3/playlistItems";
/// Base loop. RSS, playlist polls and classification keep their own schedules.
const LOOP: Duration = Duration::from_secs(30);
const RSS_TICK: Duration = Duration::from_secs(180);
const FEED_CONCURRENCY: usize = 2;
const PLAYLIST_CONCURRENCY: usize = 4;
const MIN_PLAYLIST_INTERVAL: Duration = Duration::from_secs(60);
/// Per channel, the go-live weighting polls between every 15 min in the
/// roster's quietest hours and every minute (`MIN_PLAYLIST_INTERVAL`) in its
/// busiest.
const MIN_POLLS_PER_HOUR: f64 = 4.0;
const MAX_POLLS_PER_HOUR: f64 = 60.0;
const TARGET_POLL_INTERVAL: Duration = Duration::from_secs(60);
/// Pushes are classified this long after the first one, in one batch.
const PUSH_DEBOUNCE: Duration = Duration::from_secs(2);

/// YouTube channel IDs from channels.json plus the active channel.
pub(crate) async fn roster_channel_ids(cfg: &Config) -> Vec<String> {
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

/// Uploads-playlist poll for the restream target: index mode, monitor on.
fn restream_target(cfg: &Config) -> Option<String> {
    if super::youtube::monitor_mode(cfg) != super::youtube::MonitorMode::Index {
        return None;
    }
    // Reserve the target's cadence deterministically when priority is also on.
    let monitored = super::youtube::monitored_channels(cfg);
    if monitored.contains(&cfg.youtube.channel_id) {
        Some(cfg.youtube.channel_id.clone())
    } else if monitored.contains(&cfg.priority_channel.youtube_channel_id) {
        Some(cfg.priority_channel.youtube_channel_id.clone())
    } else {
        None
    }
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
fn playlist_interval(
    channels: usize,
    usable_keys: usize,
    target_reserved: bool,
) -> Option<Duration> {
    let reserved = RESERVED_UNITS
        + if target_reserved {
            TARGET_RESERVED_UNITS
        } else {
            0
        };
    let budget = (usable_keys as u64 * u64::from(DAILY_UNIT_BUDGET)).checked_sub(reserved)?;
    if channels == 0 || budget == 0 {
        return None;
    }
    let secs = (86_400 * channels as u64).div_ceil(budget);
    Some(Duration::from_secs(secs).max(MIN_PLAYLIST_INTERVAL))
}

/// The restream target's uploads-playlist cadence, stretched with the pool.
fn target_poll_interval(stretch: u32) -> Duration {
    TARGET_POLL_INTERVAL * stretch.max(1)
}

/// Whether to fetch the restream target's uploads playlist this cycle.
fn should_poll_target(
    now: Instant,
    target_due: Option<Instant>,
    live: bool,
    already_due: bool,
    no_playlist: bool,
) -> bool {
    if live || already_due || no_playlist {
        return false;
    }
    target_due.is_none_or(|at| at <= now)
}

/// Whether roster polling runs: only while its day-average interval beats the
/// RSS tick, or while RSS is down. Decided on the average so it does not
/// switch on and off with the hour.
fn polling_on(base: Option<Duration>, rss_down: bool) -> bool {
    base.is_some_and(|interval| interval < RSS_TICK || rss_down)
}

/// The roster lane's units for today: the pool minus the protected reserves,
/// halved while WebSub pushes are healthy (the restream target is in the
/// protected lane, so its cadence never changes).
fn playlist_day_units(usable_keys: usize, target_reserved: bool, websub_slowed: bool) -> u64 {
    let reserved = RESERVED_UNITS
        + if target_reserved {
            TARGET_RESERVED_UNITS
        } else {
            0
        };
    let units = (usable_keys as u64 * u64::from(DAILY_UNIT_BUDGET)).saturating_sub(reserved);
    if websub_slowed {
        units / 2
    } else {
        units
    }
}

/// Go-live weights spread over neighbouring hours (1-2-1, around the clock),
/// so a sparse hour between two busy ones is not starved.
fn smooth(weights: &[f64; 24]) -> [f64; 24] {
    std::array::from_fn(|h| {
        (weights[(h + 23) % 24] + 2.0 * weights[h] + weights[(h + 1) % 24]) / 4.0
    })
}

/// Every UTC hour, whole: a day's plan.
const WHOLE_DAY: [f64; 24] = [1.0; 24];

/// Polls per channel for each UTC hour that spend `units` over `span`, how
/// much of each UTC hour the plan covers. Flat without weights, as before the
/// roster had a history. With weights, each hour polls in proportion to its
/// smoothed go-lives, bounded by `MIN_POLLS_PER_HOUR` and
/// `MAX_POLLS_PER_HOUR`, at the one scale that still spends `units`.
fn hourly_polls(
    channels: usize,
    units: u64,
    weights: Option<&[f64; 24]>,
    span: &[f64; 24],
) -> [f64; 24] {
    let hours: f64 = span.iter().sum();
    if channels == 0 || hours <= 0.0 {
        return [0.0; 24];
    }
    let per_channel = units as f64 / channels as f64;
    let flat = [(per_channel / hours).min(MAX_POLLS_PER_HOUR); 24];
    let Some(weights) = weights else {
        return flat;
    };
    let smoothed = smooth(weights);
    // A hair of weight everywhere, so hours without go-lives still take what
    // is left once every other hour is at the one-minute cap.
    let hair = smoothed.iter().copied().fold(0.0, f64::max) * 1e-6;
    if hair <= 0.0
        || per_channel <= hours * MIN_POLLS_PER_HOUR
        || per_channel >= hours * MAX_POLLS_PER_HOUR
    {
        return flat;
    }
    let at = |scale: f64| -> [f64; 24] {
        std::array::from_fn(|h| {
            (scale * (smoothed[h] + hair)).clamp(MIN_POLLS_PER_HOUR, MAX_POLLS_PER_HOUR)
        })
    };
    let spend = |polls: [f64; 24]| polls.iter().zip(span).map(|(p, s)| p * s).sum::<f64>();
    // Spend only grows with the scale: bracket it, then halve the bracket.
    let (mut low, mut high) = (0.0, 1.0);
    while spend(at(high)) < per_channel {
        high *= 2.0;
    }
    for _ in 0..64 {
        let mid = (low + high) / 2.0;
        if spend(at(mid)) < per_channel {
            low = mid;
        } else {
            high = mid;
        }
    }
    at(high)
}

/// Seconds between polls at this rate, from a minute to a day.
fn interval_of(polls_per_hour: f64) -> Duration {
    const DAY: Duration = Duration::from_secs(86_400);
    if polls_per_hour <= 0.0 {
        return DAY;
    }
    Duration::from_secs_f64((3600.0 / polls_per_hour).ceil().min(DAY.as_secs_f64()))
        .max(MIN_PLAYLIST_INTERVAL)
}

/// How much of each UTC hour is still ahead in the Pacific day, from 0 to 1.
fn remaining_span(now: DateTime<Utc>, day_left: f64) -> [f64; 24] {
    let mut span = [0.0; 24];
    let mut at = now.timestamp();
    let end = at + (day_left * 86_400.0).round() as i64;
    while at < end {
        let hour = at.div_euclid(3600);
        let next = ((hour + 1) * 3600).min(end);
        span[hour.rem_euclid(24) as usize] += (next - at) as f64 / 3600.0;
        at = next;
    }
    span
}

/// Shortfalls under this are reported as on plan. Polls land at the start of
/// each interval, so spend runs up to a poll per channel ahead of the plan;
/// the refit absorbs that without anyone needing to hear about it.
const SHORT_REPORTED_AT: f64 = 1.05;

/// How the roster's day plan fits the room the protected lane leaves it.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
enum Fit {
    /// The plan fits.
    #[default]
    Plan,
    /// Refit into the room left; the plan wanted this many times more.
    Short(f64),
    /// Not even one more poll per channel fits: the roster waits so that the
    /// protected lane does not.
    Paused,
}

impl Fit {
    /// As the settings view and the log report it: shortfalls under
    /// `SHORT_REPORTED_AT` count as on plan, and the factor rounds up to a
    /// tenth.
    fn reported(self) -> Fit {
        match self {
            Fit::Short(factor) if factor < SHORT_REPORTED_AT => Fit::Plan,
            Fit::Short(factor) => Fit::Short((factor * 10.0).ceil() / 10.0),
            other => other,
        }
    }
}

/// The roster's polls per UTC hour for what is left of the Pacific day
/// (`span`). It keeps to the day's plan, never faster, while `room` covers
/// the rest of it; otherwise `room` is spread over the hours left by the same
/// rule.
fn fit_plan(
    channels: usize,
    day_units: u64,
    weights: Option<&[f64; 24]>,
    span: &[f64; 24],
    room: u64,
) -> ([f64; 24], Fit) {
    let plan = hourly_polls(channels, day_units, weights, &WHOLE_DAY);
    let wanted = plan.iter().zip(span).map(|(p, s)| p * s).sum::<f64>() * channels as f64;
    if wanted <= room as f64 {
        (plan, Fit::Plan)
    } else if room < channels as u64 {
        (plan, Fit::Paused)
    } else {
        (
            hourly_polls(channels, room, weights, span),
            Fit::Short(wanted / room as f64),
        )
    }
}

/// A channel stamped under a slower hour is due no later than one current
/// interval from now, so the busy hours start on time.
fn pull_in(due: &mut HashMap<String, Instant>, now: Instant, interval: Duration) {
    let Some(latest) = now.checked_add(interval) else {
        return;
    };
    for at in due.values_mut() {
        if *at > latest {
            *at = latest;
        }
    }
}

/// Fewest usable keys whose interval beats the RSS tick for this roster.
fn keys_needed(channels: usize, target_reserved: bool) -> Option<usize> {
    if channels == 0 {
        return None;
    }
    (1..).find(|keys| {
        playlist_interval(channels, *keys, target_reserved)
            .is_some_and(|interval| interval < RSS_TICK)
    })
}

/// What the last `poll_playlists` decided, for the settings view.
#[derive(Clone, Copy)]
struct PlaylistStatus {
    on: bool,
    /// This hour's interval.
    interval: Option<Duration>,
    /// How the roster's plan fits the room the protected lane leaves it, as
    /// reported.
    fit: Fit,
    /// The store's and the restream target's multiplier.
    target_stretch: u32,
    rss_enabled: bool,
    rss_down: bool,
    roster_len: usize,
    target_reserved: bool,
    websub_slowed: bool,
    /// Weighted by the roster's go-live hours rather than flat.
    by_hour: bool,
    /// Go-lives counted across the roster (decayed).
    golives: f64,
    /// Per UTC hour: share of the roster's go-lives, and the poll interval.
    golive_share: [f64; 24],
    hour_intervals: [Option<Duration>; 24],
    book: LaneBook,
    day_units: u64,
}

static PLAYLIST_STATUS: Mutex<Option<PlaylistStatus>> = Mutex::new(None);

/// Uploads-playlist polling as of the worker's last loop; `None` before the
/// first loop or without a key.
pub(crate) fn playlist_status() -> Option<serde_json::Value> {
    let status = (*PLAYLIST_STATUS.lock().unwrap_or_else(|e| e.into_inner()))?;
    Some(serde_json::json!({
        "on": status.on,
        "interval_secs": status.interval.map(|interval| interval.as_secs()),
        "stretch": match status.fit {
            Fit::Short(factor) => factor,
            Fit::Plan | Fit::Paused => 1.0,
        },
        "paused": status.fit == Fit::Paused,
        "target_stretch": status.target_stretch,
        "rss_down": status.rss_down,
        "rss_enabled": status.rss_enabled,
        "keys_needed": keys_needed(status.roster_len, status.target_reserved),
        "websub_slowed": status.websub_slowed,
        "by_hour": status.by_hour,
        "golives": status.golives.round() as u64,
        "golives_needed": super::youtube_hours::MIN_GOLIVES as u64,
        "hours": (0..24)
            .map(|h| serde_json::json!({
                "golive_share": status.golive_share[h],
                "interval_secs": status.hour_intervals[h].map(|interval| interval.as_secs()),
            }))
            .collect::<Vec<_>>(),
        "lanes": {
            "protected_used": status.book.protected_used,
            "protected_budget": status.book.protected_budget,
            "playlist_used": status.book.playlist_used,
            "playlist_budget": status.day_units,
            "playlist_room": status.book.playlist_room(),
        },
    }))
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
    lane: Lane,
) -> (String, PlaylistOutcome) {
    let Some(playlist) = uploads_playlist_id(&channel_id) else {
        return (channel_id, PlaylistOutcome::NotFound);
    };
    let query = [
        ("part", "snippet"),
        ("playlistId", playlist.as_str()),
        ("maxResults", "5"),
    ];
    let result = google_get::<PlaylistResponse>(
        &keys,
        proxy.as_deref(),
        PLAYLIST_ITEMS_URL,
        &query,
        1,
        lane,
    )
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
    WebSub,
}

impl Source {
    fn label(self) -> &'static str {
        match self {
            Source::Rss => "rss",
            Source::Playlist => "playlist",
            Source::WebSub => "websub",
        }
    }
}

/// First listings are kept this long, longer than a push can be late.
const FIRST_SEEN_KEEP: Duration = Duration::from_secs(48 * 60 * 60);

/// When RSS or an uploads playlist first listed each video, so a WebSub push
/// can say whether it came first (`found_before`).
#[derive(Default)]
struct FirstSeen(HashMap<String, (Source, Instant)>);

impl FirstSeen {
    /// Notes the IDs not listed before; forgets listings past `FIRST_SEEN_KEEP`.
    fn note<'a>(&mut self, ids: impl IntoIterator<Item = &'a str>, source: Source, now: Instant) {
        self.0
            .retain(|_, (_, at)| now.saturating_duration_since(*at) < FIRST_SEEN_KEEP);
        for id in ids {
            self.0.entry(id.to_string()).or_insert((source, now));
        }
    }

    fn before(&self, id: &str, now: Instant) -> Option<(Source, Duration)> {
        self.0
            .get(id)
            .map(|(source, at)| (*source, now.saturating_duration_since(*at)))
    }
}

static FIRST_SEEN: Mutex<Option<FirstSeen>> = Mutex::new(None);

fn note_first_seen<'a>(ids: impl IntoIterator<Item = &'a str>, source: Source) {
    let mut guard = FIRST_SEEN.lock().unwrap_or_else(|e| e.into_inner());
    guard
        .get_or_insert_with(FirstSeen::default)
        .note(ids, source, Instant::now());
}

/// Which discovery source (`rss` / `playlist`) listed this video first, and
/// how long ago; `None` when neither has.
pub(crate) fn found_before(video_id: &str) -> Option<(&'static str, Duration)> {
    let guard = FIRST_SEEN.lock().unwrap_or_else(|e| e.into_inner());
    guard
        .as_ref()?
        .before(video_id, Instant::now())
        .map(|(source, ago)| (source.label(), ago))
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
    rss_enabled: bool,
    next_rss: Option<Instant>,
    /// Re-check of known rows and retry after a failed classification.
    next_recheck: Option<Instant>,
    playlist_on: bool,
    /// How the roster's plan last fit, as reported.
    fit: Fit,
    /// Next uploads-playlist poll per channel.
    playlist_due: HashMap<String, Instant>,
    /// What each uploads playlist last listed.
    playlist_seen: HashMap<String, Vec<FeedEntry>>,
    /// Channels without an uploads playlist, until the roster changes.
    no_playlist: HashSet<String>,
    /// Restream target last polled on the 60s cadence.
    target_id: Option<String>,
    target_due: Option<Instant>,
    /// Roster polling halved because WebSub pushes are healthy.
    websub_slowed: bool,
    /// Roster polling weighted by go-live hours.
    by_hour: bool,
}

impl Worker {
    fn configure_rss(&mut self, enabled: bool) {
        if self.rss_enabled != enabled {
            self.rss_enabled = enabled;
            // Resume promptly but retain per-feed backoff and HTTP validators.
            self.next_rss = None;
        }
    }

    async fn tick(&mut self) -> Result<(), String> {
        if crate::cluster::yt_index_role().is_peer() {
            // The owner node discovers for the cluster; its rows come with the index.
            return Ok(());
        }
        let cfg = load_config().await.map_err(|e| e.to_string())?;
        let keys = cfg.youtube_api_keys();
        if keys.is_empty() {
            // Nothing can be classified without a key; drop stale rows too.
            *DISCOVERY.lock().unwrap_or_else(|e| e.into_inner()) = None;
            *PLAYLIST_STATUS.lock().unwrap_or_else(|e| e.into_inner()) = None;
            *self = Worker::default();
            crate::webui::holodex_list::wake();
            return Ok(());
        }
        let roster = roster_channel_ids(&cfg).await;
        let target = restream_target(&cfg);
        if roster != self.roster {
            self.no_playlist.clear();
            self.playlist_due.retain(|id, _| roster.contains(id));
            self.playlist_seen
                .retain(|id, _| roster.contains(id) || target.as_deref() == Some(id.as_str()));
            self.roster = roster.clone();
        }
        let proxy = cfg.youtube.proxy.clone();
        self.configure_rss(cfg.youtube_rss_enabled);
        let now = Instant::now();
        // IDs listed by a fetch made in this loop, or pushed since the last.
        let mut fresh: HashMap<String, Source> = HashMap::new();
        let (pushed, pushed_fresh) = super::youtube_websub::pushed_entries();
        for id in pushed_fresh {
            fresh.insert(id, Source::WebSub);
        }

        if self.rss_enabled && self.next_rss.is_none_or(|at| at <= now) {
            self.next_rss = Some(now + RSS_TICK);
            self.read_feeds(&roster, proxy.as_deref(), &mut fresh)
                .await?;
        }
        self.poll_playlists(&roster, &keys, proxy, &mut fresh, now, target)
            .await;
        self.classify(&keys, cfg.youtube.proxy.as_deref(), &fresh, &pushed, now)
            .await
    }

    async fn read_feeds(
        &mut self,
        roster: &[String],
        proxy: Option<&str>,
        fresh: &mut HashMap<String, Source>,
    ) -> Result<(), String> {
        if !self.rss_enabled {
            return Ok(());
        }
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
                note_first_seen(
                    entries.iter().map(|entry| entry.video_id.as_str()),
                    Source::Rss,
                );
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
        target: Option<String>,
    ) {
        let target_reserved = target.is_some();
        set_target_reserved(target_reserved);
        let usable = usable_key_count(keys);
        let base = playlist_interval(roster.len(), usable, target_reserved);
        let degraded = self.rss_enabled && self.rss.degraded(now);
        let on = polling_on(base, !self.rss_enabled || degraded);
        let websub_slowed = super::youtube_websub::healthy();
        if websub_slowed != self.websub_slowed {
            self.websub_slowed = websub_slowed;
            if on {
                tracing::info!(
                    "{}",
                    if websub_slowed {
                        "WebSub 推送正常，上传列表轮询放慢一倍 (转播目标不变)"
                    } else {
                        "WebSub 推送中断，上传列表轮询恢复原速"
                    }
                );
            }
        }

        // Today's roster units, spread by the hours the roster goes live, and
        // slowed only as far as the protected lane's remaining need requires.
        let utc = Utc::now();
        let book = lane_book(keys);
        let day_units = playlist_day_units(usable, target_reserved, websub_slowed);
        let hours = super::youtube_hours::weights(roster);
        let span = remaining_span(utc, book.day_left);
        let (polls, fit) = fit_plan(
            roster.len(),
            day_units,
            hours.as_ref(),
            &span,
            book.playlist_room(),
        );
        let hour_intervals: [Option<Duration>; 24] = std::array::from_fn(|h| {
            base?;
            (fit != Fit::Paused).then(|| interval_of(polls[h]))
        });
        let interval = hour_intervals[utc.hour() as usize];
        let fit = fit.reported();
        let target_stretch = book.protected_stretch();

        let by_hour = hours.is_some();
        if by_hour != self.by_hour {
            self.by_hour = by_hour;
            match interval.filter(|_| on && by_hour) {
                Some(interval) => tracing::info!(
                    "YouTube 上传列表按开播时段轮询: 本时段每频道 {}s",
                    interval.as_secs()
                ),
                None if !by_hour => tracing::info!("YouTube 上传列表恢复均匀轮询"),
                None => {}
            }
        }
        // Shown while it is still learning, too.
        let counted = super::youtube_hours::roster_hours(roster);
        let golive_total: f64 = counted.iter().sum();
        *PLAYLIST_STATUS.lock().unwrap_or_else(|e| e.into_inner()) = Some(PlaylistStatus {
            on,
            interval,
            fit,
            target_stretch,
            rss_enabled: self.rss_enabled,
            rss_down: degraded,
            roster_len: roster.len(),
            target_reserved,
            websub_slowed,
            by_hour,
            golives: golive_total,
            golive_share: std::array::from_fn(|h| {
                if golive_total > 0.0 {
                    counted[h] / golive_total
                } else {
                    0.0
                }
            }),
            hour_intervals,
            book,
            day_units,
        });
        let fit_changed = fit != self.fit;
        self.fit = fit;
        let active = interval.filter(|_| on);
        if on != self.playlist_on {
            self.playlist_on = on;
            match active {
                Some(interval) => tracing::info!(
                    "YouTube 上传列表轮询开启: 本时段每频道 {}s{}",
                    interval.as_secs(),
                    if degraded { " (RSS 不可用)" } else { "" }
                ),
                None => tracing::info!("YouTube 上传列表轮询关闭"),
            }
        } else if on && fit_changed {
            match (fit, active) {
                (Fit::Short(factor), Some(interval)) => tracing::info!(
                    "YouTube 配额需留给索引与转播目标，上传列表轮询慢于计划 {} 倍: 本时段每频道 {}s",
                    factor,
                    interval.as_secs()
                ),
                (_, Some(interval)) => tracing::info!(
                    "YouTube 上传列表轮询恢复计划: 本时段每频道 {}s",
                    interval.as_secs()
                ),
                (_, None) => tracing::info!("YouTube 配额只够索引与转播目标，上传列表轮询暂停"),
            }
        }
        let stretch = target_stretch;

        let live_channels: HashSet<String> = {
            let guard = DISCOVERY.lock().unwrap_or_else(|e| e.into_inner());
            guard
                .iter()
                .flat_map(|state| state.found.values())
                .filter(|row| row.status == "live")
                .map(|row| row.channel.id.clone())
                .collect()
        };

        let mut due: Vec<String> = Vec::new();
        if let Some(interval) = active {
            pull_in(&mut self.playlist_due, now, interval);
            // Spread first polls evenly over one interval.
            let count = roster.len().max(1) as u32;
            for (index, id) in roster.iter().enumerate() {
                self.playlist_due
                    .entry(id.clone())
                    .or_insert(now + interval * index as u32 / count);
            }
            due = roster
                .iter()
                .filter(|id| !self.no_playlist.contains(*id) && !live_channels.contains(*id))
                .filter(|id| self.playlist_due.get(*id).is_some_and(|at| *at <= now))
                .cloned()
                .collect();
            for id in &due {
                self.playlist_due.insert(id.clone(), now + interval);
            }
        } else {
            self.playlist_due.clear();
            if let Some(ref id) = target {
                self.playlist_seen.retain(|k, _| k == id);
            } else {
                self.playlist_seen.clear();
            }
        }

        if self.target_id.as_deref() != target.as_deref() {
            if let Some(ref id) = target {
                tracing::info!(
                    "YouTube 转播目标 {} 上传列表轮询: 每 {}s",
                    id,
                    target_poll_interval(stretch).as_secs()
                );
            }
            self.target_id = target.clone();
            self.target_due = None;
        }
        if let Some(ref id) = target {
            let live = live_channels.contains(id);
            let already_due = due.iter().any(|due_id| due_id == id);
            let no_playlist = self.no_playlist.contains(id);
            if already_due {
                self.target_due = Some(now + target_poll_interval(stretch));
            } else if should_poll_target(now, self.target_due, live, already_due, no_playlist) {
                due.push(id.clone());
                self.target_due = Some(now + target_poll_interval(stretch));
            }
        } else {
            self.target_due = None;
        }

        if due.is_empty() {
            return;
        }

        let results: Vec<(String, PlaylistOutcome)> = stream::iter(due)
            .map(|id| {
                let lane = if target.as_deref() == Some(id.as_str()) {
                    Lane::Protected
                } else {
                    Lane::Playlist
                };
                fetch_playlist(keys.to_vec(), proxy.clone(), id, lane)
            })
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
                    note_first_seen(
                        entries.iter().map(|entry| entry.video_id.as_str()),
                        Source::Playlist,
                    );
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
        pushed: &[FeedEntry],
        now: Instant,
    ) -> Result<(), String> {
        let mut entries: HashMap<String, (FeedEntry, Source)> = HashMap::new();
        for entry in pushed {
            entries.insert(entry.video_id.clone(), (entry.clone(), Source::WebSub));
        }
        for entry in self.rss.entries().filter(|_| self.rss_enabled) {
            entries
                .entry(entry.video_id.clone())
                .or_insert_with(|| (entry.clone(), Source::Rss));
        }
        for entry in self.playlist_seen.values().flatten() {
            entries
                .entry(entry.video_id.clone())
                .or_insert_with(|| (entry.clone(), Source::Playlist));
        }
        let pushed_known = |state: &Discovery| {
            fresh.iter().any(|(id, source)| {
                *source == Source::WebSub
                    && state
                        .found
                        .get(id)
                        .is_some_and(|row| row.status == "upcoming")
            })
        };

        let recheck;
        let (candidates, known) = {
            let mut guard = DISCOVERY.lock().unwrap_or_else(|e| e.into_inner());
            let state = guard.get_or_insert_with(Discovery::default);
            state.ignored.retain(|id| entries.contains_key(id));
            // A push for a known waiting room re-checks the known rows now.
            recheck = self.next_recheck.is_none_or(|at| at <= now) || pushed_known(state);
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
        drop(guard);
        crate::webui::holodex_list::wake();
        Ok(())
    }
}

/// Holodex rows plus discovered live/upcoming rows Holodex did not send, and
/// streams seen live moments ago that both have since dropped (see
/// `youtube_data::recent_live_rows`). Callers filter by channel and horizon
/// afterwards as usual.
pub fn merge_discovered(streams: Vec<HolodexStream>) -> Vec<HolodexStream> {
    if let crate::cluster::YtIndexRole::Peer(index) = crate::cluster::yt_index_role() {
        // The owner's discovered and recently live rows.
        return merge_rows(streams, index.discovered.iter());
    }
    let found: Vec<HolodexStream> = DISCOVERY
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .map(|state| state.found.values().cloned().collect())
        .unwrap_or_default();
    let present: HashSet<String> = streams.iter().chain(&found).map(|s| s.id.clone()).collect();
    let recent = super::youtube_data::recent_live_rows(&present);
    merge_rows(streams, found.iter().chain(&recent))
}

fn merge_rows<'a>(
    mut streams: Vec<HolodexStream>,
    found: impl Iterator<Item = &'a HolodexStream>,
) -> Vec<HolodexStream> {
    let mut present: HashSet<String> = streams.iter().map(|s| s.id.clone()).collect();
    for row in found {
        if present.insert(row.id.clone()) {
            streams.push(row.clone());
        }
    }
    streams
}

static DISCOVERY_WORKER_STARTED: AtomicBool = AtomicBool::new(false);

pub struct DiscoveryWorker(tokio::task::JoinHandle<()>);

impl Drop for DiscoveryWorker {
    fn drop(&mut self) {
        self.0.abort();
        DISCOVERY_WORKER_STARTED.store(false, Ordering::SeqCst);
    }
}

pub fn start_discovery_worker() -> Option<DiscoveryWorker> {
    if DISCOVERY_WORKER_STARTED.swap(true, Ordering::SeqCst) {
        return None;
    }
    Some(DiscoveryWorker(tokio::spawn(async {
        let mut worker = Worker::default();
        loop {
            if let Err(e) = worker.tick().await {
                tracing::warn!("YouTube 直播发现失败: {}", e);
            }
            tokio::select! {
                _ = tokio::time::sleep(LOOP) => {}
                _ = super::youtube_websub::pushed() => {
                    // Let a burst of pushes land in one videos.list batch.
                    tokio::time::sleep(PUSH_DEBOUNCE).await;
                }
            }
        }
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

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

        // A row both discovered and recently live is added once.
        let recent = [base_row(&entry("missing")), base_row(&entry("dropped"))];
        let merged = merge_rows(Vec::new(), found.iter().chain(&recent));
        let ids: Vec<&str> = merged.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, vec!["shared", "missing", "dropped"]);
    }

    fn roster(n: usize) -> Vec<String> {
        (0..n).map(|i| format!("UC{i}")).collect()
    }

    #[test]
    fn playlist_interval_follows_the_key_count() {
        let secs = |keys| playlist_interval(37, keys, false).map(|d| d.as_secs());
        assert_eq!(secs(0), None);
        assert_eq!(secs(1), Some(533));
        assert_eq!(secs(2), Some(214));
        assert_eq!(secs(3), Some(134));
        assert_eq!(secs(4), Some(97));
        assert_eq!(secs(6), Some(63));
        assert_eq!(playlist_interval(3, 6, false), Some(MIN_PLAYLIST_INTERVAL));
        assert_eq!(playlist_interval(0, 2, false), None);
    }

    #[test]
    fn healthy_websub_halves_the_roster_units_without_switching_it_off() {
        let three_keys = playlist_interval(37, 3, true);
        assert!(polling_on(three_keys, false));
        let full = hourly_polls(37, playlist_day_units(3, true, false), None, &WHOLE_DAY);
        let slowed = hourly_polls(37, playlist_day_units(3, true, true), None, &WHOLE_DAY);
        assert_eq!(interval_of(full[0]), three_keys.unwrap());
        assert!(
            interval_of(slowed[0]) > RSS_TICK,
            "past the RSS tick, yet still on"
        );
        assert!((full[0] / slowed[0] - 2.0).abs() < 0.01);
        assert_eq!(
            target_poll_interval(1),
            TARGET_POLL_INTERVAL,
            "the restream target keeps its 60s cadence"
        );
    }

    /// Go-lives per JST hour in ny's index on 2026-09-28 (443 streams),
    /// rotated into UTC buckets.
    fn measured_weights() -> [f64; 24] {
        let jst = [
            12.0, 7.0, 1.0, 2.0, 1.0, 3.0, 4.0, 4.0, 11.0, 4.0, 5.0, 8.0, 20.0, 5.0, 19.0, 21.0,
            27.0, 27.0, 43.0, 54.0, 63.0, 49.0, 28.0, 25.0,
        ];
        std::array::from_fn(|utc| jst[(utc + 9) % 24])
    }

    #[test]
    fn polls_follow_the_go_live_hours_within_fifteen_minutes_and_a_minute() {
        let units = 12_000;
        let polls = hourly_polls(33, units, Some(&measured_weights()), &WHOLE_DAY);
        let secs = |jst: usize| interval_of(polls[(jst + 15) % 24]).as_secs();
        for jst in 2..=6 {
            assert_eq!(secs(jst), 900, "{jst}:00 JST is quiet");
        }
        for jst in 18..=21 {
            assert!(secs(jst) < 119, "{jst}:00 JST is under half of flat 238s");
        }
        assert!(polls
            .iter()
            .all(|p| (MIN_POLLS_PER_HOUR - 1e-9..=MAX_POLLS_PER_HOUR + 1e-9).contains(p)));
        let spent: f64 = polls.iter().sum::<f64>() * 33.0;
        assert!(
            (spent - units as f64).abs() / (units as f64) < 0.01,
            "{spent}"
        );
    }

    #[test]
    fn polling_is_flat_without_a_history_and_capped_at_a_minute() {
        let flat = hourly_polls(33, 12_000, None, &WHOLE_DAY);
        assert!(flat.iter().all(|p| *p == flat[0]));
        assert_eq!(
            interval_of(flat[0]),
            Duration::from_secs(238),
            "today's WebSub-halved 3-key cadence"
        );
        let rich = hourly_polls(3, 50_000, Some(&measured_weights()), &WHOLE_DAY);
        assert!(rich.iter().all(|p| *p == MAX_POLLS_PER_HOUR));
    }

    #[test]
    fn a_busy_hour_pulls_in_channels_stamped_under_a_quiet_one() {
        let now = Instant::now();
        let mut due: HashMap<String, Instant> = [
            ("quiet".to_string(), now + Duration::from_secs(890)),
            ("soon".to_string(), now + Duration::from_secs(10)),
        ]
        .into_iter()
        .collect();
        pull_in(&mut due, now, Duration::from_secs(79));
        assert_eq!(due["quiet"], now + Duration::from_secs(79));
        assert_eq!(due["soon"], now + Duration::from_secs(10));
    }

    fn at(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    #[test]
    fn the_plan_covers_only_the_rest_of_the_pacific_day() {
        // 06:30 UTC in PDT: half an hour left of the Pacific day.
        let span = remaining_span(at("2026-09-28T06:30:00Z"), 1.0 / 48.0);
        assert_eq!(span[6], 0.5);
        assert_eq!(span.iter().sum::<f64>(), 0.5);
        // Twelve hours from 11:45 UTC: a quarter of 11:00, all of 12-22, and
        // three quarters of 23:00.
        let span = remaining_span(at("2026-09-28T11:45:00Z"), 0.5);
        assert_eq!(span[11], 0.25);
        assert!((12..23).all(|h| span[h] == 1.0));
        assert_eq!(span[23], 0.75);
        assert_eq!(span.iter().sum::<f64>(), 12.0);
    }

    #[test]
    fn small_shortfalls_report_as_on_plan() {
        assert_eq!(Fit::Short(1.04).reported(), Fit::Plan);
        assert_eq!(Fit::Short(1.31).reported(), Fit::Short(1.4));
        assert_eq!(Fit::Paused.reported(), Fit::Paused);
    }

    fn book(pool_left: u64, protected_used: u64, playlist_used: u64, day_left: f64) -> LaneBook {
        LaneBook {
            pool_left,
            protected_used,
            protected_budget: RESERVED_UNITS,
            playlist_used,
            day_left,
        }
    }

    #[test]
    fn the_roster_yields_to_the_protected_lane_and_the_store_never_waits() {
        // WebSub down, measured hours: the full 24k lane at 21:00 JST
        // (12:00 UTC, 5h into the PDT day), on plan so far.
        let day_units = playlist_day_units(3, false, false);
        let weights = measured_weights();
        let span = remaining_span(at("2026-09-28T12:00:00Z"), 19.0 / 24.0);
        let plan = hourly_polls(33, day_units, Some(&weights), &WHOLE_DAY);
        let wanted = plan.iter().zip(&span).map(|(p, s)| p * s).sum::<f64>() * 33.0;
        let spent = (day_units as f64 - wanted) as u64;
        let fit =
            |book: &LaneBook| fit_plan(33, day_units, Some(&weights), &span, book.playlist_room());

        // The protected lane on its reserve's pace: the roster keeps its plan.
        let on_plan = book(27_000 - spent - 625 + 50, 625, spent, 19.0 / 24.0);
        assert_eq!(fit(&on_plan), (plan, Fit::Plan));
        assert_eq!(on_plan.protected_stretch(), 1);

        // The store 1,000 units over: the roster slows, the store does not.
        let over = book(27_000 - spent - 1_625, 1_625, spent, 19.0 / 24.0);
        let (polls, fit_over) = fit(&over);
        assert!(
            matches!(fit_over, Fit::Short(factor) if factor > 1.0),
            "{fit_over:?}"
        );
        let refit = polls.iter().zip(&span).map(|(p, s)| p * s).sum::<f64>() * 33.0;
        assert!(refit <= over.playlist_room() as f64 + 1.0, "{refit}");
        assert_eq!(over.protected_stretch(), 1);

        // No room for another poll per channel: it waits, the store does not.
        let dry = book(2_000, 1_625, 25_000, 19.0 / 24.0);
        assert_eq!(fit(&dry).1, Fit::Paused);
    }

    /// Peers get pushed streams through the index's `discovered` rows, which
    /// the index node publishes from `merge_discovered(Vec::new())`.
    #[tokio::test]
    async fn a_pushed_live_stream_is_published_even_with_rss_disabled() {
        use super::super::youtube_data::{YtLiveDetails, YtSnippet, YtVideo};
        let entry = FeedEntry {
            video_id: "pushLiveVid1".to_string(),
            channel_id: "UCpushlive".to_string(),
            channel_name: "Name".to_string(),
            title: "Live now".to_string(),
            published: None,
        };
        super::super::youtube_index::record_for_test(
            std::slice::from_ref(&entry.video_id),
            vec![YtVideo {
                id: entry.video_id.clone(),
                snippet: YtSnippet {
                    title: "Live now".to_string(),
                    channel_id: "UCpushlive".to_string(),
                },
                live_streaming_details: Some(YtLiveDetails {
                    actual_start_time: Some("2026-09-27T00:00:00Z".to_string()),
                    ..Default::default()
                }),
            }],
        );
        let fresh: HashMap<String, Source> = [(entry.video_id.clone(), Source::WebSub)]
            .into_iter()
            .collect();
        Worker::default()
            .classify(
                &["key".to_string()],
                None,
                &fresh,
                std::slice::from_ref(&entry),
                Instant::now(),
            )
            .await
            .unwrap();
        let published = merge_discovered(Vec::new());
        assert!(published
            .iter()
            .any(|row| row.id == entry.video_id && row.status == "live"));
    }

    #[tokio::test]
    async fn pushes_for_old_or_private_videos_add_no_row() {
        use super::super::youtube_data::{YtLiveDetails, YtSnippet, YtVideo};
        let entry = |id: &str| FeedEntry {
            video_id: id.to_string(),
            channel_id: "UCpushtest".to_string(),
            channel_name: "Name".to_string(),
            title: "Edited title".to_string(),
            published: None,
        };
        let old = entry("pushOldVid01");
        let private = entry("pushPrivate1");
        super::super::youtube_index::record_for_test(
            &[old.video_id.clone(), private.video_id.clone()],
            vec![YtVideo {
                id: old.video_id.clone(),
                snippet: YtSnippet {
                    title: "Edited title".to_string(),
                    channel_id: "UCpushtest".to_string(),
                },
                live_streaming_details: Some(YtLiveDetails {
                    actual_start_time: Some("2026-09-01T00:00:00Z".to_string()),
                    actual_end_time: Some("2026-09-01T02:00:00Z".to_string()),
                    scheduled_start_time: None,
                    concurrent_viewers: None,
                }),
            }],
        );
        let fresh: HashMap<String, Source> = [
            (old.video_id.clone(), Source::WebSub),
            (private.video_id.clone(), Source::WebSub),
        ]
        .into_iter()
        .collect();
        let mut worker = Worker::default();
        worker
            .classify(
                &["key".to_string()],
                None,
                &fresh,
                &[old.clone(), private.clone()],
                Instant::now(),
            )
            .await
            .unwrap();
        let guard = DISCOVERY.lock().unwrap();
        let state = guard.as_ref().unwrap();
        for id in [&old.video_id, &private.video_id] {
            assert!(!state.found.contains_key(id), "{id} added");
            assert!(state.ignored.contains(id), "{id} not ignored");
        }
    }

    #[test]
    fn polling_runs_while_it_beats_rss_or_rss_is_down() {
        let three_keys = playlist_interval(37, 3, false);
        assert!(polling_on(three_keys, false));
        let one_key = playlist_interval(37, 1, false);
        assert!(!polling_on(one_key, false));
        assert!(polling_on(one_key, true), "RSS down keeps polling");
        assert!(!polling_on(None, true));
    }

    #[test]
    fn keys_needed_is_the_fewest_that_beat_rss() {
        assert_eq!(keys_needed(37, false), Some(3));
        assert_eq!(keys_needed(1, false), Some(1));
        assert_eq!(keys_needed(0, false), None);
    }

    #[test]
    fn the_target_reserve_lengthens_the_roster_interval() {
        assert_eq!(
            playlist_interval(37, 1, true).map(|d| d.as_secs()),
            Some(702)
        );
        assert_eq!(keys_needed(30, false), Some(2));
        assert_eq!(
            keys_needed(30, true),
            Some(3),
            "the 1,440-unit target reserve can cost a key"
        );
        assert_eq!(keys_needed(37, true), Some(3));
    }

    #[test]
    fn the_restream_target_polls_every_minute_unless_live_or_already_due() {
        let now = Instant::now();
        assert_eq!(target_poll_interval(1), TARGET_POLL_INTERVAL);
        assert_eq!(target_poll_interval(2), TARGET_POLL_INTERVAL * 2);
        assert!(should_poll_target(now, None, false, false, false));
        assert!(!should_poll_target(
            now,
            Some(now + TARGET_POLL_INTERVAL),
            false,
            false,
            false
        ));
        assert!(should_poll_target(
            now + TARGET_POLL_INTERVAL,
            Some(now + TARGET_POLL_INTERVAL),
            false,
            false,
            false
        ));
        assert!(
            !should_poll_target(now, None, true, false, false),
            "already live in discovery"
        );
        assert!(
            !should_poll_target(now, None, false, true, false),
            "roster poll already has it"
        );
        assert!(!should_poll_target(now, None, false, false, true));
    }

    #[test]
    fn restream_target_needs_index_mode_and_the_monitor() {
        let mut cfg: crate::config::Config = serde_json::from_value(serde_json::json!({
            "auto_cover": false, "enable_anti_collision": false, "interval": 15,
            "bililive": { "enable_danmaku_command": false, "room": 1, "bili_rtmp_url": "", "bili_rtmp_key": "" },
            "youtube": {}, "twitch": {}, "enable_lol_monitor": false, "anti_collision_list": {}
        }))
        .unwrap();
        cfg.youtube.channel_id = "UCtarget".to_string();
        cfg.youtube.enable_monitor = true;
        cfg.holodex_monitor_gate = true;
        cfg.youtube_api_key = Some("key".to_string());
        assert_eq!(restream_target(&cfg).as_deref(), Some("UCtarget"));
        cfg.holodex_monitor_gate = false;
        assert_eq!(restream_target(&cfg), None, "rescue mode");
        cfg.holodex_monitor_gate = true;
        cfg.youtube.enable_monitor = false;
        assert_eq!(restream_target(&cfg), None);
    }

    #[test]
    fn a_video_keeps_the_source_that_listed_it_first() {
        let t0 = Instant::now();
        let mut seen = FirstSeen::default();
        seen.note(["abc"], Source::Playlist, t0);
        seen.note(["abc", "def"], Source::Rss, t0 + Duration::from_secs(30));
        assert_eq!(
            seen.before("abc", t0 + Duration::from_secs(40)),
            Some((Source::Playlist, Duration::from_secs(40)))
        );
        assert_eq!(
            seen.before("def", t0 + Duration::from_secs(40)),
            Some((Source::Rss, Duration::from_secs(10)))
        );
        assert_eq!(seen.before("ghi", t0), None);
        seen.note(std::iter::empty(), Source::Rss, t0 + FIRST_SEEN_KEEP);
        assert_eq!(seen.before("abc", t0 + FIRST_SEEN_KEEP), None, "forgotten");
        assert!(seen.before("def", t0 + FIRST_SEEN_KEEP).is_some());
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

    #[tokio::test]
    async fn disabled_rss_skips_requests_and_reenable_keeps_other_discovery_state() {
        let mut worker = Worker::default();
        worker
            .playlist_due
            .insert("UCtarget".into(), Instant::now());
        worker.next_rss = Some(Instant::now() + RSS_TICK);
        let mut fresh = HashMap::new();
        // Invalid proxy would fail before any feed request if the gate vanished.
        worker
            .read_feeds(&roster(2), Some("://invalid"), &mut fresh)
            .await
            .unwrap();
        assert!(fresh.is_empty());
        worker.configure_rss(true);
        assert!(worker.next_rss.is_none());
        assert!(worker.playlist_due.contains_key("UCtarget"));
        assert!(!worker.rss.plan(&roster(2), Instant::now()).is_empty());
        worker.configure_rss(false);
        assert!(polling_on(
            playlist_interval(50, 1, false),
            !worker.rss_enabled
        ));
    }

    #[test]
    fn target_reserve_does_not_randomly_choose_priority() {
        let mut cfg = crate::cluster::tests::test_config("local", 0);
        cfg.youtube_api_key = Some("key".into());
        cfg.youtube.enable_monitor = true;
        cfg.youtube.channel_id = "UCtarget".into();
        cfg.priority_channel.enabled = true;
        cfg.priority_channel.auto_restart = true;
        cfg.priority_channel.youtube_channel_id = "UCpriority".into();
        assert_eq!(restream_target(&cfg).as_deref(), Some("UCtarget"));
        cfg.youtube.enable_monitor = false;
        assert_eq!(restream_target(&cfg).as_deref(), Some("UCpriority"));
    }

    #[tokio::test]
    async fn a_cluster_peer_leaves_discovery_to_the_owner() {
        let peer = crate::cluster::YtIndexRole::Peer(Default::default());
        let mut worker = Worker {
            roster: roster(3),
            ..Worker::default()
        };
        let result = crate::cluster::with_yt_index_role(peer, worker.tick()).await;
        assert_eq!(result, Ok(()));
        assert_eq!(worker.roster, roster(3), "no config read, no feed planned");
    }
}
