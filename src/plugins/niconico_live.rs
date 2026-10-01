//! Niconico liveness from each roster channel's live listing page.
//!
//! Holodex does not list Niconico. While a stream list is leased, the worker
//! asks about every roster channel, and the lists overlay those answers
//! (`overlay`). While the Niconico monitor is on, it asks about the target
//! too, whose turn to live ends the monitor's wait (`take_monitor_wake`), as
//! YouTube's and Twitch's go-live do. There is no batch API, so each channel
//! is one `ch.nicovideo.jp/{id}/live` GET.
//!
//! vspo posts a start time and does not go live unannounced, so an idle or
//! far-scheduled channel is asked once a day. From ten minutes before that
//! start, and while OnAir, the listing is asked every five minutes: the
//! waiting-room start is not worth a tighter probe. The main monitor loop
//! (`monitor_status`) reads that same cache; it only GETs when the cadence
//! says so, or when ffmpeg just exited and must know if the program is still
//! on.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use chrono::{DateTime, Local};
use futures_util::future::join_all;
use tokio::sync::Notify;

use super::holodex::{HolodexChannel, HolodexStream};
use super::niconico::{
    fetch_channel_listing, live_id_from_link, normalize_channel_id, watch_url, ChannelLiveListing,
    ChannelProgram,
};
use crate::config::{load_config, Config};

/// Idle and far-scheduled listings. vspo announces the start and does not
/// go live without one.
const DAY: Duration = Duration::from_secs(24 * 60 * 60);
/// OnAir, and scheduled starts inside `NEAR_START`.
const LIVE_PROBE: Duration = Duration::from_secs(5 * 60);
/// Switch from the daily probe to `LIVE_PROBE` this far before `start_at`.
const NEAR_START: chrono::TimeDelta = chrono::Duration::minutes(10);
/// A channel no answer covers yet (a list just leased, a roster edit) is
/// asked this soon instead.
const NEW_CHANNEL_GAP: Duration = Duration::from_secs(10);
/// Older answers no longer correct the lists. Longer than `DAY` so a daily
/// probe still overlays until the next one.
const FRESH_FOR: Duration = Duration::from_secs(26 * 60 * 60);
const TICK: Duration = Duration::from_secs(10);
/// Channel pages are fetched a few at a time.
const CHANNELS_PER_PASS: usize = 4;

/// A channels.json entry with both a YouTube channel and a Niconico slug.
/// Overlay rows reuse that YouTube channel id so the allowlist, name and
/// photo match Holodex; they are still a separate card from the YouTube
/// stream on the same channel.
#[derive(Debug, Clone, PartialEq)]
struct RosterChannel {
    channel_id: String,
    youtube_id: String,
    name: String,
}

/// Per asked channel, its listing, and the roster entries the lists read
/// them for. A missing map entry is unanswered, not idle.
#[derive(Debug, Default, Clone, PartialEq)]
struct Answers {
    listings: HashMap<String, ChannelLiveListing>,
    roster: Vec<RosterChannel>,
}

#[derive(Default)]
struct PollState {
    last_attempt: HashMap<String, Instant>,
    /// Last attempt for these ids returned no listing.
    failed: HashSet<String>,
    answered_at: Option<Instant>,
    answers: Answers,
    /// Warn once per failure streak, then debug.
    failing: bool,
}

impl PollState {
    /// Picks the due channels and marks them attempted. A loaded roster that
    /// differs from the recorded one is recorded now, even if nothing is due:
    /// the target's fresh listing (asked for the monitor while no list was
    /// leased, so with no roster) would otherwise not overlay until that
    /// channel is due again. Returns the due ids and whether the roster
    /// changed.
    fn begin_pass(
        &mut self,
        ids: &[String],
        roster: &[RosterChannel],
        now: Instant,
        now_local: DateTime<Local>,
    ) -> (Vec<String>, bool) {
        let due: Vec<String> = ids
            .iter()
            .filter(|id| {
                channel_due(
                    self.last_attempt.get(*id).copied(),
                    self.answers.listings.get(*id),
                    self.failed.contains(*id),
                    now,
                    now_local,
                )
            })
            .cloned()
            .collect();
        for id in &due {
            self.last_attempt.insert(id.clone(), now);
        }
        let adopted = !roster.is_empty() && roster != self.answers.roster.as_slice();
        if adopted {
            self.answers.roster = roster.to_vec();
        }
        (due, adopted)
    }

