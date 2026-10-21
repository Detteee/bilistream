//! Twitch liveness from one batched GQL query per `POLL_EVERY`.
//!
//! Holodex notices a Twitch stream minutes late. While a stream list is
//! leased, the worker asks about every roster login, and the lists correct
//! Holodex's Twitch items with the answers (`overlay`). While the Twitch monitor
//! is on, it asks about the target too, whose turn to live ends the monitor's
//! wait (`take_monitor_wake`), as YouTube's go-live does. GQL needs no key and
//! has no quota, so every node asks for itself.

use super::twitch::channel_id_from_url;
use std::collections::{HashMap, HashSet};
use std::error::Error;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use serde_json::json;
use tokio::sync::Notify;

use super::stream_item::{StreamChannel, StreamItem};
use crate::config::{load_config, Config};

const POLL_EVERY: Duration = Duration::from_secs(30);
/// A login no answer covers yet (a list just leased, a roster edit) is asked
/// this soon instead.
const NEW_LOGIN_GAP: Duration = Duration::from_secs(10);
/// Older answers no longer correct Holodex.
const FRESH_FOR: Duration = Duration::from_secs(90);
const TICK: Duration = Duration::from_secs(10);
/// GQL's `users(logins:)` takes at most 100 logins.
const LOGINS_PER_QUERY: usize = 100;

const LIVE_QUERY: &str = r#"
query GetLive($logins: [String!]) {
    users(logins: $logins) {
        login
        profileImageURL(width: 70)
        stream {
            id
            title
            type
            viewersCount
            createdAt
            game { name }
            previewImageURL(width: 640, height: 360)
        }
    }
}"#;

/// A live Twitch stream, as GQL reports it.
#[derive(Debug, Clone, PartialEq)]
struct TwitchLive {
    stream_id: String,
    title: String,
    game: Option<String>,
    viewers: Option<i32>,
    started_at: Option<String>,
    preview: Option<String>,
    avatar: Option<String>,
}

/// A channels.json entry with both a YouTube channel and a Twitch login. The
/// lists key items by the YouTube channel ID, as Holodex does.
#[derive(Debug, Clone, PartialEq)]
struct RosterLogin {
    login: String,
    youtube_id: String,
    name: String,
}

/// Per asked login, its live stream or `None` (offline or unknown), and the
/// roster entries the lists read them for.
#[derive(Debug, Default, Clone)]
struct Answers {
    live: HashMap<String, Option<TwitchLive>>,
    roster: Vec<RosterLogin>,
}

#[derive(Default)]
struct PollState {
    attempt: Option<Instant>,
    answered_at: Option<Instant>,
    answers: Answers,
    /// Warn once per failure streak, then debug.
    failing: bool,
}

impl PollState {
    /// Keeps a successful answer. Returns the target login when it turned live
    /// (its last answer was missing or offline), and whether the lists read
    /// these answers and they changed.
    fn record(
        &mut self,
        answers: Answers,
        target: Option<&str>,
        now: Instant,
    ) -> (Option<String>, bool) {
        let is_live =
            |answers: &Answers, login: &str| matches!(answers.live.get(login), Some(Some(_)));
        let woken = target
            .filter(|login| is_live(&answers, login) && !is_live(&self.answers, login))
            .map(str::to_string);
        let changed = !answers.roster.is_empty()
            && (answers.live != self.answers.live || answers.roster != self.answers.roster);
        self.answers = answers;
        self.answered_at = Some(now);
        (woken, changed)
    }
}

static STATE: LazyLock<Mutex<PollState>> = LazyLock::new(|| Mutex::new(PollState::default()));
/// `notify_one` keeps one permit while the worker is busy.
static WAKE: Notify = Notify::const_new();

/// Target logins whose go-live no monitor pass picked up yet.
static MONITOR_WAKES: Mutex<Option<HashSet<String>>> = Mutex::new(None);

fn with_state<T>(f: impl FnOnce(&mut PollState) -> T) -> T {
    let mut guard = STATE.lock().unwrap_or_else(|e| e.into_inner());
    f(&mut guard)
}

fn normalize(login: &str) -> String {
    login.trim().to_ascii_lowercase()
}

