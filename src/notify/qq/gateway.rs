//! QQ gateway session (design §7, research §6).
//!
//! Runs only on the gateway-owning node (the execution primary) while
//! `keep_online` is on or a bind window is open. It reports what it sees to
//! the worker over a channel; it never sends a message itself. Reconnect
//! backoff lives in this task, not in the worker loop.

use super::token::{AccessToken, TokenCache};
use futures_util::{SinkExt, StreamExt};
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::tungstenite::Message;

/// GROUP_AND_C2C_EVENT: C2C_MESSAGE_CREATE, FRIEND_ADD, C2C_MSG_REJECT/RECEIVE.
pub const INTENTS: u64 = 1 << 25;
const MAX_BACKOFF: Duration = Duration::from_secs(300);
const MAX_FRAME_BYTES: usize = 64 * 1024;

/// `Debug` is manual: openids and message text (which may be the pairing
/// code) never reach a log or panic message.
#[derive(Clone, PartialEq, Eq)]
pub enum GatewayEvent {
    Ready,
    Closed {
        reason: String,
        /// Reconnecting cannot help (4013/4014/4914/4915, bad credentials);
        /// the task ended.
        fatal: bool,
        /// The AppID/AppSecret were rejected (token business code 100016,
        /// 100007 or 10004, or two 4004 closes in a row before READY). The
        /// worker sets `bad_credentials` and keeps the gateway down until the
        /// credentials are saved again.
        bad_credentials: bool,
    },
    /// C2C_MESSAGE_CREATE. `content` is the raw text (only matched against a
    /// pairing code, never logged above debug).
    C2c {
        openid: String,
        msg_id: String,
        content: String,
        at: u64,
    },
    FriendAdd {
        openid: String,
        event_id: Option<String>,
        at: u64,
    },
    /// C2C_MSG_RECEIVE (true) / C2C_MSG_REJECT (false): the admin toggled
    /// active messages.
    ActiveMessages {
        /// Whose toggle it is; only the admin's counts.
        openid: Option<String>,
        accepted: bool,
        at: u64,
    },
}

impl std::fmt::Debug for GatewayEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        const REDACTED: &str = "[redacted]";
        match self {
            Self::Ready => f.write_str("Ready"),
            Self::Closed {
                reason,
                fatal,
                bad_credentials,
            } => f
                .debug_struct("Closed")
                .field("reason", reason)
                .field("fatal", fatal)
                .field("bad_credentials", bad_credentials)
                .finish(),
            Self::C2c {
                openid: _,
                msg_id,
                content: _,
                at,
            } => f
                .debug_struct("C2c")
                .field("openid", &REDACTED)
                .field("msg_id", msg_id)
                .field("content", &REDACTED)
                .field("at", at)
                .finish(),
            Self::FriendAdd {
                openid: _,
                event_id,
                at,
            } => f
                .debug_struct("FriendAdd")
                .field("openid", &REDACTED)
                .field("event_id", event_id)
                .field("at", at)
                .finish(),
            Self::ActiveMessages {
                openid,
                accepted,
                at,
            } => f
                .debug_struct("ActiveMessages")
                .field("openid", &openid.as_ref().map(|_| REDACTED))
                .field("accepted", accepted)
                .field("at", at)
                .finish(),
        }
    }
}

fn text(value: &serde_json::Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(|v| v.as_str())
        .filter(|v| !v.is_empty())
        .map(str::to_string)
}

