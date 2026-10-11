//! Incident tracker and anti-spam policy (design §4, review §6).
//!
//! Level-triggered: the worker passes the full set of currently-bad
//! conditions on every reconcile, so a lost wake-up can never leave an
//! incident stuck. Pure: the caller passes the clock and the merged ledger.

use super::config::NotifyConfig;
use super::event::{AlertEvent, Condition, EventKind, ItemExtra, Phase, Severity};
use super::ledger::NotifyLedger;
use std::collections::{BTreeMap, HashMap, VecDeque};

/// A queued message that was never sent is dropped after this long.
pub const ITEM_TTL_SECS: u64 = 3600;
/// Ledger marks this much older than a local incident still count as "the
/// cluster already sent it" (confirm timers on two nodes start a little apart).
const LEDGER_MATCH_SLACK_SECS: u64 = 180;
/// Three appearances within this window open a 「状态反复」 alert at once.
pub const FLAP_WINDOW_SECS: u64 = 600;
pub const FLAP_THRESHOLD: usize = 3;
pub(crate) const LATCHED_REASONS: [&str; 4] = [
    "ffmpeg_repeated_failures",
    "external_api_unreachable",
    "network_isolated",
    "node_fault_latched",
];

#[derive(Clone, Debug)]
pub struct PolicyConfig {
    pub confirm_secs: u64,
    pub recovery_hold_secs: u64,
    pub quorum_confirm_secs: u64,
    pub cooldown_secs: u64,
    pub critical_cooldown_secs: u64,
    pub coalesce_secs: u64,
    /// Minutes after midnight UTC+8, `[start, end)`; may wrap past midnight.
    pub quiet: Option<(u32, u32)>,
    pub events: BTreeMap<String, bool>,
}

impl PolicyConfig {
    pub fn from_config(cfg: &NotifyConfig) -> Self {
        Self {
            confirm_secs: cfg.confirm_secs,
            recovery_hold_secs: cfg.recovery_hold_secs,
            quorum_confirm_secs: cfg.quorum_confirm_secs,
            cooldown_secs: cfg.cooldown_secs,
            critical_cooldown_secs: cfg.critical_cooldown_secs,
            coalesce_secs: cfg.coalesce_secs.min(300),
            quiet: cfg.quiet_hours.as_deref().and_then(parse_quiet_hours),
            events: cfg.events.clone(),
        }
    }

    pub fn event_enabled(&self, kind: EventKind) -> bool {
        // The go-live notice has its own switch (watch.enabled).
        if matches!(kind, EventKind::Test | EventKind::StreamLive) {
            return true;
        }
        self.events
            .get(kind.key())
            .copied()
            .unwrap_or(kind.severity() >= Severity::Warning)
    }

    fn confirm_for(&self, kind: EventKind, variant: &str) -> u64 {
        match kind {
            EventKind::NodeDegraded if LATCHED_REASONS.contains(&variant) => 0,
            EventKind::NodeDown | EventKind::NodeDegraded | EventKind::SelfCheckFailing => {
                self.confirm_secs
            }
            EventKind::QuorumLost => self.quorum_confirm_secs,
            EventKind::Failover | EventKind::Test | EventKind::StreamLive => 0,
        }
    }

    fn cooldown_for(&self, kind: EventKind) -> u64 {
        if kind == EventKind::StreamLive {
            // Deduped per stream session (live.rs), not by time.
            0
        } else if kind.severity() == Severity::Critical {
            self.critical_cooldown_secs
        } else {
            self.cooldown_secs
        }
    }

    /// When a warning produced at `now` may go out (end of quiet hours).
    fn release_at(&self, severity: Severity, now: u64) -> u64 {
        let Some((start, end)) = self.quiet else {
            return now;
        };
        if severity == Severity::Critical || start == end {
            return now;
        }
        let local = now + 8 * 3600;
        let minute = ((local % 86_400) / 60) as u32;
        let inside = if start < end {
            minute >= start && minute < end
        } else {
            minute >= start || minute < end
        };
        if !inside {
            return now;
        }
        let wait_minutes = (end + 1440 - minute) % 1440;
        now - (local % 60) + u64::from(wait_minutes) * 60
    }
}

