use super::*;

pub async fn get_config() -> Result<Json<serde_json::Value>, StatusCode> {
    let cfg = load_config()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let config_json = json!({
        "interval": cfg.interval,
        "auto_cover": cfg.auto_cover,
        "enable_anti_collision": cfg.enable_anti_collision,
        "enable_lol_monitor": cfg.enable_lol_monitor,
        "lol_monitor_interval": cfg.lol_monitor_interval,
        "riot_api_key": cfg.riot_api_key.clone().unwrap_or_default(),
        "holodex_api_key": cfg.holodex_api_key.clone().unwrap_or_default(),
        "holodex_jwt_configured": cfg
            .holodex_jwt
            .as_ref()
            .is_some_and(|j| !j.is_empty()),
        "holodex_skip_jwt_verify": cfg.holodex_skip_jwt_verify,
        "holodex_monitor_gate": cfg.holodex_monitor_gate,
        "anti_collision_list": cfg.anti_collision_list.clone(),
        "bilibili": {
            "room": cfg.bililive.room,
            "enable_danmaku_command": cfg.bililive.enable_danmaku_command,
        },
        "youtube": {
            "enable_monitor": cfg.youtube.enable_monitor,
            "channel_name": cfg.youtube.channel_name,
            "channel_id": cfg.youtube.channel_id,
            "area_v2": cfg.youtube.area_v2,
            "proxy": cfg.youtube.proxy,
            "cookies_file": cfg.youtube.cookies_file,
            "cookies_from_browser": cfg.youtube.cookies_from_browser,
            "deno_path": cfg.youtube.deno_path,
            "ffmpeg_cache": {
                "enabled": cfg.youtube.ffmpeg_cache.enabled,
                "latency_secs": cfg.youtube.ffmpeg_cache.latency_secs,
            },
        },
        "twitch": {
            "enable_monitor": cfg.twitch.enable_monitor,
            "channel_name": cfg.twitch.channel_name,
            "channel_id": cfg.twitch.channel_id,
            "area_v2": cfg.twitch.area_v2,
            "proxy_region": cfg.twitch.proxy_region,
            "proxy": cfg.twitch.proxy,
            "ffmpeg_cache": {
                "enabled": cfg.twitch.ffmpeg_cache.enabled,
                "latency_secs": cfg.twitch.ffmpeg_cache.latency_secs,
            },
        },
    });

    Ok(Json(config_json))
}

#[derive(Deserialize, Serialize)]
pub struct UpdateConfigRequest {
    #[serde(skip_serializing)]
    expected: Option<HashMap<String, serde_json::Value>>,
    interval: Option<u64>,
    auto_cover: Option<bool>,
    enable_anti_collision: Option<bool>,
    enable_lol_monitor: Option<bool>,
    lol_monitor_interval: Option<u64>,
    riot_api_key: Option<String>,
    holodex_api_key: Option<String>,
    holodex_jwt: Option<String>,
    holodex_skip_jwt_verify: Option<bool>,
    holodex_monitor_gate: Option<bool>,
    twitch_proxy_region: Option<String>,
    twitch_proxy: Option<String>,
    youtube_proxy: Option<String>,
    youtube_deno_path: Option<String>,
    anti_collision_list: Option<HashMap<String, i32>>,
    enable_danmaku_command: Option<bool>,
    youtube_enable_monitor: Option<bool>,
    twitch_enable_monitor: Option<bool>,
    youtube_cookies_from_browser: Option<String>,
    youtube_cookies_file: Option<String>,
}

pub(crate) const EDIT_CONFLICT: &str = "配置已被其他操作修改，请重新加载后再保存";
pub(crate) const EDIT_INVALID: &str = "配置修改缺少有效的原始值";

/// Check the values the browser actually edited, before applying its patch.
/// save_config then detects changes racing this request under its write lock.
pub(crate) fn validate_edit_preconditions(
    current: &serde_json::Value,
    patch: &serde_json::Value,
    expected: Option<&HashMap<String, serde_json::Value>>,
) -> Result<(), &'static str> {
    let Some(expected) = expected else {
        return Ok(()); // Compatible with existing API clients.
    };
    for (key, value) in patch.as_object().ok_or(EDIT_INVALID)? {
        if value.is_null() {
            continue;
        }
        let before = expected.get(key).ok_or(EDIT_INVALID)?;
        let now = current.get(key).ok_or(EDIT_INVALID)?;
        if before != now {
            return Err(EDIT_CONFLICT);
        }
    }
    Ok(())
}

