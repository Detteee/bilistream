//! Shared monitored-config identity hashes (subset vs full payload).

use super::state::read_json_file;
use super::types::*;
use crate::config::Config;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

pub fn monitored_config_from_config(cfg: &Config) -> MonitoredConfig {
    let mut payload = MonitoredConfig {
        interval: cfg.interval,
        auto_cover: cfg.auto_cover,
        enable_anti_collision: cfg.enable_anti_collision,
        anti_collision_list: cfg.anti_collision_list.clone(),
        enable_danmaku_command: cfg.bililive.enable_danmaku_command,
        enable_youtube_monitor: cfg.enable_youtube_monitor,
        enable_twitch_monitor: cfg.enable_twitch_monitor,
        youtube: cfg.youtube.clone(),
        twitch: cfg.twitch.clone(),
        priority_channel: cfg.priority_channel.clone(),
        niconico_enable_monitor: cfg.niconico.enable_monitor,
        channels_json: read_json_file("channels.json"),
        areas_json: read_json_file("areas.json"),
    };
    sanitize_local_source_settings(&mut payload);
    payload
}

pub(crate) fn sanitize_local_source_settings(payload: &mut MonitoredConfig) {
    payload.youtube.cookies_file = None;
    payload.youtube.cookies_from_browser = None;
    payload.youtube.proxy = None;
    payload.youtube.deno_path = None;
    payload.twitch.proxy = None;
}

pub fn monitored_config_version(cfg: &Config) -> String {
    canonical_value_hash(&monitored_sync_value_from_config(cfg))
}

/// Save notifications cover every shared setting, beyond the channel-status hash.
pub(crate) fn shared_settings_version(cfg: &Config) -> String {
    let mut payload = monitored_config_from_config(cfg);
    payload.enable_danmaku_command = false;
    payload.enable_youtube_monitor = false;
    payload.enable_twitch_monitor = false;
    payload.youtube.enable_monitor = false;
    payload.twitch.enable_monitor = false;
    payload.priority_channel.enabled = false;
    payload.priority_channel.auto_restart = false;
    payload.niconico_enable_monitor = false;
    monitored_config_integrity_version_from_payload(&payload)
}

pub fn monitored_config_version_from_payload(payload: &MonitoredConfig) -> String {
    let value = monitored_sync_value(payload);
    canonical_value_hash(&value)
}

pub fn monitored_config_integrity_version(cfg: &Config) -> String {
    monitored_config_integrity_version_from_payload(&monitored_config_from_config(cfg))
}

pub fn monitored_config_integrity_version_from_payload(payload: &MonitoredConfig) -> String {
    let value = serde_json::to_value(payload).unwrap_or(serde_json::Value::Null);
    canonical_value_hash(&value)
}

pub(crate) fn monitored_sync_value(payload: &MonitoredConfig) -> serde_json::Value {
    // Must stay field-for-field identical to monitored_sync_value_from_config
    // so a node's own version hash matches the one derived from a peer payload.
    serde_json::json!({
        "youtube": {
            "channel_name": payload.youtube.channel_name,
            "channel_id": payload.youtube.channel_id,
        },
        "twitch": {
            "channel_name": payload.twitch.channel_name,
            "channel_id": payload.twitch.channel_id,
        },
        "priority_channel": {
            "channel_name": payload.priority_channel.channel_name,
            "youtube_channel_id": payload.priority_channel.youtube_channel_id,
            "twitch_channel_id": payload.priority_channel.twitch_channel_id,
            "default_area": payload.priority_channel.default_area,
        },
        "channels_json": payload.channels_json,
        "areas_json": payload.areas_json,
    })
}

pub(crate) fn monitored_sync_value_from_config(cfg: &Config) -> serde_json::Value {
    // Only shared configuration belongs here. The priority channel's enabled
    // and auto_restart flags are per-node monitor toggles, so including them
    // both pushed one node's toggle onto its peers and made nodes that merely
    // differ by a toggle report as out of sync.
    serde_json::json!({
        "youtube": {
            "channel_name": cfg.youtube.channel_name,
            "channel_id": cfg.youtube.channel_id,
        },
        "twitch": {
            "channel_name": cfg.twitch.channel_name,
            "channel_id": cfg.twitch.channel_id,
        },
        "priority_channel": {
            "channel_name": cfg.priority_channel.channel_name,
            "youtube_channel_id": cfg.priority_channel.youtube_channel_id,
            "twitch_channel_id": cfg.priority_channel.twitch_channel_id,
            "default_area": cfg.priority_channel.default_area,
        },
        "channels_json": read_json_file("channels.json"),
        "areas_json": read_json_file("areas.json"),
    })
}

pub(crate) fn canonical_value_hash(value: &serde_json::Value) -> String {
    let serialized = canonical_json(value);
    let mut hasher = DefaultHasher::new();
    serialized.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

pub(crate) fn canonical_json(value: &serde_json::Value) -> String {
    let mut output = String::new();
    write_canonical_json(value, &mut output);
    output
}

pub(crate) fn write_canonical_json(value: &serde_json::Value, output: &mut String) {
    match value {
        serde_json::Value::Null => output.push_str("null"),
        serde_json::Value::Bool(value) => output.push_str(if *value { "true" } else { "false" }),
        serde_json::Value::Number(value) => output.push_str(&value.to_string()),
        serde_json::Value::String(value) => {
            output.push_str(&serde_json::to_string(value).unwrap_or_default());
        }
        serde_json::Value::Array(values) => {
            output.push('[');
            for (index, item) in values.iter().enumerate() {
                if index > 0 {
                    output.push(',');
                }
                write_canonical_json(item, output);
            }
            output.push(']');
        }
        serde_json::Value::Object(map) => {
            let mut keys = map.keys().collect::<Vec<_>>();
            keys.sort();
            output.push('{');
            for (index, key) in keys.into_iter().enumerate() {
                if index > 0 {
                    output.push(',');
                }
                output.push_str(&serde_json::to_string(key).unwrap_or_default());
                output.push(':');
                write_canonical_json(&map[key], output);
            }
            output.push('}');
        }
    }
}