/// "HH:MM-HH:MM" → minutes after midnight.
pub fn parse_quiet_hours(raw: &str) -> Option<(u32, u32)> {
    let (start, end) = raw.trim().split_once('-')?;
    let parse = |part: &str| -> Option<u32> {
        let (hours, minutes) = part.trim().split_once(':')?;
        let hours: u32 = hours.parse().ok()?;
        let minutes: u32 = minutes.parse().ok()?;
        (hours < 24 && minutes < 60).then_some(hours * 60 + minutes)
    };
    Some((parse(start)?, parse(end)?))
}

/// How a reconcile treats the condition set (review §6.3, §0.8).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Normal,
    /// Expected pause (membership change, handoff, startup grace): open
    /// nothing new, hold confirm timers, keep open incidents.
    Freeze,
    /// This node lost the majority. Only the collapsed 「失去多数派」 condition
    /// is meaningful; incidents about other nodes are kept, not resolved.
    Minority,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ItemKind {
    Alert,
    Resolved { duration_secs: u64 },
}

#[derive(Clone, Debug)]
pub struct OutItem {
    pub key: String,
    pub kind: EventKind,
    pub item: ItemKind,
    pub node: String,
    pub detail: Option<String>,
    /// When the incident started (first appearance).
    pub incident_at: u64,
    pub queued_at: u64,
    pub not_before: u64,
    /// Repeats folded into this message (cooldown or flapping).
    pub suppressed: u32,
    /// `Some(n)`: a 「状态反复」 alert, n appearances within the flap window.
    pub flapping: Option<u32>,
    pub extra: ItemExtra,
}