    /// Keeps a successful (possibly partial) answer. Failed channels keep
    /// their last listing. Unasked channels that are still in `keep` keep
    /// theirs too. Returns the target channel when it turned live, and
    /// whether the lists read these answers and they changed.
    fn record(
        &mut self,
        fetched: HashMap<String, ChannelLiveListing>,
        roster: Vec<RosterChannel>,
        asked: &[String],
        keep: &HashSet<String>,
        target: Option<&str>,
        now: Instant,
    ) -> (Option<String>, bool) {
        let is_live = |listings: &HashMap<String, ChannelLiveListing>, id: &str| {
            matches!(listings.get(id), Some(ChannelLiveListing::OnAir(_)))
        };
        let mut listings = self.answers.listings.clone();
        listings.retain(|id, _| keep.contains(id));
        for id in asked {
            if let Some(listing) = fetched.get(id) {
                listings.insert(id.clone(), listing.clone());
            }
        }
        let woken = target
            .filter(|id| is_live(&listings, id) && !is_live(&self.answers.listings, id))
            .map(str::to_string);
        let next = Answers { listings, roster };
        let changed = !next.roster.is_empty()
            && (next.listings != self.answers.listings || next.roster != self.answers.roster);
        self.answers = next;
        self.answered_at = Some(now);
        (woken, changed)
    }
}

static STATE: LazyLock<Mutex<PollState>> = LazyLock::new(|| Mutex::new(PollState::default()));
/// `notify_one` keeps one permit while the worker is busy.
static WAKE: Notify = Notify::const_new();

/// Target channels whose go-live no monitor pass picked up yet.
static MONITOR_WAKES: Mutex<Option<HashSet<String>>> = Mutex::new(None);

fn with_state<T>(f: impl FnOnce(&mut PollState) -> T) -> T {
    let mut guard = STATE.lock().unwrap_or_else(|e| e.into_inner());
    f(&mut guard)
}

fn wake_monitor(channel_id: &str) {
    let mut guard = MONITOR_WAKES.lock().unwrap_or_else(|e| e.into_inner());
    guard
        .get_or_insert_with(HashSet::new)
        .insert(channel_id.to_string());
}

/// Whether the target channel has a go-live wake waiting; taking it clears it.
pub fn take_monitor_wake(channel_id: &str) -> bool {
    let id = niconico_channel_id_of(channel_id);
    if id.is_empty() {
        return false;
    }
    let mut guard = MONITOR_WAKES.lock().unwrap_or_else(|e| e.into_inner());
    guard.as_mut().is_some_and(|wakes| wakes.remove(&id))
}

fn niconico_channel_id_of(channel_id: &str) -> String {
    normalize_channel_id(channel_id)
}

/// The target channel while the Niconico monitor is on.
fn target_channel(enable_monitor: bool, channel_id: &str) -> Option<String> {
    let id = niconico_channel_id_of(channel_id);
    (enable_monitor && !id.is_empty()).then_some(id)
}

/// Ask the listing pages now (or as soon as the worker is free). A newly
/// leased list calls this so the roster is covered without waiting for the
/// next tick.
pub(crate) fn wake() {
    WAKE.notify_one();
}

/// The roster's Niconico channels that also name a YouTube channel.
async fn roster_channels() -> Vec<RosterChannel> {
    match crate::storage::read_json::<serde_json::Value>("channels.json") {
        Ok(json) => roster_from_value(&json),
        Err(_) => Vec::new(),
    }
}

#[cfg(test)]
fn parse_roster(content: &str) -> Vec<RosterChannel> {
    let Ok(json) = serde_json::from_str::<serde_json::Value>(content) else {
        return Vec::new();
    };
    roster_from_value(&json)
}

fn roster_from_value(json: &serde_json::Value) -> Vec<RosterChannel> {
    let mut seen = HashSet::new();
    json.get("channels")
        .and_then(|v| v.as_array())
        .into_iter()
        .flatten()
        .filter_map(|channel| {
            let platform = |key: &str| {
                channel
                    .get("platforms")
                    .and_then(|p| p.get(key))
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|v| !v.is_empty())
            };
            let channel_id = niconico_channel_id_of(platform("niconico")?);
            if channel_id.is_empty() {
                return None;
            }
            let youtube_id = platform("youtube")?.to_string();
            let name = channel.get("name").and_then(|v| v.as_str()).unwrap_or("");
            seen.insert(channel_id.clone()).then(|| RosterChannel {
                channel_id,
                youtube_id,
                name: name.to_string(),
            })
        })
        .collect()
}

async fn fetch_listings(
    ids: &[String],
    proxy: Option<&str>,
) -> Result<HashMap<String, ChannelLiveListing>, String> {
    let proxy = proxy.map(str::to_string);
    let mut listings = HashMap::with_capacity(ids.len());
    let mut last_err = None;
    for chunk in ids.chunks(CHANNELS_PER_PASS) {
        let futs = chunk.iter().map(|id| {
            let id = id.clone();
            let proxy = proxy.clone();
            async move {
                let result = fetch_channel_listing(&id, proxy.as_deref())
                    .await
                    .map_err(|e| e.to_string());
                (id, result)
            }
        });
        for (id, result) in join_all(futs).await {
            match result {
                Ok(listing) => {
                    listings.insert(id, listing);
                }
                Err(e) => last_err = Some(e),
            }
        }
    }
    if listings.is_empty() {
        return Err(last_err.unwrap_or_else(|| "Niconico 频道直播页查询失败".to_string()));
    }
    Ok(listings)
}

