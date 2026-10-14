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
        let value = data.get(name).and_then(|v| {
            v.as_i64()
                .or_else(|| v.as_f64().filter(|f| f.is_finite()).map(|f| f as i64))
        });
        if value.is_some_and(|v| v < 0) {
            return Err(StopLiveDataError::Sentinel(name));
        }
        *slot = value;
    }
    let [live_time_raw, add_fans, new_fans_club, danmu_num, max_online, watched_count] = fields;
    Ok(BiliTotals {
        live_time_raw,
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
    ["live_key", "up_session"].iter().find_map(|field| {
        let raw = match data.get(*field)? {
            Value::String(s) => s.trim().to_string(),
            Value::Number(n) => n.to_string(),
            _ => return None,
        };
        (!raw.is_empty() && raw != "0" && !raw.starts_with('<')).then_some(raw)
    })
}

/// Allowlist-redacted copy for capture mode: `code` + the six fields only.
pub(crate) fn redact_stop_live_data(reply: &Value) -> Value {
    let mut data = serde_json::Map::new();
    if let Some(src) = reply.get("data").and_then(Value::as_object) {
        for name in STOP_LIVE_DATA_FIELDS {
            if let Some(v) = src.get(name) {
                data.insert(name.to_string(), v.clone());
            }
        }
    }
    serde_json::json!({ "code": reply.get("code").cloned().unwrap_or(Value::Null), "data": data })
}

/// GetInfo capture keeps only `code`, `live_status` and whether a key was
/// present — never the key, uid, room or any URL.
pub(crate) fn redact_room_info(reply: &Value) -> Value {
    serde_json::json!({
        "code": reply.get("code").cloned().unwrap_or(Value::Null),
        "data": {
            "live_status": reply.pointer("/data/live_status").cloned().unwrap_or(Value::Null),
            "has_live_key": parse_room_live_key(reply).is_some(),
        }
    })
}

/// Fetch schedule: one call ≈ 5 s after the stop, one retry ≈ 5 min later.
pub(crate) const FIRST_DELAY: std::time::Duration = std::time::Duration::from_secs(5);
pub(crate) const RETRY_DELAY: std::time::Duration = std::time::Duration::from_secs(300);

/// Runs the schedule with an injected fetcher (tests use a fake). Returns
/// the totals, or `None` after both attempts failed. Warn lines never
/// include the key.
pub(crate) async fn fetch_with_retry<F, Fut>(
    mut fetch: F,
    first: std::time::Duration,
    retry: std::time::Duration,
) -> Option<BiliTotals>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<Value, String>>,
{
    tokio::time::sleep(first).await;
    for attempt in 0..2 {
        if attempt == 1 {
            tokio::time::sleep(retry).await;
        }
        match fetch().await {
            Ok(reply) => {
                super::capture::stop_live_data(&reply);
                match parse_stop_live_data(&reply) {
                    Ok(totals) => return Some(totals),
                    Err(error) => {
                        tracing::warn!("B站直播汇总无效（第 {} 次）: {error:?}", attempt + 1)
                    }
                }
            }
            Err(error) => tracing::warn!(
                "B站直播汇总请求失败（第 {} 次）: {}",
                attempt + 1,
                crate::logging::safe_text(&error, 120)
            ),
        }
    }
    None
}
