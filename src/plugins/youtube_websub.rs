//! Push discovery through YouTube's WebSub (PubSubHubbub) hub.
//!
//! The hub POSTs a small Atom snippet to our callback whenever a subscribed
//! channel publishes or edits a video. It is a hint only: pushes can be late or
//! missing, fire for edits and old videos, and never for a waiting room going
//! live. So a push only queues its video ID for discovery, which confirms it
//! with `videos.list` like any RSS or playlist entry.
//!
//! The callback runs on its own listener that serves nothing but
//! [`CALLBACK_PATH`], so exposing that port can never expose the WebUI. Without
//! a callback URL (or without a YouTube key) nothing listens and the hub is
//! never contacted.

use super::http::pooled_client;
use super::youtube_rss::{parse_feed, roster_channel_ids, FeedEntry};
use crate::config::load_config;
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Query};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use futures_util::stream::{self, StreamExt};
use hmac::{Hmac, KeyInit, Mac};
use sha1::Sha1;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU16, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};
use tokio::sync::Notify;

pub const CALLBACK_PATH: &str = "/websub/youtube";
const HUB_URL: &str = "https://pubsubhubbub.appspot.com/subscribe";
const TOPIC_PREFIX: &str = "https://www.youtube.com/xml/feeds/videos.xml?channel_id=";
/// Also accepted in verification, in case the hub echoes the plain feed URL.
const ALT_TOPIC_PREFIX: &str = "https://www.youtube.com/feeds/videos.xml?channel_id=";
const LEASE_SECONDS: u64 = 864_000;
const TICK: Duration = Duration::from_secs(30);
/// A subscription the hub hasn't verified by then counts as failed.
const PENDING_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const RETRY_AFTER_FAILURE: Duration = Duration::from_secs(10 * 60);
/// Pushed entries stay discovery candidates this long.
const PUSH_KEEP: Duration = Duration::from_secs(24 * 60 * 60);
/// Pushes count as healthy while one was verified within this window.
const HEALTHY_WINDOW: Duration = Duration::from_secs(24 * 60 * 60);
const BODY_LIMIT: usize = 256 * 1024;
const HUB_CONCURRENCY: usize = 4;
/// Global token bucket on the callback: behind a proxy every request shares
/// one address, so a per-IP limit would be meaningless.
const RATE_PER_SEC: f64 = 20.0;
const RATE_BURST: f64 = 100.0;

static ENABLED: AtomicBool = AtomicBool::new(true);
static WEBUI_PORT: AtomicU16 = AtomicU16::new(0);
static HUB: Mutex<Option<Hub>> = Mutex::new(None);
static PUSH_WAKE: Notify = Notify::const_new();
static WORKER_STARTED: AtomicBool = AtomicBool::new(false);

/// Lets a caller (the cluster build) keep this node from subscribing. Checked
/// once per subscriber cycle.
pub fn set_enabled(enabled: bool) {
    ENABLED.store(enabled, Ordering::SeqCst);
}

pub(crate) fn set_webui_port(port: u16) {
    WEBUI_PORT.store(port, Ordering::SeqCst);
}

/// The WebUI's port once it is listening.
pub(crate) fn webui_port() -> Option<u16> {
    Some(WEBUI_PORT.load(Ordering::SeqCst)).filter(|port| *port != 0)
}

/// Per-process `hub.secret`: 32 random bytes, hex encoded. Never logged.
fn secret() -> &'static str {
    static SECRET: OnceLock<String> = OnceLock::new();
    SECRET.get_or_init(|| {
        let mut bytes = [0u8; 32];
        getrandom::fill(&mut bytes).expect("OS random source");
        hex(&bytes)
    })
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(text.get(i..i + 2)?, 16).ok())
        .collect()
}

fn topic(channel_id: &str) -> String {
    format!("{TOPIC_PREFIX}{channel_id}")
}

