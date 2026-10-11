//! Go-live notices for the 开播通知关注列表 (watch list).
//!
//! Trigger point (documented in design §2.1): a watched channel counts as
//! live when the YouTube, Twitch or Niconico status of this node or of any
//! fresh cluster node reports `is_live = true` for that channel id. That is
//! the same poll result that makes bilistream start the relay, so the notice
//! is not tied to whether the relay actually started. The sender's notifier
//! sees it on its next wake (at most 15 s later).
//!
//! One notice per stream session, cluster-wide: the ledger mark
//! `stream.live:<platform>:<id>` is open while the session lasts. A session
//! ends after the channel has not been seen live for `END_HOLD_SECS`, so a
//! short poll gap or a failover never sends a second notice.
//!
//! All detection lives in [`LiveWatch::channel_live`], the single call site
//! the analytics plan will later swap for an `events::emit(ChannelLive)`.

use super::config::WatchConfig;
use super::event::{AlertEvent, EventKind, ItemExtra, LiveInfo, Phase};
use super::ledger::NotifyLedger;
use crate::webui::state::StatusData;
use std::collections::HashMap;

/// A gap shorter than this keeps the same stream session.
pub const END_HOLD_SECS: u64 = 600;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LiveObservation {
    pub platform: String,
    pub channel_id: String,
    pub title: Option<String>,
}

impl LiveObservation {
    pub fn ident(&self) -> String {
        if self.platform == "twitch" {
            format!("{}:{}", self.platform, self.channel_id.to_ascii_lowercase())
        } else {
            format!("{}:{}", self.platform, self.channel_id)
        }
    }
}

/// Live channels in one node's status.
pub fn observations(status: &StatusData) -> Vec<LiveObservation> {
    let mut out = Vec::new();
    if let Some(yt) = status.youtube.as_ref().filter(|s| s.is_live) {
        out.push(("youtube", yt.channel_id.clone(), yt.title.clone()));
    }
    if let Some(tw) = status.twitch.as_ref().filter(|s| s.is_live) {
        out.push(("twitch", tw.channel_id.clone(), tw.title.clone()));
    }
    if let Some(nico) = status.niconico.as_ref().filter(|s| s.is_live) {
        out.push(("niconico", nico.channel_id.clone(), nico.title.clone()));
    }
    out.into_iter()
        .filter(|(_, id, _)| !id.trim().is_empty())
        .map(|(platform, id, title)| LiveObservation {
            platform: platform.to_string(),
            channel_id: id.trim().to_string(),
            title,
        })
        .collect()
}

pub fn ledger_key(ident: &str) -> String {
    format!("{}:{}", EventKind::StreamLive.key(), ident)
}

/// The ledger says this session was already announced and has not ended.
fn session_open(ledger: &NotifyLedger, key: &str) -> bool {
    ledger
        .mark(key)
        .is_some_and(|mark| mark.resolved_at.is_none_or(|at| at < mark.notified_at))
}

#[derive(Default)]
pub struct LiveWatch {
    /// ident → last time any node reported it live.
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
        watch: &WatchConfig,
        seen: &[LiveObservation],
        ledger: &mut NotifyLedger,
        can_send: bool,
        already_queued: impl Fn(&str) -> bool,
    ) -> Vec<AlertEvent> {
        let mut events = Vec::new();
        let watched: HashMap<String, &super::config::WatchChannel> = watch
            .channels
            .iter()
            .map(|channel| (channel.ident(), channel))
            .collect();
        for obs in seen {
            let ident = obs.ident();
            let Some(channel) = watched.get(&ident) else {
                continue;
            };
            self.last_seen.insert(ident.clone(), now);
            let key = ledger_key(&ident);
            if !watch.enabled || !can_send || session_open(ledger, &key) || already_queued(&key) {
                continue;
            }
            events.push(
                AlertEvent::new(EventKind::StreamLive, Phase::Fire, &obs.platform, now)
                    .with_variant(&channel.channel_id_key())
                    .with_extra(ItemExtra {
                        old_owner: None,
                        live: Some(LiveInfo {
                            platform: obs.platform.clone(),
                            name: channel.display().to_string(),
                            title: obs.title.clone(),
                        }),
                    }),
            );
        }
        // End sessions nobody has reported live for a while. The grace
        // after start lets heartbeats arrive first.
        if can_send && now.saturating_sub(started_at) >= END_HOLD_SECS {
            for ident in watched.keys() {
                let key = ledger_key(ident);
                let last = self.last_seen.get(ident).copied().unwrap_or(started_at);
                if session_open(ledger, &key) && now.saturating_sub(last) >= END_HOLD_SECS {
                    ledger.record_resolved(&key, now);
                }
            }
        }
        self.last_seen
            .retain(|_, at| now.saturating_sub(*at) < END_HOLD_SECS * 6);
        events
    }
}

