pub(crate) mod warning;

use self::warning::{warning_tracker, BroadcastSource, WarningReleaseReason};
use super::twitch::get_twitch_status;
use super::youtube::{get_youtube_area_topic, get_youtube_status};
use crate::cluster::{monitored_config_version, sync_monitored_config_after_change};
use crate::config::{load_config, save_config, Config};
use crate::logging::{safe_diagnostic, safe_text};
use crate::plugins::banned_keywords::{
    banned_keyword_hit, danmaku_banned_keywords, danmaku_haystack,
};
use crate::plugins::bilibili;
use crate::plugins::source_status::SourceStatus;
use lazy_static::lazy_static;
use serde::{Deserialize, Serialize};
use std::future::Future;
use std::io;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::Notify;

static DANMAKU_RUNNING: AtomicBool = AtomicBool::new(false);
static DANMAKU_STOP_SIGNAL: AtomicBool = AtomicBool::new(false);
static DANMAKU_LIFECYCLE: Mutex<()> = Mutex::new(());

lazy_static! {
    static ref DANMAKU_COMMANDS_ENABLED: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));
    static ref DANMAKU_STOP_NOTIFY: Notify = Notify::new();
    static ref CONFIG_UPDATED: AtomicBool = AtomicBool::new(false);
    static ref CONFIG_UPDATE_NOTIFY: Notify = Notify::new();
}

pub fn is_danmaku_running() -> bool {
    DANMAKU_RUNNING.load(Ordering::Relaxed)
}

pub fn set_danmaku_running(running: bool) {
    DANMAKU_RUNNING.store(running, Ordering::Relaxed);
}

pub fn is_danmaku_commands_enabled() -> bool {
    DANMAKU_COMMANDS_ENABLED.load(Ordering::Relaxed)
}

pub fn set_danmaku_commands_enabled(enabled: bool) {
    DANMAKU_COMMANDS_ENABLED.store(enabled, Ordering::Relaxed);
}

pub fn set_danmaku_stop_signal(stop: bool) {
    DANMAKU_STOP_SIGNAL.store(stop, Ordering::Relaxed);
    if stop {
        DANMAKU_STOP_NOTIFY.notify_waiters();
    }
}

pub fn should_stop_danmaku() -> bool {
    DANMAKU_STOP_SIGNAL.load(Ordering::Relaxed)
}

pub async fn wait_danmaku_stop_signal() {
    if should_stop_danmaku() {
        return;
    }

    let notified = DANMAKU_STOP_NOTIFY.notified();
    if should_stop_danmaku() {
        return;
    }
    notified.await;
}

/// The stop must interrupt the whole operation, including DNS, HTTP setup,
/// WebSocket writes and command processing, rather than just retry sleeps.
pub(crate) async fn until_danmaku_stopped<T>(operation: impl Future<Output = T>) -> Option<T> {
    tokio::select! {
        biased;
        _ = wait_danmaku_stop_signal() => None,
        result = operation => Some(result),
    }
}

struct DanmakuRunningGuard;

impl Drop for DanmakuRunningGuard {
    fn drop(&mut self) {
        set_danmaku_running(false);
    }
}

#[derive(Serialize, Deserialize, Clone)]
struct Platforms {
    youtube: Option<String>,
    twitch: Option<String>,
}

#[derive(Serialize, Deserialize, Clone)]
struct Channel {
    name: String,
    platforms: Platforms,
    riot_puuid: Option<String>,
    #[serde(default)]
    aliases: Vec<String>,
}

#[derive(Serialize, Deserialize, Clone)]
struct ChannelsConfig {
    channels: Vec<Channel>,
}

// Cache channels config to avoid repeated file reads and parsing
lazy_static! {
    static ref CHANNELS_CACHE: Mutex<Option<(ChannelsConfig, u64)>> = Mutex::new(None);
}

fn load_channels() -> Result<ChannelsConfig, Box<dyn std::error::Error>> {
    let store = crate::storage::global()?;
    let mut cache = CHANNELS_CACHE.lock().unwrap_or_else(|e| e.into_inner());
    let revision = store.revision("channels.json")?.unwrap_or(0);
    if let Some((config, saved)) = cache.as_ref() {
        if *saved == revision {
            return Ok(config.clone());
        }
    }
    let config: ChannelsConfig = crate::storage::read_document("channels.json")?;
    *cache = Some((config.clone(), revision));
    Ok(config)
}

pub fn get_channel_id(
    platform: &str,
    channel_name: &str,
) -> Result<Option<String>, Box<dyn std::error::Error>> {
    let config = load_channels()?;

    for channel in &config.channels {
        // Check both name and aliases without cloning whole channel (case-insensitive)
        if channel.name.to_lowercase() == channel_name.to_lowercase()
            || channel
                .aliases
                .iter()
                .any(|a| a.to_lowercase() == channel_name.to_lowercase())
        {
            return Ok(match platform {
                "YT" => channel.platforms.youtube.as_ref().map(|s| s.to_string()),
                "TW" => channel.platforms.twitch.as_ref().map(|s| s.to_string()),
                _ => None,
            });
        }
    }
    Ok(None)
}

pub fn get_channel_name(
    platform: &str,
    channel_id: &str,
) -> Result<Option<String>, Box<dyn std::error::Error>> {
    let config = load_channels()?;

    for channel in &config.channels {
        match platform {
            "YT" => {
                if let Some(id) = &channel.platforms.youtube {
                    if id == channel_id {
                        return Ok(Some(channel.name.clone()));
                    }
                }
            }
            "TW" => {
                if let Some(id) = &channel.platforms.twitch {
                    if id == channel_id {
                        return Ok(Some(channel.name.clone()));
                    }
                }
            }
            _ => return Ok(None),
        }
    }
    Ok(None)
}

pub fn get_puuid(channel_name: &str) -> Result<String, Box<dyn std::error::Error>> {
    let config = load_channels()?;

    for channel in &config.channels {
        // Check both name and aliases (case-insensitive)
        if channel.name.to_lowercase() == channel_name.to_lowercase()
            || channel
                .aliases
                .iter()
                .any(|a| a.to_lowercase() == channel_name.to_lowercase())
        {
            return Ok(channel
                .riot_puuid
                .as_ref()
                .map(|s| s.to_string())
                .unwrap_or_default());
        }
    }
    tracing::error!("PUUID not found for channel: {}", channel_name);
    Ok(String::new())
}

// Optional: Helper function to get all channels for a platform
pub fn get_all_channels(
    platform: &str,
) -> Result<Vec<(String, String)>, Box<dyn std::error::Error>> {
    let config = load_channels()?;
    let mut channels = Vec::new();

    for channel in &config.channels {
        match platform {
            "YT" => {
                if let Some(id) = &channel.platforms.youtube {
                    channels.push((channel.name.clone(), id.clone()));
                }
            }
            "TW" => {
                if let Some(id) = &channel.platforms.twitch {
                    channels.push((channel.name.clone(), id.clone()));
                }
            }
            _ => (),
        }
    }

    Ok(channels)
}

/// Updates the configuration JSON file with new channel values.
struct TargetUpdate {
    changed: bool,
    previous_version: String,
    config: Config,
}

async fn update_config(
    platform: &str,
    channel_name: &str,
    channel_id: &str,
    area_id: u64,
) -> Result<TargetUpdate, Box<dyn std::error::Error>> {
    prepare_target_update(
        load_config().await?,
        platform,
        channel_name,
        channel_id,
        area_id,
        |mut config| async move {
            save_config(&mut config).await?;
            Ok(config)
        },
    )
    .await
}

async fn prepare_target_update<Save, Saved>(
    mut config: Config,
    platform: &str,
    channel_name: &str,
    channel_id: &str,
    area_id: u64,
    save: Save,
) -> Result<TargetUpdate, Box<dyn std::error::Error>>
where
    Save: FnOnce(Config) -> Saved,
    Saved: Future<Output = Result<Config, Box<dyn std::error::Error>>>,
{
    let old_monitored_config_version = monitored_config_version(&config);
    let changed = apply_danmaku_target(&mut config, platform, channel_name, channel_id, area_id)?;
    if changed {
        config = save(config).await?;
    }
    Ok(TargetUpdate {
        changed,
        previous_version: old_monitored_config_version,
        config,
    })
}

