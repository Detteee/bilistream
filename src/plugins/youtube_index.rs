//! `videos.list` answers kept per video ID.
//!
//! The 15s per-request cache gives way to a store of every video ID asked
//! about in the last hour. Each is re-checked on its own cadence: live streams
//! every minute (end detection), waiting rooms from 5 min before their schedule
//! until 30 min after it every 30s, the monitored channels' waiting rooms up to
//! 1.5h late in steps, other streams due within 6h or long overdue every 5 min,
//! the rest every 30 min. Due IDs share calls, 50 per unit, and the intervals
//! stretch when the key pool spends faster than the Pacific day passes.

use super::youtube_data::{
    budget_remaining_fraction, classify, fetch_videos, pacific_day_left, stretch, YtLiveState,
    YtVideo, MAX_IDS_PER_CALL,
};
use crate::config::{load_config, Config};
use chrono::{DateTime, Utc};
use std::collections::{HashMap, HashSet};
use std::error::Error;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// End detection for live streams.
const LIVE: Duration = Duration::from_secs(60);
/// Waiting rooms about to open, or open late.
const GOING_LIVE: Duration = Duration::from_secs(30);
const WARM: Duration = Duration::from_secs(5 * 60);
const COLD: Duration = Duration::from_secs(30 * 60);
/// An upcoming stream polls at `GOING_LIVE` from this long before its
/// schedule...
const GOING_LIVE_BEFORE_SECS: i64 = 5 * 60;
/// ...until this long after it. Free-chat frames scheduled weeks ago would
/// otherwise hold the fastest tier forever.
const GOING_LIVE_AFTER_SECS: i64 = 30 * 60;
/// A monitored channel's waiting room past `GOING_LIVE_AFTER_SECS`: re-checked
/// at the step's interval while at most this late, then `WARM`.
const LATE_STEPS: [(i64, Duration); 2] = [
    (60 * 60, Duration::from_secs(2 * 60)),
    (90 * 60, Duration::from_secs(3 * 60)),
];
const WARM_BEFORE_SECS: i64 = 6 * 60 * 60;
/// IDs nobody asked about for this long are dropped.
const UNWANTED_AFTER: Duration = Duration::from_secs(60 * 60);
/// Caps the store.
const MAX_ENTRIES: usize = 2_000;
const RETRY_AFTER: Duration = Duration::from_secs(60);
const STATS_EVERY: Duration = Duration::from_secs(60 * 60);
const REFRESH_TICK: Duration = Duration::from_secs(10);

struct Entry {
    /// `None`: YouTube omitted the ID.
    video: Option<YtVideo>,
    checked: Instant,
    wanted: Instant,
}

impl Entry {
    /// Age as a share of the entry's interval: 1 means due.
    fn urgency(
        &self,
        now: Instant,
        utc: DateTime<Utc>,
        stretch: u32,
        monitored: &HashSet<String>,
    ) -> f64 {
        let watched = self
            .video
            .as_ref()
            .is_some_and(|video| monitored.contains(&video.snippet.channel_id));
        let period = interval(self.video.as_ref(), utc, watched) * stretch;
        now.duration_since(self.checked).as_secs_f64() / period.as_secs_f64()
    }

    /// Two intervals without a refresh means refreshes are failing. A stale
    /// answer is not served, just as a failed call serves none.
    fn fresh(
        &self,
        now: Instant,
        utc: DateTime<Utc>,
        stretch: u32,
        monitored: &HashSet<String>,
    ) -> bool {
        self.urgency(now, utc, stretch, monitored) < 2.0
    }
}

#[derive(Default)]
struct Store {
    entries: HashMap<String, Entry>,
    /// Interval multiplier from the last refresh; 0 before the first.
    stretch: u32,
    retry_at: Option<Instant>,
    /// `youtube::monitored_channels`, from the last refresher pass.
    monitored: HashSet<String>,
    units: u32,
    stats_since: Option<Instant>,
}

static STORE: Mutex<Option<Store>> = Mutex::new(None);

