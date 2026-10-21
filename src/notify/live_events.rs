//! 本直播间开播/换台/下播 notices (contract Addendum A rev 2, D, E rev 3).
//!
//! The unit is the Bilibili room session, not the upstream source: the room
//! going live opens `generation`, a source change inside it is one
//! `LiveKind::LiveSwitch`, and only the room going offline closes it, with
//! one 下播 that names the source chain and the end cause (Addendum E rev 5
//! item 20).
//!
//! The streaming node records the broadcast in the synced ledger
//! (`LiveLedger::broadcast`); only the gateway holder sends. Each notice
//! goes out as a plain active message first and falls back to the passive
//! reply, then a wakeup outside the alert reserve, then `pending`, only on a
//! definite refusal (40034105 / 40034100) or after the ambiguous-retry limit.
//!
//! Every attempt on every path is preceded by a committed `sending` record
//! (`live_event_delivery[{generation, kind}]`). A record that is `delivered`
//! is never sent again; a `sending` record from an older gateway term is an
//! ambiguous result and takes the retry path (a rare duplicate is accepted,
//! a lost notice is not). Nothing here holds the openid, a msg_id or a
//! token: the records, the broadcast and `active_push` travel in heartbeats.

use super::ledger::NotifyLedger;
use super::qq::api::{ApiResult, QqMode};
use super::sink::{PassiveTarget, PASSIVE_MAX_REPLIES};
use crate::events::BroadcastEndCause;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io;

/// Live events never use the last two open wakeup periods.
pub const WAKEUP_RESERVE_FOR_ALERTS: usize = 2;
/// Ambiguous active results: at most this many active attempts in total…
pub const AMBIGUOUS_MAX_ATTEMPTS: u32 = 3;
/// …within this window from the first one.
pub const AMBIGUOUS_WINDOW_SECS: u64 = 120;
/// Addendum E rev 5 item 21: a held (`pending`) notice tries active again
/// at most this often, or at once after an active send was accepted.
pub const PENDING_ACTIVE_RETRY_SECS: u64 = 600;
/// Delay before attempt 2 and 3, counted from the first attempt.
const AMBIGUOUS_RETRY_AT: [u64; 3] = [0, 20, 60];
/// QQ refused the active message for good (no permission / rate limited).
pub const CODE_NO_PERMISSION: i64 = 40034105;
pub const CODE_RATE_LIMITED: i64 = 40034100;
/// Passive reply window closed (Addendum E item 3).
pub const PASSIVE_CLOSED_CODES: [i64; 3] = [40034005, 304103, 40034128];
const TITLE_CHARS: usize = 40;
const NAME_CHARS: usize = 40;
/// Bilibili's 警告/切断 reason after 结束原因; the same cap as the history detail.
const REASON_CHARS: usize = 160;
const SHANGHAI_SECS: i32 = 8 * 3600;
/// Switched sources kept for the 下播 chain; older ones only survive as the
/// session's first source.
const RECENT_SOURCES: usize = 6;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LiveKind {
    LiveStart,
    /// The room stayed live and the relayed source changed (换台).
    LiveSwitch,
    LiveEnd,
}

impl LiveKind {
    pub fn as_str(self) -> &'static str {
        match self {
            LiveKind::LiveStart => "live_start",
            LiveKind::LiveSwitch => "live_switch",
            LiveKind::LiveEnd => "live_end",
        }
    }
    fn label(self) -> &'static str {
        match self {
            LiveKind::LiveStart => "开播",
            LiveKind::LiveSwitch => "换台",
            LiveKind::LiveEnd => "下播",
        }
    }
}

/// Identity of one notice inside a broadcast: the kind plus, for
/// `LiveSwitch`, the `Broadcast::switch_seq` it announces. A session switches
/// sources many times, so switch notices cannot share a single record slot.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LiveEventKey {
    pub kind: LiveKind,
    pub seq: u32,
}

impl LiveEventKey {
    pub const fn start() -> Self {
        Self {
            kind: LiveKind::LiveStart,
            seq: 0,
        }
    }
    pub const fn end() -> Self {
        Self {
            kind: LiveKind::LiveEnd,
            seq: 0,
        }
    }
    pub const fn switch(seq: u32) -> Self {
        Self {
            kind: LiveKind::LiveSwitch,
            seq,
        }
    }
    /// The wire form used in records, logs and dedup keys:
    /// `live_start`, `live_end`, `live_switch:3`.
    pub fn as_str(self) -> String {
        match self.kind {
            LiveKind::LiveSwitch => format!("live_switch:{}", self.seq),
            other => other.as_str().to_string(),
        }
    }
}

impl std::fmt::Display for LiveEventKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.as_str())
    }
}

impl Serialize for LiveEventKey {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.as_str())
    }
}

