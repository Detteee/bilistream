//! Channel Atom parsing, conditional feed requests and RSS backoff.

use super::RSS_TICK;
use crate::plugins::http::response_bytes_limited;
use regex::Regex;
use reqwest::header::{ETAG, IF_MODIFIED_SINCE, IF_NONE_MATCH, LAST_MODIFIED, RETRY_AFTER};
use std::collections::HashMap;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

const FEED_URL: &str = "https://www.youtube.com/feeds/videos.xml?channel_id=";
const FEED_JITTER_MS: u64 = 500;
/// A 429 without `Retry-After` backs the feed off this long, doubling.
const RATE_LIMIT_BACKOFF: Duration = Duration::from_secs(30 * 60);
/// A single failing feed (404/5xx) backs off from one RSS tick, doubling.
const FEED_ERROR_BACKOFF: Duration = RSS_TICK;
const MAX_FEED_BACKOFF: Duration = Duration::from_secs(60 * 60);
/// A `Retry-After` beyond this is treated as bogus.
const MAX_RETRY_AFTER: Duration = Duration::from_secs(6 * 60 * 60);
/// This many 429s in one pass pause RSS as a whole.
const PAUSE_AFTER_429S: usize = 3;
const OUTAGE_PROBE_EVERY: Duration = Duration::from_secs(10 * 60);
/// Fewer attempted feeds than this cannot tell an outage from a few bad channels.
const OUTAGE_MIN_FEEDS: usize = 4;

#[derive(Debug, Clone, PartialEq)]
pub struct FeedEntry {
    pub video_id: String,
    pub channel_id: String,
    pub channel_name: String,
    pub title: String,
    pub published: Option<String>,
}

fn entry_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?s)<entry>(.*?)</entry>").expect("valid regex"))
}

fn tag_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"(?s)<yt:videoId>(?P<vid>[^<]*)</yt:videoId>|<yt:channelId>(?P<cid>[^<]*)</yt:channelId>|<name>(?P<name>[^<]*)</name>|<published>(?P<pub>[^<]*)</published>|<title>(?P<title>[^<]*)</title>",
        )
        .expect("valid regex")
    })
}

fn unescape(text: &str) -> String {
    text.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&amp;", "&")
}