/// How long to wait after a successful listing before asking again.
fn next_gap(listing: &ChannelLiveListing, now: DateTime<Local>) -> Duration {
    match listing {
        ChannelLiveListing::Idle => DAY,
        ChannelLiveListing::OnAir(_) => LIVE_PROBE,
        ChannelLiveListing::Scheduled(program) => match program.start_at {
            Some(start) => {
                let near = start - NEAR_START;
                if now >= near {
                    LIVE_PROBE
                } else {
                    (near - now).to_std().unwrap_or(Duration::ZERO).min(DAY)
                }
            }
            None => DAY,
        },
    }
}

fn channel_due(
    last: Option<Instant>,
    listing: Option<&ChannelLiveListing>,
    failed: bool,
    now: Instant,
    now_local: DateTime<Local>,
) -> bool {
    let Some(at) = last else {
        return true;
    };
    let since = now.saturating_duration_since(at);
    if failed {
        return since >= LIVE_PROBE;
    }
    match listing {
        None => since >= NEW_CHANNEL_GAP,
        Some(listing) => since >= next_gap(listing, now_local),
    }
}

fn listing_if_fresh(
    state: &PollState,
    id: &str,
    now: Instant,
    now_local: DateTime<Local>,
) -> Option<ChannelLiveListing> {
    let listing = state.answers.listings.get(id).cloned();
    let due = channel_due(
        state.last_attempt.get(id).copied(),
        listing.as_ref(),
        state.failed.contains(id),
        now,
        now_local,
    );
    (!due).then_some(listing).flatten()
}

type ListingStatus = (
    bool,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<DateTime<Local>>,
    Option<String>,
);

fn status_from_listing(listing: &ChannelLiveListing) -> ListingStatus {
    match listing {
        ChannelLiveListing::OnAir(program) => (
            true,
            None,
            program.title.clone(),
            Some(watch_url(&program.live_id)),
            None,
            Some(program.live_id.clone()),
        ),
        ChannelLiveListing::Scheduled(program) => (
            false,
            None,
            program.title.clone(),
            Some(watch_url(&program.live_id)),
            program.start_at,
            Some(program.live_id.clone()),
        ),
        ChannelLiveListing::Idle => (false, None, None, None, None, None),
    }
}

fn remember_listing(
    state: &mut PollState,
    id: &str,
    listing: ChannelLiveListing,
    now: Instant,
) -> (Option<String>, bool) {
    state.failed.remove(id);
    let mut keep: HashSet<String> = state.answers.listings.keys().cloned().collect();
    keep.insert(id.to_string());
    let asked = [id.to_string()];
    let roster = state.answers.roster.clone();
    state.record(
        [(id.to_string(), listing)].into_iter().collect(),
        roster,
        &asked,
        &keep,
        Some(id),
        now,
    )
}

/// The Niconico monitor's live check. Uses the same schedule-aware listing
/// cache as the panels unless `force` (ffmpeg died and we must know now).
pub async fn monitor_status(
    cfg: &crate::config::Niconico,
    force: bool,
) -> Result<
    (
        bool,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<DateTime<Local>>,
        Option<String>,
    ),
    Box<dyn std::error::Error>,
> {
    let id = niconico_channel_id_of(&cfg.channel_id);
    if id.is_empty() {
        return super::niconico::get_niconico_status(cfg).await;
    }
    let now = Instant::now();
    let now_local = Local::now();
    if !force {
        if let Some(listing) = with_state(|state| listing_if_fresh(state, &id, now, now_local)) {
            return Ok(status_from_listing(&listing));
        }
    }
    with_state(|state| {
        state.last_attempt.insert(id.clone(), now);
    });
    match fetch_channel_listing(&id, cfg.proxy.as_deref()).await {
        Ok(listing) => {
            let (woken, changed) = with_state(|state| {
                if state.failing {
                    tracing::info!("Niconico 直播状态查询恢复");
                }
                state.failing = false;
                remember_listing(state, &id, listing.clone(), now)
            });
            if let Some(channel_id) = woken {
                wake_monitor(&channel_id);
            }
            if changed {
                crate::webui::holodex_list::wake();
            }
            Ok(status_from_listing(&listing))
        }
        Err(e) => {
            let stale = with_state(|state| {
                state.failed.insert(id.clone());
                state.answers.listings.get(&id).cloned()
            });
            match stale {
                Some(listing) => Ok(status_from_listing(&listing)),
                None => Err(e),
            }
        }
    }
}

