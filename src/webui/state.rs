use lazy_static::lazy_static;
use serde::Serialize;
use std::collections::VecDeque;
use std::sync::{LockResult, Mutex, RwLock};

use crate::config::Config;

lazy_static! {
    static ref LOG_BUFFER: Mutex<Option<VecDeque<String>>> = Mutex::new(None);
    static ref STATUS_CACHE: RwLock<Option<StatusData>> = RwLock::new(None);
}

#[derive(Serialize, Clone, Default)]
pub struct StatusData {
    pub bilibili: BiliStatus,
    pub youtube: Option<YtStatus>,
    pub twitch: Option<TwStatus>,
}

#[derive(Serialize, Clone, Default)]
pub struct BiliStatus {
    pub is_live: bool,
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
    pub stream_total_bytes: u64,
    pub stream_cache_total_bytes: u64,
    pub hls_cache_active: bool,
    pub enable_danmaku_command: bool,
}

#[derive(Serialize, Clone, Default)]
pub struct NetworkStatus {
    pub stream_speed: Option<f32>,
    pub stream_cache_speed: Option<f32>,
    pub stream_bitrate_kbps: Option<f32>,
    pub stream_cache_bitrate_kbps: Option<f32>,
    pub stream_fps: Option<f32>,
    pub stream_frame: Option<u64>,
    pub stream_total_bytes: u64,
    pub stream_cache_total_bytes: u64,
    pub hls_cache_active: bool,
}

#[derive(Serialize, Clone)]
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

#[derive(Serialize, Clone)]
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
    let mut buffer = recover_lock(LOG_BUFFER.lock(), "webui log buffer");
    *buffer = Some(VecDeque::with_capacity(500));
}

pub fn add_log_line(line: String) {
    let mut buffer = recover_lock(LOG_BUFFER.lock(), "webui log buffer");
    if let Some(ref mut buf) = *buffer {
        buf.push_back(line);
        if buf.len() > 500 {
            buf.pop_front();
        }
    }
}

pub fn get_logs() -> Vec<String> {
    let buffer = recover_lock(LOG_BUFFER.lock(), "webui log buffer");
    if let Some(ref buf) = *buffer {
        buf.iter().cloned().collect()
    } else {
        Vec::new()
    }
}

pub fn update_status_cache(status: StatusData) {
    let mut cache = recover_lock(STATUS_CACHE.write(), "webui status cache");
    *cache = Some(status);
}

pub fn update_status_cache_with(update: impl FnOnce(&mut StatusData)) {
    let mut cache = recover_lock(STATUS_CACHE.write(), "webui status cache");
    let status = cache.get_or_insert_with(StatusData::default);
    update(status);
}

pub fn get_status_cache() -> Option<StatusData> {
    let cache = recover_lock(STATUS_CACHE.read(), "webui status cache");
    cache.clone()
}

pub fn refresh_status_cache_config_from(cfg: &Config) {
    update_status_cache_with(|cached_status| {
        cached_status.bilibili.enable_danmaku_command = cfg.bililive.enable_danmaku_command;

        if cfg.youtube.enable_monitor && !cfg.youtube.channel_id.is_empty() {
            let yt_area_name = crate::plugins::get_area_name(cfg.youtube.area_v2)
                .unwrap_or_else(|| format!("未知分区 (ID: {})", cfg.youtube.area_v2));

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
}

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
