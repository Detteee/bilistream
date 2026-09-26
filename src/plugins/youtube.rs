use super::danmaku::get_channel_name;
use super::holodex::get_holodex_live_title;
pub use super::holodex::{
    get_holodex_favorites_live, get_holodex_streams, holodex_jwt_is_expired, holodex_unix_now,
    refresh_holodex_jwt, sync_holodex_jwt_if_needed, HolodexStream,
};
use super::utils::{
    add_yt_dlp_cookies_args, command_output_with_timeout, configure_no_window, executable_command,
};
use crate::config::load_config;
use chrono::{DateTime, Local};
use regex::Regex;
use std::collections::{HashMap, HashSet};
use std::error::Error;
use std::process::Command;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

// Helper function to get yt-dlp command path
fn get_yt_dlp_command() -> String {
    executable_command("yt-dlp.exe", "yt-dlp")
}

const YT_DLP_TIMEOUT: Duration = Duration::from_secs(45);

fn add_youtube_extractor_args(command: &mut Command) {
    command
        .arg("--extractor-args")
        .arg("youtube:formats=duplicate;player-client=default,web_embedded");
}

fn scheduled_title_suffix_regex() -> Option<&'static Regex> {
    static SCHEDULED_TITLE_SUFFIX_RE: OnceLock<Option<Regex>> = OnceLock::new();
    SCHEDULED_TITLE_SUFFIX_RE
        .get_or_init(|| Regex::new(r"\s+\d{4}-\d{2}-\d{2}\s+\d{2}:\d{2}$").ok())
        .as_ref()
}

fn strip_scheduled_title_suffix(line: &str) -> String {
    let trimmed = line.trim();
    match scheduled_title_suffix_regex() {
        Some(regex) => regex.replace(trimmed, "").trim().to_string(),
        None => trimmed.to_string(),
    }
}

fn live_event_minutes_regex() -> Option<&'static Regex> {
    static LIVE_EVENT_MINUTES_RE: OnceLock<Option<Regex>> = OnceLock::new();
    LIVE_EVENT_MINUTES_RE
        .get_or_init(|| Regex::new(r"This live event will begin in (\d+) minutes").ok())
        .as_ref()
}

fn live_event_hours_regex() -> Option<&'static Regex> {
    static LIVE_EVENT_HOURS_RE: OnceLock<Option<Regex>> = OnceLock::new();
    LIVE_EVENT_HOURS_RE
        .get_or_init(|| Regex::new(r"This live event will begin in (\d+) hours").ok())
        .as_ref()
}

fn live_event_days_regex() -> Option<&'static Regex> {
    static LIVE_EVENT_DAYS_RE: OnceLock<Option<Regex>> = OnceLock::new();
    LIVE_EVENT_DAYS_RE
        .get_or_init(|| Regex::new(r"This live event will begin in (\d+) days").ok())
        .as_ref()
}

fn capture_first_i64(regex: Option<&Regex>, text: &str) -> Option<i64> {
    regex?.captures(text)?.get(1)?.as_str().parse::<i64>().ok()
}

fn m3u8_url_regex() -> Option<&'static Regex> {
    static M3U8_URL_RE: OnceLock<Option<Regex>> = OnceLock::new();
    M3U8_URL_RE
        .get_or_init(|| Regex::new(r"https://[^\s]+\.m3u8[^\s]*").ok())
        .as_ref()
}

fn yt_dlp_video_id_from_stdout(stdout: &str) -> Option<String> {
    let mut lines = stdout.lines().filter_map(|line| {
        let trimmed = line.trim();
        (!trimmed.is_empty()).then_some(trimmed)
    });
    let first = lines.next()?;
    lines.next().map(|_| first.to_string())
}

fn first_m3u8_url_from_stdout(stdout: &str) -> Option<(String, bool)> {
    let mut matches = m3u8_url_regex()?.find_iter(stdout);
    let first = matches.next()?.as_str().to_string();
    let has_multiple = matches.next().is_some();
    Some((first, has_multiple))
}

fn optional_channel_name_for_holodex<E: std::fmt::Display>(
    lookup: Result<Option<String>, E>,
    channel_id: &str,
) -> Option<String> {
    match lookup {
        Ok(channel_name) => channel_name,
        Err(e) => {
            tracing::debug!(
                "Unable to resolve YouTube channel name for {}: {}",
                channel_id,
                e
            );
            None
        }
    }
}

pub struct Youtube {
    pub channel_name: String,
    pub channel_id: String,
    pub proxy: Option<String>,
}
impl Youtube {
    pub fn new(channel_name: &str, channel_id: &str, proxy: Option<String>) -> Self {
        Youtube {
            channel_name: channel_name.to_string(),
            channel_id: channel_id.to_string(),
            proxy,
        }
    }

    pub async fn get_status(
        &self,
    ) -> Result<
        (
            bool,                    // is_live
            Option<String>,          // topic
            Option<String>,          // title
            Option<String>,          // m3u8_url
            Option<DateTime<Local>>, // start_time
            Option<String>,          // video_id
        ),
        Box<dyn Error>,
    > {
        get_youtube_status_with(&self.channel_id, Probe::Throttled).await
    }
}

/// A scheduled stream stops counting as "next up" once it is this far ahead.
const UPCOMING_HORIZON_HOURS: i64 = 30;

/// The stream a channel is currently on: the live one if there is any,
/// otherwise the soonest stream scheduled within the next 30 hours.
///
/// The monitor loop and the WebUI status refresh both resolve a channel through
/// this, so they cannot disagree about which stream a channel is on.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct YoutubeChannelStatus {
    pub is_live: bool,
    pub topic: Option<String>,
    pub title: Option<String>,
    pub scheduled_start: Option<DateTime<Local>>,
    pub video_id: Option<String>,
}

