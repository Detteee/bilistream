//! Channel Atom parsing, conditional feed requests and RSS backoff.

use super::RSS_TICK;
use crate::logging::{human_elapsed, safe_diagnostic, safe_text, FailureReport};
use crate::plugins::http::response_bytes_limited;
use regex::Regex;
use reqwest::header::{ETAG, IF_MODIFIED_SINCE, IF_NONE_MATCH, LAST_MODIFIED, RETRY_AFTER};
use std::collections::{BTreeMap, HashMap, HashSet};
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
const RSS_BODY_LIMIT: usize = 2 * 1024 * 1024;

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

/// Reporting travels beside the health outcome; it never changes its meaning.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum FeedFailure {
    Timeout,
    Transport,
    Body,
    Http(u16),
}

pub(super) struct FeedRead {
    pub(super) outcome: FeedOutcome,
    pub(super) failure: Option<FeedFailure>,
}

impl From<FeedOutcome> for FeedRead {
    fn from(outcome: FeedOutcome) -> Self {
        Self {
            outcome,
            failure: None,
        }
    }
}

impl FeedRead {
    fn failed(failure: FeedFailure) -> Self {
        Self {
            outcome: FeedOutcome::Failed,
            failure: Some(failure),
        }
    }
}

fn failure_classes(causes: &[FeedFailure]) -> String {
    let mut counts = BTreeMap::new();
    for cause in causes {
        *counts.entry(*cause).or_insert(0_usize) += 1;
    }
    if counts.is_empty() {
        return "未提供分类".to_string();
    }
    let mut labels: Vec<String> = counts
        .iter()
        .take(6)
        .map(|(cause, count)| {
            let label = match cause {
                FeedFailure::Timeout => "请求超时".to_string(),
                FeedFailure::Transport => "连接/请求发送".to_string(),
                FeedFailure::Body => "响应读取/体积限制".to_string(),
                FeedFailure::Http(status) => format!("HTTP {status}"),
            };
            format!("{label}×{count}")
        })
        .collect();
    if counts.len() > labels.len() {
        labels.push(format!("另{}类", counts.len() - labels.len()));
    }
    labels.join(", ")
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
/// `record_with_causes` takes what came back and independent log categories.
pub(super) struct RssHealth {
    mode: RssMode,
    feeds: HashMap<String, FeedState>,
    probe_cursor: usize,
    // Reporting history is independent of mode, validators and feed backoff.
    report: FailureReport,
    report_roster: Vec<String>,
    unverified: HashSet<String>,
    report_node: Option<String>,
}

impl Default for RssHealth {
    fn default() -> Self {
        Self {
            mode: RssMode::Up,
            feeds: HashMap::new(),
            probe_cursor: 0,
            report: FailureReport::default(),
            report_roster: Vec::new(),
            unverified: HashSet::new(),
            report_node: None,
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
        if self.report_roster != roster {
            self.report_roster = roster.to_vec();
            self.reset_reporting();
        }
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
                    tracing::info!(
                        "{} YouTube RSS 暂停结束，恢复扫描，等待本轮结果",
                        self.node_tag()
                    );
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

    #[cfg(test)]
    pub(super) fn record(&mut self, results: Vec<(String, FeedOutcome)>, now: Instant) {
        self.record_with_causes(results, &[], now);
    }

    pub(super) fn record_with_causes(
        &mut self,
        results: Vec<(String, FeedOutcome)>,
        causes: &[FeedFailure],
        now: Instant,
    ) {
        let before = self.mode;
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
                self.mode = RssMode::Up;
            } else if attempted > 0 {
                self.mode = RssMode::Down {
                    probe_at: now + OUTAGE_PROBE_EVERY,
                };
            }
        } else if retry_afters.len() >= PAUSE_AFTER_429S {
            let longest = retry_afters.iter().flatten().max().copied();
            let pause = RATE_LIMIT_BACKOFF.max(longest.unwrap_or_default());
            self.mode = RssMode::Paused { until: now + pause };
        } else if attempted >= OUTAGE_MIN_FEEDS && failed * 2 >= attempted {
            self.mode = RssMode::Down {
                probe_at: now + OUTAGE_PROBE_EVERY,
            };
            penalize = false;
        }

        self.record_verification(&results, failed + retry_afters.len() > 0, now);
        self.note_round(before, attempted, failed, retry_afters.len(), causes, now);

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

    pub(super) fn reset_reporting(&mut self) {
        self.report = FailureReport::default();
        self.unverified.clear();
    }

    pub(super) fn set_report_node(&mut self, node: &str) {
        let node = safe_text(node, 64);
        if self.report_node.as_deref() != Some(node.as_str()) {
            self.report_node = Some(node);
            self.reset_reporting();
        }
    }

    /// `[node]` for this round's records; the raw node is kept for reporting state.
    fn node_tag(&self) -> String {
        crate::logging::node_tag(self.report_node.as_deref().unwrap_or("unknown"))
    }

    fn record_verification(
        &mut self,
        results: &[(String, FeedOutcome)],
        failed: bool,
        now: Instant,
    ) {
        let pending = self.report.summary(now).is_some();
        if failed && !pending {
            // Success from before this failure episode is not recovery proof.
            self.unverified = self.report_roster.iter().cloned().collect();
        }
        if failed || pending {
            for (id, outcome) in results {
                if !self.report_roster.contains(id) {
                    continue;
                }
                if matches!(
                    outcome,
                    FeedOutcome::Fresh { .. } | FeedOutcome::NotModified
                ) {
                    self.unverified.remove(id);
                } else {
                    self.unverified.insert(id.clone());
                }
            }
        }
    }

    fn note_round(
        &mut self,
        before: RssMode,
        attempted: usize,
        failed: usize,
        limited: usize,
        causes: &[FeedFailure],
        now: Instant,
    ) {
        if attempted == 0 {
            return;
        }
        let bad = failed + limited;
        let successful = attempted - bad;
        let probe = matches!(before, RssMode::Down { .. });
        if probe && successful > 0 {
            // The health state resumes scanning after one probe. It does not
            // establish that the other feeds have recovered.
            tracing::info!(
                "{} YouTube RSS 探测成功，恢复全量扫描；本次成功={}/{}，完整恢复待验证",
                self.node_tag(),
                successful,
                attempted
            );
            return;
        }
        if bad > 0 {
            let due = self.report.failure(now, bad as u64);
            let phase_changed =
                std::mem::discriminant(&before) != std::mem::discriminant(&self.mode);
            if let Some(summary) =
                due.or_else(|| phase_changed.then(|| self.report.summary(now)).flatten())
            {
                let impact = match self.mode {
                    RssMode::Down { .. } => format!(
                        "RSS 不可用，每 {} 探测一次",
                        human_elapsed(OUTAGE_PROBE_EVERY)
                    ),
                    RssMode::Paused { until } => format!(
                        "RSS 暂停 {}",
                        human_elapsed(until.saturating_duration_since(now))
                    ),
                    RssMode::Up => "保留缓存，按现有退避继续读取".to_string(),
                };
                tracing::warn!(
                    "{} YouTube RSS 本轮成功={}/{} 请求失败={} HTTP429={}；{}；{}；分类={}",
                    self.node_tag(),
                    successful,
                    attempted,
                    failed,
                    limited,
                    impact,
                    summary,
                    failure_classes(causes)
                );
            }
        } else if !probe
            && !self.report_roster.is_empty()
            && (attempted == self.report_roster.len()
                || (self.report.summary(now).is_some() && self.unverified.is_empty()))
        {
            if let Some(summary) = self.report.success(now) {
                tracing::info!(
                    "{} YouTube RSS 所有频道均已重新验证成功；本轮成功={}/{} 已验证={}/{} 此前失败={}次 持续={}",
                    self.node_tag(),
                    attempted,
                    attempted,
                    self.report_roster.len(),
                    self.report_roster.len(),
                    summary.failures,
                    human_elapsed(summary.elapsed)
                );
            }
        } else if let Some(summary) = self.report.pending(now) {
            tracing::info!(
                "{} YouTube RSS 本轮成功={}/{}，尚有{}个频道待重新验证；{}",
                self.node_tag(),
                successful,
                attempted,
                self.unverified.len(),
                summary
            );
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
    node: &str,
    channel_id: &str,
    etag: Option<String>,
    last_modified: Option<String>,
) -> FeedRead {
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
    read_feed_request(request, node, channel_id).await
}

async fn read_feed_request(
    request: reqwest::RequestBuilder,
    node: &str,
    channel_id: &str,
) -> FeedRead {
    let response = match request.send().await {
        Ok(response) => response,
        Err(e) => {
            let failure = if e.is_timeout() {
                FeedFailure::Timeout
            } else {
                FeedFailure::Transport
            };
            tracing::debug!(
                "{} YouTube RSS {} 读取失败: {}",
                node,
                safe_text(channel_id, 64),
                safe_diagnostic(&e.without_url().to_string())
            );
            return FeedRead::failed(failure);
        }
    };
    read_feed_response(response, node, channel_id).await
}

async fn read_feed_response(response: reqwest::Response, node: &str, channel_id: &str) -> FeedRead {
    let status = response.status();
    if status == reqwest::StatusCode::NOT_MODIFIED {
        return FeedOutcome::NotModified.into();
    }
    if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
        let retry_after = header_string(&response, RETRY_AFTER)
            .and_then(|value| parse_retry_after(&value, chrono::Utc::now()));
        tracing::debug!(
            "{} YouTube RSS {} HTTP 429，Retry-After={}",
            node,
            safe_text(channel_id, 64),
            retry_after
                .map(human_elapsed)
                .unwrap_or_else(|| "按现有退避".to_string())
        );
        return FeedRead {
            outcome: FeedOutcome::RateLimited { retry_after },
            failure: Some(FeedFailure::Http(429)),
        };
    }
    if !status.is_success() {
        tracing::debug!(
            "{} YouTube RSS {} 读取失败: {}",
            node,
            safe_text(channel_id, 64),
            status
        );
        return FeedRead::failed(FeedFailure::Http(status.as_u16()));
    }
    let etag = header_string(&response, ETAG);
    let last_modified = header_string(&response, LAST_MODIFIED);
    match response_bytes_limited(response, RSS_BODY_LIMIT).await {
        Ok(bytes) => FeedOutcome::Fresh {
            entries: parse_feed(&String::from_utf8_lossy(&bytes)),
            etag,
            last_modified,
        }
        .into(),
        Err(e) => {
            tracing::debug!(
                "{} YouTube RSS {} 读取失败: {}",
                node,
                safe_text(channel_id, 64),
                safe_diagnostic(&e.to_string())
            );
            FeedRead::failed(FeedFailure::Body)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pending_success_uses_the_actual_batch_denominator() {
        let now = Instant::now();
        let ids = roster(3);
        let (dispatch, logs) = crate::logging::test_capture::subscriber(tracing::Level::INFO);
        tracing::dispatcher::with_default(&dispatch, || {
            let mut health = RssHealth::default();
            health.set_report_node("jp");
            health.plan(&ids, now);
            health.record(pass(&ids, FeedOutcome::Failed), now);
            health.record(
                vec![(ids[0].clone(), FeedOutcome::NotModified)],
                now + Duration::from_secs(300),
            );
            assert!(health.report.summary(now).is_some());
        });
        let text = logs.text();
        assert!(
            text.contains("[jp] YouTube RSS 本轮成功=1/1，尚有2个频道待重新验证"),
            "{text}"
        );
        assert!(!text.contains("本轮成功=1/3"));
    }

    fn response(status: u16, body: Vec<u8>) -> reqwest::Response {
        axum::http::Response::builder()
            .status(status)
            .body(body)
            .unwrap()
            .into()
    }

    #[tokio::test]
    async fn real_feed_failures_reach_one_safe_round_summary_with_distinct_categories() {
        use tracing::instrument::WithSubscriber;

        crate::install_crypto_provider();
        let mut reads = Vec::new();
        for status in [403, 502] {
            let read =
                read_feed_response(response(status, Vec::new()), "jp", "UC_example_001").await;
            assert_eq!(read.outcome, FeedOutcome::Failed);
            assert_eq!(read.failure, Some(FeedFailure::Http(status)));
            reads.push(read);
        }
        let too_large = read_feed_response(
            response(200, vec![b'x'; RSS_BODY_LIMIT + 1]),
            "jp",
            "UC_example_002",
        )
        .await;
        assert_eq!(too_large.outcome, FeedOutcome::Failed);
        assert_eq!(too_large.failure, Some(FeedFailure::Body));
        reads.push(too_large);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let connection = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            drop(socket);
        });
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let (debug_dispatch, debug_logs) =
            crate::logging::test_capture::subscriber(tracing::Level::DEBUG);
        let disconnected = read_feed_request(
            client
                .get(format!("http://{address}/?key=synthetic-rss-token"))
                .timeout(Duration::from_secs(2)),
            "[jp]",
            "UC_example_003",
        )
        .with_subscriber(debug_dispatch)
        .await;
        connection.await.unwrap();
        assert_eq!(disconnected.outcome, FeedOutcome::Failed);
        assert_eq!(disconnected.failure, Some(FeedFailure::Transport));
        assert!(debug_logs
            .text()
            .contains("[jp] YouTube RSS UC_example_003 读取失败"));
        assert!(!debug_logs.text().contains("synthetic-rss-token"));
        reads.push(disconnected);

        let causes: Vec<_> = reads.iter().filter_map(|read| read.failure).collect();
        let results: Vec<_> = reads
            .into_iter()
            .enumerate()
            .map(|(index, read)| (format!("UC{index}"), read.outcome))
            .collect();
        let (dispatch, logs) = crate::logging::test_capture::subscriber(tracing::Level::INFO);
        tracing::dispatcher::with_default(&dispatch, || {
            let mut health = RssHealth::default();
            let now = Instant::now();
            health.set_report_node("jp");
            health.plan(&roster(4), now);
            health.record_with_causes(results, &causes, now);
            assert!(matches!(health.mode, RssMode::Down { .. }));
        });
        let text = logs.text();
        assert_eq!(text.matches("WARN [rss]").count(), 1, "{text}");
        for class in [
            "HTTP 403×1",
            "HTTP 502×1",
            "连接/请求发送×1",
            "响应读取/体积限制×1",
        ] {
            assert!(text.contains(class), "{text}");
        }
        assert!(!text.contains("synthetic-rss-token"));
    }

    #[tokio::test]
    async fn category_reporting_preserves_empty_feed_and_rate_limit_outcomes() {
        // Atom matching has never been a fallible parser. Do not relabel an
        // empty/unmatched successful body as Failed just to obtain a log class.
        let empty = read_feed_response(
            response(200, b"no matching Atom entry".to_vec()),
            "jp",
            "UC_example_001",
        )
        .await;
        assert!(matches!(empty.outcome, FeedOutcome::Fresh { entries, .. } if entries.is_empty()));
        assert_eq!(empty.failure, None);
        let limited = read_feed_response(response(429, Vec::new()), "jp", "UC_example_001").await;
        assert_eq!(
            limited.outcome,
            FeedOutcome::RateLimited { retry_after: None }
        );
        assert_eq!(limited.failure, Some(FeedFailure::Http(429)));
        let categories = failure_classes(&(400..420).map(FeedFailure::Http).collect::<Vec<_>>());
        assert!(categories.ends_with("另14类"));
        assert!(!categories.contains("HTTP 419"));
    }

    #[test]
    fn repeated_feed_failures_have_bounded_summaries_and_one_verified_recovery() {
        let now = Instant::now();
        let ids = roster(2);
        let (dispatch, logs) = crate::logging::test_capture::subscriber(tracing::Level::INFO);
        tracing::dispatcher::with_default(&dispatch, || {
            let mut health = RssHealth::default();
            health.set_report_node("jp");
            health.plan(&ids, now);
            for attempt in 0..12 {
                health.record(
                    vec![
                        (ids[0].clone(), FeedOutcome::Failed),
                        (ids[1].clone(), FeedOutcome::NotModified),
                    ],
                    now + Duration::from_secs(attempt * 30),
                );
            }
            assert_eq!(health.feeds[&ids[0]].failures, 12);
            assert_eq!(
                health.mode,
                RssMode::Up,
                "reporting does not classify a small roster as an outage"
            );
            health.record(
                ids.iter()
                    .cloned()
                    .map(|id| (id, FeedOutcome::NotModified))
                    .collect(),
                now + Duration::from_secs(360),
            );
            health.record(
                ids.iter()
                    .cloned()
                    .map(|id| (id, FeedOutcome::NotModified))
                    .collect(),
                now + Duration::from_secs(361),
            );
        });
        let text = logs.text();
        assert_eq!(text.matches("WARN [rss]").count(), 2, "{text}");
        assert!(text.contains("累计失败=11次 持续=5分"));
        assert!(text.contains("本轮成功=1/2 请求失败=1 HTTP429=0"));
        assert!(text.contains("[jp] YouTube RSS 本轮成功=1/2"));
        assert_eq!(text.matches("所有频道均已重新验证成功").count(), 1);
        assert!(text.contains("此前失败=12次 持续=6分"));
    }

    #[test]
    fn probe_and_partial_rounds_do_not_claim_full_roster_recovery() {
        let now = Instant::now();
        let ids = roster(4);
        let (dispatch, logs) = crate::logging::test_capture::subscriber(tracing::Level::INFO);
        tracing::dispatcher::with_default(&dispatch, || {
            let mut health = RssHealth::default();
            health.plan(&ids, now);
            health.record(
                ids.iter()
                    .cloned()
                    .map(|id| (id, FeedOutcome::Failed))
                    .collect(),
                now,
            );
            let probe_at = now + OUTAGE_PROBE_EVERY;
            let probe = health.plan(&ids, probe_at);
            assert_eq!(probe.len(), 1);
            health.record(vec![(probe[0].clone(), FeedOutcome::NotModified)], probe_at);
            assert_eq!(health.mode, RssMode::Up);
            assert!(logs.text().contains("探测成功，恢复全量扫描"));
            assert!(!logs.text().contains("所有频道均已重新验证成功"));
            assert_eq!(health.report.summary(probe_at).unwrap().failures, 4);

            let partial_at = probe_at + RSS_TICK;
            health.record(
                ids.iter()
                    .enumerate()
                    .map(|(index, id)| {
                        (
                            id.clone(),
                            if index == 0 {
                                FeedOutcome::Failed
                            } else {
                                FeedOutcome::NotModified
                            },
                        )
                    })
                    .collect(),
                partial_at,
            );
            assert!(logs.text().contains("本轮成功=3/4"));
            assert!(!logs.text().contains("所有频道均已重新验证成功"));
            let subset_at = partial_at + Duration::from_secs(1);
            health.record(
                ids[1..]
                    .iter()
                    .cloned()
                    .map(|id| (id, FeedOutcome::NotModified))
                    .collect(),
                subset_at,
            );
            assert!(!logs.text().contains("所有频道均已重新验证成功"));
            health.record(
                ids.iter()
                    .cloned()
                    .map(|id| (id, FeedOutcome::NotModified))
                    .collect(),
                partial_at + RSS_TICK,
            );
            assert!(health.report.summary(partial_at + RSS_TICK).is_none());
        });
        let text = logs.text();
        assert_eq!(
            text.matches("所有频道均已重新验证成功").count(),
            1,
            "{text}"
        );
        assert!(text.contains("本轮成功=4/4 已验证=4/4 此前失败=5次"));
    }

    #[test]
    fn a_new_roster_starts_new_reporting_without_resetting_health() {
        let now = Instant::now();
        let mut health = RssHealth::default();
        let ids = roster(4);
        health.plan(&ids, now);
        health.record(
            ids.iter()
                .cloned()
                .map(|id| (id, FeedOutcome::Failed))
                .collect(),
            now,
        );
        let mode = health.mode;
        health.plan(&["UC_example_002".to_string()], now);
        assert!(health.report.summary(now).is_none());
        assert_eq!(health.mode, mode);
        health.record(
            vec![("UC_example_002".to_string(), FeedOutcome::Failed)],
            now,
        );
        assert_eq!(health.report.summary(now).unwrap().failures, 1);
    }

    #[test]
    fn staggered_backoff_recovers_through_real_planned_rounds() {
        let now = Instant::now();
        let ids = roster(3);
        let (dispatch, logs) = crate::logging::test_capture::subscriber(tracing::Level::INFO);
        tracing::dispatcher::with_default(&dispatch, || {
            let mut health = RssHealth::default();
            assert_eq!(health.plan(&ids, now), ids);
            health.record(
                vec![
                    (
                        ids[0].clone(),
                        FeedOutcome::RateLimited {
                            retry_after: Some(Duration::from_secs(300)),
                        },
                    ),
                    (ids[1].clone(), FeedOutcome::Failed),
                    (ids[2].clone(), FeedOutcome::NotModified),
                ],
                now,
            );
            let first_at = now + FEED_ERROR_BACKOFF;
            let first = health.plan(&ids, first_at);
            assert_eq!(first, ids[1..]);
            health.record(pass(&first, FeedOutcome::NotModified), first_at);
            assert_eq!(health.unverified, HashSet::from([ids[0].clone()]));
            assert!(!logs.text().contains("所有频道均已重新验证成功"));

            // Recovered feeds have no retry_at, so the final backed-off feed
            // joins them in the next actual plan rather than staying pending.
            let final_at = now + Duration::from_secs(300);
            let final_round = health.plan(&ids, final_at);
            assert_eq!(final_round, ids);
            health.record(pass(&final_round, FeedOutcome::NotModified), final_at);
            assert!(health.report.summary(final_at).is_none());
            assert_eq!(health.mode, RssMode::Up);
        });
        assert_eq!(logs.text().matches("所有频道均已重新验证成功").count(), 1);
    }

    #[test]
    fn verification_can_finish_across_partial_batches_and_a_new_failure_invalidates_it() {
        let now = Instant::now();
        let ids = roster(3);
        let (dispatch, logs) = crate::logging::test_capture::subscriber(tracing::Level::INFO);
        tracing::dispatcher::with_default(&dispatch, || {
            let mut health = RssHealth::default();
            health.plan(&ids, now);
            health.record(pass(&ids, FeedOutcome::Failed), now);
            health.record(
                vec![(ids[0].clone(), FeedOutcome::NotModified)],
                now + RSS_TICK,
            );
            health.record(
                vec![(ids[1].clone(), FeedOutcome::NotModified)],
                now + RSS_TICK + Duration::from_secs(1),
            );
            health.record(
                vec![(ids[0].clone(), FeedOutcome::Failed)],
                now + RSS_TICK + Duration::from_secs(2),
            );
            health.record(
                vec![(ids[2].clone(), FeedOutcome::NotModified)],
                now + RSS_TICK + Duration::from_secs(3),
            );
            assert!(!logs.text().contains("所有频道均已重新验证成功"));
            health.record(
                vec![(ids[0].clone(), FeedOutcome::NotModified)],
                now + RSS_TICK + Duration::from_secs(4),
            );
            assert!(health.unverified.is_empty());
            assert!(health
                .report
                .summary(now + RSS_TICK + Duration::from_secs(4))
                .is_none());
        });
        let text = logs.text();
        assert_eq!(text.matches("所有频道均已重新验证成功").count(), 1);
        assert!(text.contains("本轮成功=1/1 已验证=3/3 此前失败=4次"));
    }

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