/// Asks the listing pages when due: about the roster while a list is leased,
/// and about the target while its monitor is on. A leased pass records the
/// roster even when no channel is due.
async fn poll_pass(cfg: &Config) {
    let target = target_channel(cfg.niconico.enable_monitor, &cfg.niconico.channel_id);
    let roster = if crate::webui::holodex_list::any_leased() {
        roster_channels().await
    } else {
        Vec::new()
    };
    let mut seen = HashSet::new();
    let ids: Vec<String> = roster
        .iter()
        .map(|entry| entry.channel_id.clone())
        .chain(target.clone())
        .filter(|id| seen.insert(id.clone()))
        .collect();
    if ids.is_empty() {
        return;
    }
    let now = Instant::now();
    let now_local = Local::now();
    let (due, adopted) = with_state(|state| state.begin_pass(&ids, &roster, now, now_local));
    if due.is_empty() {
        if adopted {
            crate::webui::holodex_list::wake();
        }
        return;
    }
    let keep: HashSet<String> = ids.iter().cloned().collect();
    let fetched = fetch_listings(&due, cfg.niconico.proxy.as_deref()).await;
    let (woken, changed) = with_state(|state| {
        state.last_attempt.retain(|id, _| keep.contains(id));
        state.failed.retain(|id| keep.contains(id));
        match fetched {
            Ok(listings) => {
                if state.failing {
                    tracing::info!("Niconico 直播状态查询恢复");
                }
                state.failing = false;
                for id in &due {
                    if listings.contains_key(id) {
                        state.failed.remove(id);
                    } else {
                        state.failed.insert(id.clone());
                    }
                }
                state.record(listings, roster, &due, &keep, target.as_deref(), now)
            }
            Err(e) => {
                if state.failing {
                    tracing::debug!("Niconico 直播状态查询失败: {}", e);
                } else {
                    tracing::warn!(
                        "Niconico 直播状态查询失败，列表沿用上次的 Niconico 状态: {}",
                        e
                    );
                }
                state.failing = true;
                for id in &due {
                    state.failed.insert(id.clone());
                }
                (None, false)
            }
        }
    });
    if let Some(channel_id) = woken {
        wake_monitor(&channel_id);
    }
    if adopted || changed {
        crate::webui::holodex_list::wake();
    }
}

/// Holodex's rows corrected by the listings, or as they are when no listing
/// has answered within `FRESH_FOR`.
pub(crate) fn overlay(rows: Vec<HolodexStream>) -> Vec<HolodexStream> {
    let now = Instant::now();
    let answers = with_state(|state| {
        let fresh = state
            .answered_at
            .is_some_and(|at| now.saturating_duration_since(at) < FRESH_FOR);
        fresh.then(|| state.answers.clone())
    });
    match answers {
        Some(answers) => apply(rows, &answers),
        None => rows,
    }
}

fn is_niconico_row(row: &HolodexStream) -> bool {
    row.stream_type == "placeholder"
        && live_id_from_link(row.link.as_deref().unwrap_or("")).is_some()
}

/// A live or scheduled listing updates that channel's Niconico row, or adds
/// one; idle drops that channel's Niconico rows. Unasked channels keep
/// Holodex's rows as they are.
fn apply(mut rows: Vec<HolodexStream>, answers: &Answers) -> Vec<HolodexStream> {
    for entry in &answers.roster {
        let Some(listing) = answers.listings.get(&entry.channel_id) else {
            continue;
        };
        match listing {
            ChannelLiveListing::Idle => {
                rows.retain(|row| !(is_niconico_row(row) && row.channel.id == entry.youtube_id));
            }
            ChannelLiveListing::OnAir(program) => {
                upsert(&mut rows, entry, program, "live");
            }
            ChannelLiveListing::Scheduled(program) => {
                upsert(&mut rows, entry, program, "upcoming");
            }
        }
    }
    rows
}

fn upsert(
    rows: &mut Vec<HolodexStream>,
    entry: &RosterChannel,
    program: &ChannelProgram,
    status: &str,
) {
    let want = program.live_id.as_str();
    rows.retain(|row| {
        if !is_niconico_row(row) || row.channel.id != entry.youtube_id {
            return true;
        }
        row.status == status
            && live_id_from_link(row.link.as_deref().unwrap_or("")).as_deref() == Some(want)
    });
    if let Some(row) = rows
        .iter_mut()
        .find(|row| is_niconico_row(row) && row.channel.id == entry.youtube_id)
    {
        if let Some(title) = program.title.as_deref().filter(|title| !title.is_empty()) {
            row.title = title.to_string();
        }
        if let Some(start) = start_rfc3339(program) {
            row.start_scheduled = Some(start.clone());
            if status == "live" {
                row.start_actual = Some(start.clone());
            }
            row.available_at = Some(start);
        }
        if let Some(thumbnail) = program
            .thumbnail
            .as_deref()
            .filter(|thumbnail| !thumbnail.is_empty())
        {
            row.thumbnail = Some(thumbnail.to_string());
        }
    } else {
        rows.push(new_row(entry, program, status, rows));
    }
}

fn start_rfc3339(program: &ChannelProgram) -> Option<String> {
    program.start_at.as_ref().map(|dt| dt.to_rfc3339())
}