fn select_holodex_channel_status_at(
    channel_id: &str,
    streams: &[HolodexStream],
    now: DateTime<chrono::Utc>,
) -> YoutubeChannelStatus {
    let channel_streams: Vec<&HolodexStream> = streams
        .iter()
        .filter(|s| s.channel.id == channel_id)
        .collect();

    if let Some(live) = channel_streams.iter().find(|s| s.status == "live") {
        return YoutubeChannelStatus {
            is_live: true,
            topic: live.topic_id.clone(),
            title: Some(live.title.clone()),
            scheduled_start: None,
            video_id: Some(live.id.clone()),
        };
    }

    let horizon = now + chrono::Duration::hours(UPCOMING_HORIZON_HOURS);
    let mut upcoming: Vec<&HolodexStream> = channel_streams
        .iter()
        .copied()
        .filter(|s| {
            if s.status != "upcoming" {
                return false;
            }
            match s
                .start_scheduled
                .as_deref()
                .and_then(|t| DateTime::parse_from_rfc3339(t).ok())
            {
                Some(scheduled) => scheduled.with_timezone(&chrono::Utc) <= horizon,
                // Keep streams whose schedule is missing or unparseable.
                None => true,
            }
        })
        .collect();

    // RFC3339 timestamps from Holodex are UTC-normalised, so ordering the
    // strings orders the schedule.
    upcoming.sort_by(|a, b| match (&a.start_scheduled, &b.start_scheduled) {
        (Some(time_a), Some(time_b)) => time_a.cmp(time_b),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => std::cmp::Ordering::Equal,
    });

    let Some(next) = upcoming.first() else {
        return YoutubeChannelStatus::default();
    };

    YoutubeChannelStatus {
        is_live: false,
        topic: next.topic_id.clone(),
        title: Some(next.title.clone()),
        scheduled_start: next.start_scheduled.as_deref().and_then(|t| {
            DateTime::parse_from_rfc3339(t)
                .ok()
                .map(|dt| dt.with_timezone(&Local))
        }),
        video_id: Some(next.id.clone()),
    }
}

pub fn select_holodex_channel_status(
    channel_id: &str,
    streams: &[HolodexStream],
) -> YoutubeChannelStatus {
    select_holodex_channel_status_at(channel_id, streams, chrono::Utc::now())
}

/// How the monitor decides whether a YouTube channel is live.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MonitorMode {
    /// yt-dlp 兜底: yt-dlp every tick, whatever the other sources say.
    Rescue,
    /// Holodex rows plus discovered rows, corrected by `videos.list`. yt-dlp
    /// confirms live streams and runs as a throttled safety probe.
    Index,
}

/// The 基础设置 switch is stored as `holodex_monitor_gate`, where `false`
/// means rescue. Without any Holodex or YouTube key there is no index to
/// consult, so that also means rescue.
pub fn monitor_mode(cfg: &crate::config::Config) -> MonitorMode {
    let has_key = |key: &Option<String>| key.as_deref().is_some_and(|key| !key.trim().is_empty());
    monitor_mode_for(
        cfg.holodex_monitor_gate,
        has_key(&cfg.holodex_api_key),
        !cfg.youtube_api_keys().is_empty(),
    )
}

fn monitor_mode_for(gate: bool, has_holodex_key: bool, has_youtube_key: bool) -> MonitorMode {
    if gate && (has_holodex_key || has_youtube_key) {
        MonitorMode::Index
    } else {
        MonitorMode::Rescue
    }
}

/// Whether a caller may skip yt-dlp between safety probes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Probe {
    /// One-off checks (danmaku 转播, switch preflight): with no index answer,
    /// ask yt-dlp now.
    Now,
    /// The monitor loop: when the index says not live, run yt-dlp at most
    /// once per `SAFETY_PROBE_INTERVAL`.
    Throttled,
    /// The priority-channel probe during another restream: yt-dlp every
    /// time, whatever the index says. An unscheduled go-live has no video ID
    /// until it starts, so the index cannot be trusted to know it.
    Always,
}

const SAFETY_PROBE_INTERVAL: Duration = Duration::from_secs(300);
/// One-off checks trust an "upcoming" answer without yt-dlp, so they re-fetch
/// any answer older than this.
const ONE_OFF_MAX_AGE: Duration = Duration::from_secs(15);

/// How old a `videos.list` answer a caller accepts. The monitor loop takes the
/// store's tiers; 检测间隔 paces only its Holodex check and yt-dlp on live.
fn overlay_max_age(probe: Probe) -> Option<Duration> {
    match probe {
        Probe::Now => Some(ONE_OFF_MAX_AGE),
        Probe::Throttled | Probe::Always => None,
    }
}

/// Channels whose monitor acts on a go-live: late steps in the store's
/// re-checks, and a wake when YouTube answers live.
pub(crate) fn monitored_channels(cfg: &crate::config::Config) -> HashSet<String> {
    let mut channels = HashSet::new();
    if cfg.youtube.enable_monitor && !cfg.youtube.channel_id.is_empty() {
        channels.insert(cfg.youtube.channel_id.clone());
    }
    // The priority probe waits on its own wake while another channel restreams.
    let priority = &cfg.priority_channel;
    if priority.enabled && priority.auto_restart && !priority.youtube_channel_id.is_empty() {
        channels.insert(priority.youtube_channel_id.clone());
    }
    channels
}

/// Channels whose go-live YouTube answered and no monitor pass picked up yet.
static MONITOR_WAKES: Mutex<Option<HashSet<String>>> = Mutex::new(None);

/// Ends the channel's monitor wait early: YouTube answered live.
pub(crate) fn wake_monitor(channel_id: &str) {
    let mut guard = MONITOR_WAKES.lock().unwrap_or_else(|e| e.into_inner());
    guard
        .get_or_insert_with(HashSet::new)
        .insert(channel_id.to_string());
}

/// Whether the channel has a wake waiting; taking it clears it.
pub fn take_monitor_wake(channel_id: &str) -> bool {
    let mut guard = MONITOR_WAKES.lock().unwrap_or_else(|e| e.into_inner());
    guard.as_mut().is_some_and(|wakes| wakes.remove(channel_id))
}

/// Live video IDs the monitor is skipping (warning/cut-off or banned keyword).
/// Later ticks reuse the index answer instead of running yt-dlp for the m3u8.
static SKIPPED_LIVE: Mutex<Option<HashMap<String, String>>> = Mutex::new(None);

fn with_skipped_live<T>(f: impl FnOnce(&mut HashMap<String, String>) -> T) -> T {
    let mut guard = SKIPPED_LIVE.lock().unwrap_or_else(|e| e.into_inner());
    f(guard.get_or_insert_with(HashMap::new))
}

/// Remember that this channel's live video is being skipped.
pub fn mark_skipped_live(channel_id: &str, video_id: &str) {
    if channel_id.is_empty() || video_id.is_empty() {
        return;
    }
    with_skipped_live(|skipped| {
        skipped.insert(channel_id.to_string(), video_id.to_string());
    });
}