impl<'de> Deserialize<'de> for LiveEventKey {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        use serde::de::Error;
        let raw = String::deserialize(deserializer)?;
        let unknown = || Error::custom(format!("unknown live event key {raw}"));
        match raw.split_once(':') {
            Some(("live_switch", seq)) => Ok(Self::switch(seq.parse().map_err(Error::custom)?)),
            Some(_) => Err(unknown()),
            // A pre-换台 ledger only holds 开播/下播 records.
            None => match raw.as_str() {
                "live_start" => Ok(Self::start()),
                "live_end" => Ok(Self::end()),
                "live_switch" => Ok(Self::switch(0)),
                _ => Err(unknown()),
            },
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Channel {
    Active,
    Passive,
    Wakeup,
    /// No channel left (`pending`) or the switch is off (`off`).
    None,
}

impl Channel {
    pub fn as_str(self) -> &'static str {
        match self {
            Channel::Active => "active",
            Channel::Passive => "passive",
            Channel::Wakeup => "wakeup",
            Channel::None => "none",
        }
    }
}

/// Order matters for the join of equal-term, equal-seq records only.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryState {
    Sending,
    Refused,
    /// Every channel is used up: waits for the next passive window
    /// (`skipped_quota`, `pending_live_events`).
    Pending,
    /// `live_events_enabled` was off when the holder saw it: never sent.
    Off,
    Delivered,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeliveryRecord {
    pub state: DeliveryState,
    /// Gateway-holder term of the writer.
    pub term: u64,
    pub channel: Channel,
    pub at: u64,
    /// Writes per record (any node), so the newest write of a term wins.
    #[serde(default)]
    pub seq: u64,
    /// Active attempts made so far for this record (ambiguous-retry limit).
    #[serde(default)]
    pub attempts: u32,
    /// First active attempt (start of the 2-minute window).
    #[serde(default)]
    pub first_at: u64,
    /// E rev 6 item 25: this notice was held (`pending`) once. From then
    /// on it never uses a wakeup, so it can never touch the periods kept
    /// for alerts, even after an unclear active retry on a later cycle.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub held: bool,
}

impl DeliveryRecord {
    fn rank(&self) -> (bool, u64, u64, DeliveryState, u64) {
        (
            self.state == DeliveryState::Delivered,
            self.term,
            self.seq,
            self.state,
            self.at,
        )
    }
}

/// One relayed source of a session, as the 换台 notice saw it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceEntry {
    #[serde(default)]
    pub platform: String,
    #[serde(default)]
    pub channel: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub at: u64,
}

impl SourceEntry {
    fn clean(info: &LiveSource, at: u64) -> Self {
        Self {
            platform: crate::logging::safe_text(&info.platform, NAME_CHARS),
            channel: crate::logging::safe_text(&info.channel, NAME_CHARS),
            title: crate::logging::safe_text(&info.title, TITLE_CHARS),
            at,
        }
    }
}

/// One Bilibili room session: `generation` only moves when the room goes live
/// again. A source switch inside the session is a 换台, never a new 开播.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Broadcast {
    pub generation: u64,
    /// `platform:channel` of the first source. Addendum E rev 5 item 20: a
    /// different source while the Bilibili broadcast is open is NOT a new
    /// broadcast; this keeps the source the 开播 named.
    #[serde(default)]
    pub stream_key: String,
    #[serde(default)]
    pub platform: String,
    #[serde(default)]
    pub channel: String,
    #[serde(default)]
    pub title: String,
    pub started_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<u64>,
    /// `platform:channel` selected now; empty means the first source.
    #[serde(default)]
    pub current_key: String,
    /// Switches announced in this session (the newest `LiveSwitch` ordinal).
    #[serde(default)]
    pub switch_seq: u32,
    /// The tail of the switched sources, newest last, bounded by
    /// `RECENT_SOURCES`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub recent: Vec<SourceEntry>,
    /// Why the room went offline; absent while it is live.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_cause: Option<BroadcastEndCause>,
    /// Bilibili's reason for a `WarningCut`. Local to the node that saw the
    /// danmaku frame and never on the wire or on disk (no ledger change): a
    /// peer or a restarted node renders the fixed label instead.
    #[serde(skip)]
    pub end_detail: Option<crate::events::EndDetail>,
}

/// `active_push` per node (Addendum E rev 2 item 10). Stored as unix
/// seconds; `/status` renders RFC3339.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActivePush {
    /// `ok` | `no_permission` | `rate_limited`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_result: Option<String>,
    #[serde(default)]
    pub last_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refused_since: Option<u64>,
    /// `YYYYMM` in Asia/Shanghai that `month_sent` counts.
    #[serde(default)]
    pub month: u32,
    #[serde(default)]
    pub month_sent: u32,
}

impl ActivePush {
    fn rank(&self) -> (u64, u32, u32) {
        (self.last_at, self.month, self.month_sent)
    }

    /// The `/status` shape. `month_sent` reads 0 once the month rolled over.
    pub fn view(&self, now: u64) -> serde_json::Value {
        let month_sent = if self.month == shanghai_month(now) {
            self.month_sent
        } else {
            0
        };
        serde_json::json!({
            "last_result": self.last_result,
            "last_at": (self.last_at > 0).then(|| rfc3339(self.last_at)),
            "refused_since": self.refused_since.map(rfc3339),
            "month_sent": month_sent,
        })
    }

    pub fn empty_view() -> serde_json::Value {
        ActivePush::default().view(0)
    }
}

/// Synced, id-free live-event state inside the notify ledger.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LiveLedger {
    #[serde(default)]
    pub broadcast: Broadcast,
    /// `live_event_delivery` for `broadcast.generation` only.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub delivery: BTreeMap<LiveEventKey, DeliveryRecord>,
    /// Highest gateway-holder term handed out.
    #[serde(default)]
    pub term: u64,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub active_push: BTreeMap<String, ActivePush>,
}

