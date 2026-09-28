//! When the roster goes live, learned from YouTube's own answers.
//!
//! Every `videos.list` answer with an `actualStartTime` counts once, in its
//! channel's UTC-hour bucket, weighted by age (half-life four weeks) and
//! decayed as time passes, so the shape follows the roster's current habits.
//! The uploads-playlist poller spreads its budget over the day by these weights
//! (`youtube_rss::hourly_intervals`). The counts live in
//! `youtube_golive_hours.json` next to the binary, so a restart keeps them.

use super::youtube_data::YtVideo;
use chrono::{DateTime, Timelike, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

const FILE_NAME: &str = "youtube_golive_hours.json";
const HALF_LIFE_SECS: f64 = 28.0 * 86_400.0;
/// Older starts are not counted (they would weigh about 1/9) and leave the
/// dedupe set.
const KEEP_SECS: i64 = 90 * 86_400;
/// Buckets are decayed at most this often.
const DECAY_EVERY_SECS: i64 = 60 * 60;
/// The file is written at most this often.
const SAVE_EVERY_SECS: i64 = 60;
/// Fewer go-lives than this across the roster keep polling flat.
pub(crate) const MIN_GOLIVES: f64 = 50.0;

#[derive(Default, Serialize, Deserialize, Debug, Clone, PartialEq)]
struct Hours {
    /// Channel ID → decayed go-lives per UTC hour, as of `decayed_at`.
    channels: HashMap<String, [f64; 24]>,
    /// Counted video ID → its start (Unix seconds).
    counted: HashMap<String, i64>,
    /// Unix seconds; 0 before the first decay.
    decayed_at: i64,
}

fn decay_factor(secs: i64) -> f64 {
    0.5_f64.powf(secs.max(0) as f64 / HALF_LIFE_SECS)
}

impl Hours {
    /// Counts each start not seen before. Returns whether anything changed.
    fn record<'a>(
        &mut self,
        starts: impl IntoIterator<Item = (&'a str, &'a str, DateTime<Utc>)>,
        now: DateTime<Utc>,
    ) -> bool {
        let now = now.timestamp();
        let mut changed = self.decay(now);
        for (video, channel, start) in starts {
            let at = start.timestamp();
            if channel.is_empty()
                || at > now
                || now - at > KEEP_SECS
                || self.counted.contains_key(video)
            {
                continue;
            }
            let buckets = self
                .channels
                .entry(channel.to_string())
                .or_insert([0.0; 24]);
            buckets[start.hour() as usize] += decay_factor(now - at);
            self.counted.insert(video.to_string(), at);
            changed = true;
        }
        changed
    }

    fn decay(&mut self, now: i64) -> bool {
        if self.decayed_at == 0 {
            self.decayed_at = now;
            return true;
        }
        let elapsed = now - self.decayed_at;
        if elapsed < DECAY_EVERY_SECS {
            return false;
        }
        let factor = decay_factor(elapsed);
        for buckets in self.channels.values_mut() {
            for value in buckets.iter_mut() {
                *value *= factor;
            }
        }
        self.channels
            .retain(|_, buckets| buckets.iter().sum::<f64>() > 0.001);
        self.counted.retain(|_, at| now - *at <= KEEP_SECS);
        self.decayed_at = now;
        true
    }

    /// The roster's go-lives per UTC hour.
    fn roster_sum(&self, roster: &[String]) -> [f64; 24] {
        let mut sum = [0.0; 24];
        for buckets in roster.iter().filter_map(|id| self.channels.get(id)) {
            for (total, value) in sum.iter_mut().zip(buckets) {
                *total += value;
            }
        }
        sum
    }
}

/// The roster's weights, or `None` while fewer than `MIN_GOLIVES` are counted.
fn weights_of(sum: [f64; 24]) -> Option<[f64; 24]> {
    (sum.iter().sum::<f64>() >= MIN_GOLIVES).then_some(sum)
}

struct State {
    hours: Hours,
    /// `None` in tests and when the binary's path is unknown.
    path: Option<PathBuf>,
    saved_at: i64,
    dirty: bool,
}

static STATE: Mutex<Option<State>> = Mutex::new(None);

fn load() -> State {
    // Tests never read or write the file next to the test binary.
    let path = std::env::current_exe()
        .ok()
        .filter(|_| !cfg!(test))
        .map(|exe| exe.with_file_name(FILE_NAME));
    let hours = path
        .as_ref()
        .and_then(|path| std::fs::read(path).ok())
        .map(|bytes| {
            serde_json::from_slice(&bytes).unwrap_or_else(|e| {
                tracing::warn!("{} 无法解析，重新学习开播时段: {}", FILE_NAME, e);
                Hours::default()
            })
        })
        .unwrap_or_default();
    State {
        hours,
        path,
        saved_at: 0,
        dirty: false,
    }
}

fn with_state<T>(f: impl FnOnce(&mut State) -> T) -> T {
    let mut guard = STATE.lock().unwrap_or_else(|e| e.into_inner());
    f(guard.get_or_insert_with(load))
}

