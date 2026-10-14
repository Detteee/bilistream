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
    /// Allowlisted totals of a `Done` fetch, so every node can join them to
    /// its own sessions (no revenue field exists).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub totals: Option<super::event::BiliTotals>,
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

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BiliLiveBook {
    pub entries: Vec<BiliLiveMark>,
}

fn better(a: &SummaryMark, b: &SummaryMark) -> bool {
    // Highest phase wins; ties by lower node id. Order-independent.
    (a.phase, std::cmp::Reverse(a.node.clone())) > (b.phase, std::cmp::Reverse(b.node.clone()))
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
                    if mark.started_at < mine.started_at {
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
                .then(a.live_key.cmp(&b.live_key))
        });
        self.entries.truncate(MAX_ENTRIES);
    }

    /// Newest open key for `room`.
    pub fn open_key(&self, room: u64) -> Option<&BiliLiveMark> {
        self.entries
            .iter()
            .filter(|m| m.room == room && m.stopped_at.is_none())
            .max_by_key(|m| m.started_at)
    }

    /// Newest key for `room`, open or stopped (used at the summary fetch).
    pub fn latest(&self, room: u64) -> Option<&BiliLiveMark> {
        self.entries
            .iter()
            .filter(|m| m.room == room)
            .max_by_key(|m| m.started_at)
    }
}

static LOCAL: Mutex<BiliLiveBook> = Mutex::new(BiliLiveBook {
    entries: Vec::new(),
});

fn local() -> std::sync::MutexGuard<'static, BiliLiveBook> {
    LOCAL.lock().unwrap_or_else(|e| e.into_inner())
}

/// What this node advertises in its heartbeat; `None` when empty.
pub(crate) fn heartbeat_book() -> Option<BiliLiveBook> {
    let book = local();
    (!book.entries.is_empty()).then(|| book.clone())
}

/// Local book joined with every peer's heartbeat book. The guard on the
/// cluster state is dropped before this returns.
pub(crate) fn merged(cfg: &crate::config::Config) -> BiliLiveBook {
    let mut book = local().clone();
    if cfg.cluster.enabled {
        let peers: Vec<BiliLiveBook> = crate::cluster::peer_live_books(&cfg.cluster.node_id);
        for peer in &peers {
            book.join(peer);
        }
        // Keep what we learned so a restart of the sender does not lose it.
        local().join(&book);
    }
    book
}

pub(crate) fn upsert(mark: BiliLiveMark) {
    local().join(&BiliLiveBook {
        entries: vec![mark],
    });
}

pub(crate) fn record_started(room: u64, live_key: &str, by: &str, now: u64) {
    upsert(BiliLiveMark {
        room,
        live_key: live_key.to_string(),
        started_at: now,
        started_by: by.to_string(),
        stopped_at: None,
        summary: SummaryMark::default(),
    });
}

pub(crate) fn set_stopped(room: u64, live_key: &str, now: u64) {
    let mut book = local();
    if let Some(mark) = book
        .entries
        .iter_mut()
        .find(|m| m.room == room && m.live_key == live_key)
    {
        mark.stopped_at = Some(mark.stopped_at.unwrap_or(now));
    }
}

pub(crate) fn set_summary(
    room: u64,
    live_key: &str,
    phase: SummaryPhase,
    node: &str,
    now: u64,
    totals: Option<super::event::BiliTotals>,
) {
    let mut book = local();
    if let Some(mark) = book
        .entries
        .iter_mut()
        .find(|m| m.room == room && m.live_key == live_key)
    {
        let next = SummaryMark {
            phase,
            node: Some(node.to_string()),
            at: now,
            totals,
        };
        if phase != SummaryPhase::Claimed || mark.summary.phase == SummaryPhase::Pending {
            mark.summary = next;
        }
    }
}

/// After claiming and waiting one heartbeat: back off if a lower node id
/// claimed it, or anyone finished it.
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
    local().entries.clear();
}