impl super::config::WatchChannel {
    /// The id part of `ident()`, used as the dedup variant.
    pub fn channel_id_key(&self) -> String {
        self.ident()
            .split_once(':')
            .map(|(_, id)| id.to_string())
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::notify::config::WatchChannel;

    fn watch() -> WatchConfig {
        WatchConfig {
            enabled: true,
            channels: vec![WatchChannel {
                platform: "twitch".into(),
                channel_id: "SomeLogin".into(),
                name: "主频道".into(),
            }],
        }
    }

    fn live(title: &str) -> Vec<LiveObservation> {
        vec![
            LiveObservation {
                platform: "twitch".into(),
                channel_id: "somelogin".into(),
                title: Some(title.into()),
            },
            LiveObservation {
                platform: "youtube".into(),
                channel_id: "UCnotwatched".into(),
                title: None,
            },
        ]
    }

    #[test]
    fn one_notice_per_session_and_a_new_one_after_it_ends() {
        let mut lw = LiveWatch::default();
        let mut ledger = NotifyLedger::default();
        let w = watch();
        let events = lw.channel_live(1_000, 0, &w, &live("t"), &mut ledger, true, |_| false);
        assert_eq!(events.len(), 1);
        let event = &events[0];
        assert_eq!(event.dedup_key(), "stream.live:twitch:somelogin");
        assert_eq!(event.kind.severity(), crate::notify::event::Severity::Info);
        assert_eq!(event.extra.live.as_ref().unwrap().name, "主频道");
        // Delivered: the ledger mark opens the session.
        ledger.record_notified(&event.dedup_key(), 1_010);
        // Still live, and a short gap: nothing new.
        assert!(lw
            .channel_live(1_100, 0, &w, &live("t"), &mut ledger, true, |_| false)
            .is_empty());
        assert!(lw
            .channel_live(1_400, 0, &w, &[], &mut ledger, true, |_| false)
            .is_empty());
        assert!(lw
            .channel_live(1_500, 0, &w, &live("t"), &mut ledger, true, |_| false)
            .is_empty());
        // Offline for the hold: the session ends; the next live notifies.
        lw.channel_live(1_500 + END_HOLD_SECS, 0, &w, &[], &mut ledger, true, |_| {
            false
        });
        let again = lw.channel_live(
            1_600 + END_HOLD_SECS,
            0,
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
            .channel_live(1_000, 0, &watch(), &live("t"), &mut ledger, false, |_| {
                false
            })
            .is_empty());
        let mut off = watch();
        off.enabled = false;
        assert!(lw
            .channel_live(1_000, 0, &off, &live("t"), &mut ledger, true, |_| false)
            .is_empty());
        // A copy is already queued: not twice.
        assert!(lw
            .channel_live(1_000, 0, &watch(), &live("t"), &mut ledger, true, |_| true)
            .is_empty());
    }

    #[test]
    fn a_peer_ledger_mark_suppresses_after_failover() {
        // The old sender announced it; the new sender sees the mark.
        let mut ledger = NotifyLedger::default();
        ledger.record_notified("stream.live:twitch:somelogin", 900);
        let mut lw = LiveWatch::default();
        assert!(lw
            .channel_live(1_000, 0, &watch(), &live("t"), &mut ledger, true, |_| false)
            .is_empty());
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
