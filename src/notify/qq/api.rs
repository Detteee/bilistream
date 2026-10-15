//! QQ OpenAPI v2 request bodies and response parsing (research §3).

use super::errors::{classify, describe, ErrorClass};
use super::token::AccessToken;
use crate::notify::sink::AttemptContext;
use std::time::Duration;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QqMode {
    /// Reply to the admin's latest message (`msg_id` + `msg_seq`).
    Passive,
    /// Plain active message.
    Active,
    /// Interaction recall (`is_wakeup`).
    Wakeup,
}

impl QqMode {
    pub fn as_str(self) -> &'static str {
        match self {
            QqMode::Passive => "passive",
            QqMode::Active => "active",
            QqMode::Wakeup => "wakeup",
        }
    }
}

pub struct C2cMessage<'a> {
    pub openid: &'a str,
    pub content: &'a str,
    pub markdown: bool,
    pub mode: QqMode,
    pub msg_id: Option<&'a str>,
    /// Passive reply to an event (FRIEND_ADD) instead of a message.
    pub event_id: Option<&'a str>,
    pub msg_seq: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApiResult {
    pub ok: bool,
    /// 0 for a network error or timeout.
    pub http_status: u16,
    pub code: Option<i64>,
    pub trace_id: Option<String>,
}

impl ApiResult {
    pub fn class(&self) -> ErrorClass {
        classify(self.http_status, self.code)
    }
    pub fn reason(&self) -> String {
        describe(self.code, self.http_status)
    }
}

pub fn c2c_body(message: &C2cMessage<'_>) -> serde_json::Value {
    let mut body = if message.markdown {
        serde_json::json!({"msg_type": 2, "markdown": {"content": message.content}})
    } else {
        serde_json::json!({"msg_type": 0, "content": message.content})
    };
    match message.mode {
        QqMode::Passive => {
            match (message.msg_id, message.event_id) {
                (Some(msg_id), _) => body["msg_id"] = msg_id.into(),
                (None, Some(event_id)) => body["event_id"] = event_id.into(),
                (None, None) => {}
            }
            body["msg_seq"] = message.msg_seq.max(1).into();
        }
        QqMode::Wakeup => body["is_wakeup"] = true.into(),
        QqMode::Active => {}
    }
    body
}

/// Path segments come from config; keep them to the openid alphabet.
fn safe_segment(value: &str) -> String {
    value
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
        .collect()
}

pub async fn send_c2c(
    http: &reqwest::Client,
    api_base: &str,
    token: &AccessToken,
    message: &C2cMessage<'_>,
    context: &AttemptContext,
    at: u64,
) -> ApiResult {
    let url = format!(
        "{}/v2/users/{}/messages",
        api_base.trim_end_matches('/'),
        safe_segment(message.openid)
    );
    // Creating/polling the HTTP request is the attempt boundary. A cancelled
    // future still records an unknown result; token/quota preflights never
    // create this guard and cannot inflate message-attempt totals.
    let mut attempt = AttemptRecord::new(context, message.mode.as_str(), at);
    let response = http
        .post(url)
        .timeout(REQUEST_TIMEOUT)
        .header("Authorization", format!("QQBot {}", token.expose()))
        .json(&c2c_body(message))
        .send()
        .await;
    let response = match response {
        Ok(response) => response,
        Err(_) => {
            return ApiResult {
                ok: false,
                http_status: 0,
                code: None,
                trace_id: None,
            }
        }
    };
    let status = response.status().as_u16();
    let clean_trace = |raw: &str| {
        let mut clean = raw.to_string();
        for sensitive in [token.expose(), message.openid] {
            if !sensitive.is_empty() {
                clean = clean.replace(sensitive, "<set>");
            }
        }
        crate::logging::safe_text(&clean, 80)
    };
    let header_trace = response
        .headers()
        .get("x-tps-trace-id")
        .and_then(|value| value.to_str().ok())
        .map(clean_trace);
    let body: serde_json::Value = read_json(response).await;
    let code = body
        .get("err_code")
        .or_else(|| body.get("code"))
        .and_then(|value| value.as_i64())
        .filter(|code| *code != 0);
    let trace_id = header_trace.or_else(|| {
        body.get("trace_id")
            .and_then(|value| value.as_str())
            .map(clean_trace)
    });
    // 201/202 are "async success" but may carry an error body instead of
    // the message id.
    let ok = matches!(status, 200..=202) && code.is_none() && body.get("id").is_some();
    let result = ApiResult {
        ok,
        http_status: status,
        code,
        trace_id,
    };
    attempt.complete(&result);
    result
}

struct AttemptRecord<'a> {
    context: &'a AttemptContext,
    mode: &'static str,
    at: u64,
    result: Option<ApiResult>,
}

impl<'a> AttemptRecord<'a> {
    fn new(context: &'a AttemptContext, mode: &'static str, at: u64) -> Self {
        Self {
            context,
            mode,
            at,
            result: None,
        }
    }

    fn complete(&mut self, result: &ApiResult) {
        self.result = Some(result.clone());
    }
}

impl Drop for AttemptRecord<'_> {
    fn drop(&mut self) {
        let result = self.result.as_ref();
        crate::events::emit_at(
            self.at,
            self.context.event(
                self.mode,
                result.is_some_and(|result| result.ok),
                result.and_then(|result| result.code),
                result.and_then(|result| result.trace_id.clone()),
            ),
        );
    }
}

/// `GET {api_base}/gateway` → the WebSocket URL.
pub async fn gateway_url(
    http: &reqwest::Client,
    api_base: &str,
    token: &AccessToken,
) -> Result<String, ApiResult> {
    let response = http
        .get(format!("{}/gateway", api_base.trim_end_matches('/')))
        .timeout(REQUEST_TIMEOUT)
        .header("Authorization", format!("QQBot {}", token.expose()))
        .send()
        .await
        .map_err(|_| ApiResult {
            ok: false,
            http_status: 0,
            code: None,
            trace_id: None,
        })?;
    let status = response.status().as_u16();
    let body: serde_json::Value = read_json(response).await;
    match body.get("url").and_then(|url| url.as_str()) {
        Some(url) if status == 200 && (url.starts_with("wss://") || url.starts_with("ws://")) => {
            Ok(url.to_string())
        }
        _ => Err(ApiResult {
            ok: false,
            http_status: status,
            code: body
                .get("code")
                .or_else(|| body.get("err_code"))
                .and_then(|code| code.as_i64()),
            trace_id: None,
        }),
    }
}

/// QQ bodies are small; cap them (64 KiB) so a broken proxy cannot fill memory.
pub(crate) async fn read_json<T: serde::de::DeserializeOwned + Default>(
    response: reqwest::Response,
) -> T {
    crate::plugins::http::response_bytes_limited(response, 64 * 1024)
        .await
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}
