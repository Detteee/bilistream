use super::*;

pub async fn get_config() -> Result<Json<serde_json::Value>, StatusCode> {
    let cfg = load_config()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(config_response(&cfg)))
}

fn credential_mask(value: &str) -> String {
    value
        .chars()
        .map(|c| if c == '\n' { '\n' } else { '•' })
        .collect()
}

fn proxy_password_range(value: &str) -> Option<std::ops::Range<usize>> {
    let start = value.find("://").map_or(0, |offset| offset + 3);
    let authority = value[start..].split(['/', '?', '#']).next()?;
    let at = authority.rfind('@')?;
    let colon = authority[..at].find(':')?;
    Some(start + colon + 1..start + at)
}

fn proxy_display(value: Option<&str>) -> String {
    let value = value.unwrap_or_default();
    let Some(range) = proxy_password_range(value) else {
        return value.to_owned();
    };
    let length = percent_encoding::percent_decode_str(&value[range.clone()])
        .decode_utf8_lossy()
        .chars()
        .count();
    let mut display = value.to_owned();
    display.replace_range(range, &"•".repeat(length));
    display
}

fn resolve_proxy_input(
    input: Option<String>,
    saved: Option<&str>,
    keep_password: bool,
    revision: Option<u64>,
) -> Result<Option<String>, StatusCode> {
    let Some(mut input) = input else {
        return if keep_password {
            Err(StatusCode::BAD_REQUEST)
        } else {
            Ok(None)
        };
    };
    let range = proxy_password_range(&input);
    if keep_password {
        // The frontend sends an empty password slot and an explicit intent;
        // the revision check in update_config protects the retained password.
        let range = range.ok_or(StatusCode::BAD_REQUEST)?;
        let saved = saved.ok_or(StatusCode::BAD_REQUEST)?;
        let saved_range = proxy_password_range(saved).ok_or(StatusCode::BAD_REQUEST)?;
        if revision.is_none() || !range.is_empty() || saved_range.is_empty() {
            return Err(StatusCode::BAD_REQUEST);
        }
        input.replace_range(range, &saved[saved_range]);
    } else if range.is_some_and(|range| {
        percent_encoding::percent_decode_str(&input[range])
            .decode_utf8_lossy()
            .contains('•')
    }) {
        return Err(StatusCode::BAD_REQUEST);
    }
    Ok(Some(input))
}

/// The roster summary the exemption picker lists: one row per configured
/// channel with its platform identities. Unknown/gone identities never appear.
fn exempt_channel_catalog() -> serde_json::Value {
    let Ok(roster) = crate::config::load_channels() else {
        return serde_json::json!([]);
    };
    let mut channels = Vec::new();
    for channel in &roster.channels {
        let platforms: Vec<_> = crate::plugins::warning_exempt::channel_identities(channel)
            .into_iter()
            .map(|entry| {
                serde_json::json!({
                    "platform": entry.platform,
                    "channel_id": entry.channel_id,
                })
            })
            .collect();
        if !platforms.is_empty() {
            channels.push(serde_json::json!({
                "name": channel.name,
                "platforms": platforms,
            }));
        }
    }
    serde_json::Value::Array(channels)
}

fn config_response(cfg: &Config) -> serde_json::Value {
    let (niconico_channel_id, niconico_channel_name) =
        crate::plugins::niconico_channel_identity(&cfg.niconico);

    let mut response = json!({
        "interval": cfg.interval,
        "auto_cover": cfg.auto_cover,
        "show_priority_channel": cfg.show_priority_channel,
        "show_twitch": cfg.show_twitch,
        "show_niconico": cfg.show_niconico,
        "youtube_rss_enabled": cfg.youtube_rss_enabled,
        "enable_anti_collision": cfg.enable_anti_collision,
        "enable_lol_monitor": cfg.enable_lol_monitor,
        "lol_monitor_interval": cfg.lol_monitor_interval,
        "riot_api_key": "",
        "holodex_api_key": "",
        "holodex_jwt_configured": cfg
            .holodex_jwt
            .as_ref()
            .is_some_and(|j| !j.is_empty()),
        "holodex_skip_jwt_verify": cfg.holodex_skip_jwt_verify,
        "holodex_monitor_gate": cfg.holodex_monitor_gate,
        "youtube_api_key": "",
        "youtube_websub_callback_url": cfg.youtube_websub_callback_url.clone().unwrap_or_default(),
        "youtube_websub_port": cfg.youtube_websub_port,
        "anti_collision_list": cfg.anti_collision_list.clone(),
        "enable_youtube_monitor": cfg.enable_youtube_monitor,
        "enable_twitch_monitor": cfg.enable_twitch_monitor,
        "cluster": cfg.cluster.clone(),
        "history": cfg.history,
        "bilibili": {
            "room": cfg.bililive.room,
            "enable_danmaku_command": cfg.bililive.enable_danmaku_command,
        },
        "youtube": {
            "enable_monitor": cfg.youtube.enable_monitor,
            "channel_name": cfg.youtube.channel_name,
            "channel_id": cfg.youtube.channel_id,
            "area_v2": cfg.youtube.area_v2,
            "proxy": proxy_display(cfg.youtube.proxy.as_deref()),
            "proxy_configured": cfg.youtube.proxy.as_ref().is_some_and(|s| !s.is_empty()),
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
            "proxy": proxy_display(cfg.twitch.proxy.as_deref()),
            "proxy_configured": cfg.twitch.proxy.as_ref().is_some_and(|s| !s.is_empty()),
            "ffmpeg_cache": {
                "enabled": cfg.twitch.ffmpeg_cache.enabled,
                "latency_secs": cfg.twitch.ffmpeg_cache.latency_secs,
            },
        },
        "niconico": {
            "enable_monitor": cfg.niconico.enable_monitor,
            "channel_name": niconico_channel_name,
            "channel_id": niconico_channel_id,
            "live_id": cfg.niconico.live_id,
            "area_v2": cfg.niconico.area_v2,
            "quality": cfg.niconico.quality,
            "user_session_configured": cfg.niconico.user_session.as_ref().is_some_and(|s| !s.is_empty()),
            "user_session_mask": credential_mask(cfg.niconico.user_session.as_deref().unwrap_or_default()),
            "session_check_enabled": cfg.niconico.session_check_enabled,
            "proxy": proxy_display(cfg.niconico.proxy.as_deref()),
            "proxy_configured": cfg.niconico.proxy.as_ref().is_some_and(|s| !s.is_empty()),
            "ffmpeg_cache": {
                "enabled": cfg.niconico.ffmpeg_cache.enabled,
                "latency_secs": cfg.niconico.ffmpeg_cache.latency_secs,
            },
        },
        "priority_channel": {
            "enabled": cfg.priority_channel.enabled,
            "channel_name": cfg.priority_channel.channel_name,
            "youtube_channel_id": cfg.priority_channel.youtube_channel_id,
            "twitch_channel_id": cfg.priority_channel.twitch_channel_id,
            "default_area": cfg.priority_channel.default_area,
            "auto_restart": cfg.priority_channel.auto_restart,
        }
    });
    response["warning_hold_exempt"] = json!(cfg.warning_hold_exempt);
    response["warning_hold_exempt_reasons"] = json!(cfg.warning_hold_exempt_reasons);
    response["warning_hold_exempt_all_low_risk"] = json!(cfg.warning_hold_exempt_all_low_risk);
    response["warning_hold_exempt_channels"] = exempt_channel_catalog();
    response["riot_api_key_configured"] =
        json!(cfg.riot_api_key.as_ref().is_some_and(|s| !s.is_empty()));
    response["holodex_api_key_configured"] =
        json!(cfg.holodex_api_key.as_ref().is_some_and(|s| !s.is_empty()));
    response["youtube_api_key_configured"] = json!(!cfg.youtube_api_keys().is_empty());
    response["riot_api_key_mask"] = json!(credential_mask(
        cfg.riot_api_key.as_deref().unwrap_or_default()
    ));
    response["holodex_api_key_mask"] = json!(credential_mask(
        cfg.holodex_api_key.as_deref().unwrap_or_default()
    ));
    response["youtube_api_key_mask"] = json!(credential_mask(&cfg.youtube_api_keys().join("\n")));
    response["secret_revision"] = json!(crate::config::config_data_revision(cfg));
    response
}

