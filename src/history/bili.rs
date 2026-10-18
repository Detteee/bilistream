//! Bilibili live-key and best-effort summary orchestration. Hot paths only
//! record observations; HTTP work, ownership checks, and recovery run here.

use super::{capture, emit, enrich, live_book, now, recording_enabled, EventKind};
use crate::config::Config;
use live_book::{BiliLiveMark, SummaryPhase};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

fn local_node(cfg: &Config) -> String {
    if cfg.cluster.enabled {
        cfg.cluster.node_id.clone()
    } else {
        "local".into()
    }
}

#[derive(Clone, Copy)]
struct RoomObservation {
    room: u64,
    live: bool,
    at: u64,
    changed_at: u64,
    generation: u64,
}

#[derive(Default)]
struct RoomObservations(Vec<RoomObservation>);

impl RoomObservations {
    fn observe(&mut self, room: u64, live: bool, at: u64) -> bool {
        if let Some(old) = self.0.iter_mut().find(|old| old.room == room) {
            if at < old.at {
                return false;
            }
            if old.live != live {
                old.changed_at = at;
                if live {
                    old.generation = old.generation.saturating_add(1);
                }
            }
            old.at = at;
            old.live = live;
        } else {
            self.0.push(RoomObservation {
                room,
                live,
                at,
                changed_at: at,
                generation: u64::from(live),
            });
        }
        self.0
            .sort_by_key(|observation| std::cmp::Reverse(observation.at));
        self.0.truncate(live_book::MAX_ENTRIES);
        true
    }

    fn recent(&self, room: u64, at: u64, ttl: u64) -> Option<RoomObservation> {
        self.0
            .iter()
            .find(|observation| {
                observation.room == room && at.saturating_sub(observation.at) <= ttl
            })
            .copied()
    }

    fn generation(&self, room: u64) -> Option<u64> {
        self.0
            .iter()
            .find(|observation| observation.room == room)
            .map(|observation| observation.generation)
    }
}

static ROOM_OBSERVATIONS: Mutex<RoomObservations> = Mutex::new(RoomObservations(Vec::new()));

/// Receives facts from the already-running room status client / WebSocket.
/// No new polling, filesystem access, spawning, or await on those paths.
pub(crate) fn observe_room_status(room: u64, live: bool) {
    observe_room_status_at(room, live, now());
}

pub(crate) fn observe_room_status_at(room: u64, live: bool, at: u64) {
    if recording_enabled() && room != 0 {
        let accepted = ROOM_OBSERVATIONS
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .observe(room, live, at);
        if accepted && !live {
            // Preserve this edge before a quick subsequent LIVE observation
            // replaces it. Fetch eligibility remains in schedule_summary.
            live_book::observe_stopped(room, at);
        }
    }
}

/// `startLive` returned its opaque broadcast identity. The complete mark is
/// mirrored by the writer and travels only in the existing signed heartbeat.
pub fn on_bili_started(cfg: &Config, live_key: &str) {
    if live_key.is_empty() || cfg.bililive.room <= 0 || !recording_enabled() {
        return;
    }
    let room = cfg.bililive.room.max(0) as u64;
    observe_room_status(room, true);
    live_book::record_started(room, live_key, &local_node(cfg), now());
}

/// GetInfo fills a missing key and validates a restored open key once. The
/// parser refuses an offline reply; a stopped room cannot recover a key.
pub(super) async fn adopt_room_live_key(cfg: &Config) -> Option<String> {
    let room = cfg.bililive.room.max(0) as u64;
    adopt_room_live_key_for_observation(cfg, room_generation(room)).await
}

/// Capture this before spawning a relay's fallback. Reading it only when the
/// async task starts can attach the next broadcast's key to the old session.
pub(super) fn room_generation(room: u64) -> Option<u64> {
    ROOM_OBSERVATIONS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .generation(room)
}