fn row_login(item: &StreamItem) -> Option<String> {
    if item.stream_type != "placeholder" {
        return None;
    }
    channel_id_from_url(item.link.as_deref()?)
}

fn wake_monitor(login: &str) {
    let mut guard = MONITOR_WAKES.lock().unwrap_or_else(|e| e.into_inner());
    guard
        .get_or_insert_with(HashSet::new)
        .insert(normalize(login));
}

/// Whether the target login has a go-live wake waiting; taking it clears it.
pub fn take_monitor_wake(login: &str) -> bool {
    let mut guard = MONITOR_WAKES.lock().unwrap_or_else(|e| e.into_inner());
    guard
        .as_mut()
        .is_some_and(|wakes| wakes.remove(&normalize(login)))
}

/// The target login while the Twitch monitor is on.
fn target_login(enable_monitor: bool, channel_id: &str) -> Option<String> {
    let login = normalize(channel_id);
    (enable_monitor && !login.is_empty()).then_some(login)
}

/// Ask GQL now (or as soon as the worker is free). A newly leased list calls
/// this so the roster is covered without waiting for the next tick.
pub(crate) fn wake() {
    WAKE.notify_one();
}

/// The roster's Twitch logins that also name a YouTube channel.
async fn roster_logins() -> Vec<RosterLogin> {
    match crate::storage::read_document::<serde_json::Value>("channels.json") {
        Ok(json) => roster_from_value(&json),
        Err(_) => Vec::new(),
    }
}

#[cfg(test)]
fn parse_roster(content: &str) -> Vec<RosterLogin> {
    let Ok(json) = serde_json::from_str::<serde_json::Value>(content) else {
        return Vec::new();
    };
    roster_from_value(&json)
}

fn roster_from_value(json: &serde_json::Value) -> Vec<RosterLogin> {
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
            let login = normalize(platform("twitch")?);
            let youtube_id = platform("youtube")?.to_string();
            let name = channel.get("name").and_then(|v| v.as_str()).unwrap_or("");
            seen.insert(login.clone()).then(|| RosterLogin {
                login,
                youtube_id,
                name: name.to_string(),
            })
        })
        .collect()
}

fn parse_live(
    response: &serde_json::Value,
    asked: &[String],
) -> HashMap<String, Option<TwitchLive>> {
    // Unknown logins come back as null, so every asked login starts offline.
    let mut live: HashMap<String, Option<TwitchLive>> =
        asked.iter().map(|login| (login.clone(), None)).collect();
    let users = response["data"]["users"].as_array().into_iter().flatten();
    for user in users {
        let Some(login) = user["login"].as_str().map(normalize) else {
            continue;
        };
        let stream = &user["stream"];
        // The monitor's own check: a rerun is not live.
        if stream["type"] != "live" {
            continue;
        }
        let text = |value: &serde_json::Value| value.as_str().map(str::to_string);
        live.insert(
            login,
            Some(TwitchLive {
                stream_id: text(&stream["id"]).unwrap_or_default(),
                title: text(&stream["title"]).unwrap_or_default(),
                game: text(&stream["game"]["name"]).filter(|g| !g.is_empty()),
                viewers: stream["viewersCount"]
                    .as_i64()
                    .and_then(|v| i32::try_from(v).ok()),
                started_at: text(&stream["createdAt"]),
                preview: text(&stream["previewImageURL"]),
                avatar: text(&user["profileImageURL"]),
            }),
        );
    }
    live
}

async fn fetch_live(
    logins: &[String],
) -> Result<HashMap<String, Option<TwitchLive>>, Box<dyn Error>> {
    let client = super::http::pooled_client(None)?;
    let mut live = HashMap::with_capacity(logins.len());
    for chunk in logins.chunks(LOGINS_PER_QUERY) {
        let response = client
            .post("https://gql.twitch.tv/gql")
            .timeout(Duration::from_secs(15))
            .header("Client-ID", super::twitch::GQL_CLIENT_ID)
            .json(&json!({ "query": LIVE_QUERY, "variables": { "logins": chunk } }))
            .send()
            .await?;
        let body: serde_json::Value =
            super::http::response_json_limited(response.error_for_status()?).await?;
        if body
            .get("errors")
            .and_then(|errors| errors.as_array())
            .is_some_and(|errors| !errors.is_empty())
        {
            return Err("Twitch GraphQL returned errors".into());
        }
        live.extend(parse_live(&body, chunk));
    }
    Ok(live)
}

