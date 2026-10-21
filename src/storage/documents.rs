//! Shape validation for stored documents.
//!
//! These documents are inputs to peer sync and backup restore, so a malformed
//! record is refused before it is written. This is not a migration: the file
//! import path no longer exists.

use super::{invalid, NAMES};
use serde_json::Value;
use std::io;

/// `YT_channels[]` / `TW_channels[]`: the channel format before `channels[]`.
/// No current build writes it and nothing reads it any more.
pub(crate) const LEGACY_CHANNEL_KEYS: [&str; 2] = ["YT_channels", "TW_channels"];

/// A `channels.json` still carrying an old-format key is refused by name.
/// The error names the key only, never a channel name or id inside it.
pub(crate) fn reject_legacy_channels(value: &Value) -> io::Result<()> {
    match LEGACY_CHANNEL_KEYS
        .iter()
        .find(|key| value.get(**key).is_some())
    {
        Some(key) => Err(invalid(&format!(
            "channels.json 含旧格式字段 {key}，本版本不再读取其中的频道；请在频道管理中重新添加这些频道"
        ))),
        None => Ok(()),
    }
}

pub(crate) fn validate(name: &str, value: &Value) -> io::Result<()> {
    if !NAMES.contains(&name) {
        return Err(invalid("备份包含未知数据类型"));
    }
    let bad = || invalid(&format!("{name} 格式无效，旧记录已保留"));
    match name {
        "config.json" => {
            serde_json::from_value::<crate::config::Config>(value.clone()).map_err(|_| bad())?;
        }
        "channels.json" => {
            reject_legacy_channels(value)?;
            let channels = value
                .get("channels")
                .and_then(Value::as_array)
                .ok_or_else(bad)?;
            for channel in channels {
                if channel.get("name").and_then(Value::as_str).is_none()
                    || !channel.get("platforms").is_some_and(Value::is_object)
                    || !channel.get("aliases").is_some_and(Value::is_array)
                {
                    return Err(bad());
                }
            }
        }
        "areas.json" => {
            if !value.get("areas").is_some_and(Value::is_array) {
                return Err(bad());
            }
        }
        "cookies.json" => {
            if !value
                .pointer("/cookie_info/cookies")
                .is_some_and(Value::is_array)
                || !value.get("token_info").is_some_and(Value::is_object)
            {
                return Err(bad());
            }
        }
        "cookies.txt" => {
            let text = value.as_str().ok_or_else(bad)?;
            if !text.is_empty() {
                crate::plugins::youtube_cookies::validate_netscape(text).map_err(|_| bad())?;
            }
        }
        "invalid_words.txt" => {
            if !value.is_string() {
                return Err(bad());
            }
        }
        "youtube_quota.json" => {
            if !value.get("day").is_some_and(Value::is_i64)
                || !value.get("keys").is_some_and(Value::is_object)
            {
                return Err(bad());
            }
        }
        "youtube_golive_hours.json" => {
            let channels = value
                .get("channels")
                .and_then(Value::as_object)
                .ok_or_else(bad)?;
            if !value.get("counted").is_some_and(Value::is_object)
                || !value.get("decayed_at").is_some_and(Value::is_i64)
            {
                return Err(bad());
            }
            for buckets in channels.values() {
                if !buckets
                    .as_array()
                    .is_some_and(|v| v.len() == 24 && v.iter().all(Value::is_number))
                {
                    return Err(bad());
                }
            }
        }
        "warning_exempt_cooldown.json" => {
            serde_json::from_value::<crate::plugins::warning_exempt::ExemptCooldown>(value.clone())
                .map_err(|_| bad())?;
        }
        _ => return Err(bad()),
    }
    Ok(())
}