/// Parse one dispatch (`op` 0) payload. Pure, for tests.
pub fn parse_dispatch(kind: &str, data: &serde_json::Value, now: u64) -> Option<GatewayEvent> {
    match kind {
        "READY" | "RESUMED" => Some(GatewayEvent::Ready),
        "C2C_MESSAGE_CREATE" => {
            let openid = data
                .get("author")
                .and_then(|author| text(author, "user_openid"))?;
            Some(GatewayEvent::C2c {
                openid,
                msg_id: text(data, "id")?,
                content: data
                    .get("content")
                    .and_then(|c| c.as_str())
                    .unwrap_or_default()
                    .chars()
                    .take(200)
                    .collect(),
                at: now,
            })
        }
        "FRIEND_ADD" => Some(GatewayEvent::FriendAdd {
            openid: text(data, "openid")?,
            event_id: None,
            at: now,
        }),
        "C2C_MSG_RECEIVE" => Some(GatewayEvent::ActiveMessages {
            openid: text(data, "openid"),
            accepted: true,
            at: now,
        }),
        "C2C_MSG_REJECT" => Some(GatewayEvent::ActiveMessages {
            openid: text(data, "openid"),
            accepted: false,
            at: now,
        }),
        _ => None,
    }
}

/// Parse a raw gateway frame into `(op, seq, event)` plus the hello interval.
pub struct Frame {
    pub op: i64,
    pub seq: Option<u64>,
    pub event: Option<GatewayEvent>,
    pub heartbeat_ms: Option<u64>,
    pub session_id: Option<String>,
    /// Top-level dispatch `id`, for de-duplicating replayed events.
    pub dispatch_id: Option<String>,
}

pub fn parse_frame(raw: &str, now: u64) -> Option<Frame> {
    let value: serde_json::Value = serde_json::from_str(raw).ok()?;
    let op = value.get("op")?.as_i64()?;
    let seq = value.get("s").and_then(|s| s.as_u64());
    let data = value.get("d").cloned().unwrap_or_default();
    let kind = value.get("t").and_then(|t| t.as_str()).unwrap_or_default();
    let mut event = (op == 0)
        .then(|| parse_dispatch(kind, &data, now))
        .flatten();
    // FRIEND_ADD replies need the dispatch id.
    if let Some(GatewayEvent::FriendAdd { event_id, .. }) = event.as_mut() {
        *event_id = text(&value, "id");
    }
    Some(Frame {
        op,
        seq,
        event,
        heartbeat_ms: (op == 10)
            .then(|| data.get("heartbeat_interval").and_then(|h| h.as_u64()))
            .flatten(),
        session_id: (kind == "READY")
            .then(|| text(&data, "session_id"))
            .flatten(),
        dispatch_id: (op == 0).then(|| text(&value, "id")).flatten(),
    })
}

pub struct GatewayParams {
    pub http: reqwest::Client,
    pub tokens: Arc<TokenCache>,
    pub api_base: String,
    pub token_url: String,
    pub app_id: String,
    pub secret: crate::notify::store::AppSecret,
    /// Hello after connect, and READY/RESUMED after identify, must arrive
    /// within this long.
    pub handshake_timeout: Duration,
    /// First reconnect delay; doubles up to 5 minutes.
    pub initial_backoff: Duration,
    /// Cap on the extra wait after a throttled token fetch (100001);
    /// `TOKEN_THROTTLE_BACKOFF` in production, short in tests.
    pub throttle_backoff: Duration,
}

impl GatewayParams {
    pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);
    pub const INITIAL_BACKOFF: Duration = Duration::from_secs(2);
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// What to do after a session ends (QQ gateway close codes, research §6).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Next {
    /// Network error, 4009, op 7: resume the session if we have one.
    Resume,
    /// 4006/4007/4008/4900–4913, op 9: the session is dead, identify anew.
    Reidentify,
    /// 4004 or an OpenAPI authentication refusal: fetch a new token and identify.
    ReauthReidentify,
    /// 4013/4014 (no C2C intent), 4914 (bot offline), 4915 (banned):
    /// reconnecting cannot help. Stop and show the reason.
    Fatal,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ended {
    pub next: Next,
    pub reason: String,
    pub bad_credentials: bool,
    /// READY/RESUMED was seen in this session.
    pub ready: bool,
    /// Only a WebSocket 4004 counts toward the bad-credentials latch. An
    /// HTTP token refusal during discovery is retried with backoff.
    gateway_auth_rejected: bool,
    /// Wait at least this long before the next attempt (token 100001).
    pub min_backoff: Option<Duration>,
}