pub(super) async fn adopt_room_live_key_for_observation(
    cfg: &Config,
    generation: Option<u64>,
) -> Option<String> {
    if !recording_enabled() {
        return None;
    }
    let room = cfg.bililive.room.max(0) as u64;
    // This task can start after a room or cookie edit. Use current local
    // credentials, while retaining the room/generation captured by its caller.
    let current = match crate::config::load_config().await {
        Ok(current) if current.bililive.room.max(0) as u64 == room => current,
        _ => return None,
    };
    let cfg = &current;
    live_book::merged(cfg);
    if room_generation(room) != generation {
        note_live_key_unavailable("getinfo_broadcast_changed");
        return None;
    }
    let allowed = room_lookup_allowed(
        cfg,
        crate::cluster::local_monitoring_allowed(cfg),
        crate::notify::is_recorder(),
    );
    match live_book::begin_key_lookup(room, allowed) {
        live_book::KeyLookup::Known(key) => return Some(key),
        live_book::KeyLookup::Unavailable => return None,
        live_book::KeyLookup::Fetch => {}
    }
    let observed_at = now();
    let reply = match crate::plugins::bilibili::bili_room_info(cfg).await {
        Ok(reply) => reply,
        Err(_) => {
            note_live_key_unavailable("getinfo_failed");
            return None;
        }
    };
    capture::room_info(&reply);
    let current_generation = room_generation(room);
    let key = getinfo_key_for_observation(&reply, room, generation, current_generation)?;
    // A peer may have supplied it during the lookup. Merge preserves its
    // original start time instead of re-dating the room on failover.
    live_book::record_started(
        room,
        &key,
        &format!("getinfo:{}", local_node(cfg)),
        observed_at,
    );
    Some(key)
}

fn room_lookup_allowed(cfg: &Config, primary: bool, recorder: bool) -> bool {
    crate::plugins::bilibili::history_account_ready(cfg, cfg.bililive.room.max(0) as u64)
        && (primary || recorder)
}

fn getinfo_key_for_observation(
    reply: &serde_json::Value,
    room: u64,
    generation: Option<u64>,
    current_generation: Option<u64>,
) -> Option<String> {
    if current_generation != generation {
        note_live_key_unavailable("getinfo_broadcast_changed");
        return None;
    }
    let reply_room = reply
        .pointer("/data/room_id")
        .and_then(|room| room.as_u64().or_else(|| room.as_str()?.parse().ok()));
    if reply_room != Some(room) {
        note_live_key_unavailable("getinfo_room_mismatch");
        return None;
    }
    room_key_or_note(reply)
}

pub(crate) fn note_live_key_unavailable(reason: &str) {
    tracing::info!("live_key unavailable ({reason})");
}

pub(crate) fn room_key_or_note(reply: &serde_json::Value) -> Option<String> {
    let key = enrich::parse_room_live_key(reply);
    if key.is_none() {
        note_live_key_unavailable("getinfo_no_key");
    }
    key
}

type FlightSet = Arc<Mutex<Vec<(u64, String)>>>;
static FETCHING: LazyLock<FlightSet> = LazyLock::new(|| Arc::new(Mutex::new(Vec::new())));

/// Removing the reservation on Drop also covers task panic/cancellation.
/// The book's finite claim lease remains the cross-process safety net.
struct Flight {
    flights: FlightSet,
    room: u64,
    key: String,
}

impl Flight {
    fn reserve(flights: &FlightSet, room: u64, key: &str) -> Option<Self> {
        let mut active = flights.lock().unwrap_or_else(|e| e.into_inner());
        if active.len() >= live_book::MAX_ENTRIES
            || active.iter().any(|(r, k)| *r == room && k == key)
        {
            return None;
        }
        active.push((room, key.to_string()));
        Some(Self {
            flights: Arc::clone(flights),
            room,
            key: key.to_string(),
        })
    }
}

impl Drop for Flight {
    fn drop(&mut self) {
        self.flights
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|(room, key)| *room != self.room || key != &self.key);
    }
}