fn with_store<T>(f: impl FnOnce(&mut Store) -> T) -> T {
    let mut guard = STORE.lock().unwrap_or_else(|e| e.into_inner());
    f(guard.get_or_insert_with(Store::default))
}

/// Seed answers (and omissions, for asked IDs missing from `videos`) as if
/// `videos.list` had just returned them.
#[cfg(test)]
pub(crate) fn record_for_test(asked: &[String], videos: Vec<YtVideo>) {
    with_store(|store| {
        store.record(asked, videos, Instant::now());
    });
}

/// How often an answer is re-checked, before stretching. `monitored`: the
/// video is a monitored channel's.
fn interval(video: Option<&YtVideo>, now: DateTime<Utc>, monitored: bool) -> Duration {
    let Some(video) = video else {
        return COLD;
    };
    match classify(video) {
        YtLiveState::Live { .. } => LIVE,
        YtLiveState::Upcoming { scheduled } => {
            let until = scheduled
                .as_deref()
                .and_then(|at| DateTime::parse_from_rfc3339(at).ok())
                .map(|at| (at.with_timezone(&Utc) - now).num_seconds());
            match until {
                Some(secs) if secs > WARM_BEFORE_SECS => COLD,
                Some(secs) if (-GOING_LIVE_AFTER_SECS..=GOING_LIVE_BEFORE_SECS).contains(&secs) => {
                    GOING_LIVE
                }
                Some(secs) if monitored && secs < 0 => LATE_STEPS
                    .iter()
                    .find(|(late, _)| -secs <= *late)
                    .map_or(WARM, |(_, step)| *step),
                // Within 6h, long overdue, or without a schedule.
                _ => WARM,
            }
        }
        YtLiveState::Ended | YtLiveState::Vod => COLD,
    }
}

/// Whether `next` turns the video live: its previous answer was missing or
/// not live.
fn went_live(previous: Option<&YtVideo>, next: &YtVideo) -> bool {
    let is_live = |video: &YtVideo| matches!(classify(video), YtLiveState::Live { .. });
    is_live(next) && !previous.is_some_and(is_live)
}

/// IDs to re-check now: every due one, with the calls they need filled up by
/// those past half their interval. Riding along costs nothing and keeps
/// entries checked together, so a tier stays one call per interval instead of
/// drifting apart into many.
fn plan<'a>(urgencies: impl IntoIterator<Item = (&'a str, f64)>) -> Vec<String> {
    let mut ranked: Vec<(&str, f64)> = urgencies
        .into_iter()
        .filter(|(_, urgency)| *urgency >= 0.5)
        .collect();
    let due = ranked.iter().filter(|(_, urgency)| *urgency >= 1.0).count();
    if due == 0 {
        return Vec::new();
    }
    ranked.sort_by(|a, b| b.1.total_cmp(&a.1));
    ranked.truncate(due.div_ceil(MAX_IDS_PER_CALL) * MAX_IDS_PER_CALL);
    ranked.into_iter().map(|(id, _)| id.to_string()).collect()
}

impl Store {
    fn stretch(&self) -> u32 {
        self.stretch.max(1)
    }

    /// Fresh answers for `ids`, which now count as wanted, and the IDs that
    /// have to be fetched. An answer checked more than `max_age` ago counts as
    /// missing.
    fn lookup(
        &mut self,
        ids: Vec<String>,
        now: Instant,
        utc: DateTime<Utc>,
        max_age: Option<Duration>,
    ) -> (HashMap<String, YtVideo>, Vec<String>) {
        let stretch = self.stretch();
        let mut answered = HashMap::new();
        let mut missing = Vec::new();
        for id in ids {
            let Some(entry) = self.entries.get_mut(&id) else {
                missing.push(id);
                continue;
            };
            entry.wanted = now;
            let too_old = max_age.is_some_and(|max| now.duration_since(entry.checked) > max);
            if too_old || !entry.fresh(now, utc, stretch, &self.monitored) {
                missing.push(id);
            } else if let Some(video) = &entry.video {
                answered.insert(id, video.clone());
            }
        }
        (answered, missing)
    }

