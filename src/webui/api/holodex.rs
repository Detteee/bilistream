use super::*;

// Holodex API - Get live/upcoming streams
#[derive(Serialize, Debug)]
pub struct HolodexStreamWithArea {
    pub id: String,
    pub title: String,
    pub stream_type: String,
    pub topic_id: Option<String>,
    pub status: String,
    pub start_scheduled: Option<String>,
    pub start_actual: Option<String>,
    pub available_at: Option<String>,
    pub published_at: Option<String>,
    pub live_viewers: Option<i32>,
    pub channel_id: String,
    pub channel_name: String,
    pub channel_photo: Option<String>,
    pub suggested_area_id: Option<u64>,
    pub suggested_area_name: Option<String>,
    pub is_placeholder: bool,
    pub placeholder_type: Option<String>,
    pub external_link: Option<String>,
    pub thumbnail: Option<String>,
}

/// Holodex Favorites/Home drop streams that never got `start_actual` once the
/// schedule is more than two hours old (`!start_actual && now > scheduled + 2h`).
/// Rows YouTube classified skip this: its `actualStartTime` is authoritative.
fn holodex_has_start_actual(stream: &crate::plugins::holodex::HolodexStream) -> bool {
    stream
        .start_actual
        .as_deref()
        .is_some_and(|start| !start.is_empty())
}

fn holodex_scheduled_at(
    stream: &crate::plugins::holodex::HolodexStream,
) -> Option<chrono::DateTime<chrono::Utc>> {
    chrono::DateTime::parse_from_rfc3339(stream.start_scheduled.as_deref()?)
        .ok()
        .map(|scheduled| scheduled.with_timezone(&chrono::Utc))
}

fn holodex_unconfirmed_and_stale(
    stream: &crate::plugins::holodex::HolodexStream,
    now: chrono::DateTime<chrono::Utc>,
) -> bool {
    if stream.yt_confirmed || holodex_has_start_actual(stream) {
        return false;
    }
    holodex_scheduled_at(stream)
        .is_some_and(|scheduled| now > scheduled + chrono::Duration::hours(2))
}

pub(crate) fn filter_holodex_streams(
    streams: Vec<crate::plugins::holodex::HolodexStream>,
    allowed_channel_ids: HashSet<String>,
) -> Vec<crate::plugins::holodex::HolodexStream> {
    filter_holodex_streams_at(streams, allowed_channel_ids, chrono::Utc::now())
}

/// Roster membership and Holodex avatars use the YouTube channel id, including
/// Niconico/Twitch placeholders. Upcoming rows are hidden only when *that
/// platform* is already live, so 激ロー and the official YouTube relay both
/// stay on the panel.
fn live_slot_key(stream: &crate::plugins::holodex::HolodexStream) -> String {
    if stream
        .link
        .as_deref()
        .and_then(crate::plugins::live_id_from_link)
        .is_some()
    {
        return format!("NC:{}", stream.channel.id);
    }
    if let Some(login) = stream
        .link
        .as_deref()
        .and_then(parse_twitch_login_from_link)
    {
        return format!("TW:{login}");
    }
    format!("YT:{}", stream.channel.id)
}

fn filter_holodex_streams_at(
    streams: Vec<crate::plugins::holodex::HolodexStream>,
    allowed_channel_ids: HashSet<String>,
    now: chrono::DateTime<chrono::Utc>,
) -> Vec<crate::plugins::holodex::HolodexStream> {
    let mut live_slots: HashSet<String> = HashSet::new();
    for stream in &streams {
        if stream.status == "live" && !holodex_unconfirmed_and_stale(stream, now) {
            live_slots.insert(live_slot_key(stream));
        }
    }

    let thirty_hours_later = now + chrono::Duration::hours(30);

    streams
        .into_iter()
        .filter(|stream| {
            if !allowed_channel_ids.contains(&stream.channel.id) {
                return false;
            }

            if holodex_unconfirmed_and_stale(stream, now) {
                return false;
            }

            if stream.status == "live" {
                return true;
            }

            if live_slots.contains(&live_slot_key(stream)) {
                return false;
            }

            if stream.status == "upcoming" {
                if let Some(scheduled_utc) = holodex_scheduled_at(stream) {
                    return scheduled_utc <= thirty_hours_later;
                }
                return true;
            }

            true
        })
        .collect()
}

