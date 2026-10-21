//! Stream-switch command and existing monitor/config orchestration.
use super::config::{
    niconico_monitor_reload_needed, twitch_monitor_reload_needed, youtube_monitor_reload_needed,
};
use super::ApiResponse;
use crate::cluster::{monitored_config_version, sync_monitored_config_after_change};
use crate::config::load_config;
use crate::logging::safe_text;
use crate::plugins::set_config_updated;
use crate::plugins::twitch::channel_id_from_url;
use crate::webui::state::{
    get_status_cache, refresh_status_cache_config_from, update_status_cache, NicoStatus, TwStatus,
    YtStatus,
};
use axum::{http::StatusCode, Json};
use serde::Deserialize;

// Switch to a Holodex stream
#[derive(Deserialize)]
pub struct SwitchStreamRequest {
    pub channel_id: String,
    pub area_id: Option<u64>,
    pub title: Option<String>,
    pub topic_id: Option<String>,
    pub status: Option<String>,
    #[serde(default)]
    pub platform: Option<String>,
    #[serde(default)]
    pub external_link: Option<String>,
    #[serde(default)]
    pub twitch_channel_id: Option<String>,
}

fn lookup_platform_id_from_channels(
    channels_json: &serde_json::Value,
    youtube_channel_id: &str,
    platform: &str,
) -> Option<String> {
    let channels = channels_json.get("channels")?.as_array()?;
    for channel in channels {
        let platforms = channel.get("platforms")?;
        if platforms.get("youtube").and_then(|v| v.as_str()) != Some(youtube_channel_id) {
            continue;
        }
        let id = platforms.get(platform).and_then(|v| v.as_str())?.trim();
        if !id.is_empty() {
            return Some(id.to_string());
        }
    }
    None
}

fn lookup_twitch_id_from_channels(
    channels_json: &serde_json::Value,
    youtube_channel_id: &str,
) -> Option<String> {
    lookup_platform_id_from_channels(channels_json, youtube_channel_id, "twitch")
}

fn lookup_niconico_id_from_channels(
    channels_json: &serde_json::Value,
    youtube_channel_id: &str,
) -> Option<String> {
    lookup_platform_id_from_channels(channels_json, youtube_channel_id, "niconico")
        .map(|id| crate::plugins::normalize_channel_id(&id))
        .filter(|id| !id.is_empty())
}

fn lookup_niconico_name_from_channels(
    channels_json: &serde_json::Value,
    youtube_channel_id: &str,
) -> Option<String> {
    let channels = channels_json.get("channels")?.as_array()?;
    for channel in channels {
        let platforms = channel.get("platforms")?;
        if platforms.get("youtube").and_then(|v| v.as_str()) != Some(youtube_channel_id) {
            continue;
        }
        let niconico_name = channel
            .get("niconico_name")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|name| !name.is_empty());
        let name = channel
            .get("name")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|name| !name.is_empty());
        return niconico_name.or(name).map(str::to_string);
    }
    None
}

/// Resolve the existing command precedence: explicit login, watch URL, roster.
fn twitch_channel_id(
    payload: &SwitchStreamRequest,
    channels_json: &serde_json::Value,
) -> Option<String> {
    payload
        .twitch_channel_id
        .as_deref()
        .filter(|id| !id.is_empty())
        .map(|s| s.to_string())
        .or_else(|| {
            payload
                .external_link
                .as_deref()
                .and_then(channel_id_from_url)
        })
        .or_else(|| lookup_twitch_id_from_channels(channels_json, &payload.channel_id))
}