fn config_form_values(cfg: &Config) -> serde_json::Value {
    json!({
        "interval": cfg.interval,
        "auto_cover": cfg.auto_cover,
        "enable_anti_collision": cfg.enable_anti_collision,
        "enable_danmaku_command": cfg.bililive.enable_danmaku_command,
        "enable_lol_monitor": cfg.enable_lol_monitor,
        "lol_monitor_interval": cfg.lol_monitor_interval.unwrap_or(1),
        "riot_api_key": cfg.riot_api_key.as_deref().unwrap_or_default().trim(),
        "holodex_api_key": cfg.holodex_api_key.as_deref().unwrap_or_default().trim(),
        "anti_collision_list": cfg.anti_collision_list,
        "youtube_proxy": cfg.youtube.proxy.as_deref().unwrap_or_default().trim(),
        "twitch_proxy": cfg.twitch.proxy.as_deref().unwrap_or_default().trim(),
        "twitch_proxy_region": cfg.twitch.proxy_region,
        "youtube_cookies_from_browser": cfg.youtube.cookies_from_browser.as_deref().unwrap_or_default().trim(),
        "youtube_cookies_file": cfg.youtube.cookies_file.as_deref().unwrap_or_default().trim(),
        "youtube_deno_path": cfg.youtube.deno_path.as_deref().unwrap_or_default().trim(),
    })
}

pub(crate) fn monitor_target_reload_needed(
    previous_enabled: bool,
    current_enabled: bool,
    previous_channel_name: &str,
    current_channel_name: &str,
    previous_channel_id: &str,
    current_channel_id: &str,
) -> bool {
    previous_enabled != current_enabled
        || (current_enabled
            && (previous_channel_name != current_channel_name
                || previous_channel_id != current_channel_id))
}

pub(crate) fn youtube_monitor_reload_needed(previous: &Config, current: &Config) -> bool {
    monitor_target_reload_needed(
        previous.youtube.enable_monitor,
        current.youtube.enable_monitor,
        &previous.youtube.channel_name,
        &current.youtube.channel_name,
        &previous.youtube.channel_id,
        &current.youtube.channel_id,
    )
}

pub(crate) fn twitch_monitor_reload_needed(previous: &Config, current: &Config) -> bool {
    monitor_target_reload_needed(
        previous.twitch.enable_monitor,
        current.twitch.enable_monitor,
        &previous.twitch.channel_name,
        &current.twitch.channel_name,
        &previous.twitch.channel_id,
        &current.twitch.channel_id,
    )
}

pub(crate) fn monitor_reload_needed(previous: &Config, current: &Config) -> bool {
    youtube_monitor_reload_needed(previous, current)
        || twitch_monitor_reload_needed(previous, current)
}