    /// YouTube's answer for every asked ID; one it did not return is omitted.
    /// Returns the monitored channels whose video it turned live.
    fn record(&mut self, asked: &[String], videos: Vec<YtVideo>, now: Instant) -> Vec<String> {
        let mut returned: HashMap<String, YtVideo> = videos
            .into_iter()
            .map(|video| (video.id.clone(), video))
            .collect();
        let mut woken = Vec::new();
        for id in asked {
            let video = returned.remove(id);
            let previous = self.entries.get(id).and_then(|entry| entry.video.as_ref());
            if let Some(next) = &video {
                if self.monitored.contains(&next.snippet.channel_id) && went_live(previous, next) {
                    woken.push(next.snippet.channel_id.clone());
                }
            }
            match self.entries.get_mut(id) {
                Some(entry) => {
                    entry.video = video;
                    entry.checked = now;
                }
                None => {
                    let entry = Entry {
                        video,
                        checked: now,
                        wanted: now,
                    };
                    self.entries.insert(id.clone(), entry);
                }
            }
        }
        self.cap();
        woken
    }

    fn prune(&mut self, now: Instant) {
        self.entries
            .retain(|_, entry| now.duration_since(entry.wanted) < UNWANTED_AFTER);
        self.cap();
    }

    /// Past `MAX_ENTRIES`, the IDs unwanted the longest go first.
    fn cap(&mut self) {
        let excess = self.entries.len().saturating_sub(MAX_ENTRIES);
        if excess == 0 {
            return;
        }
        let mut by_wanted: Vec<(Instant, String)> = self
            .entries
            .iter()
            .map(|(id, entry)| (entry.wanted, id.clone()))
            .collect();
        by_wanted.sort();
        for (_, id) in by_wanted.into_iter().take(excess) {
            self.entries.remove(&id);
        }
    }

    fn log_stats(&mut self, now: Instant) {
        let since = *self.stats_since.get_or_insert(now);
        if now.duration_since(since) < STATS_EVERY {
            return;
        }
        tracing::info!(
            "YouTube 索引: 过去一小时 videos.list 调用 {} 次，跟踪 {} 个视频，刷新间隔 {} 倍",
            self.units,
            self.entries.len(),
            self.stretch()
        );
        self.units = 0;
        self.stats_since = Some(now);
    }
}

/// Wakes the monitor of each channel an answer turned live.
fn wake_monitors(channels: Vec<String>) {
    for channel_id in channels {
        super::youtube::wake_monitor(&channel_id);
    }
}

/// `videos_for`: fresh answers from the store. Unknown, stale and, with
/// `max_age`, older IDs are fetched now and kept.
pub(crate) async fn store_videos(
    keys: &[String],
    proxy: Option<&str>,
    ids: Vec<String>,
    max_age: Option<Duration>,
) -> Result<HashMap<String, YtVideo>, Box<dyn Error>> {
    let now = Instant::now();
    let (mut answered, missing) = with_store(|store| store.lookup(ids, now, Utc::now(), max_age));
    if missing.is_empty() {
        return Ok(answered);
    }
    let fetched = fetch_videos(keys, proxy, &missing).await?;
    let woken = with_store(|store| {
        store.units += missing.len().div_ceil(MAX_IDS_PER_CALL) as u32;
        let woken = store.record(&missing, fetched, now);
        for id in missing {
            if let Some(video) = store.entries.get(&id).and_then(|entry| entry.video.clone()) {
                answered.insert(id, video);
            }
        }
        woken
    });
    wake_monitors(woken);
    Ok(answered)
}

