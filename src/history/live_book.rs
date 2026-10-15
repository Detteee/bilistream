//! The cluster `bili_live` book (design §17.2): which B站 live session
//! (`live_key`) each room is in. Travels only inside the signed heartbeat;
//! never logged (`Debug` redacts the key), never in `/api/*` bodies.

use serde::{Deserialize, Serialize};
use std::sync::Mutex;

pub(crate) const MAX_ENTRIES: usize = 8;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SummaryPhase {
    #[default]
    Pending,
    Claimed,
    Failed,
    Done,
}

#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SummaryMark {
    pub phase: SummaryPhase,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node: Option<String>,
    #[serde(default)]
    pub at: u64,
    /// New claim rounds supersede abandoned claims without changing the
    /// legacy phase vocabulary understood by older heartbeat readers.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub generation: u64,
    /// Encoded as Failed + this flag so an older peer can still decode the
    /// heartbeat. New nodes keep skipped distinct in the durable store.
    #[serde(default, skip_serializing_if = "is_false")]
    pub skipped: bool,
    /// Allowlisted totals of a `Done` fetch, so every node can join them to
    /// its own sessions (no revenue field exists).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub totals: Option<super::event::BiliTotals>,
}

fn is_zero(value: &u64) -> bool {
    *value == 0
}

fn is_false(value: &bool) -> bool {
    !value
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BiliLiveMark {
    pub room: u64,
    pub live_key: String,
    pub started_at: u64,
    pub started_by: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stopped_at: Option<u64>,
    #[serde(default)]
    pub summary: SummaryMark,
}

impl std::fmt::Debug for BiliLiveMark {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BiliLiveMark")
            .field("room", &self.room)
            .field("live_key", &"<redacted>")
            .field("started_at", &self.started_at)
            .field("started_by", &self.started_by)
            .field("stopped_at", &self.stopped_at)
            .field("summary", &self.summary.phase)
            .finish()
    }
}

impl BiliLiveMark {
    pub(crate) fn record(&self) -> super::event::BiliLiveRecord {
        use super::event::BiliSummaryPhase;
        super::event::BiliLiveRecord {
            room: self.room,
            live_key: self.live_key.clone(),
            started_at: self.started_at,
            started_by: self.started_by.clone(),
            stopped_at: self.stopped_at,
            summary_phase: match self.summary.phase {
                SummaryPhase::Pending => BiliSummaryPhase::Pending,
                SummaryPhase::Claimed => BiliSummaryPhase::Claimed,
                SummaryPhase::Failed if self.summary.skipped => BiliSummaryPhase::Skipped,
                SummaryPhase::Failed => BiliSummaryPhase::Failed,
                SummaryPhase::Done => BiliSummaryPhase::Done,
            },
            summary_node: self.summary.node.clone(),
            summary_at: self.summary.at,
            summary_generation: self.summary.generation,
            totals: self.summary.totals.clone(),
        }
    }