fn heartbeat_secs(cfg: &Config) -> u64 {
    if cfg.cluster.enabled {
        cfg.cluster.heartbeat_interval_secs.max(1)
    } else {
        0
    }
}

fn fallback_grace(cfg: &Config) -> u64 {
    heartbeat_secs(cfg).saturating_mul(2).saturating_add(30)
}

fn claim_lease(cfg: &Config) -> u64 {
    // A live owner has enough time for its one heartbeat, initial delay,
    // bounded HTTP calls and one delayed retry. No renewal timer can hold a
    // claim forever after a process crashes or authority is lost.
    enrich::FIRST_DELAY
        .as_secs()
        .saturating_add(enrich::RETRY_DELAY.as_secs())
        .saturating_add(enrich::FETCH_TIMEOUT.as_secs().saturating_mul(2))
        .saturating_add(fallback_grace(cfg))
}

#[derive(Clone, Copy)]
struct SummaryAuthority {
    primary: bool,
    recorder: bool,
    fallback_grace: u64,
}

impl SummaryAuthority {
    fn permits(self, stopped_at: u64, at: u64) -> bool {
        self.primary || (self.recorder && at.saturating_sub(stopped_at) >= self.fallback_grace)
    }

    fn ready(self, mark: &BiliLiveMark, at: u64) -> bool {
        !matches!(
            mark.summary.phase,
            SummaryPhase::Done | SummaryPhase::Failed
        ) && mark
            .stopped_at
            .is_some_and(|stopped| self.permits(stopped, at))
    }

    fn observed_stop(
        self,
        mark: &BiliLiveMark,
        observation: RoomObservation,
        at: u64,
    ) -> Option<u64> {
        (!observation.live
            && observation.room == mark.room
            && observation.at >= mark.started_at
            && self.permits(observation.changed_at, at))
        .then_some(observation.changed_at.max(mark.started_at))
    }
}

fn authority(cfg: &Config) -> SummaryAuthority {
    let enabled = recording_enabled();
    SummaryAuthority {
        primary: enabled && crate::cluster::local_monitoring_allowed(cfg),
        recorder: enabled && crate::notify::is_recorder(),
        fallback_grace: fallback_grace(cfg),
    }
}

fn eligible_at(cfg: &Config, stopped_at: u64, at: u64) -> bool {
    authority(cfg).permits(stopped_at, at)
}

fn summary_enabled() -> bool {
    super::shared().is_some_and(|shared| {
        shared
            .cfg
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .bili_stop_summary
    })
}

/// A confirmed stop is recorded synchronously. Already-stopped peer marks
/// still schedule locally when this node becomes eligible to recover them.
pub fn on_bili_stopped(cfg: &Config) {
    if !recording_enabled() {
        return;
    }
    let room = cfg.bililive.room.max(0) as u64;
    observe_room_status(room, false);
    let book = live_book::merged(cfg);
    let Some(mark) = book.latest(room) else {
        return;
    };
    live_book::set_stopped(room, &mark.live_key, now());
    if let Some(mark) = live_book::merged(cfg).mark(room, &mark.live_key) {
        schedule_summary(cfg, mark);
    }
}