fn new_row(
    entry: &RosterChannel,
    program: &ChannelProgram,
    status: &str,
    rows: &[HolodexStream],
) -> HolodexStream {
    let known = rows
        .iter()
        .find(|row| row.channel.id == entry.youtube_id)
        .map(|row| row.channel.clone());
    let channel = HolodexChannel {
        id: entry.youtube_id.clone(),
        name: known
            .as_ref()
            .map(|channel| channel.name.clone())
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| entry.name.clone()),
        photo: known
            .and_then(|channel| channel.photo)
            .filter(|photo| !photo.is_empty()),
    };
    let start = start_rfc3339(program);
    HolodexStream {
        id: format!("niconico-{}", program.live_id),
        title: program.title.clone().unwrap_or_default(),
        stream_type: "placeholder".to_string(),
        topic_id: None,
        published_at: None,
        available_at: start.clone(),
        status: status.to_string(),
        start_scheduled: start.clone(),
        start_actual: start.filter(|_| status == "live"),
        live_viewers: None,
        channel,
        link: Some(watch_url(&program.live_id)),
        thumbnail: program.thumbnail.clone(),
        placeholder_type: Some("external-stream".to_string()),
        yt_confirmed: false,
    }
}

static WORKER_STARTED: AtomicBool = AtomicBool::new(false);

pub struct NiconicoLiveWorker(tokio::task::JoinHandle<()>);

impl Drop for NiconicoLiveWorker {
    fn drop(&mut self) {
        self.0.abort();
        WORKER_STARTED.store(false, Ordering::SeqCst);
    }
}