    pub(crate) fn from_record(record: super::event::BiliLiveRecord) -> Self {
        use super::event::BiliSummaryPhase;
        Self {
            room: record.room,
            live_key: record.live_key,
            started_at: record.started_at,
            started_by: record.started_by,
            stopped_at: record.stopped_at,
            summary: SummaryMark {
                phase: match record.summary_phase {
                    BiliSummaryPhase::Pending => SummaryPhase::Pending,
                    BiliSummaryPhase::Claimed => SummaryPhase::Claimed,
                    BiliSummaryPhase::Failed | BiliSummaryPhase::Skipped => SummaryPhase::Failed,
                    BiliSummaryPhase::Done => SummaryPhase::Done,
                },
                node: record.summary_node,
                at: record.summary_at,
                generation: record.summary_generation,
                skipped: record.summary_phase == BiliSummaryPhase::Skipped,
                totals: record.totals,
            },
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BiliLiveBook {
    pub entries: Vec<BiliLiveMark>,
}

fn better(a: &SummaryMark, b: &SummaryMark) -> bool {
    fn rank(mark: &SummaryMark) -> u8 {
        match mark.phase {
            SummaryPhase::Pending => 0,
            SummaryPhase::Claimed => 1,
            SummaryPhase::Failed => 2,
            SummaryPhase::Done => 3,
        }
    }
    fn totals(mark: &SummaryMark) -> Option<[Option<i64>; 6]> {
        mark.totals.as_ref().map(|t| {
            [
                t.live_time_raw,
                t.add_fans,
                t.new_fans_club,
                t.danmu_num,
                t.max_online,
                t.watched_count,
            ]
        })
    }
    fn round(mark: &SummaryMark) -> u64 {
        // Older peers do not serialize generation. Their first claim still
        // competes with our first claim by node id. A stripped echo of a
        // later round cannot undo that later round.
        if mark.phase == SummaryPhase::Pending {
            mark.generation
        } else {
            mark.generation.max(1)
        }
    }
    // Successful totals are definitive. Within a round terminal failure wins
    // over a claim and the lower claimant wins ties. An abandoned claimant's
    // delayed failure cannot cancel a newer round, and an older peer's lossy
    // echo cannot roll a generation back.
    (
        a.phase == SummaryPhase::Done,
        round(a),
        rank(a),
        std::cmp::Reverse(a.node.as_deref()),
        a.at,
        a.skipped,
        totals(a),
        a.generation,
    ) > (
        b.phase == SummaryPhase::Done,
        round(b),
        rank(b),
        std::cmp::Reverse(b.node.as_deref()),
        b.at,
        b.skipped,
        totals(b),
        b.generation,
    )
}

impl BiliLiveBook {
    /// Union by `(room, live_key)`: `started_at` min, `stopped_at` max,
    /// summary by precedence. Keeps the newest `MAX_ENTRIES`.
    pub fn join(&mut self, other: &BiliLiveBook) {
        for mark in &other.entries {
            match self
                .entries
                .iter_mut()
                .find(|m| m.room == mark.room && m.live_key == mark.live_key)
            {
                Some(mine) => {
                    if (mark.started_at, &mark.started_by) < (mine.started_at, &mine.started_by) {
                        mine.started_at = mark.started_at;
                        mine.started_by = mark.started_by.clone();
                    }
                    mine.stopped_at = mine.stopped_at.max(mark.stopped_at);
                    if better(&mark.summary, &mine.summary) {
                        mine.summary = mark.summary.clone();
                    }
                }
                None => self.entries.push(mark.clone()),
            }
        }
        self.trim();
    }

    fn trim(&mut self) {
        self.entries.sort_by(|a, b| {
            b.started_at
                .cmp(&a.started_at)
                .then(a.room.cmp(&b.room))
                .then(a.live_key.cmp(&b.live_key))
        });
        self.entries.truncate(MAX_ENTRIES);
    }

    /// Newest open key for `room`.
    pub fn open_key(&self, room: u64) -> Option<&BiliLiveMark> {
        self.entries
            .iter()
            .filter(|m| m.room == room && m.stopped_at.is_none() && !m.summary.skipped)
            .max_by_key(|m| m.started_at)
    }

    /// Newest key for `room`, open or stopped (used at the summary fetch).
    pub fn latest(&self, room: u64) -> Option<&BiliLiveMark> {
        self.entries
            .iter()
            .filter(|m| m.room == room)
            .max_by_key(|m| m.started_at)
    }

    pub(crate) fn mark(&self, room: u64, key: &str) -> Option<&BiliLiveMark> {
        self.entries
            .iter()
            .find(|mark| mark.room == room && mark.live_key == key)
    }

    fn observe_stopped(&mut self, room: u64, at: u64) {
        for mark in self.entries.iter_mut().filter(|mark| {
            mark.room == room
                && mark.started_at <= at
                && mark.stopped_at.is_none()
                && !mark.summary.skipped
        }) {
            mark.stopped_at = Some(at);
        }
    }

    /// A stopped peer mark is eligible too. A still-live lease blocks a new
    /// claim, but a process crash cannot reserve the summary indefinitely.
    pub(crate) fn claim(
        &mut self,
        room: u64,
        key: &str,
        me: &str,
        now: u64,
        lease: u64,
    ) -> Option<u64> {
        let mark = self
            .entries
            .iter_mut()
            .find(|mark| mark.room == room && mark.live_key == key)?;
        if mark.stopped_at.is_none()
            || matches!(
                mark.summary.phase,
                SummaryPhase::Failed | SummaryPhase::Done
            )
        {
            return None;
        }
        if mark.summary.phase == SummaryPhase::Claimed && !claim_expired(&mark.summary, now, lease)
        {
            return None;
        }
        let generation = if mark.summary.phase == SummaryPhase::Claimed {
            mark.summary.generation.max(1).saturating_add(1)
        } else {
            mark.summary.generation.saturating_add(1)
        };
        mark.summary = SummaryMark {
            phase: SummaryPhase::Claimed,
            node: Some(me.to_string()),
            at: now,
            generation,
            skipped: false,
            totals: None,
        };
        Some(generation)
    }

    pub(crate) fn owns_claim(&self, room: u64, key: &str, me: &str, generation: u64) -> bool {
        self.mark(room, key).is_some_and(|mark| {
            mark.summary.phase == SummaryPhase::Claimed
                && mark.summary.node.as_deref() == Some(me)
                && mark.summary.generation == generation
        })
    }

    pub(crate) fn finish_claim(
        &mut self,
        room: u64,
        key: &str,
        me: &str,
        generation: u64,
        at: u64,
        totals: Option<super::event::BiliTotals>,
    ) -> bool {
        if !self.owns_claim(room, key, me, generation) {
            return false;
        }
        let mark = self
            .entries
            .iter_mut()
            .find(|mark| mark.room == room && mark.live_key == key)
            .unwrap();
        mark.summary.phase = if totals.is_some() {
            SummaryPhase::Done
        } else {
            SummaryPhase::Failed
        };
        mark.summary.at = at;
        mark.summary.totals = totals;
        true
    }
}

fn claim_expired(mark: &SummaryMark, now: u64, lease: u64) -> bool {
    now.saturating_sub(mark.at) >= lease || mark.at > now.saturating_add(lease)
}

#[derive(Default)]
struct LocalBook {
    book: BiliLiveBook,
    restored: Vec<RestoredKey>,
}

struct RestoredKey {
    room: u64,
    key: String,
    attempted: bool,
}

/// One decision before GetInfo. A missing cookie or authority never reserves
/// an attempt; a failed network lookup remains unknown until new live proof.
#[derive(PartialEq, Eq)]
pub(crate) enum KeyLookup {
    Known(String),
    Fetch,
    Unavailable,
}

impl LocalBook {
    fn restore(&mut self, book: BiliLiveBook) {
        for mark in &book.entries {
            if mark.stopped_at.is_none()
                && !mark.summary.skipped
                && self.book.mark(mark.room, &mark.live_key).is_none()
            {
                self.restored.push(RestoredKey {
                    room: mark.room,
                    key: mark.live_key.clone(),
                    attempted: false,
                });
            }
        }
        self.book.join(&book);
        self.restored
            .retain(|key| self.book.mark(key.room, &key.key).is_some());
    }

    fn needs_revalidation(&self, room: u64, key: &str) -> bool {
        self.restored
            .iter()
            .any(|old| old.room == room && old.key == key)
    }

    fn begin_lookup(&mut self, room: u64, allowed: bool) -> KeyLookup {
        if let Some(mark) = self.book.open_key(room) {
            let old = self
                .restored
                .iter_mut()
                .find(|old| old.room == room && old.key == mark.live_key);
            let Some(old) = old else {
                return KeyLookup::Known(mark.live_key.clone());
            };
            if !allowed || old.attempted {
                return KeyLookup::Unavailable;
            }
            old.attempted = true;
        } else if !allowed {
            return KeyLookup::Unavailable;
        }
        KeyLookup::Fetch
    }

    fn record_started(&mut self, room: u64, key: &str, by: &str, at: u64) {
        // An authoritative current key can prove that the old broadcast ended,
        // but cannot establish when it ended while every node was offline.
        // Retain NULL stopped_at and skip those old totals instead of making
        // up a stop edge or attaching the new relay to the old broadcast.
        for old in self
            .restored
            .iter()
            .filter(|old| old.room == room && old.key != key)
        {
            if let Some(mark) = self.book.entries.iter_mut().find(|mark| {
                mark.room == old.room
                    && mark.live_key == old.key
                    && mark.stopped_at.is_none()
                    && mark.summary.phase != SummaryPhase::Done
            }) {
                mark.summary = SummaryMark {
                    phase: SummaryPhase::Failed,
                    node: Some(by.strip_prefix("getinfo:").unwrap_or(by).to_string()),
                    at,
                    generation: mark.summary.generation.max(1).saturating_add(1),
                    skipped: true,
                    totals: None,
                };
            }
        }
        self.restored.retain(|old| old.room != room);
        self.book.join(&BiliLiveBook {
            entries: vec![BiliLiveMark {
                room,
                live_key: key.to_string(),
                started_at: at,
                started_by: by.to_string(),
                stopped_at: None,
                summary: SummaryMark::default(),
            }],
        });
    }

    fn heartbeat_book(&self) -> Option<BiliLiveBook> {
        let entries: Vec<_> = self
            .book
            .entries
            .iter()
            .filter(|mark| {
                mark.stopped_at.is_some() || !self.needs_revalidation(mark.room, &mark.live_key)
            })
            .cloned()
            .collect();
        (!entries.is_empty()).then_some(BiliLiveBook { entries })
    }
}

static LOCAL: Mutex<LocalBook> = Mutex::new(LocalBook {
    book: BiliLiveBook {
        entries: Vec::new(),
    },
    restored: Vec::new(),
});

fn local() -> std::sync::MutexGuard<'static, LocalBook> {
    LOCAL.lock().unwrap_or_else(|e| e.into_inner())
}

fn update<R>(change: impl FnOnce(&mut BiliLiveBook) -> R) -> R {
    update_local(|state| change(&mut state.book))
}

fn update_local<R>(change: impl FnOnce(&mut LocalBook) -> R) -> R {
    let (result, changed) = {
        let mut state = local();
        let before = state.book.clone();
        let result = change(&mut state);
        let changed: Vec<_> = state
            .book
            .entries
            .iter()
            .filter(|mark| before.mark(mark.room, &mark.live_key) != Some(*mark))
            .cloned()
            .collect();
        (result, changed)
    };
    for mark in changed {
        super::emit(super::EventKind::BiliLiveState {
            record: mark.record(),
        });
    }
    result
}

/// Startup restoration runs before bus sealing and therefore never emits.
/// Merge protects any newer in-memory observation racing the DB open.
pub(crate) fn restore(book: BiliLiveBook) {
    local().restore(book);
}

pub(crate) fn needs_revalidation(room: u64, key: &str) -> bool {
    local().needs_revalidation(room, key)
}

pub(crate) fn begin_key_lookup(room: u64, allowed: bool) -> KeyLookup {
    local().begin_lookup(room, allowed)
}

/// What this node advertises in its heartbeat; `None` when empty.
pub(crate) fn heartbeat_book() -> Option<BiliLiveBook> {
    local().heartbeat_book()
}

/// Local book joined with every peer's heartbeat book. The guard on the
/// cluster state is dropped before this returns.
pub(crate) fn merged(cfg: &crate::config::Config) -> BiliLiveBook {
    let peers = if cfg.cluster.enabled {
        crate::cluster::peer_live_books(&cfg.cluster.node_id)
    } else {
        Vec::new()
    };
    update(|book| {
        for peer in peers {
            book.join(&peer);
        }
        book.clone()
    })
}

pub(crate) fn record_started(room: u64, live_key: &str, by: &str, now: u64) {
    update_local(|state| state.record_started(room, live_key, by, now));
}

pub(crate) fn set_stopped(room: u64, live_key: &str, now: u64) {
    update(|book| {
        if let Some(mark) = book
            .entries
            .iter_mut()
            .find(|m| m.room == room && m.live_key == live_key)
        {
            mark.stopped_at = Some(mark.stopped_at.unwrap_or(now));
        }
    });
}

pub(crate) fn observe_stopped(room: u64, at: u64) {
    update(|book| book.observe_stopped(room, at));
}

pub(crate) fn claim_summary(room: u64, key: &str, me: &str, now: u64, lease: u64) -> Option<u64> {
    update(|book| book.claim(room, key, me, now, lease))
}

pub(crate) fn finish_claim(
    room: u64,
    key: &str,
    me: &str,
    generation: u64,
    at: u64,
    totals: Option<super::event::BiliTotals>,
) -> bool {
    update(|book| book.finish_claim(room, key, me, generation, at, totals))
}

pub(crate) fn skip_summary(room: u64, key: &str, me: &str, at: u64) {
    update(|book| {
        if let Some(mark) = book
            .entries
            .iter_mut()
            .find(|mark| mark.room == room && mark.live_key == key)
        {
            if mark.summary.phase == SummaryPhase::Pending {
                mark.summary = SummaryMark {
                    phase: SummaryPhase::Failed,
                    node: Some(me.to_string()),
                    at,
                    generation: mark.summary.generation,
                    skipped: true,
                    totals: None,
                };
            }
        }
    });
}

/// After claiming and waiting one heartbeat: back off if a lower node id
/// claimed it, or anyone finished it.
#[cfg(test)]
pub(crate) fn should_back_off(book: &BiliLiveBook, room: u64, live_key: &str, me: &str) -> bool {
    book.entries
        .iter()
        .find(|m| m.room == room && m.live_key == live_key)
        .is_some_and(|m| match (&m.summary.phase, m.summary.node.as_deref()) {
            (SummaryPhase::Done, _) => true,
            (SummaryPhase::Claimed | SummaryPhase::Failed, Some(node)) => node < me,
            _ => false,
        })
}

#[cfg(test)]
pub(crate) fn reset_for_test() {
    *local() = LocalBook::default();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history::event::{BiliSummaryPhase, BiliTotals, Event, EventKind};

    fn stopped() -> BiliLiveBook {
        BiliLiveBook {
            entries: vec![BiliLiveMark {
                room: 42,
                live_key: "synthetic-broadcast".into(),
                started_at: 10,
                started_by: "a".into(),
                stopped_at: Some(90),
                summary: SummaryMark::default(),
            }],
        }
    }

    fn restored_open() -> LocalBook {
        let mut book = stopped();
        book.entries[0].stopped_at = None;
        let mut local = LocalBook::default();
        local.restore(book);
        local
    }

    #[test]
    fn restored_open_key_is_unknown_until_one_eligible_lookup_confirms_it() {
        let mut local = restored_open();
        let original = local.book.clone();
        assert!(local.needs_revalidation(42, "synthetic-broadcast"));
        assert!(
            local.heartbeat_book().is_none(),
            "do not advertise an unverified old broadcast"
        );
        assert!(matches!(
            local.begin_lookup(42, false),
            KeyLookup::Unavailable
        ));
        assert!(
            !local.restored[0].attempted,
            "missing credentials or authority do not consume the attempt"
        );
        assert!(matches!(local.begin_lookup(42, true), KeyLookup::Fetch));
        assert!(matches!(
            local.begin_lookup(42, true),
            KeyLookup::Unavailable
        ));
        assert!(
            local.needs_revalidation(42, "synthetic-broadcast"),
            "a failed request keeps the association unknown"
        );
        assert!(
            local.heartbeat_book().is_none(),
            "failure does not re-advertise the stale key"
        );
        local.record_started(42, "synthetic-broadcast", "getinfo:b", 100);
        assert_eq!(
            local.book, original,
            "same-key proof changes no stored fact or start time"
        );
        assert!(!local.needs_revalidation(42, "synthetic-broadcast"));
        assert_eq!(local.heartbeat_book(), Some(original));
        assert!(
            matches!(local.begin_lookup(42, false), KeyLookup::Known(key) if key == "synthetic-broadcast")
        );
    }

    #[test]
    fn restored_old_broadcast_is_retired_without_inventing_its_stop_time() {
        let mut local = restored_open();
        assert!(matches!(local.begin_lookup(42, true), KeyLookup::Fetch));
        local.record_started(42, "synthetic-current", "getinfo:b", 100);
        let old = local.book.mark(42, "synthetic-broadcast").unwrap();
        assert_eq!(old.stopped_at, None);
        assert_eq!(old.record().summary_phase, BiliSummaryPhase::Skipped);
        assert_eq!(
            local.book.open_key(42).unwrap().live_key,
            "synthetic-current"
        );
        assert_eq!(
            local.book.open_key(42).unwrap().started_at,
            100,
            "only the new observation time is known"
        );
        assert!(
            matches!(local.begin_lookup(42, true), KeyLookup::Known(key) if key == "synthetic-current")
        );
        local.book.observe_stopped(42, 200);
        assert_eq!(
            local
                .book
                .mark(42, "synthetic-broadcast")
                .unwrap()
                .stopped_at,
            None
        );
        assert_eq!(
            local.book.mark(42, "synthetic-current").unwrap().stopped_at,
            Some(200)
        );

        let conn = crate::history::schema::open_rw(std::path::Path::new(":memory:")).unwrap();
        let mut writer = crate::history::writer::Writer::new(conn, "b", 2);
        let events: Vec<_> = local
            .book
            .entries
            .iter()
            .map(|mark| Event {
                at: 200,
                kind: EventKind::BiliLiveState {
                    record: mark.record(),
                },
            })
            .collect();
        writer.commit(&events).unwrap();
        let restored = crate::history::writer::load_live_book(&writer.conn).unwrap();
        assert_eq!(restored, local.book);
        let (stopped_at, state): (Option<i64>, String) = writer.conn.query_row(
            "SELECT ended_at,summary_state FROM bili_lives WHERE live_key='synthetic-broadcast'", [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        ).unwrap();
        assert_eq!((stopped_at, state.as_str()), (None, "skipped"));
    }

    #[test]
    fn restoration_does_not_quarantine_a_newer_live_fact_observed_during_startup() {
        let mut local = LocalBook::default();
        local.record_started(42, "synthetic-current", "a", 100);
        let mut older = stopped();
        older.entries[0].stopped_at = None;
        local.restore(older);
        assert!(
            matches!(local.begin_lookup(42, false), KeyLookup::Known(key) if key == "synthetic-current")
        );
        assert_eq!(local.heartbeat_book().unwrap().entries.len(), 1);
    }

    #[test]
    fn superseded_restored_key_uses_only_the_deployed_v1_wire_phase_vocabulary() {
        // Deployed v1 has this four-variant enum and ignores unknown struct
        // fields. Keeping it separate catches an accidental `skipped` variant
        // that would reject the entire heartbeat on those peers.
        #[derive(Deserialize)]
        #[serde(rename_all = "snake_case")]
        enum V1Phase {
            Pending,
            Claimed,
            Failed,
            Done,
        }
        #[derive(Deserialize)]
        struct V1Summary {
            phase: V1Phase,
        }
        #[derive(Deserialize)]
        struct V1Mark {
            summary: V1Summary,
        }
        #[derive(Deserialize)]
        struct V1Book {
            entries: Vec<V1Mark>,
        }

        let mut local = restored_open();
        local.record_started(42, "synthetic-current", "getinfo:b", 100);
        let wire = serde_json::to_value(local.heartbeat_book().unwrap()).unwrap();
        let v1: V1Book = serde_json::from_value(wire.clone()).unwrap();
        assert!(v1
            .entries
            .iter()
            .any(|mark| matches!(mark.summary.phase, V1Phase::Failed)));
        assert!(v1
            .entries
            .iter()
            .any(|mark| matches!(mark.summary.phase, V1Phase::Pending)));
        let mut echoed = wire;
        for entry in echoed["entries"].as_array_mut().unwrap() {
            let summary = entry["summary"].as_object_mut().unwrap();
            summary.remove("skipped");
            summary.remove("generation");
        }
        local.book.join(&serde_json::from_value(echoed).unwrap());
        let old = local.book.mark(42, "synthetic-broadcast").unwrap();
        assert_eq!(old.record().summary_phase, BiliSummaryPhase::Skipped);
        assert_eq!(old.stopped_at, None);
        assert_eq!(
            local.book.open_key(42).unwrap().live_key,
            "synthetic-current"
        );
    }

    #[test]
    fn observed_stop_keeps_old_key_closed_across_a_quick_new_broadcast() {
        let mut book = stopped();
        book.entries[0].stopped_at = None;
        book.observe_stopped(42, 90);
        book.join(&BiliLiveBook {
            entries: vec![BiliLiveMark {
                live_key: "synthetic-new-broadcast".into(),
                started_at: 91,
                stopped_at: None,
                ..book.entries[0].clone()
            }],
        });
        book.observe_stopped(42, 90);
        assert_eq!(
            book.open_key(42).unwrap().live_key,
            "synthetic-new-broadcast"
        );
        assert_eq!(
            book.mark(42, "synthetic-broadcast").unwrap().stopped_at,
            Some(90)
        );
        book.observe_stopped(43, 100);
        assert!(
            book.open_key(42).is_some(),
            "other rooms cannot close this broadcast"
        );
    }

    #[test]
    fn expired_claim_gets_new_round_and_cannot_be_overwritten_by_old_winner() {
        let mut book = stopped();
        assert_eq!(book.claim(42, "synthetic-broadcast", "a", 100, 60), Some(1));
        let old = book.clone();
        assert_eq!(book.claim(42, "synthetic-broadcast", "b", 159, 60), None);
        assert_eq!(book.claim(42, "synthetic-broadcast", "b", 160, 60), Some(2));
        book.join(&old);
        assert!(book.owns_claim(42, "synthetic-broadcast", "b", 2));
        let mut late_failure = old.clone();
        late_failure.entries[0].summary.phase = SummaryPhase::Failed;
        late_failure.entries[0].summary.at = 161;
        book.join(&late_failure);
        assert!(book.owns_claim(42, "synthetic-broadcast", "b", 2));
        assert!(!book.finish_claim(42, "synthetic-broadcast", "a", 1, 161, None));
        let totals = BiliTotals {
            watched_count: Some(77),
            ..BiliTotals::default()
        };
        assert!(book.finish_claim(42, "synthetic-broadcast", "b", 2, 165, Some(totals.clone())));
        assert_eq!(book.claim(42, "synthetic-broadcast", "a", 1000, 60), None);
        assert!(!book.finish_claim(42, "synthetic-broadcast", "b", 2, 166, None));
        assert_eq!(book.entries[0].summary.totals, Some(totals));
    }

    #[test]
    fn mixed_version_echo_preserves_generation_and_known_skipped_state() {
        fn older_echo(book: &BiliLiveBook) -> BiliLiveBook {
            let mut value = serde_json::to_value(book).unwrap();
            for mark in value["entries"].as_array_mut().unwrap() {
                let summary = mark["summary"].as_object_mut().unwrap();
                assert!(matches!(
                    summary["phase"].as_str().unwrap(),
                    "pending" | "claimed" | "failed" | "done"
                ));
                summary.remove("generation");
                summary.remove("skipped");
            }
            serde_json::from_value(value).unwrap()
        }
        let mut book = stopped();
        book.claim(42, "synthetic-broadcast", "b", 100, 60).unwrap();
        book.claim(42, "synthetic-broadcast", "b", 160, 60).unwrap();
        book.join(&older_echo(&book.clone()));
        assert_eq!(book.entries[0].summary.generation, 2);
        book.entries[0].summary.phase = SummaryPhase::Failed;
        book.entries[0].summary.skipped = true;
        let old = older_echo(&book);
        book.join(&old);
        assert_eq!(
            book.entries[0].record().summary_phase,
            BiliSummaryPhase::Skipped
        );
        let mut other_order = old;
        other_order.join(&book);
        assert_eq!(other_order, book);
        assert!(!format!("{:?}", book.entries[0].record()).contains("synthetic-broadcast"));
    }

    #[test]
    fn fresh_legacy_claim_competes_in_first_round_and_expiry_creates_a_later_round() {
        let mut modern = stopped();
        modern
            .claim(42, "synthetic-broadcast", "b", 100, 60)
            .unwrap();
        let mut legacy = stopped();
        legacy.entries[0].summary = SummaryMark {
            phase: SummaryPhase::Claimed,
            node: Some("a".into()),
            at: 100,
            ..Default::default()
        };
        let modern_before = modern.clone();
        modern.join(&legacy);
        legacy.join(&modern_before);
        assert_eq!(modern, legacy);
        assert!(modern.owns_claim(42, "synthetic-broadcast", "a", 0));
        assert!(!modern.owns_claim(42, "synthetic-broadcast", "b", 1));
        assert_eq!(modern.claim(42, "synthetic-broadcast", "b", 159, 60), None);
        assert_eq!(
            modern.claim(42, "synthetic-broadcast", "b", 160, 60),
            Some(2)
        );
        modern.join(&legacy);
        assert!(modern.owns_claim(42, "synthetic-broadcast", "b", 2));
        legacy.join(&modern);
        assert_eq!(modern, legacy);
    }

    #[test]
    fn a_future_dated_abandoned_claim_does_not_reserve_work_forever() {
        let mut book = stopped();
        book.claim(42, "synthetic-broadcast", "a", 1000, 60)
            .unwrap();
        assert_eq!(book.claim(42, "synthetic-broadcast", "b", 100, 60), Some(2));
    }

    #[test]
    fn same_round_concurrent_claims_converge_before_request_and_failed_is_terminal() {
        let mut a = stopped();
        let mut b = a.clone();
        a.claim(42, "synthetic-broadcast", "a", 100, 60).unwrap();
        b.claim(42, "synthetic-broadcast", "b", 100, 60).unwrap();
        let a_before_heartbeat = a.clone();
        a.join(&b);
        b.join(&a_before_heartbeat);
        assert_eq!(a, b);
        assert!(a.owns_claim(42, "synthetic-broadcast", "a", 1));
        assert!(!b.owns_claim(42, "synthetic-broadcast", "b", 1));
        assert!(a.finish_claim(42, "synthetic-broadcast", "a", 1, 105, None));
        b.join(&a);
        assert_eq!(b.claim(42, "synthetic-broadcast", "b", 1000, 60), None);
        assert!(b.entries[0].summary.totals.is_none());
    }

    #[test]
    fn durable_book_restores_claims_metadata_and_totals_across_restarts() {
        let path = std::env::temp_dir().join(format!(
            "bilistream-live-book-{}-{}.sqlite",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let mut book = stopped();
        book.claim(42, "synthetic-broadcast", "a", 100, 60).unwrap();
        {
            let conn = crate::history::schema::open_rw(&path).unwrap();
            let mut writer = crate::history::writer::Writer::new(conn, "a", 1);
            writer
                .commit(&[Event {
                    at: 100,
                    kind: EventKind::BiliLiveState {
                        record: book.entries[0].record(),
                    },
                }])
                .unwrap();
        }
        {
            let conn = crate::history::schema::open_rw(&path).unwrap();
            let mut writer = crate::history::writer::Writer::new(conn, "b", 2);
            writer
                .apply(&Event {
                    at: 150,
                    kind: EventKind::ProcessStarted { version: "test" },
                })
                .unwrap();
            let mut restored = crate::history::writer::load_live_book(&writer.conn).unwrap();
            assert_eq!(
                restored, book,
                "restart must not turn a recoverable claim into failure"
            );
            assert_eq!(
                restored.claim(42, "synthetic-broadcast", "b", 159, 60),
                None
            );
            let generation = restored
                .claim(42, "synthetic-broadcast", "b", 160, 60)
                .unwrap();
            restored.finish_claim(
                42,
                "synthetic-broadcast",
                "b",
                generation,
                165,
                Some(BiliTotals {
                    watched_count: Some(77),
                    ..Default::default()
                }),
            );
            let completed = Event {
                at: 165,
                kind: EventKind::BiliLiveState {
                    record: restored.entries[0].record(),
                },
            };
            writer
                .commit(&[
                    completed.clone(),
                    completed,
                    Event {
                        at: 166,
                        kind: EventKind::BiliLiveState {
                            record: book.entries[0].record(),
                        },
                    },
                ])
                .unwrap();
            let mut reloaded = crate::history::writer::load_live_book(&writer.conn).unwrap();
            assert_eq!(reloaded, restored);
            assert_eq!(
                reloaded.claim(42, "synthetic-broadcast", "a", 1000, 60),
                None
            );
            let rows: u64 = writer
                .conn
                .query_row("SELECT count(*) FROM bili_lives", [], |r| r.get(0))
                .unwrap();
            assert_eq!(
                rows, 1,
                "a replay or delayed pre-crash mark cannot double-count totals"
            );
        }
        let _ = std::fs::remove_file(path);
    }
}