#[cfg(test)]
pub(crate) mod warning_test_support {
    use super::*;
    pub(crate) static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    /// Fake only config persistence and reply/peer I/O; use the production
    /// target comparison, result branches and accepted-command effects.
    pub(crate) async fn accept_target(
        config: &mut Config,
        warnings: Arc<warning::WarningTracker>,
        platform: &str,
        name: &str,
        id: &str,
        area: u64,
    ) -> (usize, String) {
        let writes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let write_count = writes.clone();
        let update =
            prepare_target_update(config.clone(), platform, name, id, area, move |config| {
                write_count.fetch_add(1, Ordering::Relaxed);
                std::future::ready(Ok(config))
            })
            .await;
        *config = update.as_ref().unwrap().config.clone();
        let request = TargetRequest {
            id: NEXT_TARGET_REQUEST.fetch_add(1, Ordering::Relaxed),
            node: config.cluster.node_id.clone(),
            source: BroadcastSource::new(platform, name, id, None),
            area,
        };
        let outcome = DanmakuTargetLog {
            node: request.node.clone(),
            request_id: request.id,
            platform: platform.to_owned(),
            channel: name.to_owned(),
            area: Some((area, "示例分区".into())),
            ..Default::default()
        };
        let viewer_reply_override = held_channel_switch_notice(&warnings, &request.source);
        let reply_text = Arc::new(Mutex::new(String::new()));
        let reply_slot = reply_text.clone();
        finish_target_update(
            update,
            outcome,
            viewer_reply_override,
            move |_, _, _| async move {
                accept_target_request(&request);
                String::new()
            },
            move |message| {
                *reply_slot.lock().unwrap() = message;
                std::future::ready(())
            },
        )
        .await;
        let reply = reply_text.lock().unwrap().clone();
        (writes.load(Ordering::Relaxed), reply)
    }

    pub(crate) fn reset_trace() {
        *PENDING_TARGET
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
        clear_config_updated();
    }
}

fn apply_danmaku_target(
    config: &mut Config,
    platform: &str,
    channel_name: &str,
    channel_id: &str,
    area_id: u64,
) -> io::Result<bool> {
    let (current_name, current_id, current_area) = match platform {
        "YT" => (
            &mut config.youtube.channel_name,
            &mut config.youtube.channel_id,
            &mut config.youtube.area_v2,
        ),
        "TW" => (
            &mut config.twitch.channel_name,
            &mut config.twitch.channel_id,
            &mut config.twitch.area_v2,
        ),
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "unknown platform",
            ))
        }
    };
    // This comparison receives the corrected area, after live metadata has
    // been checked. A repeated command can therefore repair a saved wrong area.
    if current_name == channel_name && current_id == channel_id && *current_area == area_id {
        return Ok(false);
    }
    *current_name = channel_name.to_owned();
    *current_id = channel_id.to_owned();
    *current_area = area_id;
    Ok(true)
}

/// Load areas configuration from areas.json
fn load_areas_config() -> Option<serde_json::Value> {
    crate::storage::read_document("areas.json").ok()
}

/// Bilibili's catch-all 其他单机. The restream loop starts here when nothing
/// else has been chosen yet. A Holodex card still suggests it when a keyword
/// actually matched.
pub const DEFAULT_AREA_ID: u64 = 235;

/// Topic plus title, which is what Holodex cards and the restream loop both
/// feed into [`check_area_id_with_title`]. Underscores become spaces so a
/// Holodex `just_chatting` topic can hit a `just chatting` keyword.
pub fn stream_area_haystack(topic: Option<&str>, title: &str) -> String {
    let title = title.trim();
    match topic.map(str::trim).filter(|topic| !topic.is_empty()) {
        Some(topic) => format!("{topic} {title}"),
        None => title.to_string(),
    }
}

/// Determines the area id based on the live title by checking keywords from areas.json
pub fn check_area_id_with_title(live_title: &str, current_area_id: u64) -> u64 {
    matched_area_id_from_title(live_title).unwrap_or(current_area_id)
}

fn matched_area_id_from_title(live_title: &str) -> Option<u64> {
    area_id_matching_keywords(live_title, &load_areas_config()?)
}

fn area_id_matching_keywords(live_title: &str, areas_config: &serde_json::Value) -> Option<u64> {
    let title = live_title.to_lowercase().replace("_", " ");
    let areas = areas_config["areas"].as_array()?;
    for area in areas {
        let (Some(id), Some(keywords)) = (area["id"].as_u64(), area["title_keywords"].as_array())
        else {
            continue;
        };
        for keyword in keywords {
            let Some(kw) = keyword.as_str() else {
                continue;
            };
            let kw = kw.trim().to_lowercase();
            if !kw.is_empty() && title.contains(&kw) {
                return Some(id);
            }
        }
    }
    None
}

/// Suggested Holodex 切换 area from the current `areas.json` keywords.
///
/// Reads the file on every call so a keyword edit shows up on the next Holodex
/// refresh instead of waiting for a process restart. A keyword hit on 235
/// (其他单机) is still a suggestion; only a miss leaves the picker to open.
pub fn suggest_area_from_stream(topic: Option<&str>, title: &str) -> (Option<u64>, Option<String>) {
    match matched_area_id_from_title(&stream_area_haystack(topic, title)) {
        Some(id) => (Some(id), get_area_name(id)),
        None => (None, None),
    }
}

/// Resolve area alias to area name using areas.json
/// What the `%查询` reply should do about the public status page.
#[derive(Debug, PartialEq, Eq)]
enum QueryAdvert {
    Send(String),
    /// Configured, but longer than a danmaku carries. Saving through the Web UI
    /// rejects this, so it only happens with a hand-edited config.
    TooLong(String),
    None,
}

fn query_advert(cfg: &Config) -> QueryAdvert {
    if let Some(advert) = cfg.cluster.public_status.danmaku_advert() {
        return QueryAdvert::Send(advert);
    }

    let url = cfg.cluster.public_status.public_url.trim();
    if url.is_empty() {
        QueryAdvert::None
    } else {
        QueryAdvert::TooLong(url.to_string())
    }
}

fn resolve_area_alias(alias: &str) -> String {
    let alias_trimmed = alias.trim();
    let alias_lower = alias_trimmed.to_lowercase();

    // Load areas configuration
    let areas_config = match load_areas_config() {
        Some(config) => config,
        None => {
            tracing::debug!("无法加载 areas.json 配置");
            return alias_trimmed.to_string();
        }
    };

    // Check each area's aliases
    if let Some(areas) = areas_config["areas"].as_array() {
        for area in areas {
            if let Some(name) = area["name"].as_str() {
                let name_lower = name.to_lowercase();

                // First check if the input matches the area name itself
                if alias_lower == name_lower {
                    tracing::debug!("分区别名 '{}' 匹配到分区名称: {}", alias_trimmed, name);
                    return name.to_string();
                }

                // Then check aliases
                if let Some(aliases) = area["aliases"].as_array() {
                    for area_alias in aliases {
                        if let Some(a) = area_alias.as_str() {
                            if alias_lower == a.to_lowercase() {
                                tracing::debug!(
                                    "分区别名 '{}' 匹配到别名 '{}', 返回分区: {}",
                                    alias_trimmed,
                                    a,
                                    name
                                );
                                return name.to_string();
                            }
                        }
                    }
                }
            }
        }
    }

    tracing::debug!("未找到分区别名 '{}' 的匹配项", safe_text(alias_trimmed, 80));
    alias_trimmed.to_string()
}

/// Requested identity is resolved as the existing command checks progress.
/// One emitter owns the final accepted/no-op/rejected record; it does not
/// decide whether a command is allowed or mutate config/monitor state.
#[derive(Default)]
struct DanmakuTargetLog {
    node: String,
    platform: String,
    channel: String,
    area: Option<(u64, String)>,
    requested_area: Option<u64>,
    forced_without_title: bool,
    request_id: u64,
    /// The sender's display name, so the log line identifies the real sender
    /// instead of a role placeholder.
    sender: String,
    channel_id: String,
}

static NEXT_TARGET_REQUEST: AtomicU64 = AtomicU64::new(1);
static PENDING_TARGET: Mutex<Option<PendingTarget>> = Mutex::new(None);

#[derive(Clone)]
pub(crate) struct TargetRequest {
    pub id: u64,
    node: String,
    pub source: BroadcastSource,
    area: u64,
}

struct PendingTarget {
    request: TargetRequest,
    last_decision: Option<String>,
}

fn remember_target_request(request: TargetRequest) {
    let mut pending = PENDING_TARGET
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if pending
        .as_ref()
        .is_none_or(|old| old.request.id <= request.id)
    {
        *pending = Some(PendingTarget {
            request,
            last_decision: None,
        });
    }
}

fn warning_cut_switch_notice(channel_name: &str) -> String {
    format!("⚠️ {channel_name} 因警告/切断，请切换其他频道")
}

fn held_channel_switch_notice(
    warnings: &warning::WarningTracker,
    source: &BroadcastSource,
) -> Option<String> {
    warnings
        .snapshot()
        .holds(source)
        .then(|| warning_cut_switch_notice(&source.channel_name))
}

fn accept_target_request(request: &TargetRequest) {
    remember_target_request(request.clone());
    set_config_updated();
}

pub(crate) fn reject_disabled_danmaku_target(node: &str, command: &str, sender: &str) {
    let parts: Vec<_> = command.split('%').collect();
    DanmakuTargetLog {
        node: node.to_owned(),
        platform: parts.get(2).unwrap_or(&"").to_uppercase(),
        channel: parts.get(3).unwrap_or(&"").to_string(),
        request_id: NEXT_TARGET_REQUEST.fetch_add(1, Ordering::Relaxed),
        sender: sender.to_owned(),
        ..Default::default()
    }
    .reject("换台命令未启用");
}

