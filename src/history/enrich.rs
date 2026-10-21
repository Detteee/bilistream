//! B站 session totals (design §7.2). The StopLiveData parser is an
//! allowlist: `code` + six `data` fields. `HamsterRmb` (revenue) is never read.

use super::event::BiliTotals;
use serde_json::Value;

/// The capture allowlist: `code` plus these `data` fields, nothing else.
pub(crate) const STOP_LIVE_DATA_FIELDS: [&str; 6] = [
    "LiveTime",
    "AddFans",
    "NewFansClub",
    "DanmuNum",
    "MaxOnline",
    "WatchedCount",
];

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum StopLiveDataError {
    /// Non-zero `code` (for example -101, not logged in).
    Code(i64),
    NoData,
    /// Any allowlisted field is negative (unknown live_key → -999999).
    Sentinel(&'static str),
}

/// Validity (verified 2026-10-14): `code == 0`, `data` present, and no
/// allowlisted field negative. A missing or non-integer field in a valid
/// reply becomes NULL for that field only.
pub(crate) fn parse_stop_live_data(reply: &Value) -> Result<BiliTotals, StopLiveDataError> {
    let code = reply.get("code").and_then(Value::as_i64).unwrap_or(-1);
    if code != 0 {
        return Err(StopLiveDataError::Code(code));
    }
    let data = reply
        .get("data")
        .and_then(Value::as_object)
        .ok_or(StopLiveDataError::NoData)?;
    let mut fields = [None; 6];
    for (slot, name) in fields.iter_mut().zip(STOP_LIVE_DATA_FIELDS) {
        let value = data.get(name);
        if value.and_then(Value::as_f64).is_some_and(|v| v < 0.0) {
            return Err(StopLiveDataError::Sentinel(name));
        }
        // Counts are integers. In particular -0.5 must not truncate to zero
        // and turn an upstream negative sentinel into a successful summary.
        *slot = value.and_then(Value::as_i64);
    }
    let [live_time_secs, add_fans, new_fans_club, danmu_num, max_online, watched_count] = fields;
    Ok(BiliTotals {
        live_time_secs,
        add_fans,
        new_fans_club,
        danmu_num,
        max_online,
        watched_count,
    })
}

/// `GetInfo` (read-only fallback): the room's current live key while live.
/// Reads `data.live_key`, then `data.up_session` (the same id in the
/// replies we have seen). Redacted placeholders, `0` and empty are no key.
pub(crate) fn parse_room_live_key(reply: &Value) -> Option<String> {
    if reply.get("code").and_then(Value::as_i64) != Some(0) {
        return None;
    }
    let data = reply.get("data")?;
    if data.get("live_status").and_then(Value::as_i64) != Some(1) {
        return None;
    }
    ["live_key", "up_session"]
        .iter()
        .find_map(|field| parse_live_key(data.get(*field)?))
}

/// The opaque identity is retained verbatim, within the heartbeat book's
/// size budget. Numeric sentinels and placeholders are not identities.
pub(crate) fn parse_live_key(value: &Value) -> Option<String> {
    let raw = match value {
        Value::String(value) => value.trim().to_string(),
        Value::Number(value) => value.as_u64()?.to_string(),
        _ => return None,
    };
    (!raw.is_empty()
        && raw != "0"
        && !raw.starts_with('<')
        && raw.len() <= 256
        && !raw.chars().any(char::is_control))
    .then_some(raw)
}

/// Allowlist-redacted copy for capture mode: `code` + the six fields only.
pub(crate) fn redact_stop_live_data(reply: &Value) -> Value {
    let mut data = serde_json::Map::new();
    if let Some(src) = reply.get("data").and_then(Value::as_object) {
        for name in STOP_LIVE_DATA_FIELDS {
            if let Some(v) = src.get(name) {
                // Provider schema changes must not put arbitrary nested
                // strings/objects into a supposedly allowlisted fixture.
                data.insert(
                    name.to_string(),
                    if v.is_number() {
                        v.clone()
                    } else {
                        Value::Null
                    },
                );
            }
        }
    }
    serde_json::json!({ "code": reply.get("code").and_then(Value::as_i64), "data": data })
}

/// GetInfo capture keeps only `code`, `live_status` and whether a key was
/// present — never the key, uid, room or any URL.
pub(crate) fn redact_room_info(reply: &Value) -> Value {
    serde_json::json!({
        "code": reply.get("code").and_then(Value::as_i64),
        "data": {
            "live_status": reply.pointer("/data/live_status").and_then(Value::as_i64),
            "has_live_key": parse_room_live_key(reply).is_some(),
        }
    })
}

/// Fetch schedule: one call ≈ 5 s after the stop, one retry ≈ 5 min later.
pub(crate) const FIRST_DELAY: std::time::Duration = std::time::Duration::from_secs(5);
pub(crate) const RETRY_DELAY: std::time::Duration = std::time::Duration::from_secs(300);
pub(crate) const FETCH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum FetchResult {
    Finished(Option<BiliTotals>),
    /// Ownership disappeared before an attempt or while it was in flight.
    /// Keep the claim recoverable; this is not an upstream failure.
    Cancelled,
}

/// Runs the schedule with an injected fetcher (tests use a fake). Returns
/// the totals, or `None` after both attempts failed. Warn lines never
/// include the key.
#[cfg(test)]
pub(crate) async fn fetch_with_retry<F, Fut>(
    fetch: F,
    first: std::time::Duration,
    retry: std::time::Duration,
) -> Option<BiliTotals>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<Value, String>>,
{
    match fetch_with_retry_guarded(fetch, first, retry, || true).await {
        FetchResult::Finished(totals) => totals,
        FetchResult::Cancelled => None,
    }
}

/// Same validated parser and two-attempt schedule, with an ownership check
/// before each request and before publishing a result. Session closing never
/// waits on this future, and losing authority is distinct from failed totals.
pub(crate) async fn fetch_with_retry_guarded<F, Fut, G>(
    mut fetch: F,
    first: std::time::Duration,
    retry: std::time::Duration,
    mut owns_claim: G,
) -> FetchResult
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<Value, String>>,
    G: FnMut() -> bool,
{
    tokio::time::sleep(first).await;
    for attempt in 0..2 {
        if attempt == 1 {
            tokio::time::sleep(retry).await;
        }
        if !owns_claim() {
            return FetchResult::Cancelled;
        }
        let reply = fetch().await;
        if !owns_claim() {
            return FetchResult::Cancelled;
        }
        match reply {
            Ok(reply) => {
                super::capture::stop_live_data(&reply);
                match parse_stop_live_data(&reply) {
                    Ok(totals) => return FetchResult::Finished(Some(totals)),
                    Err(error) => {
                        tracing::warn!("B站直播汇总无效（第 {} 次）：{error:?}", attempt + 1)
                    }
                }
            }
            Err(error) => tracing::warn!(
                "B站直播汇总请求失败（第 {} 次）：{}",
                attempt + 1,
                safe_fetch_failure(&error)
            ),
        }
    }
    FetchResult::Finished(None)
}

/// Fetchers return classes, never provider bodies/URLs. Keep that guarantee
/// even when a future caller accidentally passes an unsanitized error.
fn safe_fetch_failure(error: &str) -> &str {
    if matches!(error, "timeout" | "invalid json" | "request failed")
        || error.strip_prefix("http ").is_some_and(|code| {
            code.parse::<u16>()
                .is_ok_and(|code| (400..600).contains(&code))
        })
    {
        error
    } else {
        "request failed"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    #[test]
    fn negative_fraction_is_invalid_and_positive_fraction_is_not_a_count() {
        assert_eq!(
            parse_stop_live_data(&serde_json::json!({"code":0,"data":{"AddFans":-0.5}})),
            Err(StopLiveDataError::Sentinel("AddFans"))
        );
        let totals = parse_stop_live_data(
            &serde_json::json!({"code":0,"data":{"DanmuNum":2.5,"WatchedCount":0}}),
        )
        .unwrap();
        assert_eq!(totals.danmu_num, None);
        assert_eq!(totals.watched_count, Some(0));
    }

    #[tokio::test]
    async fn lost_claim_cancels_before_first_request_and_before_delayed_retry() {
        let requests = AtomicUsize::new(0);
        let fetch = || {
            requests.fetch_add(1, Ordering::Relaxed);
            async { Err("timeout".to_string()) }
        };
        assert_eq!(
            fetch_with_retry_guarded(fetch, Duration::ZERO, Duration::ZERO, || false).await,
            FetchResult::Cancelled
        );
        assert_eq!(requests.load(Ordering::Relaxed), 0);
        let checks = AtomicUsize::new(0);
        assert_eq!(
            fetch_with_retry_guarded(fetch, Duration::ZERO, Duration::from_millis(1), || checks
                .fetch_add(1, Ordering::Relaxed)
                < 2)
            .await,
            FetchResult::Cancelled
        );
        assert_eq!(
            requests.load(Ordering::Relaxed),
            1,
            "the lost owner cannot perform the second attempt"
        );
    }

    #[test]
    fn future_fetch_errors_cannot_leak_body_or_live_key_into_warning() {
        assert_eq!(safe_fetch_failure("timeout"), "timeout");
        assert_eq!(safe_fetch_failure("http 403"), "http 403");
        assert_eq!(
            safe_fetch_failure("request failed for live_key=SYNTHETIC"),
            "request failed"
        );
        assert_eq!(safe_fetch_failure("http 403\nSYNTHETIC"), "request failed");
    }

    #[test]
    fn capture_allowlist_rejects_nested_or_string_values_in_numeric_fields() {
        let reply = serde_json::json!({"code":"SYNTHETIC", "data":{
            "live_status":{"live_key":"SYNTHETIC"},
            "WatchedCount":{"token":"SYNTHETIC"}, "AddFans":"SYNTHETIC", "MaxOnline":12,
            "live_key":"SYNTHETIC"}});
        let totals = redact_stop_live_data(&reply);
        assert_eq!(totals["data"]["MaxOnline"], 12);
        assert!(totals["data"]["WatchedCount"].is_null());
        assert!(!totals.to_string().contains("SYNTHETIC"));
        assert!(!redact_room_info(&reply).to_string().contains("SYNTHETIC"));
    }

    #[test]
    fn live_key_parser_accepts_opaque_keys_but_rejects_sentinels_and_unbounded_values() {
        assert_eq!(
            parse_live_key(&serde_json::json!(42)).as_deref(),
            Some("42")
        );
        assert_eq!(
            parse_live_key(&serde_json::json!(" synthetic-key ")).as_deref(),
            Some("synthetic-key")
        );
        for value in [
            serde_json::json!(null),
            serde_json::json!(0),
            serde_json::json!(-1),
            serde_json::json!(1.5),
            serde_json::json!(true),
            serde_json::json!(""),
            serde_json::json!("<redacted>"),
            serde_json::json!("a\nb"),
            serde_json::json!("a".repeat(257)),
        ] {
            assert_eq!(parse_live_key(&value), None);
        }
    }
}
