//! YouTube Data API overlay for Holodex rows.
//!
//! Holodex keeps rows `upcoming` after go-live, keeps finished or never-started
//! rows `live`, and lags YouTube in both directions. `videos.list` returns
//! YouTube's own `liveStreamingDetails` for up to 50 known video IDs per unit,
//! so the rows Holodex already sent are re-classified from that instead.
//!
//! Everything here degrades to the Holodex rows unchanged: no key, the daily
//! budget spent, or any API error.

use super::holodex::HolodexStream;
use super::http::{pooled_client, response_json_limited};
use crate::config::load_config;
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::error::Error;
use std::sync::Mutex;
use std::time::{Duration, Instant};

const VIDEOS_URL: &str = "https://www.googleapis.com/youtube/v3/videos";
const MAX_IDS_PER_CALL: usize = 50;
/// Google's default is 10,000 units/day; stop short so retries and the other
/// methods a later discovery pass needs still fit.
const DAILY_UNIT_BUDGET: u32 = 8_000;
/// Panel refreshes and monitor ticks inside this window reuse one answer.
const CACHE_TTL: Duration = Duration::from_secs(15);

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
/// members-only, deleted) and placeholder rows keep their Holodex values.
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
                return Some(stream);
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

/// Day index that rolls over at midnight Pacific. Uses PST all year: during
/// daylight time this resets an hour after Google does, which only makes the
/// local count more conservative.
fn pacific_day(now: chrono::DateTime<chrono::Utc>) -> i64 {
    (now - chrono::Duration::hours(8))
        .timestamp()
        .div_euclid(86_400)
}

#[derive(Default)]
struct QuotaCounter {
    day: i64,
    used: u32,
}

impl QuotaCounter {
    fn try_spend(&mut self, units: u32, day: i64) -> bool {
        if self.day != day {
            self.day = day;
            self.used = 0;
        }
        if self.used + units > DAILY_UNIT_BUDGET {
            return false;
        }
        self.used += units;
        true
    }
}

static QUOTA: Mutex<QuotaCounter> = Mutex::new(QuotaCounter { day: 0, used: 0 });

/// Answers per video ID; `None` records that YouTube omitted the ID so it is
/// not asked for again inside the TTL.
type VideoCache = HashMap<String, (Instant, Option<YtVideo>)>;
static CACHE: Mutex<Option<VideoCache>> = Mutex::new(None);

async fn fetch_videos(
    api_key: &str,
    proxy: Option<&str>,
    ids: &[String],
) -> Result<Vec<YtVideo>, Box<dyn Error>> {
    let client = pooled_client(proxy)?;
    let mut videos = Vec::new();
    for chunk in ids.chunks(MAX_IDS_PER_CALL) {
        let spent = QUOTA
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .try_spend(1, pacific_day(chrono::Utc::now()));
        if !spent {
            return Err("YouTube Data API daily budget used up".into());
        }
        let query = serde_urlencoded::to_string([
            ("part", "snippet,liveStreamingDetails"),
            ("id", &chunk.join(",")),
            ("key", api_key),
        ])?;
        let response = client
            .get(format!("{VIDEOS_URL}?{query}"))
            .timeout(Duration::from_secs(10))
            .send()
            .await?;
        if !response.status().is_success() {
            return Err(format!("YouTube Data API error: {}", response.status()).into());
        }
        let body: VideosResponse = response_json_limited(response).await?;
        videos.extend(body.items);
    }
    Ok(videos)
}

pub(crate) async fn videos_for(
    api_key: &str,
    proxy: Option<&str>,
    ids: Vec<String>,
) -> Result<HashMap<String, YtVideo>, Box<dyn Error>> {
    let now = Instant::now();
    let mut answered = HashMap::new();
    let mut missing = Vec::new();
    {
        let mut guard = CACHE.lock().unwrap_or_else(|e| e.into_inner());
        let cache = guard.get_or_insert_with(HashMap::new);
        cache.retain(|_, (at, _)| now.duration_since(*at) < CACHE_TTL);
        for id in ids {
            match cache.get(&id) {
                Some((_, Some(video))) => {
                    answered.insert(id, video.clone());
                }
                Some((_, None)) => {}
                None => missing.push(id),
            }
        }
    }
    if missing.is_empty() {
        return Ok(answered);
    }

    let fetched = fetch_videos(api_key, proxy, &missing).await?;
    let mut guard = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    let cache = guard.get_or_insert_with(HashMap::new);
    let mut returned: HashMap<String, YtVideo> = fetched
        .into_iter()
        .map(|video| (video.id.clone(), video))
        .collect();
    for id in missing {
        let video = returned.remove(&id);
        cache.insert(id.clone(), (now, video.clone()));
        if let Some(video) = video {
            answered.insert(id, video);
        }
    }
    Ok(answered)
}

/// Holodex rows corrected by YouTube, or unchanged when the overlay is off or fails.
pub async fn apply_youtube_overlay(streams: Vec<HolodexStream>) -> Vec<HolodexStream> {
    let Ok(cfg) = load_config().await else {
        return streams;
    };
    let Some(api_key) = cfg.youtube_api_key.as_deref().filter(|key| !key.is_empty()) else {
        return streams;
    };

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

    match videos_for(api_key, cfg.youtube.proxy.as_deref(), ids).await {
        Ok(videos) => overlay(streams, &videos),
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
    fn the_quota_counter_stops_at_the_budget_and_resets_each_pacific_day() {
        let mut quota = QuotaCounter::default();
        assert!(quota.try_spend(DAILY_UNIT_BUDGET, 1));
        assert!(!quota.try_spend(1, 1));
        assert!(quota.try_spend(1, 2));
    }

    #[test]
    fn the_pacific_day_rolls_over_at_eight_utc() {
        let at = |s: &str| {
            chrono::DateTime::parse_from_rfc3339(s)
                .unwrap()
                .with_timezone(&chrono::Utc)
        };
        let before = pacific_day(at("2026-09-24T07:59:59Z"));
        assert_eq!(pacific_day(at("2026-09-24T08:00:00Z")), before + 1);
        assert_eq!(pacific_day(at("2026-09-24T00:00:00Z")), before);
    }
}
