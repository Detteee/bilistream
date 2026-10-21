//! QQ C2C sink: token cache, delivery-mode ladder, gateway (design §5–§7).

pub mod api;
pub mod errors;
pub mod gateway;
pub mod token;

use super::sink::{DeliveryContext, Rendered, SendOutcome, Sink};
use super::store::AppSecret;
use api::{ApiResult, C2cMessage, QqMode};
use errors::ErrorClass;
use std::sync::Arc;
use token::{TokenCache, TokenError};

/// Credentials are rejected for good until the settings change.
pub const BAD_CREDENTIAL_CODES: [i64; 3] = [100016, 100007, 10004];

#[derive(Clone)]
pub struct QqTarget {
    pub app_id: String,
    pub secret: AppSecret,
    pub openid: String,
    pub api_base: String,
    pub token_url: String,
}

impl std::fmt::Debug for QqTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QqTarget")
            .field("app_id_set", &!self.app_id.is_empty())
            .field("secret", &self.secret)
            .field("api_base", &self.api_base)
            .finish_non_exhaustive()
    }
}

pub struct QqSink {
    pub http: reqwest::Client,
    pub tokens: Arc<TokenCache>,
    pub target: QqTarget,
}

enum Attempt {
    /// The outcome, and whether the last request reached QQ (an HTTP status
    /// came back). Quota is charged only when it did.
    Done(SendOutcome, bool),
    /// This mode is closed; try the next one.
    NextMode(ApiResult),
}

fn token_outcome(error: TokenError) -> SendOutcome {
    match error.class {
        ErrorClass::Transient | ErrorClass::RateLimited => SendOutcome::Retry {
            reason: error.reason,
            code: error.code,
            trace_id: None,
        },
        _ => SendOutcome::Undeliverable {
            bad_credentials: error
                .code
                .is_some_and(|code| BAD_CREDENTIAL_CODES.contains(&code)),
            reason: error.reason,
            code: error.code,
            trace_id: None,
        },
    }
}

impl QqSink {
    pub async fn token(&self) -> Result<token::AccessToken, TokenError> {
        self.tokens
            .get(
                &self.http,
                &self.target.token_url,
                &self.target.app_id,
                self.target.secret.expose(),
            )
            .await
    }