pub(crate) fn map_holodex_streams_with_area(
    streams: Vec<crate::plugins::holodex::HolodexStream>,
) -> Vec<HolodexStreamWithArea> {
    streams
        .into_iter()
        .map(|stream| {
            let is_placeholder = stream.stream_type == "placeholder";
            let (suggested_area_id, suggested_area_name) =
                crate::plugins::suggest_area_from_stream(stream.topic_id.as_deref(), &stream.title);

            HolodexStreamWithArea {
                id: stream.id,
                title: stream.title,
                stream_type: stream.stream_type,
                topic_id: stream.topic_id,
                status: stream.status,
                start_scheduled: stream.start_scheduled,
                start_actual: stream.start_actual,
                available_at: stream.available_at,
                published_at: stream.published_at,
                live_viewers: stream.live_viewers,
                channel_id: stream.channel.id,
                channel_name: stream.channel.name,
                channel_photo: stream.channel.photo.filter(|p| !p.is_empty()),
                suggested_area_id,
                suggested_area_name,
                is_placeholder,
                placeholder_type: stream.placeholder_type,
                external_link: stream.link,
                thumbnail: stream.thumbnail,
            }
        })
        .collect()
}

pub(crate) async fn ensure_holodex_username(cfg: &mut crate::config::Config, jwt: &str) {
    if cfg
        .holodex_username
        .as_ref()
        .is_some_and(|name| !name.is_empty())
    {
        return;
    }

    let Some(api_key) = cfg.holodex_api_key.as_deref().filter(|key| !key.is_empty()) else {
        return;
    };

    let Ok(Some(refresh)) = crate::plugins::holodex::refresh_holodex_jwt(api_key, jwt).await else {
        return;
    };

    let mut should_save = false;
    if let Some(name) = refresh.username.filter(|name| !name.is_empty()) {
        cfg.holodex_username = Some(name);
        should_save = true;
    }
    if let Some(new_jwt) = refresh.jwt.filter(|token| !token.is_empty()) {
        cfg.holodex_jwt = Some(new_jwt);
        cfg.holodex_jwt_refreshed_at = Some(crate::plugins::holodex::holodex_unix_now());
        should_save = true;
    }
    if should_save {
        let _ = crate::config::save_config(cfg).await;
    }
}

pub(crate) async fn apply_holodex_jwt_sync(
    cfg: &mut crate::config::Config,
) -> Result<(String, bool), String> {
    let jwt = cfg
        .holodex_jwt
        .as_ref()
        .filter(|token| !token.is_empty())
        .ok_or_else(|| "Holodex JWT not configured".to_string())?
        .clone();

    ensure_holodex_username(cfg, &jwt).await;

    if cfg.holodex_skip_jwt_verify {
        return Ok((jwt, false));
    }

    let api_key = cfg
        .holodex_api_key
        .as_ref()
        .filter(|key| !key.is_empty())
        .ok_or_else(|| "Holodex API key not configured".to_string())?
        .clone();

    let sync = crate::plugins::holodex::sync_holodex_jwt_if_needed(
        &api_key,
        &jwt,
        cfg.holodex_jwt_refreshed_at,
        cfg.holodex_username.clone(),
    )
    .await?;

    let mut should_save = false;
    if sync.jwt != jwt {
        cfg.holodex_jwt = Some(sync.jwt.clone());
        should_save = true;
    }
    if sync.refreshed_at != cfg.holodex_jwt_refreshed_at {
        cfg.holodex_jwt_refreshed_at = sync.refreshed_at;
        should_save = true;
    }
    if sync.username.is_some() && sync.username != cfg.holodex_username {
        cfg.holodex_username = sync.username.clone();
        should_save = true;
    }
    if should_save {
        if let Err(e) = crate::config::save_config(cfg).await {
            tracing::warn!("Failed to save Holodex JWT state: {}", e);
        }
    }

    Ok((sync.jwt, sync.token_rotated))
}