impl Ended {
    fn new(next: Next, reason: impl Into<String>) -> Self {
        Self {
            next,
            reason: reason.into(),
            bad_credentials: false,
            ready: false,
            gateway_auth_rejected: false,
            min_backoff: None,
        }
    }

    fn bad_credentials(reason: impl Into<String>) -> Self {
        Self {
            bad_credentials: true,
            ..Self::new(Next::Fatal, reason)
        }
    }

    fn after_ready(mut self, ready: bool) -> Self {
        self.ready = ready;
        self
    }
}

/// Token fetches answered 100001 (too frequent) wait at least this long.
pub const TOKEN_THROTTLE_BACKOFF: Duration = Duration::from_secs(60);

fn is_bad_credential(code: Option<i64>) -> bool {
    code.is_some_and(|code| super::BAD_CREDENTIAL_CODES.contains(&code))
}

/// What a failed token (or gateway-URL) request means for the session.
fn token_failure(http_status: u16, code: Option<i64>, reason: String) -> Ended {
    if is_bad_credential(code) {
        Ended::bad_credentials(reason)
    } else if code == Some(100001) {
        Ended {
            min_backoff: Some(TOKEN_THROTTLE_BACKOFF),
            ..Ended::new(Next::Resume, reason)
        }
    } else if super::errors::classify(http_status, code) == super::errors::ErrorClass::NeedsReauth {
        Ended::new(Next::ReauthReidentify, reason)
    } else {
        Ended::new(Next::Resume, reason)
    }
}

/// Two 4004 closes in a row before READY, the second on a freshly fetched
/// token: the credentials do not work for the gateway.
pub const AUTH_REJECTIONS_FATAL: u8 = 2;

/// Map a close code to the next step.
pub fn classify_close(code: u16) -> Ended {
    match code {
        4004 => Ended {
            gateway_auth_rejected: true,
            ..Ended::new(Next::ReauthReidentify, "网关鉴权失败 (4004)")
        },
        4006..=4008 | 4900..=4913 => Ended::new(Next::Reidentify, format!("网关会话失效 ({code})")),
        4013 | 4014 => Ended::new(
            Next::Fatal,
            format!("机器人没有单聊消息事件权限 ({code})，无法自动获取 openid"),
        ),
        4914 => Ended::new(Next::Fatal, "机器人已下架，只能在沙箱使用 (4914)"),
        4915 => Ended::new(Next::Fatal, "机器人已被封禁 (4915)"),
        _ => Ended::new(Next::Resume, format!("网关关闭连接 ({code})")),
    }
}

/// Dispatch ids seen recently; a resume replays events.
const SEEN_IDS: usize = 64;

