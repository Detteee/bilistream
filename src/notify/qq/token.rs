//! Per-node access-token cache (design §6, review §9).
//!
//! QQ issues one token per app: fetching again within its lifetime returns
//! the same token with its remaining `expires_in`, and a renewal in the last
//! 60 s leaves the old one valid for those 60 s. So nodes never invalidate
//! each other, and tokens are never sent between nodes.

use super::errors::{classify, describe, ErrorClass};
use serde::Deserialize;
use std::time::{Duration, Instant};
use zeroize::Zeroizing;

/// Refresh this long before expiry. Must stay below QQ's 60 s renewal band,
/// or every fetch inside the band returns an already "stale" token.
const REFRESH_MARGIN: Duration = Duration::from_secs(30);
/// Never call getAppAccessToken more often than this.
const MIN_FETCH_GAP: Duration = Duration::from_secs(10);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone)]
pub struct AccessToken(Zeroizing<String>);

impl AccessToken {
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for AccessToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("[redacted]")
    }
}

struct Cached {
    token: AccessToken,
    refresh_at: Instant,
    expires_at: Instant,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenError {
    pub class: ErrorClass,
    pub code: Option<i64>,
    pub http_status: u16,
    pub reason: String,
}

#[derive(Default)]
pub struct TokenCache {
    inner: tokio::sync::Mutex<Option<Cached>>,
    last_fetch: std::sync::Mutex<Option<Instant>>,
    #[cfg(test)]
    pub(crate) fetches: std::sync::atomic::AtomicUsize,
}

#[derive(Deserialize)]
struct TokenReply {
    #[serde(default)]
    access_token: Option<String>,
    #[serde(default, deserialize_with = "number_or_string")]
    expires_in: Option<u64>,
    #[serde(default)]
    code: Option<i64>,
}

fn number_or_string<'de, D: serde::Deserializer<'de>>(de: D) -> Result<Option<u64>, D::Error> {
    let value = Option::<serde_json::Value>::deserialize(de)?;
    Ok(match value {
        Some(serde_json::Value::Number(n)) => n.as_u64(),
        Some(serde_json::Value::String(s)) => s.trim().parse().ok(),
        _ => None,
    })
}

impl TokenCache {
    /// A valid token, fetched at most once at a time (the async mutex is held
    /// across the fetch: single flight).
    pub async fn get(
        &self,
        http: &reqwest::Client,
        token_url: &str,
        app_id: &str,
        secret: &str,
    ) -> Result<AccessToken, TokenError> {
        self.get_with_clock(http, token_url, app_id, secret, Instant::now)
            .await
    }

    async fn get_with_clock(
        &self,
        http: &reqwest::Client,
        token_url: &str,
        app_id: &str,
        secret: &str,
        clock: impl Fn() -> Instant,
    ) -> Result<AccessToken, TokenError> {
        let mut guard = self.inner.lock().await;
        let now = clock();
        if let Some(cached) = guard.as_ref() {
            if now < cached.refresh_at {
                return Ok(cached.token.clone());
            }
            let recently_fetched = self
                .last_fetch
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .is_some_and(|at| now.duration_since(at) < MIN_FETCH_GAP);
            if recently_fetched {
                // `MIN_FETCH_GAP` is a fetch gate, not just a return gate: a
                // fetch inside the gap is throttled by QQ (100001) and costs a
                // full gateway backoff. Keep serving the token we already have;
                // once it is expired report a retryable failure instead of
                // hammering the endpoint.
                if now < cached.expires_at {
                    return Ok(cached.token.clone());
                }
                return Err(TokenError {
                    class: ErrorClass::Transient,
                    code: None,
                    http_status: 0,
                    reason: "Token 刷新过于频繁".to_string(),
                });
            }
        }
        if !crate::notify::config::endpoint_allowed(token_url) {
            // Never post the AppSecret to an unparsed or plain-http host.
            return Err(TokenError {
                class: ErrorClass::Undeliverable,
                code: None,
                http_status: 0,
                reason: "Token 地址无效".to_string(),
            });
        }
        *self.last_fetch.lock().unwrap_or_else(|e| e.into_inner()) = Some(now);
        #[cfg(test)]
        self.fetches
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let reply = http
            .post(token_url)
            .timeout(REQUEST_TIMEOUT)
            .json(&serde_json::json!({"appId": app_id, "clientSecret": secret}))
            .send()
            .await
            .map_err(|_| TokenError {
                class: ErrorClass::Transient,
                code: None,
                http_status: 0,
                reason: describe(None, 0),
            })?;
        let status = reply.status().as_u16();
        let bytes = crate::plugins::http::response_bytes_limited(reply, 64 * 1024).await;
        let body: TokenReply = bytes
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .ok_or_else(|| TokenError {
                class: classify(status, None),
                code: None,
                http_status: status,
                reason: describe(None, status),
            })?;
        let token = match (body.code, body.access_token) {
            // Errors arrive with HTTP 200 and a body code: never trust status.
            (Some(code), _) if code != 0 => None,
            (_, Some(token)) if !token.is_empty() && status == 200 => Some(token),
            _ => None,
        };
        let Some(token) = token else {
            let class = match body.code {
                // A token fetch can't be fixed by refetching it.
                Some(code) => match classify(status, Some(code)) {
                    ErrorClass::NeedsReauth => ErrorClass::Undeliverable,
                    class => class,
                },
                None => classify(status, None),
            };
            return Err(TokenError {
                class,
                code: body.code,
                http_status: status,
                reason: describe(body.code, status),
            });
        };
        let lifetime = Duration::from_secs(body.expires_in.unwrap_or(7200).clamp(60, 7200));
        let mut expires_at = now + lifetime;
        if let Some(previous) = guard.as_ref() {
            // Same token back: keep the earlier expiry in case QQ reported a
            // full lifetime for a token it issued earlier.
            if previous.token.expose() == token {
                expires_at = expires_at.min(previous.expires_at);
            }
        }
        let refresh_at = expires_at.checked_sub(REFRESH_MARGIN).unwrap_or(now);
        let token = AccessToken(Zeroizing::new(token));
        *guard = Some(Cached {
            token: token.clone(),
            refresh_at,
            expires_at,
        });
        Ok(token)
    }