#[derive(Deserialize, Serialize, Default)]
pub struct UpdateConfigRequest {
    #[serde(skip_serializing)]
    expected_secret_revision: Option<u64>,
    clear_riot_api_key: Option<bool>,
    clear_holodex_api_key: Option<bool>,
    clear_youtube_api_key: Option<bool>,
    #[serde(skip_serializing)]
    expected: Option<HashMap<String, serde_json::Value>>,
    interval: Option<u64>,
    auto_cover: Option<bool>,
    show_priority_channel: Option<bool>,
    show_twitch: Option<bool>,
    show_niconico: Option<bool>,
    youtube_rss_enabled: Option<bool>,
    enable_anti_collision: Option<bool>,
    enable_lol_monitor: Option<bool>,
    lol_monitor_interval: Option<u64>,
    riot_api_key: Option<String>,
    holodex_api_key: Option<String>,
    holodex_jwt: Option<String>,
    holodex_skip_jwt_verify: Option<bool>,
    holodex_monitor_gate: Option<bool>,
    youtube_api_key: Option<String>,
    youtube_websub_callback_url: Option<String>,
    youtube_websub_port: Option<u16>,
    twitch_proxy_region: Option<String>,
    twitch_proxy: Option<String>,
    #[serde(skip_serializing)]
    twitch_proxy_keep_password: Option<bool>,
    clear_twitch_proxy: Option<bool>,
    youtube_proxy: Option<String>,
    #[serde(skip_serializing)]
    youtube_proxy_keep_password: Option<bool>,
    clear_youtube_proxy: Option<bool>,
    youtube_deno_path: Option<String>,
    anti_collision_list: Option<HashMap<String, i32>>,
    enable_danmaku_command: Option<bool>,
    youtube_enable_monitor: Option<bool>,
    twitch_enable_monitor: Option<bool>,
    warning_hold_exempt: Option<Vec<crate::plugins::warning_exempt::ExemptChannel>>,
    warning_hold_exempt_reasons: Option<Vec<String>>,
    warning_hold_exempt_all_low_risk: Option<bool>,
    youtube_cookies_from_browser: Option<String>,
    youtube_cookies_file: Option<String>,
    niconico_user_session: Option<String>,
    clear_niconico_user_session: Option<bool>,
    niconico_session_check_enabled: Option<bool>,
    niconico_proxy: Option<String>,
    #[serde(skip_serializing)]
    niconico_proxy_keep_password: Option<bool>,
    clear_niconico_proxy: Option<bool>,
    cluster: Option<ClusterConfig>,
    history: Option<HistoryPatch>,
}

#[derive(Deserialize, Serialize, Default)]
#[serde(deny_unknown_fields)]
struct HistoryPatch {
    enabled: Option<bool>,
    sample_secs: Option<u64>,
    bili_sample_secs: Option<u64>,
    bili_stop_summary: Option<bool>,
    capture: Option<bool>,
    retention: Option<HistoryRetentionPatch>,
}

#[derive(Deserialize, Serialize, Default)]
#[serde(deny_unknown_fields)]
struct HistoryRetentionPatch {
    raw_days: Option<u64>,
    rollup_days: Option<u64>,
    event_days: Option<u64>,
    event_rows: Option<u64>,
    max_mb: Option<u64>,
}