    /// One mode, with reauth-once, markdown downgrade and minimal re-render.
    async fn attempt(
        &self,
        message: &Rendered,
        mode: QqMode,
        msg_id: Option<&str>,
        msg_seq: u32,
        cta: Option<&str>,
        ctx: &mut DeliveryContext<'_>,
    ) -> Attempt {
        let mut reauthed = false;
        let mut minimal = false;
        let mut markdown = message.markdown.is_some() && !ctx.ledger.modes.markdown_unsupported();
        loop {
            let token = match self.token().await {
                Ok(token) => token,
                Err(error) => return Attempt::Done(token_outcome(error), false),
            };
            let content = if minimal {
                message.minimal.clone()
            } else if markdown {
                let body = message.markdown.as_deref().unwrap_or(&message.text);
                crate::notify::format::with_cta(body, cta, true)
            } else {
                crate::notify::format::with_cta(&message.text, cta, false)
            };
            let content = content.as_str();
            let result = api::send_c2c(
                &self.http,
                &self.target.api_base,
                &token,
                &C2cMessage {
                    openid: &self.target.openid,
                    content,
                    markdown: markdown && !minimal,
                    mode,
                    msg_id,
                    event_id: None,
                    msg_seq,
                },
                &message.attempt,
                ctx.now,
            )
            .await;
            tracing::debug!(
                "QQ 通知发送：mode={} ok={} http={} code={:?} trace={:?}",
                mode.as_str(),
                result.ok,
                result.http_status,
                result.code,
                result.trace_id
            );
            let reached = result.http_status != 0;
            if result.ok {
                return Attempt::Done(
                    SendOutcome::Delivered {
                        mode: mode.as_str(),
                        trace_id: result.trace_id,
                    },
                    true,
                );
            }
            match result.class() {
                ErrorClass::NeedsReauth if !reauthed => {
                    reauthed = true;
                    self.tokens.invalidate(&token).await;
                }
                ErrorClass::NeedsReauth | ErrorClass::Transient => {
                    return Attempt::Done(
                        SendOutcome::Retry {
                            reason: result.reason(),
                            code: result.code,
                            trace_id: result.trace_id,
                        },
                        reached,
                    )
                }
                ErrorClass::RateLimited => {
                    ctx.ledger.record_rate_limited(ctx.now);
                    return Attempt::Done(
                        SendOutcome::Retry {
                            reason: result.reason(),
                            code: result.code,
                            trace_id: result.trace_id,
                        },
                        reached,
                    );
                }
                ErrorClass::Markdown if markdown => {
                    ctx.ledger.modes.markdown_unsupported_at = ctx.now.max(1);
                    markdown = false;
                }
                ErrorClass::ContentRejected if !minimal => minimal = true,
                ErrorClass::ModeUnavailable => return Attempt::NextMode(result),
                _ => {
                    if result.code == Some(40054013) {
                        ctx.ledger.modes.admin_rejected_at = ctx.now.max(1);
                    }
                    if result
                        .code
                        .is_some_and(|code| BAD_CREDENTIAL_CODES.contains(&code))
                    {
                        self.tokens.clear().await;
                    }
                    return Attempt::Done(
                        SendOutcome::Undeliverable {
                            bad_credentials: result
                                .code
                                .is_some_and(|code| BAD_CREDENTIAL_CODES.contains(&code)),
                            reason: result.reason(),
                            code: result.code,
                            trace_id: result.trace_id,
                        },
                        reached,
                    );
                }
            }
        }
    }
}