impl Broadcast {
    /// The source relayed last in this session: the newest 换台, else the
    /// channel the 开播 named. The 下播 notice and its log line both name it.
    pub fn newest_source(&self) -> &str {
        self.recent
            .last()
            .map(|entry| entry.channel.as_str())
            .unwrap_or(&self.channel)
    }
}

impl LiveLedger {
    pub fn is_empty(&self) -> bool {
        *self == LiveLedger::default()
    }

    /// Commutative, associative, idempotent.
    pub fn join(&mut self, other: &LiveLedger) {
        self.term = self.term.max(other.term);
        let mine = self.broadcast.clone();
        let theirs = other.broadcast.clone();
        let same_session = theirs.generation == mine.generation
            && theirs.started_at == mine.started_at
            && theirs.stream_key == mine.stream_key;
        if (theirs.generation, theirs.started_at, &theirs.stream_key)
            > (mine.generation, mine.started_at, &mine.stream_key)
        {
            // A newer broadcast: older records are cleared (rule 14).
            let ended = if same_session {
                theirs.ended_at.max(mine.ended_at)
            } else {
                theirs.ended_at
            };
            let end_cause = if !same_session {
                theirs.end_cause
            } else if mine.ended_at > theirs.ended_at {
                mine.end_cause
            } else {
                theirs.end_cause.or(mine.end_cause)
            };
            if theirs.generation > mine.generation {
                self.delivery = other.delivery.clone();
            } else {
                merge_records(&mut self.delivery, &other.delivery);
            }
            let end_detail = if same_session && end_cause == mine.end_cause {
                mine.end_detail.clone()
            } else {
                None
            };
            self.broadcast = theirs;
            self.broadcast.ended_at = ended;
            self.broadcast.end_cause = end_cause;
            self.broadcast.end_detail = end_detail;
        } else if same_session {
            if theirs.ended_at > mine.ended_at {
                self.broadcast.ended_at = theirs.ended_at;
                if self.broadcast.end_cause != theirs.end_cause {
                    self.broadcast.end_detail = None;
                }
                self.broadcast.end_cause = theirs.end_cause;
            } else if theirs.ended_at == mine.ended_at && self.broadcast.end_cause.is_none() {
                self.broadcast.end_cause = theirs.end_cause;
            }
            // Switch state is monotonic: a node that has not seen the newest
            // source yet loses only that field, never the whole session.
            if theirs.switch_seq > mine.switch_seq
                || (theirs.switch_seq == mine.switch_seq && theirs.recent.len() > mine.recent.len())
            {
                self.broadcast.switch_seq = theirs.switch_seq.max(mine.switch_seq);
                self.broadcast.recent = theirs.recent.clone();
                if !theirs.current_key.is_empty() {
                    self.broadcast.current_key = theirs.current_key.clone();
                }
            }
            merge_records(&mut self.delivery, &other.delivery);
        }
        for (node, push) in &other.active_push {
            match self.active_push.get_mut(node) {
                Some(own) if own.rank() >= push.rank() => {}
                Some(own) => *own = push.clone(),
                None => {
                    self.active_push.insert(node.clone(), push.clone());
                }
            }
        }
        self.prune_switch_records();
    }

    /// The runtime selected this source while the room is live: open the
    /// broadcast if none is open, otherwise record a 换台 when the source
    /// moved. Idempotent for a re-selection of the current source.
    pub fn source(&mut self, info: &LiveSource, now: u64) -> SourceOutcome {
        if self.broadcast.generation == 0 || self.broadcast.ended_at.is_some() {
            let generation = self.broadcast.generation + 1;
            self.broadcast = Broadcast {
                generation,
                stream_key: info.stream_key.clone(),
                current_key: info.stream_key.clone(),
                platform: crate::logging::safe_text(&info.platform, NAME_CHARS),
                channel: crate::logging::safe_text(&info.channel, NAME_CHARS),
                title: crate::logging::safe_text(&info.title, TITLE_CHARS),
                started_at: now,
                ..Broadcast::default()
            };
            self.delivery.clear();
            return SourceOutcome::Started(generation);
        }
        let current = if self.broadcast.current_key.is_empty() {
            self.broadcast.stream_key.clone()
        } else {
            self.broadcast.current_key.clone()
        };
        if current == info.stream_key {
            return SourceOutcome::Same(self.broadcast.generation);
        }
        self.broadcast.switch_seq += 1;
        self.broadcast.current_key = info.stream_key.clone();
        self.broadcast.recent.push(SourceEntry::clean(info, now));
        if self.broadcast.recent.len() > RECENT_SOURCES {
            let drop = self.broadcast.recent.len() - RECENT_SOURCES;
            self.broadcast.recent.drain(..drop);
        }
        // A newer source supersedes the switch notice still in flight: the old
        // source is stale by the time that notice would be sent.
        self.prune_switch_records();
        SourceOutcome::Switched(self.broadcast.generation, self.broadcast.switch_seq)
    }

    /// The Bilibili room went offline. Only an open broadcast ends, once, and
    /// the first cause wins.
    pub fn end(&mut self, now: u64, cause: BroadcastEndCause) -> bool {
        self.end_with(now, cause, None)
    }

