use serde::Serialize;
#[cfg(test)]
use std::sync::LockResult;

use crate::config::Config;

#[derive(Serialize, Clone, Default, PartialEq)]
pub struct StatusData {
    pub bilibili: BiliStatus,
    pub youtube: Option<YtStatus>,
    pub twitch: Option<TwStatus>,
}

#[derive(Serialize, Clone, Default, PartialEq)]
pub struct BiliStatus {
    pub is_live: bool,
    /// This node's publisher, independent of the shared Bilibili room state.
    #[serde(default)]
    pub ffmpeg_running: bool,
    pub title: String,
    pub area_id: u64,
    pub area_name: String,
    pub stream_quality: Option<String>,
    pub stream_speed: Option<f32>,
    pub stream_cache_speed: Option<f32>,
    pub stream_bitrate_kbps: Option<f32>,
    pub stream_cache_bitrate_kbps: Option<f32>,
    pub stream_fps: Option<f32>,
    pub stream_frame: Option<u64>,
    pub stream_time_secs: Option<u32>,
    pub stream_cache_time_secs: Option<u32>,
    pub hls_cache_active: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stream_bitrate_history: Vec<f32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stream_cache_bitrate_history: Vec<f32>,
    pub enable_danmaku_command: bool,
}

impl BiliStatus {
    pub fn apply_network(&mut self, network: NetworkStatus) {
        self.ffmpeg_running = network.ffmpeg_running;
        self.stream_speed = network.stream_speed;
        self.stream_cache_speed = network.stream_cache_speed;
        self.stream_bitrate_kbps = network.stream_bitrate_kbps;
        self.stream_cache_bitrate_kbps = network.stream_cache_bitrate_kbps;
        self.stream_fps = network.stream_fps;
        self.stream_frame = network.stream_frame;
        self.stream_time_secs = network.stream_time_secs;
        self.stream_cache_time_secs = network.stream_cache_time_secs;
        self.hls_cache_active = network.hls_cache_active;
        self.stream_bitrate_history = network.stream_bitrate_history;
        self.stream_cache_bitrate_history = network.stream_cache_bitrate_history;
    }
}

#[derive(Serialize, Clone, Default, PartialEq)]
pub struct NetworkStatus {
    #[serde(default)]
    pub ffmpeg_running: bool,
    pub stream_speed: Option<f32>,
    pub stream_cache_speed: Option<f32>,
    pub stream_bitrate_kbps: Option<f32>,
    pub stream_cache_bitrate_kbps: Option<f32>,
    pub stream_fps: Option<f32>,
    pub stream_frame: Option<u64>,
    pub stream_time_secs: Option<u32>,
    pub stream_cache_time_secs: Option<u32>,
    pub hls_cache_active: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stream_bitrate_history: Vec<f32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stream_cache_bitrate_history: Vec<f32>,
}

#[derive(Serialize, Clone, PartialEq)]
pub struct YtStatus {
    pub is_live: bool,
    pub title: Option<String>,
    pub topic: Option<String>,
    pub channel_name: String,
    pub channel_id: String,
    pub quality: String,
    pub area_id: u64,
    pub area_name: String,
    pub crop_enabled: bool,
    pub ffmpeg_cache_enabled: bool,
    pub ffmpeg_cache_latency_secs: u64,
}

#[derive(Serialize, Clone, PartialEq)]
pub struct TwStatus {
    pub is_live: bool,
    pub title: Option<String>,
    pub game: Option<String>,
    pub channel_name: String,
    pub channel_id: String,
    pub quality: String,
    pub area_id: u64,
    pub area_name: String,
    pub crop_enabled: bool,
    pub ffmpeg_cache_enabled: bool,
    pub ffmpeg_cache_latency_secs: u64,
}

pub fn init_log_buffer() {
    crate::AppState::current().init_log_buffer();
}

pub fn add_log_line(line: String) {
    crate::AppState::current().add_log_line(line);
}

pub fn get_logs() -> Vec<String> {
    crate::AppState::current().get_logs()
}

pub fn update_status_cache(status: StatusData) {
    crate::AppState::current().update_status_cache(status);
}

pub fn update_status_cache_with(update: impl FnOnce(&mut StatusData)) {
    crate::AppState::current().update_status_cache_with(update);
}

/// Wake the status refresh worker so external live status is re-fetched now
/// instead of at the next poll interval (used right after state changes).
pub fn request_status_refresh() {
    crate::AppState::current().request_status_refresh();
}

/// Resolves when someone calls [`request_status_refresh`].
pub async fn status_refresh_requested() {
    crate::AppState::current().status_refresh_requested().await;
}

pub fn get_status_cache() -> Option<StatusData> {
    crate::AppState::current().get_status_cache()
}