/// Observability only: this request never changes rank or ffmpeg ownership.
pub(crate) fn pending_target_request(cfg: &Config) -> Option<TargetRequest> {
    let mut pending = PENDING_TARGET
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let request = &pending.as_ref()?.request;
    let (name, id, area) = match request.source.platform.as_str() {
        "YT" => (
            &cfg.youtube.channel_name,
            &cfg.youtube.channel_id,
            cfg.youtube.area_v2,
        ),
        "TW" => (
            &cfg.twitch.channel_name,
            &cfg.twitch.channel_id,
            cfg.twitch.area_v2,
        ),
        _ => return None,
    };
    if request.source.channel_name != *name
        || request.source.channel_id != warning::canonical_channel_id(&request.source.platform, id)
        || request.area != area
    {
        *pending = None;
        return None;
    }
    Some(request.clone())
}

pub(crate) fn report_target_decision(
    request: &TargetRequest,
    selected: Option<&BroadcastSource>,
    reason: &str,
) {
    let selected = selected.map_or_else(
        || "暂无可用频道".to_owned(),
        |source| {
            format!(
                "[{}] {}",
                safe_text(&source.platform, 8),
                safe_text(&source.channel_name, 64)
            )
        },
    );
    let decision = format!("{selected}；原因={reason}");
    {
        let mut pending = PENDING_TARGET
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(pending) = pending
            .as_mut()
            .filter(|pending| pending.request.id == request.id)
        else {
            return;
        };
        if pending.last_decision.as_deref() == Some(&decision) {
            return;
        }
        pending.last_decision = Some(decision.clone());
    }
    tracing::info!(
        "[{}] 弹幕 %转播% #{} 选台结果={}；请求=[{}] {}",
        safe_text(&request.node, 64),
        request.id,
        decision,
        safe_text(&request.source.platform, 8),
        safe_text(&request.source.channel_name, 64)
    );
}