    /// [`Self::end`] carrying Bilibili's reason. A later, more detailed
    /// reason for the same `WarningCut` (CUT_OFF then CUT_OFF_V2) replaces a
    /// less detailed one; an equal or less detailed one never does. Only the
    /// first end changes the synced ledger.
    pub fn end_with(
        &mut self,
        now: u64,
        cause: BroadcastEndCause,
        detail: Option<crate::events::EndDetail>,
    ) -> bool {
        if self.broadcast.generation == 0 {
            return false;
        }
        let detail = detail
            .filter(|d| cause == BroadcastEndCause::WarningCut && d.rank > 0 && !d.text.is_empty());
        if self.broadcast.ended_at.is_some() {
            if self.broadcast.end_cause == Some(BroadcastEndCause::WarningCut) {
                if let Some(detail) = detail {
                    let current = self.broadcast.end_detail.as_ref().map_or(0, |d| d.rank);
                    if detail.rank > current {
                        self.broadcast.end_detail = Some(detail);
                    }
                }
            }
            return false;
        }
        self.broadcast.ended_at = Some(now.max(self.broadcast.started_at));
        self.broadcast.end_cause = Some(cause);
        self.broadcast.end_detail = detail;
        true
    }

    /// Notices due for this broadcast, in the order they happened.
    pub fn due_events(&self) -> Vec<LiveEventKey> {
        if self.broadcast.generation == 0 {
            return Vec::new();
        }
        let mut keys = vec![LiveEventKey::start()];
        // A 换台 continues the 开播: while that first notice is still queued
        // (or being retried) the session's opening line is the one to chase.
        let start_answered = self
            .delivery
            .get(&LiveEventKey::start())
            .is_some_and(|record| {
                matches!(record.state, DeliveryState::Delivered | DeliveryState::Off)
            });
        if self.broadcast.switch_seq > 0 && start_answered {
            keys.push(LiveEventKey::switch(self.broadcast.switch_seq));
        }
        if self.broadcast.ended_at.is_some() {
            keys.push(LiveEventKey::end());
        }
        keys
    }

    /// Addendum D `pending_live_events` (one entry per kind, not per switch).
    pub fn pending(&self) -> Vec<&'static str> {
        let mut kinds: Vec<&'static str> = self
            .delivery
            .iter()
            .filter(|(_, record)| record.state == DeliveryState::Pending)
            .map(|(key, _)| key.kind.as_str())
            .collect();
        kinds.dedup();
        kinds
    }

    /// When the worker must wake for an ambiguous retry or a held notice's
    /// periodic active retry.
    pub fn next_retry_at(&self) -> Option<u64> {
        self.delivery
            .values()
            .filter_map(|record| match record.state {
                DeliveryState::Sending
                    if record.channel == Channel::Active
                        && record.attempts < AMBIGUOUS_MAX_ATTEMPTS =>
                {
                    Some(retry_at(record))
                }
                DeliveryState::Pending => Some(record.at + PENDING_ACTIVE_RETRY_SECS),
                _ => None,
            })
            .min()
    }

    pub fn clamp_future(&mut self, now: u64) {
        let limit = now + super::ledger::MAX_SKEW_SECS;
        self.delivery.retain(|_, record| record.at <= limit);
        self.active_push.retain(|_, push| push.last_at <= limit);
    }

    fn write(&mut self, key: LiveEventKey, mut record: DeliveryRecord) {
        let seq = self.delivery.get(&key).map(|old| old.seq).unwrap_or(0) + 1;
        record.seq = seq;
        self.delivery.insert(key, record);
    }

    /// Keep only the current switch's record: a superseded 换台 is never sent
    /// and must not ride along in heartbeats.
    fn prune_switch_records(&mut self) {
        let current = self.broadcast.switch_seq;
        self.delivery
            .retain(|key, _| key.kind != LiveKind::LiveSwitch || key.seq == current);
    }
}

fn merge_records(
    mine: &mut BTreeMap<LiveEventKey, DeliveryRecord>,
    theirs: &BTreeMap<LiveEventKey, DeliveryRecord>,
) {
    for (kind, record) in theirs {
        match mine.get_mut(kind) {
            Some(own) if own.rank() >= record.rank() => {}
            Some(own) => *own = record.clone(),
            None => {
                mine.insert(*kind, record.clone());
            }
        }
    }
}

fn retry_at(record: &DeliveryRecord) -> u64 {
    let index = (record.attempts as usize).min(AMBIGUOUS_RETRY_AT.len() - 1);
    record.first_at + AMBIGUOUS_RETRY_AT[index]
}

/// What the runtime knows when it selects a relay source. No ids beyond the
/// source's platform/channel names, which the status page shows anyway.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LiveSource {
    pub stream_key: String,
    pub platform: String,
    pub channel: String,
    pub title: String,
}

/// What `LiveLedger::source` did with the selected source.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SourceOutcome {
    /// The room had no open session: this opened generation `u64`.
    Started(u64),
    /// The room stayed live: `(generation, switch ordinal)`.
    Switched(u64, u32),
    /// The same source was selected again: `generation`.
    Same(u64),
}

/// Signals from the streaming runtime; queued without waiting.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LiveSignal {
    /// A source is selected and the Bilibili room is live.
    Source(LiveSource),
    /// The Bilibili room went offline, with Bilibili's reason for a
    /// `WarningCut` when the danmaku client saw one.
    Ended(BroadcastEndCause, Option<crate::events::EndDetail>),
}

pub fn shanghai_month(now: u64) -> u32 {
    use chrono::Datelike;
    let at = shanghai(now);
    at.year() as u32 * 100 + at.month()
}

