use serde::{Deserialize, Serialize};

use crate::config::Config;

#[derive(Debug, Serialize, Deserialize, Clone, Default, PartialEq)]
pub struct StatusData {
    pub bilibili: BiliStatus,
    pub youtube: Option<YtStatus>,
    pub twitch: Option<TwStatus>,
    #[serde(default)]
    pub niconico: Option<NicoStatus>,
    pub priority_channel: Option<PriorityChannelStatus>,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
pub struct PriorityChannelStatus {
    pub enabled: bool,
    #[serde(default)]
    pub auto_restart: bool,
    pub channel_name: String,
    pub is_live: bool,
    pub platform: Option<String>,
    pub title: Option<String>,
    pub default_area: u64,
}

#[derive(Debug, Serialize, Deserialize, Clone, Default, PartialEq)]
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

#[derive(Debug, Serialize, Deserialize, Clone, Default, PartialEq)]
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

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
pub struct YtStatus {
    pub is_live: bool,
    #[serde(default)]
    pub enable_monitor: bool,
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

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
pub struct TwStatus {
    pub is_live: bool,
    #[serde(default)]
    pub enable_monitor: bool,
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

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
pub struct NicoStatus {
    pub is_live: bool,
    #[serde(default)]
    pub enable_monitor: bool,
    pub title: Option<String>,
    pub channel_name: String,
    pub channel_id: String,
    #[serde(default)]
    pub live_id: Option<String>,
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