pub async fn api_holodex_auth_status() -> Json<serde_json::Value> {
    let mut cfg = match load_config().await {
        Ok(c) => c,
        Err(e) => {
            return Json(json!({
                "success": false,
                "message": format!("Failed to load config: {}", e)
            }));
        }
    };

    if cfg.holodex_api_key.as_ref().is_none_or(|k| k.is_empty()) {
        return Json(json!({
            "success": true,
            "data": {
                "logged_in": false,
                "message": "Holodex API key not configured"
            }
        }));
    }

    let jwt = match cfg.holodex_jwt.as_ref().filter(|j| !j.is_empty()) {
        Some(jwt) => jwt.clone(),
        None => {
            return Json(json!({
                "success": true,
                "data": {
                    "logged_in": false,
                    "skip_jwt_verify": cfg.holodex_skip_jwt_verify
                }
            }));
        }
    };

    if cfg.holodex_skip_jwt_verify {
        ensure_holodex_username(&mut cfg, &jwt).await;
        return Json(json!({
            "success": true,
            "data": {
                "logged_in": true,
                "username": cfg.holodex_username,
                "skip_jwt_verify": true,
                "jwt_refreshed": false
            }
        }));
    }

    let (active_jwt, jwt_refreshed) = match apply_holodex_jwt_sync(&mut cfg).await {
        Ok(result) => result,
        Err(e) => {
            if crate::plugins::holodex::holodex_jwt_is_expired(&jwt) {
                return Json(json!({
                    "success": true,
                    "data": {
                        "logged_in": false,
                        "expired": true,
                        "message": e
                    }
                }));
            }
            tracing::warn!("Holodex JWT sync skipped: {}", e);
            (jwt, false)
        }
    };

    if crate::plugins::holodex::holodex_jwt_is_expired(&active_jwt) {
        return Json(json!({
            "success": true,
            "data": {
                "logged_in": false,
                "expired": true
            }
        }));
    }

    Json(json!({
        "success": true,
        "data": {
            "logged_in": true,
            "username": cfg.holodex_username,
            "skip_jwt_verify": cfg.holodex_skip_jwt_verify,
            "jwt_refreshed": jwt_refreshed
        }
    }))
}

/// YouTube Data API key pool and uploads-playlist polling for the settings
/// view. Keys appear only as fingerprints.
pub async fn api_youtube_key_status() -> Json<serde_json::Value> {
    let cfg = match load_config().await {
        Ok(c) => c,
        Err(e) => {
            return Json(json!({
                "success": false,
                "message": format!("Failed to load config: {}", e)
            }));
        }
    };
    let keys = cfg.youtube_api_keys();
    if keys.is_empty() {
        return Json(json!({ "success": true, "data": { "configured": false } }));
    }
    let mut data = crate::plugins::youtube_data::key_pool_status(&keys);
    data["configured"] = json!(true);
    data["playlist"] = json!(crate::plugins::youtube_discovery::playlist_status());
    data["websub"] = json!(crate::plugins::youtube_websub::status());
    Json(json!({ "success": true, "data": data }))
}

#[derive(Deserialize)]
pub struct HolodexStreamsQuery {
    /// When true, fetch account favorites (requires JWT). Otherwise uses channels.json.
    #[serde(default)]
    favorites: bool,
    /// When true, fetch Holodex now instead of waiting for its cadence.
    #[serde(default)]
    force: bool,
}

pub async fn api_get_holodex_streams(
    Query(query): Query<HolodexStreamsQuery>,
) -> Json<serde_json::Value> {
    use crate::webui::holodex_list::{current, ListKind};
    let kind = if query.favorites {
        ListKind::Favorites
    } else {
        ListKind::Channels
    };
    match current(kind, query.force).await {
        Ok(snapshot) => Json(json!({
            "success": true,
            "source": kind.source(),
            "data": map_holodex_streams_with_area(snapshot.rows.clone())
        })),
        Err(message) => Json(json!({ "success": false, "message": message })),
    }
}