fn schedule_summary(cfg: &Config, mark: &BiliLiveMark) {
    let Some(stopped_at) = mark.stopped_at else {
        return;
    };
    if !crate::plugins::bilibili::history_account_ready(cfg, mark.room)
        || !authority(cfg).ready(mark, now())
    {
        return;
    }
    let Some(flight) = Flight::reserve(&FETCHING, mark.room, &mark.live_key) else {
        return;
    };
    tokio::spawn(async move {
        let _flight = flight;
        let room = _flight.room;
        let key = _flight.key.clone();
        // Scheduling can outlive the config which requested it. Validate the
        // current cookie, room and authority before reserving shared work.
        let cfg = match crate::config::load_config().await {
            Ok(cfg)
                if crate::plugins::bilibili::history_account_ready(&cfg, room)
                    && eligible_at(&cfg, stopped_at, now()) =>
            {
                cfg
            }
            _ => return,
        };
        let me = local_node(&cfg);
        if !summary_enabled() {
            live_book::skip_summary(room, &key, &me, now());
            return;
        }
        // Refresh the shared book immediately before attempting a claim.
        live_book::merged(&cfg);
        let Some(generation) = live_book::claim_summary(room, &key, &me, now(), claim_lease(&cfg))
        else {
            return;
        };
        tokio::time::sleep(Duration::from_secs(heartbeat_secs(&cfg))).await;
        let active = Arc::new(AtomicBool::new(true));
        let request_active = Arc::clone(&active);
        let fetch_key = key.clone();
        let result = enrich::fetch_with_retry_guarded(
            || {
                let active = Arc::clone(&request_active);
                let key = fetch_key.clone();
                let me = me.clone();
                async move {
                    // Re-read off the stream path so changed membership and
                    // the live summary toggle apply before either attempt.
                    let fresh = match crate::config::load_config().await {
                        Ok(cfg)
                            if crate::plugins::bilibili::history_account_ready(&cfg, room)
                                && eligible_at(&cfg, stopped_at, now())
                                && summary_enabled()
                                && live_book::merged(&cfg)
                                    .owns_claim(room, &key, &me, generation) =>
                        {
                            cfg
                        }
                        _ => {
                            active.store(false, Ordering::Relaxed);
                            return Err("request cancelled".into());
                        }
                    };
                    crate::plugins::bilibili::bili_stop_live_data(&fresh, &key).await
                }
            },
            enrich::FIRST_DELAY,
            enrich::RETRY_DELAY,
            || {
                active.load(Ordering::Relaxed)
                    && summary_enabled()
                    && eligible_at(&cfg, stopped_at, now())
                    && live_book::merged(&cfg).owns_claim(room, &key, &me, generation)
            },
        )
        .await;
        if let enrich::FetchResult::Finished(totals) = result {
            live_book::finish_claim(room, &key, &me, generation, now(), totals);
        }
    });
}