            if platform_channel_configured(&cfg.youtube.channel_name, &cfg.youtube.channel_id) {
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
                yt_status.enable_monitor = cfg.youtube.enable_monitor;
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
                        enable_monitor: cfg.youtube.enable_monitor,
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

            if platform_channel_configured(&cfg.twitch.channel_name, &cfg.twitch.channel_id) {
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
                tw_status.enable_monitor = cfg.twitch.enable_monitor;
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
                        enable_monitor: cfg.twitch.enable_monitor,
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
            if cached_status
                .priority_channel
                .as_ref()
                .is_some_and(|status| status.channel_name != cfg.priority_channel.channel_name)
            {
                cached_status.priority_channel = None;
            }
            if let Some(ref mut priority_status) = cached_status.priority_channel {
                priority_status.enabled = cfg.priority_channel.enabled;
                priority_status.auto_restart = cfg.priority_channel.auto_restart;
                if !priority_status.enabled {
                    priority_status.is_live = false;
                    priority_status.platform = None;
                    priority_status.title = None;
                }
                priority_status.channel_name = cfg.priority_channel.channel_name.clone();
                priority_status.default_area = cfg.priority_channel.default_area;
            } else {
                cached_status.priority_channel = Some(PriorityChannelStatus {
                    enabled: cfg.priority_channel.enabled,
                    auto_restart: cfg.priority_channel.auto_restart,
                    channel_name: cfg.priority_channel.channel_name.clone(),
                    is_live: false,
                    platform: None,
                    title: None,
                    default_area: cfg.priority_channel.default_area,
                });
            }
        let nico_configured = crate::plugins::niconico_configured(&cfg.niconico)
            || !cfg.niconico.channel_name.trim().is_empty();
        if nico_configured {
            let nico_area_name = crate::plugins::get_area_name(cfg.niconico.area_v2)
                .unwrap_or_else(|| format!("未知分区 (ID: {})", cfg.niconico.area_v2));
            if cached_status.niconico.as_ref().is_some_and(|status| status.channel_id != cfg.niconico.channel_id) {
                cached_status.niconico = None;
            }
            if let Some(ref mut nico_status) = cached_status.niconico {
                nico_status.enable_monitor = cfg.niconico.enable_monitor;
                nico_status.channel_name = cfg.niconico.channel_name.clone();
                nico_status.channel_id = cfg.niconico.channel_id.clone();
                nico_status.area_id = cfg.niconico.area_v2;
                nico_status.area_name = nico_area_name;
                nico_status.quality = cfg.niconico.quality.clone();
                nico_status.crop_enabled = cfg.niconico.crop.is_some();
                nico_status.ffmpeg_cache_enabled = cfg.niconico.ffmpeg_cache.enabled;
                nico_status.ffmpeg_cache_latency_secs = cfg.niconico.ffmpeg_cache.latency_secs;
            } else {
                cached_status.niconico = Some(NicoStatus {
                    is_live: false,
                    enable_monitor: cfg.niconico.enable_monitor,
                    title: Some("-".to_string()),
                    channel_name: cfg.niconico.channel_name.clone(),
                    channel_id: cfg.niconico.channel_id.clone(),
                    live_id: None,
                    area_id: cfg.niconico.area_v2,
                    area_name: nico_area_name,
                    quality: cfg.niconico.quality.clone(),
                    crop_enabled: cfg.niconico.crop.is_some(),
                    ffmpeg_cache_enabled: cfg.niconico.ffmpeg_cache.enabled,
                    ffmpeg_cache_latency_secs: cfg.niconico.ffmpeg_cache.latency_secs,
                });
            }
        } else {
            cached_status.niconico = None;
        }

        });
    });
}

pub(crate) fn platform_channel_configured(channel_name: &str, channel_id: &str) -> bool {
    !channel_name.trim().is_empty() || !channel_id.trim().is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{
        BiliLive, ClusterConfig, Config, Credentials, FfmpegCache, PriorityChannel, Twitch, Youtube,
    };
    use std::collections::HashMap;

    fn status_cache_test_config() -> Config {
        Config {
            auto_cover: false,
            enable_anti_collision: false,
            interval: 60,
            bililive: BiliLive {
                enable_danmaku_command: true,
                room: 1,
                bili_rtmp_url: String::new(),
                bili_rtmp_key: String::new(),
                credentials: Credentials::default(),
            },
            twitch: Twitch {
                enable_monitor: false,
                channel_name: "tw-channel".to_string(),
                area_v2: 235,
                channel_id: "twid".to_string(),
                proxy_region: String::new(),
                quality: "best".to_string(),
                proxy: None,
                crop: None,
                ffmpeg_cache: FfmpegCache::default(),
            },
            youtube: Youtube {
                enable_monitor: false,
                channel_name: "yt-channel".to_string(),
                channel_id: "ytid".to_string(),
                area_v2: 235,
                quality: "best".to_string(),
                cookies_file: None,
                cookies_from_browser: None,
                proxy: None,
                deno_path: None,
                crop: None,
                ffmpeg_cache: FfmpegCache::default(),
            },
            holodex_api_key: None,
            holodex_jwt: None,
            holodex_jwt_refreshed_at: None,
            holodex_username: None,
            holodex_skip_jwt_verify: false,
            riot_api_key: None,
            enable_lol_monitor: false,
            lol_monitor_interval: None,
            anti_collision_list: HashMap::new(),
            priority_channel: PriorityChannel {
                enabled: true,
                auto_restart: true,
                channel_name: "priority-channel".to_string(),
                default_area: 235,
                youtube_channel_id: "priority-yt".to_string(),
                twitch_channel_id: "priority-tw".to_string(),
            },
            enable_youtube_monitor: false,
            enable_twitch_monitor: false,
            niconico: crate::config::Niconico::default(),
            cluster: ClusterConfig::default(),
        }
    }

    #[test]
    fn config_refresh_preserves_configured_monitor_toggles_for_webui() {
        update_status_cache(StatusData::default());

        let mut cfg = status_cache_test_config();
        refresh_status_cache_config_from(&cfg);

        let status = get_status_cache().expect("status cache should be initialized");
        assert!(status.bilibili.enable_danmaku_command);

        let youtube = status.youtube.expect("youtube status should be present");
        assert!(!youtube.enable_monitor);
        assert_eq!(youtube.channel_name, "yt-channel");
        assert_eq!(youtube.channel_id, "ytid");

        let twitch = status.twitch.expect("twitch status should be present");
        assert!(!twitch.enable_monitor);
        assert_eq!(twitch.channel_name, "tw-channel");
        assert_eq!(twitch.channel_id, "twid");

        let priority = status
            .priority_channel
            .expect("priority status should be present");
        assert!(priority.enabled);
        assert!(priority.auto_restart);
        assert_eq!(priority.channel_name, "priority-channel");
        assert_eq!(priority.default_area, 235);

        cfg.youtube.enable_monitor = true;
        cfg.twitch.enable_monitor = true;
        refresh_status_cache_config_from(&cfg);

        let status = get_status_cache().expect("status cache should be refreshed");
        assert!(status.bilibili.enable_danmaku_command);
        assert!(status.youtube.expect("youtube status").enable_monitor);
        assert!(status.twitch.expect("twitch status").enable_monitor);
        let priority = status.priority_channel.expect("priority status");
        assert!(priority.enabled);
        assert!(priority.auto_restart);
    }
}