// Switch to a Holodex stream
#[derive(Deserialize)]
pub struct SwitchToHolodexStream {
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

pub(crate) fn parse_twitch_login_from_link(link: &str) -> Option<String> {
    let link = link.trim();
    for prefix in [
        "https://www.twitch.tv/",
        "https://twitch.tv/",
        "http://www.twitch.tv/",
        "http://twitch.tv/",
    ] {
        if let Some(rest) = link.strip_prefix(prefix) {
            let login = rest.split(&['/', '?', '#'][..]).next().unwrap_or("").trim();
            if !login.is_empty() {
                return Some(login.to_string());
            }
        }
    }
    None
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

pub(crate) fn lookup_twitch_id_from_channels(
    channels_json: &serde_json::Value,
    youtube_channel_id: &str,
) -> Option<String> {
    lookup_platform_id_from_channels(channels_json, youtube_channel_id, "twitch")
}

pub(crate) fn lookup_niconico_id_from_channels(
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

pub async fn switch_to_holodex_stream(
    Json(payload): Json<SwitchToHolodexStream>,
) -> Result<ApiResponse<()>, StatusCode> {
    tracing::info!(
        "Switching to Holodex channel: {} (area: {:?})",
        payload.channel_id,
        payload.area_id
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
    let previous_cfg = cfg.clone();
    let old_monitored_config_version = monitored_config_version(&cfg);

    let channels_json: serde_json::Value = crate::storage::read_json("channels.json")
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    // Find channel name - try both new and old formats
    let mut channel_name = None;

    // Try new format: channels[].platforms.youtube
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

    // Try old format if not found: YT_channels[].channel_id
    if channel_name.is_none() {
        if let Some(yt_channels) = channels_json.get("YT_channels").and_then(|v| v.as_array()) {
            for channel in yt_channels {
                if let Some(id) = channel.get("channel_id").and_then(|v| v.as_str()) {
                    if id == payload.channel_id {
                        channel_name = channel
                            .get("channel_name")
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
            .and_then(parse_twitch_login_from_link)
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
            "Successfully switched to Niconico channel: {} ({})",
            cfg.niconico.channel_name,
            cfg.niconico.channel_id
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

        current_cache.niconico = Some(NicoStatus {
            is_live,
            enable_monitor: cfg.niconico.enable_monitor,
            title: Some(stream_title),
            channel_name: cfg.niconico.channel_name.clone(),
            channel_id: cfg.niconico.channel_id.clone(),
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
        let twitch_channel_id = payload
            .twitch_channel_id
            .as_deref()
            .filter(|id| !id.is_empty())
            .map(|s| s.to_string())
            .or_else(|| {
                payload
                    .external_link
                    .as_deref()
                    .and_then(parse_twitch_login_from_link)
            })
            .or_else(|| lookup_twitch_id_from_channels(&channels_json, &payload.channel_id));

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
            "Successfully switched to Twitch channel: {} ({})",
            cfg.twitch.channel_name,
            cfg.twitch.channel_id
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
        "Successfully switched to channel: {} ({})",
        cfg.youtube.channel_name,
        cfg.youtube.channel_id
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
    use crate::plugins::holodex::{HolodexChannel, HolodexStream};
    use chrono::{DateTime, Utc};

    fn at(rfc3339: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(rfc3339)
            .unwrap()
            .with_timezone(&Utc)
    }

    fn stream(
        id: &str,
        status: &str,
        scheduled: Option<&str>,
        start_actual: Option<&str>,
    ) -> HolodexStream {
        HolodexStream {
            id: id.to_string(),
            title: format!("{id} title"),
            stream_type: "stream".to_string(),
            topic_id: None,
            published_at: None,
            available_at: scheduled.map(str::to_string),
            status: status.to_string(),
            start_scheduled: scheduled.map(str::to_string),
            start_actual: start_actual.map(str::to_string),
            live_viewers: None,
            channel: HolodexChannel {
                id: "channel-id".to_string(),
                ..Default::default()
            },
            link: None,
            thumbnail: None,
            placeholder_type: None,
            yt_confirmed: false,
        }
    }

    fn ids() -> HashSet<String> {
        HashSet::from(["channel-id".to_string()])
    }

    fn kept(streams: Vec<HolodexStream>, now: DateTime<Utc>) -> Vec<String> {
        filter_holodex_streams_at(streams, ids(), now)
            .into_iter()
            .map(|stream| stream.id)
            .collect()
    }

    #[test]
    fn a_live_stream_with_start_actual_is_kept() {
        let now = at("2026-09-02T15:00:00Z");
        let streams = vec![stream(
            "confirmed",
            "live",
            Some("2026-09-01T14:00:00Z"),
            Some("2026-09-01T14:03:00Z"),
        )];
        assert_eq!(kept(streams, now), vec!["confirmed"]);
    }

    #[test]
    fn a_live_stream_without_start_actual_is_kept_inside_the_two_hour_grace() {
        let now = at("2026-09-01T15:30:00Z");
        let streams = vec![stream("fresh", "live", Some("2026-09-01T14:00:00Z"), None)];
        assert_eq!(kept(streams, now), vec!["fresh"]);
    }

    #[test]
    fn a_live_stream_without_start_actual_is_kept_at_exactly_two_hours() {
        let now = at("2026-09-01T16:00:00Z");
        let streams = vec![stream("edge", "live", Some("2026-09-01T14:00:00Z"), None)];
        assert_eq!(kept(streams, now), vec!["edge"]);
    }

    #[test]
    fn an_empty_start_actual_counts_as_missing() {
        let now = at("2026-09-02T15:00:00Z");
        let streams = vec![stream(
            "empty",
            "live",
            Some("2026-09-01T14:00:00Z"),
            Some(""),
        )];
        assert!(kept(streams, now).is_empty());
    }

    #[test]
    fn a_live_stream_without_a_schedule_is_kept() {
        let now = at("2026-09-02T15:00:00Z");
        let streams = vec![stream("no-sched", "live", None, None)];
        assert_eq!(kept(streams, now), vec!["no-sched"]);
    }

    #[test]
    fn a_stale_unconfirmed_upcoming_is_dropped() {
        let now = at("2026-09-01T17:00:00Z");
        let streams = vec![stream(
            "waiting-room",
            "upcoming",
            Some("2026-09-01T14:00:00Z"),
            None,
        )];
        assert!(kept(streams, now).is_empty());
    }

    #[test]
    fn a_hung_live_does_not_hide_a_later_upcoming_on_the_same_channel() {
        let now = at("2026-09-02T15:00:00Z");
        let streams = vec![
            stream("hung", "live", Some("2026-09-01T14:00:00Z"), None),
            stream("later", "upcoming", Some("2026-09-02T20:00:00Z"), None),
        ];
        assert_eq!(kept(streams, now), vec!["later"]);
    }

    #[test]
    fn a_youtube_confirmed_upcoming_past_its_schedule_is_kept() {
        let now = at("2026-09-02T15:00:00Z");
        let mut late = stream("late", "upcoming", Some("2026-09-01T14:00:00Z"), None);
        late.yt_confirmed = true;
        assert_eq!(kept(vec![late], now), vec!["late"]);
    }

    fn niconico_live(id: &str) -> HolodexStream {
        let mut stream = stream(
            id,
            "live",
            Some("2026-09-02T14:00:00Z"),
            Some("2026-09-02T14:00:00Z"),
        );
        stream.stream_type = "placeholder".to_string();
        stream.link = Some("https://live.nicovideo.jp/watch/lv351182284".to_string());
        stream
    }

    fn niconico_upcoming(id: &str) -> HolodexStream {
        let mut stream = stream(id, "upcoming", Some("2026-09-02T20:00:00Z"), None);
        stream.stream_type = "placeholder".to_string();
        stream.link = Some("https://live.nicovideo.jp/watch/lv351230205".to_string());
        stream
    }

    #[test]
    fn a_niconico_live_and_the_same_channel_s_youtube_live_are_both_kept() {
        let now = at("2026-09-02T15:00:00Z");
        let streams = vec![
            niconico_live("niconico-lv1"),
            stream(
                "yt-live",
                "live",
                Some("2026-09-02T14:05:00Z"),
                Some("2026-09-02T14:05:00Z"),
            ),
        ];
        assert_eq!(kept(streams, now), vec!["niconico-lv1", "yt-live"]);
    }

    #[test]
    fn a_niconico_live_does_not_hide_the_same_channel_s_youtube_upcoming() {
        let now = at("2026-09-02T15:00:00Z");
        let streams = vec![
            niconico_live("niconico-lv1"),
            stream("yt-up", "upcoming", Some("2026-09-02T20:00:00Z"), None),
        ];
        assert_eq!(kept(streams, now), vec!["niconico-lv1", "yt-up"]);
    }

    #[test]
    fn a_youtube_live_does_not_hide_the_same_channel_s_niconico_upcoming() {
        let now = at("2026-09-02T15:00:00Z");
        let streams = vec![
            stream(
                "yt-live",
                "live",
                Some("2026-09-02T14:05:00Z"),
                Some("2026-09-02T14:05:00Z"),
            ),
            niconico_upcoming("niconico-lv2"),
        ];
        assert_eq!(kept(streams, now), vec!["yt-live", "niconico-lv2"]);
    }

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
}