fn shanghai(now: u64) -> chrono::DateTime<chrono::FixedOffset> {
    let zone = chrono::FixedOffset::east_opt(SHANGHAI_SECS).expect("valid offset");
    chrono::DateTime::from_timestamp(now as i64, 0)
        .unwrap_or_default()
        .with_timezone(&zone)
}

pub fn rfc3339(at: u64) -> String {
    shanghai(at).to_rfc3339_opts(chrono::SecondsFormat::Secs, false)
}

/// One notice of a relay session: 开播, a 换台, or the single 下播. A held
/// notice retains its event time and names the node sending it now.
pub fn render(key: LiveEventKey, broadcast: &Broadcast, catch_up: bool, sender: &str) -> String {
    let clean = |value: &str, cols: usize| super::format::sanitize(value, cols);
    let mut lines = Vec::new();
    let at = match key.kind {
        LiveKind::LiveStart | LiveKind::LiveSwitch => {
            let entry = match key.kind {
                LiveKind::LiveStart => SourceEntry {
                    platform: broadcast.platform.clone(),
                    channel: broadcast.channel.clone(),
                    title: broadcast.title.clone(),
                    at: broadcast.started_at,
                },
                _ => switched_source(broadcast, key.seq),
            };
            let platform = clean(&entry.platform, 16);
            let channel = clean(&entry.channel, NAME_CHARS);
            let source = if channel.is_empty() {
                "当前频道".to_string()
            } else {
                channel
            };
            let title = clean(&entry.title, TITLE_CHARS);
            lines.push(match key.kind {
                LiveKind::LiveStart => format!("🟢 开始转播 {source}"),
                _ => format!("🟢换台 {source}"),
            });
            push_platform_title(&mut lines, &platform, &title);
            entry.at
        }
        LiveKind::LiveEnd => {
            let end = broadcast.ended_at.unwrap_or(broadcast.started_at);
            let length = super::format::duration_text(end.saturating_sub(broadcast.started_at));
            lines.push("⚪ 本场转播已结束".to_string());
            lines.push(session_line(broadcast, &length));
            if let Some(cause) = broadcast.end_cause {
                let source = clean(&newest_source_channel(broadcast), NAME_CHARS);
                let label = cause.notice_label(&source);
                let detail = broadcast
                    .end_detail
                    .as_ref()
                    .filter(|_| cause == BroadcastEndCause::WarningCut)
                    .map(|d| clean(&d.text, REASON_CHARS))
                    .filter(|text| !text.is_empty());
                lines.push(match detail {
                    Some(text) => format!("结束原因：{label} · {text}"),
                    None => format!("结束原因：{label}"),
                });
            }
            end
        }
    };
    if catch_up {
        lines.push(format!(
            "补发：{}于 {} (UTC+8)",
            match key.kind {
                LiveKind::LiveStart => "开始",
                LiveKind::LiveSwitch => "换台",
                LiveKind::LiveEnd => "结束",
            },
            super::format::utc8_time(at)
        ));
    }
    super::format::with_sender(&lines.join("\n"), sender)
}

fn push_platform_title(lines: &mut Vec<String>, platform: &str, title: &str) {
    match (platform.is_empty(), title.is_empty()) {
        (false, false) => lines.push(format!("{platform} · {title}")),
        (false, true) => lines.push(platform.to_string()),
        (true, false) => lines.push(title.to_string()),
        (true, true) => {}
    }
}

/// The source relayed last in this session: the newest 换台, else the channel
/// the 开播 named. A source end names it in the `结束原因` line.
fn newest_source_channel(broadcast: &Broadcast) -> String {
    broadcast.newest_source().to_string()
}

/// The source a switch notice announces. Only the newest switch is ever due
/// (`LiveLedger::prune_switch_records`), so the tail of `recent` is it.
fn switched_source(broadcast: &Broadcast, seq: u32) -> SourceEntry {
    if seq == broadcast.switch_seq {
        if let Some(entry) = broadcast.recent.last() {
            return entry.clone();
        }
    }
    SourceEntry {
        platform: broadcast.platform.clone(),
        channel: broadcast.channel.clone(),
        title: broadcast.title.clone(),
        at: broadcast.started_at,
    }
}

/// `本场 橘ひなの -> Kamito，共持续 1 天 3 小时。` A session that never
/// switched keeps the older `本场从 … 开始` wording.
fn session_line(broadcast: &Broadcast, length: &str) -> String {
    let clean = |value: &str| super::format::sanitize(value, NAME_CHARS);
    let first = clean(&broadcast.channel);
    let mut names: Vec<String> = Vec::new();
    if !first.is_empty() {
        names.push(first);
    }
    names.extend(
        broadcast
            .recent
            .iter()
            .map(|entry| clean(&entry.channel))
            .filter(|name| !name.is_empty()),
    );
    let total = broadcast.switch_seq as usize + 1;
    match names.len() {
        0 => format!("本场共持续 {length}。"),
        1 => format!("本场从 {} 开始，共持续 {length}。", names[0]),
        _ if total <= RECENT_SOURCES + 1 => {
            format!("本场 {}，共持续 {length}。", names.join(" -> "))
        }
        _ => {
            let last = names.last().cloned().unwrap_or_default();
            format!(
                "本场 {} -> …（共 {total} 个频道）-> {last}，共持续 {length}。",
                names[0]
            )
        }
    }
}

/// One C2C request for a live notice.
pub struct LiveSend<'a> {
    pub attempt: super::sink::AttemptContext,
    pub at: u64,
    pub mode: QqMode,
    pub msg_id: Option<&'a str>,
    pub msg_seq: u32,
    pub text: &'a str,
}