pub fn refresh_status_cache_config_from(cfg: &Config) {
    crate::config::with_current_config(cfg, || {
        update_status_cache_with(|cached_status| {
            cached_status.bilibili.enable_danmaku_command = cfg.bililive.enable_danmaku_command;

            if cfg.youtube.enable_monitor && !cfg.youtube.channel_id.is_empty() {
                let yt_area_name = crate::plugins::get_area_name(cfg.youtube.area_v2)
                    .unwrap_or_else(|| format!("未知分区 (ID: {})", cfg.youtube.area_v2));

                if cached_status
                    .youtube
                    .as_ref()
                    .is_some_and(|status| status.channel_id != cfg.youtube.channel_id)
                {
                    cached_status.youtube = None;
                }
                if let Some(ref mut yt_status) = cached_status.youtube {
                    yt_status.channel_name = cfg.youtube.channel_name.clone();
                    yt_status.channel_id = cfg.youtube.channel_id.clone();
                    yt_status.area_id = cfg.youtube.area_v2;
                    yt_status.area_name = yt_area_name;
                    yt_status.quality = cfg.youtube.quality.clone();
                    yt_status.crop_enabled = cfg.youtube.crop.is_some();
                    yt_status.ffmpeg_cache_enabled = cfg.youtube.ffmpeg_cache.enabled;
                    yt_status.ffmpeg_cache_latency_secs = cfg.youtube.ffmpeg_cache.latency_secs;
                } else {
                    cached_status.youtube = Some(YtStatus {
                        is_live: false,
                        title: Some("-".to_string()),
                        channel_name: cfg.youtube.channel_name.clone(),
                        channel_id: cfg.youtube.channel_id.clone(),
                        area_id: cfg.youtube.area_v2,
                        area_name: yt_area_name,
                        topic: Some("-".to_string()),
                        quality: cfg.youtube.quality.clone(),
                        crop_enabled: cfg.youtube.crop.is_some(),
                        ffmpeg_cache_enabled: cfg.youtube.ffmpeg_cache.enabled,
                        ffmpeg_cache_latency_secs: cfg.youtube.ffmpeg_cache.latency_secs,
                    });
                }
            } else {
                cached_status.youtube = None;
            }

            if cfg.twitch.enable_monitor && !cfg.twitch.channel_id.is_empty() {
                let tw_area_name = crate::plugins::get_area_name(cfg.twitch.area_v2)
                    .unwrap_or_else(|| format!("未知分区 (ID: {})", cfg.twitch.area_v2));

                if cached_status
                    .twitch
                    .as_ref()
                    .is_some_and(|status| status.channel_id != cfg.twitch.channel_id)
                {
                    cached_status.twitch = None;
                }
                if let Some(ref mut tw_status) = cached_status.twitch {
                    tw_status.channel_name = cfg.twitch.channel_name.clone();
                    tw_status.channel_id = cfg.twitch.channel_id.clone();
                    tw_status.area_id = cfg.twitch.area_v2;
                    tw_status.area_name = tw_area_name;
                    tw_status.quality = cfg.twitch.quality.clone();
                    tw_status.crop_enabled = cfg.twitch.crop.is_some();
                    tw_status.ffmpeg_cache_enabled = cfg.twitch.ffmpeg_cache.enabled;
                    tw_status.ffmpeg_cache_latency_secs = cfg.twitch.ffmpeg_cache.latency_secs;
                } else {
                    cached_status.twitch = Some(TwStatus {
                        is_live: false,
                        title: Some("-".to_string()),
                        channel_name: cfg.twitch.channel_name.clone(),
                        channel_id: cfg.twitch.channel_id.clone(),
                        area_id: cfg.twitch.area_v2,
                        area_name: tw_area_name,
                        game: Some("-".to_string()),
                        quality: cfg.twitch.quality.clone(),
                        crop_enabled: cfg.twitch.crop.is_some(),
                        ffmpeg_cache_enabled: cfg.twitch.ffmpeg_cache.enabled,
                        ffmpeg_cache_latency_secs: cfg.twitch.ffmpeg_cache.latency_secs,
                    });
                }
            } else {
                cached_status.twitch = None;
            }
        });
    });
}

#[cfg(test)]
fn recover_lock<T>(lock: LockResult<T>, name: &str) -> T {
    lock.unwrap_or_else(|poisoned| {
        tracing::warn!("Recovering poisoned {}", name);
        poisoned.into_inner()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::panic::{catch_unwind, AssertUnwindSafe};
    use std::sync::Mutex;

    #[test]
    fn recover_lock_returns_inner_after_poison() {
        let lock = Mutex::new(7);
        let _ = catch_unwind(AssertUnwindSafe(|| {
            let _guard = lock.lock().unwrap();
            panic!("poison test lock");
        }));

        let mut guard = recover_lock(lock.lock(), "test lock");
        *guard += 1;

        assert_eq!(*guard, 8);
    }
}
