use lazy_static::lazy_static;
use serde::Serialize;
use std::collections::VecDeque;
use std::sync::{LockResult, Mutex, RwLock};

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