// async_trait marks the boxed future #[must_use] on its own.
#[allow(clippy::double_must_use)]
#[async_trait::async_trait]
pub trait LiveTransport: Send + Sync {
    async fn send(&self, request: LiveSend<'_>) -> ApiResult;
}

/// Writes the ledger to the node's synced state and waits for the commit.
#[allow(clippy::double_must_use)]
#[async_trait::async_trait]
pub trait LiveCommit: Send + Sync {
    async fn commit(&self, ledger: &NotifyLedger) -> io::Result<()>;
}

/// How QQ answered, for the ladder.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Answer {
    Accepted,
    NoPermission,
    RateLimited,
    PassiveClosed,
    /// Network error, timeout, 5xx, or anything without a clear refusal.
    Ambiguous,
    /// A definite refusal of this channel that is not a fallback trigger
    /// (wakeup exhausted, content rejected …).
    Refused,
}

fn answer(result: &ApiResult) -> Answer {
    if result.ok {
        return Answer::Accepted;
    }
    match result.code {
        Some(CODE_NO_PERMISSION) => Answer::NoPermission,
        Some(CODE_RATE_LIMITED) => Answer::RateLimited,
        Some(code) if PASSIVE_CLOSED_CODES.contains(&code) => Answer::PassiveClosed,
        Some(_) if (400..500).contains(&result.http_status) || result.http_status == 200 => {
            Answer::Refused
        }
        _ => Answer::Ambiguous,
    }
}

/// What one `drive` did, for status, logs and tests.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DriveReport {
    /// `(kind, channel)` accepted by QQ.
    pub delivered: Vec<(LiveKind, Channel)>,
    /// Kinds that ended in `pending` this round (`skipped_quota`).
    pub skipped: Vec<LiveKind>,
    /// A `sending` write failed: nothing was sent for that kind.
    pub commit_failed: bool,
    /// Requests made, in order (`channel` per request).
    pub requests: Vec<(LiveKind, Channel)>,
    /// The ledger changed and should be saved/published.
    pub changed: bool,
    /// `(code, refused_since, month_sent)` when active refusals began.
    pub refusal_began: Option<(i64, u64, u32)>,
}

pub struct Driver<'a> {
    pub ledger: &'a mut NotifyLedger,
    pub passive: Option<&'a mut PassiveTarget>,
    /// `Some(term)` while this node holds the gateway; `None` stops sending.
    pub term: Option<u64>,
    /// Stable node id for the ledger and the configured display name for text.
    pub node: &'a str,
    pub sender_name: &'a str,
    pub enabled: bool,
    pub now: u64,
}

/// Live events may use a wakeup only outside the alert reserve.
pub fn wakeup_allowed(ledger: &NotifyLedger, now: u64) -> bool {
    ledger.wakeup.admin_last_c2c_at.is_some()
        && ledger.wakeup_available(now)
        && ledger
            .wakeups_left(now)
            .is_some_and(|left| left > WAKEUP_RESERVE_FOR_ALERTS)
}

/// End of the current open wakeup period (Addendum A `wakeup_next_close_at`).
pub fn wakeup_next_close_at(ledger: &NotifyLedger, now: u64) -> Option<u64> {
    const ENDS: [u64; 4] = [86_400, 3 * 86_400, 7 * 86_400, 30 * 86_400];
    let last = ledger.wakeup.admin_last_c2c_at?;
    let period = ledger.wakeup_period(now)?;
    Some(last + ENDS[period as usize])
}

/// `quota` for /api/notify and the logged-in /status (no ledger ids).
pub fn quota_view(
    ledger: &NotifyLedger,
    passive: Option<&PassiveTarget>,
    now: u64,
) -> serde_json::Value {
    serde_json::json!({
        "wakeup_remaining": ledger.wakeups_left(now).unwrap_or(0),
        "wakeup_reserve": WAKEUP_RESERVE_FOR_ALERTS,
        "wakeup_next_close_at": wakeup_next_close_at(ledger, now),
        "passive_window_open": passive.is_some_and(|target| target.usable(now)),
    })
}

/// The next step for one kind, from its record.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Next {
    Done,
    Wait,
    Active {
        attempts: u32,
        first_at: u64,
    },
    From(Channel),
    PendingRetry,
    /// A held notice retried active first (rev 5 item 21).
    PendingActive,
}

