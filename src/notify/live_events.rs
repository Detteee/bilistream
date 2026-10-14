//! 本直播间开播/下播 notices (contract Addendum A rev 2, D, E rev 3).
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
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io;

/// Live events never use the last two open wakeup periods.
pub const WAKEUP_RESERVE_FOR_ALERTS: usize = 2;
/// Ambiguous active results: at most this many active attempts in total…
pub const AMBIGUOUS_MAX_ATTEMPTS: u32 = 3;
/// …within this window from the first one.
pub const AMBIGUOUS_WINDOW_SECS: u64 = 120;
/// Delay before attempt 2 and 3, counted from the first attempt.
const AMBIGUOUS_RETRY_AT: [u64; 3] = [0, 20, 60];
/// QQ refused the active message for good (no permission / rate limited).
pub const CODE_NO_PERMISSION: i64 = 40034105;
pub const CODE_RATE_LIMITED: i64 = 40034100;
/// Passive reply window closed (Addendum E item 3).
pub const PASSIVE_CLOSED_CODES: [i64; 3] = [40034005, 304103, 40034128];
const TITLE_CHARS: usize = 40;
const NAME_CHARS: usize = 40;
const SHANGHAI_SECS: i32 = 8 * 3600;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LiveKind {
    LiveStart,
    LiveEnd,
}

impl LiveKind {
    pub fn as_str(self) -> &'static str {
        match self {
            LiveKind::LiveStart => "live_start",
            LiveKind::LiveEnd => "live_end",
        }
    }
    fn label(self) -> &'static str {
        match self {
            LiveKind::LiveStart => "开播",
            LiveKind::LiveEnd => "下播",
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

/// One broadcast of this room: the counter only moves when a new broadcast
/// starts (a re-call of start-live while it is open reuses it).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Broadcast {
    pub generation: u64,
    /// `platform:channel` of the source; a different one is a new broadcast.
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
    pub delivery: BTreeMap<LiveKind, DeliveryRecord>,
    /// Highest gateway-holder term handed out.
    #[serde(default)]
    pub term: u64,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub active_push: BTreeMap<String, ActivePush>,
}

impl LiveLedger {
    pub fn is_empty(&self) -> bool {
        *self == LiveLedger::default()
    }

    /// Commutative, associative, idempotent.
    pub fn join(&mut self, other: &LiveLedger) {
        self.term = self.term.max(other.term);
        let mine = &self.broadcast;
        let theirs = &other.broadcast;
        if (theirs.generation, theirs.started_at, &theirs.stream_key)
            > (mine.generation, mine.started_at, &mine.stream_key)
        {
            // A newer broadcast: older records are cleared (rule 14).
            let ended =
                if theirs.generation == mine.generation && theirs.stream_key == mine.stream_key {
                    theirs.ended_at.max(mine.ended_at)
                } else {
                    theirs.ended_at
                };
            if theirs.generation > mine.generation {
                self.delivery = other.delivery.clone();
            } else {
                merge_records(&mut self.delivery, &other.delivery);
            }
            self.broadcast = theirs.clone();
            self.broadcast.ended_at = ended;
        } else if theirs.generation == mine.generation {
            if theirs.started_at == mine.started_at && theirs.stream_key == mine.stream_key {
                self.broadcast.ended_at = mine.ended_at.max(theirs.ended_at);
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
    }

    /// Successful start-live on the streaming node. Returns the generation.
    pub fn start(&mut self, info: &LiveStartInfo, now: u64) -> u64 {
        let open = self.broadcast.generation > 0 && self.broadcast.ended_at.is_none();
        if open && self.broadcast.stream_key == info.stream_key {
            // Restart or failover re-calling start-live: same broadcast.
            return self.broadcast.generation;
        }
        self.broadcast = Broadcast {
            generation: self.broadcast.generation + 1,
            stream_key: info.stream_key.clone(),
            platform: crate::logging::safe_text(&info.platform, NAME_CHARS),
            channel: crate::logging::safe_text(&info.channel, NAME_CHARS),
            title: crate::logging::safe_text(&info.title, TITLE_CHARS),
            started_at: now,
            ended_at: None,
        };
        self.delivery.clear();
        self.broadcast.generation
    }

    /// 直播结束 on the active node. Only an open broadcast ends, once.
    pub fn end(&mut self, now: u64) -> bool {
        if self.broadcast.generation == 0 || self.broadcast.ended_at.is_some() {
            return false;
        }
        self.broadcast.ended_at = Some(now.max(self.broadcast.started_at));
        true
    }

    /// Kinds that are due for this broadcast.
    pub fn due_kinds(&self) -> Vec<LiveKind> {
        if self.broadcast.generation == 0 {
            return Vec::new();
        }
        let mut kinds = vec![LiveKind::LiveStart];
        if self.broadcast.ended_at.is_some() {
            kinds.push(LiveKind::LiveEnd);
        }
        kinds
    }

    /// Addendum D `pending_live_events`.
    pub fn pending(&self) -> Vec<&'static str> {
        self.delivery
            .iter()
            .filter(|(_, record)| record.state == DeliveryState::Pending)
            .map(|(kind, _)| kind.as_str())
            .collect()
    }

    /// When the worker must wake for an ambiguous retry.
    pub fn next_retry_at(&self) -> Option<u64> {
        self.delivery
            .values()
            .filter(|record| {
                record.state == DeliveryState::Sending
                    && record.channel == Channel::Active
                    && record.attempts < AMBIGUOUS_MAX_ATTEMPTS
            })
            .map(retry_at)
            .min()
    }

    pub fn clamp_future(&mut self, now: u64) {
        let limit = now + super::ledger::MAX_SKEW_SECS;
        self.delivery.retain(|_, record| record.at <= limit);
        self.active_push.retain(|_, push| push.last_at <= limit);
    }

    fn write(&mut self, kind: LiveKind, mut record: DeliveryRecord) {
        let seq = self.delivery.get(&kind).map(|old| old.seq).unwrap_or(0) + 1;
        record.seq = seq;
        self.delivery.insert(kind, record);
    }
}

fn merge_records(
    mine: &mut BTreeMap<LiveKind, DeliveryRecord>,
    theirs: &BTreeMap<LiveKind, DeliveryRecord>,
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

/// What the runtime knows at a successful start-live. No ids beyond the
/// source's platform/channel names, which the status page shows anyway.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LiveStartInfo {
    pub stream_key: String,
    pub platform: String,
    pub channel: String,
    pub title: String,
}

/// Signals from the streaming runtime; queued without waiting.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LiveSignal {
    Started(LiveStartInfo),
    Ended,
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

fn short_time(at: u64) -> String {
    shanghai(at).format("%m-%d %H:%M").to_string()
}

fn duration_text(secs: u64) -> String {
    let minutes = secs / 60;
    if minutes >= 60 {
        format!("{}小时{}分", minutes / 60, minutes % 60)
    } else {
        format!("{minutes}分")
    }
}

/// qq-notify-alert-formats.md §3.5. No revenue, no totals, no ids.
pub fn render(kind: LiveKind, broadcast: &Broadcast, catch_up: bool) -> String {
    let clean = |value: &str, cols: usize| super::format::sanitize(value, cols);
    let platform = clean(&broadcast.platform, 16);
    let channel = clean(&broadcast.channel, NAME_CHARS);
    let source = if channel.is_empty() {
        platform.clone()
    } else {
        channel.clone()
    };
    let (text, at) = match kind {
        LiveKind::LiveStart => {
            let title = clean(&broadcast.title, TITLE_CHARS);
            let title = if title.is_empty() {
                "（无标题）".to_string()
            } else {
                title
            };
            let from = match (platform.is_empty(), channel.is_empty()) {
                (false, false) => format!("{platform}/{channel}"),
                _ => source,
            };
            (
                format!("开播 · {title} · 来源 {from}"),
                broadcast.started_at,
            )
        }
        LiveKind::LiveEnd => {
            let end = broadcast.ended_at.unwrap_or(broadcast.started_at);
            let length = duration_text(end.saturating_sub(broadcast.started_at));
            (format!("下播 · 本场 {length} · 来源 {source}"), end)
        }
    };
    if catch_up {
        format!("（补发）{text} · {}", short_time(at))
    } else {
        text
    }
}

/// One C2C request for a live notice.
pub struct LiveSend<'a> {
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
    pub node: &'a str,
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
    Active { attempts: u32, first_at: u64 },
    From(Channel),
    PendingRetry,
}