enum DanmakuTargetOutcome<'a> {
    Updated(&'a str),
    Unchanged,
    Rejected(&'a str),
}

impl DanmakuTargetLog {
    fn emit(&self, outcome: DanmakuTargetOutcome<'_>) {
        let node = safe_text(
            if self.node.is_empty() {
                "local"
            } else {
                &self.node
            },
            64,
        );
        let origin = format!(
            "弹幕 %转播% #{}（{}）",
            self.request_id,
            if self.sender.is_empty() {
                "未知发送者".to_string()
            } else {
                safe_text(&self.sender, 96)
            }
        );
        let identity = if self.platform.is_empty() {
            format!("[{node}] {origin}")
        } else {
            format!(
                "[{node}] {origin} [{}] {}",
                safe_text(&self.platform, 16),
                safe_text(
                    if self.channel.is_empty() {
                        "未知频道"
                    } else {
                        &self.channel
                    },
                    64
                )
            )
        };
        let area = self.area.as_ref().map_or_else(String::new, |(id, name)| {
            format!("；分区={}({id})", safe_text(name, 48))
        });
        let (action, sync) = match outcome {
            DanmakuTargetOutcome::Updated(sync) => ("监听目标已更新", sync),
            DanmakuTargetOutcome::Unchanged => ("监听目标未变", ""),
            DanmakuTargetOutcome::Rejected(reason) => {
                tracing::warn!(
                    "{} 监听目标更新已拒绝{}；原因={}",
                    identity,
                    area,
                    safe_diagnostic(reason)
                );
                return;
            }
        };
        let mut detail = String::new();
        if let (Some(requested), Some((actual, _))) = (self.requested_area, &self.area) {
            if requested != *actual {
                detail.push_str(&format!("；按标题/游戏由分区 {requested} 修正"));
            }
        }
        if self.forced_without_title {
            detail.push_str("；主播允许无标题频道");
        }
        if !sync.is_empty() {
            detail.push_str(&safe_text(sync, 240));
        }
        tracing::info!("{} {}{}{}", identity, action, area, detail);
    }

    fn reject(&self, reason: &str) {
        self.emit(DanmakuTargetOutcome::Rejected(reason));
    }
}

/// Finish the existing config-result branches. The caller supplies its current
/// update effects and reply transport; reporting never decides those effects.
async fn finish_target_update<AfterUpdate, AfterFuture, Reply, ReplyFuture>(
    config_update: Result<TargetUpdate, Box<dyn std::error::Error>>,
    outcome: DanmakuTargetLog,
    viewer_reply_override: Option<String>,
    after_update: AfterUpdate,
    reply: Reply,
) where
    AfterUpdate: FnOnce(String, Config, bool) -> AfterFuture,
    AfterFuture: Future<Output = String>,
    Reply: FnOnce(String) -> ReplyFuture,
    ReplyFuture: Future<Output = ()>,
{
    let area_name = outcome
        .area
        .as_ref()
        .map(|(_, name)| name.as_str())
        .unwrap_or_default();
    match config_update {
        Ok(update) => {
            // Emit the accepted-save fact before waking selection or waiting
            // for peers. Acknowledging a target is not proof that it is pushed.
            outcome.emit(if update.changed {
                DanmakuTargetOutcome::Updated("")
            } else {
                DanmakuTargetOutcome::Unchanged
            });
            let changed = update.changed;
            let sync_message = after_update(update.previous_version, update.config, changed).await;
            if !sync_message.is_empty() {
                tracing::info!(
                    "{} 弹幕 %转播% #{} 目标同步结果{}",
                    safe_text(&outcome.node, 64),
                    outcome.request_id,
                    safe_text(&sync_message, 240)
                );
            }
            let message = viewer_reply_override.unwrap_or_else(|| {
                if changed {
                    format!(
                        "更新：{} - {} - {}",
                        outcome.platform, outcome.channel, area_name
                    )
                } else {
                    format!(
                        "{} 监听对象已是：{} - {}",
                        outcome.platform, outcome.channel, area_name
                    )
                }
            });
            reply(message).await;
        }
        Err(error) => {
            outcome.reject(&format!("更新配置时出错: {error}"));
            reply(format!("错误：更新配置时出错 {error}")).await;
        }
    }
}

/// One owner-facing sentence for a banned keyword hit; the command reply and
/// the auto-monitor alert share it, so the channel comes first and the reader
/// cannot mistake it for a bad area argument.
pub(crate) fn banned_keyword_text(channel_name: &str, platform: &str, keyword: &str) -> String {
    format!(
        "不可点播：{} 在 {} 的标题/分区含关键词: {}。",
        safe_text(channel_name, 32),
        safe_text(platform, 8),
        safe_text(keyword, 32)
    )
}

/// The messages for one banned-keyword hit: the sentence plus `网页详情：<url>`
/// when both fit the send cap, otherwise the URL as its own send so neither
/// part is truncated.
fn banned_keyword_messages(main: String, advert: Option<String>) -> Vec<String> {
    match advert {
        Some(advert)
            if crate::config::danmaku_width(&main) + crate::config::danmaku_width(&advert)
                <= crate::config::DANMAKU_MAX_WIDTH =>
        {
            vec![format!("{main}{advert}")]
        }
        Some(advert) => vec![main, advert],
        None => vec![main],
    }
}

/// Send the banned-keyword notice. The detail line rides in the same danmaku
/// when it fits; otherwise it goes as a second send (Bilibili cooldown) rather
/// than being cut off.
pub(crate) async fn send_banned_keyword_notice(
    cfg: &Config,
    channel_name: &str,
    platform: &str,
    keyword: &str,
) -> Result<(), String> {
    let main = banned_keyword_text(channel_name, platform, keyword);
    let mut last_error = None;
    for (index, message) in
        banned_keyword_messages(main, cfg.cluster.public_status.danmaku_advert())
            .iter()
            .enumerate()
    {
        if index > 0 {
            tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
        }
        if let Err(error) = bilibili::send_danmaku(cfg, message).await {
            last_error = Some(error.to_string());
        }
    }
    last_error.map_or(Ok(()), Err)
}

/// Processes a single danmaku command.
pub async fn process_danmaku(command: &str) {
    process_danmaku_with_owner(command, false, "").await;
}

/// Processes a single danmaku command with owner flag and sender identity
/// (`uid=… uname`, used only for the log line).
pub async fn process_danmaku_with_owner(command: &str, is_owner: bool, sender: &str) {
    if !command.starts_with(" :") {
        return;
    }
    // tracing::info!("弹幕:{}", &command[2..]);
    let command = command.replace(" ", "").replace("　", "");
    let normalized_danmaku = command.replace("％", "%");

    let cfg = match load_config().await {
        Ok(cfg) => cfg,
        Err(e) => {
            tracing::error!(
                "加载配置失败，忽略弹幕命令: {}",
                safe_diagnostic(&e.to_string())
            );
            return;
        }
    };
    // Add check for 查询 command
    if normalized_danmaku.contains("%查询") {
        // tracing::info!("查询命令收到");
        let channel_name = cfg.youtube.channel_name.clone();
        let area_name = area_name_or_unknown(cfg.youtube.area_v2);
        let _ =
            bilibili::send_danmaku(&cfg, &format!("YT: {} - {}", channel_name, area_name)).await;
        let channel_name = cfg.twitch.channel_name.clone();
        let area_name = area_name_or_unknown(cfg.twitch.area_v2);
        // bilibili 发送弹幕cooldown > 1秒
        tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
        let _ =
            bilibili::send_danmaku(&cfg, &format!("TW: {} - {}", channel_name, area_name)).await;

        // Advertising the status page rides on %查询 only: error replies and
        // switch confirmations already spend the cooldown, and repeating a link
        // on every failed command would just be spam.
        //
        // The advert is sent by whichever node runs the danmaku client, not by
        // the node serving the page — the two are usually different, and this
        // is the one in the live chat.
        match query_advert(&cfg) {
            QueryAdvert::Send(advert) => {
                tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
                let _ = bilibili::send_danmaku(&cfg, &advert).await;
            }
            QueryAdvert::TooLong(url) => {
                // send_danmaku does not check length: Bilibili rejects an
                // over-length message with a non-zero code that every caller
                // discards, so without this the line would vanish silently.
                tracing::warn!("公开状态页地址过长，%查询 未播报：{}", url);
            }
            QueryAdvert::None => {}
        }
        return;
    }

    // Continue with existing command processing for %转播% commands
    if !normalized_danmaku.contains("%转播%") {
        // Not a command, ignore silently
        return;
    }

    // tracing::info!("转播命令收到: {}", normalized_danmaku);
    let danmaku_command = normalized_danmaku.replace(" :", "");
    let mut outcome = DanmakuTargetLog {
        node: cfg.cluster.node_id.clone(),
        request_id: NEXT_TARGET_REQUEST.fetch_add(1, Ordering::Relaxed),
        sender: sender.to_owned(),
        ..Default::default()
    };
    let warnings = warning_tracker();

    // Replace full-width ％ with half-width %
    let parts: Vec<&str> = danmaku_command.split('%').collect();
    // tracing::info!("弹幕:{:?}", parts);
    if parts.len() < 5 {
        outcome.reject("弹幕命令格式错误");
        let _ = bilibili::send_danmaku(&cfg, "错误：弹幕命令格式错误").await;
        return;
    }

    let platform = parts[2].to_uppercase();
    outcome.platform = platform.clone();
    outcome.channel = parts[3].to_string();
    if platform.to_uppercase() != "YT" && platform.to_uppercase() != "TW" {
        outcome.reject("不支持的平台");
        let _ = bilibili::send_danmaku(&cfg, "错误：弹幕命令格式错误").await;
        return;
    }
    let channel_name = parts[3];
    let area_alias = parts[4];

    if area_alias.is_empty() {
        outcome.reject("分区不能为空");
        let _ = bilibili::send_danmaku(&cfg, "错误：分区不能为空").await;
        return;
    }

    tracing::debug!("原始分区输入: '{}'", safe_text(area_alias, 80));
    let area_name = resolve_area_alias(area_alias);
    tracing::debug!("解析后的分区名称: '{}'", safe_text(&area_name, 80));

    let area_id = match get_area_id(&area_name) {
        Ok(id) => id,
        Err(e) => {
            outcome.reject(&e.to_string());
            let _ = bilibili::send_danmaku(&cfg, &format!("错误：{}", e)).await;
            return;
        }
    };

    outcome.area = Some((area_id, area_name.clone()));
    outcome.requested_area = Some(area_id);

    if platform.eq("YT") || platform.eq("TW") {
        let channel_id = match get_channel_id(&platform, channel_name) {
            Ok(id) => id,
            Err(e) => {
                outcome.reject(&format!("检查频道时出错: {e}"));
                let _ = bilibili::send_danmaku(&cfg, &format!("错误：检查频道时出错 {}", e)).await;
                return;
            }
        };

        if channel_id.is_none() {
            outcome.reject("频道未在当前平台列表中");
            let _ = bilibili::send_danmaku(
                &cfg,
                &format!("错误：频道 {} 未在{}列表中", channel_name, platform),
            )
            .await;
            return;
        }

        let Some(channel_id_str) = channel_id.as_deref() else {
            outcome.reject("频道未在当前平台列表中");
            let _ = bilibili::send_danmaku(
                &cfg,
                &format!("错误：频道 {} 未在{}列表中", channel_name, platform),
            )
            .await;
            return;
        };
        let resolved_channel_name = match get_channel_name(&platform, channel_id_str) {
            Ok(Some(name)) => name,
            Ok(None) => {
                outcome.reject(&format!("频道 ID {} 未在当前平台列表中", channel_id_str));
                let _ = bilibili::send_danmaku(
                    &cfg,
                    &format!("错误：频道 ID {} 未在{}列表中", channel_id_str, platform),
                )
                .await;
                return;
            }
            Err(e) => {
                outcome.reject(&format!("获取频道名称时出错: {e}"));
                return;
            }
        };
        outcome.channel = resolved_channel_name.clone();
        outcome.channel_id = warning::canonical_channel_id(&platform, channel_id_str);

        let (live_title, live_topic, youtube_video_id) = if platform.eq_ignore_ascii_case("YT") {
            // get youtube live status
            match get_youtube_status(channel_id_str).await {
                Ok(SourceStatus {
                    topic,
                    title,
                    video_id,
                    ..
                }) => {
                    let t = match title {
                        Some(t) => t,
                        None => {
                            if is_owner {
                                // Owner can force switch even without title
                                outcome.forced_without_title = true;
                                tracing::debug!("主播允许无标题的 YT 频道");
                                "无标题直播".to_string()
                            } else {
                                outcome.reject("获取 YT 直播标题失败");
                                let _ =
                                    bilibili::send_danmaku(&cfg, "错误：获取YT直播标题失败").await;
                                tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
                                let _ =
                                    bilibili::send_danmaku(&cfg, "请确认是否已开（预告）窗").await;
                                return;
                            }
                        }
                    };
                    (t, topic.unwrap_or_default(), video_id)
                }
                Err(e) => {
                    outcome.reject(&format!("获取 YT 直播标题时出错: {e}"));
                    let _ =
                        bilibili::send_danmaku(&cfg, &format!("错误：获取YT直播标题时出错 {}", e))
                            .await;
                    return;
                }
            }
        } else {
            // TW
            match get_twitch_status(channel_id_str).await {
                Ok((is_live, topic, title, _)) => {
                    if !is_live {
                        outcome.reject("TW 频道未在直播");
                        let _ = bilibili::send_danmaku(
                            &cfg,
                            &format!("错误: {} 未在直播", resolved_channel_name),
                        )
                        .await;
                        return;
                    }

                    let t = match title {
                        Some(t) => t,
                        None => {
                            if is_owner {
                                // Owner can force switch even without title
                                outcome.forced_without_title = true;
                                tracing::debug!("主播允许无标题的 TW 频道");
                                "无标题直播".to_string()
                            } else {
                                outcome.reject("获取 TW 直播标题失败");
                                let _ =
                                    bilibili::send_danmaku(&cfg, "错误：获取TW直播标题失败").await;
                                return;
                            }
                        }
                    };
                    (t, topic.unwrap_or_default(), None)
                }
                Err(e) => {
                    outcome.reject(&format!("获取 TW 状态时出错: {e}"));
                    let _ =
                        bilibili::send_danmaku(&cfg, &format!("错误：获取TW直播标题时出错 {}", e))
                            .await;
                    return;
                }
            }
        };
        // `%转播%` names a channel, not a video. For YT this title is the live
        // stream or the soonest upcoming one (`select_holodex_channel_status`),
        // so a later clean title on the same channel cannot bypass the list.
        let live_topic_title = danmaku_haystack(&live_topic, &live_title);

        if let Some(keyword) = banned_keyword_hit(&live_topic_title, &danmaku_banned_keywords()) {
            outcome.reject(&format!("直播标题/分区包含不支持的关键词: {}", keyword));
            let _ = send_banned_keyword_notice(&cfg, channel_name, &platform, &keyword).await;
            return;
        }

        // Area matching needs the same game/topic that powers WebUI suggestions.
        // Keep this optional lookup separate from command validation and liveness.
        let area_topic = if platform == "YT" && live_topic.trim().is_empty() {
            get_youtube_area_topic(channel_id_str, youtube_video_id.as_deref()).await
        } else {
            Some(live_topic)
        };
        let updated_area_id = check_area_id_with_title(
            &stream_area_haystack(area_topic.as_deref(), &live_title),
            area_id,
        );

        let updated_area_name = match get_area_name(updated_area_id) {
            Some(name) => name,
            None => {
                outcome.reject(&format!(
                    "无法获取修正后的分区名称（ID: {updated_area_id}）"
                ));
                let _ = bilibili::send_danmaku(&cfg, "错误：无法获取更新后的分区名称").await;
                return;
            }
        };
        outcome.area = Some((updated_area_id, updated_area_name.clone()));

        if updated_area_id != area_id {
            tracing::debug!(
                "按直播标题/游戏关键词修正弹幕分区: {} -> {} (ID: {})",
                safe_text(&area_name, 48),
                safe_text(&updated_area_name, 48),
                updated_area_id
            );
        }

        let config_update = update_config(
            &platform,
            &resolved_channel_name,
            channel_id_str,
            updated_area_id,
        )
        .await;
        let request = TargetRequest {
            id: outcome.request_id,
            node: outcome.node.clone(),
            source: BroadcastSource::new(
                &platform,
                &resolved_channel_name,
                &outcome.channel_id,
                None,
            ),
            area: updated_area_id,
        };
        let viewer_reply_override = held_channel_switch_notice(&warnings, &request.source);
        finish_target_update(
            config_update,
            outcome,
            viewer_reply_override,
            |old_monitored_config_version, updated_config, changed| async move {
                crate::config::with_current_config(&updated_config, || {
                    // Wake ranking for a validated command, including an
                    // unchanged target. A matching warning hold stays.
                    accept_target_request(&request)
                });
                if let Some(active) = warnings.active_source() {
                    report_target_decision(
                        &request,
                        Some(&active),
                        "当前转播继续，下一轮按自动顺序选台",
                    );
                }

                if changed
                    && old_monitored_config_version != monitored_config_version(&updated_config)
                {
                    sync_monitored_config_after_change(&updated_config).await
                } else {
                    String::new()
                }
            },
            |message| async move {
                let _ = bilibili::send_danmaku(&cfg, &message).await;
            },
        )
        .await;
    } else {
        outcome.reject("不支持的平台");
        let _ = bilibili::send_danmaku(&cfg, &format!("错误：不支持的平台 {}", platform)).await;
    }
}

/// Main function to start the danmaku client in the background.
/// The client runs continuously and monitors for WARNING/CUT_OFF messages.
/// Danmaku commands are only processed when enabled via set_danmaku_commands_enabled().
pub fn run_danmaku() {
    let _lifecycle = DANMAKU_LIFECYCLE.lock().unwrap_or_else(|e| e.into_inner());
    if DANMAKU_RUNNING
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        tracing::warn!("弹幕客户端已在运行");
        return;
    }

    set_danmaku_stop_signal(false);

    let running_guard = DanmakuRunningGuard;
    let spawned = std::thread::Builder::new()
        .name("bilistream-danmaku".to_string())
        .spawn(move || {
            let _running_guard = running_guard;
            let rt = match tokio::runtime::Runtime::new() {
                Ok(rt) => rt,
                Err(e) => {
                    tracing::error!("创建弹幕客户端运行时失败: {}", e);
                    return;
                }
            };
            rt.block_on(async {
                tracing::info!("启动弹幕客户端");
                until_danmaku_stopped(async {
                    let cfg = match load_config().await {
                        Ok(cfg) => cfg,
                        Err(e) => {
                            tracing::error!("加载弹幕客户端配置失败: {}", e);
                            return;
                        }
                    };
                    let room_id = cfg.bililive.room;

                    // Create danmaku client config
                    let danmaku_config = crate::plugins::danmaku_client::DanmakuConfig {
                        room_id: room_id as u64,
                        sessdata: cfg.bililive.credentials.sessdata.clone(),
                        bili_jct: cfg.bililive.credentials.bili_jct.clone(),
                        dede_user_id: cfg.bililive.credentials.dede_user_id.clone(),
                        dede_user_id_ckmd5: cfg.bililive.credentials.dede_user_id_ckmd5.clone(),
                        buvid3: cfg.bililive.credentials.buvid3.clone(),
                    };

                    // Wrap config in Arc for sharing across tasks
                    let cfg_arc = Arc::new(cfg);
                    // Use the global DANMAKU_COMMANDS_ENABLED Arc
                    let enable_commands = DANMAKU_COMMANDS_ENABLED.clone();

                    // Run danmaku client - it will keep running
                    if let Err(e) = crate::plugins::danmaku_client::run_native_danmaku_client(
                        danmaku_config,
                        cfg_arc,
                        enable_commands,
                    )
                    .await
                    {
                        tracing::error!("弹幕客户端错误: {}", e);
                    }
                })
                .await;
                tracing::info!("弹幕客户端已停止");
            });
            // A cancelled DNS lookup can leave a blocking resolver task behind.
            // It must not hold up stopping this client or starting its replacement.
            rt.shutdown_timeout(Duration::from_secs(1));
        });
    if let Err(error) = spawned {
        tracing::error!("创建弹幕客户端线程失败: {}", error);
    }
}