fn topic_channel(topic: &str) -> Option<&str> {
    topic
        .strip_prefix(TOPIC_PREFIX)
        .or_else(|| topic.strip_prefix(ALT_TOPIC_PREFIX))
        .filter(|id| !id.is_empty())
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum SubState {
    Pending { since: Instant },
    Verified { renew_at: Instant },
    Failed { retry_at: Instant },
}

struct Pushed {
    entry: FeedEntry,
    at: Instant,
    /// Not yet handed to discovery.
    fresh: bool,
}

struct Bucket {
    tokens: f64,
    at: Instant,
}

impl Bucket {
    fn take(&mut self, now: Instant) -> bool {
        let elapsed = now.saturating_duration_since(self.at).as_secs_f64();
        self.tokens = (self.tokens + elapsed * RATE_PER_SEC).min(RATE_BURST);
        self.at = now;
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

/// Subscriber and callback state, shared by the worker and the handlers.
struct Hub {
    subs: HashMap<String, SubState>,
    /// Unsubscribes the hub may still verify.
    unsubs: HashSet<String>,
    roster: HashSet<String>,
    pushed: HashMap<String, Pushed>,
    last_push: Option<(Instant, chrono::DateTime<chrono::Utc>)>,
    bucket: Bucket,
    listening: Option<u16>,
    error: Option<String>,
}

impl Hub {
    fn new(now: Instant) -> Self {
        Self {
            subs: HashMap::new(),
            unsubs: HashSet::new(),
            roster: HashSet::new(),
            pushed: HashMap::new(),
            last_push: None,
            bucket: Bucket {
                tokens: RATE_BURST,
                at: now,
            },
            listening: None,
            error: None,
        }
    }

    fn counts(&self) -> (usize, usize, usize) {
        self.subs
            .values()
            .fold((0, 0, 0), |(v, p, f), state| match state {
                SubState::Verified { .. } => (v + 1, p, f),
                SubState::Pending { .. } => (v, p + 1, f),
                SubState::Failed { .. } => (v, p, f + 1),
            })
    }

    fn healthy(&self, now: Instant) -> bool {
        let (verified, _, failed) = self.counts();
        verified > 0
            && failed == 0
            && self
                .last_push
                .is_some_and(|(at, _)| now.saturating_duration_since(at) < HEALTHY_WINDOW)
    }

    /// Answer to a verification GET: the challenge to echo, or `None` (404)
    /// for anything we did not ask for.
    fn verify(&mut self, query: &HashMap<String, String>, now: Instant) -> Option<String> {
        let mode = query.get("hub.mode")?.as_str();
        let channel = topic_channel(query.get("hub.topic")?)?.to_string();
        let challenge = query.get("hub.challenge").cloned();
        match mode {
            "subscribe" => {
                if !matches!(self.subs.get(&channel), Some(SubState::Pending { .. })) {
                    return None;
                }
                let lease = query
                    .get("hub.lease_seconds")
                    .and_then(|secs| secs.parse::<u64>().ok())
                    .unwrap_or(LEASE_SECONDS);
                self.subs.insert(
                    channel,
                    SubState::Verified {
                        renew_at: now + Duration::from_secs(lease * 4 / 5),
                    },
                );
                challenge
            }
            "unsubscribe" => self.unsubs.remove(&channel).then_some(challenge?),
            "denied" => {
                self.subs.get(&channel)?;
                tracing::warn!("WebSub 订阅被拒绝: {}", channel);
                self.subs.insert(
                    channel,
                    SubState::Failed {
                        retry_at: now + RETRY_AFTER_FAILURE,
                    },
                );
                Some(String::new())
            }
            _ => None,
        }
    }

    /// Accept a signed push: queue its roster entries for discovery. Returns
    /// the queued video IDs.
    fn push(
        &mut self,
        secret: &str,
        signature: Option<&str>,
        body: &[u8],
        now: Instant,
    ) -> Result<Vec<String>, StatusCode> {
        if !signature_ok(secret, signature, body) {
            return Err(StatusCode::UNAUTHORIZED);
        }
        self.last_push = Some((now, chrono::Utc::now()));
        let text = String::from_utf8_lossy(body);
        let mut queued = Vec::new();
        for entry in parse_feed(&text) {
            if !self.roster.contains(&entry.channel_id) {
                continue;
            }
            queued.push(entry.video_id.clone());
            self.pushed.insert(
                entry.video_id.clone(),
                Pushed {
                    entry,
                    at: now,
                    fresh: true,
                },
            );
        }
        Ok(queued)
    }
}

/// `X-Hub-Signature: sha1=<hex hmac of the raw body>`, compared in constant time.
fn signature_ok(secret: &str, signature: Option<&str>, body: &[u8]) -> bool {
    let Some(expected) = signature
        .and_then(|value| value.trim().strip_prefix("sha1="))
        .and_then(unhex)
    else {
        return false;
    };
    let Ok(mut mac) = Hmac::<Sha1>::new_from_slice(secret.as_bytes()) else {
        return false;
    };
    mac.update(body);
    mac.verify_slice(&expected).is_ok()
}

fn with_hub<T>(f: impl FnOnce(&mut Hub) -> T) -> T {
    let mut guard = HUB.lock().unwrap_or_else(|e| e.into_inner());
    f(guard.get_or_insert_with(|| Hub::new(Instant::now())))
}

fn rate_ok() -> bool {
    with_hub(|hub| hub.bucket.take(Instant::now()))
}

async fn verify_handler(Query(query): Query<HashMap<String, String>>) -> Response {
    if !rate_ok() {
        return StatusCode::TOO_MANY_REQUESTS.into_response();
    }
    match with_hub(|hub| hub.verify(&query, Instant::now())) {
        Some(challenge) => ([(header::CONTENT_TYPE, "text/plain")], challenge).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn push_handler(headers: HeaderMap, body: Bytes) -> Response {
    if !rate_ok() {
        return StatusCode::TOO_MANY_REQUESTS.into_response();
    }
    let signature = headers
        .get("x-hub-signature")
        .and_then(|value| value.to_str().ok());
    match with_hub(|hub| hub.push(secret(), signature, &body, Instant::now())) {
        Ok(queued) => {
            if !queued.is_empty() {
                tracing::debug!("WebSub 推送: {}", queued.join(", "));
                PUSH_WAKE.notify_one();
            }
            StatusCode::NO_CONTENT.into_response()
        }
        Err(status) => status.into_response(),
    }
}

/// Serves only [`CALLBACK_PATH`]; every other path is 404.
pub fn callback_router() -> Router {
    Router::new()
        .route(CALLBACK_PATH, get(verify_handler).post(push_handler))
        .layer(DefaultBodyLimit::max(BODY_LIMIT))
}

/// Resolves when a push queued new entries.
pub(crate) async fn pushed() {
    PUSH_WAKE.notified().await;
}

/// Pushed entries still worth classifying, and the IDs pushed since the last
/// call.
pub(crate) fn pushed_entries() -> (Vec<FeedEntry>, Vec<String>) {
    let mut guard = HUB.lock().unwrap_or_else(|e| e.into_inner());
    let Some(hub) = guard.as_mut() else {
        return (Vec::new(), Vec::new());
    };
    let now = Instant::now();
    hub.pushed
        .retain(|_, pushed| now.saturating_duration_since(pushed.at) < PUSH_KEEP);
    let mut fresh = Vec::new();
    let entries = hub
        .pushed
        .iter_mut()
        .map(|(id, pushed)| {
            if std::mem::take(&mut pushed.fresh) {
                fresh.push(id.clone());
            }
            pushed.entry.clone()
        })
        .collect();
    (entries, fresh)
}

/// Whether pushes are arriving: a verified push within 24h, no failing
/// subscription, and at least one verified.
pub(crate) fn healthy() -> bool {
    let guard = HUB.lock().unwrap_or_else(|e| e.into_inner());
    guard
        .as_ref()
        .is_some_and(|hub| hub.healthy(Instant::now()))
}

/// Subscriptions and pushes for the settings view; `None` while WebSub is off.
pub(crate) fn status() -> Option<serde_json::Value> {
    let guard = HUB.lock().unwrap_or_else(|e| e.into_inner());
    let hub = guard.as_ref()?;
    if hub.listening.is_none() && hub.error.is_none() && hub.subs.is_empty() {
        return None;
    }
    let (verified, pending, failed) = hub.counts();
    Some(serde_json::json!({
        "verified": verified,
        "pending": pending,
        "failed": failed,
        "last_push": hub.last_push.map(|(_, at)| at.to_rfc3339()),
        "listening": hub.listening,
        "error": hub.error,
        "healthy": hub.healthy(Instant::now()),
    }))
}

async fn hub_request(
    client: reqwest::Client,
    subscribe: bool,
    callback: String,
    channel: String,
) -> (String, Result<(), String>) {
    let mode = if subscribe {
        "subscribe"
    } else {
        "unsubscribe"
    };
    let lease = LEASE_SECONDS.to_string();
    let topic = topic(&channel);
    let mut form = vec![
        ("hub.mode", mode),
        ("hub.topic", topic.as_str()),
        ("hub.callback", callback.as_str()),
        ("hub.verify", "async"),
    ];
    if subscribe {
        form.push(("hub.lease_seconds", lease.as_str()));
        form.push(("hub.secret", secret()));
    }
    let body = match serde_urlencoded::to_string(&form) {
        Ok(body) => body,
        Err(e) => return (channel, Err(e.to_string())),
    };
    let result = client
        .post(HUB_URL)
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(body)
        .send()
        .await
        .map_err(|e| e.without_url().to_string())
        .and_then(|response| {
            if response.status().is_success() {
                Ok(())
            } else {
                Err(format!("HTTP {}", response.status()))
            }
        });
    (channel, result)
}

struct Listener {
    port: u16,
    task: tokio::task::JoinHandle<()>,
}

#[derive(Default)]
struct Worker {
    listener: Option<Listener>,
    callback: Option<String>,
    counts: (usize, usize, usize),
}

impl Worker {
    async fn stop_listener(&mut self) {
        if let Some(listener) = self.listener.take() {
            listener.task.abort();
            let _ = listener.task.await;
            tracing::info!("WebSub 回调监听已关闭 (端口 {})", listener.port);
        }
    }

    async fn reconcile_listener(&mut self, port: u16) {
        if self.listener.as_ref().is_some_and(|l| l.port == port) {
            return;
        }
        self.stop_listener().await;
        let addr = std::net::SocketAddr::new(crate::webui::listen::listen_bind(), port);
        let bound = if webui_port() == Some(port) {
            Err("与 Web UI 端口相同".to_string())
        } else {
            tokio::net::TcpListener::bind(addr)
                .await
                .map_err(|e| e.to_string())
        };
        match bound {
            Ok(listener) => {
                tracing::info!("WebSub 回调监听: {}{}", addr, CALLBACK_PATH);
                let task = tokio::spawn(async move {
                    if let Err(e) = axum::serve(listener, callback_router()).await {
                        tracing::warn!("WebSub 回调监听退出: {}", e);
                    }
                });
                self.listener = Some(Listener { port, task });
                with_hub(|hub| {
                    hub.listening = Some(port);
                    hub.error = None;
                });
            }
            Err(e) => {
                let message = format!("WebSub 回调端口 {} 无法监听: {}", port, e);
                let changed = with_hub(|hub| {
                    hub.listening = None;
                    let changed = hub.error.as_deref() != Some(message.as_str());
                    hub.error = Some(message.clone());
                    changed
                });
                if changed {
                    tracing::warn!("{}", message);
                }
            }
        }
    }

    async fn tick(&mut self) {
        let Ok(cfg) = load_config().await else {
            return;
        };
        let callback = cfg
            .youtube_websub_callback_url
            .as_deref()
            .map(str::trim)
            .filter(|url| !url.is_empty())
            .map(str::to_string);
        let active = callback.is_some()
            && !cfg.youtube_api_keys().is_empty()
            && ENABLED.load(Ordering::SeqCst);
        let Some(callback) = callback.filter(|_| active) else {
            if self.callback.take().is_some() {
                tracing::info!("WebSub 已关闭");
            }
            self.stop_listener().await;
            *HUB.lock().unwrap_or_else(|e| e.into_inner()) = None;
            self.counts = (0, 0, 0);
            return;
        };
        self.reconcile_listener(cfg.youtube_websub_port).await;

        let roster = roster_channel_ids(&cfg).await;
        let now = Instant::now();
        let callback_changed = self.callback.as_deref() != Some(callback.as_str());
        self.callback = Some(callback.clone());
        let (subscribe, unsubscribe) = with_hub(|hub| {
            if callback_changed {
                hub.subs.clear();
            }
            hub.roster = roster.iter().cloned().collect();
            let removed: Vec<String> = hub
                .subs
                .keys()
                .filter(|id| !hub.roster.contains(*id))
                .cloned()
                .collect();
            for id in &removed {
                hub.subs.remove(id);
                hub.unsubs.insert(id.clone());
            }
            for state in hub.subs.values_mut() {
                if let SubState::Pending { since } = *state {
                    if now.saturating_duration_since(since) >= PENDING_TIMEOUT {
                        *state = SubState::Failed {
                            retry_at: now + RETRY_AFTER_FAILURE,
                        };
                    }
                }
            }
            let due: Vec<String> = roster
                .iter()
                .filter(|id| match hub.subs.get(*id) {
                    None => true,
                    Some(SubState::Pending { .. }) => false,
                    Some(SubState::Verified { renew_at }) => *renew_at <= now,
                    Some(SubState::Failed { retry_at }) => *retry_at <= now,
                })
                .cloned()
                .collect();
            // Pending before the request: the hub may verify before it answers.
            for id in &due {
                hub.subs
                    .insert(id.clone(), SubState::Pending { since: now });
            }
            (due, removed)
        });

        if !subscribe.is_empty() || !unsubscribe.is_empty() {
            match pooled_client(cfg.youtube.proxy.as_deref()) {
                Ok(client) => {
                    // Owned items: borrowing futures make tick() unspawnable.
                    let jobs: Vec<(bool, String)> = subscribe
                        .into_iter()
                        .map(|id| (true, id))
                        .chain(unsubscribe.into_iter().map(|id| (false, id)))
                        .collect();
                    let results: Vec<(bool, String, Result<(), String>)> = stream::iter(jobs)
                        .map(|(subscribe, id)| {
                            let client = client.clone();
                            let callback = callback.clone();
                            async move {
                                let (id, result) =
                                    hub_request(client, subscribe, callback, id).await;
                                (subscribe, id, result)
                            }
                        })
                        .buffer_unordered(HUB_CONCURRENCY)
                        .collect()
                        .await;
                    with_hub(|hub| {
                        for (subscribe, id, result) in results {
                            let Err(e) = result else { continue };
                            let mode = if subscribe { "订阅" } else { "退订" };
                            tracing::debug!("WebSub {} {} 失败: {}", mode, id, e);
                            if subscribe {
                                hub.subs.insert(
                                    id,
                                    SubState::Failed {
                                        retry_at: Instant::now() + RETRY_AFTER_FAILURE,
                                    },
                                );
                            } else {
                                hub.unsubs.remove(&id);
                            }
                        }
                    });
                }
                Err(e) => tracing::warn!("WebSub 无法创建 HTTP 客户端: {}", e),
            }
        }

        let counts = with_hub(|hub| hub.counts());
        if counts != self.counts {
            self.counts = counts;
            tracing::info!(
                "WebSub 订阅: 已验证 {} / 等待 {} / 失败 {}",
                counts.0,
                counts.1,
                counts.2
            );
        }
    }
}

pub struct WebSubWorker(tokio::task::JoinHandle<()>);

impl Drop for WebSubWorker {
    fn drop(&mut self) {
        self.0.abort();
        WORKER_STARTED.store(false, Ordering::SeqCst);
    }
}

pub fn start_websub_worker() -> Option<WebSubWorker> {
    if WORKER_STARTED.swap(true, Ordering::SeqCst) {
        return None;
    }
    Some(WebSubWorker(tokio::spawn(async {
        let mut worker = Worker::default();
        loop {
            worker.tick().await;
            tokio::time::sleep(TICK).await;
        }
    })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use tower::ServiceExt;

    const CHANNEL: &str = "UCgYCMluaLpERsyNXlPOvBtA";
    const PUSH: &str = r#"<feed xmlns:yt="http://www.youtube.com/xml/schemas/2015" xmlns="http://www.w3.org/2005/Atom">
 <entry>
  <id>yt:video:VIDEO123456</id>
  <yt:videoId>VIDEO123456</yt:videoId>
  <yt:channelId>UCgYCMluaLpERsyNXlPOvBtA</yt:channelId>
  <title>Title</title>
  <author><name>Name</name></author>
 </entry>
</feed>"#;

    fn sign(secret: &str, body: &[u8]) -> String {
        let mut mac = Hmac::<Sha1>::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(body);
        format!("sha1={}", hex(&mac.finalize().into_bytes()))
    }

    fn hub_with_roster(now: Instant) -> Hub {
        let mut hub = Hub::new(now);
        hub.roster.insert(CHANNEL.to_string());
        hub
    }

    fn query(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn unsigned_or_badly_signed_pushes_are_rejected_untouched() {
        let now = Instant::now();
        let mut hub = hub_with_roster(now);
        let body = PUSH.as_bytes();
        assert_eq!(
            hub.push("secret", None, body, now),
            Err(StatusCode::UNAUTHORIZED)
        );
        let wrong = sign("other", body);
        assert_eq!(
            hub.push("secret", Some(&wrong), body, now),
            Err(StatusCode::UNAUTHORIZED)
        );
        assert_eq!(
            hub.push("secret", Some("sha1=zz"), body, now),
            Err(StatusCode::UNAUTHORIZED)
        );
        assert!(hub.pushed.is_empty());
        assert!(hub.last_push.is_none());

        let good = sign("secret", body);
        assert_eq!(
            hub.push("secret", Some(&good), body, now),
            Ok(vec!["VIDEO123456".to_string()])
        );
        assert!(hub.last_push.is_some());
    }

    #[test]
    fn pushes_skip_off_roster_channels_and_deleted_entries() {
        let now = Instant::now();
        let mut hub = Hub::new(now);
        let body = PUSH.as_bytes();
        let sig = sign("s", body);
        assert_eq!(hub.push("s", Some(&sig), body, now), Ok(vec![]));

        let deleted = r#"<feed><at:deleted-entry ref="yt:video:VIDEO123456"><at:by><uri>https://www.youtube.com/channel/UCgYCMluaLpERsyNXlPOvBtA</uri></at:by></at:deleted-entry></feed>"#;
        let mut hub = hub_with_roster(now);
        let sig = sign("s", deleted.as_bytes());
        assert_eq!(
            hub.push("s", Some(&sig), deleted.as_bytes(), now),
            Ok(vec![])
        );
    }

    #[test]
    fn verification_echoes_only_pending_topics() {
        let now = Instant::now();
        let mut hub = hub_with_roster(now);
        let q = query(&[
            ("hub.mode", "subscribe"),
            ("hub.topic", &topic(CHANNEL)),
            ("hub.challenge", "abc"),
            ("hub.lease_seconds", "1000"),
        ]);
        assert_eq!(hub.verify(&q, now), None, "not requested");

        hub.subs
            .insert(CHANNEL.to_string(), SubState::Pending { since: now });
        assert_eq!(hub.verify(&q, now), Some("abc".to_string()));
        assert_eq!(
            hub.subs.get(CHANNEL),
            Some(&SubState::Verified {
                renew_at: now + Duration::from_secs(800)
            }),
            "renews at 80% of the granted lease"
        );
        assert_eq!(hub.verify(&q, now), None, "already verified");

        hub.subs
            .insert(CHANNEL.to_string(), SubState::Pending { since: now });
        let alt = query(&[
            ("hub.mode", "subscribe"),
            ("hub.topic", &format!("{ALT_TOPIC_PREFIX}{CHANNEL}")),
            ("hub.challenge", "xyz"),
        ]);
        assert_eq!(hub.verify(&alt, now), Some("xyz".to_string()));

        let unsub = query(&[
            ("hub.mode", "unsubscribe"),
            ("hub.topic", &topic("UCother")),
            ("hub.challenge", "u"),
        ]);
        assert_eq!(hub.verify(&unsub, now), None);
        hub.unsubs.insert("UCother".to_string());
        assert_eq!(hub.verify(&unsub, now), Some("u".to_string()));
        assert!(hub.unsubs.is_empty());
    }

    #[test]
    fn healthy_needs_a_recent_push_and_no_failures() {
        let now = Instant::now();
        let mut hub = hub_with_roster(now);
        hub.subs
            .insert(CHANNEL.to_string(), SubState::Verified { renew_at: now });
        assert!(!hub.healthy(now), "no push yet");
        hub.last_push = Some((now, chrono::Utc::now()));
        assert!(hub.healthy(now));
        assert!(!hub.healthy(now + HEALTHY_WINDOW), "24h silent");
        hub.subs
            .insert("UCother".to_string(), SubState::Failed { retry_at: now });
        assert!(!hub.healthy(now), "a failing subscription");
    }

    #[test]
    fn rate_cap_refills_over_time() {
        let now = Instant::now();
        let mut bucket = Bucket {
            tokens: 2.0,
            at: now,
        };
        assert!(bucket.take(now));
        assert!(bucket.take(now));
        assert!(!bucket.take(now));
        assert!(bucket.take(now + Duration::from_millis(100)));
    }

    #[tokio::test]
    async fn callback_router_serves_only_the_callback_path() {
        for path in ["/", "/api/config", "/websub", "/index.html"] {
            let response = callback_router()
                .oneshot(axum::http::Request::get(path).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::NOT_FOUND, "{path}");
        }
        let response = callback_router()
            .oneshot(
                axum::http::Request::get(format!(
                    "{CALLBACK_PATH}?hub.mode=subscribe&hub.topic=x&hub.challenge=c"
                ))
                .body(Body::empty())
                .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "unknown topic");
        let response = callback_router()
            .oneshot(
                axum::http::Request::post(CALLBACK_PATH)
                    .body(Body::from(PUSH))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }
}
