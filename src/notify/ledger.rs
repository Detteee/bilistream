//! Cluster-wide notify ledger (design §8.3).
//!
//! All nodes share one QQ bot, so what was already sent and how much of the
//! shared quota was spent travels in heartbeat snapshots. Every node joins
//! every ledger it sees. `join` is commutative, associative and idempotent
//! (only maxima and unions), so wall-clock skew between nodes and the order
//! heartbeats arrive in never change the result. The ledger never holds a
//! secret, a token or the admin openid.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const MAX_INCIDENT_MARKS: usize = 32;
/// Fifty channel episodes plus their three per-account compatibility marks
/// must not evict each other (or displace fault-incident history).
pub const MAX_WATCH_MARKS: usize = super::config::WATCH_MAX_CHANNELS * 4;
/// Shared active-message budget. Below the unverified-bot limits printed in
/// the QQ docs (30/min bot-wide, 20/min per user, 1000/day), which are
/// themselves unconfirmed.
pub const ACTIVE_PER_MINUTE: usize = 10;
pub const ACTIVE_PER_DAY: u32 = 200;
pub const ACTIVE_REPROBE_SECS: u64 = 86_400;
/// Peer timestamps further ahead than this are a fast clock, not facts.
pub const MAX_SKEW_SECS: u64 = 300;
/// The longest rate-limit block this build ever sets is 60 s; anything
/// further ahead than skew + this is bogus and dropped (review N1).
pub const MAX_RATE_LIMIT_SECS: u64 = 600;
/// Per-node last-send entries kept in the ledger.
pub const MAX_LAST_SEND: usize = 16;
/// `is_wakeup` periods after the admin's last message: today, 1–3, 3–7 and
/// 7–30 days.
const WAKEUP_PERIOD_ENDS: [u64; 4] = [86_400, 3 * 86_400, 7 * 86_400, 30 * 86_400];
const UTC8_OFFSET: u64 = 8 * 3600;
const MAX_ACTIVE_MINUTE: usize = ACTIVE_PER_MINUTE * 2;
const MAX_REASON_CHARS: usize = 80;

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct NotifyLedger {
    pub updated_at: u64,
    /// node_id of the newest writer (informational).
    #[serde(default)]
    pub author: String,
    #[serde(default)]
    pub incidents: Vec<IncidentMark>,
    /// Active sends per author node and UTC+8 day (G-counter: max per
    /// author, sum for the value), so two senders never undercount.
    #[serde(default)]
    pub active_day: BTreeMap<String, DayCount>,
    /// Active send times, newest last.
    #[serde(default)]
    pub active_minute: Vec<u64>,
    /// QQ answered 429 / 40034100: no send before this time.
    #[serde(default)]
    pub rate_limited_until: u64,
    #[serde(default)]
    pub wakeup: WakeupState,
    #[serde(default)]
    pub modes: ModeMarks,
    /// Undelivered alerts per author node (G-counter).
    #[serde(default)]
    pub undelivered_total: BTreeMap<String, u32>,
    /// Per author: how much of `undelivered_total` a delivered message
    /// already reported.
    #[serde(default)]
    pub undelivered_reported: BTreeMap<String, u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_reason: Option<TimedReason>,
    /// Last real (non-test) send attempt per sender node, so standbys can
    /// show `sender.last_send`. `{at, ok, code, via}` only: no reason text,
    /// no trace or message ids. Joined by max per node (order-independent).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub last_send: BTreeMap<String, LastSend>,
    /// 开播/下播 broadcast, delivery records, gateway term and `active_push`
    /// (Addendum A/E). No ids, no msg_id, no openid.
    #[serde(
        default,
        skip_serializing_if = "super::live_events::LiveLedger::is_empty"
    )]
    pub live: super::live_events::LiveLedger,
    /// This node's id, used as the counter key. Never serialized or joined.
    #[serde(skip)]
    pub local: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct IncidentMark {
    /// `EventKind` key + node (+ variant); see `event::dedup_key`.
    pub key: String,
    pub notified_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved_at: Option<u64>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
pub struct DayCount {
    /// Days since the epoch in UTC+8.
    pub day: u64,
    pub count: u32,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct WakeupState {
    /// Time of the admin's last C2C message seen by a gateway session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub admin_last_c2c_at: Option<u64>,
    /// `(cycle start, period index)` pairs; only the current cycle counts.
    #[serde(default)]
    pub used: Vec<(u64, u8)>,
    /// Without a known last message, at most one wakeup attempt per day.
    #[serde(default)]
    pub unknown_attempt_at: u64,
}

/// Delivery-mode facts as timestamps, so the join stays a plain maximum and
/// a later reset wins everywhere.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ModeMarks {
    /// QQ refused active messages (40034105).
    #[serde(default)]
    pub active_unavailable_at: u64,
    /// The admin turned 主动消息 off (40054013 / C2C_MSG_REJECT) or on.
    #[serde(default)]
    pub admin_rejected_at: u64,
    #[serde(default)]
    pub admin_accepted_at: u64,
    #[serde(default)]
    pub markdown_unsupported_at: u64,
    /// Credentials or settings changed: every sticky fact above is void.
    #[serde(default)]
    pub reset_at: u64,
}

impl ModeMarks {
    /// Sticky for a day, then re-probed (one spurious 40034105 must not
    /// disable active sends forever).
    pub fn active_unavailable(&self, now: u64) -> bool {
        self.active_unavailable_at > self.reset_at
            && now.saturating_sub(self.active_unavailable_at) < ACTIVE_REPROBE_SECS
    }
    pub fn admin_rejected(&self) -> bool {
        self.admin_rejected_at > self.reset_at.max(self.admin_accepted_at)
    }
    pub fn markdown_unsupported(&self) -> bool {
        self.markdown_unsupported_at > self.reset_at
    }
}

/// One node's last send attempt as advertised in heartbeats. Field order
/// matters: the derived `Ord` compares `at` first, so the newest wins.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
pub struct LastSend {
    pub at: u64,
    pub ok: bool,
    pub code: Option<i64>,
    /// Delivery mode: `active`, `passive` or `wakeup`.
    pub via: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
pub struct TimedReason {
    pub at: u64,
    pub reason: String,
}

pub fn utc8_day(now: u64) -> u64 {
    (now + UTC8_OFFSET) / 86_400
}

impl NotifyLedger {
    pub fn touch(&mut self, author: &str, now: u64) {
        if now >= self.updated_at {
            self.updated_at = now;
            self.author = author.to_string();
        }
    }

    pub fn mark(&self, key: &str) -> Option<&IncidentMark> {
        self.incidents.iter().find(|mark| mark.key == key)
    }

    pub fn record_notified(&mut self, key: &str, at: u64) {
        match self.incidents.iter_mut().find(|mark| mark.key == key) {
            Some(mark) if mark.notified_at >= at => {}
            Some(mark) => {
                mark.notified_at = at;
                mark.resolved_at = None;
            }
            None => self.incidents.push(IncidentMark {
                key: key.to_string(),
                notified_at: at,
                resolved_at: None,
            }),
        }
        self.trim_marks();
    }

    pub fn record_resolved(&mut self, key: &str, at: u64) {
        if let Some(mark) = self.incidents.iter_mut().find(|mark| mark.key == key) {
            mark.resolved_at = mark.resolved_at.max(Some(at));
        }
    }

    fn trim_marks(&mut self) {
        // Deterministic order makes the join independent of arrival order.
        self.incidents.sort_by(|a, b| {
            b.notified_at
                .cmp(&a.notified_at)
                .then_with(|| a.key.cmp(&b.key))
        });
        let (mut incidents, mut watch) = (0, 0);
        self.incidents.retain(|mark| {
            let watch_key = mark
                .key
                .strip_prefix("stream.live:")
                .and_then(|key| key.split_once(':'))
                .is_some_and(|(kind, name)| {
                    matches!(
                        kind,
                        "configured" | "manual" | "youtube" | "twitch" | "niconico"
                    ) && !name.is_empty()
                });
            let (count, max) = if watch_key {
                (&mut watch, MAX_WATCH_MARKS)
            } else {
                (&mut incidents, MAX_INCIDENT_MARKS)
            };
            *count += 1;
            *count <= max
        });
    }

    fn minute_count(&self, now: u64) -> usize {
        self.active_minute
            .iter()
            .filter(|at| now.saturating_sub(**at) < 60)
            .count()
    }

    pub fn active_budget_left(&self, now: u64) -> bool {
        let day_count = self.active_today(now);
        now >= self.rate_limited_until
            && self.minute_count(now) < ACTIVE_PER_MINUTE
            && day_count < ACTIVE_PER_DAY
    }

    /// Charged per attempt, not per success: a timed-out send may have landed.
    pub fn record_active(&mut self, now: u64) {
        // The wire format is a set of second stamps. Allocate an unused
        // nearby stamp for every burst attempt, including after importing a
        // former sender's ledger. Future stamps count conservatively until
        // they age out; simply using now + 1 loses the third same-second send.
        let stamp = (0..=MAX_ACTIVE_MINUTE)
            .map(|offset| now.saturating_add(offset as u64))
            .find(|stamp| !self.active_minute.contains(stamp))
            .unwrap_or(now);
        self.active_minute.push(stamp);
        self.normalize_minute();
        let today = utc8_day(now);
        let own = self.active_day.entry(self.local.clone()).or_default();
        if own.day != today {
            *own = DayCount {
                day: today,
                count: 0,
            };
        }
        own.count += 1;
        // Older days never matter again.
        self.active_day.retain(|_, count| count.day + 1 >= today);
    }

    /// Active sends today across all authors.
    pub fn active_today(&self, now: u64) -> u32 {
        let today = utc8_day(now);
        self.active_day
            .values()
            .filter(|count| count.day == today)
            .map(|count| count.count)
            .sum()
    }

    fn normalize_minute(&mut self) {
        self.active_minute.sort_unstable();
        self.active_minute.dedup();
        if self.active_minute.len() > MAX_ACTIVE_MINUTE {
            let excess = self.active_minute.len() - MAX_ACTIVE_MINUTE;
            self.active_minute.drain(..excess);
        }
    }

    pub fn record_rate_limited(&mut self, now: u64) {
        self.rate_limited_until = self.rate_limited_until.max(now + 60);
    }

    /// The current wakeup period, or `None` once 30 days have passed.
    pub fn wakeup_period(&self, now: u64) -> Option<u8> {
        let last = self.wakeup.admin_last_c2c_at?;
        let elapsed = now.saturating_sub(last);
        WAKEUP_PERIOD_ENDS
            .iter()
            .position(|end| elapsed < *end)
            .map(|index| index as u8)
    }

    fn period_used(&self, period: u8) -> bool {
        let cycle = self.wakeup.admin_last_c2c_at.unwrap_or_default();
        self.wakeup.used.contains(&(cycle, period))
    }

    pub fn wakeup_available(&self, now: u64) -> bool {
        match self.wakeup.admin_last_c2c_at {
            Some(_) => self
                .wakeup_period(now)
                .is_some_and(|period| !self.period_used(period)),
            None => now.saturating_sub(self.wakeup.unknown_attempt_at) >= 86_400,
        }
    }

    /// Wakeups left in the current 30-day cycle, when it is known.
    pub fn wakeups_left(&self, now: u64) -> Option<usize> {
        let period = self.wakeup_period(now)?;
        Some(
            (period..WAKEUP_PERIOD_ENDS.len() as u8)
                .filter(|index| !self.period_used(*index))
                .count(),
        )
    }

    /// After a wakeup sent now: how many are left and when the next one can
    /// go (start of the next unused period). `None` when the cycle is unknown.
    pub fn wakeup_after_use(&self, now: u64) -> Option<(usize, Option<u64>)> {
        let last = self.wakeup.admin_last_c2c_at?;
        let period = self.wakeup_period(now)?;
        let left = self.wakeups_left(now)?.saturating_sub(1);
        let next = ((period + 1)..WAKEUP_PERIOD_ENDS.len() as u8)
            .find(|index| !self.period_used(*index))
            .map(|index| last + WAKEUP_PERIOD_ENDS[index as usize - 1]);
        Some((left, next))
    }

    pub fn record_wakeup(&mut self, now: u64) {
        match (self.wakeup.admin_last_c2c_at, self.wakeup_period(now)) {
            (Some(cycle), Some(period)) => {
                if !self.wakeup.used.contains(&(cycle, period)) {
                    self.wakeup.used.push((cycle, period));
                }
            }
            _ => self.wakeup.unknown_attempt_at = self.wakeup.unknown_attempt_at.max(now),
        }
        self.normalize_wakeup();
    }

    /// The admin messaged the bot: a new 30-day wakeup cycle starts.
    pub fn record_admin_message(&mut self, at: u64) {
        // A start stamped in the future (fast peer clock) must not pin the
        // cycle: a real message now replaces it.
        if self
            .wakeup
            .admin_last_c2c_at
            .is_none_or(|last| at > last || last > at + MAX_SKEW_SECS)
        {
            self.wakeup.admin_last_c2c_at = Some(at);
        }
        self.normalize_wakeup();
    }

    fn normalize_wakeup(&mut self) {
        let cycle = self.wakeup.admin_last_c2c_at.unwrap_or_default();
        self.wakeup.used.retain(|(start, _)| *start >= cycle);
        self.wakeup.used.sort_unstable();
        self.wakeup.used.dedup();
    }

    pub fn record_undelivered(&mut self, count: u32, reason: &str, at: u64) {
        let own = self
            .undelivered_total
            .entry(self.local.clone())
            .or_default();
        *own = own.saturating_add(count);
        self.set_reason(reason, at);
    }

    pub fn set_reason(&mut self, reason: &str, at: u64) {
        // The ledger is advertised in heartbeats: no credentials or ids.
        let reason = mask_ids(&crate::logging::safe_text(reason, MAX_REASON_CHARS * 2));
        let reason: String = reason.chars().take(MAX_REASON_CHARS).collect();
        let next = TimedReason { at, reason };
        if self.last_reason.as_ref().is_none_or(|last| next > *last) {
            self.last_reason = Some(next);
        }
    }

    /// This node made a real send attempt (tests do not count).
    pub fn record_last_send(&mut self, at: u64, ok: bool, code: Option<i64>, via: Option<&str>) {
        let next = LastSend {
            at,
            ok,
            code,
            via: via.map(str::to_string),
        };
        match self.last_send.get_mut(&self.local) {
            Some(own) if *own >= next => {}
            Some(own) => *own = next,
            None => {
                self.last_send.insert(self.local.clone(), next);
            }
        }
        self.trim_last_send();
    }

    fn trim_last_send(&mut self) {
        if self.last_send.len() <= MAX_LAST_SEND {
            return;
        }
        // Deterministic: keep the newest, ties by node id.
        let mut entries: Vec<(String, LastSend)> =
            std::mem::take(&mut self.last_send).into_iter().collect();
        entries.sort_by(|a, b| b.1.at.cmp(&a.1.at).then_with(|| a.0.cmp(&b.0)));
        entries.truncate(MAX_LAST_SEND);
        self.last_send = entries.into_iter().collect();
    }

    pub fn undelivered_pending(&self) -> u32 {
        self.undelivered_total
            .iter()
            .map(|(author, total)| {
                total.saturating_sub(self.undelivered_reported.get(author).copied().unwrap_or(0))
            })
            .sum()
    }

    pub fn mark_undelivered_reported(&mut self) {
        for (author, total) in &self.undelivered_total {
            let reported = self.undelivered_reported.entry(author.clone()).or_default();
            *reported = (*reported).max(*total);
        }
    }

    /// Drop or cap timestamps from the future (a peer with a fast clock, or
    /// a persisted state from before the local clock was fixed). Applied to
    /// peer ledgers before the join and to the loaded state.
    pub fn clamp_future(&mut self, now: u64) {
        let limit = now + MAX_SKEW_SECS;
        self.incidents.retain(|mark| mark.notified_at <= limit);
        for mark in &mut self.incidents {
            if mark.resolved_at.is_some_and(|at| at > limit) {
                mark.resolved_at = Some(limit.min(mark.notified_at.max(now)));
            }
        }
        self.active_minute.retain(|at| *at <= limit);
        let today = utc8_day(limit);
        self.active_day.retain(|_, count| count.day <= today);
        // Dropped, not capped: a capped stamp would be re-capped against the
        // receiver's moving clock on every heartbeat and never expire (N1).
        if self.rate_limited_until > now + MAX_SKEW_SECS + MAX_RATE_LIMIT_SECS {
            self.rate_limited_until = 0;
        }
        self.last_send.retain(|_, send| send.at <= limit);
        if self.wakeup.admin_last_c2c_at.is_some_and(|at| at > limit) {
            self.wakeup.admin_last_c2c_at = None;
        }
        self.wakeup.used.retain(|(start, _)| *start <= limit);
        self.wakeup.unknown_attempt_at = self.wakeup.unknown_attempt_at.min(limit);
        let modes = &mut self.modes;
        for at in [
            &mut modes.active_unavailable_at,
            &mut modes.admin_rejected_at,
            &mut modes.admin_accepted_at,
            &mut modes.markdown_unsupported_at,
            &mut modes.reset_at,
        ] {
            if *at > limit {
                *at = 0;
            }
        }
        if self
            .last_reason
            .as_ref()
            .is_some_and(|reason| reason.at > limit)
        {
            self.last_reason = None;
        }
        self.live.clamp_future(now);
        if self.updated_at > limit {
            self.updated_at = 0;
            self.author.clear();
        }
    }

    /// Join a ledger seen in a peer's heartbeat (see the module docs).
    pub fn join(&mut self, other: &NotifyLedger) {
        if (other.updated_at, &other.author) > (self.updated_at, &self.author) {
            self.updated_at = other.updated_at;
            self.author = other.author.clone();
        }
        for mark in &other.incidents {
            match self.incidents.iter_mut().find(|own| own.key == mark.key) {
                Some(own) if mark.notified_at > own.notified_at => *own = mark.clone(),
                Some(own) if mark.notified_at == own.notified_at => {
                    own.resolved_at = own.resolved_at.max(mark.resolved_at)
                }
                Some(_) => {}
                None => self.incidents.push(mark.clone()),
            }
        }
        self.trim_marks();
        join_max(&mut self.active_day, &other.active_day);
        self.active_minute
            .extend(other.active_minute.iter().copied());
        self.normalize_minute();
        self.rate_limited_until = self.rate_limited_until.max(other.rate_limited_until);
        self.wakeup.admin_last_c2c_at = self
            .wakeup
            .admin_last_c2c_at
            .max(other.wakeup.admin_last_c2c_at);
        self.wakeup.used.extend(other.wakeup.used.iter().copied());
        self.wakeup.unknown_attempt_at = self
            .wakeup
            .unknown_attempt_at
            .max(other.wakeup.unknown_attempt_at);
        self.normalize_wakeup();
        let (mine, theirs) = (&mut self.modes, &other.modes);
        mine.active_unavailable_at = mine.active_unavailable_at.max(theirs.active_unavailable_at);
        mine.admin_rejected_at = mine.admin_rejected_at.max(theirs.admin_rejected_at);
        mine.admin_accepted_at = mine.admin_accepted_at.max(theirs.admin_accepted_at);
        mine.markdown_unsupported_at = mine
            .markdown_unsupported_at
            .max(theirs.markdown_unsupported_at);
        mine.reset_at = mine.reset_at.max(theirs.reset_at);
        join_max(&mut self.undelivered_total, &other.undelivered_total);
        join_max(&mut self.undelivered_reported, &other.undelivered_reported);
        if other.last_reason > self.last_reason {
            self.last_reason = other.last_reason.clone();
        }
        join_max(&mut self.last_send, &other.last_send);
        self.trim_last_send();
        self.live.join(&other.live);
    }
}

/// Reason text bound for a response, the status, a log line or the ledger:
/// the exact configured openid becomes `<set>`, then every other openid-like
/// run is masked (contract: replace, then `mask_ids`).
pub fn mask_reason(text: &str, openid: Option<&str>) -> String {
    let replaced = match openid.map(str::trim).filter(|id| !id.is_empty()) {
        Some(id) => text.replace(id, "<set>"),
        None => text.to_string(),
    };
    mask_ids(&replaced)
}

/// Hide openid-like runs (24+ letters/digits).
pub fn mask_ids(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut run = String::new();
    let flush = |run: &mut String, out: &mut String| {
        if run.chars().count() >= 24 {
            out.push('…');
        } else {
            out.push_str(run);
        }
        run.clear();
    };
    for ch in text.chars() {
        if ch.is_ascii_alphanumeric() {
            run.push(ch);
        } else {
            flush(&mut run, &mut out);
            out.push(ch);
        }
    }
    flush(&mut run, &mut out);
    out
}

fn join_max<V: Ord + Clone>(mine: &mut BTreeMap<String, V>, theirs: &BTreeMap<String, V>) {
    for (key, value) in theirs {
        match mine.get_mut(key) {
            Some(own) if *own >= *value => {}
            Some(own) => *own = value.clone(),
            None => {
                mine.insert(key.clone(), value.clone());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(seed: u64, author: &str) -> NotifyLedger {
        let mut ledger = NotifyLedger {
            local: author.to_string(),
            ..Default::default()
        };
        ledger.touch(author, 1_000 + seed);
        ledger.record_notified(&format!("k{}", seed % 3), 900 + seed);
        ledger.record_notified("shared", 800 + seed * 7 % 5);
        if seed.is_multiple_of(2) {
            ledger.record_resolved("shared", 950 + seed);
        }
        ledger.record_active(1_000 + seed);
        ledger.record_admin_message(500 + seed % 2);
        ledger.record_wakeup(600 + seed);
        ledger.record_undelivered(seed as u32 % 3, &format!("reason {seed}"), 700 + seed);
        ledger.modes.active_unavailable_at = seed * 3;
        ledger.record_last_send(
            1_000 + seed,
            seed.is_multiple_of(2),
            Some(seed as i64),
            Some("active"),
        );
        ledger
    }

    fn joined(order: &[&NotifyLedger]) -> NotifyLedger {
        let mut out = NotifyLedger::default();
        for ledger in order {
            out.join(ledger);
        }
        out
    }

    #[test]
    fn join_is_commutative_associative_and_idempotent() {
        let a = sample(1, "a");
        let b = sample(2, "b");
        let c = sample(5, "c");
        let reference = joined(&[&a, &b, &c]);
        for order in [
            [&a, &c, &b],
            [&b, &a, &c],
            [&b, &c, &a],
            [&c, &a, &b],
            [&c, &b, &a],
        ] {
            assert_eq!(joined(&order), reference);
        }
        let mut twice = reference.clone();
        twice.join(&reference);
        twice.join(&a);
        assert_eq!(twice, reference);
        let mut ab = a.clone();
        ab.join(&b);
        let mut bc = b.clone();
        bc.join(&c);
        let mut left = ab;
        left.join(&c);
        let mut right = a.clone();
        right.join(&bc);
        assert_eq!(left, right);
    }

    #[test]
    fn marks_are_capped_by_recency() {
        let mut ledger = NotifyLedger::default();
        for i in 0..40 {
            ledger.record_notified(&format!("key-{i}"), i);
        }
        assert_eq!(ledger.incidents.len(), MAX_INCIDENT_MARKS);
        assert!(ledger.mark("key-39").is_some());
        assert!(ledger.mark("key-0").is_none());
    }

    #[test]
    fn fifty_watch_episodes_keep_all_aliases_without_displacing_fault_marks() {
        let mut ledger = NotifyLedger::default();
        for i in 0..super::super::config::WATCH_MAX_CHANNELS {
            for kind in ["configured", "youtube", "twitch", "niconico"] {
                ledger.record_notified(&format!("stream.live:{kind}:channel-{i}"), 100 + i as u64);
            }
        }
        for i in 0..40 {
            ledger.record_notified(&format!("cluster.node_down:node-{i}"), 300 + i);
        }
        assert_eq!(ledger.incidents.len(), MAX_WATCH_MARKS + MAX_INCIDENT_MARKS);
        for i in 0..super::super::config::WATCH_MAX_CHANNELS {
            for kind in ["configured", "youtube", "twitch", "niconico"] {
                assert!(ledger
                    .mark(&format!("stream.live:{kind}:channel-{i}"))
                    .is_some());
            }
        }
        assert!(ledger.mark("cluster.node_down:node-39").is_some());
        assert!(ledger.mark("cluster.node_down:node-0").is_none());
        let mut newer = NotifyLedger::default();
        for i in 0..60 {
            newer.record_notified(&format!("stream.live:manual:new-{i}"), 500 + i);
            // Unknown stream.live variants share the normal bounded history.
            newer.record_notified(&format!("stream.live:unknown:{i}"), 600 + i);
        }
        let mut left = ledger.clone();
        left.join(&newer);
        let mut right = newer;
        right.join(&ledger);
        assert_eq!(left, right);
        assert_eq!(left.incidents.len(), MAX_WATCH_MARKS + MAX_INCIDENT_MARKS);
        let once = left.clone();
        left.join(&right);
        assert_eq!(left, once);
    }

    #[test]
    fn active_budget_minute_and_day() {
        let mut ledger = NotifyLedger::default();
        let now = 1_800_000_000;
        for i in 0..ACTIVE_PER_MINUTE as u64 {
            assert!(ledger.active_budget_left(now + i));
            ledger.record_active(now + i);
        }
        assert!(!ledger.active_budget_left(now + 20));
        assert!(ledger.active_budget_left(now + 61 + ACTIVE_PER_MINUTE as u64));
        ledger.active_day.insert(
            "peer".into(),
            DayCount {
                day: utc8_day(now),
                count: ACTIVE_PER_DAY,
            },
        );
        assert!(!ledger.active_budget_left(now + 120));
        // The next UTC+8 day starts a fresh count.
        assert!(ledger.active_budget_left(now + 86_400));
        ledger.record_rate_limited(now + 86_400);
        assert!(!ledger.active_budget_left(now + 86_430));
    }

    #[test]
    fn same_second_bursts_and_sender_handoff_preserve_the_minute_budget() {
        let now = 1_800_000_000;
        let mut first = NotifyLedger {
            local: "a".into(),
            ..Default::default()
        };
        for _ in 0..6 {
            assert!(first.active_budget_left(now));
            first.record_active(now);
        }
        assert_eq!(first.minute_count(now), 6);
        let mut next = NotifyLedger {
            local: "b".into(),
            ..Default::default()
        };
        next.join(&first);
        for _ in 6..ACTIVE_PER_MINUTE {
            assert!(next.active_budget_left(now));
            next.record_active(now);
        }
        assert_eq!(next.minute_count(now), ACTIVE_PER_MINUTE);
        assert_eq!(next.active_today(now), ACTIVE_PER_MINUTE as u32);
        assert!(!next.active_budget_left(now));
        first.join(&next);
        next.join(&first);
        assert!(!first.active_budget_left(now));
        assert_eq!(next.minute_count(now), ACTIVE_PER_MINUTE);
        assert!(next.active_budget_left(now + 60 + ACTIVE_PER_MINUTE as u64));
    }

    #[test]
    fn wakeup_periods_roll_over() {
        let mut ledger = NotifyLedger::default();
        let start = 1_800_000_000;
        assert!(ledger.wakeup_available(start));
        ledger.record_wakeup(start);
        assert!(!ledger.wakeup_available(start + 3600));
        assert!(ledger.wakeup_available(start + 86_400));

        ledger.record_admin_message(start);
        assert_eq!(ledger.wakeups_left(start), Some(4));
        ledger.record_wakeup(start + 10);
        assert!(!ledger.wakeup_available(start + 20));
        assert_eq!(ledger.wakeups_left(start + 20), Some(3));
        assert!(ledger.wakeup_available(start + 2 * 86_400));
        ledger.record_wakeup(start + 2 * 86_400);
        assert_eq!(ledger.wakeups_left(start + 4 * 86_400), Some(2));
        assert!(!ledger.wakeup_available(start + 31 * 86_400));
        // A new admin message starts a new cycle.
        ledger.record_admin_message(start + 31 * 86_400);
        assert_eq!(ledger.wakeups_left(start + 31 * 86_400), Some(4));
    }

    #[test]
    fn mode_marks_reset_and_undelivered_report() {
        let mut ledger = NotifyLedger::default();
        ledger.modes.active_unavailable_at = 10;
        assert!(ledger.modes.active_unavailable(20));
        // Re-probed after a day.
        assert!(!ledger.modes.active_unavailable(10 + ACTIVE_REPROBE_SECS));
        ledger.modes.reset_at = 11;
        assert!(!ledger.modes.active_unavailable(20));
        ledger.modes.admin_rejected_at = 20;
        assert!(ledger.modes.admin_rejected());
        ledger.modes.admin_accepted_at = 21;
        assert!(!ledger.modes.admin_rejected());

        ledger.record_undelivered(2, "主动消息无权限 (40034105)", 30);
        assert_eq!(ledger.undelivered_pending(), 2);
        let stale_peer = ledger.clone();
        ledger.mark_undelivered_reported();
        ledger.join(&stale_peer);
        assert_eq!(ledger.undelivered_pending(), 0);
    }

    #[test]
    fn future_stamps_from_a_fast_peer_are_ignored() {
        let now = 1_800_000_000;
        let mut peer = NotifyLedger {
            local: "fast".into(),
            ..Default::default()
        };
        for i in 0..ACTIVE_PER_MINUTE as u64 {
            peer.record_active(now + 3600 + i);
        }
        peer.rate_limited_until = now + 86_400;
        peer.record_admin_message(now + 7200);
        peer.modes.active_unavailable_at = now + 7200;
        let mut mine = NotifyLedger::default();
        let mut clean = peer.clone();
        clean.clamp_future(now);
        mine.join(&clean);
        // A far-future rate limit is dropped, not capped (N1).
        assert_eq!(mine.rate_limited_until, 0);
        assert!(mine.active_budget_left(now + 10));
        assert_eq!(mine.wakeup.admin_last_c2c_at, None);
        assert!(!mine.modes.active_unavailable(now));
        // An already-poisoned cycle start is replaced by a real message.
        let mut poisoned = NotifyLedger::default();
        poisoned.record_admin_message(now + 7200);
        poisoned.record_admin_message(now);
        assert_eq!(poisoned.wakeup.admin_last_c2c_at, Some(now));
    }

    #[test]
    fn a_fast_peer_rate_limit_never_rolls_forward_over_repeated_merges() {
        // Clerk's rr_m3b: one node a day fast once got a 429. It keeps
        // advertising the same stamp; the receiver must not keep blocking.
        let start = 1_800_000_000;
        let mut fast = NotifyLedger {
            local: "fast".into(),
            ..Default::default()
        };
        fast.record_rate_limited(start + 86_400);
        let mut mine = NotifyLedger {
            local: "me".into(),
            ..Default::default()
        };
        for t in [0, 1800, 3600, 7200, 14_400] {
            let now = start + t;
            mine.clamp_future(now);
            let mut peer = fast.clone();
            peer.clamp_future(now);
            mine.join(&peer);
            assert!(mine.active_budget_left(now + 1), "blocked at +{t}");
            assert_eq!(mine.rate_limited_until, 0);
        }
        // A plausible, near-future stamp from a peer is still honoured once,
        // and expires instead of sliding.
        let mut near = NotifyLedger::default();
        near.record_rate_limited(start + 100);
        let mut peer = near.clone();
        peer.clamp_future(start + 100);
        mine.join(&peer);
        assert!(!mine.active_budget_left(start + 120));
        for t in [200, 400, 800] {
            let mut peer = near.clone();
            peer.clamp_future(start + t);
            mine.join(&peer);
            mine.clamp_future(start + t);
        }
        assert!(mine.active_budget_left(start + 800));
    }

    #[test]
    fn last_send_joins_per_node_in_any_order_without_reasons_or_ids() {
        let mut a = NotifyLedger {
            local: "a".into(),
            ..Default::default()
        };
        let mut b = NotifyLedger {
            local: "b".into(),
            ..Default::default()
        };
        a.record_last_send(100, true, None, Some("active"));
        a.record_last_send(90, false, Some(1), Some("active")); // older: ignored
        b.record_last_send(120, false, Some(11255), Some("passive"));
        let mut ab = a.clone();
        ab.join(&b);
        let mut ba = b.clone();
        ba.join(&a);
        assert_eq!(ab.last_send, ba.last_send);
        assert_eq!(ab.last_send["a"].at, 100);
        assert!(ab.last_send["a"].ok);
        assert_eq!(ab.last_send["b"].code, Some(11255));
        let mut twice = ab.clone();
        twice.join(&ab);
        assert_eq!(twice, ab);
        let json = serde_json::to_string(&ab.last_send).unwrap();
        assert_eq!(
            json,
            r#"{"a":{"at":100,"ok":true,"code":null,"via":"active"},"b":{"at":120,"ok":false,"code":11255,"via":"passive"}}"#
        );
        // From the future: dropped.
        let mut future = b.clone();
        future.record_last_send(10_000, true, None, None);
        future.clamp_future(100);
        assert!(future.last_send.is_empty());
    }

    #[test]
    fn reasons_replace_the_exact_openid_then_mask_ids() {
        let openid = "SAMPLE0OPENID0VALUE0ABCDEF012345";
        let text = format!("QQ 接口返回 11255：invalid request openid={openid} trace=0123456789abcdef0123456789abcdef");
        let masked = mask_reason(&text, Some(openid));
        assert_eq!(
            masked,
            "QQ 接口返回 11255：invalid request openid=<set> trace=…"
        );
        // A short openid (below the 24-char run) is still replaced exactly.
        assert_eq!(
            mask_reason("to ABC123 failed", Some("ABC123")),
            "to <set> failed"
        );
        assert_eq!(mask_reason("plain", None), "plain");
    }

    #[test]
    fn counters_from_two_senders_add_up() {
        let now = 1_800_000_000;
        let mut a = NotifyLedger {
            local: "a".into(),
            ..Default::default()
        };
        let mut b = NotifyLedger {
            local: "b".into(),
            ..Default::default()
        };
        for i in 0..3 {
            a.record_active(now + i);
            b.record_active(now + 100 + i);
        }
        a.record_undelivered(2, "x", now);
        b.record_undelivered(3, "y", now);
        let mut joined = a.clone();
        joined.join(&b);
        assert_eq!(joined.active_today(now + 200), 6);
        assert_eq!(joined.undelivered_pending(), 5);
        joined.mark_undelivered_reported();
        let mut back = b.clone();
        back.join(&joined);
        assert_eq!(back.undelivered_pending(), 0);
    }

    #[test]
    fn ledger_json_has_no_openid_or_secret_fields() {
        let json = serde_json::to_string(&sample(3, "node-a")).unwrap();
        for forbidden in ["openid", "secret", "token", "app_id"] {
            assert!(!json.contains(forbidden), "{forbidden} in {json}");
        }
    }

    #[test]
    fn the_advertised_ledger_carries_no_secret_token_or_openid() {
        let mut ledger = sample(1_000, "n1");
        ledger.record_admin_message(900);
        ledger.record_undelivered(2, "openid ABCDEF0123456789ABCDEF0123456789 token=xyz", 950);
        ledger.set_reason("access_token abc", 960);
        let text = serde_json::to_string(&ledger).unwrap();
        for needle in [
            "ABCDEF0123456789ABCDEF0123456789",
            "token=xyz",
            "access_token abc",
        ] {
            assert!(!text.contains(needle), "{needle} leaked: {text}");
        }
    }
}