/// Enable or disable danmaku command processing.
/// The client continues to monitor for WARNING/CUT_OFF regardless of this setting.
pub fn enable_danmaku_commands(enabled: bool) {
    if is_danmaku_commands_enabled() == enabled {
        return;
    }
    set_danmaku_commands_enabled(enabled);
    if enabled {
        tracing::info!("弹幕命令已启用");
    } else {
        tracing::info!("弹幕命令已禁用");
    }
    // The public page keys 切换 on this gate. Bust the 5s snapshot so a
    // poll that lands after 转播 ends (and the "可使用弹幕指令进行换台"
    // danmaku) does not keep serving the closed state.
    crate::webui::public::snapshot::invalidate_public_status_snapshot();
    crate::webui::state::request_status_refresh();
}

/// Stop the danmaku client
pub async fn stop_danmaku() {
    {
        let _lifecycle = DANMAKU_LIFECYCLE.lock().unwrap_or_else(|e| e.into_inner());
        if !is_danmaku_running() {
            return;
        }

        if !should_stop_danmaku() {
            tracing::info!("停止弹幕客户端");
        }
        set_danmaku_stop_signal(true);
    }
    enable_danmaku_commands(false);

    // Wait for the client to stop gracefully without blocking the runtime.
    let mut attempts = 0;
    while is_danmaku_running() && attempts < 20 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        attempts += 1;
    }

    if is_danmaku_running() {
        tracing::warn!("弹幕客户端停止超时，保持停止信号等待后台退出");
    } else {
        tracing::info!("弹幕客户端已成功停止");
        // Only the next run clears the signal. A concurrent stop must not
        // clear a cancellation that another caller is still relying on.
    }
}

/// Set the warning stop flag and store the channel that was stopped
pub fn set_warning_stop(channel_name: String) {
    let warnings = warning_tracker();
    let mut event = warnings.capture_event();
    warnings.mark_event(&mut event, Some(&format!("【转播】{channel_name}")));
}

/// Check if we should skip streaming due to a recent warning
pub fn should_skip_due_to_warning(channel_name: &str) -> bool {
    warning_tracker().snapshot().holds_name(channel_name)
}

/// The provider source a platform code names in this config. The status cards,
/// the release endpoint and the runtime all ask here, so none of them can
/// disagree about which channel a code means.
pub(crate) fn configured_broadcast_source(cfg: &Config, platform: &str) -> Option<BroadcastSource> {
    match platform {
        "YT" => Some(BroadcastSource::new(
            "YT",
            &cfg.youtube.channel_name,
            &cfg.youtube.channel_id,
            None,
        )),
        "TW" => Some(BroadcastSource::new(
            "TW",
            &cfg.twitch.channel_name,
            &cfg.twitch.channel_id,
            None,
        )),
        "NC" => {
            let (channel_id, channel_name) =
                crate::plugins::niconico_channel_identity(&cfg.niconico);
            Some(BroadcastSource::new("NC", &channel_name, &channel_id, None))
        }
        _ => None,
    }
}

/// If the exemption releases the current hold for `source`, the matched
/// reason id (for the R8 log line and the R4 rail record). Read-only: it
/// never logs or records, so status cards can ask it every poll.
pub(crate) fn exempt_hold_release(
    cfg: &Config,
    warning: &warning::WarningSnapshot,
    source: &BroadcastSource,
) -> Option<crate::plugins::warning_exempt::ReasonId> {
    use crate::plugins::warning_exempt::ReasonId;
    let reason_ids = warning.hold_reasons();
    if !warning.holds(source) {
        return None;
    }
    let decision = crate::plugins::warning_exempt::HoldDecision {
        exempt: &cfg.warning_hold_exempt,
        allowed_reasons: &cfg.warning_hold_exempt_reasons,
        all_low_risk: cfg.warning_hold_exempt_all_low_risk,
        platform: &source.platform,
        channel_id: &source.channel_id,
        // Empty classify is the high-risk `unrecognized` settings class.
        reason_ids: &reason_ids,
        // The rail costs a document read plus a peer scan; ask only after the
        // cheap configuration conditions pass.
        rail_hit: false,
        room_lock: false,
    };
    if !decision.configured_release() {
        return None;
    }
    if crate::plugins::warning_exempt::rail_hit(
        &source.platform,
        &source.channel_id,
        warning.episode_id_for(source),
        crate::storage::now(),
    ) {
        return None;
    }
    Some(
        reason_ids
            .iter()
            .next()
            .copied()
            .unwrap_or(ReasonId::Unrecognized),
    )
}

