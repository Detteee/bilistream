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

pub(crate) fn room_info(reply: &Value) {
    if ENABLED.load(Ordering::Relaxed) {
        capture("room_get_info", super::enrich::redact_room_info(reply));
    }
}

#[cfg(test)]
pub(crate) fn is_enabled_for_test() -> bool {
    ENABLED.load(Ordering::Relaxed) || crate::config::HistoryConfig::default().capture
}