/// `active_ok_at`: when this node's last active send was accepted (0 =
/// never, or refused since).
fn next_step(record: Option<&DeliveryRecord>, term: u64, now: u64, active_ok_at: u64) -> Next {
    let Some(record) = record else {
        return Next::Active {
            attempts: 0,
            first_at: now,
        };
    };
    match record.state {
        DeliveryState::Delivered | DeliveryState::Off => Next::Done,
        // Rev 5 item 21: held notices go back to active first once an
        // active send works again, and otherwise every 10 minutes.
        DeliveryState::Pending
            if active_ok_at > record.at
                || now.saturating_sub(record.at) >= PENDING_ACTIVE_RETRY_SECS =>
        {
            Next::PendingActive
        }
        DeliveryState::Pending => Next::PendingRetry,
        DeliveryState::Refused => Next::From(after(record.channel)),
        DeliveryState::Sending => {
            // Our own ambiguous attempt in this term, or an older term's
            // unfinished one (rule 13): both take the ambiguous-retry path.
            if record.channel == Channel::Active {
                let window_left = now.saturating_sub(record.first_at) < AMBIGUOUS_WINDOW_SECS;
                if record.attempts < AMBIGUOUS_MAX_ATTEMPTS && window_left {
                    if record.term == term && now < retry_at(record) {
                        return Next::Wait;
                    }
                    return Next::Active {
                        attempts: record.attempts,
                        first_at: record.first_at,
                    };
                }
                return Next::From(Channel::Passive);
            }
            if record.term == term {
                // Same term, non-active channel still `sending`: the result
                // was ambiguous (a timeout or network error on passive or
                // wakeup). Addendum E rev 5 item 22 allows moving on to the
                // next channel: that may duplicate the notice if the first
                // request did land, which is accepted (a lost notice is
                // worse), and we never spend the same budget twice.
                return Next::From(after(record.channel));
            }
            // Older term crashed mid passive/wakeup: retry active first (free),
            // accepting a rare duplicate.
            Next::Active {
                attempts: 0,
                first_at: now,
            }
        }
    }
}

fn after(channel: Channel) -> Channel {
    match channel {
        Channel::Active => Channel::Passive,
        Channel::Passive => Channel::Wakeup,
        Channel::Wakeup | Channel::None => Channel::None,
    }
}

impl Driver<'_> {
    fn record(
        &self,
        state: DeliveryState,
        channel: Channel,
        attempts: u32,
        first_at: u64,
    ) -> DeliveryRecord {
        DeliveryRecord {
            state,
            term: self.term.unwrap_or(0),
            channel,
            at: self.now,
            seq: 0,
            attempts,
            first_at,
            held: false,
        }
    }

    fn note_active(&mut self, result: Answer, report: &mut DriveReport, code: Option<i64>) {
        if !matches!(
            result,
            Answer::Accepted | Answer::NoPermission | Answer::RateLimited
        ) {
            // Ambiguous results leave active_push unchanged (rule 7).
            return;
        }
        let month = shanghai_month(self.now);
        let push = self
            .ledger
            .live
            .active_push
            .entry(self.node.to_string())
            .or_default();
        if push.month != month {
            push.month = month;
            push.month_sent = 0;
        }
        match result {
            Answer::Accepted => {
                push.last_result = Some("ok".into());
                push.refused_since = None;
                push.month_sent += 1;
            }
            Answer::NoPermission | Answer::RateLimited => {
                push.last_result = Some(
                    if result == Answer::NoPermission {
                        "no_permission"
                    } else {
                        "rate_limited"
                    }
                    .into(),
                );
                if push.refused_since.is_none() {
                    push.refused_since = Some(self.now);
                    report.refusal_began = Some((code.unwrap_or(0), self.now, push.month_sent));
                }
            }
            _ => {}
        }
        push.last_at = self.now;
    }
}