fn poll_due(attempt: Option<Instant>, new_login: bool, now: Instant) -> bool {
    let Some(at) = attempt else {
        return true;
    };
    let since = now.saturating_duration_since(at);
    since >= POLL_EVERY || (new_login && since >= NEW_LOGIN_GAP)
}

/// Asks GQL when due: about the roster while a list is leased, and about the
/// target while its monitor is on.
async fn poll_pass(cfg: &Config) {
    let target = target_login(cfg.twitch.enable_monitor, &cfg.twitch.channel_id);
    let roster = if crate::webui::stream_list::any_leased() {
        roster_logins().await
    } else {
        Vec::new()
    };
    let mut seen = HashSet::new();
    let logins: Vec<String> = roster
        .iter()
        .map(|entry| entry.login.clone())
        .chain(target.clone())
        .filter(|login| seen.insert(login.clone()))
        .collect();
    if logins.is_empty() {
        return;
    }
    let now = Instant::now();
    let due = with_state(|state| {
        let new_login = logins
            .iter()
            .any(|login| !state.answers.live.contains_key(login));
        let due = poll_due(state.attempt, new_login, now);
        if due {
            state.attempt = Some(now);
        }
        due
    });
    if !due {
        return;
    }
    // Box<dyn Error> is not Send: reduce it before the next await.
    let fetched = fetch_live(&logins).await.map_err(|e| e.to_string());
    let (woken, changed) = with_state(|state| match fetched {
        Ok(live) => {
            if state.failing {
                tracing::info!("Twitch 直播状态查询恢复");
            }
            state.failing = false;
            state.record(Answers { live, roster }, target.as_deref(), now)
        }
        Err(e) => {
            if state.failing {
                tracing::debug!("Twitch 直播状态查询失败：{}", e);
            } else {
                tracing::warn!(
                    "Twitch 直播状态查询失败，列表沿用 Holodex 的 Twitch 状态：{}",
                    e
                );
            }
            state.failing = true;
            (None, false)
        }
    });
    if let Some(login) = woken {
        wake_monitor(&login);
    }
    if changed {
        crate::webui::stream_list::wake();
    }
}

/// The list's items corrected by the answers (`apply`), or as they are when GQL
/// has not answered within `FRESH_FOR`.
pub(crate) fn overlay(items: Vec<StreamItem>) -> Vec<StreamItem> {
    let now = Instant::now();
    let answers = with_state(|state| {
        let fresh = state
            .answered_at
            .is_some_and(|at| now.saturating_duration_since(at) < FRESH_FOR);
        fresh.then(|| state.answers.clone())
    });
    match answers {
        Some(answers) => apply(items, &answers),
        None => items,
    }
}

/// A live answer updates the login's live item, or adds one while the backstop
/// has not noticed the stream; an offline answer drops the login's live items.
/// Unasked logins keep their items as they are.
fn apply(mut items: Vec<StreamItem>, answers: &Answers) -> Vec<StreamItem> {
    items.retain(|item| {
        item.status != "live"
            || !row_login(item).is_some_and(|login| matches!(answers.live.get(&login), Some(None)))
    });
    for entry in &answers.roster {
        let Some(Some(stream)) = answers.live.get(&entry.login) else {
            continue;
        };
        let existing = items
            .iter_mut()
            .find(|item| item.status == "live" && row_login(item).as_deref() == Some(&entry.login));
        match existing {
            Some(item) => {
                if !stream.title.is_empty() {
                    item.title = stream.title.clone();
                }
                if stream.started_at.is_some() {
                    item.start_actual = stream.started_at.clone();
                }
                item.live_viewers = stream.viewers.or(item.live_viewers);
                item.topic_id = item.topic_id.take().or_else(|| stream.game.clone());
                item.thumbnail = item.thumbnail.take().or_else(|| stream.preview.clone());
            }
            None => {
                let item = new_item(entry, stream, &items);
                items.push(item);
            }
        }
    }
    items
}