impl HistoryPatch {
    fn apply(
        self,
        saved: &crate::config::HistoryConfig,
    ) -> Result<crate::config::HistoryConfig, &'static str> {
        let mut next = saved.clone();
        if let Some(value) = self.enabled {
            next.enabled = value
        }
        if let Some(value) = self.sample_secs {
            next.sample_secs = value
        }
        if let Some(value) = self.bili_sample_secs {
            next.bili_sample_secs = value
        }
        if let Some(value) = self.bili_stop_summary {
            next.bili_stop_summary = value
        }
        if let Some(value) = self.capture {
            next.capture = value
        }
        if let Some(p) = self.retention {
            if let Some(value) = p.raw_days {
                next.retention.raw_days = value
            }
            if let Some(value) = p.rollup_days {
                next.retention.rollup_days = value
            }
            if let Some(value) = p.event_days {
                next.retention.event_days = value
            }
            if let Some(value) = p.event_rows {
                next.retention.event_rows = value
            }
            if let Some(value) = p.max_mb {
                next.retention.max_mb = value
            }
        }
        if !(5..=60).contains(&next.sample_secs) || !(5..=60).contains(&next.bili_sample_secs) {
            return Err("统计采样间隔须为 5–60 秒");
        }
        if !(1..=90).contains(&next.retention.raw_days)
            || !(1..=90).contains(&next.retention.rollup_days)
            || next.retention.raw_days > next.retention.rollup_days
        {
            return Err("统计样本保留期须为 1–90 天，原始样本不能长于分钟数据");
        }
        if !(1..=3650).contains(&next.retention.event_days)
            || !(100..=2_000_000).contains(&next.retention.event_rows)
        {
            return Err("统计事件保留期须为 1–3650 天，记录上限须为 100–2000000");
        }
        if !(16..=65_536).contains(&next.retention.max_mb) {
            return Err("统计空间上限须为 16–65536 MiB");
        }
        Ok(next)
    }
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
        "show_priority_channel": cfg.show_priority_channel,
        "show_twitch": cfg.show_twitch,
        "show_niconico": cfg.show_niconico,
        "youtube_rss_enabled": cfg.youtube_rss_enabled,
        "auto_cover": cfg.auto_cover,
        "enable_anti_collision": cfg.enable_anti_collision,
        "enable_danmaku_command": cfg.bililive.enable_danmaku_command,
        "holodex_monitor_gate": cfg.holodex_monitor_gate,
        "enable_lol_monitor": cfg.enable_lol_monitor,
        "lol_monitor_interval": cfg.lol_monitor_interval.unwrap_or(1),
        "riot_api_key": "",
        "clear_riot_api_key": false,
        "holodex_api_key": "",
        "clear_holodex_api_key": false,
        "youtube_api_key": "",
        "clear_youtube_api_key": false,
        "youtube_websub_callback_url": cfg.youtube_websub_callback_url.as_deref().unwrap_or_default().trim(),
        "youtube_websub_port": cfg.youtube_websub_port,
        "anti_collision_list": cfg.anti_collision_list,
        "youtube_proxy": "",
        "clear_youtube_proxy": false,
        "twitch_proxy": "",
        "clear_twitch_proxy": false,
        "twitch_proxy_region": cfg.twitch.proxy_region,
        "youtube_cookies_from_browser": cfg.youtube.cookies_from_browser.as_deref().unwrap_or_default().trim(),
        "youtube_cookies_file": cfg.youtube.cookies_file.as_deref().unwrap_or_default().trim(),
        "youtube_deno_path": cfg.youtube.deno_path.as_deref().unwrap_or_default().trim(),
        "niconico_user_session": "", // write-only: never echo a stored credential
        "clear_niconico_user_session": false,
        "niconico_session_check_enabled": cfg.niconico.session_check_enabled,
        "niconico_proxy": "",
        "clear_niconico_proxy": false,
        "cluster": cfg.cluster,
        "history": cfg.history,
        "warning_hold_exempt": cfg.warning_hold_exempt,
        "warning_hold_exempt_reasons": cfg.warning_hold_exempt_reasons,
        "warning_hold_exempt_all_low_risk": cfg.warning_hold_exempt_all_low_risk,
    })
}

/// The callback URL must be empty or `http(s)`, and the callback port must be
/// a real port other than the WebUI's.
pub(crate) fn validate_websub(url: Option<&str>, port: Option<u16>) -> Result<(), String> {
    if let Some(url) = url.map(str::trim).filter(|url| !url.is_empty()) {
        let parsed = reqwest::Url::parse(url).map_err(|_| "WebSub 回调地址无效".to_string())?;
        if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
            return Err("WebSub 回调地址必须是 http(s) 地址".to_string());
        }
    }
    if let Some(port) = port {
        if port == 0 {
            return Err("WebSub 回调端口无效".to_string());
        }
        if Some(port) == crate::plugins::youtube_websub::webui_port() {
            return Err("WebSub 回调端口不能与 Web UI 端口相同".to_string());
        }
    }
    Ok(())
}

#[cfg(test)]
#[test]
fn cluster_and_niconico_edits_validate_against_the_loaded_configuration() {
    let mut cfg = crate::test_support::config("local", 0);
    cfg.niconico.proxy = Some(" http://proxy ".into());
    let current = config_form_values(&cfg);
    assert_eq!(current["niconico_proxy"], "");
    assert!(current.get("niconico_cookies_file").is_none());
    let expected = HashMap::from([("cluster".into(), current["cluster"].clone())]);
    let patch = json!({"cluster": {"priority": 20}});
    assert!(validate_edit_preconditions(&current, &patch, Some(&expected)).is_ok());
    cfg.cluster.public_status.node_id = "peer".into();
    assert_eq!(
        validate_edit_preconditions(&config_form_values(&cfg), &patch, Some(&expected)),
        Err(EDIT_CONFLICT)
    );
    // An unrelated form edit does not depend on any cluster field.
    assert!(validate_edit_preconditions(
        &config_form_values(&cfg),
        &json!({"interval": 75}),
        Some(&HashMap::from([("interval".into(), json!(cfg.interval))]))
    )
    .is_ok());
}

/// The settings route reports a rejected edit as `success: false` + message
/// (its established shape; it has no coded-error envelope).
fn edit_refusal(message: String) -> ApiResponse<()> {
    ApiResponse {
        success: false,
        data: None,
        message: Some(message),
    }
}

