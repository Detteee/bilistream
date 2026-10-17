//! Go-live notices for the 开播通知关注列表 (watch list).
//!
//! Trigger point (documented in design §2.1): a watched channel counts as
//! live when an index row for one of its linked accounts says so. The rows
//! come from two places, both fed into [`LiveWatch::channel_live`]: the
//! YouTube/Twitch/Niconico status card of this node or of any fresh cluster
//! node (only the relayed channel), and the watched roster read from the
//! Holodex/YouTube index by [`super::watch_index`]. The index read is what
//! makes a channel that is merely watched, never restreamed, report going
//! live. The notice is not tied to whether the relay actually started.
//!
//! One notice per channel episode, cluster-wide. All linked platforms share
//! `stream.live:<kind>:<name>`; old per-account marks are recognized too.
//! Only an all-platform offline gap of `END_HOLD_SECS` ends an episode.
//!
//! All detection lives in [`LiveWatch::channel_live`], the single call site
//! the analytics plan will later swap for an `events::emit(ChannelLive)`.

use super::config::{watch_account_ident, WatchCatalog, WatchConfig};
use super::event::{AlertEvent, EventKind, ItemExtra, LiveInfo, Phase};
use super::ledger::NotifyLedger;
use crate::webui::state::StatusData;
use std::collections::{HashMap, HashSet};

/// A gap shorter than this keeps the same stream session.
pub const END_HOLD_SECS: u64 = 600;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LiveObservation {
    pub platform: String,
    pub channel_id: String,
    /// Configured display name from the status card. Empty when a peer status
    /// only carried the platform id.
    pub channel_name: String,
    pub title: Option<String>,
}

impl LiveObservation {
    pub fn ident(&self) -> String {
        watch_account_ident(&self.platform, &self.channel_id)
    }

    /// A name a person can read. A raw YouTube channel id is not one.
    pub fn readable_name(&self) -> Option<String> {
        for value in [self.channel_name.trim(), self.channel_id.trim()] {
            if value.is_empty() {
                continue;
            }
            if self.platform.eq_ignore_ascii_case("youtube") && looks_like_youtube_channel_id(value)
            {
                continue;
            }
            return Some(value.to_string());
        }
        None
    }
}

fn looks_like_youtube_channel_id(value: &str) -> bool {
    let compact: String = value.chars().filter(|ch| !ch.is_whitespace()).collect();
    compact.len() >= 22
        && compact.starts_with("UC")
        && compact
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_')
}

/// Live channels in one node's status.
pub fn observations(status: &StatusData) -> Vec<LiveObservation> {
    let mut out = Vec::new();
    if let Some(yt) = status.youtube.as_ref().filter(|s| s.is_live) {
        out.push((
            "youtube",
            yt.channel_id.clone(),
            yt.channel_name.clone(),
            yt.title.clone(),
        ));
    }
    if let Some(tw) = status.twitch.as_ref().filter(|s| s.is_live) {
        out.push((
            "twitch",
            tw.channel_id.clone(),
            tw.channel_name.clone(),
            tw.title.clone(),
        ));
    }
    if let Some(nico) = status.niconico.as_ref().filter(|s| s.is_live) {
        out.push((
            "niconico",
            nico.channel_id.clone(),
            nico.channel_name.clone(),
            nico.title.clone(),
        ));
    }
    out.into_iter()
        .filter(|(_, id, _, _)| !id.trim().is_empty())
        .map(|(platform, id, name, title)| LiveObservation {
            platform: platform.to_string(),
            channel_id: id.trim().to_string(),
            channel_name: name.trim().to_string(),
            title,
        })
        .collect()
}

pub fn ledger_key(ident: &str) -> String {
    format!("{}:{}", EventKind::StreamLive.key(), ident)
}

/// The ledger says this session was already announced and has not ended.
pub(crate) fn session_open(ledger: &NotifyLedger, key: &str) -> bool {
    ledger
        .mark(key)
        .is_some_and(|mark| mark.resolved_at.is_none_or(|at| at < mark.notified_at))
}