/// Runs until the task is aborted or a fatal close. Reconnects with backoff.
pub async fn run(params: GatewayParams, events: mpsc::Sender<GatewayEvent>) {
    let mut backoff = params.initial_backoff;
    let mut resume: Option<(String, u64)> = None;
    let mut seen: VecDeque<String> = VecDeque::with_capacity(SEEN_IDS);
    let mut auth_rejections = 0u8;
    loop {
        let started = std::time::Instant::now();
        let mut ended = session(&params, &events, &mut resume, &mut seen).await;
        if ended.gateway_auth_rejected && !ended.ready {
            // The token cache is cleared below, so the next session fetches a
            // fresh token: a second rejection is not a stale token.
            auth_rejections += 1;
            if auth_rejections >= AUTH_REJECTIONS_FATAL {
                ended =
                    Ended::bad_credentials("网关鉴权连续失败 (4004)，AppID 或 AppSecret 可能无效");
            }
        } else {
            auth_rejections = 0;
        }
        match ended.next {
            Next::Resume => {}
            Next::Reidentify => resume = None,
            Next::ReauthReidentify => {
                resume = None;
                params.tokens.clear().await;
            }
            Next::Fatal => resume = None,
        }
        let fatal = ended.next == Next::Fatal;
        let _ = events
            .send(GatewayEvent::Closed {
                reason: ended.reason.clone(),
                fatal,
                bad_credentials: ended.bad_credentials,
            })
            .await;
        tracing::debug!("QQ 网关会话结束：{}", ended.reason);
        if fatal {
            tracing::warn!("QQ 网关已停止：{}", ended.reason);
            return;
        }
        if started.elapsed() > Duration::from_secs(120) {
            backoff = params.initial_backoff;
        }
        if let Some(min) = ended.min_backoff {
            backoff = backoff.max(min.min(params.throttle_backoff));
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(MAX_BACKOFF);
    }
}

async fn token(params: &GatewayParams) -> Result<AccessToken, super::token::TokenError> {
    params
        .tokens
        .get(
            &params.http,
            &params.token_url,
            &params.app_id,
            params.secret.expose(),
        )
        .await
}

/// wss only; plain ws is allowed for loopback (tests).
fn allowed_url(url: &str) -> bool {
    let Ok(parsed) = reqwest::Url::parse(url) else {
        return false;
    };
    match parsed.scheme() {
        "wss" => true,
        "ws" => matches!(
            parsed.host_str(),
            Some("127.0.0.1") | Some("localhost") | Some("[::1]")
        ),
        _ => false,
    }
}

async fn session(
    params: &GatewayParams,
    events: &mpsc::Sender<GatewayEvent>,
    resume: &mut Option<(String, u64)>,
    seen: &mut VecDeque<String>,
) -> Ended {
    // The token's business code is the primary bad-credentials signal.
    let token = match token(params).await {
        Ok(token) => token,
        Err(error) => return token_failure(error.http_status, error.code, error.reason),
    };
    let url = match super::api::gateway_url(&params.http, &params.api_base, &token).await {
        Ok(url) => url,
        Err(result) => return token_failure(result.http_status, result.code, result.reason()),
    };
    if !allowed_url(&url) {
        return Ended::new(Next::Resume, "网关地址不是 wss");
    }
    let config = WebSocketConfig::default()
        .max_message_size(Some(MAX_FRAME_BYTES))
        .max_frame_size(Some(MAX_FRAME_BYTES));
    let socket = match tokio::time::timeout(
        params.handshake_timeout,
        tokio_tungstenite::connect_async_with_config(url.as_str(), Some(config), false),
    )
    .await
    {
        Ok(Ok((socket, _))) => socket,
        Ok(Err(_)) => return Ended::new(Next::Resume, "网关连接失败"),
        Err(_) => return Ended::new(Next::Resume, "网关连接超时"),
    };
    let (mut sink, mut stream) = socket.split();
    let mut heartbeat = tokio::time::interval(Duration::from_secs(3600));
    heartbeat.tick().await;
    let mut seq: Option<u64> = resume.as_ref().map(|(_, s)| *s);
    let mut awaiting_ack = false;
    let mut ready = false;
    // Hello, then READY/RESUMED, each within the handshake timeout.
    let mut handshake = Some(tokio::time::Instant::now() + params.handshake_timeout);
    loop {
        let deadline =
            handshake.unwrap_or_else(|| tokio::time::Instant::now() + Duration::from_secs(86_400));
        tokio::select! {
            frame = stream.next() => {
                let message = match frame {
                    Some(Ok(message)) => message,
                    Some(Err(_)) => return Ended::new(Next::Resume, "网关连接中断").after_ready(ready),
                    None => return Ended::new(Next::Resume, "网关连接关闭").after_ready(ready),
                };
                let raw = match message {
                    Message::Text(text) if text.len() <= MAX_FRAME_BYTES => text.to_string(),
                    Message::Close(frame) => {
                        let code = frame.map(|f| u16::from(f.code)).unwrap_or(0);
                        return classify_close(code).after_ready(ready);
                    }
                    _ => continue,
                };
                let Some(frame) = parse_frame(&raw, now_secs()) else { continue };
                if frame.seq.is_some() {
                    seq = frame.seq;
                    if let Some((_, s)) = resume.as_mut() {
                        *s = frame.seq.unwrap_or(*s);
                    }
                }
                match frame.op {
                    10 => {
                        let interval = frame.heartbeat_ms.unwrap_or(41_250).clamp(5_000, 120_000);
                        heartbeat = tokio::time::interval(Duration::from_millis(interval));
                        heartbeat.tick().await;
                        let auth = format!("QQBot {}", token.expose());
                        let payload = match resume.as_ref() {
                            Some((session_id, last)) => serde_json::json!({
                                "op": 6,
                                "d": {"token": auth, "session_id": session_id, "seq": last}
                            }),
                            None => serde_json::json!({
                                "op": 2,
                                "d": {"token": auth, "intents": INTENTS, "shard": [0, 1], "properties": {}}
                            }),
                        };
                        if sink.send(Message::Text(payload.to_string().into())).await.is_err() {
                            return Ended::new(Next::Resume, "网关鉴权发送失败");
                        }
                        handshake = Some(tokio::time::Instant::now() + params.handshake_timeout);
                    }
                    11 => awaiting_ack = false,
                    7 => return Ended::new(Next::Resume, "网关要求重连").after_ready(ready),
                    9 => return Ended::new(Next::Reidentify, "网关会话无效").after_ready(ready),
                    0 => {
                        if let Some(session_id) = frame.session_id {
                            *resume = Some((session_id, seq.unwrap_or(0)));
                        }
                        if frame.event == Some(GatewayEvent::Ready) {
                            handshake = None;
                            ready = true;
                        }
                        if let Some(id) = frame.dispatch_id {
                            if seen.contains(&id) {
                                tracing::debug!("QQ 网关重复事件已忽略");
                                continue;
                            }
                            if seen.len() >= SEEN_IDS {
                                seen.pop_front();
                            }
                            seen.push_back(id);
                        }
                        if let Some(event) = frame.event {
                            // A full channel means the worker is busy: the event
                            // is lost, so it must not be lost silently. The
                            // event may be a pairing code or an admin command.
                            if events.try_send(event).is_err() {
                                let dropped = super::super::note_gateway_drop();
                                tracing::warn!(
                                    "QQ 网关事件处理不及，已丢弃一个事件（累计 {dropped}）"
                                );
                            }
                        }
                    }
                    _ => {}
                }
            }
            _ = heartbeat.tick() => {
                if awaiting_ack {
                    return Ended::new(Next::Resume, "网关心跳无响应").after_ready(ready);
                }
                awaiting_ack = true;
                let payload = serde_json::json!({"op": 1, "d": seq});
                if sink.send(Message::Text(payload.to_string().into())).await.is_err() {
                    return Ended::new(Next::Resume, "网关心跳发送失败").after_ready(ready);
                }
            }
            _ = tokio::time::sleep_until(deadline), if handshake.is_some() => {
                // No Hello / READY: start clean next time.
                return Ended::new(Next::Reidentify, "网关握手超时");
            }
        }
    }
}

#[cfg(test)]
#[path = "gateway_tests.rs"]
mod session_tests;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gateway_event_debug_never_shows_openid_or_content() {
        const OPENID: &str = "SYNTHETICOPENID0000000000000000AA";
        for event in [
            GatewayEvent::C2c {
                openid: OPENID.into(),
                msg_id: "synthetic-msg".into(),
                content: "synthetic-pairing-731942".into(),
                at: 1,
            },
            GatewayEvent::FriendAdd {
                openid: OPENID.into(),
                event_id: None,
                at: 1,
            },
            GatewayEvent::ActiveMessages {
                openid: Some(OPENID.into()),
                accepted: true,
                at: 1,
            },
        ] {
            for text in [format!("{event:?}"), format!("{event:#?}")] {
                assert!(!text.contains(OPENID), "{text}");
                assert!(!text.contains("synthetic-pairing-731942"), "{text}");
                assert!(text.contains("[redacted]"), "{text}");
            }
        }
    }

    #[test]
    fn parses_c2c_message_with_openid_and_msg_id() {
        let raw = r#"{"op":0,"s":7,"t":"C2C_MESSAGE_CREATE","id":"C2C_MESSAGE_CREATE:abc","d":{"id":"ROBOT1.0_msg","content":" 482913 ","timestamp":"2026-10-11T15:00:00+08:00","author":{"user_openid":"E4F0A1B2C3D4E5F60718293A4B5C6D7E"}}}"#;
        let frame = parse_frame(raw, 100).unwrap();
        assert_eq!(frame.op, 0);
        assert_eq!(frame.seq, Some(7));
        assert_eq!(
            frame.event,
            Some(GatewayEvent::C2c {
                openid: "E4F0A1B2C3D4E5F60718293A4B5C6D7E".into(),
                msg_id: "ROBOT1.0_msg".into(),
                content: " 482913 ".into(),
                at: 100,
            })
        );
    }

    #[test]
    fn parses_friend_add_hello_ready_and_toggles() {
        let raw = r#"{"op":0,"s":2,"t":"FRIEND_ADD","id":"FRIEND_ADD:xyz","d":{"timestamp":1760000000,"openid":"ABCDEF0123456789"}}"#;
        assert_eq!(
            parse_frame(raw, 5).unwrap().event,
            Some(GatewayEvent::FriendAdd {
                openid: "ABCDEF0123456789".into(),
                event_id: Some("FRIEND_ADD:xyz".into()),
                at: 5,
            })
        );
        let hello = parse_frame(r#"{"op":10,"d":{"heartbeat_interval":41250}}"#, 0).unwrap();
        assert_eq!(hello.heartbeat_ms, Some(41250));
        let ready = parse_frame(
            r#"{"op":0,"s":1,"t":"READY","d":{"session_id":"s-1","version":1}}"#,
            0,
        )
        .unwrap();
        assert_eq!(ready.session_id.as_deref(), Some("s-1"));
        assert_eq!(ready.event, Some(GatewayEvent::Ready));
        let reject = parse_frame(r#"{"op":0,"t":"C2C_MSG_REJECT","d":{"openid":"A"}}"#, 9).unwrap();
        assert_eq!(
            reject.event,
            Some(GatewayEvent::ActiveMessages {
                openid: Some("A".into()),
                accepted: false,
                at: 9
            })
        );
    }

    #[test]
    fn close_codes_pick_the_next_step() {
        assert_eq!(classify_close(4009).next, Next::Resume);
        assert_eq!(classify_close(1006).next, Next::Resume);
        for code in [4006, 4007, 4008, 4900, 4913] {
            assert_eq!(classify_close(code).next, Next::Reidentify, "{code}");
        }
        assert_eq!(classify_close(4004).next, Next::ReauthReidentify);
        for code in [4013, 4014, 4914, 4915] {
            assert_eq!(classify_close(code).next, Next::Fatal, "{code}");
        }
        assert!(allowed_url("wss://api.sgroup.qq.com/websocket"));
        assert!(allowed_url("ws://127.0.0.1:9/ws"));
        assert!(!allowed_url("ws://evil.example/ws"));
        assert!(!allowed_url("http://127.0.0.1/ws"));
    }

    #[test]
    fn author_id_is_not_an_openid() {
        let raw = r#"{"op":0,"t":"C2C_MESSAGE_CREATE","d":{"id":"m","content":"x","author":{"id":"U1"}}}"#;
        assert!(parse_frame(raw, 0).unwrap().event.is_none());
    }

    #[test]
    fn ignores_group_events_and_garbage() {
        assert!(parse_frame("not json", 0).is_none());
        let group = parse_frame(r#"{"op":0,"t":"GROUP_AT_MESSAGE_CREATE","d":{}}"#, 0).unwrap();
        assert!(group.event.is_none());
        let missing =
            parse_frame(r#"{"op":0,"t":"C2C_MESSAGE_CREATE","d":{"id":"m"}}"#, 0).unwrap();
        assert!(missing.event.is_none());
    }
}