/// Validate and canonicalize the reason ids a save carries: only built-in ids
/// are accepted (a peer push ignores unknown ids instead), duplicates collapse
/// and the order is preserved. The settings route reports refusals as an
/// ApiResponse message (the route has no coded-error shape yet).
pub(crate) fn validated_exempt_reasons(reasons: Vec<String>) -> Result<Vec<String>, String> {
    let (known, unknown) = crate::plugins::warning_exempt::split_reason_ids(reasons);
    match unknown.first() {
        Some(reason) => Err(format!("违规类型无效：{reason}")),
        None => Ok(known),
    }
}

/// The global low-risk switch only means something when the low-risk class is
/// ticked; refuse a combination that would silently do nothing.
pub(crate) fn validated_exempt_all_low_risk(
    reasons: &[String],
    all_low_risk: bool,
) -> Result<(), String> {
    if all_low_risk
        && !reasons.iter().any(|reason| {
            reason == crate::plugins::warning_exempt::ReasonId::IdleRecording.as_str()
        })
    {
        return Err("「应用到全部频道」需要先勾选低风险类型（挂机/录播）".into());
    }
    Ok(())
}

/// Validate and canonicalize the exemption channel identities a save carries:
/// each must be one of the monitored roster's platform identities. The stored
/// entry takes the roster's display name, never the client's; duplicates
/// collapse.
pub(crate) fn validated_exempt_channels(
    entries: Vec<crate::plugins::warning_exempt::ExemptChannel>,
    roster: &crate::config::ChannelsData,
) -> Result<Vec<crate::plugins::warning_exempt::ExemptChannel>, String> {
    let known = crate::plugins::warning_exempt::roster_identities(roster);
    let mut canonical: Vec<crate::plugins::warning_exempt::ExemptChannel> = Vec::new();
    for entry in entries {
        let Some(entry) = crate::plugins::warning_exempt::ExemptChannel::new(
            &entry.platform,
            &entry.channel_id,
            &entry.name,
        ) else {
            return Err("频道身份无效".into());
        };
        let Some(known) = known
            .iter()
            .find(|known| known.platform == entry.platform && known.channel_id == entry.channel_id)
        else {
            return Err(format!(
                "频道不在监控列表：{}",
                if entry.name.is_empty() {
                    &entry.channel_id
                } else {
                    &entry.name
                }
            ));
        };
        let stored = crate::plugins::warning_exempt::ExemptChannel {
            platform: known.platform.clone(),
            channel_id: known.channel_id.clone(),
            name: known.name.clone(),
        };
        if !canonical
            .iter()
            .any(|other| other.platform == stored.platform && other.channel_id == stored.channel_id)
        {
            canonical.push(stored);
        }
    }
    Ok(canonical)
}

