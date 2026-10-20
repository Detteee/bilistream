//! Capture mode (off by default, per node): allowlist-redacted B站 replies
//! go to `.trellis/fixtures/` only, which `.gitignore` excludes.

use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

static ENABLED: AtomicBool = AtomicBool::new(false);

pub(crate) fn set_enabled(on: bool) {
    ENABLED.store(on, Ordering::Relaxed);
}

/// The only directory capture ever writes to, relative to `root`.
pub(crate) fn fixtures_dir(root: &Path) -> PathBuf {
    root.join(".trellis").join("fixtures")
}

fn root() -> Option<PathBuf> {
    std::env::current_dir().ok()
}

pub(crate) fn write_to(root: &Path, name: &str, redacted: &Value) -> std::io::Result<PathBuf> {
    let dir = fixtures_dir(root);
    std::fs::create_dir_all(&dir)?;
    let file = dir.join(format!("{name}-{}.json", crate::storage::now()));
    std::fs::write(&file, serde_json::to_vec_pretty(redacted)?)?;
    Ok(file)
}

fn capture(name: &str, redacted: Value) {
    if !ENABLED.load(Ordering::Relaxed) {
        return;
    }
    if let Some(root) = root() {
        if let Err(error) = write_to(&root, name, &redacted) {
            tracing::debug!("统计采样写入失败: {error}");
        }
    }
}

pub(crate) fn stop_live_data(reply: &Value) {
    if ENABLED.load(Ordering::Relaxed) {
        capture("stoplivedata", super::enrich::redact_stop_live_data(reply));
    }
}

/// The strict warning/cut capture shape (PRD capture mode): only the frame
/// command, the top-level `data` key names and the cleaned 违规原因 text.
/// Room ids, uids, links, cookies and every value stay out.
pub(crate) fn warning_frame_value(cmd: &str, data_keys: &[String], reason: Option<&str>) -> Value {
    let mut value = serde_json::json!({ "cmd": cmd, "data_keys": data_keys });
    if let Some(reason) = reason.filter(|reason| !reason.is_empty()) {
        value["violation_reason"] = Value::String(reason.to_owned());
    }
    value
}

pub(crate) fn warning_frame(cmd: &str, data_keys: &[String], reason: Option<&str>) {
    if ENABLED.load(Ordering::Relaxed) {
        capture("warning_frame", warning_frame_value(cmd, data_keys, reason));
    }
}

pub(crate) fn room_info(reply: &Value) {
    if ENABLED.load(Ordering::Relaxed) {
        capture("room_get_info", super::enrich::redact_room_info(reply));
    }
}

#[cfg(test)]
pub(crate) fn is_enabled_for_test() -> bool {
    ENABLED.load(Ordering::Relaxed) || crate::config::HistoryConfig::default().capture
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn warning_frame_shape_keeps_only_the_command_keys_and_reason() {
        let value = warning_frame_value(
            "CUT_OFF_V2",
            &["cut_off_data".to_string(), "room_id".to_string()],
            Some("您本场直播存在挂机、录播等消极直播行为"),
        );
        assert_eq!(value["cmd"], "CUT_OFF_V2");
        assert_eq!(
            value["data_keys"],
            serde_json::json!(["cut_off_data", "room_id"])
        );
        assert_eq!(
            value["violation_reason"],
            "您本场直播存在挂机、录播等消极直播行为"
        );
        // Only key names travel, never a room id, uid or link value.
        assert!(value.get("room_id").is_none());
        assert!(value.get("uid").is_none());
        assert!(value.get("link_url").is_none());

        let mask = warning_frame_value("FULL_SCREEN_MASK_OPEN", &["title".to_string()], None);
        assert!(mask.get("violation_reason").is_none());
    }
}