fn new_item(entry: &RosterLogin, stream: &TwitchLive, items: &[StreamItem]) -> StreamItem {
    // Holodex's name and photo for the channel, when it lists any other item.
    let known = items
        .iter()
        .find(|item| item.channel.id == entry.youtube_id)
        .map(|item| item.channel.clone());
    let channel = StreamChannel {
        id: entry.youtube_id.clone(),
        name: known
            .as_ref()
            .map(|channel| channel.name.clone())
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| entry.name.clone()),
        photo: known
            .and_then(|channel| channel.photo)
            .filter(|photo| !photo.is_empty())
            .or_else(|| stream.avatar.clone()),
    };
    StreamItem {
        id: format!("twitch-{}", stream.stream_id),
        title: stream.title.clone(),
        stream_type: "placeholder".to_string(),
        topic_id: stream.game.clone(),
        published_at: None,
        available_at: stream.started_at.clone(),
        status: "live".to_string(),
        start_scheduled: stream.started_at.clone(),
        start_actual: stream.started_at.clone(),
        live_viewers: stream.viewers,
        channel,
        link: Some(format!("https://twitch.tv/{}", entry.login)),
        thumbnail: stream.preview.clone(),
        placeholder_type: Some("external-stream".to_string()),
        yt_confirmed: false,
    }
}

static WORKER_STARTED: AtomicBool = AtomicBool::new(false);

pub struct TwitchLiveWorker(tokio::task::JoinHandle<()>);

impl Drop for TwitchLiveWorker {
    fn drop(&mut self) {
        self.0.abort();
        WORKER_STARTED.store(false, Ordering::SeqCst);
    }
}