    /// Drop the cached token, but only if it is still the one that failed
    /// (another task may already hold a fresh one).
    pub async fn invalidate(&self, seen: &AccessToken) {
        let mut guard = self.inner.lock().await;
        if guard
            .as_ref()
            .is_some_and(|cached| cached.token.expose() == seen.expose())
        {
            *guard = None;
            *self.last_fetch.lock().unwrap_or_else(|e| e.into_inner()) = None;
        }
    }

    pub async fn clear(&self) {
        *self.inner.lock().await = None;
        *self.last_fetch.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::notify::test_support::MockQq;
    use serde_json::json;

    #[tokio::test]
    async fn independent_caches_keep_the_original_expiry_when_qq_reuses_a_token() {
        crate::install_crypto_provider();
        let mock = MockQq::start().await;
        mock.state.lock().unwrap().token_replies.extend([
            (
                200,
                json!({"access_token":"shared-test-token","expires_in":"120"}),
            ),
            (
                200,
                json!({"access_token":"shared-test-token","expires_in":105}),
            ),
            (
                200,
                json!({"access_token":"shared-test-token","expires_in":7200}),
            ),
            (
                200,
                json!({"access_token":"shared-test-token","expires_in":"7200"}),
            ),
            (
                200,
                json!({"access_token":"next-test-token","expires_in":7200}),
            ),
            (
                200,
                json!({"access_token":"next-test-token","expires_in":7200}),
            ),
        ]);
        let a = TokenCache::default();
        let b = TokenCache::default();
        let http = reqwest::Client::new();
        let url = mock.token_url();
        let epoch = Instant::now();
        macro_rules! get {
            ($cache:expr, $offset:expr) => {
                $cache
                    .get_with_clock(&http, &url, "test-app", "test-secret", || {
                        epoch + Duration::from_secs($offset)
                    })
                    .await
                    .unwrap()
            };
        }
        assert_eq!(get!(&a, 0).expose(), "shared-test-token");
        assert_eq!(get!(&b, 15).expose(), "shared-test-token");
        // Different caches fetched independently, and both expiries are t=120.
        assert_eq!(mock.state.lock().unwrap().token_calls, 2);
        assert_eq!(get!(&a, 89).expose(), "shared-test-token");
        assert_eq!(get!(&b, 89).expose(), "shared-test-token");
        assert_eq!(mock.state.lock().unwrap().token_calls, 2);
        assert_eq!(get!(&a, 91).expose(), "shared-test-token");
        assert_eq!(get!(&b, 91).expose(), "shared-test-token");
        let expiry = epoch + Duration::from_secs(120);
        assert_eq!(a.inner.lock().await.as_ref().unwrap().expires_at, expiry);
        assert_eq!(b.inner.lock().await.as_ref().unwrap().expires_at, expiry);
        assert_eq!(mock.state.lock().unwrap().token_calls, 4);
        // The larger lifetime did not turn the reused token into a new one.
        assert_eq!(get!(&a, 121).expose(), "next-test-token");
        assert_eq!(get!(&b, 121).expose(), "next-test-token");
        assert_eq!(mock.state.lock().unwrap().token_calls, 6);
    }
}
