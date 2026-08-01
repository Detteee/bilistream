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

#[derive(Default)]
pub(crate) struct OrderedChannelIds {
    ordered: Vec<String>,
    seen: HashSet<String>,
}

impl OrderedChannelIds {
    pub(crate) fn insert(&mut self, channel_id: &str) -> bool {
        let channel_id = channel_id.trim();
        if channel_id.is_empty() || !self.seen.insert(channel_id.to_string()) {
            return false;
        }
        self.ordered.push(channel_id.to_string());
        true
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.ordered.is_empty()
    }

    pub(crate) fn into_parts(self) -> (Vec<String>, HashSet<String>) {
        (self.ordered, self.seen)
    }
}

pub(crate) fn filter_holodex_streams(
    streams: Vec<crate::plugins::holodex::HolodexStream>,
    allowed_channel_ids: HashSet<String>,
) -> Vec<crate::plugins::holodex::HolodexStream> {
    let mut live_channels: HashSet<String> = HashSet::new();
    for stream in &streams {
        if stream.status == "live" {
            live_channels.insert(stream.channel.id.clone());
        }
    }

    let now = chrono::Utc::now();
    let thirty_hours_later = now + chrono::Duration::hours(30);

    streams
        .into_iter()
        .filter(|stream| {
            if !allowed_channel_ids.contains(&stream.channel.id) {
                return false;
            }

            if stream.status == "live" {
                return true;
            }

            if live_channels.contains(&stream.channel.id) {
                return false;
            }

            if stream.status == "upcoming" {
                if let Some(ref scheduled_time) = stream.start_scheduled {
                    if let Ok(scheduled) = chrono::DateTime::parse_from_rfc3339(scheduled_time) {
                        let scheduled_utc = scheduled.with_timezone(&chrono::Utc);
                        return scheduled_utc <= thirty_hours_later;
                    }
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
            let title_for_detection = if let Some(ref topic) = stream.topic_id {
                format!("{} {}", topic, stream.title)
            } else {
                stream.title.clone()
            };

            let mut suggested_area_id = 235;
            if let Some(ref topic) = stream.topic_id {
                let topic_lower = topic.to_lowercase();
                if topic_lower.contains("freechat")
                    || topic_lower.contains("talk")
                    || topic_lower.contains("singing")
                {
                    suggested_area_id = 530;
                }
                if topic_lower.contains("talk")
                    || topic_lower.contains("zatsudan")
                    || topic_lower.contains("雑談")
                    || topic_lower.contains("just chatting")
                {
                    suggested_area_id = 646;
                }
            }

            if suggested_area_id == 235 {
                suggested_area_id =
                    crate::plugins::check_area_id_with_title(&title_for_detection, 235);
            }

            let suggested_area_name = if suggested_area_id != 235 {
                crate::plugins::get_area_name(suggested_area_id)
            } else {
                None
            };

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
                suggested_area_id: if suggested_area_id != 235 {
                    Some(suggested_area_id)
                } else {
                    None
                },
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

#[derive(Deserialize)]
pub struct HolodexStreamsQuery {
    /// When true, fetch account favorites (requires JWT). Otherwise uses channels.json.
    #[serde(default)]
    favorites: bool,
}

pub async fn api_get_holodex_streams(
    Query(query): Query<HolodexStreamsQuery>,
) -> Json<serde_json::Value> {
    let mut cfg = match load_config().await {
        Ok(c) => c,
        Err(e) => {
            return Json(json!({
                "success": false,
                "message": format!("Failed to load config: {}", e)
            }));
        }
    };

    let api_key = match cfg.holodex_api_key.as_ref().filter(|k| !k.is_empty()) {
        Some(key) => key.clone(),
        None => {
            return Json(json!({
                "success": false,
                "message": "Holodex API key not configured"
            }));
        }
    };

    // Favorites mode: JWT + includePlaceholder (YouTube + Twitch external streams)
    if query.favorites {
        if cfg.holodex_jwt.as_ref().is_none_or(|j| j.is_empty()) {
            return Json(json!({
                "success": false,
                "message": "Holodex JWT required for favorites mode"
            }));
        }

        let active_jwt = match apply_holodex_jwt_sync(&mut cfg).await {
            Ok((jwt, _)) => jwt,
            Err(e) => {
                return Json(json!({
                    "success": false,
                    "message": format!("Failed to refresh Holodex JWT: {}", e)
                }));
            }
        };

        let (fav_ids, streams) = match crate::plugins::holodex::get_holodex_favorites_live(
            &api_key,
            &active_jwt,
        )
        .await
        {
            Ok(result) => result,
            Err(e) => {
                return Json(json!({
                    "success": false,
                    "message": format!("Failed to fetch Holodex favorites: {}", e)
                }));
            }
        };

        let filtered_streams = filter_holodex_streams(streams, fav_ids);
        let streams_with_area = map_holodex_streams_with_area(filtered_streams);

        return Json(json!({
            "success": true,
            "source": "favorites",
            "data": streams_with_area
        }));
    }

    // channels.json preset list (YouTube + Twitch placeholders via ?channels=...&includePlaceholder=true)
    let mut channel_ids = OrderedChannelIds::default();

    // Load channels.json for all channels
    let channels_path = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|p| p.join("channels.json")));

    if let Some(path) = channels_path {
        if let Ok(channels_content) = tokio::fs::read_to_string(path).await {
            if let Ok(channels_json) = serde_json::from_str::<serde_json::Value>(&channels_content)
            {
                // Try new format: channels[].platforms.youtube
                if let Some(channels) = channels_json.get("channels").and_then(|v| v.as_array()) {
                    for channel in channels {
                        if let Some(platforms) = channel.get("platforms") {
                            if let Some(yt_id) = platforms.get("youtube").and_then(|v| v.as_str()) {
                                channel_ids.insert(yt_id);
                            }
                        }
                    }
                }
                // Try old format: YT_channels[].channel_id (for backward compatibility)
                else if let Some(yt_channels) =
                    channels_json.get("YT_channels").and_then(|v| v.as_array())
                {
                    for channel in yt_channels {
                        if let Some(id) = channel.get("channel_id").and_then(|v| v.as_str()) {
                            channel_ids.insert(id);
                        }
                    }
                }
            }
        }
    }

    // Also add the currently configured channel if not already in list
    channel_ids.insert(&cfg.youtube.channel_id);

    if channel_ids.is_empty() {
        return Json(json!({
            "success": false,
            "message": "No YouTube channels configured"
        }));
    }

    let (channel_ids, queried_channels) = channel_ids.into_parts();

    // Call Holodex directly for configured YouTube channels.
    let streams = match crate::plugins::holodex::get_holodex_streams(channel_ids, true).await {
        Ok(s) => s,
        Err(e) => {
            return Json(json!({
                "success": false,
                "message": format!("Failed to fetch from Holodex: {}", e)
            }));
        }
    };

    let filtered_streams = filter_holodex_streams(streams, queried_channels);
    let streams_with_area = map_holodex_streams_with_area(filtered_streams);

    Json(json!({
        "success": true,
        "source": "channels",
        "data": streams_with_area
    }))
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

pub(crate) fn lookup_twitch_id_from_channels(
    channels_json: &serde_json::Value,
    youtube_channel_id: &str,
) -> Option<String> {
    if let Some(channels) = channels_json.get("channels").and_then(|v| v.as_array()) {
        for channel in channels {
            if let Some(platforms) = channel.get("platforms") {
                if platforms.get("youtube").and_then(|v| v.as_str()) == Some(youtube_channel_id) {
                    if let Some(twitch_id) = platforms.get("twitch").and_then(|v| v.as_str()) {
                        if !twitch_id.is_empty() {
                            return Some(twitch_id.to_string());
                        }
                    }
                }
            }
        }
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

    // Get channel info from channels.json
    let channels_path = std::env::current_exe()
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .with_file_name("channels.json");

    let channels_content = match tokio::fs::read_to_string(&channels_path).await {
        Ok(c) => c,
        Err(e) => {
            return Ok(ApiResponse {
                success: false,
                data: None,
                message: Some(format!("Failed to read channels.json: {}", e)),
            });
        }
    };

    let channels_json: serde_json::Value = match serde_json::from_str(&channels_content) {
        Ok(j) => j,
        Err(e) => {
            return Ok(ApiResponse {
                success: false,
                data: None,
                message: Some(format!("Failed to parse channels.json: {}", e)),
            });
        }
    };

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

    let is_twitch = payload
        .platform
        .as_deref()
        .map(|p| p.eq_ignore_ascii_case("twitch"))
        .unwrap_or(false)
        || payload.external_link.is_some();

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

        if let Err(e) = crate::config::save_config(&cfg).await {
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
                "已切换到 Twitch {} (分区: {}) - {}",
                cfg.twitch.channel_name,
                cfg.twitch.area_v2,
                if is_live { "直播中" } else { "预定直播" }
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
    if let Err(e) = crate::config::save_config(&cfg).await {
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
            "已切换到 {} (分区: {}) - {}",
            cfg.youtube.channel_name,
            cfg.youtube.area_v2,
            if is_live { "直播中" } else { "预定直播" }
        )),
    })
}
