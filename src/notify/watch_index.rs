//! Index-side go-live rows for the watched roster.
//!
//! The status card only describes the channel this node relays, so a watched
//! channel that is not restreamed never reported going live. This module polls
//! the index for every watched channel on its own task and publishes what it
//! saw for [`super::live::LiveWatch`] to consume.
//!
//! The notifier never waits on the network: [`WatchIndex::follow`] only records
//! the wanted roster, and [`WatchIndex::observations`] answers from the last
//! snapshot. A failed poll keeps that snapshot; once it is older than
//! [`STALE_AFTER`] the rows stop counting, so a dead index cannot hold an
//! episode open forever.

use super::live::{LiveObservation, ResolvedWatchChannel};
use crate::cluster::now_secs;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::Notify;
use tokio::task::JoinHandle;

/// How often the roster is re-read. Holodex answers the whole roster in one
/// request, so one poll is a single cheap call.
const POLL_EVERY: Duration = Duration::from_secs(45);

/// Rows this old stop counting. Silence from the index is not proof that a
/// stream ended, but it must not keep an episode open either.
const STALE_AFTER: u64 = 600;

#[derive(Default)]
pub struct WatchIndex {
    shared: Arc<Shared>,
    task: Option<JoinHandle<()>>,
}

impl Drop for WatchIndex {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

#[derive(Default)]
struct Shared {
    /// Watched YouTube channel ids; empty when this node does not send or the
    /// watch list is off.
    wanted: Mutex<Vec<String>>,
    /// `(read at, live rows)` of the last successful poll.
    snapshot: Mutex<Option<(u64, Vec<LiveObservation>)>>,
    /// Wakes the poller when the roster changes.
    wake: Notify,
}

impl Shared {
    fn wanted(&self) -> Vec<String> {
        lock_or_recover(&self.wanted, "notify watch roster").clone()
    }

    fn publish(&self, at: u64, rows: Vec<LiveObservation>) {
        *lock_or_recover(&self.snapshot, "notify watch snapshot") = Some((at, rows));
    }
}

/// Recover a poisoned lock instead of substituting an empty value.
///
/// A poisoned roster used to read as "nothing is watched" (the poller stops
/// for the worker's lifetime) and a poisoned snapshot as "nothing is live" (a
/// fabricated offline edge). `error-handling.md` requires `into_inner()` plus
/// a warning for these process-wide maps.
fn lock_or_recover<'a, T>(lock: &'a Mutex<T>, name: &str) -> std::sync::MutexGuard<'a, T> {
    lock.lock().unwrap_or_else(|poisoned| {
        tracing::warn!("Recovering poisoned {name}");
        poisoned.into_inner()
    })
}

impl WatchIndex {
    /// Start the poller. Called from the worker loop, so a runtime exists;
    /// workers built without one keep an empty snapshot.
    ///
    /// A finished poller is restarted: this is called once at worker boot, so
    /// a panic inside the task would otherwise stop watched-channel 开播
    /// notices for the worker's lifetime.
    pub fn start(&mut self) {
        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        if let Some(task) = self.task.as_ref() {
            if !task.is_finished() {
                return;
            }
            tracing::warn!("QQ 关注频道轮询任务已退出，重新启动");
        }
        let shared = Arc::clone(&self.shared);
        self.task = Some(tokio::spawn(poll(shared)));
    }

    /// Follow this roster. Only a changed set wakes the poller, so the 15 s
    /// worker tick costs nothing; an empty roster also clears the snapshot.
    pub fn follow(&mut self, active: bool, watched: &[ResolvedWatchChannel]) {
        let mut wanted: Vec<String> = if active {
            watched
                .iter()
                .flat_map(|channel| channel.accounts.iter())
                .filter(|(platform, _)| *platform == "youtube")
                .map(|(_, id)| id.trim().to_string())
                .filter(|id| !id.is_empty())
                .collect()
        } else {
            Vec::new()
        };
        wanted.sort();
        wanted.dedup();
        let cleared = {
            let mut guard = lock_or_recover(&self.shared.wanted, "notify watch roster");
            if *guard == wanted {
                return;
            }
            *guard = wanted;
            guard.is_empty()
        };
        if cleared {
            self.shared.publish(now_secs(), Vec::new());
        }
        self.shared.wake.notify_one();
    }

    /// Rows to treat as live now. Empty until the first poll lands, and once
    /// the last good poll is too old.
    pub fn observations(&self, now: u64) -> Vec<LiveObservation> {
        lock_or_recover(&self.shared.snapshot, "notify watch snapshot")
            .as_ref()
            .filter(|(at, _)| now.saturating_sub(*at) <= STALE_AFTER)
            .map(|(_, rows)| rows.clone())
            .unwrap_or_default()
    }