pub(crate) fn config_payload_enables_a_monitor_toggle(
    payload: &UpdateConfigRequest,
    cfg: &Config,
) -> bool {
    (payload.enable_danmaku_command == Some(true) && !cfg.bililive.enable_danmaku_command)
        || (payload.youtube_enable_monitor == Some(true) && !cfg.youtube.enable_monitor)
        || (payload.twitch_enable_monitor == Some(true) && !cfg.twitch.enable_monitor)
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

pub(crate) fn niconico_monitor_reload_needed(previous: &Config, current: &Config) -> bool {
    previous.niconico.enable_monitor != current.niconico.enable_monitor
        || (current.niconico.enable_monitor
            && (previous.niconico.channel_name != current.niconico.channel_name
                || previous.niconico.channel_id != current.niconico.channel_id
                || previous.niconico.live_id != current.niconico.live_id))
}

pub(crate) fn monitor_reload_needed(previous: &Config, current: &Config) -> bool {
    youtube_monitor_reload_needed(previous, current)
        || twitch_monitor_reload_needed(previous, current)
        || niconico_monitor_reload_needed(previous, current)
}

pub(crate) static SETTINGS_PEER_SYNC: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn schedule_settings_sync(monitored: bool, settings: bool) -> String {
    if !monitored && !settings {
        return String::new();
    }
    tokio::spawn(async move {
        let _guard = SETTINGS_PEER_SYNC.lock().await;
        // Read after queueing so a delayed job never republishes an obsolete
        // local snapshot over a more recent edit from another browser tab.
        let Ok(cfg) = load_config().await else {
            return;
        };
        if monitored && cfg.cluster.enabled && cfg.cluster.sync_monitored_channels {
            if let Err(error) = push_monitored_config_to_peers(&cfg).await {
                tracing::warn!("节点配置同步失败: {error}");
            }
        }
        if settings && cfg.cluster.enabled {
            if let Err(error) = crate::cluster::push_cluster_settings_to_peers(&cfg).await {
                tracing::warn!("集群设置同步失败: {error}");
            }
        }
    });
    "；节点同步在后台进行".into()
}

pub async fn update_config(
    Json(mut payload): Json<UpdateConfigRequest>,
) -> Result<ApiResponse<()>, StatusCode> {
    // Load current config
    let mut cfg = load_config()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    if let Some(cluster) = payload.cluster.take() {
        match crate::cluster::fence_cluster_edit(
            &cfg.cluster,
            cluster,
            crate::cluster::membership::current_lifecycle(),
        ) {
            Ok(cluster) => payload.cluster = Some(cluster),
            Err(message) => {
                return Ok(ApiResponse {
                    success: false,
                    data: None,
                    message: Some(message.to_string()),
                })
            }
        }
    }
    let secrets_changed = payload.riot_api_key.is_some()
        || payload.holodex_api_key.is_some()
        || payload.youtube_api_key.is_some()
        || payload.clear_riot_api_key == Some(true)
        || payload.clear_holodex_api_key == Some(true)
        || payload.clear_youtube_api_key == Some(true)
        || payload.niconico_user_session.is_some()
        || payload.clear_niconico_user_session == Some(true)
        || payload.youtube_proxy.is_some()
        || payload.clear_youtube_proxy == Some(true)
        || payload.twitch_proxy.is_some()
        || payload.clear_twitch_proxy == Some(true)
        || payload.niconico_proxy.is_some()
        || payload.clear_niconico_proxy == Some(true);
    if secrets_changed
        && payload
            .expected_secret_revision
            .is_some_and(|revision| revision != crate::config::config_data_revision(&cfg))
    {
        return Err(StatusCode::CONFLICT);
    }
    payload.youtube_proxy = resolve_proxy_input(
        payload.youtube_proxy,
        cfg.youtube.proxy.as_deref(),
        payload.youtube_proxy_keep_password == Some(true),
        payload.expected_secret_revision,
    )?;
    payload.twitch_proxy = resolve_proxy_input(
        payload.twitch_proxy,
        cfg.twitch.proxy.as_deref(),
        payload.twitch_proxy_keep_password == Some(true),
        payload.expected_secret_revision,
    )?;
    payload.niconico_proxy = resolve_proxy_input(
        payload.niconico_proxy,
        cfg.niconico.proxy.as_deref(),
        payload.niconico_proxy_keep_password == Some(true),
        payload.expected_secret_revision,
    )?;
    let legacy_imports = [("cookies.txt", payload.youtube_cookies_file.clone())];
    let document_edits = tokio::task::spawn_blocking(
        move || -> std::io::Result<Vec<(&'static str, serde_json::Value)>> {
            let mut edits = Vec::new();
            for (name, path) in legacy_imports {
                if let Some(path) = path.filter(|s| !s.trim().is_empty()) {
                    let path = std::path::PathBuf::from(path);
                    let path = if path.is_absolute() {
                        path
                    } else {
                        crate::storage::paths::executable_dir()?.join(path)
                    };
                    if std::fs::metadata(&path)?.len()
                        > crate::plugins::youtube_cookies::MAX_COOKIE_BYTES as u64
                    {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::InvalidInput,
                            "Cookie 文件过大",
                        ));
                    }
                    let text = std::fs::read_to_string(path)?;
                    crate::plugins::youtube_cookies::validate_netscape(&text)?;
                    edits.push((name, serde_json::Value::String(text)));
                }
            }
            Ok(edits)
        },
    )
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
    .map_err(|_| StatusCode::BAD_REQUEST)?;
    let previous_cfg = cfg.clone();
    let old_monitored_config_version = crate::cluster::shared_settings_version(&cfg);
    let cluster_changed = payload.cluster.is_some();
    let danmaku_command_changed = payload.enable_danmaku_command;
    // The settings form always posts the current checkbox states. Only reject
    // when this save would turn a monitor on, not when cookies/proxy change
    // while danmaku is already enabled.
    if config_payload_enables_a_monitor_toggle(&payload, &cfg)
        && !local_node_can_enable_monitor_toggles(&cfg)
    {
        return Ok(monitor_toggle_enable_rejected_response());
    }

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

    if let Err(message) = validate_websub(
        payload.youtube_websub_callback_url.as_deref(),
        payload.youtube_websub_port,
    ) {
        return Ok(ApiResponse {
            success: false,
            data: None,
            message: Some(message),
        });
    }

    if let Some(session) = payload
        .niconico_user_session
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        if session
            .strip_prefix("user_session=")
            .unwrap_or(session)
            .is_empty()
            || session.len() > 8192
            || !session
                .bytes()
                .all(|b| b.is_ascii_graphic() && b != b';' && b != b',')
        {
            return Ok(ApiResponse {
                success: false,
                data: None,
                message: Some(
                    "user_session 只能填写 Cookie 的值，不能包含空格、换行或其他 Cookie".into(),
                ),
            });
        }
    }
    let mut holodex_jwt_saved = false;

    if let Some(history) = payload.history.take() {
        match history.apply(&cfg.history) {
            Ok(history) => cfg.history = history,
            Err(message) => {
                return Ok(ApiResponse {
                    success: false,
                    data: None,
                    message: Some(message.into()),
                })
            }
        }
    }

    // Update fields
    if let Some(visible) = payload.show_priority_channel {
        cfg.show_priority_channel = visible;
    }
    if let Some(visible) = payload.show_twitch {
        cfg.show_twitch = visible;
    }
    if let Some(visible) = payload.show_niconico {
        cfg.show_niconico = visible;
    }
    if let Some(enabled) = payload.youtube_rss_enabled {
        cfg.youtube_rss_enabled = enabled;
    }
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
    for (slot, input, clear) in [
        (
            &mut cfg.youtube.proxy,
            payload.youtube_proxy,
            payload.clear_youtube_proxy,
        ),
        (
            &mut cfg.twitch.proxy,
            payload.twitch_proxy,
            payload.clear_twitch_proxy,
        ),
        (
            &mut cfg.niconico.proxy,
            payload.niconico_proxy,
            payload.clear_niconico_proxy,
        ),
        (
            &mut cfg.riot_api_key,
            payload.riot_api_key,
            payload.clear_riot_api_key,
        ),
        (
            &mut cfg.holodex_api_key,
            payload.holodex_api_key,
            payload.clear_holodex_api_key,
        ),
        (
            &mut cfg.youtube_api_key,
            payload.youtube_api_key,
            payload.clear_youtube_api_key,
        ),
    ] {
        if clear == Some(true) {
            *slot = None;
        } else if let Some(value) = input.map(|s| s.trim().to_owned()).filter(|s| !s.is_empty()) {
            *slot = Some(value);
        }
    }
    if let Some(url) = payload.youtube_websub_callback_url {
        let url = url.trim();
        cfg.youtube_websub_callback_url = (!url.is_empty()).then(|| url.to_string());
    }
    if let Some(port) = payload.youtube_websub_port {
        cfg.youtube_websub_port = port;
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

    if let Some(enable_danmaku_command) = payload.enable_danmaku_command {
        cfg.bililive.enable_danmaku_command = enable_danmaku_command;
    }
    if let Some(youtube_enable_monitor) = payload.youtube_enable_monitor {
        cfg.youtube.enable_monitor = youtube_enable_monitor;
        cfg.enable_youtube_monitor = youtube_enable_monitor;
    }
    if let Some(twitch_enable_monitor) = payload.twitch_enable_monitor {
        cfg.twitch.enable_monitor = twitch_enable_monitor;
        cfg.enable_twitch_monitor = twitch_enable_monitor;
    }
    if payload.warning_hold_exempt.is_some()
        || payload.warning_hold_exempt_reasons.is_some()
        || payload.warning_hold_exempt_all_low_risk.is_some()
    {
        if let Some(entries) = payload.warning_hold_exempt.take() {
            let roster =
                crate::config::load_channels().map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
            match validated_exempt_channels(entries, &roster) {
                Ok(entries) => cfg.warning_hold_exempt = entries,
                Err(message) => return Ok(edit_refusal(message)),
            }
        }
        if let Some(reasons) = payload.warning_hold_exempt_reasons.take() {
            match validated_exempt_reasons(reasons) {
                Ok(reasons) => cfg.warning_hold_exempt_reasons = reasons,
                Err(message) => return Ok(edit_refusal(message)),
            }
        }
        if let Some(all_low_risk) = payload.warning_hold_exempt_all_low_risk {
            match validated_exempt_all_low_risk(&cfg.warning_hold_exempt_reasons, all_low_risk) {
                Ok(()) => cfg.warning_hold_exempt_all_low_risk = all_low_risk,
                Err(message) => return Ok(edit_refusal(message)),
            }
        } else if cfg.warning_hold_exempt_all_low_risk {
            // Changing the reason list can remove the low-risk tick the global
            // switch depends on.
            if let Err(message) =
                validated_exempt_all_low_risk(&cfg.warning_hold_exempt_reasons, true)
            {
                return Ok(edit_refusal(message));
            }
        }
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
    if let Some(enabled) = payload.niconico_session_check_enabled {
        cfg.niconico.session_check_enabled = enabled;
    }
    if payload.clear_niconico_user_session == Some(true) {
        cfg.niconico.user_session = None;
    } else if let Some(session) = payload
        .niconico_user_session
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        cfg.niconico.user_session = Some(
            session
                .strip_prefix("user_session=")
                .unwrap_or(session)
                .to_string(),
        );
    }

    if let Some(youtube_deno_path) = payload.youtube_deno_path {
        cfg.youtube.deno_path = if youtube_deno_path.is_empty() {
            None
        } else {
            Some(youtube_deno_path)
        };
    }
    if let Some(cluster) = payload.cluster {
        cfg.cluster = cluster;
    }

    // Commit imported credentials and settings together.
    if document_edits.is_empty() {
        crate::config::save_config(&mut cfg)
            .await
            .map_err(config_save_status)?;
    } else {
        crate::config::save_config_with_transaction(&mut cfg, move |tx| {
            for (name, value) in document_edits {
                tx.write(name, value)?;
            }
            Ok(())
        })
        .await
        .map_err(config_save_status)?;
    }

    if let Some(enabled) = danmaku_command_changed {
        crate::cluster::apply_danmaku_command_runtime_state(enabled).await;
    }

    if holodex_jwt_saved {
        if let Some(jwt) = cfg.holodex_jwt.clone() {
            crate::plugins::holodex_account::ensure_holodex_username(&mut cfg, &jwt).await;
        }
    }

    if monitor_reload_needed(&previous_cfg, &cfg) {
        set_config_updated();
    }

    // Apply the exact saved config to the cache without re-reading config.json.
    refresh_status_cache_config_from(&cfg);
    crate::history::refresh_config(&cfg);
    if holodex_monitor_gate_changed {
        crate::webui::state::request_status_refresh();
    }

    let monitor_toggle_changed = danmaku_command_changed.is_some()
        || payload.youtube_enable_monitor.is_some()
        || payload.twitch_enable_monitor.is_some();
    let toggle_sync_message = if monitor_toggle_changed {
        schedule_active_monitor_state_sync_after_toggle_change(&cfg)
    } else {
        String::new()
    };
    let sync_message = schedule_settings_sync(
        cfg.cluster.enabled
            && cfg.cluster.sync_monitored_channels
            && old_monitored_config_version != crate::cluster::shared_settings_version(&cfg),
        cluster_changed,
    );

    Ok(ApiResponse {
        success: true,
        data: None,
        message: Some(format!(
            "配置已更新{}{}{}",
            sync_message,
            toggle_sync_message,
            if previous_cfg.history.enabled != cfg.history.enabled {
                "；统计记录开关将在进程重启后生效"
            } else {
                ""
            }
        )),
    })
}