#[derive(Clone, Debug)]
pub struct ResolvedWatchChannel {
    pub kind: &'static str,
    pub name: String,
    pub accounts: Vec<(&'static str, String)>,
}

impl ResolvedWatchChannel {
    pub fn key(&self) -> String {
        ledger_key(&format!("{}:{}", self.kind, self.name))
    }

    pub fn account_keys(&self) -> Vec<String> {
        self.accounts
            .iter()
            .map(|(p, id)| ledger_key(&watch_account_ident(p, id)))
            .collect()
    }

    pub fn episode_keys(&self) -> Vec<String> {
        std::iter::once(self.key())
            .chain(self.account_keys())
            .collect()
    }

    pub fn live_info(&self, seen: &[LiveObservation]) -> Option<LiveInfo> {
        let observed: Vec<_> = self
            .accounts
            .iter()
            .filter_map(|(p, id)| {
                let ident = watch_account_ident(p, id);
                seen.iter().find(|obs| obs.ident() == ident)
            })
            .collect();
        if observed.is_empty() {
            return None;
        }
        Some(LiveInfo {
            platforms: observed.iter().map(|obs| obs.platform.clone()).collect(),
            name: self.name.clone(),
            title: observed
                .iter()
                .find_map(|obs| obs.title.clone().filter(|title| !title.trim().is_empty())),
            // Preserve aliases for old senders and for a renamed configured
            // subscription to the same account during an ongoing episode.
            episode_keys: self.episode_keys(),
        })
    }

    pub fn announced(&self, ledger: &NotifyLedger) -> bool {
        self.resolved_by_accounts(ledger).is_none()
            && self
                .episode_keys()
                .iter()
                .any(|key| session_open(ledger, key))
    }