#[async_trait::async_trait]
impl Sink for QqSink {
    /// Addendum E rule 1 / rule 18: active first → passive window → wakeup
    /// (critical only) → undeliverable. Only 40034105 (no permission, or the
    /// sticky "active unavailable" fact), 40034100 (rate limited) or the
    /// other "mode unavailable" codes move on to passive; any other active
    /// error keeps the existing retry backoff and spends no passive or
    /// wakeup budget.
    async fn send(&self, message: &Rendered, ctx: &mut DeliveryContext<'_>) -> SendOutcome {
        let now = ctx.now;
        let mut last: Option<ApiResult> = None;
        let mut reasons: Vec<String> = Vec::new();
        let mut active_budget_out = false;
        let mut rate_limited: Option<String> = None;

        if ctx.ledger.modes.admin_rejected() {
            reasons.push("管理员已关闭「允许主动发送」".to_string());
        } else if ctx.ledger.modes.active_unavailable(now) {
            reasons.push("主动消息无权限".to_string());
        } else if !ctx.ledger.active_budget_left(now) {
            active_budget_out = true;
        } else {
            // Charged per attempt: a timed-out send may still have landed.
            ctx.ledger.record_active(now);
            match self
                .attempt(message, QqMode::Active, None, 0, None, ctx)
                .await
            {
                Attempt::Done(
                    SendOutcome::Retry {
                        reason,
                        code: Some(40034100),
                        ..
                    },
                    _,
                ) => {
                    // Active rate limit: a fallback trigger (rule 2).
                    reasons.push(reason.clone());
                    rate_limited = Some(reason);
                }
                Attempt::Done(outcome, _) => return outcome,
                Attempt::NextMode(result) => {
                    if result.code == Some(40034105) {
                        ctx.ledger.modes.active_unavailable_at = now.max(1);
                    }
                    reasons.push(result.reason());
                    last = Some(result);
                }
            }
        }

        let passive = ctx
            .passive
            .as_deref()
            .filter(|target| target.usable(now))
            .map(|target| (target.msg_id.clone(), target.replies_used + 1));
        if let Some((msg_id, seq)) = passive {
            let cta = (seq == super::sink::PASSIVE_MAX_REPLIES).then(super::format::last_reply_cta);
            match self
                .attempt(
                    message,
                    QqMode::Passive,
                    Some(&msg_id),
                    seq,
                    cta.as_deref(),
                    ctx,
                )
                .await
            {
                Attempt::Done(outcome, reached) => {
                    // A request that never reached QQ did not use the slot.
                    if reached {
                        if let Some(target) = ctx.passive.as_deref_mut() {
                            target.replies_used = seq;
                        }
                    }
                    return outcome;
                }
                Attempt::NextMode(result) => {
                    if let Some(target) = ctx.passive.as_deref_mut() {
                        target.replies_used = super::sink::PASSIVE_MAX_REPLIES;
                    }
                    last = Some(result);
                }
            }
        }

        if active_budget_out {
            // The shared active budget refills; no passive slot was open.
            return SendOutcome::Retry {
                reason: "共享发送额度暂时用完".to_string(),
                code: None,
                trace_id: None,
            };
        }

        if message.critical && ctx.ledger.wakeup_available(now) {
            let cta = super::format::wakeup_cta(ctx.ledger, now);
            match self
                .attempt(message, QqMode::Wakeup, None, 0, Some(&cta), ctx)
                .await
            {
                Attempt::Done(outcome, reached) => {
                    // Spend the recall only when QQ saw the request; a
                    // network error before that keeps it for the retry.
                    if reached {
                        ctx.ledger.record_wakeup(now);
                    }
                    return outcome;
                }
                Attempt::NextMode(result) => {
                    ctx.ledger.record_wakeup(now);
                    reasons.push(result.reason());
                    last = Some(result);
                }
            }
        } else if message.critical {
            reasons.push("召回额度已用完".to_string());
        }

        if let Some(reason) = rate_limited {
            // Nothing else was open: the active limit lifts, so retry with
            // the existing backoff instead of giving the alert up.
            return SendOutcome::Retry {
                reason,
                code: Some(40034100),
                trace_id: last.and_then(|result| result.trace_id),
            };
        }
        reasons.dedup();
        SendOutcome::Undeliverable {
            reason: if reasons.is_empty() {
                "没有可用的送达方式".to_string()
            } else {
                reasons.join("，")
            },
            code: last.as_ref().and_then(|result| result.code),
            trace_id: last.and_then(|result| result.trace_id),
            bad_credentials: false,
        }
    }
}

/// 开播/下播 requests (Addendum E): one plain request per channel, with
/// one re-auth. The ladder itself lives in `live_events`.
#[async_trait::async_trait]
impl super::live_events::LiveTransport for QqSink {
    async fn send(&self, request: super::live_events::LiveSend<'_>) -> ApiResult {
        let unsent = ApiResult {
            ok: false,
            http_status: 0,
            code: None,
            trace_id: None,
        };
        let mut reauthed = false;
        loop {
            let token = match self.token().await {
                Ok(token) => token,
                Err(_) => return unsent,
            };
            let message = C2cMessage {
                openid: &self.target.openid,
                content: request.text,
                markdown: false,
                mode: request.mode,
                msg_id: request.msg_id,
                event_id: None,
                msg_seq: request.msg_seq,
            };
            let call = api::send_c2c(
                &self.http,
                &self.target.api_base,
                &token,
                &message,
                &request.attempt,
                request.at,
            );
            // A timeout is an ambiguous result like a network error.
            let result = match tokio::time::timeout(std::time::Duration::from_secs(20), call).await
            {
                Ok(result) => result,
                Err(_) => unsent.clone(),
            };
            if !result.ok && result.class() == ErrorClass::NeedsReauth && !reauthed {
                reauthed = true;
                self.tokens.invalidate(&token).await;
                continue;
            }
            return result;
        }
    }
}

#[cfg(test)]
mod tests;