pub async fn update_config(
    Json(payload): Json<UpdateConfigRequest>,
) -> Result<ApiResponse<()>, StatusCode> {
    // Load current config
    let mut cfg = load_config()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let previous_cfg = cfg.clone();

    validate_edit_preconditions(
        &config_form_values(&cfg),
        &serde_json::to_value(&payload).map_err(|_| StatusCode::BAD_REQUEST)?,
        payload.expected.as_ref(),
    )
    .map_err(|error| {
        if error == EDIT_CONFLICT {
            StatusCode::CONFLICT
        } else {
            StatusCode::BAD_REQUEST
        }
    })?;

    let mut holodex_jwt_saved = false;

    // Update fields
    if let Some(interval) = payload.interval {
        cfg.interval = interval;
    }
    if let Some(auto_cover) = payload.auto_cover {
        cfg.auto_cover = auto_cover;
    }
    if let Some(enable_anti_collision) = payload.enable_anti_collision {
        cfg.enable_anti_collision = enable_anti_collision;
    }
    if let Some(enable_lol_monitor) = payload.enable_lol_monitor {
        cfg.enable_lol_monitor = enable_lol_monitor;
    }
    if let Some(lol_monitor_interval) = payload.lol_monitor_interval {
        cfg.lol_monitor_interval = Some(lol_monitor_interval);
    }
    if let Some(riot_api_key) = payload.riot_api_key {
        if !riot_api_key.is_empty() {
            cfg.riot_api_key = Some(riot_api_key);
        } else {
            cfg.riot_api_key = None;
        }
    }
    if let Some(holodex_api_key) = payload.holodex_api_key {
        if !holodex_api_key.is_empty() {
            cfg.holodex_api_key = Some(holodex_api_key);
        } else {
            cfg.holodex_api_key = None;
        }
    }
    if let Some(holodex_jwt) = payload.holodex_jwt {
        let jwt = holodex_jwt.trim();
        let jwt = jwt
            .strip_prefix("BEARER ")
            .or_else(|| jwt.strip_prefix("bearer "))
            .unwrap_or(jwt)
            .trim();
        if jwt.is_empty() {
            cfg.holodex_jwt = None;
            cfg.holodex_jwt_refreshed_at = None;
            cfg.holodex_username = None;
        } else {
            cfg.holodex_jwt = Some(jwt.to_string());
            cfg.holodex_username = None;
            holodex_jwt_saved = true;
        }
    }
    if let Some(holodex_skip_jwt_verify) = payload.holodex_skip_jwt_verify {
        cfg.holodex_skip_jwt_verify = holodex_skip_jwt_verify;
    }
    let holodex_monitor_gate_changed = payload
        .holodex_monitor_gate
        .is_some_and(|gate| gate != cfg.holodex_monitor_gate);
    if let Some(holodex_monitor_gate) = payload.holodex_monitor_gate {
        cfg.holodex_monitor_gate = holodex_monitor_gate;
    }

    if let Some(anti_collision_list) = payload.anti_collision_list {
        cfg.anti_collision_list = anti_collision_list;
    }
    if let Some(twitch_proxy_region) = payload.twitch_proxy_region {
        cfg.twitch.proxy_region = twitch_proxy_region;
    }
    if let Some(twitch_proxy) = payload.twitch_proxy {
        cfg.twitch.proxy = if twitch_proxy.is_empty() {
            None
        } else {
            Some(twitch_proxy)
        };
    }
    if let Some(youtube_proxy) = payload.youtube_proxy {
        cfg.youtube.proxy = if youtube_proxy.is_empty() {
            None
        } else {
            Some(youtube_proxy)
        };
    }
    if let Some(enable_danmaku_command) = payload.enable_danmaku_command {
        cfg.bililive.enable_danmaku_command = enable_danmaku_command;
    }
    if let Some(youtube_enable_monitor) = payload.youtube_enable_monitor {
        cfg.youtube.enable_monitor = youtube_enable_monitor;
    }
    if let Some(twitch_enable_monitor) = payload.twitch_enable_monitor {
        cfg.twitch.enable_monitor = twitch_enable_monitor;
    }
    if let Some(youtube_cookies_from_browser) = payload.youtube_cookies_from_browser {
        cfg.youtube.cookies_from_browser = if youtube_cookies_from_browser.is_empty() {
            None
        } else {
            Some(youtube_cookies_from_browser)
        };
    }
    if let Some(youtube_cookies_file) = payload.youtube_cookies_file {
        cfg.youtube.cookies_file = if youtube_cookies_file.is_empty() {
            None
        } else {
            Some(youtube_cookies_file)
        };
    }
    if let Some(youtube_deno_path) = payload.youtube_deno_path {
        cfg.youtube.deno_path = if youtube_deno_path.is_empty() {
            None
        } else {
            Some(youtube_deno_path)
        };
    }

    // Save config
    crate::config::save_config(&mut cfg)
        .await
        .map_err(config_save_status)?;

    if holodex_jwt_saved {
        if let Some(jwt) = cfg.holodex_jwt.clone() {
            ensure_holodex_username(&mut cfg, &jwt).await;
        }
    }

    if monitor_reload_needed(&previous_cfg, &cfg) {
        set_config_updated();
    }

    if holodex_monitor_gate_changed {
        crate::webui::state::request_status_refresh();
    }

    // Apply the exact saved config to the cache without re-reading config.json.
    refresh_status_cache_config_from(&cfg);

    Ok(ApiResponse {
        success: true,
        data: None,
        message: Some("配置已更新".to_string()),
    })
}