    /// A renamed group may be absent when its episode closes. Closure of
    /// every linked account at/after that group's notification proves the
    /// obsolete group mark closed too; one closed platform is not enough.
    fn resolved_by_accounts(&self, ledger: &NotifyLedger) -> Option<u64> {
        let key = self.key();
        let group = ledger.mark(&key).filter(|_| session_open(ledger, &key))?;
        let accounts = self.account_keys();
        if accounts.is_empty() {
            return None;
        }
        accounts.iter().try_fold(0, |at, key| {
            let mark = ledger.mark(key)?;
            let resolved_at = mark.resolved_at?;
            (resolved_at >= mark.notified_at && resolved_at >= group.notified_at)
                .then_some(at.max(resolved_at))
        })
    }
}

/// Resolve configured entries at check time. Historical manual entries stay
/// stored for removal only. A later catalog edit can introduce overlap;
/// never emit twice for the same observed account.
pub fn resolve_watch(watch: &WatchConfig, catalog: &WatchCatalog) -> Vec<ResolvedWatchChannel> {
    let mut used = HashSet::new();
    watch
        .channels
        .iter()
        .filter(|entry| entry.kind() == "configured")
        .filter_map(|entry| {
            let platforms = entry.platforms(catalog)?;
            let accounts: Vec<_> = platforms
                .accounts()
                .filter(|(p, id)| used.insert(watch_account_ident(p, id)))
                .map(|(p, id)| (p, id.to_string()))
                .collect();
            (!accounts.is_empty()).then(|| ResolvedWatchChannel {
                kind: entry.kind(),
                name: entry.name().to_string(),
                accounts,
            })
        })
        .collect()
}

#[derive(Default)]
pub struct LiveWatch {
    /// Channel/account mark → last observation of its linked episode. Account
    /// aliases retain the offline grace when a subscription is renamed.
    last_seen: HashMap<String, u64>,
}

impl LiveWatch {
    /// Feed the current observations. Returns the go-live events to queue
    /// (only when `can_send`) and ends sessions in the ledger (only the
    /// sender writes the ledger).
    #[allow(clippy::too_many_arguments)]
    pub fn channel_live(
        &mut self,
        now: u64,
        started_at: u64,
        enabled: bool,
        watched: &[ResolvedWatchChannel],
        seen: &[LiveObservation],
        ledger: &mut NotifyLedger,
        can_send: bool,
        already_queued: impl Fn(&str) -> bool,
    ) -> Vec<AlertEvent> {
        let mut events = Vec::new();
        for channel in watched {
            let keys = channel.episode_keys();
            // Import a delivered legacy account mark before considering a
            // new event. Migrating a live installation must remain silent.
            if can_send {
                if let Some(at) = channel.resolved_by_accounts(ledger) {
                    ledger.record_resolved(&channel.key(), at);
                }
                let notified_at = keys
                    .iter()
                    .filter_map(|key| {
                        ledger
                            .mark(key)
                            .filter(|_| session_open(ledger, key))
                            .map(|mark| mark.notified_at)
                    })
                    .max();
                if let Some(at) = notified_at {
                    for key in &keys {
                        ledger.record_notified(key, at);
                    }
                }
            }
            if let Some(info) = channel.live_info(seen) {
                for key in &keys {
                    self.last_seen.insert(key.clone(), now);
                }
                if !enabled
                    || !can_send
                    || channel.announced(ledger)
                    || keys.iter().any(|key| already_queued(key))
                {
                    continue;
                }
                events.push(
                    AlertEvent::new(EventKind::StreamLive, Phase::Fire, channel.kind, now)
                        .with_variant(&channel.name)
                        .with_extra(ItemExtra {
                            old_owner: None,
                            live: Some(info),
                        }),
                );
            } else if can_send && now.saturating_sub(started_at) >= END_HOLD_SECS {
                let last = keys
                    .iter()
                    .filter_map(|key| self.last_seen.get(key).copied())
                    .max()
                    .unwrap_or(started_at);
                if now.saturating_sub(last) >= END_HOLD_SECS {
                    for key in &keys {
                        if session_open(ledger, key) {
                            ledger.record_resolved(key, now);
                        }
                    }
                }
            }
        }
        self.last_seen
            .retain(|_, at| now.saturating_sub(*at) < END_HOLD_SECS * 6);
        events
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::notify::config::{WatchChannel, WatchPlatforms};

    fn watch() -> Vec<ResolvedWatchChannel> {
        named_watch("主频道")
    }

    fn named_watch(name: &str) -> Vec<ResolvedWatchChannel> {
        let mut catalog = WatchCatalog::default();
        catalog.channels.insert(
            name.to_string(),
            WatchPlatforms {
                twitch: Some("SomeLogin".into()),
                youtube: Some("UCwatched".into()),
                ..Default::default()
            },
        );
        resolve_watch(
            &WatchConfig {
                enabled: true,
                channels: vec![WatchChannel::Configured { name: name.into() }],
                ..Default::default()
            },
            &catalog,
        )
    }

    fn live(title: &str) -> Vec<LiveObservation> {
        vec![
            LiveObservation {
                platform: "twitch".into(),
                channel_id: "somelogin".into(),
                channel_name: String::new(),
                title: Some(title.into()),
            },
            LiveObservation {
                platform: "youtube".into(),
                channel_id: "UCnotwatched".into(),
                channel_name: String::new(),
                title: None,
            },
        ]
    }

    #[test]
    fn a_status_name_uses_the_configured_channel_and_hides_a_youtube_id() {
        let named = LiveObservation {
            platform: "youtube".into(),
            channel_id: "UC-WX1CXssCtCtc2TNIRnJzg".into(),
            channel_name: "紡木こかげ".into(),
            title: None,
        };
        assert_eq!(named.readable_name().as_deref(), Some("紡木こかげ"));
        let raw = LiveObservation {
            platform: "youtube".into(),
            channel_id: "UC-WX1CXssCtCtc2TNIRnJzg".into(),
            channel_name: String::new(),
            title: None,
        };
        assert_eq!(raw.readable_name(), None);
        let twitch = LiveObservation {
            platform: "twitch".into(),
            channel_id: "akarindao".into(),
            channel_name: "夢野あかり".into(),
            title: None,
        };
        assert_eq!(twitch.readable_name().as_deref(), Some("夢野あかり"));
    }

    #[test]
    fn one_notice_per_session_and_a_new_one_after_it_ends() {
        let mut lw = LiveWatch::default();
        let mut ledger = NotifyLedger::default();
        let w = watch();
        let events = lw.channel_live(1_000, 0, true, &w, &live("t"), &mut ledger, true, |_| false);
        assert_eq!(events.len(), 1);
        let event = &events[0];
        assert_eq!(event.dedup_key(), "stream.live:configured:主频道");
        assert_eq!(event.kind.severity(), crate::notify::event::Severity::Info);
        assert_eq!(event.extra.live.as_ref().unwrap().name, "主频道");
        // Delivered: the ledger mark opens the session.
        ledger.record_notified(&event.dedup_key(), 1_010);
        // Still live, and a short gap: nothing new.
        assert!(lw
            .channel_live(1_100, 0, true, &w, &live("t"), &mut ledger, true, |_| false)
            .is_empty());
        assert!(lw
            .channel_live(1_400, 0, true, &w, &[], &mut ledger, true, |_| false)
            .is_empty());
        assert!(lw
            .channel_live(1_500, 0, true, &w, &live("t"), &mut ledger, true, |_| false)
            .is_empty());
        // Offline for the hold: the session ends; the next live notifies.
        lw.channel_live(
            1_500 + END_HOLD_SECS,
            0,
            true,
            &w,
            &[],
            &mut ledger,
            true,
            |_| false,
        );
        let again = lw.channel_live(
            1_600 + END_HOLD_SECS,
            0,
            true,
            &w,
            &live("t2"),
            &mut ledger,
            true,
            |_| false,
        );
        assert_eq!(again.len(), 1);
    }

    #[test]
    fn standbys_and_a_disabled_list_stay_quiet() {
        let mut lw = LiveWatch::default();
        let mut ledger = NotifyLedger::default();
        assert!(lw
            .channel_live(
                1_000,
                0,
                true,
                &watch(),
                &live("t"),
                &mut ledger,
                false,
                |_| { false }
            )
            .is_empty());
        assert!(lw
            .channel_live(
                1_000,
                0,
                false,
                &watch(),
                &live("t"),
                &mut ledger,
                true,
                |_| false
            )
            .is_empty());
        // A copy is already queued: not twice.
        assert!(lw
            .channel_live(
                1_000,
                0,
                true,
                &watch(),
                &live("t"),
                &mut ledger,
                true,
                |_| true
            )
            .is_empty());
    }

    #[test]
    fn a_peer_ledger_mark_suppresses_after_failover() {
        // The old sender announced it; the new sender sees the mark.
        let mut ledger = NotifyLedger::default();
        ledger.record_notified("stream.live:twitch:somelogin", 900);
        let mut lw = LiveWatch::default();
        assert!(lw
            .channel_live(
                1_000,
                0,
                true,
                &watch(),
                &live("t"),
                &mut ledger,
                true,
                |_| false
            )
            .is_empty());
    }

    #[test]
    fn linked_platforms_share_one_episode_until_all_are_offline_for_the_grace() {
        let watched = watch();
        let mut lw = LiveWatch::default();
        let mut ledger = NotifyLedger::default();
        let youtube = LiveObservation {
            platform: "youtube".into(),
            channel_id: "UCwatched".into(),
            channel_name: String::new(),
            title: Some("yt title".into()),
        };
        let twitch = live("tw title")[0].clone();
        let both = vec![youtube.clone(), twitch.clone(), youtube.clone()];
        let first = lw.channel_live(1_000, 0, true, &watched, &both, &mut ledger, true, |_| {
            false
        });
        assert_eq!(first.len(), 1);
        assert_eq!(
            first[0].extra.live.as_ref().unwrap().platforms,
            ["youtube", "twitch"]
        );
        for key in &first[0].extra.live.as_ref().unwrap().episode_keys {
            ledger.record_notified(key, 1_001);
        }
        // A different platform remains live well past ten minutes since the
        // first source was seen. It keeps the same channel episode open.
        assert!(lw
            .channel_live(
                1_700,
                0,
                true,
                &watched,
                &[twitch],
                &mut ledger,
                true,
                |_| false
            )
            .is_empty());
        lw.channel_live(2_299, 0, true, &watched, &[], &mut ledger, true, |_| false);
        assert!(session_open(&ledger, &watched[0].key()));
        lw.channel_live(2_300, 0, true, &watched, &[], &mut ledger, true, |_| false);
        assert!(watched[0]
            .episode_keys()
            .iter()
            .all(|key| !session_open(&ledger, key)));
        assert_eq!(
            lw.channel_live(
                2_400,
                0,
                true,
                &watched,
                &[youtube],
                &mut ledger,
                true,
                |_| false
            )
            .len(),
            1
        );
    }

    #[test]
    fn renamed_entries_keep_the_account_episodes_offline_grace() {
        let old = watch();
        let renamed = named_watch("new name");
        let mut lw = LiveWatch::default();
        let mut ledger = NotifyLedger::default();
        let first = lw.channel_live(1_000, 0, true, &old, &live("t"), &mut ledger, true, |_| {
            false
        });
        assert_eq!(first.len(), 1);
        for key in &first[0].extra.live.as_ref().unwrap().episode_keys {
            ledger.record_notified(key, 1_000);
        }
        // A long-running worker must retain the recent observation
        // when a configured rename happens during a one-second gap.
        lw.channel_live(1_001, 0, true, &renamed, &[], &mut ledger, true, |_| false);
        assert!(session_open(&ledger, &renamed[0].key()));
        let youtube = vec![LiveObservation {
            platform: "youtube".into(),
            channel_id: "UCwatched".into(),
            channel_name: String::new(),
            title: None,
        }];
        assert!(
            lw.channel_live(
                1_002,
                0,
                true,
                &renamed,
                &youtube,
                &mut ledger,
                true,
                |_| false,
            )
            .is_empty(),
            "a linked source returned inside the grace"
        );
        lw.channel_live(1_601, 0, true, &renamed, &[], &mut ledger, true, |_| false);
        assert!(session_open(&ledger, &renamed[0].key()), "599 seconds");
        lw.channel_live(1_602, 0, true, &renamed, &[], &mut ledger, true, |_| false);
        assert!(
            renamed[0]
                .episode_keys()
                .iter()
                .all(|key| !session_open(&ledger, key)),
            "600 seconds"
        );
        assert_eq!(
            lw.channel_live(
                1_603,
                0,
                true,
                &renamed,
                &youtube,
                &mut ledger,
                true,
                |_| false,
            )
            .len(),
            1,
            "a new episode after the full grace"
        );
    }

    #[test]
    fn already_closed_episode_marks_do_not_change_on_later_offline_ticks() {
        let watched = watch();
        let mut lw = LiveWatch::default();
        let mut ledger = NotifyLedger::default();
        let first = lw.channel_live(
            1_000,
            0,
            true,
            &watched,
            &live("t"),
            &mut ledger,
            true,
            |_| false,
        );
        for key in &first[0].extra.live.as_ref().unwrap().episode_keys {
            ledger.record_notified(key, 1_000);
        }
        lw.channel_live(1_600, 0, true, &watched, &[], &mut ledger, true, |_| false);
        assert!(watched[0]
            .episode_keys()
            .iter()
            .all(|key| !session_open(&ledger, key)));
        let closed = ledger.clone();
        for now in [1_601, 2_000, 5_000] {
            lw.channel_live(now, 0, true, &watched, &[], &mut ledger, true, |_| false);
            assert_eq!(
                ledger, closed,
                "closed marks must not make the worker dirty again"
            );
        }
    }

    #[test]
    fn renaming_back_after_aliases_close_does_not_suppress_the_next_episode() {
        let original = watch();
        let renamed = named_watch("new name");
        let mut lw = LiveWatch::default();
        let mut ledger = NotifyLedger::default();
        let first = lw.channel_live(
            1_000,
            0,
            true,
            &original,
            &live("t"),
            &mut ledger,
            true,
            |_| false,
        );
        for key in &first[0].extra.live.as_ref().unwrap().episode_keys {
            ledger.record_notified(key, 1_000);
        }
        lw.channel_live(1_001, 0, true, &renamed, &[], &mut ledger, true, |_| false);
        assert!(lw
            .channel_live(
                1_002,
                0,
                true,
                &renamed,
                &live("t"),
                &mut ledger,
                true,
                |_| false
            )
            .is_empty());
        lw.channel_live(1_602, 0, true, &renamed, &[], &mut ledger, true, |_| false);
        // Only the renamed channel was checked during closure. Its account
        // marks prove that the obsolete original name's episode ended too.
        assert!(!original[0].announced(&ledger));
        let next = lw.channel_live(
            1_700,
            0,
            true,
            &original,
            &live("new stream"),
            &mut ledger,
            true,
            |_| false,
        );
        assert_eq!(
            next.len(),
            1,
            "an old name must not resurrect a closed episode"
        );
        for key in &next[0].extra.live.as_ref().unwrap().episode_keys {
            ledger.record_notified(key, 1_700);
        }
        assert!(lw
            .channel_live(
                1_701,
                0,
                true,
                &renamed,
                &live("new stream"),
                &mut ledger,
                true,
                |_| false
            )
            .is_empty());
    }

    #[test]
    fn staggered_starts_and_legacy_pending_items_do_not_duplicate_a_queued_episode() {
        let watched = watch();
        let mut lw = LiveWatch::default();
        let mut ledger = NotifyLedger::default();
        let first = lw.channel_live(
            1_000,
            0,
            true,
            &watched,
            &live("t"),
            &mut ledger,
            true,
            |_| false,
        );
        assert_eq!(first.len(), 1);
        let youtube = vec![LiveObservation {
            platform: "youtube".into(),
            channel_id: "UCwatched".into(),
            channel_name: String::new(),
            title: None,
        }];
        for queued_key in [first[0].dedup_key(), "stream.live:twitch:somelogin".into()] {
            assert!(lw
                .channel_live(
                    1_005,
                    0,
                    true,
                    &watched,
                    &youtube,
                    &mut ledger,
                    true,
                    |key| key == queued_key
                )
                .is_empty());
        }
        // Pending is not falsely recorded as delivered. A restarted sender
        // can reconstruct the one unsent item, rather than lose the notice.
        assert!(ledger.incidents.is_empty());
        let mut restarted = LiveWatch::default();
        assert_eq!(
            restarted
                .channel_live(
                    1_100,
                    1_100,
                    true,
                    &watched,
                    &youtube,
                    &mut ledger,
                    true,
                    |_| false
                )
                .len(),
            1
        );
    }

    #[test]
    fn an_ongoing_legacy_episode_migrates_silently_and_survives_restart() {
        let watched = watch();
        let mut ledger = NotifyLedger::default();
        ledger.record_notified("stream.live:twitch:somelogin", 900);
        let youtube = vec![LiveObservation {
            platform: "youtube".into(),
            channel_id: "UCwatched".into(),
            channel_name: String::new(),
            title: None,
        }];
        let mut lw = LiveWatch::default();
        assert!(lw
            .channel_live(
                1_000,
                1_000,
                true,
                &watched,
                &youtube,
                &mut ledger,
                true,
                |_| false
            )
            .is_empty());
        assert!(session_open(&ledger, &watched[0].key()));
        assert_eq!(ledger.mark(&watched[0].key()).unwrap().notified_at, 900);
        let serialized = serde_json::to_string(&ledger).unwrap();
        let mut restarted_ledger: NotifyLedger = serde_json::from_str(&serialized).unwrap();
        let mut restarted = LiveWatch::default();
        assert!(restarted
            .channel_live(
                1_200,
                1_200,
                true,
                &watched,
                &live("t"),
                &mut restarted_ledger,
                true,
                |_| false
            )
            .is_empty());
        restarted.channel_live(
            1_799,
            1_200,
            true,
            &watched,
            &[],
            &mut restarted_ledger,
            true,
            |_| false,
        );
        assert!(session_open(&restarted_ledger, &watched[0].key()));
        restarted.channel_live(
            1_800,
            1_200,
            true,
            &watched,
            &[],
            &mut restarted_ledger,
            true,
            |_| false,
        );
        assert!(!session_open(&restarted_ledger, &watched[0].key()));
        assert_eq!(
            restarted
                .channel_live(
                    1_801,
                    1_200,
                    true,
                    &watched,
                    &youtube,
                    &mut restarted_ledger,
                    true,
                    |_| false
                )
                .len(),
            1
        );
    }

    #[test]
    fn only_senders_migrate_or_close_shared_episode_marks() {
        let mut ledger = NotifyLedger::default();
        ledger.record_notified("stream.live:twitch:somelogin", 900);
        let before = ledger.clone();
        let mut lw = LiveWatch::default();
        assert!(lw
            .channel_live(
                1_000,
                0,
                true,
                &watch(),
                &live("t"),
                &mut ledger,
                false,
                |_| false
            )
            .is_empty());
        lw.channel_live(2_000, 0, true, &watch(), &[], &mut ledger, false, |_| false);
        assert_eq!(ledger, before);
    }

    #[test]
    fn resolution_uses_current_accounts_and_never_monitors_unavailable_legacy_entries() {
        let mut catalog = WatchCatalog::default();
        catalog.channels.insert(
            "configured".into(),
            WatchPlatforms {
                youtube: Some("UCfirst".into()),
                ..Default::default()
            },
        );
        let config = WatchConfig {
            enabled: true,
            channels: vec![
                WatchChannel::Manual {
                    name: "manual".into(),
                    platforms: WatchPlatforms {
                        youtube: Some("UCnext".into()),
                        twitch: Some("own-login".into()),
                        ..Default::default()
                    },
                },
                WatchChannel::Configured {
                    name: "configured".into(),
                },
                WatchChannel::Configured {
                    name: "missing".into(),
                },
            ],
            ..Default::default()
        };
        assert_eq!(
            resolve_watch(&config, &catalog)[0].accounts,
            [("youtube", "UCfirst".into())]
        );
        catalog.channels.get_mut("configured").unwrap().youtube = Some("UCnext".into());
        let resolved = resolve_watch(&config, &catalog);
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].accounts, [("youtube", "UCnext".into())]);
        let unavailable = vec![LiveObservation {
            platform: "twitch".into(),
            channel_id: "own-login".into(),
            channel_name: String::new(),
            title: None,
        }];
        assert!(LiveWatch::default()
            .channel_live(
                999,
                0,
                true,
                &resolved,
                &unavailable,
                &mut NotifyLedger::default(),
                true,
                |_| false
            )
            .is_empty());
        let seen = vec![LiveObservation {
            platform: "youtube".into(),
            channel_id: "UCnext".into(),
            channel_name: String::new(),
            title: None,
        }];
        let events = LiveWatch::default().channel_live(
            1_000,
            0,
            true,
            &resolved,
            &seen,
            &mut NotifyLedger::default(),
            true,
            |_| false,
        );
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].dedup_key(), "stream.live:configured:configured");
    }

    #[test]
    fn statuses_map_to_observations() {
        let status = StatusData {
            bilibili: Default::default(),
            youtube: None,
            twitch: Some(crate::webui::state::TwStatus {
                is_live: true,
                enable_monitor: true,
                title: Some("hi".into()),
                game: None,
                channel_name: "x".into(),
                channel_id: "SomeLogin".into(),
                quality: "best".into(),
                area_id: 0,
                area_name: String::new(),
                crop_enabled: false,
                ffmpeg_cache_enabled: false,
                ffmpeg_cache_latency_secs: 0,
            }),
            niconico: None,
            priority_channel: None,
        };
        let obs = observations(&status);
        assert_eq!(obs.len(), 1);
        assert_eq!(obs[0].ident(), "twitch:somelogin");
    }
}