/// Release an exempt hold at the moment the runtime would skip the source:
/// records the R4 rail first (a failed record keeps the hold, preserving the
/// one-restart guarantee), then clears the hold and logs the R8 line so every
/// later `holds()` check (ffmpeg spawn, priority transition) sees the restart.
/// Returns true only when a hold was actually released.
pub(crate) fn release_exempt_hold(
    cfg: &Config,
    warnings: &warning::WarningTracker,
    source: &BroadcastSource,
) -> bool {
    let snapshot = warnings.snapshot();
    let Some(reason) = exempt_hold_release(cfg, &snapshot, source) else {
        return false;
    };
    let episode = snapshot.episode_id_for(source).map(str::to_owned);
    if let Err(error) = crate::plugins::warning_exempt::record_restart(
        &source.platform,
        &source.channel_id,
        episode.as_deref(),
        reason.as_str(),
        crate::storage::now(),
    ) {
        tracing::warn!(
            "{}；保持跳过频道 {}",
            safe_text(&error, 160),
            safe_text(&source.channel_name, 64)
        );
        return false;
    }
    // A frame marked during the rail write leaves a newer hold: the release
    // refuses, the rail spend stands (conservative) and the caller falls
    // through to the normal skip.
    let Some(released) = warnings.release_exempt(&snapshot, source) else {
        return false;
    };
    let reason_ids = released
        .reason_ids
        .iter()
        .map(|id| id.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    tracing::info!(
        "频道 {} 已豁免警告/切断暂停；原因分类={}",
        safe_text(&released.channel, 64),
        reason_ids
    );
    crate::webui::state::request_status_refresh();
    true
}

/// Whether a hold still keeps this source from starting: the raw hold plus a
/// still-failing exemption decision. Read-only (no log, record or release).
pub(crate) fn source_is_held(
    cfg: &Config,
    warning: &warning::WarningSnapshot,
    source: &BroadcastSource,
) -> bool {
    warning.holds(source) && exempt_hold_release(cfg, warning, source).is_none()
}

/// Whether this process still holds that card's source after a warning/cut.
pub(crate) fn source_warning_held(cfg: &Config, platform: &str) -> bool {
    let Some(source) = configured_broadcast_source(cfg, platform) else {
        return false;
    };
    let warning = warning_tracker().snapshot();
    warning.holds_identity(&source) && exempt_hold_release(cfg, &warning, &source).is_none()
}

pub(crate) fn priority_warning_held(cfg: &Config) -> bool {
    priority_warning_matches(&warning_tracker().snapshot(), cfg)
}

pub(crate) fn priority_warning_matches(warning: &warning::WarningSnapshot, cfg: &Config) -> bool {
    [
        BroadcastSource::new(
            "YT",
            &cfg.priority_channel.channel_name,
            &cfg.priority_channel.youtube_channel_id,
            None,
        ),
        BroadcastSource::new(
            "TW",
            &cfg.priority_channel.channel_name,
            &cfg.priority_channel.twitch_channel_id,
            None,
        ),
    ]
    .iter()
    .any(|source| warning.holds(source) && exempt_hold_release(cfg, warning, source).is_none())
}

/// Check if we should skip streaming due to a recent warning (returns true only on first check for logging)
pub fn should_skip_due_to_warned(channel_name: &str) -> bool {
    warning_tracker().take_skip_log(&BroadcastSource::new("", channel_name, "", None))
}

/// Explicit restart clears the process-local hold. Ordinary target edits do not.
pub fn clear_warning_stop() {
    if let Some(release) = warning_tracker().clear(WarningReleaseReason::ManualRestart) {
        release.log();
    }
}

/// Set the config updated flag to skip waiting interval
pub fn set_config_updated() {
    CONFIG_UPDATED.store(true, Ordering::SeqCst);
    CONFIG_UPDATE_NOTIFY.notify_waiters();
}

/// Check if config was updated (to skip waiting)
pub fn is_config_updated() -> bool {
    CONFIG_UPDATED.load(Ordering::SeqCst)
}

/// Clear the config updated flag
pub fn clear_config_updated() {
    CONFIG_UPDATED.store(false, Ordering::SeqCst);
}

/// Wait until either config changes or the timeout expires.
pub async fn wait_config_update_or_timeout(timeout: Duration) -> bool {
    if is_config_updated() {
        return true;
    }

    let notified = CONFIG_UPDATE_NOTIFY.notified();
    if is_config_updated() {
        return true;
    }

    tokio::select! {
        _ = notified => true,
        _ = tokio::time::sleep(timeout) => is_config_updated(),
    }
}

pub fn get_area_name(area_id: u64) -> Option<String> {
    let areas: serde_json::Value = crate::storage::read_document("areas.json").ok()?;

    if let Some(areas_array) = areas["areas"].as_array() {
        for area in areas_array {
            if let (Some(id), Some(name)) = (area["id"].as_u64(), area["name"].as_str()) {
                if id == area_id {
                    return Some(name.to_string());
                }
            }
        }
    }

    tracing::error!("未知的分区ID: {}", area_id);
    None
}

fn area_name_or_unknown(area_id: u64) -> String {
    get_area_name(area_id).unwrap_or_else(|| format!("未知分区({})", area_id))
}

fn get_area_id(area_name: &str) -> Result<u64, Box<dyn std::error::Error>> {
    let area_name_trimmed = area_name.trim();
    let areas: serde_json::Value = crate::storage::read_document("areas.json")?;

    let area_name_lower = area_name_trimmed.to_lowercase();

    if let Some(areas_array) = areas["areas"].as_array() {
        for area in areas_array {
            if let (Some(id), Some(name)) = (area["id"].as_u64(), area["name"].as_str()) {
                if name.to_lowercase() == area_name_lower {
                    tracing::debug!("找到分区 '{}' 的ID: {}", area_name_trimmed, id);
                    return Ok(id);
                }
            }
        }
    }

    tracing::debug!(
        "未知的分区: '{}' (已尝试匹配)",
        safe_text(area_name_trimmed, 80)
    );
    Err(format!("未知的分区: {}", area_name_trimmed).into())
}

pub fn get_aliases(target_name: &str) -> Result<Vec<String>, Box<dyn std::error::Error>> {
    let channels = load_channels()?;
    Ok(channels
        .channels
        .iter()
        .find(|c| c.name == target_name)
        .map(|c| c.aliases.clone())
        .unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tracing::instrument::WithSubscriber;

    #[test]
    fn the_exemption_releases_a_held_card_only_when_every_class_is_ticked() {
        use crate::plugins::danmaku::warning::{BroadcastSource, WarningTracker};
        use crate::plugins::warning_exempt::{ExemptChannel, ReasonId};

        let tracker = Arc::new(WarningTracker::default());
        let source = BroadcastSource::new("YT", "星海频道", "UC1", Some("live-1"));
        let _owner = tracker.own_source(7, source.clone());
        let mut event = tracker.capture_event();
        event.set_reason_ids([ReasonId::IdleRecording].into_iter().collect());
        tracker.mark_event(&mut event, None).unwrap();
        let warning = tracker.snapshot();

        let mut cfg = crate::test_support::config("local", 0);
        cfg.warning_hold_exempt = vec![ExemptChannel::new("YT", "UC1", "星海频道").unwrap()];
        cfg.warning_hold_exempt_reasons = vec!["idle_recording".into()];
        assert_eq!(
            exempt_hold_release(&cfg, &warning, &source),
            Some(ReasonId::IdleRecording)
        );
        // Every matched class is required.
        cfg.warning_hold_exempt_reasons = vec!["game_mode_off".into()];
        assert_eq!(exempt_hold_release(&cfg, &warning, &source), None);
        // The channel must be on the exempt list.
        cfg.warning_hold_exempt_reasons = vec!["idle_recording".into()];
        cfg.warning_hold_exempt.clear();
        assert_eq!(exempt_hold_release(&cfg, &warning, &source), None);

        // A hold without a classified reason (mask / no reason / unknown text)
        // releases only when `unrecognized` is ticked for a listed channel.
        let tracker = Arc::new(WarningTracker::default());
        let _owner = tracker.own_source(8, source.clone());
        let mut event = tracker.capture_event();
        tracker.mark_event(&mut event, None).unwrap();
        let unknown = tracker.snapshot();
        cfg.warning_hold_exempt = vec![ExemptChannel::new("YT", "UC1", "星海频道").unwrap()];
        cfg.warning_hold_exempt_reasons = vec!["idle_recording".into()];
        assert_eq!(exempt_hold_release(&cfg, &unknown, &source), None);
        cfg.warning_hold_exempt_reasons = vec!["unrecognized".into()];
        assert_eq!(
            exempt_hold_release(&cfg, &unknown, &source),
            Some(ReasonId::Unrecognized)
        );
        cfg.warning_hold_exempt.clear();
        assert_eq!(exempt_hold_release(&cfg, &unknown, &source), None);
    }

    #[tokio::test]
    async fn target_result_branches_preserve_outcome_reply_and_effect_order() {
        let mut updated = crate::test_support::config("ca", 0);
        assert!(apply_danmaku_target(
            &mut updated,
            "YT",
            "示例频道001",
            "example_channel_001",
            235
        )
        .unwrap());
        let rejected: Result<TargetUpdate, Box<dyn std::error::Error>> =
            Err("保存失败 HTTP 502".into());
        for (result, expected, expected_reply) in [
            (
                Ok(TargetUpdate {
                    changed: true,
                    previous_version: "previous-version".into(),
                    config: updated.clone(),
                }),
                "监听目标已更新",
                "更新：YT - 示例频道001 - 其他单机",
            ),
            (
                Ok(TargetUpdate {
                    changed: false,
                    previous_version: "previous-version".into(),
                    config: updated,
                }),
                "监听目标未变",
                "YT 监听对象已是：示例频道001 - 其他单机",
            ),
            (
                rejected,
                "监听目标更新已拒绝",
                "错误：更新配置时出错 保存失败 HTTP 502",
            ),
        ] {
            let (dispatch, logs) = crate::logging::test_capture::subscriber(tracing::Level::DEBUG);
            let context = DanmakuTargetLog {
                node: "ca".into(),
                platform: "YT".into(),
                channel: "示例频道001".into(),
                area: Some((235, "其他单机".into())),
                requested_area: Some(107),
                forced_without_title: false,
                ..Default::default()
            };
            let effects = Arc::new(Mutex::new(Vec::new()));
            let updated_effects = Arc::clone(&effects);
            let reply_effects = Arc::clone(&effects);
            let updated_logs = logs.clone();
            let reply_logs = logs.clone();
            finish_target_update(
                result,
                context,
                None,
                move |old_version, config, changed| {
                    assert_eq!(old_version, "previous-version");
                    assert_eq!(config.youtube.channel_id, "example_channel_001");
                    assert_eq!(config.youtube.area_v2, 235);
                    assert!(updated_logs.text().contains(expected));
                    updated_effects.lock().unwrap().push("after-update");
                    std::future::ready(if changed {
                        "；已同步到 2 个节点".to_string()
                    } else {
                        String::new()
                    })
                },
                move |message| {
                    assert_eq!(message, expected_reply);
                    assert!(reply_logs.text().contains(expected));
                    reply_effects.lock().unwrap().push("reply");
                    std::future::ready(())
                },
            )
            .with_subscriber(dispatch)
            .await;
            let output = logs.text();
            assert_eq!(
                output.lines().count(),
                if expected == "监听目标已更新" {
                    2
                } else {
                    1
                },
                "{output}"
            );
            assert!(output.contains("[ca] 弹幕 %转播% #0（未知发送者） [YT] 示例频道001"));
            assert!(output.contains("分区=其他单机(235)"));
            assert!(output.contains(expected));
            if expected == "监听目标更新已拒绝" {
                assert!(output.contains(" WARN [danmaku]"));
                assert!(output.contains("保存失败 HTTP 502"));
                assert!(!output.contains("监听目标已更新"));
            } else {
                assert!(output.contains(" INFO [danmaku]"));
                assert!(output.contains("由分区 107 修正"));
            }
            assert!(!output.contains("转播成功"));
            assert!(!output.contains("原始分区输入"));
            assert!(!output.contains("解析后的分区名称"));
            if expected == "监听目标已更新" {
                assert_eq!(*effects.lock().unwrap(), ["after-update", "reply"]);
                assert!(output.contains("已同步到 2 个节点"));
            } else {
                assert_eq!(
                    *effects.lock().unwrap(),
                    if expected == "监听目标未变" {
                        vec!["after-update", "reply"]
                    } else {
                        vec!["reply"]
                    }
                );
                assert!(!output.contains("已同步到"));
            }
        }
    }

    #[tokio::test]
    async fn accepted_held_targets_keep_the_hold_and_ask_the_viewer_to_switch() {
        use warning::{BroadcastSource, WarningTracker};
        let _serial = warning_test_support::SERIAL.lock().await;
        warning_test_support::reset_trace();
        let mut cfg = crate::test_support::config("ca", 0);
        cfg.youtube.channel_name = "示例频道 001".into();
        cfg.youtube.channel_id = "UCexample001".into();
        cfg.youtube.area_v2 = 329;
        cfg.twitch.channel_name = "示例频道 002".into();
        cfg.twitch.channel_id = "example002".into();
        cfg.twitch.area_v2 = 86;
        let warnings = Arc::new(WarningTracker::default());
        let held = BroadcastSource::new(
            "YT",
            &cfg.youtube.channel_name,
            &cfg.youtube.channel_id,
            Some("episode-1"),
        );
        let _owner = warnings.own_source(1, held.clone());
        warnings
            .mark_event(&mut warnings.capture_event(), None)
            .unwrap();
        let (dispatch, logs) = crate::logging::test_capture::subscriber(tracing::Level::INFO);
        async {
            assert_eq!(
                warning_test_support::accept_target(
                    &mut cfg,
                    warnings.clone(),
                    "TW",
                    "示例频道 002",
                    "example002",
                    86
                )
                .await,
                (0, "TW 监听对象已是：示例频道 002 - 示例分区".into())
            );
            assert!(warnings.snapshot().holds(&held));
            assert_eq!(
                warning_test_support::accept_target(
                    &mut cfg,
                    warnings.clone(),
                    "YT",
                    "示例频道 001",
                    "UCexample001",
                    329
                )
                .await,
                (0, "⚠️ 示例频道 001 因警告/切断，请切换其他频道".into())
            );
            assert!(warnings.snapshot().holds(&held));
            assert!(is_config_updated());
        }
        .with_subscriber(dispatch)
        .await;
        let output = logs.text();
        assert_eq!(output.matches("监听目标未变").count(), 2);
        assert!(!output.contains("临时跳过已解除"));
        assert!(!output.contains("弹幕明确重选该频道"));
        assert!(!output.contains("同步"));
        warning_test_support::reset_trace();
    }

    #[tokio::test]
    async fn same_source_command_keeps_the_hold_when_its_display_name_has_changed() {
        let _serial = warning_test_support::SERIAL.lock().await;
        warning_test_support::reset_trace();
        let warnings = Arc::new(warning::WarningTracker::default());
        let _owner = warnings.own_source(
            1,
            BroadcastSource::new("TW", "旧显示名称", "SomeLogin", Some("stream-1")),
        );
        warnings
            .mark_event(&mut warnings.capture_event(), None)
            .unwrap();
        let mut cfg = crate::test_support::config("ca", 0);
        cfg.twitch.channel_name = "新显示名称".into();
        cfg.twitch.channel_id = "somelogin".into();
        cfg.twitch.area_v2 = 86;
        assert_eq!(
            warning_test_support::accept_target(
                &mut cfg,
                warnings.clone(),
                "TW",
                "新显示名称",
                "somelogin",
                86
            )
            .await,
            (0, "⚠️ 新显示名称 因警告/切断，请切换其他频道".into())
        );
        assert!(warnings.snapshot().has_hold());
        warning_test_support::reset_trace();
    }

    #[tokio::test]
    async fn a_failed_target_save_never_executes_warning_release_effects() {
        let warnings = Arc::new(warning::WarningTracker::default());
        let source = BroadcastSource::new("YT", "示例频道 001", "UCexample001", Some("stream-1"));
        let _owner = warnings.own_source(1, source.clone());
        warnings
            .mark_event(&mut warnings.capture_event(), None)
            .unwrap();
        let cfg = crate::test_support::config("ca", 0);
        let result =
            prepare_target_update(cfg, "YT", "示例频道 001", "UCexample001", 329, |_| async {
                Err("synthetic save failure".into())
            })
            .await;
        finish_target_update(
            result,
            DanmakuTargetLog::default(),
            None,
            |_, _, _| async {
                panic!("a failed write must not run accepted-command effects");
            },
            |_| std::future::ready(()),
        )
        .await;
        assert!(warnings.snapshot().holds(&source));
    }

    #[test]
    fn target_update_and_rejection_keep_unsafe_details_out_of_real_records() {
        let (dispatch, logs) = crate::logging::test_capture::subscriber(tracing::Level::DEBUG);
        let context = DanmakuTargetLog {
            node: "jp".into(),
            platform: "TW".into(),
            channel: "示例频道002\nERROR forged\u{1b}[31m".into(),
            area: Some((235, "其他单机".into())),
            sender: "示例观众\nforged".into(),
            ..Default::default()
        };
        tracing::dispatcher::with_default(&dispatch, || {
            context.emit(DanmakuTargetOutcome::Updated(
                "；集群同步失败 HTTP 530 Cookie: SESSDATA=SYNTHETIC-COOKIE",
            ));
            context.reject("读取失败 HTTP 403 https://user:SYNTHETIC-PROXY@proxy.invalid/path\nAuthorization: Bearer SYNTHETIC-TOKEN");
        });
        let output = logs.text();
        assert_eq!(output.lines().count(), 2, "{output}");
        assert!(output.lines().next().unwrap().contains(" INFO [danmaku]"));
        assert!(output.lines().nth(1).unwrap().contains(" WARN [danmaku]"));
        assert!(output.contains("HTTP 530"));
        assert!(output.contains("HTTP 403"));
        assert!(!output.contains("SYNTHETIC"));
        assert!(!output.contains("proxy.invalid"));
        assert!(!output.contains('\u{1b}'));
    }

    #[tokio::test]
    async fn stopping_cancels_inflight_setup_and_leaves_the_signal_set() {
        struct Reset;
        impl Drop for Reset {
            fn drop(&mut self) {
                set_danmaku_running(false);
                set_danmaku_stop_signal(false);
            }
        }
        let _reset = Reset;
        set_danmaku_stop_signal(false);
        set_danmaku_running(true);
        let (started, ready) = tokio::sync::oneshot::channel();
        let operation = tokio::spawn(async move {
            let _running = DanmakuRunningGuard;
            until_danmaku_stopped(async {
                started.send(()).unwrap();
                // Simulates a resolver, API request, WebSocket write or
                // message handler that never completes on its own.
                std::future::pending::<()>().await;
            })
            .await
        });
        ready.await.unwrap();
        tokio::time::timeout(Duration::from_secs(1), stop_danmaku())
            .await
            .expect("stop must cancel setup before its network timeout");
        assert!(operation.await.unwrap().is_none());
        assert!(!is_danmaku_running());
        assert!(should_stop_danmaku());
        assert!(
            until_danmaku_stopped(async { panic!("stopped client must not reconnect") })
                .await
                .is_none()
        );

        set_danmaku_stop_signal(false);
        assert_eq!(until_danmaku_stopped(async { 42 }).await, Some(42));
    }

    fn config_with_public_url(url: &str) -> Config {
        let mut cfg = crate::test_support::config("jp", 0);
        cfg.cluster.public_status.public_url = url.to_string();
        cfg
    }

    #[test]
    fn no_public_url_means_no_advert() {
        assert_eq!(query_advert(&config_with_public_url("")), QueryAdvert::None);
        assert_eq!(
            query_advert(&config_with_public_url("   ")),
            QueryAdvert::None
        );
    }

    #[test]
    fn a_configured_url_is_advertised_with_the_prefix() {
        assert_eq!(
            query_advert(&config_with_public_url("s.example.com")),
            QueryAdvert::Send("网页详情：s.example.com".to_string())
        );
    }

    /// A URL that pushes the advert past the 80-character cap is reported, as
    /// send_danmaku does not check length, so this must be caught here.
    #[test]
    fn an_over_length_url_is_reported_rather_than_sent() {
        let url = "https://aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        assert_eq!(
            query_advert(&config_with_public_url(url)),
            QueryAdvert::TooLong(url.to_string())
        );
    }

    #[test]
    fn gta_topic_corrects_a_repeated_other_online_games_request_to_235() {
        let title =
            "【#SURGETown 】Cafe ニャイトメア素敵な悪夢よ届け!【 #折咲もしゅ #MOSHULIVE #REJECT】";
        let areas = serde_json::json!({"areas": [
            { "id": 107, "name": "其他网游", "title_keywords": [] },
            { "id": 235, "name": "其他单机", "title_keywords": [
                "pokemon", "core keeper", "terraria", "tgc card shop simulator",
                "stardew valley", "rust", "gta"
            ], "aliases": ["单机", "其他游戏"] }
        ]});
        let requested_area = 107;
        assert_eq!(area_id_matching_keywords(title, &areas), None);
        let corrected_area =
            area_id_matching_keywords(&stream_area_haystack(Some("GTA"), title), &areas)
                .unwrap_or(requested_area);
        assert_eq!(
            corrected_area, 235,
            "a keyword hit on 235 is a correction, not a fallback"
        );

        let mut cfg: Config = serde_json::from_value(serde_json::json!({
            "auto_cover": false, "enable_anti_collision": false, "interval": 15,
            "bililive": { "enable_danmaku_command": true, "room": 1, "bili_rtmp_url": "", "bili_rtmp_key": "" },
            "youtube": { "channel_id": "moshu", "channel_name": "折咲もしゅ", "area_v2": requested_area },
            "twitch": { "channel_id": "moshu", "channel_name": "折咲もしゅ", "area_v2": requested_area },
            "enable_lol_monitor": false, "anti_collision_list": {}
        })).unwrap();
        for platform in ["YT", "TW"] {
            assert!(apply_danmaku_target(
                &mut cfg,
                platform,
                "折咲もしゅ",
                "moshu",
                corrected_area
            )
            .unwrap());
            assert!(!apply_danmaku_target(
                &mut cfg,
                platform,
                "折咲もしゅ",
                "moshu",
                corrected_area
            )
            .unwrap());
        }
        assert_eq!(cfg.youtube.area_v2, 235);
        assert_eq!(cfg.twitch.area_v2, 235);
    }

    #[test]
    fn unmatched_metadata_keeps_the_requested_area() {
        let areas = serde_json::json!({"areas": [
            { "id": 235, "title_keywords": ["gta", "rust"] }
        ]});
        for topic in [None, Some("unmapped game")] {
            let area =
                area_id_matching_keywords(&stream_area_haystack(topic, "初めてのゲーム"), &areas)
                    .unwrap_or(107);
            assert_eq!(area, 107);
        }
    }

    #[test]
    fn a_keyword_hit_on_the_catchall_is_still_a_match() {
        let areas = serde_json::json!({
            "areas": [
                { "id": 235, "title_keywords": ["rust"] },
                { "id": 252, "title_keywords": ["tarkov", "タルコフ"] }
            ]
        });
        assert_eq!(
            area_id_matching_keywords("RUST】レオラス最終日", &areas),
            Some(DEFAULT_AREA_ID)
        );
        assert_eq!(
            area_id_matching_keywords("【Escape from Tarkov】タルコフ", &areas),
            Some(252)
        );
        assert_eq!(area_id_matching_keywords("雑談します", &areas), None);
    }

    #[tokio::test]
    async fn config_update_wait_wakes_on_signal() {
        clear_config_updated();

        let waiter = tokio::spawn(wait_config_update_or_timeout(Duration::from_secs(5)));
        tokio::time::sleep(Duration::from_millis(10)).await;
        set_config_updated();

        assert!(waiter.await.unwrap());
        clear_config_updated();
    }

    #[test]
    fn a_banned_keyword_notice_names_the_channel_and_splits_a_long_detail() {
        let main = banned_keyword_text("英リサ", "YT", "apex");
        assert_eq!(main, "不可点播：英リサ 在 YT 的标题/分区含关键词: apex。");
        let advert = Some("网页详情：status.detteee.xyz".to_string());
        assert_eq!(
            banned_keyword_messages(main.clone(), advert.clone()),
            vec![
                "不可点播：英リサ 在 YT 的标题/分区含关键词: apex。网页详情：status.detteee.xyz"
                    .to_string()
            ]
        );
        // A long sentence pushes the combined line past the cap, so the
        // detail goes as its own send instead of being truncated.
        let long = format!(
            "不可点播：{} 在 YT 的标题/分区含关键词: apex。",
            "频".repeat(20)
        );
        assert_eq!(
            banned_keyword_messages(long.clone(), advert),
            vec![long, "网页详情：status.detteee.xyz".to_string()]
        );
        assert_eq!(banned_keyword_messages(main, None).len(), 1);

        let dirty = banned_keyword_text("频\n道", "YT", "k\u{1b}[31m");
        assert!(!dirty.contains('\n'), "{dirty}");
        assert!(!dirty.contains('\u{1b}'), "{dirty}");
    }

    #[test]
    fn the_target_log_uses_the_sender_identity_instead_of_a_role() {
        let (dispatch, logs) = crate::logging::test_capture::subscriber(tracing::Level::DEBUG);
        let context = DanmakuTargetLog {
            node: "jp".into(),
            platform: "YT".into(),
            channel: "示例频道001".into(),
            sender: "示例观众".into(),
            ..Default::default()
        };
        tracing::dispatcher::with_default(&dispatch, || {
            context.emit(DanmakuTargetOutcome::Unchanged);
        });
        let output = logs.text();
        assert!(output.contains("（示例观众）"), "{output}");
        assert!(!output.contains("（观众）"), "{output}");
    }
}