/// Drop the skip mark for this channel. `true` if it had one.
pub fn clear_skipped_live(channel_id: &str) -> bool {
    with_skipped_live(|skipped| skipped.remove(channel_id).is_some())
}

fn skipped_video(channel_id: &str) -> Option<String> {
    with_skipped_live(|skipped| skipped.get(channel_id).cloned())
}

/// What the index says about one channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IndexView {
    /// Rescue mode, or the index could not be read.
    Unavailable,
    Live,
    /// Not live, and Holodex itself sent a row for the channel.
    Upcoming,
    /// Nothing but discovered waiting rooms, or nothing at all.
    NoAnswer,
}

/// Reuse the index's live answer instead of running yt-dlp for the same video.
fn skip_reuse(
    view: IndexView,
    probe: Probe,
    marked_video_id: Option<&str>,
    video_id: Option<&str>,
) -> bool {
    probe == Probe::Throttled
        && view == IndexView::Live
        && marked_video_id.is_some()
        && marked_video_id == video_id
}

fn drop_skipped_live_if_stale(channel_id: &str, view: IndexView, video_id: Option<&str>) {
    let marked = skipped_video(channel_id);
    if marked.is_some() && (view != IndexView::Live || marked.as_deref() != video_id) {
        clear_skipped_live(channel_id);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MonitorAction {
    YtDlp,
    ConfirmLive,
    IndexAnswer,
}

fn decide_monitor_action(
    view: IndexView,
    probe: Probe,
    since_last_probe: Option<Duration>,
) -> MonitorAction {
    match view {
        IndexView::Unavailable => MonitorAction::YtDlp,
        IndexView::Live => MonitorAction::ConfirmLive,
        _ if probe == Probe::Always => MonitorAction::YtDlp,
        IndexView::Upcoming | IndexView::NoAnswer if probe == Probe::Throttled => {
            match since_last_probe {
                Some(age) if age < SAFETY_PROBE_INTERVAL => MonitorAction::IndexAnswer,
                _ => MonitorAction::YtDlp,
            }
        }
        IndexView::Upcoming => MonitorAction::IndexAnswer,
        IndexView::NoAnswer => MonitorAction::YtDlp,
    }
}

type YoutubeStatus = (
    bool,                    // is_live
    Option<String>,          // topic
    Option<String>,          // title
    Option<String>,          // m3u8_url
    Option<DateTime<Local>>, // start_time
    Option<String>,          // video_id
);

/// Last safety probe per channel, with its not-live answer so ticks between
/// probes keep yt-dlp's title and schedule. A probe that finds live removes
/// the entry, so the next tick probes again instead of reusing a stale URL.
type SafetyProbes = HashMap<String, (Instant, Option<YoutubeStatus>)>;
static SAFETY_PROBES: Mutex<Option<SafetyProbes>> = Mutex::new(None);

/// Probes for different channels (the target and the priority channel) stay
/// at least this far apart, so yt-dlp never hits YouTube for both at once.
const PROBE_GAP: Duration = Duration::from_secs(120);

/// A turn nobody claimed within this long lapses, so a channel that stopped
/// probing (priority switched off) cannot hold the other back.
const TURN_EXPIRES: Duration = Duration::from_secs(300);

/// Probe spacing across channels. A channel held back by the gap gets the
/// next turn, so a channel probing every tick (rescue mode) cannot starve the
/// other.
#[derive(Default)]
struct ProbeTurns {
    last: Option<(Instant, String)>,
    waiting: Option<(Instant, String)>,
}

impl ProbeTurns {
    /// Whether `channel_id` may run yt-dlp now; records the probe when it may.
    fn claim(&mut self, channel_id: &str, now: Instant) -> bool {
        if self
            .waiting
            .as_ref()
            .is_some_and(|(since, _)| now.saturating_duration_since(*since) >= TURN_EXPIRES)
        {
            self.waiting = None;
        }
        let recent_other = self.last.as_ref().is_some_and(|(at, id)| {
            id != channel_id && now.saturating_duration_since(*at) < PROBE_GAP
        });
        let other_waiting = self
            .waiting
            .as_ref()
            .is_some_and(|(_, id)| id != channel_id);
        if recent_other || other_waiting {
            if self.waiting.is_none() {
                self.waiting = Some((now, channel_id.to_string()));
            }
            return false;
        }
        if self
            .waiting
            .as_ref()
            .is_some_and(|(_, id)| id == channel_id)
        {
            self.waiting = None;
        }
        self.last = Some((now, channel_id.to_string()));
        true
    }

    /// A probe that runs regardless (the priority probe during a restream)
    /// still counts against the other channel's gap.
    fn note(&mut self, channel_id: &str, now: Instant) {
        self.last = Some((now, channel_id.to_string()));
    }
}

static PROBE_TURNS: Mutex<Option<ProbeTurns>> = Mutex::new(None);

fn with_probe_turns<T>(f: impl FnOnce(&mut ProbeTurns) -> T) -> T {
    let mut guard = PROBE_TURNS.lock().unwrap_or_else(|e| e.into_inner());
    f(guard.get_or_insert_with(ProbeTurns::default))
}

fn last_safety_probe(channel_id: &str) -> Option<(Duration, Option<YoutubeStatus>)> {
    let guard = SAFETY_PROBES.lock().unwrap_or_else(|e| e.into_inner());
    let (at, status) = guard.as_ref()?.get(channel_id)?;
    Some((at.elapsed(), status.clone()))
}

fn record_safety_probe(channel_id: &str, status: Option<YoutubeStatus>) {
    let mut guard = SAFETY_PROBES.lock().unwrap_or_else(|e| e.into_inner());
    let probes = guard.get_or_insert_with(HashMap::new);
    if status.as_ref().is_some_and(|status| status.0) {
        probes.remove(channel_id);
    } else {
        probes.insert(channel_id.to_string(), (Instant::now(), status));
    }
}

/// Holodex rows (when a Holodex key is set) plus this channel's discovered
/// rows, corrected by YouTube, and the IDs Holodex itself sent.
async fn index_rows(
    cfg: &crate::config::Config,
    channel_id: &str,
    max_age: Option<Duration>,
) -> Result<(Vec<HolodexStream>, HashSet<String>), String> {
    let has_holodex_key = cfg
        .holodex_api_key
        .as_deref()
        .is_some_and(|key| !key.trim().is_empty());
    let holodex = if has_holodex_key {
        get_holodex_streams(vec![channel_id.to_string()], false)
            .await
            .map_err(|e| e.to_string())?
    } else {
        Vec::new()
    };
    let holodex_ids = holodex.iter().map(|s| s.id.clone()).collect();
    let streams = with_discovered_rows(channel_id, holodex);
    let streams = super::youtube_data::apply_youtube_overlay_within(streams, max_age).await;
    Ok((streams, holodex_ids))
}

fn youtube_channel_status_from_probe(
    is_live: bool,
    topic: Option<String>,
    title: Option<String>,
    scheduled_start: Option<DateTime<Local>>,
    video_id: Option<String>,
) -> YoutubeChannelStatus {
    YoutubeChannelStatus {
        is_live,
        topic,
        title,
        scheduled_start,
        video_id,
    }
}

/// Channel status without a playable URL.
///
/// In index mode this is an index read, cheap enough for the WebUI poller.
/// In rescue mode it takes the same yt-dlp path as the monitor loop so the
/// dashboard cannot disagree about live/upcoming.
pub async fn get_youtube_channel_status(
    channel_id: &str,
) -> Result<YoutubeChannelStatus, Box<dyn Error>> {
    let cfg = load_config().await?;
    if monitor_mode(&cfg) == MonitorMode::Rescue {
        let (is_live, topic, title, _, scheduled_start, video_id) =
            get_youtube_status(channel_id).await?;
        return Ok(youtube_channel_status_from_probe(
            is_live,
            topic,
            title,
            scheduled_start,
            video_id,
        ));
    }

    let (streams, _) = index_rows(&cfg, channel_id, None).await?;
    Ok(select_holodex_channel_status(channel_id, &streams))
}

/// Holodex rows plus this channel's streams found by RSS discovery.
fn with_discovered_rows(channel_id: &str, streams: Vec<HolodexStream>) -> Vec<HolodexStream> {
    super::youtube_rss::merge_discovered(streams)
        .into_iter()
        .filter(|s| s.channel.id == channel_id)
        .collect()
}

/// Channel status without a playable URL, falling back to yt-dlp in rescue
/// mode.
///
/// In index mode this stays an index read. In rescue mode it uses
/// `get_youtube_status`, which resolves a stream URL as a side effect of
/// answering.
pub async fn get_youtube_channel_metadata(
    channel_id: &str,
) -> Result<YoutubeChannelStatus, Box<dyn Error>> {
    let cfg = load_config().await?;
    if monitor_mode(&cfg) == MonitorMode::Index {
        return get_youtube_channel_status(channel_id).await;
    }

    let (is_live, topic, title, _, scheduled_start, video_id) =
        get_youtube_status(channel_id).await?;
    Ok(youtube_channel_status_from_probe(
        is_live,
        topic,
        title,
        scheduled_start,
        video_id,
    ))
}

/// yt-dlp does not supply Holodex's game/topic. Look it up separately for area
/// correction without making Holodex a live-status gate when that is disabled.
pub(crate) async fn get_youtube_area_topic(
    channel_id: &str,
    video_id: Option<&str>,
) -> Option<String> {
    let video_id = video_id.filter(|id| !id.is_empty())?;
    let cfg = load_config().await.ok()?;
    cfg.holodex_api_key
        .as_deref()
        .filter(|key| !key.is_empty())?;

    let streams = match tokio::time::timeout(
        Duration::from_secs(5),
        get_holodex_streams(vec![channel_id.to_string()], false),
    )
    .await
    {
        Ok(Ok(streams)) => streams,
        Ok(Err(error)) => {
            tracing::debug!("弹幕分区补充游戏信息失败: {}", error);
            return None;
        }
        Err(_) => {
            tracing::debug!("弹幕分区补充游戏信息超时，使用已获取的标题");
            return None;
        }
    };
    youtube_area_topic_for_video(channel_id, video_id, &streams)
}

fn youtube_area_topic_for_video(
    channel_id: &str,
    video_id: &str,
    streams: &[HolodexStream],
) -> Option<String> {
    streams
        .iter()
        .find(|stream| {
            stream.id == video_id
                && stream.channel.id == channel_id
                && stream.stream_type != "placeholder"
                && matches!(stream.status.as_str(), "live" | "upcoming")
        })?
        .topic_id
        .clone()
        .filter(|topic| !topic.trim().is_empty())
}

pub async fn get_youtube_status(channel_id: &str) -> Result<YoutubeStatus, Box<dyn Error>> {
    get_youtube_status_with(channel_id, Probe::Now).await
}

/// `get_youtube_status` that always asks yt-dlp (see `Probe::Always`).
pub async fn get_youtube_status_ytdlp(channel_id: &str) -> Result<YoutubeStatus, Box<dyn Error>> {
    get_youtube_status_with(channel_id, Probe::Always).await
}

async fn get_youtube_status_with(
    channel_id: &str,
    probe: Probe,
) -> Result<YoutubeStatus, Box<dyn Error>> {
    let cfg = load_config().await?;
    let proxy = cfg.youtube.proxy.clone();
    let quality = cfg.youtube.quality.clone();
    let cookies_file = &cfg.youtube.cookies_file;
    let cookies_from_browser = &cfg.youtube.cookies_from_browser;
    let deno_path = &cfg.youtube.deno_path;

    // The error is reduced to a String inside `index_rows`: Box<dyn Error> is
    // not Send, and held across the awaits below it would make this whole
    // future unspawnable.
    let (view, status) = match monitor_mode(&cfg) {
        MonitorMode::Rescue => {
            tracing::debug!("yt-dlp 兜底模式，直接查询 {}", channel_id);
            (IndexView::Unavailable, YoutubeChannelStatus::default())
        }
        MonitorMode::Index => match index_rows(&cfg, channel_id, overlay_max_age(probe)).await {
            Ok((streams, holodex_ids)) => {
                let status = select_holodex_channel_status(channel_id, &streams);
                // Holodex omits some streams entirely, so only its own row for
                // this channel (or a stream YouTube calls live) answers. A
                // discovered waiting room alone must not hide a stream that
                // went live on another video.
                let answered = streams
                    .iter()
                    .any(|s| s.channel.id == channel_id && holodex_ids.contains(&s.id));
                let view = if status.is_live {
                    IndexView::Live
                } else if answered {
                    IndexView::Upcoming
                } else {
                    IndexView::NoAnswer
                };
                (view, status)
            }
            Err(e) => {
                tracing::error!("Holodex API failed: {}, using yt-dlp", e);
                (IndexView::Unavailable, YoutubeChannelStatus::default())
            }
        },
    };

    let last_probe = match probe {
        Probe::Throttled => last_safety_probe(channel_id),
        Probe::Now | Probe::Always => None,
    };
    let index_answer = |status: YoutubeChannelStatus| {
        (
            false,
            status.topic,
            status.title,
            None,
            status.scheduled_start,
            status.video_id,
        )
    };

    if skip_reuse(
        view,
        probe,
        skipped_video(channel_id).as_deref(),
        status.video_id.as_deref(),
    ) {
        return Ok((
            true,
            status.topic,
            status.title,
            None,
            None,
            status.video_id,
        ));
    }
    drop_skipped_live_if_stale(channel_id, view, status.video_id.as_deref());

    let mut action = decide_monitor_action(view, probe, last_probe.as_ref().map(|(age, _)| *age));
    if action == MonitorAction::YtDlp {
        let now = Instant::now();
        if probe == Probe::Throttled {
            if !with_probe_turns(|turns| turns.claim(channel_id, now)) {
                // Another channel was probed within 2 min: answer from the
                // index or the last probe, and probe on a later tick.
                tracing::debug!("yt-dlp 探测 {} 延后: 另一频道 2 分钟内刚探测过", channel_id);
                action = MonitorAction::IndexAnswer;
            }
        } else {
            with_probe_turns(|turns| turns.note(channel_id, now));
        }
    }
    match action {
        MonitorAction::ConfirmLive => {
            // The index knows the stream; yt-dlp resolves the playable URL and
            // has the final say on whether it is actually live.
            let (is_live, _, _, m3u8_url, _, _) = get_status_with_yt_dlp(
                channel_id,
                proxy,
                status.title.clone(),
                Some(&quality),
                cookies_file,
                cookies_from_browser,
                deno_path,
            )
            .await?;
            Ok((
                is_live,
                status.topic,
                status.title,
                m3u8_url,
                None,
                status.video_id,
            ))
        }
        MonitorAction::IndexAnswer => match view {
            IndexView::Upcoming => Ok(index_answer(status)),
            // Between safety probes with nothing listed, repeat the last
            // probe's answer.
            _ => Ok(last_probe
                .and_then(|(_, status)| status)
                .unwrap_or((false, None, None, None, None, None))),
        },
        MonitorAction::YtDlp => {
            if view != IndexView::Unavailable {
                tracing::debug!("yt-dlp 安全探测 {} ({:?})", channel_id, view);
            }
            // Recorded before running, so a failing yt-dlp is not retried
            // every tick.
            if probe == Probe::Throttled {
                record_safety_probe(channel_id, None);
            }
            let title = get_youtube_live_title(channel_id).await?;
            // Passing the title in keeps yt-dlp from looking it up again.
            let probed = get_status_with_yt_dlp(
                channel_id,
                proxy,
                title,
                Some(&quality),
                cookies_file,
                cookies_from_browser,
                deno_path,
            )
            .await?;
            let (is_live, _, title, m3u8_url, start_time, video_id) = probed;
            let result = if !is_live && view == IndexView::Upcoming {
                // YouTube's schedule for the listed stream beats yt-dlp's
                // rounded "begins in N hours".
                index_answer(status)
            } else {
                (is_live, None, title, m3u8_url, start_time, video_id)
            };
            if probe == Probe::Throttled {
                record_safety_probe(channel_id, Some(result.clone()));
            }
            Ok(result)
        }
    }
}

// Update get_status_with_yt_dlp to match the new order
async fn get_status_with_yt_dlp(
    channel_id: &str,
    proxy: Option<String>,
    title: Option<String>,
    quality: Option<&str>,
    cookies_file: &Option<String>,
    cookies_from_browser: &Option<String>,
    deno_path: &Option<String>,
) -> Result<
    (
        bool,                    // is_live
        Option<String>,          // topic
        Option<String>,          // title
        Option<String>,          // m3u8_url
        Option<DateTime<Local>>, // start_time
        Option<String>,          // video_id
    ),
    Box<dyn Error>,
> {
    let quality = quality.unwrap_or("best");

    let mut command = Command::new(get_yt_dlp_command());
    configure_no_window(&mut command);

    // Add deno runtime if path is configured
    if let Some(deno) = deno_path {
        if !deno.is_empty() {
            command.arg("--js-runtimes");
            command.arg(format!("deno:{}", deno));
        }
    }

    if let Some(proxy) = proxy.clone() {
        command.arg("--proxy");
        command.arg(proxy);
    }

    // Add cookies arguments
    add_yt_dlp_cookies_args(&mut command, cookies_file, cookies_from_browser);
    add_youtube_extractor_args(&mut command);

    command.arg("-f");
    command.arg(quality);
    command.arg("--print").arg("id");
    command.arg("-g");

    command.arg(format!(
        "https://www.youtube.com/channel/{}/live",
        channel_id
    ));
    let output = command_output_with_timeout(command, YT_DLP_TIMEOUT, "yt-dlp").await?;
    // println!("{:?}", output);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    // Extract video ID from stdout (first line when using --print id)
    let video_id = yt_dlp_video_id_from_stdout(&stdout);

    if stderr.contains("ERROR: [youtube") {
        // Check for scheduled start time in stderr
        if let Some(minutes) = capture_first_i64(live_event_minutes_regex(), &stderr) {
            let start_time = chrono::Local::now() + chrono::Duration::minutes(minutes);
            return Ok((false, None, title, None, Some(start_time), video_id));
        }
        if let Some(hours) = capture_first_i64(live_event_hours_regex(), &stderr) {
            let start_time = chrono::Local::now() + chrono::Duration::hours(hours);
            let title = if title.is_some() {
                title
            } else {
                get_youtube_live_title(channel_id).await?
            };
            return Ok((false, None, title, None, Some(start_time), video_id)); // Return scheduled start time
        }
        if let Some(days) = capture_first_i64(live_event_days_regex(), &stderr) {
            let start_time = chrono::Local::now() + chrono::Duration::days(days);
            let title = if title.is_some() {
                title
            } else {
                get_youtube_live_title(channel_id).await?
            };
            return Ok((false, None, title, None, Some(start_time), video_id)); // Return scheduled start time
        }
        return Ok((false, None, None, None, None, video_id)); // Channel is not live and no scheduled time
    } else if let Some((m3u8_url, has_multiple)) = first_m3u8_url_from_stdout(&stdout) {
        if has_multiple {
            tracing::warn!("Multiple m3u8 URLs found (likely separate video and audio streams)");
            tracing::warn!("Using first URL: {}", m3u8_url);
        }
        return Ok((true, None, title, Some(m3u8_url), None, video_id));
    }

    Err("Unexpected output from yt-dlp".into())
}

pub async fn get_youtube_live_title(channel_id: &str) -> Result<Option<String>, Box<dyn Error>> {
    let cfg = load_config().await?;
    let proxy = cfg.youtube.proxy.clone();
    let cookies_file = &cfg.youtube.cookies_file;
    let cookies_from_browser = &cfg.youtube.cookies_from_browser;
    let channel_name =
        optional_channel_name_for_holodex(get_channel_name("YT", channel_id), channel_id);

    // Helper function to get title using yt-dlp
    let get_title_with_ytdlp = || async {
        let mut command = Command::new(get_yt_dlp_command());
        configure_no_window(&mut command);
        if let Some(ref p) = proxy {
            command.arg("--proxy").arg(p);
        }
        add_yt_dlp_cookies_args(&mut command, cookies_file, cookies_from_browser);
        add_youtube_extractor_args(&mut command);
        command.arg("-e").arg(format!(
            "https://www.youtube.com/channel/{}/live",
            channel_id
        ));

        let output = command_output_with_timeout(command, YT_DLP_TIMEOUT, "yt-dlp").await?;
        let title_str = String::from_utf8_lossy(&output.stdout);

        let title = title_str
            .lines()
            .rfind(|line| {
                !line.trim().is_empty()
                    && !line.starts_with("WARNING")
                    && !line.starts_with("ERROR")
            })
            .map(strip_scheduled_title_suffix)
            .filter(|s| !s.is_empty());

        Ok::<_, Box<dyn Error>>(title)
    };

    // In index mode try Holodex first. Holodex omits some streams, so a
    // missing title also falls back to yt-dlp.
    if monitor_mode(&cfg) == MonitorMode::Index {
        if let Some(key) = cfg.holodex_api_key.clone().filter(|k| !k.is_empty()) {
            match get_holodex_live_title(&key, channel_id, channel_name.as_deref()).await {
                Ok(Some(title)) => return Ok(Some(title)),
                Ok(None) => {}
                Err(_) => {
                    tracing::warn!("Holodex API failed, falling back to yt-dlp");
                }
            }
        }
    }

    // Fallback to yt-dlp
    get_title_with_ytdlp().await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugins::holodex::HolodexChannel;

    #[test]
    fn index_mode_needs_the_switch_off_and_some_key() {
        use MonitorMode::{Index, Rescue};
        assert_eq!(monitor_mode_for(false, true, true), Rescue);
        assert_eq!(monitor_mode_for(true, false, false), Rescue);
        assert_eq!(monitor_mode_for(true, true, false), Index);
        assert_eq!(monitor_mode_for(true, false, true), Index);
    }

    #[test]
    fn probes_of_two_channels_stay_two_minutes_apart_and_take_turns() {
        let t0 = Instant::now();
        let at = |secs: u64| t0 + Duration::from_secs(secs);
        let mut turns = ProbeTurns::default();
        assert!(turns.claim("target", at(0)));
        assert!(
            !turns.claim("priority", at(60)),
            "within 2 min of the target"
        );
        assert!(
            !turns.claim("target", at(120)),
            "priority waited, so it goes next"
        );
        assert!(turns.claim("priority", at(120)));
        assert!(!turns.claim("target", at(180)));
        assert!(turns.claim("target", at(240)), "its turn after the gap");

        let mut turns = ProbeTurns::default();
        assert!(turns.claim("target", at(0)));
        assert!(!turns.claim("priority", at(30)));
        // Priority switched off and never came back for its turn.
        assert!(!turns.claim("target", at(200)));
        assert!(turns.claim("target", at(330)), "an unclaimed turn lapses");

        let mut turns = ProbeTurns::default();
        turns.note("priority", at(0));
        assert!(!turns.claim("target", at(100)), "a forced probe counts too");
        assert!(turns.claim("target", at(400)));
    }

    #[test]
    fn monitor_actions_follow_the_decision_table() {
        use IndexView::*;
        use MonitorAction::*;
        let fresh = Some(Duration::from_secs(60));
        let stale = Some(SAFETY_PROBE_INTERVAL);
        let cases = [
            (Unavailable, Probe::Throttled, fresh, YtDlp),
            (Unavailable, Probe::Now, None, YtDlp),
            (Live, Probe::Throttled, fresh, ConfirmLive),
            (Live, Probe::Now, None, ConfirmLive),
            (Upcoming, Probe::Now, None, IndexAnswer),
            (NoAnswer, Probe::Now, None, YtDlp),
            (Upcoming, Probe::Throttled, fresh, IndexAnswer),
            (NoAnswer, Probe::Throttled, fresh, IndexAnswer),
            (Upcoming, Probe::Throttled, stale, YtDlp),
            (NoAnswer, Probe::Throttled, stale, YtDlp),
            (NoAnswer, Probe::Throttled, None, YtDlp),
            (Upcoming, Probe::Always, None, YtDlp),
            (NoAnswer, Probe::Always, None, YtDlp),
            (Live, Probe::Always, None, ConfirmLive),
        ];
        for (view, probe, age, expected) in cases {
            assert_eq!(
                decide_monitor_action(view, probe, age),
                expected,
                "{view:?} {probe:?} {age:?}"
            );
        }
    }

    #[test]
    fn a_dropped_stream_only_reaches_its_own_channels_monitor() {
        let live = |id: &str, channel: &str| HolodexStream {
            id: id.to_string(),
            title: String::new(),
            stream_type: "stream".to_string(),
            topic_id: None,
            published_at: None,
            available_at: None,
            status: "live".to_string(),
            start_scheduled: None,
            start_actual: Some("2026-09-25T10:00:00Z".to_string()),
            live_viewers: None,
            channel: HolodexChannel {
                id: channel.to_string(),
                ..Default::default()
            },
            link: None,
            thumbnail: None,
            placeholder_type: None,
            yt_confirmed: true,
        };
        super::super::youtube_data::remember_live_for_test(live("drop-a", "UCdropA"));
        super::super::youtube_data::remember_live_for_test(live("drop-b", "UCdropB"));

        // Holodex sent nothing for A during the outage: A's stream is back as live.
        let rows = with_discovered_rows("UCdropA", Vec::new());
        let ids: Vec<&str> = rows.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, vec!["drop-a"]);
        assert!(select_holodex_channel_status("UCdropA", &rows).is_live);

        // After a switch to C, neither dropped stream touches C's view.
        assert!(with_discovered_rows("UCdropC", Vec::new()).is_empty());
    }

    #[test]
    fn one_off_checks_cap_answer_age_and_the_monitor_loop_does_not() {
        assert_eq!(overlay_max_age(Probe::Now), Some(Duration::from_secs(15)));
        assert_eq!(overlay_max_age(Probe::Throttled), None);
    }

    #[test]
    fn only_a_monitored_configured_channel_counts() {
        let mut cfg: crate::config::Config = serde_json::from_value(serde_json::json!({
            "auto_cover": false, "enable_anti_collision": false, "interval": 15,
            "bililive": { "enable_danmaku_command": false, "room": 1, "bili_rtmp_url": "", "bili_rtmp_key": "" },
            "youtube": {}, "twitch": {}, "enable_lol_monitor": false, "anti_collision_list": {}
        }))
        .unwrap();
        cfg.youtube.channel_id = "UCtarget".to_string();
        cfg.youtube.enable_monitor = true;
        assert_eq!(
            monitored_channels(&cfg),
            HashSet::from(["UCtarget".to_string()])
        );
        cfg.youtube.enable_monitor = false;
        assert!(monitored_channels(&cfg).is_empty(), "monitor off");
        cfg.youtube.enable_monitor = true;
        cfg.youtube.channel_id.clear();
        assert!(monitored_channels(&cfg).is_empty(), "no channel");
        cfg.priority_channel.youtube_channel_id = "UCpriority".to_string();
        cfg.priority_channel.enabled = true;
        cfg.priority_channel.auto_restart = true;
        assert_eq!(
            monitored_channels(&cfg),
            HashSet::from(["UCpriority".to_string()]),
            "priority switching on"
        );
        cfg.priority_channel.auto_restart = false;
        assert!(
            monitored_channels(&cfg).is_empty(),
            "priority switching off"
        );
    }

    #[test]
    fn a_wake_reaches_only_its_own_channel_once() {
        wake_monitor("UCwakeA");
        assert!(!take_monitor_wake("UCwakeB"));
        assert!(take_monitor_wake("UCwakeA"));
        assert!(!take_monitor_wake("UCwakeA"), "taking it clears it");
    }

    #[test]
    fn a_skipped_live_id_is_reused_until_it_changes() {
        use IndexView::*;
        let cases = [
            (Live, Probe::Throttled, Some("v1"), Some("v1"), true),
            (Live, Probe::Throttled, Some("v1"), Some("v2"), false),
            (Live, Probe::Throttled, None, Some("v1"), false),
            (Live, Probe::Throttled, None, None, false),
            (Live, Probe::Throttled, Some("v1"), None, false),
            (Live, Probe::Now, Some("v1"), Some("v1"), false),
            (Upcoming, Probe::Throttled, Some("v1"), Some("v1"), false),
            (NoAnswer, Probe::Throttled, Some("v1"), Some("v1"), false),
            (Unavailable, Probe::Throttled, Some("v1"), Some("v1"), false),
        ];
        for (view, probe, marked, video_id, expected) in cases {
            assert_eq!(
                skip_reuse(view, probe, marked, video_id),
                expected,
                "{view:?} {probe:?} {marked:?} {video_id:?}"
            );
        }

        mark_skipped_live("UCskip-stale", "vid-a");
        drop_skipped_live_if_stale("UCskip-stale", IndexView::Live, Some("vid-a"));
        assert_eq!(skipped_video("UCskip-stale").as_deref(), Some("vid-a"));
        drop_skipped_live_if_stale("UCskip-stale", IndexView::Live, Some("vid-b"));
        assert!(skipped_video("UCskip-stale").is_none(), "new id drops it");

        mark_skipped_live("UCskip-ended", "vid-a");
        drop_skipped_live_if_stale("UCskip-ended", IndexView::Upcoming, Some("vid-a"));
        assert!(skipped_video("UCskip-ended").is_none(), "not live drops it");
        assert!(!clear_skipped_live("UCskip-ended"));
    }

    #[test]
    fn a_probe_that_finds_live_is_not_reused() {
        let offline: YoutubeStatus = (false, None, Some("t".to_string()), None, None, None);
        record_safety_probe("UCprobe-offline", Some(offline.clone()));
        let (age, cached) = last_safety_probe("UCprobe-offline").unwrap();
        assert!(age < SAFETY_PROBE_INTERVAL);
        assert_eq!(cached, Some(offline));

        record_safety_probe("UCprobe-live", None);
        let live: YoutubeStatus = (true, None, None, Some("u".to_string()), None, None);
        record_safety_probe("UCprobe-live", Some(live));
        assert!(last_safety_probe("UCprobe-live").is_none());
    }

    #[test]
    fn youtube_status_futures_stay_send() {
        // The WebUI spawns these, so a stray Box<dyn Error> held across an
        // await would break the callers rather than this module.
        fn assert_send<T: Send>(_: T) {}

        assert_send(get_youtube_status("channel-id"));
        assert_send(get_youtube_status_with("channel-id", Probe::Throttled));
        assert_send(get_youtube_channel_status("channel-id"));
    }

    fn holodex_stream(id: &str, status: &str, scheduled: Option<&str>) -> HolodexStream {
        HolodexStream {
            id: id.to_string(),
            title: format!("{} title", id),
            stream_type: "stream".to_string(),
            topic_id: Some(format!("{} topic", id)),
            published_at: None,
            available_at: None,
            status: status.to_string(),
            start_scheduled: scheduled.map(str::to_string),
            start_actual: None,
            live_viewers: None,
            channel: HolodexChannel {
                id: "channel-id".to_string(),
                ..Default::default()
            },
            link: None,
            thumbnail: None,
            placeholder_type: None,
            yt_confirmed: false,
        }
    }

    #[test]
    fn area_topic_uses_only_the_video_resolved_by_youtube() {
        let mut current = holodex_stream("gta-live", "live", None);
        current.topic_id = Some("GTA".into());
        let mut later = holodex_stream("later", "upcoming", None);
        later.topic_id = Some("minecraft".into());
        let mut streams = [later, current];
        assert_eq!(
            youtube_area_topic_for_video("channel-id", "gta-live", &streams).as_deref(),
            Some("GTA")
        );
        assert_eq!(
            youtube_area_topic_for_video("channel-id", "different-video", &streams),
            None
        );
        assert_eq!(
            youtube_area_topic_for_video("another-channel", "gta-live", &streams),
            None
        );
        streams[1].stream_type = "placeholder".into();
        assert_eq!(
            youtube_area_topic_for_video("channel-id", "gta-live", &streams),
            None
        );
        streams[1].stream_type = "stream".into();
        streams[1].status = "past".into();
        assert_eq!(
            youtube_area_topic_for_video("channel-id", "gta-live", &streams),
            None
        );
        assert_eq!(
            youtube_area_topic_for_video("channel-id", "later", &streams).as_deref(),
            Some("minecraft")
        );
    }

    fn at(rfc3339: &str) -> DateTime<chrono::Utc> {
        DateTime::parse_from_rfc3339(rfc3339)
            .unwrap()
            .with_timezone(&chrono::Utc)
    }

    #[test]
    fn channel_status_prefers_the_live_stream_over_anything_scheduled() {
        let streams = vec![
            holodex_stream("upcoming-1", "upcoming", Some("2026-08-01T10:00:00Z")),
            holodex_stream("live-1", "live", None),
        ];

        let status =
            select_holodex_channel_status_at("channel-id", &streams, at("2026-08-01T09:00:00Z"));

        assert!(status.is_live);
        assert_eq!(status.video_id.as_deref(), Some("live-1"));
        assert_eq!(status.scheduled_start, None);
    }

    #[test]
    fn channel_status_picks_the_earliest_upcoming_stream() {
        let streams = vec![
            holodex_stream("later", "upcoming", Some("2026-08-01T20:00:00Z")),
            holodex_stream("sooner", "upcoming", Some("2026-08-01T12:00:00Z")),
        ];

        let status =
            select_holodex_channel_status_at("channel-id", &streams, at("2026-08-01T09:00:00Z"));

        assert!(!status.is_live);
        assert_eq!(status.video_id.as_deref(), Some("sooner"));
        assert_eq!(status.title.as_deref(), Some("sooner title"));
        assert!(status.scheduled_start.is_some());
    }

    /// `%转播%` checks this selected title. A later clean stream on the same
    /// channel must not make the channel requestable while 雑談 is still next.
    #[test]
    fn channel_status_banned_hit_uses_the_earliest_upcoming_title() {
        let mut later = holodex_stream("later", "upcoming", Some("2026-08-01T20:00:00Z"));
        later.title = "【 Phasmophobia 】 ウンウンウンウンOK幽霊ね!!!!!".to_string();
        let mut sooner = holodex_stream("sooner", "upcoming", Some("2026-08-01T12:00:00Z"));
        sooner.title = "【朝活雑談】 もう9月!!!!!!!!!!".to_string();

        let status = select_holodex_channel_status_at(
            "channel-id",
            &[later, sooner],
            at("2026-08-01T09:00:00Z"),
        );
        let haystack = crate::plugins::banned_keywords::danmaku_haystack(
            status.topic.as_deref().unwrap_or_default(),
            status.title.as_deref().unwrap_or_default(),
        );
        let banned = vec!["雑談".to_string()];

        assert_eq!(status.video_id.as_deref(), Some("sooner"));
        assert_eq!(
            crate::plugins::banned_keywords::banned_keyword_hit(&haystack, &banned).as_deref(),
            Some("雑談")
        );
    }

    #[test]
    fn channel_status_ignores_streams_beyond_the_upcoming_horizon() {
        let streams = vec![holodex_stream(
            "far-off",
            "upcoming",
            Some("2026-08-03T09:00:00Z"),
        )];

        let status =
            select_holodex_channel_status_at("channel-id", &streams, at("2026-08-01T09:00:00Z"));

        assert_eq!(status, YoutubeChannelStatus::default());
    }

    #[test]
    fn channel_status_keeps_upcoming_streams_without_a_parseable_schedule() {
        let streams = vec![holodex_stream("no-schedule", "upcoming", None)];

        let status =
            select_holodex_channel_status_at("channel-id", &streams, at("2026-08-01T09:00:00Z"));

        assert!(!status.is_live);
        assert_eq!(status.video_id.as_deref(), Some("no-schedule"));
        assert_eq!(status.scheduled_start, None);
    }

    #[test]
    fn channel_status_ignores_other_channels() {
        let mut other = holodex_stream("other-live", "live", None);
        other.channel.id = "someone-else".to_string();

        let status =
            select_holodex_channel_status_at("channel-id", &[other], at("2026-08-01T09:00:00Z"));

        assert_eq!(status, YoutubeChannelStatus::default());
    }

    #[test]
    fn strip_scheduled_title_suffix_removes_yt_dlp_date_suffix() {
        assert_eq!(
            strip_scheduled_title_suffix("Stream Title 2026-07-04 20:30"),
            "Stream Title"
        );
    }

    #[test]
    fn strip_scheduled_title_suffix_preserves_normal_title() {
        assert_eq!(
            strip_scheduled_title_suffix("Stream Title 20:30"),
            "Stream Title 20:30"
        );
    }

    #[test]
    fn yt_dlp_video_id_requires_following_output_line() {
        assert_eq!(
            yt_dlp_video_id_from_stdout("video-id\nhttps://example.com/live.m3u8\n").as_deref(),
            Some("video-id")
        );
        assert_eq!(yt_dlp_video_id_from_stdout("video-id\n"), None);
    }

    #[test]
    fn first_m3u8_url_from_stdout_detects_multiple_urls() {
        let (url, has_multiple) = first_m3u8_url_from_stdout(
            "video-id\nhttps://example.com/video.m3u8?token=1\nhttps://example.com/audio.m3u8\n",
        )
        .expect("m3u8 URL should be found");

        assert_eq!(url, "https://example.com/video.m3u8?token=1");
        assert!(has_multiple);
    }

    #[test]
    fn first_m3u8_url_from_stdout_returns_none_without_url() {
        assert!(first_m3u8_url_from_stdout("video-id\nnot-a-stream\n").is_none());
    }

    #[test]
    fn live_event_delay_parsers_ignore_invalid_values() {
        assert_eq!(
            capture_first_i64(
                live_event_hours_regex(),
                "ERROR: [youtube] This live event will begin in 12 hours"
            ),
            Some(12)
        );
        assert_eq!(
            capture_first_i64(
                live_event_days_regex(),
                "ERROR: [youtube] This live event will begin in many days"
            ),
            None
        );
    }
}