/// Asks listing pages when due, checking every `TICK`.
pub fn start_niconico_live_worker() -> Option<NiconicoLiveWorker> {
    if WORKER_STARTED.swap(true, Ordering::SeqCst) {
        return None;
    }
    Some(NiconicoLiveWorker(tokio::spawn(async {
        loop {
            match load_config().await.map_err(|e| e.to_string()) {
                Ok(cfg) => poll_pass(&cfg).await,
                Err(e) => tracing::debug!("Niconico 直播状态查询读取配置失败: {}", e),
            }
            tokio::select! {
                _ = WAKE.notified() => {}
                _ = tokio::time::sleep(TICK) => {}
            }
        }
    })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{FixedOffset, TimeZone};

    fn jst() -> FixedOffset {
        FixedOffset::east_opt(9 * 3600).unwrap()
    }

    fn program(live_id: &str, title: &str, start: bool) -> ChannelProgram {
        ChannelProgram {
            live_id: live_id.to_string(),
            title: Some(title.to_string()),
            start_at: start.then(|| {
                jst()
                    .with_ymd_and_hms(2026, 9, 27, 20, 0, 0)
                    .single()
                    .unwrap()
                    .with_timezone(&chrono::Local)
            }),
            thumbnail: None,
        }
    }

    fn vspo() -> RosterChannel {
        RosterChannel {
            channel_id: "vspo".to_string(),
            youtube_id: "UCvspo".to_string(),
            name: "ぶいすぽっ!【公式】".to_string(),
        }
    }

    fn row(id: &str, status: &str, link: Option<&str>) -> HolodexStream {
        HolodexStream {
            id: id.to_string(),
            title: "holodex title".to_string(),
            stream_type: if link.is_some() {
                "placeholder".to_string()
            } else {
                "stream".to_string()
            },
            topic_id: None,
            published_at: None,
            available_at: None,
            status: status.to_string(),
            start_scheduled: None,
            start_actual: None,
            live_viewers: None,
            channel: HolodexChannel {
                id: "UCvspo".to_string(),
                name: "ぶいすぽ holodex".to_string(),
                photo: Some("https://yt3.ggpht.com/vspo".to_string()),
            },
            link: link.map(str::to_string),
            thumbnail: None,
            placeholder_type: link.map(|_| "external-stream".to_string()),
            yt_confirmed: false,
        }
    }

    fn answers(listing: ChannelLiveListing) -> Answers {
        Answers {
            listings: [("vspo".to_string(), listing)].into_iter().collect(),
            roster: vec![vspo()],
        }
    }

    fn keep(asked: &[String]) -> HashSet<String> {
        asked.iter().cloned().collect()
    }

    fn program_start() -> DateTime<Local> {
        jst()
            .with_ymd_and_hms(2026, 9, 27, 20, 0, 0)
            .single()
            .unwrap()
            .with_timezone(&Local)
    }

    #[test]
    fn a_live_channel_the_list_lacks_gets_a_niconico_row() {
        let rows = apply(
            vec![row("yt1", "upcoming", None)],
            &answers(ChannelLiveListing::OnAir(program(
                "lv351182284",
                "激ロー",
                true,
            ))),
        );
        assert_eq!(rows.len(), 2);
        let added = &rows[1];
        assert_eq!(added.id, "niconico-lv351182284");
        assert_eq!(added.status, "live");
        assert_eq!(
            added.link.as_deref(),
            Some("https://live.nicovideo.jp/watch/lv351182284")
        );
        assert_eq!(added.channel.id, "UCvspo");
        assert_eq!(added.channel.name, "ぶいすぽ holodex");
        assert!(added.start_actual.is_some());
    }

    #[test]
    fn a_listing_thumbnail_is_copied_onto_the_niconico_row() {
        let mut listing = program("lv351182284", "激ロー", true);
        listing.thumbnail = Some(
            "https://listing-thumbnail.live.nicovideo.jp?image=prod-lv351182284/t.jpg&w=640&h=360"
                .to_string(),
        );
        let rows = apply(Vec::new(), &answers(ChannelLiveListing::OnAir(listing)));
        assert_eq!(
            rows[0].thumbnail.as_deref(),
            Some(
                "https://listing-thumbnail.live.nicovideo.jp?image=prod-lv351182284/t.jpg&w=640&h=360"
            )
        );
    }

    #[test]
    fn a_scheduled_channel_adds_an_upcoming_row() {
        let rows = apply(
            Vec::new(),
            &answers(ChannelLiveListing::Scheduled(program(
                "lv351230205",
                "TOYBOX",
                true,
            ))),
        );
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].status, "upcoming");
        assert_eq!(rows[0].id, "niconico-lv351230205");
        assert_eq!(rows[0].channel.name, "ぶいすぽっ!【公式】");
        assert!(rows[0].start_actual.is_none());
        assert!(rows[0].start_scheduled.is_some());
    }

    #[test]
    fn a_live_listing_updates_holodex_s_row_instead_of_adding_one() {
        let rows = apply(
            vec![row(
                "placeholder-1",
                "live",
                Some("https://live.nicovideo.jp/watch/lv351182284"),
            )],
            &answers(ChannelLiveListing::OnAir(program(
                "lv351182284",
                "new title",
                false,
            ))),
        );
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, "placeholder-1");
        assert_eq!(rows[0].title, "new title");
    }

    #[test]
    fn idle_drops_only_that_channel_s_niconico_rows() {
        let rows = apply(
            vec![
                row("live", "live", Some("https://live.nicovideo.jp/watch/lv1")),
                row(
                    "tonight",
                    "upcoming",
                    Some("https://live.nicovideo.jp/watch/lv2"),
                ),
                row("yt", "live", None),
                row("tw", "live", Some("https://twitch.tv/someone")),
            ],
            &answers(ChannelLiveListing::Idle),
        );
        let ids: Vec<&str> = rows.iter().map(|row| row.id.as_str()).collect();
        assert_eq!(ids, ["yt", "tw"]);
    }

    #[test]
    fn without_answers_holodex_stands() {
        let original = vec![row(
            "live",
            "live",
            Some("https://live.nicovideo.jp/watch/lv1"),
        )];
        let rows = apply(
            original.clone(),
            &Answers {
                listings: HashMap::new(),
                roster: vec![vspo()],
            },
        );
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].title, original[0].title);
    }

    #[test]
    fn going_live_replaces_the_upcoming_row() {
        let rows = apply(
            vec![row(
                "niconico-lvold",
                "upcoming",
                Some("https://live.nicovideo.jp/watch/lvold"),
            )],
            &answers(ChannelLiveListing::OnAir(program("lvnew", "now", false))),
        );
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, "niconico-lvnew");
        assert_eq!(rows[0].status, "live");
    }

    #[test]
    fn the_roster_keeps_channels_that_also_name_a_youtube_channel() {
        let roster = parse_roster(
            r#"{"channels": [
                {"name": "ぶいすぽっ!【公式】", "platforms": {"youtube": "UCvspo", "niconico": " vspo "}},
                {"name": "niconico only", "platforms": {"niconico": "ch123"}},
                {"name": "youtube only", "platforms": {"youtube": "UCyt"}},
                {"name": "dup", "platforms": {"youtube": "UCdup", "niconico": "vspo"}}
            ]}"#,
        );
        assert_eq!(roster, vec![vspo()]);
    }

    #[test]
    fn the_target_is_asked_only_while_its_monitor_is_on() {
        assert_eq!(
            target_channel(true, " https://ch.nicovideo.jp/vspo ").as_deref(),
            Some("vspo")
        );
        assert_eq!(target_channel(false, "vspo"), None);
        assert_eq!(target_channel(true, ""), None);
        assert_eq!(target_channel(true, "  "), None);
    }

    #[test]
    fn idle_and_far_schedules_are_asked_once_a_day() {
        let start = program_start();
        assert_eq!(next_gap(&ChannelLiveListing::Idle, start), DAY);
        assert_eq!(
            next_gap(
                &ChannelLiveListing::Scheduled(program("lv1", "t", false)),
                start
            ),
            DAY,
            "scheduled without a start time"
        );
        assert_eq!(
            next_gap(
                &ChannelLiveListing::Scheduled(program("lv1", "t", true)),
                start - chrono::Duration::hours(48)
            ),
            DAY
        );
    }

    #[test]
    fn a_schedule_is_asked_when_the_ten_minute_window_opens() {
        let start = program_start();
        let now = start - chrono::Duration::minutes(11);
        assert_eq!(
            next_gap(
                &ChannelLiveListing::Scheduled(program("lv1", "t", true)),
                now
            ),
            Duration::from_secs(60)
        );
    }

    #[test]
    fn near_start_and_on_air_are_probed_every_five_minutes() {
        let start = program_start();
        let scheduled = ChannelLiveListing::Scheduled(program("lv1", "t", true));
        assert_eq!(
            next_gap(&scheduled, start - chrono::Duration::minutes(10)),
            LIVE_PROBE
        );
        assert_eq!(
            next_gap(&scheduled, start - chrono::Duration::minutes(3)),
            LIVE_PROBE
        );
        assert_eq!(
            next_gap(&scheduled, start + chrono::Duration::minutes(2)),
            LIVE_PROBE,
            "still scheduled after the posted start"
        );
        assert_eq!(
            next_gap(&ChannelLiveListing::OnAir(program("lv1", "t", true)), start),
            LIVE_PROBE
        );
    }

    #[test]
    fn unanswered_channels_are_asked_sooner_and_failures_retry_in_five_minutes() {
        let t0 = Instant::now();
        let now = program_start();
        let s = Duration::from_secs;
        assert!(channel_due(None, None, false, t0, now));
        assert!(!channel_due(Some(t0), None, false, t0 + s(9), now));
        assert!(channel_due(Some(t0), None, false, t0 + s(10), now));
        assert!(!channel_due(
            Some(t0),
            Some(&ChannelLiveListing::Idle),
            false,
            t0 + DAY - s(1),
            now
        ));
        assert!(channel_due(
            Some(t0),
            Some(&ChannelLiveListing::Idle),
            false,
            t0 + DAY,
            now
        ));
        assert!(!channel_due(Some(t0), None, true, t0 + s(299), now));
        assert!(channel_due(Some(t0), None, true, t0 + LIVE_PROBE, now));
    }

    #[test]
    fn the_monitor_reuses_a_fresh_idle_listing() {
        let t0 = Instant::now();
        let now = program_start();
        let mut state = PollState::default();
        state
            .answers
            .listings
            .insert("vspo".to_string(), ChannelLiveListing::Idle);
        state.last_attempt.insert("vspo".to_string(), t0);
        assert_eq!(
            listing_if_fresh(&state, "vspo", t0 + Duration::from_secs(60), now),
            Some(ChannelLiveListing::Idle)
        );
        assert_eq!(listing_if_fresh(&state, "vspo", t0 + DAY, now), None);
    }

    #[test]
    fn status_from_listing_matches_the_channel_page() {
        assert!(!status_from_listing(&ChannelLiveListing::Idle).0);
        let live = status_from_listing(&ChannelLiveListing::OnAir(program(
            "lv351182284",
            "激ロー",
            true,
        )));
        assert!(live.0);
        assert_eq!(live.2.as_deref(), Some("激ロー"));
        assert_eq!(live.5.as_deref(), Some("lv351182284"));
        let scheduled = status_from_listing(&ChannelLiveListing::Scheduled(program(
            "lv351230205",
            "TOYBOX",
            true,
        )));
        assert!(!scheduled.0);
        assert!(scheduled.4.is_some());
    }

    #[test]
    fn only_the_target_turning_live_wakes_the_monitor() {
        let t0 = Instant::now();
        let target = Some("vspo");
        let mut state = PollState::default();
        let asked = ["vspo".to_string()];
        let keep = keep(&asked);
        let woken = |state: &mut PollState, listing| {
            state
                .record(
                    [("vspo".to_string(), listing)].into_iter().collect(),
                    vec![vspo()],
                    &asked,
                    &keep,
                    target,
                    t0,
                )
                .0
        };
        assert_eq!(
            woken(
                &mut state,
                ChannelLiveListing::OnAir(program("lv1", "t", false))
            )
            .as_deref(),
            Some("vspo"),
            "a first answer counts"
        );
        assert_eq!(
            woken(
                &mut state,
                ChannelLiveListing::OnAir(program("lv1", "t", false))
            ),
            None,
            "still live"
        );
        assert_eq!(woken(&mut state, ChannelLiveListing::Idle), None);
        assert_eq!(
            woken(
                &mut state,
                ChannelLiveListing::OnAir(program("lv2", "t", false))
            )
            .as_deref(),
            Some("vspo")
        );

        wake_monitor("vspo");
        assert!(take_monitor_wake("https://ch.nicovideo.jp/vspo"));
        assert!(!take_monitor_wake("vspo"), "taking clears it");
    }

    #[test]
    fn a_failed_channel_keeps_its_last_listing() {
        let t0 = Instant::now();
        let mut state = PollState::default();
        let asked = ["vspo".to_string()];
        let keep = keep(&asked);
        state.record(
            [(
                "vspo".to_string(),
                ChannelLiveListing::OnAir(program("lv1", "t", false)),
            )]
            .into_iter()
            .collect(),
            vec![vspo()],
            &asked,
            &keep,
            None,
            t0,
        );
        let (woken, changed) = state.record(HashMap::new(), vec![vspo()], &asked, &keep, None, t0);
        assert!(woken.is_none());
        assert!(!changed);
        assert!(matches!(
            state.answers.listings.get("vspo"),
            Some(ChannelLiveListing::OnAir(_))
        ));
    }

    #[test]
    fn an_unasked_channel_keeps_its_listing() {
        let t0 = Instant::now();
        let mut state = PollState::default();
        let vspo_asked = ["vspo".to_string()];
        let other = RosterChannel {
            channel_id: "ch2".to_string(),
            youtube_id: "UCch2".to_string(),
            name: "other".to_string(),
        };
        let roster = vec![vspo(), other.clone()];
        let keep: HashSet<String> = ["vspo", "ch2"].into_iter().map(str::to_string).collect();
        state.record(
            [(
                "vspo".to_string(),
                ChannelLiveListing::OnAir(program("lv1", "t", false)),
            )]
            .into_iter()
            .collect(),
            roster.clone(),
            &vspo_asked,
            &keep,
            None,
            t0,
        );
        let other_asked = ["ch2".to_string()];
        state.record(
            [("ch2".to_string(), ChannelLiveListing::Idle)]
                .into_iter()
                .collect(),
            roster,
            &other_asked,
            &keep,
            None,
            t0,
        );
        assert!(matches!(
            state.answers.listings.get("vspo"),
            Some(ChannelLiveListing::OnAir(_))
        ));
        assert!(matches!(
            state.answers.listings.get("ch2"),
            Some(ChannelLiveListing::Idle)
        ));
    }

    /// The active node: its monitor asked about the target while no list was
    /// leased, so the answer came with no roster. Once a list is leased the
    /// roster overlays that answer without waiting for the target to be due.
    #[test]
    fn a_leased_pass_overlays_the_target_s_fresh_listing_with_nothing_due() {
        let t0 = Instant::now();
        let now_local = program_start() - chrono::Duration::hours(48);
        let mut state = PollState::default();
        let asked = ["vspo".to_string()];
        state.last_attempt.insert("vspo".to_string(), t0);
        state.record(
            [(
                "vspo".to_string(),
                ChannelLiveListing::Scheduled(program("lv1", "激ロー", true)),
            )]
            .into_iter()
            .collect(),
            Vec::new(),
            &asked,
            &keep(&asked),
            Some("vspo"),
            t0,
        );
        assert!(apply(Vec::new(), &state.answers).is_empty());

        let later = t0 + Duration::from_secs(60);
        let (due, adopted) = state.begin_pass(&asked, &[vspo()], later, now_local);
        assert!(due.is_empty(), "the target's listing is still fresh");
        assert!(adopted);
        assert_eq!(state.last_attempt.get("vspo"), Some(&t0));
        let rows = apply(Vec::new(), &state.answers);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, "niconico-lv1");
        assert_eq!(rows[0].status, "upcoming");
        assert_eq!(rows[0].channel.id, "UCvspo");

        assert_eq!(
            state.begin_pass(&asked, &[vspo()], later, now_local),
            (Vec::new(), false),
            "the same roster does not rebuild the lists every tick"
        );
        assert_eq!(
            state.begin_pass(&asked, &[], later, now_local),
            (Vec::new(), false),
            "an unleased pass leaves the roster alone"
        );
        assert_eq!(state.answers.roster, vec![vspo()]);
    }

    #[test]
    fn a_pass_marks_only_due_channels_attempted() {
        let t0 = Instant::now();
        let now_local = program_start() - chrono::Duration::hours(48);
        let mut state = PollState::default();
        state.last_attempt.insert("vspo".to_string(), t0);
        state
            .answers
            .listings
            .insert("vspo".to_string(), ChannelLiveListing::Idle);
        let other = RosterChannel {
            channel_id: "ch2".to_string(),
            youtube_id: "UCch2".to_string(),
            name: "other".to_string(),
        };
        let ids = ["vspo".to_string(), "ch2".to_string()];
        let later = t0 + Duration::from_secs(60);
        let (due, adopted) = state.begin_pass(&ids, &[vspo(), other], later, now_local);
        assert_eq!(due, ["ch2".to_string()]);
        assert!(adopted);
        assert_eq!(state.last_attempt.get("vspo"), Some(&t0));
        assert_eq!(state.last_attempt.get("ch2"), Some(&later));
    }
}