impl OutItem {
    pub fn severity(&self) -> Severity {
        if self.flapping.is_some() && self.item == ItemKind::Alert {
            Severity::Warning
        } else {
            self.kind.severity()
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum State {
    Pending,
    Open,
}

#[derive(Clone, Debug)]
struct Incident {
    cond: Condition,
    first_at: u64,
    /// Confirm timer start; pushed forward while frozen.
    confirm_from: u64,
    state: State,
    clear_since: Option<u64>,
    /// Re-appearances inside the recovery hold.
    suppressed: u32,
    flapping: Option<u32>,
    /// The alert reached QQ (here, or on another node per the ledger).
    notified_at: Option<u64>,
}

#[derive(Clone, Debug, Default)]
struct Cooldown {
    last_at: u64,
    suppressed: u32,
}

#[derive(Default)]
pub struct Tracker {
    incidents: HashMap<String, Incident>,
    cooldowns: HashMap<String, Cooldown>,
    appearances: HashMap<String, VecDeque<u64>>,
    queue: Vec<OutItem>,
}

fn same_node_family(a: &Condition, b: &Condition) -> bool {
    a.node == b.node
        && matches!(a.kind, EventKind::NodeDown | EventKind::NodeDegraded)
        && matches!(b.kind, EventKind::NodeDown | EventKind::NodeDegraded)
}

impl Tracker {
    pub fn open_incidents(&self) -> usize {
        self.incidents.len()
    }

    pub fn queued(&self) -> usize {
        self.queue.len()
    }

    pub fn queue(&self) -> &[OutItem] {
        &self.queue
    }

    /// One-shot events (failover, test, Phase 3 local events).
    pub fn on_event(&mut self, event: AlertEvent, cfg: &PolicyConfig) {
        if event.phase != Phase::Fire || !cfg.event_enabled(event.kind) {
            return;
        }
        let key = event.dedup_key();
        let mut suppressed = 0;
        if event.kind != EventKind::Test {
            let cooldown = self.cooldowns.entry(key.clone()).or_default();
            if cooldown.last_at > 0
                && event.at.saturating_sub(cooldown.last_at) < cfg.cooldown_for(event.kind)
            {
                cooldown.suppressed += 1;
                return;
            }
            suppressed = std::mem::take(&mut cooldown.suppressed);
            cooldown.last_at = event.at;
        }
        self.queue.push(OutItem {
            key,
            kind: event.kind,
            item: ItemKind::Alert,
            node: event.node,
            detail: event.detail,
            incident_at: event.at,
            queued_at: event.at,
            not_before: if event.kind == EventKind::Test {
                event.at
            } else {
                cfg.release_at(event.kind.severity(), event.at)
            },
            suppressed,
            flapping: None,
            extra: event.extra,
        });
    }

    /// Bring incidents in line with the current condition set.
    pub fn reconcile(
        &mut self,
        now: u64,
        active: &BTreeMap<String, Condition>,
        mode: Mode,
        cfg: &PolicyConfig,
    ) {
        for (key, cond) in active {
            if !cfg.event_enabled(cond.kind) {
                continue;
            }
            if let Some(incident) = self.incidents.get_mut(key) {
                if incident.clear_since.take().is_some() {
                    incident.suppressed += 1;
                }
                if cond.detail.is_some() {
                    incident.cond.detail = cond.detail.clone();
                }
                continue;
            }
            if mode == Mode::Freeze {
                continue;
            }
            let seen = self.appearances.entry(key.clone()).or_default();
            seen.push_back(now);
            while seen
                .front()
                .is_some_and(|at| now.saturating_sub(*at) > FLAP_WINDOW_SECS)
            {
                seen.pop_front();
            }
            let flapping = (seen.len() >= FLAP_THRESHOLD).then_some(seen.len() as u32);
            self.incidents.insert(
                key.clone(),
                Incident {
                    cond: cond.clone(),
                    first_at: now,
                    confirm_from: now,
                    state: State::Pending,
                    clear_since: None,
                    suppressed: 0,
                    flapping,
                    notified_at: None,
                },
            );
        }
        let mut gone = Vec::new();
        for (key, incident) in &mut self.incidents {
            if mode == Mode::Freeze && incident.state == State::Pending {
                incident.confirm_from = now;
            }
            if active.contains_key(key) {
                continue;
            }
            let superseded = active
                .values()
                .any(|other| same_node_family(&incident.cond, other));
            let keep_in_minority =
                mode == Mode::Minority && incident.cond.kind != EventKind::QuorumLost;
            if keep_in_minority || (mode == Mode::Freeze && incident.state == State::Open) {
                continue;
            }
            if incident.state == State::Pending || superseded {
                gone.push(key.clone());
            } else if incident.clear_since.is_none() {
                incident.clear_since = Some(now);
            }
        }
        for key in gone {
            self.incidents.remove(&key);
            self.queue
                .retain(|item| !(item.key == key && item.item == ItemKind::Alert));
        }
        // Flap history of keys that are long gone does not need to live forever.
        self.appearances.retain(|key, seen| {
            self.incidents.contains_key(key)
                || seen
                    .back()
                    .is_some_and(|at| now.saturating_sub(*at) <= FLAP_WINDOW_SECS)
        });
    }

    /// Advance timers. `ledger` is the merged cluster view of what was sent.
    pub fn tick(&mut self, now: u64, cfg: &PolicyConfig, ledger: &NotifyLedger) {
        let mut resolved = Vec::new();
        let mut alerts = Vec::new();
        let mut drop_alerts = Vec::new();
        for (key, incident) in &mut self.incidents {
            let sent_elsewhere = ledger.mark(key).filter(|mark| {
                mark.resolved_at.is_none()
                    && mark.notified_at + LEDGER_MATCH_SLACK_SECS >= incident.first_at
            });
            let confirm = if incident.flapping.is_some() {
                0
            } else {
                cfg.confirm_for(incident.cond.kind, &incident.cond.variant)
            };
            if incident.state == State::Pending
                && now.saturating_sub(incident.confirm_from) >= confirm
            {
                incident.state = State::Open;
                if let Some(mark) = sent_elsewhere {
                    incident.notified_at = Some(mark.notified_at);
                } else {
                    let ledger_last = ledger.mark(key).map_or(0, |mark| mark.notified_at);
                    let cooldown = self.cooldowns.entry(key.clone()).or_default();
                    let last = cooldown.last_at.max(ledger_last);
                    if last > 0 && now.saturating_sub(last) < cfg.cooldown_for(incident.cond.kind) {
                        cooldown.suppressed += 1;
                    } else {
                        cooldown.last_at = now;
                        let severity = if incident.flapping.is_some() {
                            Severity::Warning
                        } else {
                            incident.cond.kind.severity()
                        };
                        alerts.push(OutItem {
                            key: key.clone(),
                            kind: incident.cond.kind,
                            item: ItemKind::Alert,
                            node: incident.cond.node.clone(),
                            detail: incident.cond.detail.clone(),
                            incident_at: incident.first_at,
                            queued_at: now,
                            not_before: cfg.release_at(severity, now),
                            suppressed: std::mem::take(&mut cooldown.suppressed),
                            flapping: incident.flapping,
                            extra: ItemExtra::default(),
                        });
                    }
                }
            } else if incident.state == State::Open && incident.notified_at.is_none() {
                if let Some(mark) = sent_elsewhere {
                    incident.notified_at = Some(mark.notified_at);
                    drop_alerts.push(key.clone());
                }
            }
            let hold = if incident.flapping.is_some() {
                cfg.recovery_hold_secs.saturating_mul(2)
            } else {
                cfg.recovery_hold_secs
            };
            if let Some(since) = incident.clear_since {
                if now.saturating_sub(since) >= hold {
                    resolved.push(key.clone());
                }
            }
        }
        self.queue.extend(alerts);
        for key in drop_alerts {
            self.queue
                .retain(|item| !(item.key == key && item.item == ItemKind::Alert));
        }
        for key in resolved {
            let Some(incident) = self.incidents.remove(&key) else {
                continue;
            };
            let had_queued_alert = self
                .queue
                .iter()
                .any(|item| item.key == key && item.item == ItemKind::Alert);
            if had_queued_alert {
                // Never went out: nothing to resolve.
                self.queue
                    .retain(|item| !(item.key == key && item.item == ItemKind::Alert));
                continue;
            }
            if incident.notified_at.is_none() {
                continue;
            }
            let ended = incident.clear_since.unwrap_or(now);
            self.queue.push(OutItem {
                key,
                kind: incident.cond.kind,
                item: ItemKind::Resolved {
                    duration_secs: ended.saturating_sub(incident.first_at),
                },
                node: incident.cond.node,
                detail: None,
                incident_at: incident.first_at,
                queued_at: now,
                not_before: cfg.release_at(incident.cond.kind.severity(), now),
                suppressed: incident.suppressed,
                flapping: incident.flapping,
                extra: ItemExtra::default(),
            });
        }
        self.queue.retain(|item| match item.item {
            // Another node already delivered this resolved message.
            ItemKind::Resolved { .. } => !ledger.mark(&item.key).is_some_and(|mark| {
                mark.resolved_at
                    .is_some_and(|at| at + LEDGER_MATCH_SLACK_SECS >= item.incident_at)
            }),
            // Stateless alerts (failover) are observed by every node; only
            // the first sender's copy goes out.
            ItemKind::Alert => {
                item.kind.stateful()
                    || item.kind == EventKind::Test
                    || !ledger.mark(&item.key).is_some_and(|mark| {
                        mark.notified_at + LEDGER_MATCH_SLACK_SECS >= item.incident_at
                    })
            }
        });
    }

    /// Earliest time `tick` may change something, for the worker's timer.
    /// `with_queue` is false on nodes that cannot send right now, so a
    /// standby with queued copies does not wake every second.
    pub fn next_deadline(&self, cfg: &PolicyConfig, with_queue: bool) -> Option<u64> {
        let incidents = self.incidents.values().filter_map(|incident| {
            if incident.state == State::Pending {
                let confirm = if incident.flapping.is_some() {
                    0
                } else {
                    cfg.confirm_for(incident.cond.kind, &incident.cond.variant)
                };
                Some(incident.confirm_from + confirm)
            } else {
                let hold = if incident.flapping.is_some() {
                    cfg.recovery_hold_secs.saturating_mul(2)
                } else {
                    cfg.recovery_hold_secs
                };
                incident.clear_since.map(|since| since + hold)
            }
        });
        let queue = self.queue.iter().filter(|_| with_queue).map(|item| {
            if item.kind == EventKind::Test {
                item.not_before
            } else {
                item.queued_at.max(item.not_before) + cfg.coalesce_secs
            }
        });
        incidents.chain(queue).min()
    }

    /// Items older than the TTL. The caller decides whether they count as
    /// undelivered.
    pub fn expire(&mut self, now: u64) -> Vec<OutItem> {
        let (expired, kept) = std::mem::take(&mut self.queue)
            .into_iter()
            .partition(|item| now.saturating_sub(item.queued_at) >= ITEM_TTL_SECS);
        self.queue = kept;
        expired
    }

    /// A batch is ready once its oldest releasable item has waited the
    /// coalescing window. A test message goes out at once.
    pub fn take_batch(&mut self, now: u64, cfg: &PolicyConfig) -> Vec<OutItem> {
        let ready: Vec<&OutItem> = self
            .queue
            .iter()
            .filter(|item| item.not_before <= now)
            .collect();
        let Some(oldest) = ready
            .iter()
            .map(|item| item.queued_at.max(item.not_before))
            .min()
        else {
            return Vec::new();
        };
        let has_test = ready.iter().any(|item| item.kind == EventKind::Test);
        if !has_test && now.saturating_sub(oldest) < cfg.coalesce_secs {
            return Vec::new();
        }
        let (mut batch, kept): (Vec<_>, Vec<_>) = std::mem::take(&mut self.queue)
            .into_iter()
            .partition(|item| item.not_before <= now);
        self.queue = kept;
        batch.sort_by(|a, b| {
            let resolved = |item: &OutItem| matches!(item.item, ItemKind::Resolved { .. });
            resolved(a)
                .cmp(&resolved(b))
                .then(b.severity().cmp(&a.severity()))
                .then(a.queued_at.cmp(&b.queued_at))
        });
        batch
    }

    /// Put a batch back after a transient failure (it keeps its age, so the
    /// TTL still applies).
    pub fn requeue(&mut self, mut batch: Vec<OutItem>) {
        batch.append(&mut self.queue);
        self.queue = batch;
    }

    /// Drop queued items the caller may never send (for example kinds the
    /// no-quorum fallback sender is not allowed to report).
    pub fn discard(&mut self, mut drop: impl FnMut(&OutItem) -> bool) {
        self.queue.retain(|item| !drop(item));
    }

    /// Nodes removed from the cluster (membership change) are not
    /// 「已恢复」: drop their incidents and queued items silently.
    pub fn forget_nodes(&mut self, mut gone: impl FnMut(&str) -> bool) {
        self.incidents
            .retain(|_, incident| !gone(&incident.cond.node));
        self.queue
            .retain(|item| !(item.kind.stateful() && gone(&item.node)));
    }

    /// Open (confirmed) incidents, worst first, for the 状态/通知 replies.
    pub fn open_entries(&self) -> Vec<(EventKind, String, u64, Severity)> {
        let mut out: Vec<_> = self
            .incidents
            .values()
            .filter(|incident| incident.state == State::Open && incident.clear_since.is_none())
            .map(|incident| {
                let severity = if incident.flapping.is_some() {
                    Severity::Warning
                } else {
                    incident.cond.kind.severity()
                };
                (
                    incident.cond.kind,
                    incident.cond.node.clone(),
                    incident.first_at,
                    severity,
                )
            })
            .collect();
        out.sort_by(|a, b| b.3.cmp(&a.3).then(a.2.cmp(&b.2)));
        out
    }

    /// Is an item with this key queued?
    pub fn has_queued(&self, key: &str) -> bool {
        self.queue.iter().any(|item| item.key == key)
    }

    /// The batch reached QQ: its incidents count as notified. Only called on
    /// delivery, so a resolved message never follows an alert that failed.
    pub fn delivered(&mut self, batch: &[OutItem], at: u64) {
        for item in batch {
            if item.item == ItemKind::Alert {
                if let Some(incident) = self.incidents.get_mut(&item.key) {
                    incident.notified_at = Some(at);
                }
            }
        }
    }
}

#[cfg(test)]
#[path = "policy_tests.rs"]
mod tests;