/// One refresher pass: re-check what is due. `true` when it recorded answers.
pub(crate) async fn refresh(keys: &[String], proxy: Option<&str>) -> bool {
    let now = Instant::now();
    let utc = Utc::now();
    let stretch = stretch(budget_remaining_fraction(keys), pacific_day_left(utc));
    let due = with_store(|store| {
        if stretch != store.stretch() {
            if stretch == 1 {
                tracing::info!("YouTube 索引: 配额进度恢复，刷新间隔恢复正常");
            } else {
                tracing::info!(
                    "YouTube 索引: 配额消耗快于时间进度，刷新间隔放大 {} 倍",
                    stretch
                );
            }
        }
        store.stretch = stretch;
        store.prune(now);
        if store.retry_at.is_some_and(|at| now < at) {
            return Vec::new();
        }
        let monitored = &store.monitored;
        plan(
            store
                .entries
                .iter()
                .map(|(id, entry)| (id.as_str(), entry.urgency(now, utc, stretch, monitored))),
        )
    });
    let mut recorded = false;
    if !due.is_empty() {
        match fetch_videos(keys, proxy, &due)
            .await
            .map_err(|e| e.to_string())
        {
            Ok(videos) => {
                let woken = with_store(|store| {
                    store.retry_at = None;
                    store.units += due.len().div_ceil(MAX_IDS_PER_CALL) as u32;
                    store.record(&due, videos, now)
                });
                wake_monitors(woken);
                recorded = true;
            }
            Err(e) => with_store(|store| {
                if store.retry_at.is_none() {
                    tracing::warn!("YouTube 索引刷新失败: {}", e);
                } else {
                    tracing::debug!("YouTube 索引刷新失败: {}", e);
                }
                store.retry_at = Some(now + RETRY_AFTER);
            }),
        }
    }

    with_store(|store| store.log_stats(now));
    recorded
}

/// One refresher pass for `cfg`. Without a key the store is dropped: nothing
/// is tracked and no call is made.
pub(crate) async fn refresh_tick(cfg: &Config) {
    let keys = cfg.youtube_api_keys();
    if keys.is_empty() {
        *STORE.lock().unwrap_or_else(|e| e.into_inner()) = None;
        return;
    }
    let monitored = super::youtube::monitored_channels(cfg);
    with_store(|store| store.monitored = monitored);
    if refresh(&keys, cfg.youtube.proxy.as_deref()).await {
        crate::webui::holodex_list::wake();
    }
}

static REFRESH_WORKER_STARTED: AtomicBool = AtomicBool::new(false);

pub struct StoreRefreshWorker(tokio::task::JoinHandle<()>);

impl Drop for StoreRefreshWorker {
    fn drop(&mut self) {
        self.0.abort();
        REFRESH_WORKER_STARTED.store(false, Ordering::SeqCst);
    }
}