fn next_step(record: Option<&DeliveryRecord>, term: u64, now: u64) -> Next {
    let Some(record) = record else {
        return Next::Active {
            attempts: 0,
            first_at: now,
        };
    };
    match record.state {
        DeliveryState::Delivered | DeliveryState::Off => Next::Done,
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
                // was ambiguous; never spend that budget twice. Move on.
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
    for kind in driver.ledger.live.due_kinds() {
        if !driver.enabled {
            if !driver.ledger.live.delivery.contains_key(&kind) {
                let record = driver.record(DeliveryState::Off, Channel::None, 0, driver.now);
                driver.ledger.live.write(kind, record);
                report.changed = true;
            }
            continue;
        }
        // At most a handful of steps: active ×3, passive, wakeup, pending.
        for _ in 0..8 {
            let step = next_step(driver.ledger.live.delivery.get(&kind), term, driver.now);
            let channel = match step {
                Next::Done | Next::Wait => break,
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
            let channel =
                if channel == Channel::Wakeup && !wakeup_allowed(driver.ledger, driver.now) {
                    Channel::None
                } else {
                    channel
                };
            if channel == Channel::None {
                let record = driver.record(DeliveryState::Pending, Channel::None, 0, driver.now);
                driver.ledger.live.write(kind, record);
                report.skipped.push(kind);
                report.changed = true;
                break;
            }
            // Rule 11: commit `sending` before the request, or do not send.
            let sending = driver.record(DeliveryState::Sending, channel, attempts, first_at);
            let previous = driver.ledger.live.delivery.get(&kind).cloned();
            driver.ledger.live.write(kind, sending);
            report.changed = true;
            if commit.commit(driver.ledger).await.is_err() {
                // Not sent: put the old record back so nothing claims a send.
                match previous {
                    Some(previous) => {
                        driver.ledger.live.delivery.insert(kind, previous);
                    }
                    None => {
                        driver.ledger.live.delivery.remove(&kind);
                    }
                }
                report.commit_failed = true;
                tracing::warn!("QQ {}通知未发送：发送记录写入失败，稍后重试", kind.label());
                break;
            }
            let catch_up = previous
                .as_ref()
                .is_some_and(|record| record.state == DeliveryState::Pending);
            let text = render(kind, &broadcast, catch_up);
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
            let got = answer(&result);
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
            let done = driver.record(state, channel, attempts, first_at);
            driver.ledger.live.write(kind, done);
            if state == DeliveryState::Delivered {
                report.delivered.push((kind, channel));
                // Best effort: a lost `delivered` write only risks a rare
                // duplicate from the next holder (rule 13).
                let _ = commit.commit(driver.ledger).await;
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
