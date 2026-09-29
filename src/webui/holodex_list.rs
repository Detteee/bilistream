//! Server-side stream lists for the Holodex panel.
//!
//! Each list (the roster's channels, or the Holodex account's favorites) is
//! rebuilt from the last Holodex rows, the rows discovery found and recently
//! live rows, corrected by the `videos.list` store, Twitch GQL, and Niconico
//! channel listing pages. YouTube, Twitch GQL and Niconico listings drive the
//! updates: discovery, the store, Twitch and Niconico liveness and config
//! commits call `wake()`, and a rebuild that changes the panel's content
//! publishes `events::HOLODEX`. Holodex is a one-minute backstop. A list is
//! kept only while someone reads it (a 10-min lease).

use super::api::{filter_holodex_streams, map_holodex_streams_with_area};
use super::events;
use crate::config::{load_config, Config};
use crate::plugins::holodex::HolodexStream;
use std::collections::hash_map::DefaultHasher;
use std::collections::HashSet;
use std::hash::{Hash, Hasher};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};
use tokio::sync::Notify;

/// A list nobody requested for this long goes idle.
const LEASE: Duration = Duration::from_secs(10 * 60);
/// A leased list is rebuilt at least this often, which keeps its IDs wanted
/// in the store and drops rows past the horizon.
const KEEP_WANTED: Duration = Duration::from_secs(5 * 60);
const HOLODEX_EVERY: Duration = Duration::from_secs(60);
/// Forced and input-change fetches stay at least this far apart.
const FORCE_GAP: Duration = Duration::from_secs(10);
const DEBOUNCE: Duration = Duration::from_secs(1);
const TICK: Duration = Duration::from_secs(10);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum ListKind {
    Channels,
    Favorites,
}

impl ListKind {
    pub(crate) fn source(self) -> &'static str {
        match self {
            ListKind::Channels => "channels",
            ListKind::Favorites => "favorites",
        }
    }
}

/// What a list serves: corrected, filtered rows, before any consumer's mapping.
pub(crate) struct ListSnapshot {
    pub(crate) rows: Vec<HolodexStream>,
    /// When Holodex last answered for this list; `None` before any success.
    pub(crate) holodex_ok_at: Option<Instant>,
    /// The Holodex cadence this build ran on (`holodex_every`).
    pub(crate) holodex_every: Duration,
}

struct HolodexRows {
    rows: Vec<HolodexStream>,
    /// Favorites: the channel IDs Holodex listed as favorites.
    favorites: HashSet<String>,
}

#[derive(Default)]
struct ListState {
    /// Last successful fetch, and when it answered.
    holodex: Option<HolodexRows>,
    holodex_ok_at: Option<Instant>,
    /// Last fetch attempt, successful or not, and its inputs hash.
    attempt: Option<(Instant, u64)>,
    /// The last failed fetch's message, served while there are no rows.
    last_error: Option<String>,
    built_at: Option<Instant>,
    /// The panel mapping last announced over SSE.
    published: Option<serde_json::Value>,
    /// Warn once per failure streak, then debug.
    failing: bool,
}

struct List {
    /// One build at a time.
    state: tokio::sync::Mutex<ListState>,
    /// Readable while a build runs.
    snapshot: std::sync::RwLock<Option<Arc<ListSnapshot>>>,
    leased_until: std::sync::Mutex<Option<Instant>>,
}

impl List {
    fn new() -> Self {
        List {
            state: tokio::sync::Mutex::new(ListState::default()),
            snapshot: std::sync::RwLock::new(None),
            leased_until: std::sync::Mutex::new(None),
        }
    }