/// Entries of one channel feed. Only the first of each tag inside an entry
/// counts, so `<media:title>` and nested descriptions cannot override it.
pub fn parse_feed(xml: &str) -> Vec<FeedEntry> {
    entry_regex()
        .captures_iter(xml)
        .filter_map(|entry| {
            let body = entry.get(1)?.as_str();
            let mut parsed = FeedEntry {
                video_id: String::new(),
                channel_id: String::new(),
                channel_name: String::new(),
                title: String::new(),
                published: None,
            };
            for tag in tag_regex().captures_iter(body) {
                let set = |slot: &mut String, name: &str| {
                    if slot.is_empty() {
                        if let Some(value) = tag.name(name) {
                            *slot = unescape(value.as_str().trim());
                        }
                    }
                };
                set(&mut parsed.video_id, "vid");
                set(&mut parsed.channel_id, "cid");
                set(&mut parsed.channel_name, "name");
                set(&mut parsed.title, "title");
                if parsed.published.is_none() {
                    parsed.published = tag.name("pub").map(|m| m.as_str().trim().to_string());
                }
            }
            (!parsed.video_id.is_empty() && !parsed.channel_id.is_empty()).then_some(parsed)
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq)]
pub(super) enum FeedOutcome {
    Fresh {
        entries: Vec<FeedEntry>,
        etag: Option<String>,
        last_modified: Option<String>,
    },
    NotModified,
    RateLimited {
        retry_after: Option<Duration>,
    },
    /// 404, 5xx, another status, or no response at all.
    Failed,
}

#[derive(Debug, Default)]
struct FeedState {
    failures: u32,
    retry_at: Option<Instant>,
    etag: Option<String>,
    last_modified: Option<String>,
    /// What the feed last listed, kept while it fails or is skipped.
    entries: Vec<FeedEntry>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RssMode {
    Up,
    /// Too many 429s: no feed is read until then.
    Paused {
        until: Instant,
    },
    /// Most feeds failing: read one rotating probe feed at a time.
    Down {
        probe_at: Instant,
    },
}

/// RSS health as a pure state machine: `plan` says which feeds to read,
/// `record` takes what came back.
pub(super) struct RssHealth {
    mode: RssMode,
    feeds: HashMap<String, FeedState>,
    probe_cursor: usize,
}

impl Default for RssHealth {
    fn default() -> Self {
        Self {
            mode: RssMode::Up,
            feeds: HashMap::new(),
            probe_cursor: 0,
        }
    }
}

/// `base`, doubled for each failure after the first, capped at an hour.
fn backoff(base: Duration, failures: u32) -> Duration {
    base.saturating_mul(1 << failures.saturating_sub(1).min(8))
        .min(MAX_FEED_BACKOFF)
}

impl RssHealth {
    pub(super) fn plan(&mut self, roster: &[String], now: Instant) -> Vec<String> {
        self.feeds.retain(|id, _| roster.contains(id));
        match self.mode {
            RssMode::Paused { until } if now < until => Vec::new(),
            RssMode::Down { probe_at } => {
                if now < probe_at || roster.is_empty() {
                    return Vec::new();
                }
                let probe = roster[self.probe_cursor % roster.len()].clone();
                self.probe_cursor += 1;
                vec![probe]
            }
            RssMode::Paused { .. } | RssMode::Up => {
                if self.mode != RssMode::Up {
                    tracing::info!("YouTube RSS 暂停结束，恢复读取");
                    self.mode = RssMode::Up;
                }
                roster
                    .iter()
                    .filter(|id| {
                        self.feeds
                            .get(*id)
                            .and_then(|feed| feed.retry_at)
                            .is_none_or(|at| at <= now)
                    })
                    .cloned()
                    .collect()
            }
        }
    }

    pub(super) fn validators(&self, channel_id: &str) -> (Option<String>, Option<String>) {
        self.feeds
            .get(channel_id)
            .map(|feed| (feed.etag.clone(), feed.last_modified.clone()))
            .unwrap_or_default()
    }

    pub(super) fn record(&mut self, results: Vec<(String, FeedOutcome)>, now: Instant) {
        let attempted = results.len();
        let retry_afters: Vec<Option<Duration>> = results
            .iter()
            .filter_map(|(_, outcome)| match outcome {
                FeedOutcome::RateLimited { retry_after } => Some(*retry_after),
                _ => None,
            })
            .collect();
        let failed = results
            .iter()
            .filter(|(_, outcome)| *outcome == FeedOutcome::Failed)
            .count();
        let answered = results.iter().any(|(_, outcome)| {
            matches!(
                outcome,
                FeedOutcome::Fresh { .. } | FeedOutcome::NotModified
            )
        });

        let mut penalize = true;
        if let RssMode::Down { .. } = self.mode {
            // Probes of a known outage never penalize the probed channel.
            penalize = false;
            if answered {
                tracing::info!("YouTube RSS 已恢复");
                self.mode = RssMode::Up;
            } else if attempted > 0 {
                self.mode = RssMode::Down {
                    probe_at: now + OUTAGE_PROBE_EVERY,
                };
            }
        } else if retry_afters.len() >= PAUSE_AFTER_429S {
            let longest = retry_afters.iter().flatten().max().copied();
            let pause = RATE_LIMIT_BACKOFF.max(longest.unwrap_or_default());
            tracing::warn!(
                "YouTube RSS 本轮 {} 个 feed 返回 429，暂停 {} 分钟",
                retry_afters.len(),
                pause.as_secs() / 60
            );
            self.mode = RssMode::Paused { until: now + pause };
        } else if attempted >= OUTAGE_MIN_FEEDS && failed * 2 >= attempted {
            tracing::warn!(
                "YouTube RSS 本轮 {}/{} 个 feed 失败，视为 RSS 故障，改为每 {} 分钟探测一次",
                failed,
                attempted,
                OUTAGE_PROBE_EVERY.as_secs() / 60
            );
            self.mode = RssMode::Down {
                probe_at: now + OUTAGE_PROBE_EVERY,
            };
            penalize = false;
        }

        for (id, outcome) in results {
            let feed = self.feeds.entry(id).or_default();
            match outcome {
                FeedOutcome::Fresh {
                    entries,
                    etag,
                    last_modified,
                } => {
                    *feed = FeedState {
                        entries,
                        etag,
                        last_modified,
                        ..FeedState::default()
                    };
                }
                FeedOutcome::NotModified => {
                    feed.failures = 0;
                    feed.retry_at = None;
                }
                FeedOutcome::RateLimited { retry_after } => {
                    feed.failures += 1;
                    let wait =
                        retry_after.unwrap_or_else(|| backoff(RATE_LIMIT_BACKOFF, feed.failures));
                    feed.retry_at = Some(now + wait);
                }
                FeedOutcome::Failed if penalize => {
                    feed.failures += 1;
                    feed.retry_at = Some(now + backoff(FEED_ERROR_BACKOFF, feed.failures));
                }
                FeedOutcome::Failed => {}
            }
        }
    }

    /// RSS cannot be relied on right now, so the playlist backstop runs.
    pub(super) fn degraded(&self, now: Instant) -> bool {
        match self.mode {
            RssMode::Up => false,
            RssMode::Paused { until } => now < until,
            RssMode::Down { .. } => true,
        }
    }

    pub(super) fn entries(&self) -> impl Iterator<Item = &FeedEntry> {
        self.feeds.values().flat_map(|feed| feed.entries.iter())
    }
}

/// Seconds or an HTTP date, capped at `MAX_RETRY_AFTER`.
fn parse_retry_after(value: &str, now: chrono::DateTime<chrono::Utc>) -> Option<Duration> {
    let value = value.trim();
    let wait = match value.parse::<u64>() {
        Ok(secs) => Duration::from_secs(secs),
        Err(_) => (chrono::DateTime::parse_from_rfc2822(value)
            .ok()?
            .with_timezone(&chrono::Utc)
            - now)
            .to_std()
            .unwrap_or_default(),
    };
    Some(wait.min(MAX_RETRY_AFTER))
}

fn header_string(
    response: &reqwest::Response,
    name: reqwest::header::HeaderName,
) -> Option<String> {
    response
        .headers()
        .get(name)?
        .to_str()
        .ok()
        .map(str::to_string)
}

fn unix_now() -> Duration {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
}

pub(super) async fn fetch_feed(
    client: &reqwest::Client,
    channel_id: &str,
    etag: Option<String>,
    last_modified: Option<String>,
) -> FeedOutcome {
    // Spread the two concurrent requests a little.
    let jitter = u64::from(unix_now().subsec_nanos()) % FEED_JITTER_MS;
    tokio::time::sleep(Duration::from_millis(jitter)).await;

    // The feed is served with max-age=900; a unique query skips shared caches.
    let url = format!("{FEED_URL}{channel_id}&_={}", unix_now().as_secs());
    let mut request = client.get(url).timeout(Duration::from_secs(15));
    if let Some(etag) = etag {
        request = request.header(IF_NONE_MATCH, etag);
    }
    if let Some(last_modified) = last_modified {
        request = request.header(IF_MODIFIED_SINCE, last_modified);
    }
    let response = match request.send().await {
        Ok(response) => response,
        Err(e) => {
            tracing::debug!("YouTube RSS {} 读取失败: {}", channel_id, e);
            return FeedOutcome::Failed;
        }
    };
    let status = response.status();
    if status == reqwest::StatusCode::NOT_MODIFIED {
        return FeedOutcome::NotModified;
    }
    if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
        let retry_after = header_string(&response, RETRY_AFTER)
            .and_then(|value| parse_retry_after(&value, chrono::Utc::now()));
        tracing::debug!(
            "YouTube RSS {} 429, Retry-After {:?}",
            channel_id,
            retry_after
        );
        return FeedOutcome::RateLimited { retry_after };
    }
    if !status.is_success() {
        tracing::debug!("YouTube RSS {} 读取失败: {}", channel_id, status);
        return FeedOutcome::Failed;
    }
    let etag = header_string(&response, ETAG);
    let last_modified = header_string(&response, LAST_MODIFIED);
    match response_bytes_limited(response, 2 * 1024 * 1024).await {
        Ok(bytes) => FeedOutcome::Fresh {
            entries: parse_feed(&String::from_utf8_lossy(&bytes)),
            etag,
            last_modified,
        },
        Err(e) => {
            tracing::debug!("YouTube RSS {} 读取失败: {}", channel_id, e);
            FeedOutcome::Failed
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn roster(n: usize) -> Vec<String> {
        (0..n).map(|i| format!("UC{i}")).collect()
    }
    const FEED: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<feed xmlns:yt="http://www.youtube.com/xml/schemas/2015" xmlns="http://www.w3.org/2005/Atom">
 <yt:channelId>gYCMluaLpERsyNXlPOvBtA</yt:channelId>
 <title>Channel</title>
 <entry>
  <id>yt:video:BnV1AEeqBR4</id>
  <yt:videoId>BnV1AEeqBR4</yt:videoId>
  <yt:channelId>UCgYCMluaLpERsyNXlPOvBtA</yt:channelId>
  <title>Bomb &amp; defuse</title>
  <author><name>かみと</name></author>
  <published>2026-09-24T10:59:38+00:00</published>
  <media:group><media:title>other</media:title></media:group>
 </entry>
 <entry>
  <yt:videoId>second</yt:videoId>
  <yt:channelId>UCgYCMluaLpERsyNXlPOvBtA</yt:channelId>
  <title>Two</title>
 </entry>
</feed>"#;

    #[test]
    fn parses_each_entry_with_its_full_channel_id() {
        let entries = parse_feed(FEED);
        assert_eq!(entries.len(), 2);
        assert_eq!(
            entries[0],
            FeedEntry {
                video_id: "BnV1AEeqBR4".to_string(),
                channel_id: "UCgYCMluaLpERsyNXlPOvBtA".to_string(),
                channel_name: "かみと".to_string(),
                title: "Bomb & defuse".to_string(),
                published: Some("2026-09-24T10:59:38+00:00".to_string()),
            }
        );
        assert_eq!(entries[1].video_id, "second");
        assert_eq!(entries[1].published, None);
    }

    fn pass(ids: &[String], outcome: FeedOutcome) -> Vec<(String, FeedOutcome)> {
        ids.iter().map(|id| (id.clone(), outcome.clone())).collect()
    }

    fn fresh_feed() -> FeedOutcome {
        FeedOutcome::Fresh {
            entries: Vec::new(),
            etag: Some("\"e1\"".to_string()),
            last_modified: None,
        }
    }

    #[test]
    fn feed_backoff_doubles_from_its_base_and_caps_at_an_hour() {
        assert_eq!(backoff(FEED_ERROR_BACKOFF, 1), Duration::from_secs(180));
        assert_eq!(backoff(FEED_ERROR_BACKOFF, 2), Duration::from_secs(360));
        assert_eq!(backoff(FEED_ERROR_BACKOFF, 5), Duration::from_secs(2880));
        assert_eq!(backoff(FEED_ERROR_BACKOFF, 6), MAX_FEED_BACKOFF);
        assert_eq!(backoff(RATE_LIMIT_BACKOFF, 1), Duration::from_secs(1800));
        assert_eq!(backoff(RATE_LIMIT_BACKOFF, 2), MAX_FEED_BACKOFF);
        assert_eq!(backoff(RATE_LIMIT_BACKOFF, 40), MAX_FEED_BACKOFF);
    }

    #[test]
    fn a_single_failing_feed_backs_off_alone_and_success_resets_it() {
        let ids = roster(6);
        let now = Instant::now();
        let mut health = RssHealth::default();
        let mut results = pass(&ids, fresh_feed());
        results[0].1 = FeedOutcome::Failed;
        health.record(results, now);
        assert_eq!(health.mode, RssMode::Up);
        assert_eq!(health.plan(&ids, now).len(), 5);
        assert_eq!(health.validators("UC1").0.as_deref(), Some("\"e1\""));

        let later = now + FEED_ERROR_BACKOFF;
        assert_eq!(health.plan(&ids, later).len(), 6);
        health.record(vec![(ids[0].clone(), FeedOutcome::NotModified)], later);
        assert_eq!(health.feeds["UC0"].failures, 0);
    }

    #[test]
    fn a_429_honors_retry_after_and_three_pause_every_feed() {
        let ids = roster(6);
        let now = Instant::now();
        let mut health = RssHealth::default();
        let limited = |secs: Option<u64>| FeedOutcome::RateLimited {
            retry_after: secs.map(Duration::from_secs),
        };
        let mut results = pass(&ids, fresh_feed());
        results[0].1 = limited(Some(90));
        results[1].1 = limited(None);
        health.record(results, now);
        assert_eq!(health.mode, RssMode::Up);
        assert_eq!(
            health.feeds["UC0"].retry_at,
            Some(now + Duration::from_secs(90))
        );
        assert_eq!(health.feeds["UC1"].retry_at, Some(now + RATE_LIMIT_BACKOFF));

        let mut results = pass(&ids, fresh_feed());
        results[2].1 = limited(Some(7200));
        results[3].1 = limited(None);
        results[4].1 = limited(Some(60));
        health.record(results, now);
        let until = now + Duration::from_secs(7200);
        assert_eq!(health.mode, RssMode::Paused { until });
        assert!(health.degraded(now));
        assert!(health.plan(&ids, now).is_empty());
        assert!(!health.plan(&ids, until).is_empty());
        assert_eq!(health.mode, RssMode::Up);
    }

    #[test]
    fn a_mostly_failing_pass_is_an_outage_probed_one_feed_at_a_time() {
        let ids = roster(6);
        let now = Instant::now();
        let mut health = RssHealth::default();
        let mut results = pass(&ids, FeedOutcome::Failed);
        results[5].1 = fresh_feed();
        health.record(results, now);
        let probe_at = now + OUTAGE_PROBE_EVERY;
        assert_eq!(health.mode, RssMode::Down { probe_at });
        assert!(health.degraded(now));
        assert!(
            health.feeds.values().all(|feed| feed.failures == 0),
            "outage passes do not penalize channels"
        );

        assert!(health.plan(&ids, now).is_empty());
        assert_eq!(health.plan(&ids, probe_at), vec!["UC0".to_string()]);
        health.record(pass(&roster(1), FeedOutcome::Failed), probe_at);
        let next = probe_at + OUTAGE_PROBE_EVERY;
        assert_eq!(health.mode, RssMode::Down { probe_at: next });
        assert_eq!(health.plan(&ids, next), vec!["UC1".to_string()]);
        health.record(vec![("UC1".to_string(), fresh_feed())], next);
        assert_eq!(health.mode, RssMode::Up);
        assert!(!health.degraded(next));
        assert_eq!(health.feeds["UC0"].failures, 0);
    }

    #[test]
    fn a_small_roster_is_never_called_an_outage() {
        let ids = roster(OUTAGE_MIN_FEEDS - 1);
        let mut health = RssHealth::default();
        health.record(pass(&ids, FeedOutcome::Failed), Instant::now());
        assert_eq!(health.mode, RssMode::Up);
        assert!(health.feeds.values().all(|feed| feed.failures == 1));
    }

    #[test]
    fn retry_after_takes_seconds_or_an_http_date() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-09-24T07:28:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        assert_eq!(
            parse_retry_after(" 120 ", now),
            Some(Duration::from_secs(120))
        );
        assert_eq!(
            parse_retry_after("Thu, 24 Sep 2026 07:30:00 GMT", now),
            Some(Duration::from_secs(120))
        );
        assert_eq!(
            parse_retry_after("Thu, 24 Sep 2026 07:00:00 GMT", now),
            Some(Duration::ZERO)
        );
        assert_eq!(parse_retry_after("999999", now), Some(MAX_RETRY_AFTER));
        assert_eq!(parse_retry_after("soon", now), None);
    }
}