/// The sole recovery loop uses already-observed room state and the notifier's
/// existing recorder election. It neither observes cluster edges nor polls
/// Bilibili for audience counts.
pub(super) async fn summary_sync() {
    let mut last_mirror = 0;
    let mut external_lookup = None;
    loop {
        tokio::time::sleep(Duration::from_secs(5)).await;
        let Ok(cfg) = crate::config::load_config().await else {
            continue;
        };
        let at = now();
        let room = cfg.bililive.room.max(0) as u64;
        let mut book = live_book::merged(&cfg);
        // Bounded replay repairs a dropped bus event. The writer treats an
        // identical record as a no-op, including terminal totals and dates.
        if at.saturating_sub(last_mirror) >= 60 {
            for mark in &book.entries {
                emit(EventKind::BiliLiveState {
                    record: mark.record(),
                });
            }
            last_mirror = at;
        }
        let observation = ROOM_OBSERVATIONS
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .recent(
                room,
                at,
                cfg.interval.max(fallback_grace(&cfg)).saturating_add(60),
            );
        if let Some(observation) = observation {
            if observation.live {
                // Capture externally-started broadcasts while they are still
                // live. A relay start has its own one-shot lookup; avoid
                // competing with it. A failed lookup is not polled forever.
                let attempt = (room, observation.generation);
                if book
                    .open_key(room)
                    .is_none_or(|mark| live_book::needs_revalidation(room, &mark.live_key))
                    && super::open_session_seq(room).is_none()
                    && external_lookup != Some(attempt)
                    && crate::plugins::bilibili::history_account_ready(&cfg, room)
                    && (crate::cluster::local_monitoring_allowed(&cfg)
                        || crate::notify::is_recorder())
                {
                    external_lookup = Some(attempt);
                    let _ = adopt_room_live_key(&cfg).await;
                    book = live_book::merged(&cfg);
                }
            } else if let Some(mark) = book.open_key(room) {
                if let Some(stopped_at) = authority(&cfg).observed_stop(mark, observation, at) {
                    live_book::set_stopped(room, &mark.live_key, stopped_at);
                    book = live_book::merged(&cfg);
                }
            }
        }
        for mark in book.entries.iter().filter(|mark| mark.room == room) {
            schedule_summary(&cfg, mark);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use live_book::{BiliLiveBook, SummaryMark};

    fn mark() -> BiliLiveMark {
        BiliLiveMark {
            room: 1,
            live_key: "synthetic-key".into(),
            started_at: 50,
            started_by: "a".into(),
            stopped_at: None,
            summary: SummaryMark::default(),
        }
    }

    #[test]
    fn room_lookup_requires_local_cookie_room_and_execution_or_recorder_permission() {
        let mut cfg = crate::cluster::tests::test_config("a", 0);
        assert!(!room_lookup_allowed(&cfg, true, true));
        cfg.bililive.credentials.sessdata = "synthetic-cookie".into();
        assert!(!room_lookup_allowed(&cfg, false, false));
        assert!(room_lookup_allowed(&cfg, true, false));
        assert!(room_lookup_allowed(&cfg, false, true));
        cfg.bililive.room = 0;
        assert!(!room_lookup_allowed(&cfg, true, true));
    }

    #[test]
    fn ownerless_offline_observation_waits_for_recorder_grace_and_stopped_peer_reschedules() {
        let primary = SummaryAuthority {
            primary: true,
            recorder: false,
            fallback_grace: 40,
        };
        let recorder = SummaryAuthority {
            primary: false,
            recorder: true,
            fallback_grace: 40,
        };
        let standby = SummaryAuthority {
            primary: false,
            recorder: false,
            fallback_grace: 40,
        };
        let mut observations = RoomObservations::default();
        observations.observe(1, true, 60);
        let live_generation = observations.generation(1);
        observations.observe(1, false, 100);
        observations.observe(1, false, 125);
        let offline = observations.recent(1, 139, 90).unwrap();
        assert_eq!(
            observations.generation(1),
            live_generation,
            "a late GetInfo reply can still belong to this stopped broadcast"
        );
        let mut mark = mark();
        assert_eq!(primary.observed_stop(&mark, offline, 139), Some(100));
        assert_eq!(recorder.observed_stop(&mark, offline, 139), None);
        assert_eq!(recorder.observed_stop(&mark, offline, 140), Some(100));
        assert_eq!(standby.observed_stop(&mark, offline, 140), None);
        mark.stopped_at = Some(100);
        assert!(
            recorder.ready(&mark, 140),
            "a stopped peer mark still schedules here"
        );
        assert!(primary.ready(&mark, 140));
        assert!(!standby.ready(&mark, 140));
        mark.summary.phase = SummaryPhase::Done;
        assert!(!primary.ready(&mark, 200));
        assert!(
            observations.recent(1, 216, 90).is_none(),
            "stale state is not offline proof"
        );
        observations.observe(1, true, 220);
        assert_ne!(
            observations.generation(1),
            live_generation,
            "a reply crossing the next broadcast must not attach to the old relay"
        );
        observations.observe(1, false, 200);
        assert!(observations.recent(1, 220, 90).unwrap().live);
        assert_eq!(
            primary.observed_stop(
                &BiliLiveMark {
                    started_at: 150,
                    ..mark
                },
                offline,
                160
            ),
            None
        );
    }

    #[test]
    fn getinfo_response_cannot_cross_a_room_or_broadcast_transition() {
        let response = serde_json::json!({"code":0,"data":{"live_status":1,"room_id":1,"up_session":"synthetic-key"}});
        assert_eq!(
            getinfo_key_for_observation(&response, 1, Some(7), Some(7)).as_deref(),
            Some("synthetic-key")
        );
        assert_eq!(
            getinfo_key_for_observation(&response, 1, Some(7), Some(8)),
            None
        );
        assert_eq!(
            getinfo_key_for_observation(&response, 1, None, Some(1)),
            None,
            "a lookup begun before the first observation cannot adopt a later broadcast"
        );
        assert_eq!(
            getinfo_key_for_observation(&response, 2, Some(7), Some(7)),
            None
        );
        for room_id in [serde_json::Value::Null, serde_json::json!("invalid")] {
            let mut missing_room = response.clone();
            missing_room["data"]["room_id"] = room_id;
            assert_eq!(
                getinfo_key_for_observation(&missing_room, 1, Some(7), Some(7)),
                None,
                "an account-level endpoint must identify the configured room"
            );
        }
        let stopped = serde_json::json!({"code":0,"data":{"live_status":0,"room_id":1,"up_session":"synthetic-key"}});
        assert_eq!(
            getinfo_key_for_observation(&stopped, 1, Some(7), Some(7)),
            None
        );
    }

    #[tokio::test]
    async fn cancelled_worker_releases_local_flight_but_claim_remains_bounded() {
        let flights = Arc::new(Mutex::new(Vec::new()));
        let flight = Flight::reserve(&flights, 1, "synthetic-key").unwrap();
        assert!(Flight::reserve(&flights, 1, "synthetic-key").is_none());
        let task = tokio::spawn(async move {
            let _flight = flight;
            std::future::pending::<()>().await;
        });
        task.abort();
        let _ = task.await;
        assert!(Flight::reserve(&flights, 1, "synthetic-key").is_some());
        let cfg = crate::cluster::tests::test_config("a", 0);
        let mut stopped = mark();
        stopped.stopped_at = Some(100);
        let mut book = BiliLiveBook {
            entries: vec![stopped],
        };
        let lease = claim_lease(&cfg);
        assert!((335..600).contains(&lease));
        book.claim(1, "synthetic-key", "a", 100, lease).unwrap();
        assert!(book
            .claim(1, "synthetic-key", "b", 100 + lease - 1, lease)
            .is_none());
        assert_eq!(
            book.claim(1, "synthetic-key", "b", 100 + lease, lease),
            Some(2)
        );
    }

    #[tokio::test]
    async fn merged_heartbeat_claims_allow_one_fetch_and_finished_summary_replays_once() {
        use crate::history::event::BiliTotals;
        use std::sync::atomic::AtomicUsize;
        let mut stopped = mark();
        stopped.stopped_at = Some(100);
        let mut a = BiliLiveBook {
            entries: vec![stopped],
        };
        let mut b = a.clone();
        a.claim(1, "synthetic-key", "a", 100, 400).unwrap();
        b.claim(1, "synthetic-key", "b", 100, 400).unwrap();
        let first = a.clone();
        a.join(&b);
        b.join(&first);
        let requests = AtomicUsize::new(0);
        let fetch = || {
            requests.fetch_add(1, Ordering::Relaxed);
            async { Ok(serde_json::json!({"code":0,"data":{"WatchedCount":77}})) }
        };
        let (left, right) = tokio::join!(
            enrich::fetch_with_retry_guarded(fetch, Duration::ZERO, Duration::ZERO, || a
                .owns_claim(1, "synthetic-key", "a", 1)),
            enrich::fetch_with_retry_guarded(fetch, Duration::ZERO, Duration::ZERO, || b
                .owns_claim(1, "synthetic-key", "b", 1))
        );
        assert_eq!(requests.load(Ordering::Relaxed), 1);
        let enrich::FetchResult::Finished(totals) = left else {
            panic!("winner should fetch");
        };
        assert_eq!(right, enrich::FetchResult::Cancelled);
        assert_eq!(
            totals,
            Some(BiliTotals {
                watched_count: Some(77),
                ..Default::default()
            })
        );
        assert!(a.finish_claim(1, "synthetic-key", "a", 1, 106, totals));
        b.join(&a);
        assert!(!b.finish_claim(1, "synthetic-key", "b", 1, 107, None));
        assert_eq!(a, b);
    }
}