    fn snapshot(&self) -> Option<Arc<ListSnapshot>> {
        self.snapshot
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    fn set_snapshot(&self, snapshot: Option<Arc<ListSnapshot>>) {
        *self.snapshot.write().unwrap_or_else(|e| e.into_inner()) = snapshot;
    }

    fn lease(&self) -> Option<Instant> {
        *self.leased_until.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Extends the lease; returns whether it was still running.
    fn extend_lease(&self, now: Instant) -> bool {
        let mut until = self.leased_until.lock().unwrap_or_else(|e| e.into_inner());
        let was = leased(*until, now);
        *until = Some(now + LEASE);
        was
    }
}

static CHANNELS: LazyLock<List> = LazyLock::new(List::new);
static FAVORITES: LazyLock<List> = LazyLock::new(List::new);
/// `notify_one` keeps one permit while the worker is busy.
static WAKE: Notify = Notify::const_new();

fn list(kind: ListKind) -> &'static List {
    match kind {
        ListKind::Channels => &CHANNELS,
        ListKind::Favorites => &FAVORITES,
    }
}

fn holodex_every(_usable_yt: bool) -> Duration {
    HOLODEX_EVERY
}

fn holodex_due(
    attempt: Option<(Instant, u64)>,
    inputs: u64,
    force: bool,
    usable_yt: bool,
    now: Instant,
) -> bool {
    let Some((at, attempted)) = attempt else {
        return true;
    };
    let since = now.saturating_duration_since(at);
    if inputs != attempted || force {
        since >= FORCE_GAP
    } else {
        since >= holodex_every(usable_yt)
    }
}

fn leased(until: Option<Instant>, now: Instant) -> bool {
    until.is_some_and(|until| now < until)
}

fn rebuild_due(woken: bool, holodex_due: bool, built_at: Option<Instant>, now: Instant) -> bool {
    woken
        || holodex_due
        || built_at.is_none_or(|at| now.saturating_duration_since(at) >= KEEP_WANTED)
}

/// Stores `next` and says whether it differs from what was there.
fn replace_if_changed(slot: &mut Option<serde_json::Value>, next: serde_json::Value) -> bool {
    if slot.as_ref() == Some(&next) {
        return false;
    }
    *slot = Some(next);
    true
}

fn holodex_key(cfg: &Config) -> Option<String> {
    cfg.holodex_api_key
        .clone()
        .filter(|key| !key.trim().is_empty())
}

fn holodex_jwt(cfg: &Config) -> Option<String> {
    cfg.holodex_jwt.clone().filter(|jwt| !jwt.is_empty())
}

/// What a Holodex fetch for the list depends on, hashed; `None` when the list
/// cannot fetch at all. Never logged: the favorites hash covers the JWT.
async fn holodex_inputs(kind: ListKind, cfg: &Config) -> Option<u64> {
    let key = holodex_key(cfg)?;
    let mut hasher = DefaultHasher::new();
    key.hash(&mut hasher);
    match kind {
        ListKind::Channels => {
            let mut roster = crate::plugins::youtube_discovery::roster_channel_ids(cfg).await;
            roster.sort();
            roster.hash(&mut hasher);
        }
        ListKind::Favorites => holodex_jwt(cfg)?.hash(&mut hasher),
    }
    Some(hasher.finish())
}

fn note_failure(state: &mut ListState, message: String) {
    if state.failing {
        tracing::debug!("Holodex 列表刷新失败，沿用上次结果: {}", message);
    } else {
        tracing::warn!("Holodex 列表刷新失败，沿用上次结果: {}", message);
    }
    state.failing = true;
    state.last_error = Some(message);
}

fn note_success(state: &mut ListState, rows: HolodexRows) {
    if state.failing {
        tracing::info!("Holodex 列表刷新恢复");
    }
    state.failing = false;
    state.last_error = None;
    state.holodex = Some(rows);
    state.holodex_ok_at = Some(Instant::now());
}

/// Fetches Holodex when due, then returns its rows and the channels the list
/// keeps.
async fn holodex_rows(
    kind: ListKind,
    state: &mut ListState,
    cfg: &mut Config,
    force: bool,
) -> Result<(Vec<HolodexStream>, HashSet<String>), String> {
    let has_yt = crate::plugins::youtube_data::youtube_configured(cfg);
    let usable_yt = crate::plugins::youtube_data::youtube_answers_available(cfg);
    let now = Instant::now();
    let roster = match kind {
        ListKind::Channels => {
            let roster = crate::plugins::youtube_discovery::roster_channel_ids(cfg).await;
            if roster.is_empty() {
                return Err("No YouTube channels configured".to_string());
            }
            if holodex_key(cfg).is_none() && !has_yt {
                return Err("Holodex API key not configured".to_string());
            }
            roster
        }
        ListKind::Favorites => {
            if holodex_key(cfg).is_none() {
                return Err("Holodex API key not configured".to_string());
            }
            if holodex_jwt(cfg).is_none() {
                return Err("Holodex JWT required for favorites mode".to_string());
            }
            Vec::new()
        }
    };

    match holodex_inputs(kind, cfg).await {
        Some(inputs) if holodex_due(state.attempt, inputs, force, usable_yt, now) => {
            state.attempt = Some((now, inputs));
            let fetched = match kind {
                ListKind::Channels => {
                    crate::plugins::holodex::get_holodex_streams(roster.clone(), true)
                        .await
                        .map(|rows| HolodexRows {
                            rows,
                            favorites: HashSet::new(),
                        })
                        .map_err(|e| format!("Failed to fetch from Holodex: {e}"))
                }
                ListKind::Favorites => fetch_favorites(cfg).await,
            };
            match fetched {
                Ok(rows) => note_success(state, rows),
                Err(message) => note_failure(state, message),
            }
        }
        Some(_) => {}
        None => {
            state.holodex = None;
            state.holodex_ok_at = None;
        }
    }

    if state.holodex.is_none() && (kind == ListKind::Favorites || !has_yt) {
        return Err(state
            .last_error
            .clone()
            .unwrap_or_else(|| "Holodex API key not configured".to_string()));
    }
    let (rows, favorites) = state
        .holodex
        .as_ref()
        .map(|holodex| (holodex.rows.clone(), holodex.favorites.clone()))
        .unwrap_or_default();
    let allowed = match kind {
        ListKind::Channels => roster.into_iter().collect(),
        ListKind::Favorites => favorites,
    };
    Ok((rows, allowed))
}

async fn fetch_favorites(cfg: &mut Config) -> Result<HolodexRows, String> {
    let key = holodex_key(cfg).unwrap_or_default();
    let (jwt, _) = super::api::apply_holodex_jwt_sync(cfg)
        .await
        .map_err(|e| format!("Failed to refresh Holodex JWT: {e}"))?;
    let (favorites, rows) = crate::plugins::holodex::get_holodex_favorites_live(&key, &jwt)
        .await
        .map_err(|e| format!("Failed to fetch Holodex favorites: {e}"))?;
    Ok(HolodexRows { rows, favorites })
}

/// Rebuilds the list and publishes `events::HOLODEX` when the panel's content
/// changed.
async fn build(kind: ListKind, force: bool) -> Result<Arc<ListSnapshot>, String> {
    let list = list(kind);
    let mut state = list.state.lock().await;
    let mut cfg = load_config()
        .await
        .map_err(|e| format!("Failed to load config: {e}"))?;
    let (rows, allowed) = match holodex_rows(kind, &mut state, &mut cfg, force).await {
        Ok(found) => found,
        Err(message) => {
            list.set_snapshot(None);
            state.published = None;
            return Err(message);
        }
    };
    let rows = crate::plugins::youtube_discovery::merge_discovered(rows);
    let rows = crate::plugins::youtube_data::apply_youtube_overlay(rows).await;
    let rows = crate::plugins::twitch_live::overlay(rows);
    let rows = crate::plugins::niconico_live::overlay(rows);
    let rows = filter_holodex_streams(rows, allowed);

    // Discovered rows come in hash order; compare by ID so order alone is no change.
    let mut panel = map_holodex_streams_with_area(rows.clone());
    panel.sort_by(|a, b| a.id.cmp(&b.id));
    let panel = serde_json::to_value(panel).unwrap_or_default();
    if replace_if_changed(&mut state.published, panel) {
        events::publish(events::HOLODEX);
    }
    state.built_at = Some(Instant::now());
    let usable_yt = crate::plugins::youtube_data::youtube_answers_available(&cfg);
    let snapshot = Arc::new(ListSnapshot {
        rows,
        holodex_ok_at: state.holodex_ok_at,
        holodex_every: holodex_every(usable_yt),
    });
    list.set_snapshot(Some(snapshot.clone()));
    Ok(snapshot)
}

/// Extends the list's lease and returns its rows. `force` fetches Holodex now
/// (at most once per `FORCE_GAP`); an idle or empty list is built first.
pub(crate) async fn current(kind: ListKind, force: bool) -> Result<Arc<ListSnapshot>, String> {
    let list = list(kind);
    let was_leased = list.extend_lease(Instant::now());
    if !was_leased {
        crate::plugins::twitch_live::wake();
        crate::plugins::niconico_live::wake();
    }
    if !force && was_leased {
        if let Some(snapshot) = list.snapshot() {
            return Ok(snapshot);
        }
    }
    build(kind, force).await
}

/// An input changed: rebuild the leased lists shortly.
pub(crate) fn wake() {
    WAKE.notify_one();
}

/// Whether either list is still within its lease; Twitch GQL and Niconico
/// listing pages poll the roster only while someone is reading a list.
pub(crate) fn any_leased() -> bool {
    let now = Instant::now();
    leased(CHANNELS.lease(), now) || leased(FAVORITES.lease(), now)
}

/// Forgets an idle list, so no Holodex call or rebuild runs for it.
fn go_idle(list: &List) {
    if let Ok(mut state) = list.state.try_lock() {
        if state.built_at.is_some() || state.holodex.is_some() {
            *state = ListState::default();
            list.set_snapshot(None);
        }
    }
}

async fn worker_pass(woken: bool) {
    let cfg = match load_config().await.map_err(|e| e.to_string()) {
        Ok(cfg) => cfg,
        Err(e) => {
            tracing::debug!("Holodex 列表读取配置失败: {}", e);
            return;
        }
    };
    let usable_yt = crate::plugins::youtube_data::youtube_answers_available(&cfg);
    for kind in [ListKind::Channels, ListKind::Favorites] {
        let list = list(kind);
        let now = Instant::now();
        if !leased(list.lease(), now) {
            go_idle(list);
            continue;
        }
        let inputs = holodex_inputs(kind, &cfg).await;
        let (attempt, built_at) = {
            let state = list.state.lock().await;
            (state.attempt, state.built_at)
        };
        let fetch_due =
            inputs.is_some_and(|inputs| holodex_due(attempt, inputs, false, usable_yt, now));
        if rebuild_due(woken, fetch_due, built_at, now) {
            if let Err(e) = build(kind, false).await {
                tracing::debug!("Holodex 列表 {} 重建失败: {}", kind.source(), e);
            }
        }
    }
}

static LIST_WORKER_STARTED: AtomicBool = AtomicBool::new(false);

pub struct HolodexListWorker(tokio::task::JoinHandle<()>);

impl Drop for HolodexListWorker {
    fn drop(&mut self) {
        self.0.abort();
        LIST_WORKER_STARTED.store(false, Ordering::SeqCst);
    }
}

pub fn start_holodex_list_worker() -> Option<HolodexListWorker> {
    if LIST_WORKER_STARTED.swap(true, Ordering::SeqCst) {
        return None;
    }
    Some(HolodexListWorker(tokio::spawn(async {
        loop {
            let woken = tokio::select! {
                _ = WAKE.notified() => true,
                _ = tokio::time::sleep(TICK) => false,
            };
            if woken {
                // Later wake-ups in the window share this pass.
                tokio::time::sleep(DEBOUNCE).await;
            }
            worker_pass(woken).await;
        }
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn holodex_is_a_one_minute_backstop() {
        assert_eq!(holodex_every(true), Duration::from_secs(60));
        assert_eq!(holodex_every(false), Duration::from_secs(60));
    }

    #[test]
    fn holodex_fetches_follow_cadence_input_changes_and_the_force_gap() {
        let t0 = Instant::now();
        let s = Duration::from_secs;
        assert!(holodex_due(None, 1, false, true, t0), "first fetch");
        let tried = Some((t0, 1));
        // Cadence with and without a usable key; a failed attempt counts too.
        assert!(!holodex_due(tried, 1, false, true, t0 + s(59)));
        assert!(holodex_due(tried, 1, false, true, t0 + s(60)));
        assert!(!holodex_due(tried, 1, false, false, t0 + s(59)));
        assert!(holodex_due(tried, 1, false, false, t0 + s(60)));
        // Changed inputs and forced fetches, at most once per 10s.
        assert!(!holodex_due(tried, 2, false, true, t0 + s(9)));
        assert!(holodex_due(tried, 2, false, true, t0 + s(10)));
        assert!(!holodex_due(tried, 1, true, true, t0 + s(9)));
        assert!(holodex_due(tried, 1, true, true, t0 + s(10)));
    }

    #[test]
    fn a_list_goes_idle_ten_minutes_after_the_last_request() {
        let t0 = Instant::now();
        assert!(!leased(None, t0));
        let until = Some(t0 + LEASE);
        assert!(leased(until, t0 + Duration::from_secs(10 * 60 - 1)));
        assert!(!leased(until, t0 + Duration::from_secs(10 * 60)));
    }

    #[test]
    fn a_leased_list_rebuilds_on_wake_fetch_or_every_five_minutes() {
        let t0 = Instant::now();
        let built = Some(t0);
        assert!(rebuild_due(false, false, None, t0), "never built");
        assert!(!rebuild_due(
            false,
            false,
            built,
            t0 + Duration::from_secs(299)
        ));
        assert!(rebuild_due(false, false, built, t0 + KEEP_WANTED));
        assert!(rebuild_due(true, false, built, t0));
        assert!(rebuild_due(false, true, built, t0));
    }

    #[test]
    fn an_unchanged_rebuild_publishes_nothing() {
        let mut slot = None;
        assert!(replace_if_changed(
            &mut slot,
            serde_json::json!([{"id": "a"}])
        ));
        assert!(!replace_if_changed(
            &mut slot,
            serde_json::json!([{"id": "a"}])
        ));
        assert!(replace_if_changed(
            &mut slot,
            serde_json::json!([{"id": "b"}])
        ));
    }

    #[test]
    fn a_failed_fetch_keeps_the_last_success_time() {
        let mut state = ListState::default();
        let rows = || HolodexRows {
            rows: Vec::new(),
            favorites: HashSet::new(),
        };
        note_success(&mut state, rows());
        let ok_at = state.holodex_ok_at;
        assert!(ok_at.is_some());
        note_failure(&mut state, "down".to_string());
        assert_eq!(state.holodex_ok_at, ok_at);
    }
}
