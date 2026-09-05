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
use std::error::Error;
use std::process::Command;
use std::sync::OnceLock;
use std::time::Duration;

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
        Ok(get_youtube_status(&self.channel_id).await?)
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

/// Holodex is the monitor's first gate only when the toggle is on and a key
/// is configured. Otherwise yt-dlp talks to YouTube directly.
pub fn holodex_monitor_gate_enabled(cfg: &crate::config::Config) -> bool {
    holodex_monitor_gate_is_on(cfg.holodex_monitor_gate, cfg.holodex_api_key.as_deref())
}

fn holodex_monitor_gate_is_on(gate: bool, api_key: Option<&str>) -> bool {
    gate && api_key.is_some_and(|key| !key.is_empty())
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
/// When the Holodex monitor gate is on this is a Holodex read, cheap enough
/// for the WebUI poller. When the gate is off it takes the same yt-dlp path
/// as the monitor loop so the dashboard cannot disagree about live/upcoming.
pub async fn get_youtube_channel_status(
    channel_id: &str,
) -> Result<YoutubeChannelStatus, Box<dyn Error>> {
    let cfg = load_config().await?;
    if !holodex_monitor_gate_enabled(&cfg) {
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

    let streams = get_holodex_streams(vec![channel_id.to_string()], false).await?;
    Ok(select_holodex_channel_status(channel_id, &streams))
}

/// Channel status without a playable URL, falling back to yt-dlp when Holodex
/// is not the monitor gate.
///
/// With the gate on this stays a Holodex-only read. With it off (or with no
/// API key) it uses `get_youtube_status`, which resolves a stream URL as a
/// side effect of answering.
pub async fn get_youtube_channel_metadata(
    channel_id: &str,
) -> Result<YoutubeChannelStatus, Box<dyn Error>> {
    let cfg = load_config().await?;
    if holodex_monitor_gate_enabled(&cfg) {
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

pub async fn get_youtube_status(
    channel_id: &str,
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
    let cfg = load_config().await?;
    let proxy = cfg.youtube.proxy.clone();
    let quality = cfg.youtube.quality.clone();
    let cookies_file = &cfg.youtube.cookies_file;
    let cookies_from_browser = &cfg.youtube.cookies_from_browser;
    let deno_path = &cfg.youtube.deno_path;

    if !holodex_monitor_gate_enabled(&cfg) {
        tracing::debug!("Holodex monitor gate off, using yt-dlp for {}", channel_id);
        let title = get_youtube_live_title(channel_id).await?;
        return get_status_with_yt_dlp(
            channel_id,
            proxy,
            title,
            Some(&quality),
            cookies_file,
            cookies_from_browser,
            deno_path,
        )
        .await;
    }

    // Use the multi-channel function for single channel
    //
    // The error is reduced to a String right away: Box<dyn Error> is not Send,
    // and as the match scrutinee it would stay live across the awaits in the
    // fallback arm, making this whole future unspawnable.
    match get_holodex_streams(vec![channel_id.to_string()], false)
        .await
        .map_err(|e| e.to_string())
    {
        Ok(streams) => {
            let status = select_holodex_channel_status(channel_id, &streams);

            if status.is_live {
                // Holodex knows the stream; yt-dlp resolves the playable URL and
                // has the final say on whether it is actually live.
                let (is_live, _, _, m3u8_url, _, _) = get_status_with_yt_dlp(
                    channel_id,
                    proxy.clone(),
                    status.title.clone(),
                    Some(&quality),
                    cookies_file,
                    cookies_from_browser,
                    deno_path,
                )
                .await?;
                return Ok((
                    is_live,
                    status.topic,
                    status.title,
                    m3u8_url,
                    None,
                    status.video_id,
                ));
            }

            Ok((
                false,
                status.topic,
                status.title,
                None,
                status.scheduled_start,
                status.video_id,
            ))
        }
        Err(e) => {
            tracing::error!("Holodex API failed: {}, using yt-dlp", e);
            let title = get_youtube_live_title(channel_id).await?;
            let (is_live, _, _, m3u8_url, start_time, video_id) = get_status_with_yt_dlp(
                channel_id,
                proxy,
                None,
                Some(&quality),
                cookies_file,
                cookies_from_browser,
                deno_path,
            )
            .await?;
            Ok((is_live, None, title, m3u8_url, start_time, video_id))
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
            .filter(|line| {
                !line.trim().is_empty()
                    && !line.starts_with("WARNING")
                    && !line.starts_with("ERROR")
            })
            .next_back()
            .map(strip_scheduled_title_suffix)
            .filter(|s| !s.is_empty());

        Ok::<_, Box<dyn Error>>(title)
    };

    // Try Holodex API if it is the monitor's first gate.
    if holodex_monitor_gate_enabled(&cfg) {
        if let Some(key) = cfg.holodex_api_key.clone().filter(|k| !k.is_empty()) {
            match get_holodex_live_title(&key, channel_id, channel_name.as_deref()).await {
                Ok(title) => return Ok(title),
                _ => {
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
    fn holodex_monitor_gate_requires_both_flag_and_key() {
        assert!(!holodex_monitor_gate_is_on(true, None));
        assert!(!holodex_monitor_gate_is_on(true, Some("")));
        assert!(!holodex_monitor_gate_is_on(false, Some("key")));
        assert!(holodex_monitor_gate_is_on(true, Some("key")));
    }

    #[test]
    fn youtube_status_futures_stay_send() {
        // The WebUI spawns these, so a stray Box<dyn Error> held across an
        // await would break the callers rather than this module.
        fn assert_send<T: Send>(_: T) {}

        assert_send(get_youtube_status("channel-id"));
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
        }
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
    fn optional_channel_name_for_holodex_drops_lookup_errors() {
        let result: Option<String> =
            optional_channel_name_for_holodex(Err("channels unavailable"), "channel-id");

        assert_eq!(result, None);
    }

    #[test]
    fn optional_channel_name_for_holodex_preserves_lookup_value() {
        let result =
            optional_channel_name_for_holodex(Ok::<_, &str>(Some("Channel".to_string())), "id");

        assert_eq!(result.as_deref(), Some("Channel"));
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