    /// Test seam: publish rows the poller would have produced.
    #[cfg(test)]
    pub(crate) fn publish_rows(&self, at: u64, rows: Vec<LiveObservation>) {
        self.shared.publish(at, rows);
    }
}

async fn poll(shared: Arc<Shared>) {
    loop {
        let wanted = shared.wanted();
        if !wanted.is_empty() {
            let result = crate::plugins::youtube::watch_live_rows(&wanted).await;
            // Drop a poll whose roster was replaced while it ran.
            if shared.wanted() == wanted {
                match result {
                    Ok(rows) => {
                        let rows = rows.into_iter().filter_map(observation).collect();
                        shared.publish(now_secs(), rows);
                    }
                    Err(error) => tracing::debug!("QQ 关注频道开播信号获取失败: {error}"),
                }
            }
        }
        // A roster change wakes a sleeping poller; an empty roster costs one
        // no-op wakeup per interval.
        tokio::select! {
            _ = tokio::time::sleep(POLL_EVERY) => {}
            _ = shared.wake.notified() => {}
        }
    }
}

fn observation(item: crate::plugins::stream_item::StreamItem) -> Option<LiveObservation> {
    let channel_id = item.channel.id.trim().to_string();
    if channel_id.is_empty() {
        return None;
    }
    Some(LiveObservation {
        platform: "youtube".to_string(),
        channel_id,
        channel_name: item.channel.name.trim().to_string(),
        title: Some(item.title.trim().to_string()).filter(|title| !title.is_empty()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn watched(accounts: &[(&'static str, &str)]) -> Vec<ResolvedWatchChannel> {
        vec![ResolvedWatchChannel {
            kind: "configured",
            name: "主频道".into(),
            accounts: accounts
                .iter()
                .map(|(platform, id)| (*platform, (*id).to_string()))
                .collect(),
        }]
    }

    fn row(channel_id: &str) -> LiveObservation {
        LiveObservation {
            platform: "youtube".into(),
            channel_id: channel_id.into(),
            channel_name: "被关注的频道".into(),
            title: Some("【歌枠】".into()),
        }
    }

    #[test]
    fn only_youtube_accounts_are_polled_and_an_empty_roster_clears_the_rows() {
        let mut live = WatchIndex::default();
        live.follow(
            true,
            &watched(&[("youtube", "UCa"), ("twitch", "login"), ("youtube", "UCa")]),
        );
        assert_eq!(live.shared.wanted(), vec!["UCa".to_string()]);
        live.publish_rows(100, vec![row("UCa")]);
        assert_eq!(live.observations(100), vec![row("UCa")]);

        // The watch list turning off must not keep the last rows alive.
        live.follow(false, &[]);
        assert!(live.shared.wanted().is_empty());
        assert!(live.observations(100).is_empty());
    }

    #[test]
    fn rows_stop_counting_once_the_last_poll_is_stale() {
        let mut live = WatchIndex::default();
        live.follow(true, &watched(&[("youtube", "UCa")]));
        live.publish_rows(1_000, vec![row("UCa")]);
        assert_eq!(live.observations(1_000 + STALE_AFTER).len(), 1);
        assert!(
            live.observations(1_000 + STALE_AFTER + 1).is_empty(),
            "a dead index must not hold an episode open"
        );
    }

    #[test]
    fn a_failed_poll_keeps_the_previous_rows() {
        let mut live = WatchIndex::default();
        live.follow(true, &watched(&[("youtube", "UCa")]));
        live.publish_rows(1_000, vec![row("UCa")]);
        // No publish happened here: that is what `Err` from the fetch does.
        assert_eq!(live.observations(1_010), vec![row("UCa")]);
    }

    #[test]
    fn a_poisoned_lock_still_serves_the_last_rows() {
        let mut live = WatchIndex::default();
        live.follow(true, &watched(&[("youtube", "UCa")]));
        live.publish_rows(1_000, vec![row("UCa")]);

        // Poison both maps: a panicking holder must not read as "nothing".
        let shared = Arc::clone(&live.shared);
        std::thread::spawn(move || {
            let _roster = shared.wanted.lock().unwrap();
            let _snapshot = shared.snapshot.lock().unwrap();
            panic!("poison the watch-index locks");
        })
        .join()
        .expect_err("the holder thread panics");

        assert_eq!(live.shared.wanted(), vec!["UCa".to_string()]);
        assert_eq!(live.observations(1_000), vec![row("UCa")]);
    }
}