/// Counts the go-lives in these answers and saves now and then.
pub(crate) fn record(videos: &[YtVideo]) {
    let starts: Vec<(&str, &str, DateTime<Utc>)> = videos
        .iter()
        .filter_map(|video| {
            let start = video
                .live_streaming_details
                .as_ref()?
                .actual_start_time
                .as_deref()?;
            let start = DateTime::parse_from_rfc3339(start)
                .ok()?
                .with_timezone(&Utc);
            Some((video.id.as_str(), video.snippet.channel_id.as_str(), start))
        })
        .collect();
    let now = Utc::now();
    let save = with_state(|state| {
        if state.hours.record(starts, now) {
            state.dirty = true;
        }
        let due = now.timestamp() - state.saved_at >= SAVE_EVERY_SECS;
        if !(state.dirty && due) {
            return None;
        }
        let path = state.path.clone()?;
        let bytes = serde_json::to_vec(&state.hours).ok()?;
        state.dirty = false;
        state.saved_at = now.timestamp();
        Some((path, bytes))
    });
    if let Some((path, bytes)) = save {
        tokio::task::spawn_blocking(move || {
            if let Err(e) = crate::config::write_file_atomic(&path, &bytes) {
                tracing::debug!("{} 保存失败: {}", FILE_NAME, e);
            }
        });
    }
}

/// The roster's go-lives per UTC hour, or `None` while too few are counted.
pub(crate) fn weights(roster: &[String]) -> Option<[f64; 24]> {
    weights_of(with_state(|state| state.hours.roster_sum(roster)))
}

/// The roster's go-lives per UTC hour so far, even below `MIN_GOLIVES`.
pub(crate) fn roster_hours(roster: &[String]) -> [f64; 24] {
    with_state(|state| state.hours.roster_sum(roster))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    #[test]
    fn a_start_counts_once_in_its_utc_hour() {
        let now = at("2026-09-28T00:00:00Z");
        let start = at("2026-09-27T11:30:00Z");
        let mut hours = Hours::default();
        assert!(hours.record([("v1", "UCa", start)], now));
        assert!(!hours.record([("v1", "UCa", start)], now), "same video");
        let buckets = hours.channels["UCa"];
        assert!((buckets[11] - decay_factor(12 * 3600 + 1800)).abs() < 1e-9);
        assert_eq!(buckets.iter().filter(|v| **v > 0.0).count(), 1);
    }

    #[test]
    fn old_future_and_channelless_starts_are_skipped() {
        let now = at("2026-09-28T00:00:00Z");
        let mut hours = Hours::default();
        hours.record(
            [
                ("old", "UCa", at("2024-03-31T05:00:00Z")),
                ("future", "UCa", at("2026-09-28T01:00:00Z")),
                ("nochannel", "", at("2026-09-27T01:00:00Z")),
            ],
            now,
        );
        assert!(hours.channels.is_empty());
        assert!(hours.counted.is_empty());
    }

    #[test]
    fn a_four_week_old_start_weighs_half() {
        let now = at("2026-09-28T00:00:00Z");
        let mut hours = Hours::default();
        hours.record([("v1", "UCa", now - chrono::Duration::days(28))], now);
        assert!((hours.channels["UCa"][0] - 0.5).abs() < 1e-9);
    }

    #[test]
    fn counts_fade_by_half_every_four_weeks() {
        let t0 = at("2026-09-01T00:00:00Z");
        let mut hours = Hours::default();
        hours.record([("v1", "UCa", t0)], t0);
        assert!((hours.channels["UCa"][0] - 1.0).abs() < 1e-9);
        hours.record(std::iter::empty(), t0 + chrono::Duration::minutes(30));
        assert!((hours.channels["UCa"][0] - 1.0).abs() < 1e-9, "not yet");
        hours.record(std::iter::empty(), t0 + chrono::Duration::days(28));
        assert!((hours.channels["UCa"][0] - 0.5).abs() < 1e-9);
        // The dedupe set forgets starts past 90 days, when they weigh ~1/9.
        hours.record(std::iter::empty(), t0 + chrono::Duration::days(91));
        assert!(hours.counted.is_empty());
    }

    #[test]
    fn the_roster_decides_which_channels_count() {
        let now = at("2026-09-28T00:00:00Z");
        let mut hours = Hours::default();
        let starts: Vec<(String, &str, DateTime<Utc>)> = (0..60)
            .map(|i| {
                let channel = if i % 2 == 0 { "UCa" } else { "UCb" };
                (format!("v{i}"), channel, now - chrono::Duration::minutes(i))
            })
            .collect();
        hours.record(
            starts
                .iter()
                .map(|(id, channel, start)| (id.as_str(), *channel, *start)),
            now,
        );
        let both = ["UCa".to_string(), "UCb".to_string()];
        assert!(weights_of(hours.roster_sum(&both)).is_some());
        assert!(
            weights_of(hours.roster_sum(&both[..1])).is_none(),
            "30 go-lives stay flat"
        );
    }

    #[test]
    fn the_file_round_trips() {
        let now = at("2026-09-28T00:00:00Z");
        let mut hours = Hours::default();
        hours.record([("v1", "UCa", at("2026-09-27T20:00:00Z"))], now);
        let bytes = serde_json::to_vec(&hours).unwrap();
        let back: Hours = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(back, hours);
        assert!(serde_json::from_slice::<Hours>(b"{not json").is_err());
    }
}