/// Re-checks the store's due IDs every `REFRESH_TICK`.
pub fn start_store_refresh_worker() -> Option<StoreRefreshWorker> {
    if REFRESH_WORKER_STARTED.swap(true, Ordering::SeqCst) {
        return None;
    }
    Some(StoreRefreshWorker(tokio::spawn(async {
        loop {
            // Box<dyn Error> is not Send: reduce it before the next await.
            match load_config().await.map_err(|e| e.to_string()) {
                Ok(cfg) => refresh_tick(&cfg).await,
                Err(e) => tracing::debug!("YouTube 索引刷新读取配置失败: {}", e),
            }
            tokio::time::sleep(REFRESH_TICK).await;
        }
    })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugins::youtube_data::{YtLiveDetails, YtSnippet};

    fn at(rfc3339: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(rfc3339)
            .unwrap()
            .with_timezone(&Utc)
    }

    fn video(id: &str, start: Option<&str>, end: Option<&str>, scheduled: Option<&str>) -> YtVideo {
        YtVideo {
            id: id.to_string(),
            snippet: YtSnippet {
                title: format!("{id} title"),
                channel_id: "UCchannel".to_string(),
            },
            live_streaming_details: Some(YtLiveDetails {
                actual_start_time: start.map(str::to_string),
                actual_end_time: end.map(str::to_string),
                scheduled_start_time: scheduled.map(str::to_string),
                concurrent_viewers: Some("100".to_string()),
            }),
        }
    }

    fn live(id: &str) -> YtVideo {
        video(id, Some("2026-09-25T11:00:00Z"), None, None)
    }

    fn ids(list: &[&str]) -> Vec<String> {
        list.iter().map(|id| id.to_string()).collect()
    }

    #[test]
    fn each_answer_is_rechecked_at_its_tier() {
        let now = at("2026-09-25T12:00:00Z");
        let tier = |video: YtVideo| interval(Some(&video), now, false);
        let upcoming = |scheduled: &str| tier(video("u", None, None, Some(scheduled)));

        assert_eq!(tier(live("l")), LIVE);
        assert_eq!(upcoming("2026-09-25T12:05:00Z"), GOING_LIVE);
        assert_eq!(upcoming("2026-09-25T12:05:01Z"), WARM);
        assert_eq!(
            upcoming("2026-09-25T11:30:00Z"),
            GOING_LIVE,
            "half an hour late"
        );
        assert_eq!(upcoming("2026-09-25T11:29:59Z"), WARM, "longer overdue");
        assert_eq!(upcoming("2026-09-25T18:00:00Z"), WARM);
        assert_eq!(upcoming("2026-09-25T18:00:01Z"), COLD);
        assert_eq!(tier(video("u", None, None, None)), WARM, "no schedule");
        let ended = video(
            "e",
            Some("2026-09-25T10:00:00Z"),
            Some("2026-09-25T11:00:00Z"),
            None,
        );
        assert_eq!(tier(ended), COLD);
        let upload = YtVideo {
            live_streaming_details: None,
            ..live("v")
        };
        assert_eq!(tier(upload), COLD);
        assert_eq!(interval(None, now, false), COLD, "omitted by YouTube");
    }

    #[test]
    fn a_monitored_waiting_room_running_late_steps_down_to_warm() {
        let now = at("2026-09-25T12:00:00Z");
        let late = |scheduled: &str, monitored: bool| {
            interval(
                Some(&video("u", None, None, Some(scheduled))),
                now,
                monitored,
            )
        };
        let two = Duration::from_secs(2 * 60);
        let three = Duration::from_secs(3 * 60);
        assert_eq!(late("2026-09-25T11:29:59Z", true), two);
        assert_eq!(late("2026-09-25T11:00:00Z", true), two);
        assert_eq!(late("2026-09-25T10:59:59Z", true), three);
        assert_eq!(late("2026-09-25T10:30:00Z", true), three);
        assert_eq!(late("2026-09-25T10:29:59Z", true), WARM);
        assert_eq!(late("2026-09-25T11:29:59Z", false), WARM, "other channel");
        assert_eq!(late("2026-09-25T11:30:00Z", true), GOING_LIVE);
    }

    #[test]
    fn only_an_answer_that_turns_a_video_live_counts_as_going_live() {
        let upcoming = video("v", None, None, Some("2026-09-25T12:00:00Z"));
        let ended = video(
            "v",
            Some("2026-09-25T11:00:00Z"),
            Some("2026-09-25T12:00:00Z"),
            None,
        );
        assert!(went_live(None, &live("v")));
        assert!(went_live(Some(&upcoming), &live("v")));
        assert!(!went_live(Some(&live("v")), &live("v")));
        assert!(!went_live(Some(&live("v")), &ended));
        assert!(!went_live(None, &upcoming));
    }

    #[test]
    fn recording_a_go_live_names_only_monitored_channels() {
        let t0 = Instant::now();
        let mut store = Store {
            monitored: HashSet::from(["UCchannel".to_string()]),
            ..Store::default()
        };
        let upcoming = video("v", None, None, Some("2026-09-25T12:00:00Z"));
        assert!(store.record(&ids(&["v"]), vec![upcoming], t0).is_empty());
        assert_eq!(
            store.record(&ids(&["v"]), vec![live("v")], t0),
            ids(&["UCchannel"])
        );
        assert!(store.record(&ids(&["v"]), vec![live("v")], t0).is_empty());

        let mut other = live("w");
        other.snippet.channel_id = "UCother".to_string();
        assert!(store.record(&ids(&["w"]), vec![other], t0).is_empty());
    }

    #[test]
    fn the_channel_id_parses_from_a_videos_list_answer() {
        let body = serde_json::json!({
            "id": "v",
            "snippet": {"title": "t", "channelId": "UCgYCMluaLpERsyNXlPOvBtA"},
            "liveStreamingDetails": {"scheduledStartTime": "2026-09-25T12:00:00Z"}
        });
        let video: YtVideo = serde_json::from_value(body).unwrap();
        assert_eq!(video.snippet.channel_id, "UCgYCMluaLpERsyNXlPOvBtA");
    }

    #[test]
    fn due_ids_share_their_calls_with_ids_past_half_their_interval() {
        assert!(plan([("a", 0.9), ("b", 0.6)]).is_empty(), "nothing due");
        assert_eq!(
            plan([
                ("fresh", 0.2),
                ("rider", 0.7),
                ("due", 1.4),
                ("closer", 0.9)
            ]),
            ids(&["due", "closer", "rider"])
        );

        let many: Vec<(String, f64)> = (0..60)
            .map(|i| (format!("due{i}"), 1.0))
            .chain((0..60).map(|i| (format!("ride{i}"), 0.6)))
            .collect();
        let batch = plan(many.iter().map(|(id, urgency)| (id.as_str(), *urgency)));
        assert_eq!(batch.len(), 2 * MAX_IDS_PER_CALL, "60 due need two calls");
        assert_eq!(batch.iter().filter(|id| id.starts_with("due")).count(), 60);
    }

    #[test]
    fn ids_unwanted_for_an_hour_are_dropped() {
        let t0 = Instant::now();
        let utc = at("2026-09-25T12:00:00Z");
        let mut store = Store::default();
        store.record(&ids(&["old", "asked"]), Vec::new(), t0);
        let later = t0 + UNWANTED_AFTER;
        store.lookup(ids(&["asked"]), later - Duration::from_secs(1), utc, None);
        store.prune(later);
        assert!(store.entries.contains_key("asked"));
        assert!(!store.entries.contains_key("old"));
    }

    #[test]
    fn past_the_cap_the_ids_unwanted_longest_go_first() {
        let t0 = Instant::now();
        let mut store = Store::default();
        for i in 0..MAX_ENTRIES + 2 {
            let id = format!("v{i}");
            store.record(&[id], Vec::new(), t0 + Duration::from_millis(i as u64));
        }
        assert_eq!(store.entries.len(), MAX_ENTRIES);
        assert!(!store.entries.contains_key("v0"));
        assert!(!store.entries.contains_key("v1"));
        assert!(store.entries.contains_key("v2"));
    }

    #[test]
    fn fresh_answers_are_served_and_stale_or_unknown_ids_fetched() {
        let t0 = Instant::now();
        let utc = at("2026-09-25T12:00:00Z");
        let mut store = Store::default();
        store.record(&ids(&["live", "omitted"]), vec![live("live")], t0);

        let (answered, missing) = store.lookup(
            ids(&["live", "omitted", "new"]),
            t0 + Duration::from_secs(30),
            utc,
            None,
        );
        assert_eq!(answered.keys().collect::<Vec<_>>(), vec!["live"]);
        assert_eq!(missing, ids(&["new"]));

        // Two live intervals without a refresh: refreshes are failing.
        let (answered, missing) = store.lookup(ids(&["live"]), t0 + 2 * LIVE, utc, None);
        assert!(answered.is_empty());
        assert_eq!(missing, ids(&["live"]));
    }

    #[test]
    fn a_max_age_refetches_older_answers() {
        let t0 = Instant::now();
        let utc = at("2026-09-25T12:00:00Z");
        let cap = Some(Duration::from_secs(15));
        let mut store = Store::default();
        store.record(&ids(&["live"]), vec![live("live")], t0);

        let (answered, missing) =
            store.lookup(ids(&["live"]), t0 + Duration::from_secs(15), utc, cap);
        assert_eq!(answered.len(), 1);
        assert!(missing.is_empty());

        let (answered, missing) =
            store.lookup(ids(&["live"]), t0 + Duration::from_secs(16), utc, cap);
        assert!(answered.is_empty());
        assert_eq!(missing, ids(&["live"]));
    }
}