#[derive(Deserialize, Serialize)]
pub struct UpdatePriorityChannelRequest {
    #[serde(skip_serializing)]
    expected: Option<HashMap<String, serde_json::Value>>,
    enabled: Option<bool>,
    channel_name: Option<String>,
    default_area: Option<u64>,
    auto_restart: Option<bool>,
}

pub async fn update_priority_channel(
    Json(payload): Json<UpdatePriorityChannelRequest>,
) -> Result<ApiResponse<()>, StatusCode> {
    let mut cfg = load_config()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let old_monitored_config_version = monitored_config_version(&cfg);
    let requests_monitor_toggle_enable =
        payload.enabled == Some(true) || payload.auto_restart == Some(true);
    if requests_monitor_toggle_enable && !local_node_can_enable_monitor_toggles(&cfg) {
        return Ok(monitor_toggle_enable_rejected_response());
    }

    validate_edit_preconditions(
        &json!({
            "enabled": cfg.priority_channel.enabled,
            "auto_restart": cfg.priority_channel.auto_restart,
            "channel_name": cfg.priority_channel.channel_name,
            "default_area": cfg.priority_channel.default_area,
        }),
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

    // Update fields
    if let Some(enabled) = payload.enabled {
        cfg.priority_channel.enabled = enabled;
    }
    if let Some(channel_name) = payload.channel_name {
        cfg.priority_channel.channel_name = channel_name;
        // Update platform IDs from channels.json
        crate::config::update_priority_channel_from_channels(&mut cfg);
    }
    if let Some(default_area) = payload.default_area {
        cfg.priority_channel.default_area = default_area;
    }
    if let Some(auto_restart) = payload.auto_restart {
        cfg.priority_channel.auto_restart = auto_restart;
    }

    // Save config
    crate::config::save_config(&mut cfg)
        .await
        .map_err(config_save_status)?;

    // Set config updated flag so main loop can detect the change
    set_config_updated();

    // Refresh status cache with updated configuration
    refresh_status_cache_config_from(&cfg);

    // Refresh priority channel status in background (independent of main loop)
    tokio::spawn(async {
        let _ = refresh_priority_channel_status().await;
    });

    let priority_toggle_changed = payload.enabled.is_some() || payload.auto_restart.is_some();
    let sync_message = if old_monitored_config_version != monitored_config_version(&cfg) {
        schedule_settings_sync(
            cfg.cluster.enabled && cfg.cluster.sync_monitored_channels,
            false,
        )
    } else {
        String::new()
    };
    let toggle_sync_message = if priority_toggle_changed {
        schedule_active_monitor_state_sync_after_toggle_change(&cfg)
    } else {
        String::new()
    };

    Ok(ApiResponse {
        success: true,
        data: None,
        message: Some(format!(
            "优先频道配置已更新{}{}",
            sync_message, toggle_sync_message
        )),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credentials_are_masked_and_proxy_addresses_are_visible_in_admin_config() {
        let mut cfg = crate::test_support::config("local", 0);
        cfg.niconico.user_session = Some("private-session-test-value".into());
        cfg.youtube.proxy = Some("http://user:private-proxy-password@proxy.invalid:8080".into());
        cfg.twitch.proxy = cfg.youtube.proxy.clone();
        cfg.niconico.proxy = cfg.youtube.proxy.clone();
        cfg.holodex_api_key = Some("private-holodex".into());
        cfg.riot_api_key = Some("private-riot".into());
        cfg.youtube_api_key = Some("private-youtube-one\nprivate-youtube-two".into());
        let response = config_response(&cfg);
        assert_eq!(response["niconico"]["user_session_configured"], true);
        assert!(response["niconico"].get("user_session").is_none());
        assert!(!response.to_string().contains("private-session-test-value"));
        assert_eq!(config_form_values(&cfg)["niconico_user_session"], "");
        for platform in ["youtube", "twitch", "niconico"] {
            assert_eq!(
                response[platform]["proxy"],
                format!("http://user:{}@proxy.invalid:8080", "•".repeat(22))
            );
            assert_eq!(response[platform]["proxy_configured"], true);
        }
        assert_eq!(response["niconico"]["user_session_mask"], "•".repeat(26));
        for (key, length) in [("holodex_api_key", 15), ("riot_api_key", 12)] {
            assert_eq!(response[key], "");
            assert_eq!(response[format!("{key}_mask")], "•".repeat(length));
        }
        assert_eq!(
            response["youtube_api_key_mask"],
            format!("{}\n{}", "•".repeat(19), "•".repeat(19))
        );
        for secret in ["private-holodex", "private-riot", "private-youtube"] {
            assert!(!response.to_string().contains(secret));
        }
        assert!(!response.to_string().contains("private-proxy-password"));
        assert!(!config_form_values(&cfg)
            .to_string()
            .contains("private-proxy-password"));
    }

    #[test]
    fn proxy_password_mask_preserves_the_address_and_password_edits_are_explicit() {
        let saved = "socks5h://user:p%40ss%3Aword@[::1]:1080";
        assert_eq!(
            proxy_display(Some(saved)),
            "socks5h://user:•••••••••@[::1]:1080"
        );
        assert_eq!(
            proxy_display(Some("http://127.0.0.1:7890")),
            "http://127.0.0.1:7890"
        );
        assert_eq!(
            proxy_display(Some("user:secret@host:8080")),
            "user:••••••@host:8080"
        );
        assert_eq!(
            proxy_display(Some("http://user:密码@host")),
            "http://user:••@host"
        );
        let edited = resolve_proxy_input(
            Some("socks5h://user:@proxy.invalid:1081".into()),
            Some(saved),
            true,
            Some(1),
        )
        .unwrap()
        .unwrap();
        assert_eq!(edited, "socks5h://user:p%40ss%3Aword@proxy.invalid:1081");
        assert_eq!(
            resolve_proxy_input(
                Some("http://user:new@host".into()),
                Some(saved),
                false,
                Some(1)
            )
            .unwrap()
            .as_deref(),
            Some("http://user:new@host")
        );
        assert_eq!(
            resolve_proxy_input(
                Some("http://user:@host".into()),
                Some(saved),
                false,
                Some(1)
            )
            .unwrap()
            .as_deref(),
            Some("http://user:@host")
        );
        for input in ["http://user:•••@host", "http://user:%E2%80%A2@host"] {
            assert_eq!(
                resolve_proxy_input(Some(input.into()), Some(saved), false, Some(1)).unwrap_err(),
                StatusCode::BAD_REQUEST
            );
        }
        assert!(
            resolve_proxy_input(Some("http://user:@host".into()), Some(saved), true, None).is_err()
        );
        assert!(
            resolve_proxy_input(Some("http://user:@host".into()), None, true, Some(1)).is_err()
        );
    }

    #[test]
    fn display_and_rss_settings_are_independent_conflict_checked_edits() {
        let from_browser: UpdateConfigRequest = serde_json::from_value(json!({
            "youtube_rss_enabled": false, "show_priority_channel": true
        }))
        .unwrap();
        assert_eq!(from_browser.youtube_rss_enabled, Some(false));
        assert_eq!(from_browser.show_priority_channel, Some(true));
        let previous = crate::test_support::config("local", 0);
        let mut cfg = previous.clone();
        cfg.show_priority_channel = true;
        cfg.youtube_rss_enabled = false;
        assert!(!monitor_reload_needed(&previous, &cfg));
        assert_eq!(
            monitored_config_version(&previous),
            monitored_config_version(&cfg)
        );
        let payload = UpdateConfigRequest {
            show_priority_channel: Some(false),
            youtube_rss_enabled: Some(true),
            ..Default::default()
        };
        assert!(!config_payload_enables_a_monitor_toggle(&payload, &cfg));
        let patch = serde_json::to_value(&payload).unwrap();
        let expected = HashMap::from([
            ("show_priority_channel".to_string(), json!(true)),
            ("youtube_rss_enabled".to_string(), json!(false)),
        ]);
        assert!(
            validate_edit_preconditions(&config_form_values(&cfg), &patch, Some(&expected)).is_ok()
        );
        cfg.youtube_rss_enabled = true;
        assert_eq!(
            validate_edit_preconditions(&config_form_values(&cfg), &patch, Some(&expected)),
            Err(EDIT_CONFLICT)
        );
    }

    #[test]
    fn old_configs_default_to_rss_on_and_priority_card_hidden() {
        let mut raw = serde_json::to_value(crate::test_support::config("local", 0)).unwrap();
        for field in [
            "youtube_rss_enabled",
            "show_priority_channel",
            "show_twitch",
            "show_niconico",
            "priority_channel",
            "cluster",
            "niconico",
        ] {
            raw.as_object_mut().unwrap().remove(field);
        }
        let cfg: Config = serde_json::from_value(raw).unwrap();
        assert!(cfg.youtube_rss_enabled);
        assert!(!cfg.show_priority_channel);
        assert!(cfg.show_twitch);
        assert!(!cfg.show_niconico);
        assert!(!cfg.priority_channel.enabled);
        assert!(!cfg.cluster.enabled);
    }

    #[test]
    fn settings_save_does_not_treat_already_enabled_danmaku_as_an_enable_request() {
        let mut cfg = crate::test_support::config("local", 0);
        cfg.bililive.enable_danmaku_command = true;
        let payload = UpdateConfigRequest {
            enable_danmaku_command: Some(true),
            niconico_session_check_enabled: Some(true),
            ..UpdateConfigRequest::default()
        };

        assert!(!config_payload_enables_a_monitor_toggle(&payload, &cfg));

        cfg.bililive.enable_danmaku_command = false;
        assert!(config_payload_enables_a_monitor_toggle(&payload, &cfg));
    }

    #[test]
    fn exemption_settings_are_validated_against_the_monitored_roster() {
        use crate::config::{Channel, ChannelPlatforms, ChannelsData};
        use crate::plugins::warning_exempt::ExemptChannel;

        let roster = ChannelsData {
            channels: vec![Channel {
                name: "星海频道".into(),
                niconico_name: None,
                aliases: vec![],
                platforms: ChannelPlatforms {
                    youtube: Some("UC1".into()),
                    twitch: Some("StarSea".into()),
                    niconico: None,
                },
                riot_puuid: None,
            }],
        };
        let entries = validated_exempt_channels(
            vec![
                ExemptChannel {
                    platform: "yt".into(),
                    channel_id: "UC1".into(),
                    name: "客户端名字".into(),
                },
                ExemptChannel {
                    platform: "TW".into(),
                    channel_id: "STARSEA".into(),
                    name: String::new(),
                },
                ExemptChannel {
                    platform: "TW".into(),
                    channel_id: "starsea".into(),
                    name: "重复".into(),
                },
            ],
            &roster,
        )
        .unwrap();
        assert_eq!(entries.len(), 2, "duplicate identities collapse");
        assert_eq!(entries[0].platform, "YT");
        assert_eq!(entries[0].name, "星海频道", "the roster name wins");
        assert_eq!(entries[1].channel_id, "starsea", "TW ids canonicalize");
        assert_eq!(
            validated_exempt_reasons(vec!["idle_recording".into(), "idle_recording".into()]),
            Ok(vec!["idle_recording".to_string()])
        );
        assert!(validated_exempt_reasons(vec!["not_a_reason".into()]).is_err());
        let unknown_channel = vec![ExemptChannel {
            platform: "NC".into(),
            channel_id: "ch1".into(),
            name: String::new(),
        }];
        let error = validated_exempt_channels(unknown_channel, &roster).unwrap_err();
        assert!(error.contains("不在监控列表"), "{error}");
    }

    #[test]
    fn config_response_projects_the_exemption_settings_and_the_picker_roster() {
        let mut cfg = crate::test_support::config("local", 0);
        cfg.warning_hold_exempt = vec![crate::plugins::warning_exempt::ExemptChannel {
            platform: "YT".into(),
            channel_id: "UC1".into(),
            name: "星海频道".into(),
        }];
        cfg.warning_hold_exempt_reasons = vec!["game_mode_off".into()];
        let response = config_response(&cfg);
        assert_eq!(response["warning_hold_exempt"][0]["platform"], "YT");
        assert_eq!(response["warning_hold_exempt"][0]["channel_id"], "UC1");
        assert_eq!(
            response["warning_hold_exempt_reasons"],
            json!(["game_mode_off"])
        );
        assert_eq!(response["warning_hold_exempt_all_low_risk"], json!(false));
        assert!(
            response["warning_hold_exempt_channels"].is_array(),
            "the picker roster must always be a list"
        );
    }

    #[test]
    fn config_form_values_carries_the_exemption_settings() {
        let mut cfg = crate::test_support::config("local", 0);
        cfg.warning_hold_exempt = vec![crate::plugins::warning_exempt::ExemptChannel {
            platform: "YT".into(),
            channel_id: "UC1".into(),
            name: "星海频道".into(),
        }];
        cfg.warning_hold_exempt_reasons = vec!["game_mode_off".into()];
        let values = config_form_values(&cfg);
        assert_eq!(values["warning_hold_exempt"][0]["platform"], "YT");
        assert_eq!(values["warning_hold_exempt"][0]["channel_id"], "UC1");
        assert_eq!(values["warning_hold_exempt"][0]["name"], "星海频道");
        assert_eq!(
            values["warning_hold_exempt_reasons"],
            json!(["game_mode_off"])
        );
        assert_eq!(values["warning_hold_exempt_all_low_risk"], json!(false));
    }

    #[test]
    fn the_global_low_risk_switch_requires_the_low_risk_tick() {
        assert!(validated_exempt_all_low_risk(&[], false).is_ok());
        assert!(
            validated_exempt_all_low_risk(&[], true).is_err(),
            "the switch without the idle tick would silently do nothing"
        );
        assert!(validated_exempt_all_low_risk(&["idle_recording".into()], true).is_ok());
        assert!(
            validated_exempt_all_low_risk(&["game_mode_off".into()], true).is_err(),
            "a mid/high-only list cannot back the low-risk switch"
        );
    }
}