pub async fn switch_stream(
    Json(payload): Json<SwitchStreamRequest>,
) -> Result<ApiResponse<()>, StatusCode> {
    tracing::debug!(
        "收到频道切换请求 channel={} area={}",
        safe_text(&payload.channel_id, 64),
        payload
            .area_id
            .map(|id| id.to_string())
            .unwrap_or_else(|| "沿用当前分区".into())
    );

    let mut cfg = match load_config().await {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("Failed to load config: {}", e);
            return Ok(ApiResponse {
                success: false,
                data: None,
                message: Some(format!("Failed to load config: {}", e)),
            });
        }
    };
    let log_node = safe_text(
        if cfg.cluster.node_id.is_empty() {
            "local"
        } else {
            &cfg.cluster.node_id
        },
        64,
    );
    let previous_cfg = cfg.clone();
    let old_monitored_config_version = monitored_config_version(&cfg);

    let channels_json: serde_json::Value = crate::storage::read_document("channels.json")
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    if let Err(error) = crate::storage::reject_legacy_channels(&channels_json) {
        tracing::error!("频道切换被拒绝: {error}");
        return Ok(ApiResponse {
            success: false,
            data: None,
            message: Some(error.to_string()),
        });
    }

    // Find channel name: channels[].platforms.youtube
    let mut channel_name = None;
    if let Some(channels) = channels_json.get("channels").and_then(|v| v.as_array()) {
        for channel in channels {
            if let Some(platforms) = channel.get("platforms") {
                if let Some(yt_id) = platforms.get("youtube").and_then(|v| v.as_str()) {
                    if yt_id == payload.channel_id {
                        channel_name = channel
                            .get("name")
                            .and_then(|v| v.as_str())
                            .map(|s| s.to_string());
                        break;
                    }
                }
            }
        }
    }

    // If channel name not found in channels.json, fetch from Holodex API
    if channel_name.is_none() {
        if let Some(ref api_key) = cfg.holodex_api_key {
            if !api_key.is_empty() {
                let url = format!("https://holodex.net/api/v2/channels/{}", payload.channel_id);
                let client = reqwest::Client::new();
                if let Ok(response) = client.get(&url).header("X-APIKEY", api_key).send().await {
                    if response.status().is_success() {
                        if let Ok(channel_data) = response.json::<serde_json::Value>().await {
                            channel_name = channel_data
                                .get("name")
                                .and_then(|v| v.as_str())
                                .map(|s| s.to_string());
                        }
                    }
                }
            }
        }
    }

    let channel_name = channel_name.unwrap_or_else(|| payload.channel_id.clone());

    let niconico_live_id = payload
        .external_link
        .as_deref()
        .and_then(crate::plugins::live_id_from_link);
    let is_niconico = payload
        .platform
        .as_deref()
        .is_some_and(|p| p.eq_ignore_ascii_case("niconico"))
        || niconico_live_id.is_some();
    let is_twitch = payload
        .platform
        .as_deref()
        .is_some_and(|p| p.eq_ignore_ascii_case("twitch"))
        || payload
            .external_link
            .as_deref()
            .and_then(channel_id_from_url)
            .is_some();

    if is_niconico {
        let niconico_channel_id =
            lookup_niconico_id_from_channels(&channels_json, &payload.channel_id)
                .unwrap_or_default();
        if niconico_channel_id.is_empty() && niconico_live_id.is_none() {
            return Ok(ApiResponse {
                success: false,
                data: None,
                message: Some(
                    "无法解析 Niconico 频道，请检查 external_link 或 channels.json".to_string(),
                ),
            });
        }

        let niconico_name = lookup_niconico_name_from_channels(&channels_json, &payload.channel_id)
            .unwrap_or_else(|| channel_name.clone());
        cfg.niconico.channel_id = niconico_channel_id;
        cfg.niconico.channel_name = niconico_name;
        cfg.niconico.live_id = niconico_live_id.clone().unwrap_or_default();
        if let Some(area_id) = payload.area_id {
            cfg.niconico.area_v2 = area_id;
        }

        if let Err(e) = crate::config::save_config(&mut cfg).await {
            tracing::error!("Failed to save config: {}", e);
            return Ok(ApiResponse {
                success: false,
                data: None,
                message: Some(format!("Failed to save config: {}", e)),
            });
        }

        tracing::info!(
            "{} [NC] {} 监听目标配置已更新；area={}",
            crate::logging::node_tag(&log_node),
            safe_text(&cfg.niconico.channel_name, 64),
            crate::plugins::danmaku::area_label(cfg.niconico.area_v2)
        );

        if niconico_monitor_reload_needed(&previous_cfg, &cfg) {
            set_config_updated();
        }
        refresh_status_cache_config_from(&cfg);

        let sync_message = if old_monitored_config_version != monitored_config_version(&cfg) {
            sync_monitored_config_after_change(&cfg).await
        } else {
            String::new()
        };

        let is_live = payload
            .status
            .as_ref()
            .filter(|s| !s.is_empty())
            .map(|s| s.to_lowercase() == "live")
            .unwrap_or(false);
        let stream_title = payload.title.unwrap_or_else(|| "未知标题".to_string());

        let mut current_cache = get_status_cache().unwrap_or_default();
        let nico_area_name = crate::plugins::get_area_name(cfg.niconico.area_v2)
            .unwrap_or_else(|| format!("未知分区 (ID: {})", cfg.niconico.area_v2));

        let (nico_channel_id, nico_channel_name) =
            crate::plugins::niconico_channel_identity(&cfg.niconico);
        current_cache.niconico = Some(NicoStatus {
            is_live,
            enable_monitor: cfg.niconico.enable_monitor,
            warning_held: crate::plugins::danmaku::source_warning_held(&cfg, "NC"),
            title: Some(stream_title),
            channel_name: nico_channel_name,
            channel_id: nico_channel_id,
            live_id: niconico_live_id,
            scheduled_start: None,
            quality: cfg.niconico.quality.clone(),
            area_id: cfg.niconico.area_v2,
            area_name: nico_area_name,
            crop_enabled: cfg.niconico.crop.is_some(),
            ffmpeg_cache_enabled: cfg.niconico.ffmpeg_cache.enabled,
            ffmpeg_cache_latency_secs: cfg.niconico.ffmpeg_cache.latency_secs,
        });

        update_status_cache(current_cache);

        return Ok(ApiResponse {
            success: true,
            data: Some(()),
            message: Some(format!(
                "已切换到 Niconico {} (分区: {}) - {}{}",
                cfg.niconico.channel_name,
                cfg.niconico.area_v2,
                if is_live { "直播中" } else { "预定直播" },
                sync_message
            )),
        });
    }

    if is_twitch {
        let twitch_channel_id = twitch_channel_id(&payload, &channels_json);

        let twitch_channel_id = match twitch_channel_id {
            Some(id) => id,
            None => {
                return Ok(ApiResponse {
                    success: false,
                    data: None,
                    message: Some(
                        "无法解析 Twitch 频道 ID，请检查 external_link 或 channels.json"
                            .to_string(),
                    ),
                });
            }
        };

        cfg.twitch.channel_id = twitch_channel_id.clone();
        cfg.twitch.channel_name = channel_name.clone();
        if let Some(area_id) = payload.area_id {
            cfg.twitch.area_v2 = area_id;
        }

        if let Err(e) = crate::config::save_config(&mut cfg).await {
            tracing::error!("Failed to save config: {}", e);
            return Ok(ApiResponse {
                success: false,
                data: None,
                message: Some(format!("Failed to save config: {}", e)),
            });
        }

        tracing::info!(
            "{} [TW] {} 监听目标配置已更新；area={}",
            crate::logging::node_tag(&log_node),
            safe_text(&cfg.twitch.channel_name, 64),
            crate::plugins::danmaku::area_label(cfg.twitch.area_v2)
        );

        if twitch_monitor_reload_needed(&previous_cfg, &cfg) {
            set_config_updated();
        }
        refresh_status_cache_config_from(&cfg);

        let sync_message = if old_monitored_config_version != monitored_config_version(&cfg) {
            sync_monitored_config_after_change(&cfg).await
        } else {
            String::new()
        };

        let is_live = payload
            .status
            .as_ref()
            .filter(|s| !s.is_empty())
            .map(|s| s.to_lowercase() == "live")
            .unwrap_or(false);
        let stream_title = payload.title.unwrap_or_else(|| "未知标题".to_string());
        let stream_topic = payload.topic_id.unwrap_or_else(|| "未知".to_string());

        let mut current_cache = get_status_cache().unwrap_or_default();
        let tw_area_name = crate::plugins::get_area_name(cfg.twitch.area_v2)
            .unwrap_or_else(|| format!("未知分区 (ID: {})", cfg.twitch.area_v2));

        current_cache.twitch = Some(TwStatus {
            is_live,
            enable_monitor: cfg.twitch.enable_monitor,
            warning_held: crate::plugins::danmaku::source_warning_held(&cfg, "TW"),
            title: Some(stream_title.clone()),
            game: Some(stream_topic),
            channel_name: cfg.twitch.channel_name.clone(),
            channel_id: cfg.twitch.channel_id.clone(),
            quality: cfg.twitch.quality.clone(),
            area_id: cfg.twitch.area_v2,
            area_name: tw_area_name,
            crop_enabled: cfg.twitch.crop.is_some(),
            ffmpeg_cache_enabled: cfg.twitch.ffmpeg_cache.enabled,
            ffmpeg_cache_latency_secs: cfg.twitch.ffmpeg_cache.latency_secs,
        });

        update_status_cache(current_cache);

        return Ok(ApiResponse {
            success: true,
            data: Some(()),
            message: Some(format!(
                "已切换到 Twitch {} (分区: {}) - {}{}",
                cfg.twitch.channel_name,
                cfg.twitch.area_v2,
                if is_live { "直播中" } else { "预定直播" },
                sync_message
            )),
        });
    }

    // Update config
    cfg.youtube.channel_id = payload.channel_id.clone();
    cfg.youtube.channel_name = channel_name;

    if let Some(area_id) = payload.area_id {
        cfg.youtube.area_v2 = area_id;
    }

    // Save config as JSON
    if let Err(e) = crate::config::save_config(&mut cfg).await {
        tracing::error!("Failed to save config: {}", e);
        return Ok(ApiResponse {
            success: false,
            data: None,
            message: Some(format!("Failed to save config: {}", e)),
        });
    }

    tracing::info!(
        "{} [YT] {} 监听目标配置已更新；area={}",
        crate::logging::node_tag(&log_node),
        safe_text(&cfg.youtube.channel_name, 64),
        crate::plugins::danmaku::area_label(cfg.youtube.area_v2)
    );

    if youtube_monitor_reload_needed(&previous_cfg, &cfg) {
        set_config_updated();
    }
    refresh_status_cache_config_from(&cfg);

    let sync_message = if old_monitored_config_version != monitored_config_version(&cfg) {
        sync_monitored_config_after_change(&cfg).await
    } else {
        String::new()
    };

    // Use stream data from Holodex monitor (passed from frontend)
    let is_live = payload
        .status
        .as_ref()
        .filter(|s| !s.is_empty()) // Filter out empty strings
        .map(|s| s.to_lowercase() == "live")
        .unwrap_or(false);
    let stream_title = payload.title.unwrap_or_else(|| "未知标题".to_string());
    let stream_topic = payload.topic_id.unwrap_or_else(|| "未知".to_string());

    // Update YouTube status cache immediately with stream data from Holodex monitor
    let mut current_cache = get_status_cache().unwrap_or_default();

    let yt_area_name = crate::plugins::get_area_name(cfg.youtube.area_v2)
        .unwrap_or_else(|| format!("未知分区 (ID: {})", cfg.youtube.area_v2));

    current_cache.youtube = Some(YtStatus {
        is_live, // From Holodex monitor data
        enable_monitor: cfg.youtube.enable_monitor,
        warning_held: crate::plugins::danmaku::source_warning_held(&cfg, "YT"),
        title: Some(stream_title.clone()),
        topic: Some(stream_topic),
        channel_name: cfg.youtube.channel_name.clone(),
        channel_id: cfg.youtube.channel_id.clone(),
        quality: cfg.youtube.quality.clone(),
        area_id: cfg.youtube.area_v2,
        area_name: yt_area_name,
        crop_enabled: cfg.youtube.crop.is_some(),
        ffmpeg_cache_enabled: cfg.youtube.ffmpeg_cache.enabled,
        ffmpeg_cache_latency_secs: cfg.youtube.ffmpeg_cache.latency_secs,
    });

    update_status_cache(current_cache);

    Ok(ApiResponse {
        success: true,
        data: Some(()),
        message: Some(format!(
            "已切换到 {} (分区: {}) - {}{}",
            cfg.youtube.channel_name,
            cfg.youtube.area_v2,
            if is_live { "直播中" } else { "预定直播" },
            sync_message
        )),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn niconico_ids_and_names_are_read_from_the_youtube_channel() {
        let json = serde_json::json!({
            "channels": [{
                "name": "ぶいすぽっ!【公式】",
                "niconico_name": "ぶいすぽ激ロー",
                "platforms": {
                    "youtube": "UCvspo",
                    "niconico": " https://ch.nicovideo.jp/vspo "
                }
            }]
        });
        assert_eq!(
            lookup_niconico_id_from_channels(&json, "UCvspo").as_deref(),
            Some("vspo")
        );
        assert_eq!(
            lookup_niconico_name_from_channels(&json, "UCvspo").as_deref(),
            Some("ぶいすぽ激ロー")
        );
        assert_eq!(lookup_niconico_id_from_channels(&json, "UCother"), None);
    }

    #[test]
    fn switch_request_keeps_post_keys_and_resolves_twitch_login_precedence() {
        let channels = serde_json::json!({"channels": [{
            "name": "Example", "platforms": {"youtube": "UCexample", "twitch": "roster_login"}
        }]});
        let link = " HTTPS://WWW.TWITCH.TV/Example_Channel_001/videos?x=1#live ";
        let mut request: SwitchStreamRequest = serde_json::from_value(serde_json::json!({
            "channel_id": "UCexample", "area_id": 123, "title": "Example title",
            "topic_id": "Example topic", "status": "live", "platform": "twitch",
            "external_link": link, "twitch_channel_id": null
        }))
        .unwrap();
        assert_eq!(request.topic_id.as_deref(), Some("Example topic"));
        assert_eq!(request.external_link.as_deref(), Some(link));
        assert_eq!(
            twitch_channel_id(&request, &channels).as_deref(),
            Some("example_channel_001")
        );
        request.twitch_channel_id = Some("explicit_login".to_string());
        assert_eq!(
            twitch_channel_id(&request, &channels).as_deref(),
            Some("explicit_login")
        );
        request.twitch_channel_id = None;
        request.external_link = Some("https://example.com/twitch.tv/unrelated".to_string());
        assert_eq!(
            twitch_channel_id(&request, &channels).as_deref(),
            Some("roster_login")
        );
        assert_eq!(
            twitch_channel_id(&request, &serde_json::json!({"channels": []})),
            None
        );
    }
}