/// Asks GQL when due, checking every `TICK`.
pub fn start_twitch_live_worker() -> Option<TwitchLiveWorker> {
    if WORKER_STARTED.swap(true, Ordering::SeqCst) {
        return None;
    }
    Some(TwitchLiveWorker(tokio::spawn(async {
        loop {
            // Box<dyn Error> is not Send: reduce it before the next await.
            match load_config().await.map_err(|e| e.to_string()) {
                Ok(cfg) => poll_pass(&cfg).await,
                Err(e) => tracing::debug!("Twitch 直播状态查询读取配置失败：{}", e),
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

    fn live(title: &str) -> TwitchLive {
        TwitchLive {
            stream_id: "316566340211".to_string(),
            title: title.to_string(),
            game: Some("Teamfight Tactics".to_string()),
            viewers: Some(1413),
            started_at: Some("2026-09-26T14:02:07Z".to_string()),
            preview: Some(
                "https://static-cdn.jtvnw.net/previews-ttv/live_user_urs_toko-640x360.jpg"
                    .to_string(),
            ),
            avatar: Some(
                "https://static-cdn.jtvnw.net/jtv_user_pictures/toko-70x70.png".to_string(),
            ),
        }
    }

    fn toko() -> RosterLogin {
        RosterLogin {
            login: "urs_toko".to_string(),
            youtube_id: "UCtoko".to_string(),
            name: "とおこ".to_string(),
        }
    }

    fn item(id: &str, status: &str, link: Option<&str>) -> StreamItem {
        StreamItem {
            id: id.to_string(),
            title: "holodex title".to_string(),
            stream_type: if link.is_some() {
                "placeholder"
            } else {
                "stream"
            }
            .to_string(),
            topic_id: None,
            published_at: None,
            available_at: None,
            status: status.to_string(),
            start_scheduled: None,
            start_actual: None,
            live_viewers: None,
            channel: StreamChannel {
                id: "UCtoko".to_string(),
                name: "とおこ holodex".to_string(),
                photo: Some("https://yt3.ggpht.com/toko".to_string()),
            },
            link: link.map(str::to_string),
            thumbnail: None,
            placeholder_type: link.map(|_| "external-stream".to_string()),
            yt_confirmed: false,
        }
    }

    fn answers(state: Option<TwitchLive>) -> Answers {
        Answers {
            live: [("urs_toko".to_string(), state)].into_iter().collect(),
            roster: vec![toko()],
        }
    }

    #[test]
    fn a_live_login_the_list_lacks_gets_a_twitch_row() {
        let items = apply(
            vec![item("yt1", "upcoming", None)],
            &answers(Some(live("宿題"))),
        );
        assert_eq!(items.len(), 2);
        let added = &items[1];
        assert_eq!(added.id, "twitch-316566340211");
        assert_eq!(added.status, "live");
        assert_eq!(added.link.as_deref(), Some("https://twitch.tv/urs_toko"));
        assert_eq!(added.start_actual.as_deref(), Some("2026-09-26T14:02:07Z"));
        assert_eq!(added.topic_id.as_deref(), Some("Teamfight Tactics"));
        // Keyed and named like the backstop's items for the channel.
        assert_eq!(added.channel.id, "UCtoko");
        assert_eq!(added.channel.name, "とおこ holodex");
        assert_eq!(
            added.channel.photo.as_deref(),
            Some("https://yt3.ggpht.com/toko")
        );

        let alone = apply(Vec::new(), &answers(Some(live("宿題"))));
        assert_eq!(alone[0].channel.name, "とおこ");
        assert!(alone[0]
            .channel
            .photo
            .as_deref()
            .unwrap()
            .contains("jtv_user_pictures"));
    }

    #[test]
    fn a_live_login_updates_holodex_s_row_instead_of_adding_one() {
        let items = apply(
            vec![item(
                "UBWcTxPTMD1",
                "live",
                Some("https://twitch.tv/URS_toko"),
            )],
            &answers(Some(live("new title"))),
        );
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].id, "UBWcTxPTMD1");
        assert_eq!(items[0].title, "new title");
        assert_eq!(items[0].live_viewers, Some(1413));
        assert_eq!(
            items[0].start_actual.as_deref(),
            Some("2026-09-26T14:02:07Z")
        );
    }

    #[test]
    fn an_offline_login_drops_only_its_live_rows() {
        let items = apply(
            vec![
                item("ended", "live", Some("https://www.twitch.tv/urs_toko")),
                item("tonight", "upcoming", Some("https://twitch.tv/urs_toko")),
                item("other", "live", Some("https://twitch.tv/someone_else")),
                item("yt", "live", None),
            ],
            &answers(None),
        );
        let ids: Vec<&str> = items.iter().map(|item| item.id.as_str()).collect();
        assert_eq!(ids, ["tonight", "other", "yt"]);
    }

    #[test]
    fn without_answers_holodex_stands() {
        let original = vec![item("ended", "live", Some("https://twitch.tv/urs_toko"))];
        let items = apply(
            original.clone(),
            &Answers {
                live: HashMap::new(),
                roster: vec![toko()],
            },
        );
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].title, original[0].title);
    }

    #[test]
    fn gql_answers_cover_every_asked_login() {
        let body = json!({"data": {"users": [
            {"login": "urs_toko", "profileImageURL": "https://a/p.png", "stream": {
                "id": "316566340211", "title": "宿題", "type": "live", "viewersCount": 1413,
                "createdAt": "2026-09-26T14:02:07Z", "game": {"name": "Teamfight Tactics"},
                "previewImageURL": "https://a/prev.jpg"}},
            null,
            {"login": "kamito_jp", "profileImageURL": "https://a/k.png", "stream": null},
            {"login": "rerun_ch", "stream": {"id": "1", "title": "old", "type": "rerun"}}
        ]}});
        let asked = ["urs_toko", "no_such_user", "kamito_jp", "rerun_ch"].map(str::to_string);
        let live = parse_live(&body, &asked);
        assert_eq!(live.len(), 4);
        let toko = live["urs_toko"].as_ref().unwrap();
        assert_eq!(toko.viewers, Some(1413));
        assert_eq!(toko.game.as_deref(), Some("Teamfight Tactics"));
        assert_eq!(toko.avatar.as_deref(), Some("https://a/p.png"));
        assert!(live["no_such_user"].is_none());
        assert!(live["kamito_jp"].is_none());
        assert!(live["rerun_ch"].is_none(), "a rerun is not live");
    }

    #[test]
    fn the_roster_keeps_logins_that_also_name_a_youtube_channel() {
        let roster = parse_roster(
            r#"{"channels": [
                {"name": "とおこ", "platforms": {"youtube": "UCtoko", "twitch": " URS_toko "}},
                {"name": "twitch only", "platforms": {"twitch": "tw_only"}},
                {"name": "youtube only", "platforms": {"youtube": "UCyt"}},
                {"name": "dup", "platforms": {"youtube": "UCdup", "twitch": "urs_toko"}}
            ]}"#,
        );
        assert_eq!(roster, vec![toko()]);
    }

    #[test]
    fn the_target_is_asked_only_while_its_monitor_is_on() {
        assert_eq!(
            target_login(true, " URS_toko ").as_deref(),
            Some("urs_toko")
        );
        assert_eq!(target_login(false, "urs_toko"), None);
        assert_eq!(target_login(true, ""), None);
        assert_eq!(target_login(true, "  "), None);
    }

    #[test]
    fn polls_follow_the_cadence_and_new_logins_come_sooner() {
        let t0 = Instant::now();
        let s = Duration::from_secs;
        assert!(poll_due(None, false, t0));
        assert!(!poll_due(Some(t0), false, t0 + s(29)));
        assert!(poll_due(Some(t0), false, t0 + s(30)));
        assert!(!poll_due(Some(t0), true, t0 + s(9)));
        assert!(poll_due(Some(t0), true, t0 + s(10)));
    }

    #[test]
    fn only_the_target_turning_live_wakes_the_monitor() {
        let t0 = Instant::now();
        let target = Some("urs_toko");
        let mut state = PollState::default();
        let woken = |state: &mut PollState, answer| state.record(answers(answer), target, t0).0;
        assert_eq!(
            woken(&mut state, Some(live("t"))).as_deref(),
            Some("urs_toko"),
            "a first answer counts"
        );
        assert_eq!(woken(&mut state, Some(live("t"))), None, "still live");
        assert_eq!(woken(&mut state, None), None, "went offline");
        assert_eq!(
            woken(&mut state, Some(live("t"))).as_deref(),
            Some("urs_toko")
        );
        let mut roster_only = PollState::default();
        assert_eq!(
            roster_only.record(answers(Some(live("t"))), None, t0).0,
            None
        );

        wake_monitor("URS_toko");
        assert!(take_monitor_wake("urs_toko"));
        assert!(!take_monitor_wake("urs_toko"), "taking clears it");
    }

    #[test]
    fn lists_rebuild_only_when_the_answers_they_read_change() {
        let t0 = Instant::now();
        let mut state = PollState::default();
        let mut changed = |next: Answers| state.record(next, None, t0).1;
        assert!(changed(answers(Some(live("a")))), "first answers");
        assert!(!changed(answers(Some(live("a")))), "same answers");
        assert!(changed(answers(Some(live("b")))), "new title");
        assert!(changed(answers(None)), "went offline");
        let unread = Answers {
            live: answers(Some(live("a"))).live,
            roster: Vec::new(),
        };
        assert!(!changed(unread), "no list is leased");
    }

    #[test]
    fn links_name_the_login() {
        assert_eq!(
            channel_id_from_url("https://www.twitch.tv/Urs_Toko?x=1").as_deref(),
            Some("urs_toko")
        );
        assert_eq!(
            channel_id_from_url("https://twitch.tv/urs_toko/videos").as_deref(),
            Some("urs_toko")
        );
        assert_eq!(channel_id_from_url("https://youtube.com/watch?v=x"), None);
        assert_eq!(channel_id_from_url("https://twitch.tv/"), None);
    }
    #[test]
    fn overlay_uses_canonical_hosts_and_retains_the_original_link() {
        let link = " HTTPS://WWW.TWITCH.TV/URS_Toko/videos?x=1#live ";
        let items = apply(
            vec![item("existing", "live", Some(link))],
            &answers(Some(live("updated"))),
        );
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].title, "updated");
        assert_eq!(items[0].link.as_deref(), Some(link));
        assert_eq!(
            row_login(&item(
                "unrelated",
                "live",
                Some("https://example.com/twitch.tv/urs_toko")
            )),
            None
        );
    }
}