/// Run the ladder for every due kind. Never holds anything across the
/// network but the ledger it was handed; a `sending` write that fails to
/// commit stops that kind (rule 11).
pub async fn drive(
    driver: &mut Driver<'_>,
    transport: &dyn LiveTransport,
    commit: &dyn LiveCommit,
) -> DriveReport {
    let mut report = DriveReport::default();
    let Some(term) = driver.term else {
        return report;
    };
    let broadcast = driver.ledger.live.broadcast.clone();
    for key in driver.ledger.live.due_events() {
        let kind = key.kind;
        if !driver.enabled {
            if !driver.ledger.live.delivery.contains_key(&key) {
                let record = driver.record(DeliveryState::Off, Channel::None, 0, driver.now);
                driver.ledger.live.write(key, record);
                report.changed = true;
            }
            continue;
        }
        // A held notice retried this round: its fallback never spends a
        // wakeup again (only passive, else back to pending).
        let mut from_pending = driver
            .ledger
            .live
            .delivery
            .get(&key)
            .is_some_and(|record| record.held);
        // At most a handful of steps: active ×3, passive, wakeup, pending.
        for _ in 0..8 {
            let active_ok_at = driver
                .ledger
                .live
                .active_push
                .get(driver.node)
                .filter(|push| push.last_result.as_deref() == Some("ok"))
                .map(|push| push.last_at)
                .unwrap_or(0);
            let step = next_step(
                driver.ledger.live.delivery.get(&key),
                term,
                driver.now,
                active_ok_at,
            );
            let step = match step {
                Next::PendingActive => {
                    from_pending = true;
                    Next::Active {
                        attempts: 0,
                        first_at: driver.now,
                    }
                }
                Next::PendingRetry => {
                    from_pending = true;
                    step
                }
                other => other,
            };
            let channel = match step {
                Next::Done | Next::Wait | Next::PendingActive => break,
                Next::Active { .. } => Channel::Active,
                Next::From(channel) => channel,
                Next::PendingRetry => {
                    if driver
                        .passive
                        .as_deref()
                        .is_some_and(|target| target.usable(driver.now))
                    {
                        Channel::Passive
                    } else {
                        break;
                    }
                }
            };
            let (channel, attempts, first_at) = match (channel, step) {
                (Channel::Active, Next::Active { attempts, first_at }) => {
                    (Channel::Active, attempts + 1, first_at)
                }
                (Channel::Passive, _)
                    if !driver
                        .passive
                        .as_deref()
                        .is_some_and(|target| target.usable(driver.now)) =>
                {
                    (Channel::Wakeup, 0, driver.now)
                }
                (channel, _) => (channel, 0, driver.now),
            };
            let channel = if channel == Channel::Wakeup
                && (from_pending || !wakeup_allowed(driver.ledger, driver.now))
            {
                Channel::None
            } else {
                channel
            };
            if channel == Channel::None {
                let mut record =
                    driver.record(DeliveryState::Pending, Channel::None, 0, driver.now);
                record.held = true;
                driver.ledger.live.write(key, record);
                report.skipped.push(kind);
                report.changed = true;
                break;
            }
            // Rule 11: commit `sending` before the request, or do not send.
            let mut sending = driver.record(DeliveryState::Sending, channel, attempts, first_at);
            sending.held = from_pending;
            let previous = driver.ledger.live.delivery.get(&key).cloned();
            driver.ledger.live.write(key, sending);
            report.changed = true;
            if commit.commit(driver.ledger).await.is_err() {
                // Not sent: put the old record back so nothing claims a send.
                match previous {
                    Some(previous) => {
                        driver.ledger.live.delivery.insert(key, previous);
                    }
                    None => {
                        driver.ledger.live.delivery.remove(&key);
                    }
                }
                report.commit_failed = true;
                tracing::warn!("QQ {}通知未发送：发送记录写入失败，稍后重试", kind.label());
                break;
            }
            let catch_up = from_pending
                || previous
                    .as_ref()
                    .is_some_and(|record| record.state == DeliveryState::Pending);
            let text = render(key, &broadcast, catch_up, driver.sender_name);
            let (mode, msg_id, seq) = match channel {
                Channel::Passive => {
                    let target = driver.passive.as_deref().expect("usable passive target");
                    (
                        QqMode::Passive,
                        Some(target.msg_id.clone()),
                        target.replies_used + 1,
                    )
                }
                Channel::Wakeup => (QqMode::Wakeup, None, 0),
                _ => (QqMode::Active, None, 0),
            };
            report.requests.push((kind, channel));
            let result = transport
                .send(LiveSend {
                    attempt: super::sink::AttemptContext {
                        key: format!("bili.{}", key.as_str()),
                        dedup: format!("bili.{}:{}", key.as_str(), broadcast.generation),
                        phase: kind.as_str(),
                        test: false,
                        suppressed: 0,
                        undelivered: driver.ledger.undelivered_pending(),
                    },
                    at: driver.now,
                    mode,
                    msg_id: msg_id.as_deref(),
                    msg_seq: seq,
                    text: &text,
                })
                .await;
            let reached = result.http_status != 0;
            match channel {
                Channel::Passive if reached => {
                    if let Some(target) = driver.passive.as_deref_mut() {
                        target.replies_used = seq;
                    }
                }
                Channel::Wakeup if reached => driver.ledger.record_wakeup(driver.now),
                _ => {}
            }
            let mut got = answer(&result);
            if channel == Channel::Active && matches!(got, Answer::Refused | Answer::PassiveClosed)
            {
                // M1 / rule 17: on the active channel only 40034105 and
                // 40034100 trigger the fallback. Any other definite error
                // (an unknown 4xx code …) counts toward the same 3 attempts
                // in 2 minutes, like an ambiguous result.
                got = Answer::Ambiguous;
            }
            if channel == Channel::Active {
                driver.note_active(got, &mut report, result.code);
                if got == Answer::RateLimited {
                    driver.ledger.record_rate_limited(driver.now);
                }
            }
            tracing::debug!(
                "QQ {}通知: channel={} http={} code={:?}",
                kind.label(),
                channel.as_str(),
                result.http_status,
                result.code
            );
            let state = match got {
                Answer::Accepted => DeliveryState::Delivered,
                Answer::Ambiguous if channel == Channel::Active => {
                    // Stays `sending`; retried until the limit (rule 7).
                    tracing::warn!(
                        "QQ {}通知主动发送结果不明（{}），第 {} 次",
                        kind.label(),
                        result.reason(),
                        attempts
                    );
                    let exhausted = attempts >= AMBIGUOUS_MAX_ATTEMPTS
                        || driver.now.saturating_sub(first_at) >= AMBIGUOUS_WINDOW_SECS;
                    if !exhausted {
                        break;
                    }
                    DeliveryState::Refused
                }
                Answer::PassiveClosed if channel == Channel::Passive => {
                    if let Some(target) = driver.passive.as_deref_mut() {
                        target.replies_used = PASSIVE_MAX_REPLIES;
                    }
                    DeliveryState::Refused
                }
                _ => DeliveryState::Refused,
            };
            let mut done = driver.record(state, channel, attempts, first_at);
            done.held = from_pending;
            driver.ledger.live.write(key, done);
            if state == DeliveryState::Delivered {
                report.delivered.push((kind, channel));
                // Best effort: a lost `delivered` write only risks a rare
                // duplicate from the next holder (rule 13). Rev 5 item 23:
                // WARN with the generation and kind only.
                if commit.commit(driver.ledger).await.is_err() {
                    tracing::warn!(
                        "QQ 直播通知送达记录写入失败 (generation {}, kind {})",
                        broadcast.generation,
                        kind.as_str()
                    );
                }
                break;
            }
            // Refused (or ambiguous passive/wakeup): the next channel
            // starts again with its own `sending` write (rule 12).
        }
    }
    report
}

#[cfg(test)]
#[path = "live_events_tests.rs"]
mod tests;
