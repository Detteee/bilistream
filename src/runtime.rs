mod launch;

use launch::init_logger;
pub use launch::{cli_main, request_shutdown, BackendRuntime};

use crate as bilistream;

use crate::plugins::source_status::SourceStatus;
use bilistream::cluster::{self, ClusterStreamIdentity};
use bilistream::config::{load_config, save_config, Config};
use bilistream::events::BroadcastEndCause;
use bilistream::logging::{safe_diagnostic, safe_text};
use bilistream::notify::live_events::LiveSource;
use bilistream::plugins::bilibili::get_thumbnail;
use bilistream::plugins::danmaku::warning::{
    canonical_channel_id, log_warning_mark, warning_tracker, BroadcastSource, SourceObservation,
    WarningSnapshot,
};
use bilistream::plugins::Niconico as NiconicoClient;
use bilistream::plugins::Twitch as TwitchClient;
use bilistream::plugins::Youtube as YoutubeClient;
use bilistream::plugins::{
    announce_room_lock, bili_change_live_title, bili_start_live, bili_stop_live, bili_update_area,
    bilibili, check_area_id_with_title, clear_config_updated, clear_manual_restart,
    clear_manual_stop, current_game_riot_ids, enable_danmaku_commands, ffmpeg, get_aliases,
    get_area_name, get_bili_live_status, get_bili_live_time, get_bili_room_lock, get_puuid,
    is_config_updated, is_danmaku_commands_enabled, is_danmaku_running, is_ffmpeg_running,
    niconico_channel_identity, niconico_configured, resolve_playable_priority_channel, run_danmaku,
    send_danmaku, set_manual_restart, should_skip_due_to_warning, stop_danmaku, stop_ffmpeg,
    stop_restart_generation, store_prefetched_playable_stream, streamlink_ingest,
    take_prefetched_playable_stream, timed_room_lock_wait, wait_config_update_or_timeout,
    was_manual_restart, was_manual_stop, FfmpegCacheOptions, FfmpegSource, FfmpegStreamIdentity,
    PipedIngest, PriorityChannelPlatform, BILI_START_TEMP_BAN_PREFIX,
};
use chrono::{DateTime, Local, NaiveDateTime};
use regex::Regex;
use std::borrow::Cow;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::{error::Error, time::Duration};
use textwrap;
use unicode_width::UnicodeWidthStr;

// Graceful shutdown function
async fn graceful_shutdown() {
    // Stop ffmpeg process
    stop_ffmpeg().await;
}

static NO_LIVE: AtomicBool = AtomicBool::new(false);
// Use compact representation to reduce memory footprint
static LAST_MESSAGE: Mutex<Option<Box<str>>> = Mutex::new(None);
static LAST_COLLISION: Mutex<Option<Box<str>>> = Mutex::new(None);
const DUAL_COLLISION_PLATFORM: &str = "双平台";
const MESSAGE_TIME_PATTERN: &str = r"\d{4}-\d{2}-\d{2} \d{2}:\d{2}:\d{2}";
const MESSAGE_TIME_FORMAT: &str = "%Y-%m-%d %H:%M:%S";
const MESSAGE_TIME_PLACEHOLDER: &str = "TIME";
const MESSAGE_UPDATE_TIME_THRESHOLD_MINUTES: i64 = 5;
// Track last video/stream ID for cover change detection (works across platforms)
static LAST_VIDEO_ID: Mutex<Option<String>> = Mutex::new(None);
// Track last banned keyword warning to prevent spam
static LAST_BANNED_KEYWORD_WARNING: Mutex<VecDeque<BannedNotice>> = Mutex::new(VecDeque::new());

struct BannedNotice {
    platform: StreamPlatform,
    channel_id: String,
    content: String,
}

fn remember_banned_notice(
    notices: &mut VecDeque<BannedNotice>,
    stream: &StreamCandidate,
    keyword: &str,
    title: &str,
) -> bool {
    let content = format!("{keyword}:{title}");
    if let Some(previous) = notices
        .iter_mut()
        .find(|notice| notice.platform == stream.platform && notice.channel_id == stream.channel_id)
    {
        if previous.content == content {
            return false;
        }
        previous.content = content;
        return true;
    }
    // Two YouTube IDs, two Twitch IDs and one Niconico source per pass.
    if notices.len() == 5 {
        notices.pop_front();
    }
    notices.push_back(BannedNotice {
        platform: stream.platform,
        channel_id: stream.channel_id.clone(),
        content,
    });
    true
}

fn recover_mutex_lock<'a, T>(lock: &'a Mutex<T>, name: &str) -> MutexGuard<'a, T> {
    lock.lock().unwrap_or_else(|poisoned| {
        tracing::warn!("Recovering poisoned {name} mutex");
        poisoned.into_inner()
    })
}

fn normalized_api_key(key: Option<&str>) -> Option<String> {
    key.map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn area_label(area_id: u64) -> String {
    get_area_name(area_id).unwrap_or_else(|| format!("未知分区(ID: {})", area_id))
}

fn disable_monitors_after_bili_start_temp_ban(cfg: &mut Config) -> bool {
    let mut config_changed = false;

    if cfg.youtube.enable_monitor || cfg.enable_youtube_monitor {
        cfg.youtube.enable_monitor = false;
        cfg.enable_youtube_monitor = false;
        config_changed = true;
    }

    if cfg.twitch.enable_monitor || cfg.enable_twitch_monitor {
        cfg.twitch.enable_monitor = false;
        cfg.enable_twitch_monitor = false;
        config_changed = true;
    }

    if cfg.niconico.enable_monitor {
        cfg.niconico.enable_monitor = false;
        config_changed = true;
    }

    if cfg.priority_channel.enabled {
        cfg.priority_channel.enabled = false;
        config_changed = true;
    }

    config_changed
}

/// Waits out a timed room lock. Returns true when the caller should restart the
/// monitor pass instead of starting the room or shutting monitors off.
///
/// A lock with no `lock_till` is not a timed ban; the caller keeps the untimed
/// start-failure path.
async fn wait_out_bili_room_lock(room: i32) -> bool {
    if room <= 0 {
        return false;
    }
    let warnings = warning_tracker();
    let mut warning = warnings.snapshot_event();
    let lock = match get_bili_room_lock(room).await {
        Ok(lock) => lock,
        Err(error) => {
            tracing::warn!("获取B站房间锁定状态失败: {}", error);
            return false;
        }
    };
    let Some(wait) = timed_room_lock_wait(&lock) else {
        return false;
    };
    let title = if warning.has_source() {
        None
    } else {
        get_bili_live_status(room)
            .await
            .ok()
            .map(|status| status.title)
    };
    if let Some(name) = warnings.mark_timed_event(&mut warning, title.as_deref()) {
        log_warning_mark(&name);
    }
    announce_room_lock(room, &lock);
    crate::webui::state::update_status_cache_with(|status| {
        status.bilibili.apply_room_lock(&lock);
    });
    wait_config_update_or_timeout(wait).await;
    true
}

#[derive(PartialEq)]
enum CollisionResult {
    Continue,
    Proceed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StreamPlatform {
    Youtube,
    Twitch,
    Niconico,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FfmpegLoopExitReason {
    SourceEnded,
    BiliStopped,
    IntentionalRestart {
        target_m3u8_available: bool,
    },
    ClusterHandoff,
    /// A manual stop or restart arrived while confirming the source had ended.
    EndCheckCancelled {
        restart: bool,
    },
    /// The source came back with a different broadcast (stream/video id or
    /// Niconico program id); the outer loop sets it up from scratch.
    NewBroadcast,
}

/// 结束原因 for the 统计 history (design §5.4): only the seven codes.
fn history_end_reason(
    reason: FfmpegLoopExitReason,
    manual_stop: bool,
) -> crate::history::EndReason {
    use crate::history::EndReason;
    match reason {
        FfmpegLoopExitReason::SourceEnded | FfmpegLoopExitReason::NewBroadcast => {
            EndReason::SourceEnded
        }
        FfmpegLoopExitReason::BiliStopped if manual_stop => EndReason::UserStop,
        FfmpegLoopExitReason::BiliStopped => EndReason::BiliStopped,
        FfmpegLoopExitReason::IntentionalRestart { .. }
        | FfmpegLoopExitReason::EndCheckCancelled { restart: true } => {
            EndReason::IntentionalRestart
        }
        FfmpegLoopExitReason::EndCheckCancelled { restart: false } => EndReason::UserStop,
        FfmpegLoopExitReason::ClusterHandoff => EndReason::ClusterHandoff,
    }
}

fn restart_exit_should_skip_end_danmaku(reason: FfmpegLoopExitReason) -> bool {
    matches!(
        reason,
        FfmpegLoopExitReason::IntentionalRestart {
            target_m3u8_available: true
        } | FfmpegLoopExitReason::ClusterHandoff
            | FfmpegLoopExitReason::EndCheckCancelled { .. }
            | FfmpegLoopExitReason::NewBroadcast
    )
}

/// The crop setting carries over only into a restarted stream; a stop, an
/// ended source or a new broadcast clears it.
fn exit_keeps_crop(reason: FfmpegLoopExitReason) -> bool {
    matches!(
        reason,
        FfmpegLoopExitReason::IntentionalRestart {
            target_m3u8_available: true
        } | FfmpegLoopExitReason::ClusterHandoff
            | FfmpegLoopExitReason::EndCheckCancelled { restart: true }
    )
}

/// The stop/restart signals and cluster gate read at the moment the source
/// m3u8 refetch misses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct EndSignals {
    /// This node may execute: cluster off, or it is the active node and not
    /// in the middle of a takeover (read-only `local_monitoring_allowed`).
    node_active: bool,
    /// The stop/restart generation moved past the value captured when this
    /// ffmpeg session started. Flags left over from before do not count.
    stop_or_restart_requested: bool,
    /// `MANUAL_RESTART` is set, which tells a restart apart from a stop.
    restart_flag: bool,
}

fn end_signals(
    session_generation: u64,
    generation: u64,
    restart_flag: bool,
    node_active: bool,
) -> EndSignals {
    EndSignals {
        node_active,
        stop_or_restart_requested: generation != session_generation,
        restart_flag,
    }
}

/// Captures the stop/restart generation for an ffmpeg session. The push loop
/// calls this right before each spawn; tests call the same function.
fn capture_session_generation() -> u64 {
    stop_restart_generation()
}

/// Reads the live end signals against a captured session generation.
fn current_end_signals(session_generation: u64, node_active: bool) -> EndSignals {
    end_signals(
        session_generation,
        stop_restart_generation(),
        was_manual_restart(),
        node_active,
    )
}

/// Unknown ids on either side are treated as the same broadcast.
fn same_broadcast(session: Option<&str>, reading: Option<&str>) -> bool {
    match (session, reading) {
        (Some(session), Some(reading)) => session == reading,
        _ => true,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EndRecheckCancel {
    ClusterHandoff,
    ManualStop,
    ManualRestart,
}

/// Handoff (standby / mid-takeover) wins; then a request made during this
/// session is a restart when `MANUAL_RESTART` is set, otherwise a stop.
fn end_recheck_cancel(signals: EndSignals) -> Option<EndRecheckCancel> {
    if !signals.node_active {
        Some(EndRecheckCancel::ClusterHandoff)
    } else if !signals.stop_or_restart_requested {
        None
    } else if signals.restart_flag {
        Some(EndRecheckCancel::ManualRestart)
    } else {
        Some(EndRecheckCancel::ManualStop)
    }
}

/// The source refetch made after ffmpeg exits.
#[derive(Clone, Debug, PartialEq, Eq)]
enum RefetchReading {
    /// The source answered and is live.
    Live {
        m3u8_url: Option<String>,
        stream_id: Option<String>,
    },
    /// The source answered and is not live: no new m3u8. This is the end.
    Offline,
    /// The status request itself failed (timeout, 5xx, transport). Not an end.
    Error,
}

impl RefetchReading {
    fn from_result<E>(result: Result<SourceStatus, E>) -> Self {
        match result {
            Ok(SourceStatus {
                is_live: true,
                m3u8_url,
                video_id: stream_id,
                ..
            }) => RefetchReading::Live {
                m3u8_url,
                stream_id,
            },
            Ok(_) => RefetchReading::Offline,
            Err(_) => RefetchReading::Error,
        }
    }
}

/// Signals for marking the stream ended after the loop (M1). A restart whose
/// target is not live moved the generation itself, so that one exit skips the
/// generation check; `end_action` already chose to announce. The active-node
/// check and the once-per-broadcast `announced` flag still apply.
fn post_loop_end_signals(reason: FfmpegLoopExitReason, mut signals: EndSignals) -> EndSignals {
    if matches!(reason, FfmpegLoopExitReason::IntentionalRestart { .. }) {
        signals.stop_or_restart_requested = false;
    }
    signals
}

/// How often the push loop samples whether data is moving.
const PUSH_SAMPLE_INTERVAL: Duration = Duration::from_secs(5);

/// One reading of the push stats the status page shows (`stream_bitrate_kbps`,
/// `stream_speed`, `stream_time_secs`).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct PushSample {
    bitrate_kbps: Option<f32>,
    speed: Option<f32>,
    push_time_secs: Option<u32>,
}

fn current_push_sample() -> PushSample {
    let stats = bilistream::plugins::get_ffmpeg_network_stats();
    PushSample {
        bitrate_kbps: stats.push_bitrate_kbps,
        speed: bilistream::plugins::get_ffmpeg_speed(),
        push_time_secs: stats.push_time_secs,
    }
}

/// Time an ffmpeg session spent moving data. An interval counts only when
/// both ends show bitrate > 0 and speed > 0 (the page's 转播中 rule) and the
/// stream time advanced across it (progress).
#[derive(Clone, Debug, Default)]
struct PushMovingTime {
    last: Option<(std::time::Instant, PushSample)>,
    moving: Duration,
}

impl PushMovingTime {
    fn sample(&mut self, now: std::time::Instant, sample: PushSample) {
        let moving = |s: &PushSample| {
            s.bitrate_kbps.is_some_and(|b| b > 0.0) && s.speed.is_some_and(|v| v > 0.0)
        };
        if let Some((then, prev)) = self.last {
            let progressed = matches!(
                (prev.push_time_secs, sample.push_time_secs),
                (Some(a), Some(b)) if b > a
            );
            if moving(&prev) && moving(&sample) && progressed {
                self.moving += now.saturating_duration_since(then);
            }
        }
        self.last = Some((now, sample));
    }
}

/// What the push loop does after one refetch.
#[derive(Clone, Debug, PartialEq, Eq)]
enum RefetchStep {
    /// Same broadcast still live: restart ffmpeg (with the fresh URL if any).
    Restart { m3u8_url: Option<String> },
    /// Request failed: go through the normal ffmpeg retry, nothing ends.
    Retry,
    /// The stream ended. `announce` is false when 直播结束 already went out
    /// for this broadcast. `timed_out` marks an end after
    /// `STREAM_END_ERROR_WINDOW` of request errors.
    Ended { announce: bool, timed_out: bool },
    /// Live with a different broadcast id: back to the outer loop.
    NewBroadcast,
    /// Miss while a stop/restart arrived or this node is not active.
    Cancelled(EndRecheckCancel),
}

/// The one shared "stream ended" state. It drives the 直播结束 danmaku, the
/// 换台 command gate (`DANMAKU_COMMANDS_ENABLED`), and through that gate the
/// public status page's `bilibili.enable_danmaku_command` switch button.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct StreamEndState {
    /// Broadcast this state belongs to (`platform:channel:stream id`).
    stream_key: Option<String>,
    /// The id of that broadcast, for `same_broadcast`.
    stream_id: Option<String>,
    /// The active node saw a real source-offline miss and nothing has come
    /// back live since.
    ended: bool,
    /// 直播结束 was already sent for this broadcast.
    announced: bool,
    /// Start of the current run of refetch request errors with no clear
    /// answer from the source. Cleared by any clear answer.
    errors_since: Option<std::time::Instant>,
}

/// Request errors alone for this long (no clear "live" answer) end the stream.
/// There is no other retry limit in the push loop; see the changelog.
const STREAM_END_ERROR_WINDOW: Duration = Duration::from_secs(600);

/// An ffmpeg session that pushed at least this long counts as a healthy push:
/// the stream was up, so an earlier run of status errors no longer counts
/// toward `STREAM_END_ERROR_WINDOW` (Clerk N1).
const HEALTHY_PUSH_CLEARS_ERRORS: Duration = Duration::from_secs(60);

impl StreamEndState {
    /// A stream is starting. A different broadcast clears everything; the same
    /// broadcast coming back only clears `ended`, so the gate closes while it
    /// is live and a later miss does not announce twice.
    fn start_session(&mut self, stream_key: String, stream_id: Option<&str>) {
        // Only a known, equal id is the same broadcast here: an unknown id
        // must not inherit the earlier `announced` (L1).
        let same = self.stream_key.as_deref() == Some(stream_key.as_str())
            && matches!((self.stream_id.as_deref(), stream_id), (Some(a), Some(b)) if a == b);
        if !same {
            self.announced = false;
        }
        self.stream_key = Some(stream_key);
        self.stream_id = stream_id.map(str::to_string);
        self.ended = false;
        self.errors_since = None;
    }

    /// Called when an ffmpeg session ends, before its refetch, with the time
    /// that session spent actually moving data (`PushMovingTime`). The error
    /// window only counts while the stream is down, so a healthy push of
    /// `HEALTHY_PUSH_CLEARS_ERRORS` or more clears it; a shorter one does not.
    fn on_push_ended(&mut self, pushed_for: Duration) {
        if pushed_for >= HEALTHY_PUSH_CLEARS_ERRORS {
            self.errors_since = None;
        }
    }

    fn on_refetch(
        &mut self,
        reading: RefetchReading,
        signals: EndSignals,
        now: std::time::Instant,
    ) -> RefetchStep {
        if !matches!(reading, RefetchReading::Error) {
            self.errors_since = None;
        }
        match reading {
            RefetchReading::Error => {
                let since = *self.errors_since.get_or_insert(now);
                if now.saturating_duration_since(since) < STREAM_END_ERROR_WINDOW {
                    return RefetchStep::Retry;
                }
                match self.mark_ended(signals) {
                    Some(announce) => {
                        self.errors_since = None;
                        RefetchStep::Ended {
                            announce,
                            timed_out: true,
                        }
                    }
                    None => RefetchStep::Cancelled(
                        end_recheck_cancel(signals).expect("mark_ended refused"),
                    ),
                }
            }
            RefetchReading::Live {
                m3u8_url,
                stream_id,
            } => {
                if !same_broadcast(self.stream_id.as_deref(), stream_id.as_deref()) {
                    return RefetchStep::NewBroadcast;
                }
                self.ended = false;
                RefetchStep::Restart { m3u8_url }
            }
            RefetchReading::Offline => match self.mark_ended(signals) {
                Some(announce) => RefetchStep::Ended {
                    announce,
                    timed_out: false,
                },
                None => {
                    RefetchStep::Cancelled(end_recheck_cancel(signals).expect("mark_ended refused"))
                }
            },
        }
    }

    /// Marks the stream ended unless this node is not active or a stop/restart
    /// arrived this session. Returns whether 直播结束 still has to be sent.
    /// Used by the refetch miss, the error window and the post-loop paths
    /// (B站 stopped first, M1), so all of them announce once and open 换台.
    fn mark_ended(&mut self, signals: EndSignals) -> Option<bool> {
        if end_recheck_cancel(signals).is_some() {
            return None;
        }
        self.ended = true;
        let announce = !self.announced;
        self.announced = true;
        Some(announce)
    }

    /// Commands (and the page's 切换 button) open only on an ended stream, on
    /// an active node, with commands turned on in config.
    fn commands_gate(&self, commands_configured: bool, node_active: bool) -> bool {
        self.ended && commands_configured && node_active
    }
}

/// What to announce once the push loop has exited.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EndAction {
    SkipHandoff,
    SkipRestart,
    SkipEndCheckCancelled,
    SkipNewBroadcast,
    SkipManualStop,
    SkipWarning,
    /// Source ended: send the end danmaku. `bili_live` is false when the room
    /// is already offline; that send is best effort.
    AnnounceEnd {
        bili_live: bool,
    },
    /// Source still live and the room still live: transport interruption.
    AnnounceInterrupted,
    /// Source live, room stopped from elsewhere: no danmaku.
    BiliStoppedQuietly,
}

fn end_action(
    reason: FfmpegLoopExitReason,
    manual_stop: bool,
    warning_skip: bool,
    source_live: bool,
    bili_live: bool,
) -> EndAction {
    if restart_exit_should_skip_end_danmaku(reason) {
        return match reason {
            FfmpegLoopExitReason::ClusterHandoff => EndAction::SkipHandoff,
            FfmpegLoopExitReason::EndCheckCancelled { .. } => EndAction::SkipEndCheckCancelled,
            FfmpegLoopExitReason::NewBroadcast => EndAction::SkipNewBroadcast,
            _ => EndAction::SkipRestart,
        };
    }
    if manual_stop && matches!(reason, FfmpegLoopExitReason::BiliStopped) {
        return EndAction::SkipManualStop;
    }
    if warning_skip {
        return EndAction::SkipWarning;
    }
    match (source_live, bili_live) {
        (false, bili_live) => EndAction::AnnounceEnd { bili_live },
        (true, true) => EndAction::AnnounceInterrupted,
        (true, false) => EndAction::BiliStoppedQuietly,
    }
}

/// The session 下播 cause this action implies, or `None` while the room is
/// still live: the notice must follow the room, never the push. `AutoStop` is
/// not here — it is emitted where the room is actually stopped.
fn end_cause(action: EndAction, bili_live: bool) -> Option<BroadcastEndCause> {
    match action {
        EndAction::SkipManualStop => Some(BroadcastEndCause::ManualStop),
        // The danmaku handler records this when it stops the room; this path
        // covers a warning that took the room offline without it.
        EndAction::SkipWarning => (!bili_live).then_some(BroadcastEndCause::WarningCut),
        EndAction::AnnounceEnd { bili_live: false } => Some(BroadcastEndCause::SourceEnded),
        EndAction::BiliStoppedQuietly => Some(BroadcastEndCause::BiliStopped),
        _ => None,
    }
}

fn end_danmaku_text(channel_name: &str, commands_enabled: bool) -> String {
    if commands_enabled {
        format!("{} 直播结束，可使用弹幕指令进行换台", channel_name)
    } else {
        format!("{} 直播结束", channel_name)
    }
}

/// The room is stopped automatically once it has been live this long and the
/// source has ended.
const AUTO_STOP_AFTER_HOURS: i64 = 3;

fn should_auto_stop_after_end(live_hours: i64) -> bool {
    live_hours >= AUTO_STOP_AFTER_HOURS
}

fn log_selected_stream(identity: &FfmpegStreamIdentity, title: Option<&str>, priority: bool) {
    tracing::info!(
        "{identity} 源直播可用；标题={}；选台原因={}",
        safe_text(title.unwrap_or("无标题"), 80),
        if priority {
            "可用优先频道"
        } else {
            "普通候选按 Niconico > YouTube > Twitch 排序"
        }
    );
}

fn log_ffmpeg_loop_exit(
    context: &str,
    reason: FfmpegLoopExitReason,
    execution_block: Option<&str>,
) {
    let execution = execution_block
        .map(|reason| format!("；当前执行受阻：{}", safe_diagnostic(reason)))
        .unwrap_or_default();
    match reason {
        FfmpegLoopExitReason::SourceEnded => {
            tracing::info!("{context} 源直播已结束，退出推流循环{execution}");
        }
        FfmpegLoopExitReason::BiliStopped => {
            tracing::info!("{context} B站直播已停止，退出推流循环{execution}");
        }
        FfmpegLoopExitReason::IntentionalRestart { .. } => {
            tracing::info!("{context} 收到重启或切换信号，退出本次推流循环{execution}");
        }
        FfmpegLoopExitReason::ClusterHandoff => {
            tracing::info!("{context} 集群执行权限已撤销，退出本节点推流循环{execution}");
        }
        FfmpegLoopExitReason::NewBroadcast => {
            tracing::info!("{context} 源频道已开始新的直播，回到监听重新开播{execution}");
        }
        FfmpegLoopExitReason::EndCheckCancelled { .. } => {
            tracing::info!("{context} 确认源直播结束期间收到停止或重启，退出推流循环{execution}");
        }
    }
}

/// Display name for the 开播 notice's 来源.
/// Addendum E rev 5 item 23: an unknown code shows as itself, never as a
/// known platform name.
fn live_platform_name(code: &str) -> String {
    match code {
        "YT" => "YouTube".to_string(),
        "TW" => "Twitch".to_string(),
        "NC" => "Niconico".to_string(),
        other => other.to_string(),
    }
}

#[cfg(test)]
#[test]
fn an_unknown_platform_code_is_shown_raw() {
    assert_eq!(live_platform_name("YT"), "YouTube");
    assert_eq!(live_platform_name("NC"), "Niconico");
    assert_eq!(live_platform_name("BL"), "BL");
    assert_eq!(live_platform_name(""), "");
}

impl StreamPlatform {
    fn code(self) -> &'static str {
        match self {
            StreamPlatform::Youtube => "YT",
            StreamPlatform::Twitch => "TW",
            StreamPlatform::Niconico => "NC",
        }
    }
}

#[derive(Clone)]
struct StreamCandidate {
    platform: StreamPlatform,
    is_live: bool,
    topic: Option<String>,
    title: Option<String>,
    m3u8_url: Option<String>,
    piped_ingest: Option<PipedIngest>,
    stream_id: Option<String>,
    channel_name: String,
    channel_id: String,
    area_v2: u64,
    is_priority: bool,
}

impl StreamCandidate {
    fn is_playable(&self) -> bool {
        self.is_live
            && (self.m3u8_url.as_ref().is_some_and(|url| !url.is_empty())
                || self.piped_ingest.is_some())
    }

    fn source_identity(&self) -> BroadcastSource {
        BroadcastSource::new(
            self.platform.code(),
            &self.channel_name,
            &self.channel_id,
            self.stream_id.as_deref(),
        )
    }

    fn from_status(
        platform: StreamPlatform,
        name: &str,
        id: &str,
        area: u64,
        status: SourceStatus,
    ) -> Self {
        let SourceStatus {
            is_live,
            topic,
            title,
            m3u8_url,
            video_id: stream_id,
            ..
        } = status;
        Self {
            platform,
            is_live,
            topic,
            title,
            m3u8_url,
            stream_id,
            piped_ingest: None,
            channel_name: name.to_owned(),
            channel_id: canonical_channel_id(platform.code(), id),
            area_v2: area,
            is_priority: false,
        }
    }

    fn ffmpeg_source(&self) -> Option<FfmpegSource> {
        if let Some(ingest) = self.piped_ingest.clone() {
            Some(FfmpegSource::Piped(ingest))
        } else {
            self.m3u8_url.clone().map(FfmpegSource::HlsUrl)
        }
    }

    fn stream_title(&self) -> Option<String> {
        bilistream::plugins::banned_keywords::streaming_haystack(
            self.topic.as_deref(),
            self.title.as_deref(),
        )
    }

    fn cfg_title(&self) -> String {
        format!("【转播】{}", self.channel_name)
    }
}

/// The YouTube candidate was live, then a warning or banned-keyword skip
/// dropped it. Later ticks reuse the index instead of fetching the m3u8.
fn mark_youtube_skipped_live(stream: &StreamCandidate, was_live: bool) {
    if !was_live || stream.is_live {
        return;
    }
    let Some(video_id) = stream.stream_id.as_deref().filter(|id| !id.is_empty()) else {
        return;
    };
    bilistream::plugins::youtube::mark_skipped_live(&stream.channel_id, video_id);
}

/// A skipped live stream is no longer skipped: drop the mark so the next pass
/// runs yt-dlp for the m3u8 immediately, without waiting 检测间隔.
fn refetch_skipped_youtube(stream: &StreamCandidate) -> bool {
    stream.is_live
        && stream.m3u8_url.is_none()
        && bilistream::plugins::youtube::clear_skipped_live(&stream.channel_id)
}

/// The first playable candidate: a live priority channel (YouTube, then
/// Twitch) first, then Niconico, YouTube, Twitch.
fn select_stream(
    yt: &StreamCandidate,
    tw: &StreamCandidate,
    nico: &StreamCandidate,
) -> Option<StreamCandidate> {
    [yt, tw]
        .into_iter()
        .filter(|candidate| candidate.is_priority)
        .chain([nico, yt, tw])
        .find(|candidate| candidate.is_playable())
        .cloned()
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ChannelProbe {
    platform: StreamPlatform,
    channel_name: String,
    channel_id: String,
}

#[derive(Default)]
struct SourceProbePlan {
    channels: Vec<ChannelProbe>,
    youtube: Option<usize>,
    twitch: Option<usize>,
    priority_youtube: Option<usize>,
    priority_twitch: Option<usize>,
}

impl SourceProbePlan {
    fn add(
        &mut self,
        enabled: bool,
        platform: StreamPlatform,
        name: &str,
        id: &str,
    ) -> Option<usize> {
        let id = canonical_channel_id(platform.code(), id);
        if !enabled || id.is_empty() {
            return None;
        }
        if let Some(index) = self
            .channels
            .iter()
            .position(|probe| probe.platform == platform && probe.channel_id == id)
        {
            return Some(index);
        }
        self.channels.push(ChannelProbe {
            platform,
            channel_name: name.to_owned(),
            channel_id: id,
        });
        Some(self.channels.len() - 1)
    }

    fn for_config(cfg: &Config) -> Self {
        let mut plan = Self::default();
        plan.youtube = plan.add(
            cfg.youtube.enable_monitor,
            StreamPlatform::Youtube,
            &cfg.youtube.channel_name,
            &cfg.youtube.channel_id,
        );
        plan.priority_youtube = plan.add(
            cfg.priority_channel.enabled,
            StreamPlatform::Youtube,
            &cfg.priority_channel.channel_name,
            &cfg.priority_channel.youtube_channel_id,
        );
        plan.twitch = plan.add(
            cfg.twitch.enable_monitor,
            StreamPlatform::Twitch,
            &cfg.twitch.channel_name,
            &cfg.twitch.channel_id,
        );
        plan.priority_twitch = plan.add(
            cfg.priority_channel.enabled,
            StreamPlatform::Twitch,
            &cfg.priority_channel.channel_name,
            &cfg.priority_channel.twitch_channel_id,
        );
        plan
    }
}

enum ProbedClient {
    Youtube(YoutubeClient),
    Twitch(TwitchClient),
}

struct ProbedSource {
    channel: ChannelProbe,
    client: Option<ProbedClient>,
    status: Result<SourceStatus, String>,
}

/// The plan is the sole owner of per-pass provider checks and prefetch takes.
/// Keep errors intact until warning-release evidence has been evaluated.
async fn probe_sources(cfg: &Config, plan: &SourceProbePlan) -> Option<Vec<ProbedSource>> {
    let mut probes = Vec::with_capacity(plan.channels.len());
    for channel in &plan.channels {
        let client = match channel.platform {
            StreamPlatform::Youtube => Ok(ProbedClient::Youtube(YoutubeClient::new(
                &channel.channel_name,
                &channel.channel_id,
                cfg.youtube.proxy.clone(),
            ))),
            StreamPlatform::Twitch => TwitchClient::new(
                &channel.channel_id,
                cfg.twitch.proxy_region.clone(),
                cfg.twitch.proxy.clone(),
            )
            .map(ProbedClient::Twitch)
            .map_err(|error| error.to_string()),
            StreamPlatform::Niconico => unreachable!("Niconico uses its listing/ingest path"),
        };
        let (client, status) = match client {
            Ok(client) => {
                let platform = match channel.platform {
                    StreamPlatform::Youtube => PriorityChannelPlatform::Youtube,
                    _ => PriorityChannelPlatform::Twitch,
                };
                let warning = warning_tracker().snapshot();
                let source = BroadcastSource::new(
                    channel.platform.code(),
                    &channel.channel_name,
                    &channel.channel_id,
                    None,
                );
                let held = warning.holds(&source);
                let status = if held {
                    match channel.platform {
                        StreamPlatform::Youtube => {
                            bilistream::plugins::youtube::get_youtube_warning_status(
                                &channel.channel_id,
                                warning.episode_id_for(&source),
                            )
                            .await
                            .map(|status| SourceStatus {
                                is_live: status.is_live,
                                topic: status.topic,
                                title: status.title,
                                m3u8_url: None,
                                start_time: status.scheduled_start,
                                video_id: status.video_id,
                            })
                            .map_err(|error| error.to_string())
                        }
                        _ => bilistream::plugins::twitch::get_twitch_warning_status(
                            &channel.channel_id,
                        )
                        .await
                        .map(|(is_live, topic, title, id)| SourceStatus {
                            is_live,
                            topic,
                            title,
                            m3u8_url: None,
                            start_time: None,
                            video_id: id,
                        })
                        .map_err(|error| error.to_string()),
                    }
                } else if let Some(status) =
                    take_prefetched_source_status(platform, &channel.channel_id)
                {
                    Ok(status)
                } else {
                    match &client {
                        ProbedClient::Youtube(client) => {
                            client.get_status().await.map_err(|error| error.to_string())
                        }
                        ProbedClient::Twitch(client) => {
                            client.get_status().await.map_err(|error| error.to_string())
                        }
                    }
                };
                (Some(client), status)
            }
            Err(error) => (None, Err(error)),
        };
        if let Err(error) = &status {
            tracing::warn!(
                "{} 频道 {} 状态检查失败；{}",
                channel.platform.code(),
                safe_text(&channel.channel_name, 64),
                safe_diagnostic(error)
            );
        }
        if config_changed_since_pass(cfg) {
            return None;
        }
        probes.push(ProbedSource {
            channel: channel.clone(),
            client,
            status,
        });
    }
    Some(probes)
}

fn probed_status(probes: &[ProbedSource], index: Option<usize>) -> SourceStatus {
    index
        .and_then(|index| probes.get(index))
        .and_then(|probe| probe.status.as_ref().ok())
        .cloned()
        .unwrap_or(SourceStatus::OFFLINE)
}

fn source_observation(
    platform: StreamPlatform,
    name: &str,
    id: &str,
    status: &SourceStatus,
) -> SourceObservation {
    SourceObservation::new(
        BroadcastSource::new(platform.code(), name, id, status.video_id.as_deref()),
        status.is_live,
        status.start_time.is_some()
            || (platform == StreamPlatform::Youtube
                && status.video_id.as_ref().is_some_and(|id| !id.is_empty())),
    )
}

/// Priority is an overlay on this choice only; an ineligible/unplayable
/// priority result must leave its ordinary platform's fallback in place.
fn overlay_priority(
    mut ordinary: StreamCandidate,
    mut priority: StreamCandidate,
) -> StreamCandidate {
    if priority.is_playable() {
        priority.is_priority = true;
        priority
    } else {
        ordinary.is_priority = false;
        ordinary
    }
}

fn priority_area(platform: StreamPlatform, cfg: &Config) -> u64 {
    let (target, priority, area) = match platform {
        StreamPlatform::Youtube => (
            &cfg.youtube.channel_id,
            &cfg.priority_channel.youtube_channel_id,
            cfg.youtube.area_v2,
        ),
        StreamPlatform::Twitch => (
            &cfg.twitch.channel_id,
            &cfg.priority_channel.twitch_channel_id,
            cfg.twitch.area_v2,
        ),
        StreamPlatform::Niconico => return cfg.niconico.area_v2,
    };
    if !target.is_empty()
        && canonical_channel_id(platform.code(), target)
            == canonical_channel_id(platform.code(), priority)
    {
        area
    } else {
        cfg.priority_channel.default_area
    }
}

/// Use the URL the priority monitor already confirmed, if it matches this
/// channel. Avoids a second yt-dlp/streamlink fetch right after a switch.
fn take_prefetched_source_status(
    platform: PriorityChannelPlatform,
    channel_id: &str,
) -> Option<SourceStatus> {
    take_prefetched_playable_stream(platform, channel_id).map(|stream| {
        tracing::info!(
            "复用优先频道已确认的 {} 流地址，跳过重新拉取",
            platform.label()
        );
        SourceStatus {
            is_live: true,
            topic: stream.topic,
            title: stream.title,
            m3u8_url: Some(stream.m3u8_url),
            start_time: None,
            video_id: stream.stream_id,
        }
    })
}

/// The live-status client backing the currently selected stream candidate.
enum SourceClient<'a> {
    Youtube(&'a YoutubeClient),
    Twitch(&'a TwitchClient),
    Niconico(&'a NiconicoClient),
}

fn selected_source_client<'a>(
    selected: &StreamCandidate,
    probes: &'a [ProbedSource],
    nico: &'a Option<NiconicoClient>,
) -> Option<SourceClient<'a>> {
    if selected.platform == StreamPlatform::Niconico {
        return nico.as_ref().map(SourceClient::Niconico);
    }
    let probe = probes.iter().find(|probe| {
        probe.channel.platform == selected.platform
            && probe.channel.channel_id == selected.channel_id
    })?;
    match probe.client.as_ref()? {
        ProbedClient::Youtube(client) => Some(SourceClient::Youtube(client)),
        ProbedClient::Twitch(client) => Some(SourceClient::Twitch(client)),
    }
}

impl SourceClient<'_> {
    /// The strict, always-fresh refetch: "not live" only on a positive source
    /// statement; every other failure is an `Err`.
    async fn try_status(&self) -> Result<SourceStatus, Box<dyn std::error::Error>> {
        match self {
            SourceClient::Youtube(client) => client.get_status_refetch().await,
            SourceClient::Twitch(client) => client.get_status_refetch().await,
            SourceClient::Niconico(client) => client.get_status_refetch().await,
        }
    }
}

/// The m3u8 refetch after ffmpeg exits. No client means no source: offline.
async fn refetch_source(client: &Option<SourceClient<'_>>) -> RefetchReading {
    match client {
        Some(client) => RefetchReading::from_result(client.try_status().await),
        None => RefetchReading::Offline,
    }
}

/// Errors are logged; callers decide the fallback. Cluster health is sampled
/// independently so this request's retry budget cannot delay fault detection.
async fn fetch_bili_live_status_logged(
    room: i32,
) -> Result<bilibili::BiliLiveStatus, Box<dyn std::error::Error>> {
    match get_bili_live_status(room).await {
        Ok(status) => Ok(status),
        Err(e) => {
            tracing::error!("获取B站直播状态失败: {}", e);
            Err(e)
        }
    }
}

fn report_danmaku_selection(
    cfg: &Config,
    probes: &[ProbedSource],
    yt: &StreamCandidate,
    tw: &StreamCandidate,
    nico: &StreamCandidate,
    keywords: &[String],
    warning: &WarningSnapshot,
) {
    use bilistream::plugins::danmaku::{pending_target_request, report_target_decision};
    let Some(request) = pending_target_request(cfg) else {
        return;
    };
    let selected = select_stream(yt, tw, nico);
    let enabled = match request.source.platform.as_str() {
        "YT" => cfg.youtube.enable_monitor,
        "TW" => cfg.twitch.enable_monitor,
        _ => false,
    };
    let probe = probes.iter().find(|probe| {
        probe.channel.platform.code() == request.source.platform
            && probe.channel.channel_id == request.source.channel_id
    });
    let reason = if warning.holds(&request.source) {
        "请求频道仍因警告/切断临时跳过"
    } else if !enabled {
        "请求平台的普通监控未启用"
    } else if let Some(probe) = probe {
        match &probe.status {
            Err(_) => "请求频道状态检查失败",
            Ok(status) if !status.is_live => "请求频道未直播",
            Ok(status)
                if bilistream::plugins::banned_keywords::streaming_banned_hit(
                    status.topic.as_deref(),
                    status.title.as_deref(),
                    keywords,
                )
                .is_some() =>
            {
                "请求频道标题/游戏包含禁用关键词"
            }
            Ok(status) if status.m3u8_url.as_ref().is_none_or(|url| url.is_empty()) => {
                "请求频道暂无可播放的流地址"
            }
            Ok(_)
                if selected.as_ref().is_some_and(|stream| {
                    stream.platform.code() == request.source.platform
                        && stream.channel_id == request.source.channel_id
                }) =>
            {
                "请求频道是本轮最高可用候选"
            }
            Ok(_) if selected.is_some() => "自动排序选择了更高优先级的可用频道",
            Ok(_) => "请求频道未通过防撞车检查",
        }
    } else {
        "请求频道尚未进入本轮检查"
    };
    report_target_decision(
        &request,
        selected
            .as_ref()
            .map(StreamCandidate::source_identity)
            .as_ref(),
        reason,
    );
}

/// Marks a candidate offline when its channel was previously stopped due to a
/// warning/cut-off, announcing the skip once via danmaku.
async fn skip_stream_if_previously_warned(
    stream: &mut StreamCandidate,
    cfg: &Config,
    warnings: &bilistream::plugins::danmaku::warning::WarningTracker,
) {
    let source = stream.source_identity();
    if !exclude_warned_stream(stream, &warnings.snapshot()) {
        return;
    }

    if warnings.take_skip_log(&source) {
        tracing::warn!(
            "跳过频道 {} - 之前因警告/切断停止",
            safe_text(&stream.channel_name, 64)
        );
        if cfg.bililive.enable_danmaku_command && !is_danmaku_commands_enabled() {
            enable_danmaku_commands(true);
            if let Err(e) = send_danmaku(
                cfg,
                &format!(
                    "{} 因警告/切断被跳过，可使用弹幕指令换台",
                    stream.channel_name
                ),
            )
            .await
            {
                tracing::error!("Failed to send danmaku: {}", e);
            }
        }
    }

    stream.is_live = false;
}

fn exclude_warned_stream(stream: &mut StreamCandidate, warning: &WarningSnapshot) -> bool {
    let skipped = stream.is_live && warning.holds(&stream.source_identity());
    if skipped {
        stream.is_live = false;
    }
    skipped
}

fn exclude_banned_stream(stream: &mut StreamCandidate, keywords: &[String]) -> Option<String> {
    if !stream.is_live {
        return None;
    }
    let keyword = bilistream::plugins::banned_keywords::streaming_banned_hit(
        stream.topic.as_deref(),
        stream.title.as_deref(),
        keywords,
    )?;
    stream.is_live = false;
    Some(keyword)
}

/// Marks a candidate offline when its title/topic contains a banned streaming
/// keyword, warning once per (platform, keyword, title) combination.
async fn skip_stream_if_banned_keyword(
    stream: &mut StreamCandidate,
    keywords: &[String],
    cfg: &Config,
) {
    let Some(keyword) = exclude_banned_stream(stream, keywords) else {
        return;
    };
    let default_title = "无标题".to_string();
    let stream_title = stream.stream_title();
    let title_str = stream_title.as_ref().unwrap_or(&default_title);

    let platform = stream.platform.code();
    let should_warn = {
        let mut last_warning =
            recover_mutex_lock(&LAST_BANNED_KEYWORD_WARNING, "last banned keyword warning");
        remember_banned_notice(&mut last_warning, stream, &keyword, title_str)
    };

    if should_warn {
        tracing::error!("{}直播标题/分区包含不支持的关键词: {}", platform, keyword);
        if let Err(e) =
            send_danmaku(cfg, &format!("错误：{}标题/分区含:{}", platform, keyword)).await
        {
            tracing::error!("Failed to send danmaku: {}", e);
        }
        if cfg.bililive.enable_danmaku_command {
            if !is_danmaku_commands_enabled() {
                enable_danmaku_commands(true);
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
            if let Err(e) = send_danmaku(cfg, "可使用弹幕指令进行换台").await {
                tracing::error!("Failed to send danmaku: {}", e);
            }
        }
    }

    stream.is_live = false;
}

/// Updates the Bilibili live cover from the source stream's thumbnail in the
/// background.
fn spawn_cover_update(cfg: &Config, platform: &str, channel_id: &str, stream_id: Option<String>) {
    let cfg = cfg.clone();
    let platform = platform.to_string();
    let channel_id = channel_id.to_string();
    tokio::spawn(async move {
        let proxy = match platform.as_str() {
            "YT" => cfg.youtube.proxy.clone(),
            "TW" => cfg.twitch.proxy.clone(),
            "NC" => cfg.niconico.proxy.clone(),
            _ => None,
        };
        match get_thumbnail(&platform, &channel_id, stream_id.as_deref(), proxy).await {
            Ok(cover_path) if !cover_path.is_empty() => {
                if let Err(e) = bilibili::bili_change_cover(&cfg, &cover_path).await {
                    tracing::error!("B站直播间封面替换失败: {}", e);
                } else {
                    tracing::info!("B站直播间封面替换成功");
                }
            }
            Ok(_) => {
                tracing::warn!("跳过封面更新：缩略图下载失败");
            }
            Err(e) => {
                tracing::error!("获取缩略图失败: {}", e);
            }
        }
    });
}

fn ffmpeg_exit_should_count_as_failure(
    ffmpeg_exit_failed: bool,
    source_is_live: bool,
    bili_is_live: bool,
    manual_restart: bool,
    manual_stop: bool,
    config_updated: bool,
    warning_skip: bool,
) -> bool {
    ffmpeg_exit_failed
        && source_is_live
        && bili_is_live
        && !manual_restart
        && !manual_stop
        && !config_updated
        && !warning_skip
}

/// Keep the danmaku client in step with `enable_danmaku_command`: start it when
/// the setting is on and it is not running, stop it (with the operator-visible
/// line) when the setting is off. Extracted from `run_bilistream`, which is long
/// enough that a reader could not tell this was a self-contained step.
async fn sync_danmaku_client(cfg: &bilistream::config::Config) {
    if cfg.bililive.enable_danmaku_command {
        if !is_danmaku_running() {
            run_danmaku();
        }
    } else if is_danmaku_running() {
        tracing::info!("弹幕命令已禁用，停止弹幕客户端");
        stop_danmaku().await;
    }
}

/// Load this pass's config and consume the go-live wakes the pass is about to
/// check anyway. The reload signal is cleared *before* loading so an update that
/// arrives during `load_config()` stays set for the checks further down.
async fn load_pass_config() -> Result<bilistream::config::Config, Box<dyn std::error::Error>> {
    clear_config_updated();
    let cfg = match load_config().await {
        Ok(cfg) => cfg,
        Err(e) => {
            tracing::warn!("加载配置失败，短暂重试: {}", e);
            tokio::time::sleep(Duration::from_millis(100)).await;
            load_config().await?
        }
    };
    for channel in idle_wake_channels(&cfg) {
        bilistream::plugins::youtube::take_monitor_wake(channel);
    }
    bilistream::plugins::twitch_live::take_monitor_wake(&cfg.twitch.channel_id);
    bilistream::plugins::niconico_live::take_monitor_wake(&cfg.niconico.channel_id);
    Ok(cfg)
}

/// While the cluster refuses to let this node monitor: say why once, stop the
/// danmaku client, then wait for the condition or for a config update. Returns
/// true when the caller should start the pass over.
async fn wait_out_cluster_block(
    cfg: &bilistream::config::Config,
    cluster_wait: &mut Option<(std::time::Instant, String)>,
) -> bool {
    let Some(reason) = cluster::local_monitoring_block_reason(cfg) else {
        return false;
    };
    let (_, last_reason) =
        cluster_wait.get_or_insert_with(|| (std::time::Instant::now(), String::new()));
    if *last_reason != reason {
        tracing::info!("等待集群允许监控: {}", reason);
        last_reason.clone_from(&reason);
    }
    if is_danmaku_running() {
        tracing::info!("{}，停止弹幕客户端", reason);
        stop_danmaku().await;
    }
    let retry_secs = cfg
        .cluster
        .heartbeat_interval_secs
        .max(1)
        .min(cfg.interval.max(1));
    tokio::select! {
        _ = cluster::wait_for_local_monitoring(cfg) => {}
        _ = wait_config_update_or_timeout(Duration::from_secs(retry_secs)) => {}
    }
    true
}

/// A pass with nothing configured cannot probe anything: tell the operator how to
/// configure, then wait for a config update so the WebUI has a chance to be used.
/// Returns true when the caller should start the pass over.
async fn require_a_configured_source(cfg: &bilistream::config::Config) -> bool {
    let nothing_configured = cfg.youtube.channel_id.is_empty()
        && cfg.twitch.channel_id.is_empty()
        && !niconico_configured(&cfg.niconico)
        && (!cfg.priority_channel.enabled
            || (cfg.priority_channel.youtube_channel_id.is_empty()
                && cfg.priority_channel.twitch_channel_id.is_empty()));
    if !nothing_configured {
        return false;
    }
    tracing::error!("YouTube、Twitch 和 Niconico 配置均为空");
    tracing::error!("请在 WebUI 中配置或手动编辑 config.json 文件");
    tracing::info!("提示: 打开 Web UI，按设置向导完成配置");
    wait_config_update_or_timeout(Duration::from_secs(cfg.interval)).await;
    true
}

/// State that outlives one pass of [`run_bilistream`]'s outer loop.
///
/// These three used to be locals at the top of the loop body. Lifting them into a
/// struct is what lets the loop's phases move out into functions without every
/// one of them taking three `&mut` parameters.
#[derive(Default)]
struct PassState {
    /// Set while the cluster refuses to let this node monitor. The reason is kept
    /// so the operator sees the wait start once, not on every pass.
    cluster_wait: Option<(std::time::Instant, String)>,
    /// Survives passes so the same broadcast coming back after a miss does not
    /// announce 直播结束 twice.
    stream_end: StreamEndState,
    /// The room's liveness as the previous pass observed it. The session 下播
    /// notice is keyed to this edge: only an observed online-to-offline change
    /// closes the broadcast, so a source gap never ends the room's session.
    bili_room_was_live: bool,
}

/// Fetch the room's live status and emit the session 下播 edge when the room has
/// just gone offline.
///
/// `None` means the fetch failed; the caller waits for the next pass. The edge is
/// emitted before the pass selects a source, so a room that comes back opens a new
/// session. `LiveLedger::end` only closes an open session and the first cause
/// wins, so a stop this process already announced (manual/warning/auto) keeps its
/// own cause.
async fn observe_bili_room(
    state: &mut PassState,
    cfg: &bilistream::config::Config,
) -> Option<bilibili::BiliLiveStatus> {
    let bili_live = match fetch_bili_live_status_logged(cfg.bililive.room).await {
        Ok(status) => status,
        Err(_) => {
            tracing::warn!("将在下次循环重试");
            return None;
        }
    };
    if state.bili_room_was_live && !bili_live.is_live {
        bilistream::notify::live_ended(BroadcastEndCause::BiliStopped);
    }
    state.bili_room_was_live = bili_live.is_live;
    Some(bili_live)
}

/// True when the config changed under this pass: a reload was requested, or the
/// in-memory snapshot is no longer current. Callers decide whether to clear the
/// signal and what to log — the three call sites that only `continue` rely on
/// `load_pass_config` clearing it at the top of the next pass.
fn config_changed_since_pass(cfg: &bilistream::config::Config) -> bool {
    is_config_updated() || !bilistream::config::config_is_current(cfg)
}

/// This pass's probe answers, still unfiltered.
///
/// The WebUI status cache and the `StreamCandidate` build both read the same
/// six-field answers. Exploding them into twenty locals forced the cache
/// literal to reconstruct the structs it had just taken apart.
struct PassSources {
    youtube: SourceStatus,
    twitch: SourceStatus,
    niconico: SourceStatus,
    nico_channel_id: String,
    nico_channel_name: String,
    priority_youtube: SourceStatus,
    priority_twitch: SourceStatus,
}

/// The five slots the warning/keyword filter walks, before the priority overlay.
struct RawCandidates {
    youtube: StreamCandidate,
    twitch: StreamCandidate,
    niconico: StreamCandidate,
    priority_youtube: StreamCandidate,
    priority_twitch: StreamCandidate,
}

/// Publish this pass's probe answers to the admin status cache.
///
/// The field mapping and the "configured?" gates are the original literal,
/// moved out of `run_bilistream` so the loop can keep reading top-down.
fn publish_webui_status(
    cfg: &bilistream::config::Config,
    bili_status: bilistream::BiliStatus,
    sources: &PassSources,
) {
    bilistream::update_status_cache(bilistream::StatusData {
        bilibili: bili_status,
        youtube: if cfg.youtube.enable_monitor && !cfg.youtube.channel_id.is_empty() {
            let yt_area_name = get_area_name(cfg.youtube.area_v2)
                .unwrap_or_else(|| format!("未知分区 (ID: {})", cfg.youtube.area_v2));
            Some(bilistream::YtStatus {
                is_live: sources.youtube.is_live,
                enable_monitor: cfg.youtube.enable_monitor,
                title: sources.youtube.title.clone(),
                topic: sources.youtube.topic.clone(),
                channel_name: cfg.youtube.channel_name.clone(),
                channel_id: cfg.youtube.channel_id.clone(),
                quality: cfg.youtube.quality.clone(),
                area_id: cfg.youtube.area_v2,
                area_name: yt_area_name,
                crop_enabled: cfg.youtube.crop.is_some(),
                ffmpeg_cache_enabled: cfg.youtube.ffmpeg_cache.enabled,
                ffmpeg_cache_latency_secs: cfg.youtube.ffmpeg_cache.latency_secs,
            })
        } else {
            None
        },
        twitch: if cfg.twitch.enable_monitor && !cfg.twitch.channel_id.is_empty() {
            let tw_area_name = get_area_name(cfg.twitch.area_v2)
                .unwrap_or_else(|| format!("未知分区 (ID: {})", cfg.twitch.area_v2));
            Some(bilistream::TwStatus {
                is_live: sources.twitch.is_live,
                enable_monitor: cfg.twitch.enable_monitor,
                title: sources.twitch.title.clone(),
                game: sources.twitch.topic.clone(),
                channel_name: cfg.twitch.channel_name.clone(),
                channel_id: cfg.twitch.channel_id.clone(),
                quality: cfg.twitch.quality.clone(),
                area_id: cfg.twitch.area_v2,
                area_name: tw_area_name,
                crop_enabled: cfg.twitch.crop.is_some(),
                ffmpeg_cache_enabled: cfg.twitch.ffmpeg_cache.enabled,
                ffmpeg_cache_latency_secs: cfg.twitch.ffmpeg_cache.latency_secs,
            })
        } else {
            None
        },
        niconico: if cfg.niconico.enable_monitor && niconico_configured(&cfg.niconico) {
            let nico_area_name = get_area_name(cfg.niconico.area_v2)
                .unwrap_or_else(|| format!("未知分区 (ID: {})", cfg.niconico.area_v2));
            Some(bilistream::NicoStatus {
                is_live: sources.niconico.is_live,
                enable_monitor: cfg.niconico.enable_monitor,
                title: sources.niconico.title.clone(),
                channel_name: sources.nico_channel_name.clone(),
                channel_id: sources.nico_channel_id.clone(),
                live_id: sources.niconico.video_id.clone(),
                scheduled_start: sources
                    .niconico
                    .start_time
                    .as_ref()
                    .map(|start| start.to_rfc3339()),
                quality: cfg.niconico.quality.clone(),
                area_id: cfg.niconico.area_v2,
                area_name: nico_area_name,
                crop_enabled: cfg.niconico.crop.is_some(),
                ffmpeg_cache_enabled: cfg.niconico.ffmpeg_cache.enabled,
                ffmpeg_cache_latency_secs: cfg.niconico.ffmpeg_cache.latency_secs,
            })
        } else {
            None
        },
        priority_channel: if cfg.priority_channel.enabled {
            let (is_live, platform, title) = if sources.priority_youtube.is_live {
                (
                    true,
                    Some("youtube".to_string()),
                    sources.priority_youtube.title.clone(),
                )
            } else if sources.priority_twitch.is_live {
                (
                    true,
                    Some("twitch".to_string()),
                    sources.priority_twitch.title.clone(),
                )
            } else {
                (false, None, None)
            };
            Some(bilistream::PriorityChannelStatus {
                enabled: cfg.priority_channel.enabled,
                auto_restart: cfg.priority_channel.auto_restart,
                warning_held: bilistream::plugins::danmaku::priority_warning_held(cfg),
                channel_name: cfg.priority_channel.channel_name.clone(),
                is_live,
                platform,
                title,
                default_area: cfg.priority_channel.default_area,
                default_area_name: crate::plugins::get_area_name(cfg.priority_channel.default_area),
            })
        } else {
            None
        },
    });
}

/// Turn this pass's probe answers into the five slots the filter loop walks.
///
/// Niconico is not `from_status`: it has no m3u8, and a live answer with an
/// id becomes a streamlink pipe. `start_time` is ignored here (the idle
/// preview already copied it off `PassSources.youtube`).
fn stream_candidates(cfg: &bilistream::config::Config, sources: PassSources) -> RawCandidates {
    RawCandidates {
        youtube: StreamCandidate::from_status(
            StreamPlatform::Youtube,
            &cfg.youtube.channel_name,
            &cfg.youtube.channel_id,
            cfg.youtube.area_v2,
            sources.youtube,
        ),
        twitch: StreamCandidate::from_status(
            StreamPlatform::Twitch,
            &cfg.twitch.channel_name,
            &cfg.twitch.channel_id,
            cfg.twitch.area_v2,
            sources.twitch,
        ),
        niconico: StreamCandidate {
            platform: StreamPlatform::Niconico,
            is_live: sources.niconico.is_live,
            topic: None,
            title: sources.niconico.title,
            m3u8_url: None,
            piped_ingest: if sources.niconico.is_live {
                sources
                    .niconico
                    .video_id
                    .as_deref()
                    .and_then(|live_id| streamlink_ingest(&cfg.niconico, live_id).ok())
            } else {
                None
            },
            stream_id: sources.niconico.video_id,
            channel_name: sources.nico_channel_name,
            channel_id: if sources.nico_channel_id.is_empty() {
                cfg.niconico.live_id.clone()
            } else {
                sources.nico_channel_id
            },
            area_v2: cfg.niconico.area_v2,
            is_priority: false,
        },
        priority_youtube: StreamCandidate::from_status(
            StreamPlatform::Youtube,
            &cfg.priority_channel.channel_name,
            &cfg.priority_channel.youtube_channel_id,
            priority_area(StreamPlatform::Youtube, cfg),
            sources.priority_youtube,
        ),
        priority_twitch: StreamCandidate::from_status(
            StreamPlatform::Twitch,
            &cfg.priority_channel.channel_name,
            &cfg.priority_channel.twitch_channel_id,
            priority_area(StreamPlatform::Twitch, cfg),
            sources.priority_twitch,
        ),
    }
}

/// What [`align_bili_room`] needs of the selected source and the room this pass
/// already observed. Grouped so the helper does not take twelve parameters.
struct BiliAlign<'a> {
    is_live: bool,
    title: &'a str,
    area_id: u64,
    area_v2: u64,
    cfg_title: &'a str,
    platform: &'a str,
    channel_id: &'a str,
    channel_name: &'a str,
    video_id_changed: bool,
    current_video_id: Option<&'a str>,
}

/// Match the B站 room to the source this pass selected: startLive if the room
/// is down, otherwise update title/area/cover.
///
/// `RestartPass` is the startLive failure paths that used to `continue 'outer`.
/// The config-changed check after this block stays at the call site.
enum AlignBiliRoom {
    RestartPass,
    Ready { history_area_v2: Option<u64> },
}

async fn align_bili_room(
    cfg: &mut bilistream::config::Config,
    stream_log: &FfmpegStreamIdentity,
    align: BiliAlign<'_>,
) -> AlignBiliRoom {
    let mut history_area_v2 = align.is_live.then_some(align.area_id);
    if !align.is_live {
        tracing::info!("B站未直播");
        let area_name = area_label(align.area_v2);

        match bili_start_live(cfg, align.area_v2).await {
            Ok(_) => {
                history_area_v2 = Some(align.area_v2);
                if align.title != align.cfg_title {
                    if let Err(e) = bili_change_live_title(cfg, align.cfg_title).await {
                        tracing::error!("B站直播标题变更失败: {}", e);
                    }
                }
                // 开播 notice (Addendum A) is recorded once the source
                // for this pass is selected and the room is live (see
                // `notify::live_source` below), so it carries the same
                // source the push uses.
                tracing::info!(
                    "{stream_log} B站房间已开播；分区={}（ID: {}），等待推流进程",
                    safe_text(&area_name, 32),
                    align.area_v2
                );
                recover_mutex_lock(&LAST_BANNED_KEYWORD_WARNING, "last banned keyword warning")
                    .clear();
            }
            Err(e) => {
                let error = e.to_string();
                let message = error
                    .strip_prefix(BILI_START_TEMP_BAN_PREFIX)
                    .unwrap_or(&error);
                tracing::error!("{stream_log} B站开播失败；{}", safe_diagnostic(message));
                // 60031 does not include the end time. A room_init lock with
                // lock_till waits; only an untimed ban shuts the monitors off.
                if error.starts_with(BILI_START_TEMP_BAN_PREFIX)
                    && wait_out_bili_room_lock(cfg.bililive.room).await
                {
                    return AlignBiliRoom::RestartPass;
                }
                if error.starts_with(BILI_START_TEMP_BAN_PREFIX)
                    && disable_monitors_after_bili_start_temp_ban(cfg)
                {
                    tracing::warn!("检测到B站异常开播限制，已关闭 YouTube、Twitch 和优先频道监控");
                    if let Err(save_err) = save_config(cfg).await {
                        tracing::error!("保存配置失败: {}", save_err);
                    }
                }
                tracing::warn!("{stream_log} 本次开播未完成，下轮监控重试");
                wait_config_update_or_timeout(Duration::from_secs(cfg.interval)).await;
                return AlignBiliRoom::RestartPass;
            }
        }

        if cfg.auto_cover
            && (align.title != align.cfg_title
                || align.area_id != align.area_v2
                || align.video_id_changed)
        {
            spawn_cover_update(
                cfg,
                align.platform,
                align.channel_id,
                align.current_video_id.map(str::to_owned),
            );
        }
    } else {
        if align.title != align.cfg_title {
            if let Err(e) = bili_change_live_title(cfg, align.cfg_title).await {
                tracing::error!("B站直播标题变更失败: {}", e);
            } else {
                tracing::info!(
                    "{stream_log} B站直播标题已更新；{} → {}",
                    safe_text(align.title, 32),
                    safe_text(align.cfg_title, 32)
                );
                // title is 【转播】频道名
                let bili_channel_name = align.title.split("【转播】").last().unwrap();
                if bili_channel_name != align.channel_name {
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    if let Err(e) = send_danmaku(
                        cfg,
                        &format!("换台：{} → {}", bili_channel_name, align.channel_name),
                    )
                    .await
                    {
                        tracing::error!("发送弹幕失败: {}", e);
                    }
                }
            }
        }
        if align.area_id != align.area_v2 {
            if let Err(e) = update_area(align.area_id, align.area_v2).await {
                tracing::error!("B站分区更新失败: {}", e);
            } else {
                history_area_v2 = Some(align.area_v2);
                tokio::time::sleep(Duration::from_secs(2)).await;
                if let Err(e) = bili_change_live_title(cfg, align.cfg_title).await {
                    tracing::error!("B站直播标题变更失败: {}", e);
                }
            }
        }
        if cfg.auto_cover
            && (align.title != align.cfg_title
                || align.area_id != align.area_v2
                || align.video_id_changed)
        {
            spawn_cover_update(
                cfg,
                align.platform,
                align.channel_id,
                align.current_video_id.map(str::to_owned),
            );
        }
    }
    AlignBiliRoom::Ready { history_area_v2 }
}

/// Per-platform ffmpeg inputs for one push, keyed by the platform code the pass
/// carries. These three matches used to sit in the middle of the ffmpeg loop,
/// where they were 31 lines of repetitive arms.
struct PlatformPushOptions {
    proxy: Option<String>,
    crop: Option<(u32, u32, u32, u32)>,
    cache: bilistream::config::FfmpegCache,
}

fn platform_push_options(cfg: &bilistream::config::Config, platform: &str) -> PlatformPushOptions {
    let crop = |c: &Option<bilistream::config::CropConfig>| {
        c.as_ref().map(|c| (c.width, c.height, c.x, c.y))
    };
    // The fallbacks match the original three matches exactly: an unknown platform
    // gets no proxy and no crop, and reads the Twitch cache settings.
    match platform {
        "YT" => PlatformPushOptions {
            proxy: cfg.youtube.proxy.clone(),
            crop: crop(&cfg.youtube.crop),
            cache: cfg.youtube.ffmpeg_cache.clone(),
        },
        "TW" => PlatformPushOptions {
            proxy: cfg.twitch.proxy.clone(),
            crop: crop(&cfg.twitch.crop),
            cache: cfg.twitch.ffmpeg_cache.clone(),
        },
        "NC" => PlatformPushOptions {
            proxy: cfg.niconico.proxy.clone(),
            crop: crop(&cfg.niconico.crop),
            cache: cfg.niconico.ffmpeg_cache.clone(),
        },
        _ => PlatformPushOptions {
            proxy: None,
            crop: None,
            cache: cfg.twitch.ffmpeg_cache.clone(),
        },
    }
}

/// The LoL monitor refuses a stream whose title carries a banned word: stop the
/// Bilibili broadcast (the room is offline because of the keyword, not because
/// the source went away), then tell the caller to wait. Returns true when this
/// pass must not push the stream.
async fn rejected_by_keyword(
    cfg: &bilistream::config::Config,
    monitor: &LolMonitor,
    bili_is_live: bool,
) -> bool {
    let Some(word) = monitor.invalid_word().await else {
        return false;
    };
    tracing::error!("检测到非法词汇:{}，不转播", word);
    if bili_is_live {
        match bili_stop_live(cfg).await {
            Ok(()) => bilistream::notify::live_ended(BroadcastEndCause::ContentRejected),
            Err(error) => tracing::error!("停止 B 站直播失败: {}", error),
        }
    }
    true
}

/// Wait for this FFmpeg session to exit while sampling whether data is actually
/// moving.
///
/// The sampling is the point: only time during which data moves may clear the
/// stream-end error window (N1), so a stalled session that exits cleanly still
/// counts as a failure.
async fn wait_for_push_exit(
    session: Option<ffmpeg::FfmpegSession>,
) -> (Option<std::process::ExitStatus>, PushMovingTime) {
    let mut push_moving = PushMovingTime::default();
    let exit_status = match session {
        Some(session) => {
            let wait = ffmpeg::wait_ffmpeg_session(session);
            tokio::pin!(wait);
            let mut tick = tokio::time::interval(PUSH_SAMPLE_INTERVAL);
            loop {
                tokio::select! {
                    status = &mut wait => break status,
                    _ = tick.tick() => {
                        push_moving.sample(std::time::Instant::now(), current_push_sample());
                    }
                }
            }
        }
        None => None,
    };
    (exit_status, push_moving)
}

/// Attach the monitors that watch one push: the LoL keyword monitor when this
/// stream has one, and the priority-channel monitor that keeps the priority
/// source's URL warm. Both take the session they observe.
async fn attach_push_monitors(
    session: ffmpeg::FfmpegSession,
    stream_log: &FfmpegStreamIdentity,
    lol_monitor: Option<LolMonitor>,
    selected_stream: &StreamCandidate,
) {
    if let Some(monitor) = lol_monitor {
        ffmpeg::spawn_session_task(
            session,
            monitor.run(session, stream_log.session_label(Some(session))),
        )
        .await;
    }
    ffmpeg::spawn_session_task(
        session,
        monitor_priority_channel_background(
            selected_stream.source_identity(),
            session,
            stream_log.session_label(Some(session)),
        ),
    )
    .await;
}

/// What the pass knew when the push ended. Distinct from [`EndSignals`], which is
/// the end-recheck input; field names match the locals the loop used before, so
/// the restart logic below reads unchanged.
struct PushEndSignals {
    bili_is_live: bool,
    manual_restart_requested: bool,
    manual_stop_requested: bool,
    config_updated_now: bool,
    warning_skip_now: bool,
}

/// Gather the signals that decide whether an FFmpeg exit counts as a failure, and
/// record that decision with the cluster. A failed Bilibili status read counts as
/// "still live" so a network blip cannot turn a real stream end into an error.
async fn collect_end_signals(
    cfg: &bilistream::config::Config,
    channel_name: &str,
    current_is_live: bool,
    ffmpeg_exit_failed: bool,
) -> (PushEndSignals, bool) {
    let bili_is_live = fetch_bili_live_status_logged(cfg.bililive.room)
        .await
        .map(|status| status.is_live)
        .unwrap_or(true);
    let signals = PushEndSignals {
        bili_is_live,
        manual_restart_requested: was_manual_restart(),
        manual_stop_requested: was_manual_stop(),
        config_updated_now: is_config_updated(),
        warning_skip_now: should_skip_due_to_warning(channel_name),
    };
    let count_failure = ffmpeg_exit_should_count_as_failure(
        ffmpeg_exit_failed,
        current_is_live,
        signals.bili_is_live,
        signals.manual_restart_requested,
        signals.manual_stop_requested,
        signals.config_updated_now,
        signals.warning_skip_now,
    );
    cluster::record_stream_exit(!count_failure);
    (signals, count_failure)
}

/// What one end-recheck outcome means for the pass that is pushing.
struct RefetchOutcome {
    current_is_live: bool,
    new_m3u8_url: Option<String>,
    /// Whether the 直播结束 notice still needs to go out for this broadcast.
    end_announce: bool,
    source_ended: bool,
    new_broadcast: bool,
    end_check_cancel: Option<EndRecheckCancel>,
}

/// Interpret one end-recheck step: log what happened, and say whether the pass
/// keeps pushing, whether the source ended, and whether the whole broadcast is
/// over.
///
/// The source ending is not the room ending: the room stays live for the 换台
/// window, and the session's 下播 goes out when the room itself goes offline.
fn interpret_refetch_step(step: RefetchStep, stream_log: &FfmpegStreamIdentity) -> RefetchOutcome {
    let mut outcome = RefetchOutcome {
        current_is_live: true,
        new_m3u8_url: None,
        end_announce: false,
        source_ended: false,
        new_broadcast: false,
        end_check_cancel: None,
    };
    match step {
        RefetchStep::Restart { m3u8_url } => outcome.new_m3u8_url = m3u8_url,
        RefetchStep::Retry => {
            tracing::warn!("{stream_log} 源状态请求失败，按网络错误重试推流");
        }
        RefetchStep::Ended {
            announce,
            timed_out,
        } => {
            if timed_out {
                tracing::warn!(
                    "{stream_log} 源状态请求连续失败{}秒且无在线回应，按直播结束处理",
                    STREAM_END_ERROR_WINDOW.as_secs()
                );
            } else {
                tracing::info!("{stream_log} 源播放列表刷新未找到直播，源直播已结束");
            }
            outcome.current_is_live = false;
            outcome.end_announce = announce;
            outcome.source_ended = true;
        }
        RefetchStep::NewBroadcast => outcome.new_broadcast = true,
        RefetchStep::Cancelled(cancel) => {
            outcome.current_is_live = false;
            outcome.end_check_cancel = Some(cancel);
        }
    }
    outcome
}

/// Clear every platform's crop setting once a stream ends, so the next stream
/// starts uncropped, and persist the change if anything was cleared.
///
/// The caller decides *whether* to do this: a manual restart keeps the crop so it
/// still applies to the restarted stream.
async fn clear_crop_settings(cfg: &mut bilistream::config::Config) {
    let mut config_changed = false;
    if cfg.youtube.crop.is_some() {
        tracing::info!("清除YouTube裁剪设置");
        cfg.youtube.crop = None;
        config_changed = true;
    }
    if cfg.twitch.crop.is_some() {
        tracing::info!("清除Twitch裁剪设置");
        cfg.twitch.crop = None;
        config_changed = true;
    }
    if cfg.niconico.crop.is_some() {
        tracing::info!("清除Niconico裁剪设置");
        cfg.niconico.crop = None;
        config_changed = true;
    }
    if config_changed {
        if let Err(e) = save_config(cfg).await {
            tracing::error!("保存配置失败: {}", e);
        }
    }
}

async fn run_bilistream(ffmpeg_log_level: &str) -> Result<(), Box<dyn std::error::Error>> {
    // Initialize the logger with timestamp format : 2024-11-21 12:00:00
    // Only init if not already initialized (webui mode initializes it earlier)
    if !tracing::dispatcher::has_been_set() {
        init_logger();
    }

    if is_ffmpeg_running().await {
        // Stop any existing ffmpeg process
        stop_ffmpeg().await;
    }

    // Credential renewal is explicit and runs after the interface is available.
    bilistream::config::refresh_credentials().await?;
    let mut state = PassState::default();

    'outer: loop {
        // Log outer loop restart for debugging channel switch issues
        tracing::debug!("外层循环开始 - 重新加载配置并检查频道状态");

        let mut cfg = load_pass_config().await?;

        if wait_out_cluster_block(&cfg, &mut state.cluster_wait).await {
            continue 'outer;
        }
        if let Some((started, _)) = state.cluster_wait.take() {
            tracing::info!(
                "集群监控条件已就绪，等待耗时 {:.2} 秒",
                started.elapsed().as_secs_f64()
            );
        }

        sync_danmaku_client(&cfg).await;

        // room_init.lock_till is the ban end. Wait it out before probing sources
        // or calling startLive; get_info does not carry the lock.
        if wait_out_bili_room_lock(cfg.bililive.room).await {
            continue 'outer;
        }

        if require_a_configured_source(&cfg).await {
            continue 'outer;
        }

        if config_changed_since_pass(&cfg) {
            clear_config_updated();
            tracing::info!("检测到配置更新，重新加载配置并检查频道状态");
            continue 'outer;
        }

        let warnings = warning_tracker();
        let warning_before_probe = warnings.snapshot();
        let probe_plan = SourceProbePlan::for_config(&cfg);
        let Some(probes) = probe_sources(&cfg, &probe_plan).await else {
            clear_config_updated();
            continue 'outer;
        };
        let mut youtube = probed_status(&probes, probe_plan.youtube);
        if youtube
            .start_time
            .as_ref()
            .is_some_and(|start| start > &(Local::now() + Duration::from_secs(2 * 24 * 60 * 60)))
        {
            youtube.start_time = None;
        }
        let priority_youtube = probed_status(&probes, probe_plan.priority_youtube);
        let twitch = probed_status(&probes, probe_plan.twitch);
        let priority_twitch = probed_status(&probes, probe_plan.priority_twitch);
        let nico_live = if cfg.niconico.enable_monitor && niconico_configured(&cfg.niconico) {
            Some(NiconicoClient::new(&cfg.niconico))
        } else {
            None
        };
        let nico_status = if nico_live.is_some() {
            Some(bilistream::plugins::niconico_live::monitor_status(&cfg.niconico, false).await)
        } else {
            None
        };
        let niconico = nico_status
            .as_ref()
            .and_then(|status| status.as_ref().ok())
            .cloned()
            .unwrap_or(SourceStatus::OFFLINE);

        if config_changed_since_pass(&cfg) {
            clear_config_updated();
            continue 'outer;
        }
        let (nico_channel_id, nico_channel_name) = niconico_channel_identity(&cfg.niconico);
        let sources = PassSources {
            youtube,
            twitch,
            niconico,
            nico_channel_id,
            nico_channel_name,
            priority_youtube,
            priority_twitch,
        };
        let mut observations = Vec::new();
        for (index, platform, name, id) in [
            (
                probe_plan.youtube,
                StreamPlatform::Youtube,
                &cfg.youtube.channel_name,
                &cfg.youtube.channel_id,
            ),
            (
                probe_plan.priority_youtube,
                StreamPlatform::Youtube,
                &cfg.priority_channel.channel_name,
                &cfg.priority_channel.youtube_channel_id,
            ),
            (
                probe_plan.twitch,
                StreamPlatform::Twitch,
                &cfg.twitch.channel_name,
                &cfg.twitch.channel_id,
            ),
            (
                probe_plan.priority_twitch,
                StreamPlatform::Twitch,
                &cfg.priority_channel.channel_name,
                &cfg.priority_channel.twitch_channel_id,
            ),
        ] {
            if let Some(status) = index.and_then(|index| probes[index].status.as_ref().ok()) {
                observations.push(source_observation(platform, name, id, status));
            }
        }
        if let Some(Ok(status)) = &nico_status {
            observations.push(source_observation(
                StreamPlatform::Niconico,
                &sources.nico_channel_name,
                &sources.nico_channel_id,
                status,
            ));
        }
        if let Some(release) = bilistream::config::with_current_config(&cfg, || {
            warnings.observe_sources(&warning_before_probe, &observations)
        })
        .flatten()
        {
            release.log();
        }

        let Some(bili_live) = observe_bili_room(&mut state, &cfg).await else {
            wait_config_update_or_timeout(Duration::from_secs(cfg.interval)).await;
            continue 'outer;
        };
        let bili_is_live = bili_live.is_live;
        let bili_title = bili_live.title.clone();
        let bili_area_id = bili_live.area_id;
        let mut bili_status = bilistream::BiliStatus {
            enable_danmaku_command: cfg.bililive.enable_danmaku_command,
            ..Default::default()
        };
        bili_status.apply_live_status(&bili_live);

        publish_webui_status(&cfg, bili_status, &sources);
        let scheduled_start = sources.youtube.start_time;

        let streaming_banned_keywords =
            bilistream::plugins::banned_keywords::streaming_banned_keywords();
        let mut candidates = stream_candidates(&cfg, sources);

        // Observe first, then filter every slot before the priority overlay.
        // A shared ID has one provider result; the skip logger deduplicates it.
        for stream in [
            &mut candidates.youtube,
            &mut candidates.twitch,
            &mut candidates.niconico,
            &mut candidates.priority_youtube,
            &mut candidates.priority_twitch,
        ] {
            let was_live = stream.is_live;
            skip_stream_if_previously_warned(stream, &cfg, &warnings).await;
            skip_stream_if_banned_keyword(stream, &streaming_banned_keywords, &cfg).await;
            if stream.platform == StreamPlatform::Youtube {
                mark_youtube_skipped_live(stream, was_live);
            }
        }
        if config_changed_since_pass(&cfg) {
            clear_config_updated();
            continue 'outer;
        }
        let needs_refetch = refetch_skipped_youtube(&candidates.youtube)
            | refetch_skipped_youtube(&candidates.priority_youtube);
        let mut yt_stream = overlay_priority(candidates.youtube, candidates.priority_youtube);
        let mut tw_stream = overlay_priority(candidates.twitch, candidates.priority_twitch);
        let nico_stream = candidates.niconico;
        let priority_selected = select_stream(&yt_stream, &tw_stream, &nico_stream)
            .is_some_and(|stream| stream.is_priority);
        if cfg.enable_anti_collision && !priority_selected {
            let mut yt_is_live = yt_stream.is_live;
            let mut tw_is_live = tw_stream.is_live;
            match handle_collisions(&mut yt_is_live, &mut tw_is_live).await? {
                CollisionResult::Continue => continue 'outer,
                CollisionResult::Proceed => (),
            }
            yt_stream.is_live = yt_is_live;
            tw_stream.is_live = tw_is_live;
        }

        if needs_refetch && select_stream(&yt_stream, &tw_stream, &nico_stream).is_none() {
            continue 'outer;
        }
        report_danmaku_selection(
            &cfg,
            &probes,
            &yt_stream,
            &tw_stream,
            &nico_stream,
            &streaming_banned_keywords,
            &warnings.snapshot(),
        );
        if yt_stream.is_live || tw_stream.is_live || nico_stream.is_live {
            NO_LIVE.store(false, Ordering::SeqCst);
            let Some(mut selected_stream) = select_stream(&yt_stream, &tw_stream, &nico_stream)
            else {
                if yt_stream.is_live && yt_stream.m3u8_url.is_none() {
                    tracing::warn!(
                        "YouTube 直播 {} 缺少可播放的流URL，跳过本轮转播",
                        yt_stream.channel_name
                    );
                }
                if tw_stream.is_live && tw_stream.m3u8_url.is_none() {
                    tracing::warn!(
                        "Twitch 直播 {} 缺少可播放的流URL，跳过本轮转播",
                        tw_stream.channel_name
                    );
                }
                if nico_stream.is_live && nico_stream.piped_ingest.is_none() {
                    tracing::warn!(
                        "Niconico 直播 {} 缺少可播放的摄取源，跳过本轮转播",
                        nico_stream.channel_name
                    );
                }
                tracing::warn!("未找到可转播的直播候选，等待下一轮检查");
                wait_config_update_or_timeout(Duration::from_secs(cfg.interval)).await;
                continue 'outer;
            };
            let Some(mut ffmpeg_source) = selected_stream.ffmpeg_source() else {
                tracing::warn!(
                    "{} 直播 {} 缺少可播放的流，等待下一轮检查",
                    selected_stream.platform.code(),
                    selected_stream.channel_name
                );
                wait_config_update_or_timeout(Duration::from_secs(cfg.interval)).await;
                continue 'outer;
            };

            if selected_stream.platform == StreamPlatform::Youtube {
                bilistream::plugins::youtube::clear_skipped_live(&selected_stream.channel_id);
            }

            let platform = selected_stream.platform.code();
            let channel_name = selected_stream.channel_name.clone();
            let channel_id = selected_stream.channel_id.clone();
            let mut area_v2 = selected_stream.area_v2;
            let cfg_title = selected_stream.cfg_title();
            let current_video_id = selected_stream.stream_id.clone();
            let mut title = selected_stream.title.clone();
            let stream_log = FfmpegStreamIdentity::new(
                &cfg.cluster.node_id,
                platform,
                &channel_name,
                &channel_id,
            )
            .with_episode_id(current_video_id.as_deref());
            let cluster_stream = ClusterStreamIdentity {
                platform: platform.to_string(),
                channel_name: channel_name.clone(),
                channel_id: channel_id.clone(),
                stream_id: current_video_id.clone(),
                title: title.clone(),
            };

            // Check if video/stream ID has changed
            let video_id_changed = {
                let mut last_id = recover_mutex_lock(&LAST_VIDEO_ID, "last video id");
                let changed = last_id.as_ref() != current_video_id.as_ref();
                if changed {
                    *last_id = current_video_id.clone();
                }
                changed
            };
            log_selected_stream(&stream_log, title.as_deref(), selected_stream.is_priority);

            if selected_stream.topic.is_some() && title.is_some() {
                title = selected_stream.stream_title();
                selected_stream.title = title.clone();
            }
            if !cluster::local_may_push(&cfg, Some(cluster_stream.clone())) {
                tracing::info!("{stream_log} 源直播可用，本节点等待集群执行许可");
                tokio::time::sleep(Duration::from_secs(cfg.interval)).await;
                continue 'outer;
            }
            let default_title = "无标题".to_string();
            let title_str = title.as_ref().unwrap_or(&default_title);
            area_v2 = check_area_id_with_title(title_str, area_v2);
            // Complete the initial ID check before starting either output.
            let lol_monitor = if area_v2 == 86 && cfg.enable_lol_monitor {
                let puuid = get_puuid(&channel_name)?;
                LolMonitor::new(&cfg, puuid)
            } else {
                None
            };
            if let Some(monitor) = &lol_monitor {
                if rejected_by_keyword(&cfg, monitor, bili_is_live).await {
                    wait_config_update_or_timeout(Duration::from_secs(cfg.interval.max(1))).await;
                    continue;
                }
            }

            // Disable danmaku commands only once we are committed to this stream (past skip paths).
            if is_danmaku_commands_enabled() {
                enable_danmaku_commands(false);
            }
            // Reuse bili_is_live, bili_title, bili_area_id from earlier check (line 200)
            let AlignBiliRoom::Ready { history_area_v2 } = align_bili_room(
                &mut cfg,
                &stream_log,
                BiliAlign {
                    is_live: bili_is_live,
                    title: &bili_title,
                    area_id: bili_area_id,
                    area_v2,
                    cfg_title: &cfg_title,
                    platform,
                    channel_id: &channel_id,
                    channel_name: &channel_name,
                    video_id_changed,
                    current_video_id: current_video_id.as_deref(),
                },
            )
            .await
            else {
                continue 'outer;
            };

            if config_changed_since_pass(&cfg) {
                continue 'outer;
            }
            // The room is live and this pass has committed to a source: opens
            // the session's 开播, or records a 换台 inside it (Addendum E rev 5
            // item 20). Emitted here so the notice names the source the push
            // actually uses, and re-emitted per pass so a source change is seen.
            bilistream::notify::live_source(LiveSource {
                stream_key: format!("{platform}:{channel_id}"),
                platform: live_platform_name(platform).to_string(),
                channel: channel_name.clone(),
                title: title_str.to_string(),
            });
            let source_client = selected_source_client(&selected_stream, &probes, &nico_live);

            state.stream_end.start_session(
                format!("{platform}:{channel_id}"),
                current_video_id.as_deref(),
            );
            let mut end_announce = false;
            let mut session_generation = capture_session_generation();

            // Main ffmpeg monitoring loop - blocks until stream ends
            let mut last_session = None;
            // 统计: one relay session per stream selection, opened by the
            // first real push; dropping it unfinished records `aborted`.
            let mut history_session: Option<crate::history::SessionGuard> = None;
            let ffmpeg_loop_exit_reason = loop {
                if !cluster::local_may_push(&cfg, Some(cluster_stream.clone())) {
                    set_manual_restart();
                    stop_ffmpeg().await;
                    break FfmpegLoopExitReason::ClusterHandoff;
                }

                let PlatformPushOptions { proxy, crop, cache } =
                    platform_push_options(&cfg, platform);

                // Stop/restart requests from before this session must not
                // cancel its end-of-stream recheck.
                session_generation = capture_session_generation();
                let run_history = ffmpeg::FfmpegRunHistory::default();
                let session = ffmpeg(ffmpeg::FfmpegRequest {
                    rtmp_url: cfg.bililive.bili_rtmp_url.clone(),
                    rtmp_key: cfg.bililive.bili_rtmp_key.clone(),
                    source: ffmpeg_source.clone(),
                    proxy,
                    log_level: ffmpeg_log_level.to_string(),
                    crop,
                    cache: FfmpegCacheOptions {
                        enabled: cache.enabled,
                        latency_secs: cache.latency_secs,
                    },
                    identity: stream_log.clone().with_history(run_history.clone()),
                })
                .await;
                last_session = session;
                if let Some(run) = session {
                    let guard = history_session.get_or_insert_with(|| {
                        crate::history::SessionGuard::start_with_area(
                            &cfg,
                            platform,
                            &channel_id,
                            &channel_name,
                            Some(&cfg_title),
                            history_area_v2,
                            current_video_id.as_deref(),
                        )
                    });
                    guard.run_started(run.id());
                    run_history.attach(guard.seq(), run.id());
                }

                if let Some(session) = session {
                    attach_push_monitors(
                        session,
                        &stream_log,
                        lol_monitor.clone(),
                        &selected_stream,
                    )
                    .await;
                }

                let (exit_status, push_moving) = wait_for_push_exit(session).await;

                // The supervisor owns child exit diagnostics. None can also
                // mean declined startup or an intentional/obsolete session.
                let ffmpeg_exit_at = bilistream::storage::now();
                let ffmpeg_exit_failed = exit_status.is_none_or(|status| !status.success());

                // Check if stream is still live before restarting
                tokio::time::sleep(Duration::from_secs(2)).await;

                // The end of the stream is this refetch failing to find the
                // source live: no new m3u8. Same signal as the page's 切换.
                let node_active = !cfg.cluster.enabled || cluster::local_monitoring_allowed(&cfg);
                state.stream_end.on_push_ended(push_moving.moving);
                let step = state.stream_end.on_refetch(
                    refetch_source(&source_client).await,
                    current_end_signals(session_generation, node_active),
                    std::time::Instant::now(),
                );
                let outcome = interpret_refetch_step(step, &stream_log);
                // `end_announce` is declared outside the loop and read after it,
                // so it must be assigned rather than shadowed.
                end_announce = outcome.end_announce;
                let RefetchOutcome {
                    current_is_live,
                    new_m3u8_url,
                    source_ended,
                    new_broadcast,
                    end_check_cancel,
                    ..
                } = outcome;
                let (
                    PushEndSignals {
                        bili_is_live,
                        manual_restart_requested,
                        config_updated_now,
                        ..
                    },
                    count_ffmpeg_exit_failure,
                ) = collect_end_signals(&cfg, &channel_name, current_is_live, ffmpeg_exit_failed)
                    .await;
                if let (Some(guard), Some(run)) = (&history_session, session) {
                    guard.run_exited_with_diagnostic_at(
                        ffmpeg_exit_at,
                        run.id(),
                        exit_status.and_then(|status| status.code()),
                        !ffmpeg_exit_failed,
                        count_ffmpeg_exit_failure,
                        run_history.diagnostic().as_deref(),
                    );
                }

                match end_check_cancel {
                    Some(EndRecheckCancel::ClusterHandoff) => {
                        set_manual_restart();
                        stop_ffmpeg().await;
                        break FfmpegLoopExitReason::ClusterHandoff;
                    }
                    Some(EndRecheckCancel::ManualRestart) => {
                        break FfmpegLoopExitReason::EndCheckCancelled { restart: true };
                    }
                    Some(EndRecheckCancel::ManualStop) => {
                        break FfmpegLoopExitReason::EndCheckCancelled { restart: false };
                    }
                    None => {}
                }
                if new_broadcast {
                    break FfmpegLoopExitReason::NewBroadcast;
                }

                if source_ended {
                    break FfmpegLoopExitReason::SourceEnded;
                }

                if !bili_is_live {
                    break FfmpegLoopExitReason::BiliStopped;
                }

                // Check if manual restart was requested (force immediate restart)
                if manual_restart_requested {
                    let exit_reason = FfmpegLoopExitReason::IntentionalRestart {
                        target_m3u8_available: new_m3u8_url.is_some(),
                    };
                    break exit_reason;
                }

                // Check if config was updated (channel switch)
                if config_updated_now || !bilistream::config::config_is_current(&cfg) {
                    tracing::debug!(
                        "{stream_log} 已检测到配置更新，源直播仍在线，沿用本次目标直到源直播结束"
                    );
                    // Don't break, let the stream continue until it naturally ends
                }

                // YouTube/Twitch HLS playlist URLs can rotate mid-live.
                // Niconico's lv is the program id (like a YouTube video id) and
                // does not change for one stream, so piped ingest is left as-is.
                if let FfmpegSource::HlsUrl(url) = &mut ffmpeg_source {
                    if let Some(new_url) = new_m3u8_url {
                        if new_url != *url {
                            tracing::debug!("{stream_log} 源播放地址已更新，下次启动使用新地址");
                            *url = new_url;
                        }
                    }
                }

                // Stream is still live but ffmpeg exited, restart it
                tracing::info!(
                    "{} 源直播仍在线，1秒后重试推流",
                    stream_log.session_label(session)
                );
                tokio::time::sleep(Duration::from_secs(1)).await;
            };
            let session_log = stream_log.session_label(last_session);
            // Demotion and the execution watchdog also signal MANUAL_RESTART.
            // Report the current gate without changing restart/cleanup decisions.
            let execution_block = cluster::local_monitoring_block_reason(&cfg);
            log_ffmpeg_loop_exit(
                &session_log,
                ffmpeg_loop_exit_reason,
                execution_block.as_deref(),
            );

            // Check the actual reason for ffmpeg loop exit
            let manual_stop = was_manual_stop();
            if let Some(guard) = history_session.take() {
                guard.finish(history_end_reason(ffmpeg_loop_exit_reason, manual_stop));
            }
            if matches!(ffmpeg_loop_exit_reason, FfmpegLoopExitReason::BiliStopped) {
                crate::history::on_bili_stopped(&cfg);
            }
            let manual_restart = was_manual_restart();
            let warning_skip = should_skip_due_to_warning(&channel_name);
            let restart_exit_should_skip_danmaku =
                restart_exit_should_skip_end_danmaku(ffmpeg_loop_exit_reason);

            if manual_restart {
                clear_manual_restart();
            }
            if manual_stop
                && !restart_exit_should_skip_danmaku
                && !matches!(ffmpeg_loop_exit_reason, FfmpegLoopExitReason::BiliStopped)
            {
                clear_manual_stop();
            }

            // Don't clear on a manual restart - let the crop apply to the
            // restarted stream.
            if !exit_keeps_crop(ffmpeg_loop_exit_reason) {
                clear_crop_settings(&mut cfg).await;
            }

            // Stop priority monitoring when ffmpeg loop ends
            cluster::clear_local_stream();

            // SourceEnded is the refetch miss itself; reading the source once
            // more could flap back and turn 结束 into 中断.
            // Other exits read the source strictly: only a positive offline
            // statement counts; a failed request reads as still live.
            let current_is_live = match ffmpeg_loop_exit_reason {
                FfmpegLoopExitReason::SourceEnded => false,
                _ => !matches!(
                    refetch_source(&source_client).await,
                    RefetchReading::Offline
                ),
            };
            // On error, assume still live to avoid incorrect status messages.
            let bili_is_live = fetch_bili_live_status_logged(cfg.bililive.room)
                .await
                .map(|status| status.is_live)
                .unwrap_or(true);

            let action = end_action(
                ffmpeg_loop_exit_reason,
                manual_stop,
                warning_skip,
                current_is_live,
                bili_is_live,
            );
            if matches!(
                action,
                EndAction::SkipHandoff
                    | EndAction::SkipRestart
                    | EndAction::SkipEndCheckCancelled
                    | EndAction::SkipNewBroadcast
                    | EndAction::SkipManualStop
            ) && manual_stop
            {
                clear_manual_stop();
            }
            // One 下播 for the session, from the action that left the room
            // offline. `LiveLedger::end` only closes an open session and the
            // first cause wins, so an earlier precise cause (a warning or a
            // manual stop) is never overwritten.
            if let Some(cause) = end_cause(action, bili_is_live) {
                bilistream::notify::live_ended(cause);
            }
            match action {
                EndAction::SkipHandoff => {
                    tracing::debug!("{session_log} 集群交接，跳过结束弹幕");
                }
                EndAction::SkipRestart => {
                    tracing::debug!("{session_log} 主动重启或切换，跳过结束弹幕");
                }
                EndAction::SkipNewBroadcast => {
                    tracing::debug!("{session_log} 源频道新直播，跳过结束弹幕");
                }
                EndAction::SkipEndCheckCancelled => {
                    tracing::debug!("{session_log} 确认结束期间收到停止或重启，跳过结束弹幕");
                }
                EndAction::SkipManualStop => {
                    tracing::debug!("{session_log} 主动停止，跳过结束弹幕");
                }
                EndAction::SkipWarning => {
                    tracing::info!("{session_log} 因B站警告或切断结束转播");
                }
                EndAction::AnnounceEnd { bili_live } => {
                    // B站 stopped first (or another exit) with the source
                    // positively offline: mark it ended through the same
                    // state, with the same active-node and generation checks.
                    if ffmpeg_loop_exit_reason != FfmpegLoopExitReason::SourceEnded {
                        let node_active =
                            !cfg.cluster.enabled || cluster::local_monitoring_allowed(&cfg);
                        let signals = post_loop_end_signals(
                            ffmpeg_loop_exit_reason,
                            current_end_signals(session_generation, node_active),
                        );
                        let ended = state.stream_end.mark_ended(signals);
                        end_announce = ended.unwrap_or(false);
                    }
                    if bili_live {
                        tracing::debug!("{session_log} 已确认源直播结束，B站房间仍开播");
                    } else {
                        tracing::debug!("{session_log} 已确认源直播结束，B站房间已停播");
                    }
                    // One shared ended state drives the 换台 gate (and with
                    // it the page's 切换 button) and the danmaku. A node that
                    // lost the active role since the miss does neither.
                    let node_active =
                        !cfg.cluster.enabled || cluster::local_monitoring_allowed(&cfg);
                    let gate = state
                        .stream_end
                        .commands_gate(cfg.bililive.enable_danmaku_command, node_active);
                    if gate {
                        enable_danmaku_commands(true);
                    }
                    let send_result = if node_active && end_announce {
                        if gate {
                            tokio::time::sleep(Duration::from_secs(2)).await;
                        }
                        let text =
                            end_danmaku_text(&channel_name, cfg.bililive.enable_danmaku_command);
                        send_danmaku(&cfg, &text).await.map(|_| ())
                    } else {
                        tracing::debug!("{session_log} 本节点非活动或本场已发送结束弹幕，不再发送");
                        Ok(())
                    };
                    if let Err(e) = send_result {
                        if bili_live {
                            tracing::error!("Failed to send danmaku: {}", e);
                        } else {
                            // Best effort: the room is already offline.
                            tracing::info!("{session_log} B站房间已停播，结束弹幕未发送: {}", e);
                        }
                    }

                    // Announce first, then stop a room that has been live for
                    // AUTO_STOP_AFTER_HOURS (3) hours or more.
                    if bili_live {
                        match get_bili_live_time(cfg.bililive.room).await {
                            Ok(Some(live_start)) => {
                                let hours = chrono::Local::now()
                                    .signed_duration_since(live_start)
                                    .num_hours();
                                if should_auto_stop_after_end(hours) {
                                    tracing::info!(
                                        "B站直播已超过{}小时（{}小时），自动停播",
                                        AUTO_STOP_AFTER_HOURS,
                                        hours
                                    );
                                    match bili_stop_live(&cfg).await {
                                        Ok(()) => bilistream::notify::live_ended(
                                            BroadcastEndCause::AutoStop,
                                        ),
                                        Err(e) => tracing::error!("自动停播失败: {}", e),
                                    }
                                }
                            }
                            Ok(None) => {}
                            Err(e) => {
                                tracing::warn!("获取B站直播时间失败: {}", e);
                            }
                        }
                    }
                }
                EndAction::BiliStoppedQuietly => {
                    tracing::debug!("{session_log} 已确认B站房间停止直播");
                    if cfg.bililive.enable_danmaku_command {
                        enable_danmaku_commands(true);
                    }
                }
                EndAction::AnnounceInterrupted => {
                    // Both streams are still live - this was likely a technical issue
                    tracing::info!("{session_log} 推流会话已结束，源直播与B站房间仍在线，返回监听");
                    if cfg.bililive.enable_danmaku_command {
                        enable_danmaku_commands(true);
                        tokio::time::sleep(Duration::from_secs(2)).await;
                        if let Err(e) = send_danmaku(
                            &cfg,
                            &format!("{} 流传输中断，可使用弹幕指令进行换台", channel_name),
                        )
                        .await
                        {
                            tracing::error!("Failed to send danmaku: {}", e);
                        }
                    }
                }
            }
        } else {
            // 计划直播(预告窗)
            if let Some(scheduled_start) = scheduled_start {
                let current_message = box_message(
                    &yt_stream.channel_name,
                    cfg.youtube.enable_monitor,
                    Some(scheduled_start),
                    yt_stream.title.as_deref(),
                    &tw_stream.channel_name,
                    cfg.twitch.enable_monitor,
                );
                update_last_status_message(current_message);
            } else if !NO_LIVE.load(Ordering::SeqCst) {
                let current_message = box_message(
                    &yt_stream.channel_name,
                    cfg.youtube.enable_monitor,
                    None,
                    None, // No title when not streaming
                    &tw_stream.channel_name,
                    cfg.twitch.enable_monitor,
                );
                print!("{}", current_message);
                let mut last = recover_mutex_lock(&LAST_MESSAGE, "last message");
                *last = Some(current_message.into_boxed_str());
                NO_LIVE.store(true, Ordering::SeqCst);
            }
            if cfg.bililive.enable_danmaku_command && !is_danmaku_commands_enabled() {
                enable_danmaku_commands(true);
            }

            // Check if config was updated (skip waiting if so)
            if config_changed_since_pass(&cfg) {
                clear_config_updated();
                tracing::info!("检测到配置更新，重新加载配置并检查频道状态");
                continue 'outer;
            }

            // Sleep with periodic checks for config updates
            let sleep_duration = cfg.interval;
            let check_interval = 2; // Check every 2 seconds
            let mut elapsed = 0;

            while elapsed < sleep_duration {
                let sleep_time = std::cmp::min(check_interval, sleep_duration - elapsed);
                if wait_config_update_or_timeout(Duration::from_secs(sleep_time)).await {
                    clear_config_updated();
                    tracing::info!("等待期间检测到配置更新，重新加载配置并检查频道状态");
                    continue 'outer;
                }
                elapsed += sleep_time;

                if let Some(channel) = idle_wake_channels(&cfg)
                    .into_iter()
                    .find(|channel| bilistream::plugins::youtube::take_monitor_wake(channel))
                {
                    if channel == cfg.youtube.channel_id {
                        tracing::info!("YouTube 显示目标频道已开播，立即检查");
                    } else {
                        tracing::info!("YouTube 显示优先频道已开播，立即检查");
                    }
                    continue 'outer;
                }
                if bilistream::plugins::twitch_live::take_monitor_wake(&cfg.twitch.channel_id) {
                    tracing::info!("Twitch 显示目标频道已开播，立即检查");
                    continue 'outer;
                }
                if bilistream::plugins::niconico_live::take_monitor_wake(&cfg.niconico.channel_id) {
                    tracing::info!("Niconico 显示目标频道已开播，立即检查");
                    continue 'outer;
                }

                // Check if config was updated during sleep
                if config_changed_since_pass(&cfg) {
                    clear_config_updated();
                    tracing::info!("等待期间检测到配置更新，重新加载配置并检查频道状态");
                    continue 'outer;
                }
            }
        }
    }
}

fn priority_monitor_enabled(cfg: &Config, current_source: &BroadcastSource) -> bool {
    cfg.priority_channel.enabled
        && cfg.priority_channel.auto_restart
        && !cfg.priority_channel.channel_name.is_empty()
        && !((current_source.platform == "YT"
            && !cfg.priority_channel.youtube_channel_id.is_empty()
            && current_source.channel_id
                == canonical_channel_id("YT", &cfg.priority_channel.youtube_channel_id))
            || (current_source.platform == "TW"
                && !cfg.priority_channel.twitch_channel_id.is_empty()
                && current_source.channel_id
                    == canonical_channel_id("TW", &cfg.priority_channel.twitch_channel_id)))
        && (!cfg.priority_channel.youtube_channel_id.is_empty()
            || !cfg.priority_channel.twitch_channel_id.is_empty())
}

fn priority_is_held(cfg: &Config, warning: &WarningSnapshot) -> bool {
    bilistream::plugins::danmaku::priority_warning_matches(warning, cfg)
}

fn should_probe_priority(
    cfg: &Config,
    current_source: &BroadcastSource,
    warning: &WarningSnapshot,
) -> bool {
    priority_monitor_enabled(cfg, current_source) && !priority_is_held(cfg, warning)
}

/// Channels whose go-live ends the main loop's not-live wait: the target, and
/// the priority channel while priority switching is on (the pass checks it too).
fn idle_wake_channels(cfg: &Config) -> Vec<&str> {
    let mut channels = vec![cfg.youtube.channel_id.as_str()];
    let priority = &cfg.priority_channel;
    let id = priority.youtube_channel_id.as_str();
    if priority.enabled && priority.auto_restart && !id.is_empty() && id != channels[0] {
        channels.push(id);
    }
    channels.retain(|id| !id.is_empty());
    channels
}

/// Waits `delay` in 2s steps, ending early when YouTube answers the priority
/// channel live. Wakes are per channel, so the restream target's never land here.
async fn wait_priority(delay: Duration, priority_channel_id: &str) {
    let step = Duration::from_secs(2);
    let mut waited = Duration::ZERO;
    while waited < delay {
        if !priority_channel_id.is_empty()
            && bilistream::plugins::youtube::take_monitor_wake(priority_channel_id)
        {
            tracing::info!("YouTube 显示优先频道已开播，立即检查");
            return;
        }
        let next = step.min(delay - waited);
        tokio::time::sleep(next).await;
        waited += next;
    }
}

/// Each monitor belongs to one FFmpeg session, including its initial delay and
/// in-flight probes. Shutdown/restart aborts it even when the channel is unchanged.
async fn monitor_priority_channel_background(
    current_source: BroadcastSource,
    session: ffmpeg::FfmpegSession,
    context: String,
) {
    tokio::time::sleep(Duration::from_secs(30)).await;
    loop {
        let cfg = match load_config().await.map_err(|error| error.to_string()) {
            Ok(cfg) => cfg,
            Err(error) => {
                tracing::warn!(
                    "{context} 优先频道监控配置加载失败；{}",
                    safe_diagnostic(&error)
                );
                tokio::time::sleep(Duration::from_secs(10)).await;
                continue;
            }
        };
        let delay = Duration::from_secs(cfg.interval.max(1).saturating_mul(2));
        if !priority_monitor_enabled(&cfg, &current_source) {
            wait_priority(delay, &cfg.priority_channel.youtube_channel_id).await;
            continue;
        }
        let warnings = warning_tracker();
        let before_probe = warnings.snapshot();
        if priority_is_held(&cfg, &before_probe) {
            let observations =
                bilistream::plugins::priority::observe_priority_sources(&cfg, &before_probe).await;
            let release = ffmpeg::with_ffmpeg_session(session, || {
                bilistream::config::with_current_config(&cfg, || {
                    warnings.observe_sources(&before_probe, &observations)
                })
            })
            .await
            .flatten()
            .flatten();
            if let Some(release) = release {
                release.log();
            }
        }
        if !should_probe_priority(&cfg, &current_source, &warnings.snapshot()) {
            wait_priority(delay, &cfg.priority_channel.youtube_channel_id).await;
            continue;
        }
        // This probe asks yt-dlp anyway, so a wake from before it adds nothing.
        bilistream::plugins::youtube::take_monitor_wake(&cfg.priority_channel.youtube_channel_id);
        let liveness = resolve_playable_priority_channel(&cfg).await;
        let keywords = bilistream::plugins::banned_keywords::streaming_banned_keywords();
        if let Some(keyword) = liveness.banned_streaming_keyword(&keywords) {
            tracing::warn!(
                "{context} 优先频道包含禁用关键词 {}，跳过自动切换",
                safe_text(&keyword, 32)
            );
            wait_priority(delay, &cfg.priority_channel.youtube_channel_id).await;
            continue;
        }
        let Some(prefetched) = liveness.prefetched_stream(
            &cfg.priority_channel.youtube_channel_id,
            &cfg.priority_channel.twitch_channel_id,
        ) else {
            wait_priority(delay, &cfg.priority_channel.youtube_channel_id).await;
            continue;
        };
        if cfg.enable_anti_collision {
            let aliases = get_aliases(&cfg.priority_channel.channel_name).unwrap_or_default();
            match check_collision(&cfg.priority_channel.channel_name, &aliases)
                .await
                .map_err(|error| error.to_string())
            {
                Ok(None) => {}
                Ok(Some(_)) | Err(_) => {
                    wait_priority(delay, &cfg.priority_channel.youtube_channel_id).await;
                    continue;
                }
            }
        }
        let transition_cfg = cfg.clone();
        let transition_context = context.clone();
        // Ownership and current-config checks cover the park + restart signal.
        // This overlay never writes ordinary channel targets or syncs config.
        if ffmpeg::transition_ffmpeg_session(session, async move {
            bilistream::config::with_current_config(&transition_cfg, || {
                let source = BroadcastSource::new(
                    match prefetched.platform {
                        PriorityChannelPlatform::Youtube => "YT",
                        PriorityChannelPlatform::Twitch => "TW",
                    },
                    &transition_cfg.priority_channel.channel_name,
                    &prefetched.channel_id,
                    prefetched.stream_id.as_deref(),
                );
                if warning_tracker().snapshot().holds(&source) {
                    return false;
                }
                store_prefetched_playable_stream(prefetched, &transition_cfg);
                set_manual_restart();
                bilistream::plugins::set_config_updated();
                tracing::info!(
                    "{transition_context} 优先频道 {} 已确认可播放，准备重新选台",
                    safe_text(&transition_cfg.priority_channel.channel_name, 64)
                );
                true
            })
            .unwrap_or(false)
        })
        .await
        {
            break;
        }
        wait_priority(delay, &cfg.priority_channel.youtube_channel_id).await;
    }
}

fn box_message(
    yt_channel: &str,
    yt_monitor_enabled: bool,
    scheduled_time: Option<DateTime<Local>>,
    title: Option<&str>,
    tw_channel: &str,
    tw_monitor_enabled: bool,
) -> String {
    // Calculate YouTube line
    let yt_line = if !yt_monitor_enabled {
        "YT: 监听已关闭".to_string()
    } else if let Some(scheduled_time) = scheduled_time {
        format!(
            "YT: {} 未直播，计划于 {} 开始，",
            yt_channel,
            scheduled_time.format(MESSAGE_TIME_FORMAT)
        )
    } else {
        format!(
            "YT: {} 未直播                                   ",
            yt_channel
        )
    };

    // Calculate Twitch line
    let tw_line = if !tw_monitor_enabled {
        "TW: 监听已关闭".to_string()
    } else {
        format!("TW: {} 未直播", tw_channel)
    };

    // Calculate width based on the longer line
    let yt_width = yt_line.width() + 2;
    let tw_width = tw_line.width() + 2;
    let width = std::cmp::max(yt_width, tw_width);

    let mut message = format!(
        "\r\x1b[K\x1b[1m┌{:─<width$}┐\n\
         │ {} │\n",
        "",
        yt_line,
        width = width
    );

    // Add padding for YouTube line if needed
    let yt_padding = width - 2 - yt_line.width();
    if yt_padding > 0 {
        // Remove the line we just added and re-add with proper padding
        message = format!(
            "\r\x1b[K\x1b[1m┌{:─<width$}┐\n\
             │ {}{} │\n",
            "",
            yt_line,
            " ".repeat(yt_padding),
            width = width
        );
    }

    if let Some(title_text) = title {
        let wrapped_title = textwrap::fill(title_text, width.saturating_sub(6).max(10));
        for line in wrapped_title.lines() {
            let padding = width.saturating_sub(6).saturating_sub(line.width());
            message.push_str(&format!("│     {}{} │\n", line, " ".repeat(padding)));
        }
    }

    message.push_str(&format!("├{:─<width$}┤\n", "", width = width));

    let tw_padding = width.saturating_sub(2).saturating_sub(tw_line.width());
    message.push_str(&format!(
        "│ {}{} │\n\
         └{:─<width$}┘\x1b[0m\n",
        tw_line,
        " ".repeat(tw_padding),
        "",
        width = width
    ));

    message
}

#[derive(Clone)]
struct LolMonitor {
    cfg: Config,
    puuid: String,
    riot_api_key: String,
}

fn invalid_player_word(ids: &[String], words: &str) -> Option<String> {
    words
        .lines()
        .map(str::trim)
        .filter(|word| !word.is_empty())
        .find(|word| ids.iter().any(|id| id.contains(word)))
        .map(str::to_owned)
}

impl LolMonitor {
    fn new(cfg: &Config, puuid: String) -> Option<Self> {
        if puuid.is_empty() {
            return None;
        }
        let Some(riot_api_key) = normalized_api_key(cfg.riot_api_key.as_deref()) else {
            tracing::warn!("LOL 监控已启用，但 Riot API Key 未配置，跳过本次检测");
            return None;
        };
        Some(Self {
            cfg: cfg.clone(),
            puuid,
            riot_api_key,
        })
    }

    async fn invalid_word(&self) -> Option<String> {
        let ids = match current_game_riot_ids(&self.riot_api_key, &self.puuid)
            .await
            .map_err(|error| error.to_string())
        {
            Ok(Some(ids)) => ids,
            Ok(None) => return None,
            Err(error) => {
                tracing::warn!("LOL 监控查询失败；{}", safe_diagnostic(&error));
                return None;
            }
        };
        let words = crate::storage::read_text("invalid_words.txt").ok()?;
        invalid_player_word(&ids, &words)
    }

    async fn run(self, session: ffmpeg::FfmpegSession, context: String) {
        let interval = Duration::from_secs(self.cfg.lol_monitor_interval.unwrap_or(1).max(1));
        loop {
            tokio::time::sleep(interval).await;
            if let Some(word) = self.invalid_word().await {
                let cfg = self.cfg.clone();
                // The supervisor validates the session and excludes replacement
                // startup until the remote stop has completed. Stale probes have
                // no global flag to mutate and cannot stop a replacement.
                ffmpeg::stop_ffmpeg_session_with(session, async move {
                    tracing::error!(
                        "{context} 玩家ID包含禁用词 {}，停止B站直播",
                        safe_text(&word, 32)
                    );
                    match bili_stop_live(&cfg).await {
                        Ok(()) => {
                            bilistream::notify::live_ended(BroadcastEndCause::ContentRejected)
                        }
                        Err(error) => {
                            tracing::error!(
                                "{context} 停止B站直播失败；{}",
                                safe_diagnostic(&error.to_string())
                            );
                        }
                    }
                    if let Err(error) = send_danmaku(&cfg, "检测到玩家ID存在违词汇，停止直播").await
                    {
                        tracing::error!(
                            "{context} 发送弹幕失败；{}",
                            safe_diagnostic(&error.to_string())
                        );
                    }
                    if cfg.bililive.enable_danmaku_command && !is_danmaku_commands_enabled() {
                        enable_danmaku_commands(true);
                    }
                })
                .await;
                break;
            }
        }
    }
}

async fn update_area(current_area: u64, new_area: u64) -> Result<(), Box<dyn Error>> {
    if current_area != new_area {
        tracing::info!(
            "分区改变（{}->{})",
            area_label(current_area),
            area_label(new_area)
        );
        let cfg = load_config().await?;
        bili_update_area(&cfg, new_area).await?;
    }
    Ok(())
}

fn message_time_regex() -> Option<&'static Regex> {
    static MESSAGE_TIME_RE: OnceLock<Result<Regex, regex::Error>> = OnceLock::new();

    MESSAGE_TIME_RE
        .get_or_init(|| Regex::new(MESSAGE_TIME_PATTERN))
        .as_ref()
        .ok()
}

fn extract_time(message: &str) -> Option<NaiveDateTime> {
    let re = message_time_regex()?;
    re.find(message)
        .and_then(|m| NaiveDateTime::parse_from_str(m.as_str(), MESSAGE_TIME_FORMAT).ok())
}

fn message_without_time(message: &str) -> Cow<'_, str> {
    match message_time_regex() {
        Some(re) => re.replace_all(message, MESSAGE_TIME_PLACEHOLDER),
        None => Cow::Borrowed(message),
    }
}

fn should_update_status_message(last_message: &str, current_message: &str) -> bool {
    if last_message == current_message {
        return false;
    }

    let time_diff = if let Some(last_time) = extract_time(last_message) {
        if let Some(current_time) = extract_time(current_message) {
            (current_time - last_time).num_minutes().abs()
        } else {
            i64::MAX
        }
    } else {
        i64::MAX
    };

    time_diff > MESSAGE_UPDATE_TIME_THRESHOLD_MINUTES
        || message_without_time(last_message) != message_without_time(current_message)
}

fn update_last_status_message(current_message: String) {
    let message_to_print = {
        let mut last = recover_mutex_lock(&LAST_MESSAGE, "last message");
        let should_update = last
            .as_deref()
            .map(|last_msg| should_update_status_message(last_msg, &current_message))
            .unwrap_or(true);

        if should_update {
            *last = Some(current_message.clone().into_boxed_str());
            Some(current_message)
        } else {
            None
        }
    };

    if let Some(message) = message_to_print {
        print!("{}", message);
    }
}

async fn check_collision(
    target_name: &str,
    aliases: &[String],
) -> Result<
    Option<(
        String, // room name
        i32,    // room id
        String, // target channel name
    )>,
    Box<dyn std::error::Error>,
> {
    let cfg = load_config().await?;
    for (room_name, room_id) in cfg.anti_collision_list {
        match get_bili_live_status(room_id).await {
            Ok(status) if status.is_live => {
                let title = &status.title;
                // Check if title contains the target name or aliases
                let contains_target = title.contains(target_name)
                    || aliases.iter().any(|alias| title.contains(alias));

                if contains_target {
                    // Check if this is a multi-channel stream (contains multiple channels)
                    let is_multi_channel = is_multi_channel_stream(title);

                    if is_multi_channel {
                        tracing::debug!(
                            "检测到多频道转播，跳过撞车检测: {} - {}",
                            room_name,
                            title
                        );
                        continue; // Skip collision detection for multi-channel streams
                    }

                    // This appears to be a single-channel stream, flag as collision
                    tracing::debug!(
                        "检测到撞车: {} ({}) 正在转播 {}",
                        room_name,
                        room_id,
                        target_name
                    );
                    return Ok(Some((room_name.clone(), room_id, target_name.to_string())));
                }
            }
            Err(e) => tracing::error!("获取防撞直播间 {} 状态失败: {}", room_id, e),
            _ => (),
        }
    }
    Ok(None)
}

/// Check if a stream title indicates multiple channels (not exclusive to target)
fn is_multi_channel_stream(title: &str) -> bool {
    // Load channels.json and check if multiple channels appear in the title
    if let Ok(is_multi) = has_multiple_channels_in_title(title) {
        return is_multi;
    }

    false
}

/// Check if multiple channel names from channels.json appear in the title
fn has_multiple_channels_in_title(title: &str) -> Result<bool, Box<dyn std::error::Error>> {
    let channels_json: serde_json::Value = crate::storage::read_json("channels.json")?;

    let mut found_channels = 0;
    let title_lower = title.to_lowercase();

    // Check current format: channels[].name and channels[].aliases
    if let Some(channels) = channels_json.get("channels").and_then(|v| v.as_array()) {
        for channel in channels {
            let mut channel_found = false;

            // Check main channel name
            if let Some(name) = channel.get("name").and_then(|v| v.as_str()) {
                if title_lower.contains(&name.to_lowercase()) {
                    channel_found = true;
                }
            }

            // Check aliases if main name not found
            if !channel_found {
                if let Some(aliases) = channel.get("aliases").and_then(|v| v.as_array()) {
                    for alias in aliases {
                        if let Some(alias_str) = alias.as_str() {
                            if title_lower.contains(&alias_str.to_lowercase()) {
                                channel_found = true;
                                break;
                            }
                        }
                    }
                }
            }

            if channel_found {
                found_channels += 1;
                // Early return: if we found 2+ channels, it's definitely multi-channel
                if found_channels >= 2 {
                    return Ok(true);
                }
            }
        }
    }

    // Legacy support: Check old format YT_channels[].channel_name
    if let Some(yt_channels) = channels_json.get("YT_channels").and_then(|v| v.as_array()) {
        for channel in yt_channels {
            if let Some(name) = channel.get("channel_name").and_then(|v| v.as_str()) {
                if title_lower.contains(&name.to_lowercase()) {
                    found_channels += 1;
                    // Early return for legacy format too
                    if found_channels >= 2 {
                        return Ok(true);
                    }
                }
            }
        }
    }

    // Legacy support: Check old format TW_channels[].channel_name
    if let Some(tw_channels) = channels_json.get("TW_channels").and_then(|v| v.as_array()) {
        for channel in tw_channels {
            if let Some(name) = channel.get("channel_name").and_then(|v| v.as_str()) {
                if title_lower.contains(&name.to_lowercase()) {
                    found_channels += 1;
                    // Early return for legacy format too
                    if found_channels >= 2 {
                        return Ok(true);
                    }
                }
            }
        }
    }

    // Return false if we found 0 or 1 channels
    Ok(false)
}

async fn handle_collisions(
    yt_is_live: &mut bool,
    tw_is_live: &mut bool,
) -> Result<CollisionResult, Box<dyn Error>> {
    let cfg = load_config().await?;

    let mut yt_collision = None;
    let mut tw_collision = None;

    // YouTube collision check
    if *yt_is_live {
        let target_name = &cfg.youtube.channel_name;
        let aliases = get_aliases(target_name)?;
        yt_collision = check_collision(target_name, &aliases).await?;
    }

    // Twitch collision check
    if *tw_is_live {
        let target_name = &cfg.twitch.channel_name;
        let aliases = get_aliases(target_name)?;
        tw_collision = check_collision(target_name, &aliases).await?;
    }

    match (yt_collision, tw_collision) {
        (Some(yt_collision), Some(tw_collision)) => {
            let (yt_room_name, yt_room_id, yt_target_name) = yt_collision;
            let (tw_room_name, tw_room_id, tw_target_name) = tw_collision;

            // Check if we're already in a dual-platform collision state (regardless of specific room)
            let already_in_dual_collision = {
                let last_collision = recover_mutex_lock(&LAST_COLLISION, "last collision");
                last_collision
                    .as_ref()
                    .map(|platform| platform.as_ref() == DUAL_COLLISION_PLATFORM)
                    .unwrap_or(false)
            };

            if !already_in_dual_collision {
                {
                    let mut last_collision = recover_mutex_lock(&LAST_COLLISION, "last collision");
                    *last_collision = Some(DUAL_COLLISION_PLATFORM.into());
                }

                tracing::warn!("YouTube和Twitch均检测到撞车，跳过本次转播");
                // send_danmaku(&cfg, "YT和TW双平台撞车").await?;
                // tokio::time::sleep(Duration::from_secs(2)).await;
                if let Err(e) = send_danmaku(
                    &cfg,
                    &format!("{}({})正在转{}", yt_room_name, yt_room_id, yt_target_name),
                )
                .await
                {
                    tracing::error!("Failed to send danmaku: {}", e);
                }
                if yt_room_name != tw_room_name {
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    if let Err(e) = send_danmaku(
                        &cfg,
                        &format!("{}({})正在转{}", tw_room_name, tw_room_id, tw_target_name),
                    )
                    .await
                    {
                        tracing::error!("Failed to send danmaku: {}", e);
                    }
                }
                tokio::time::sleep(Duration::from_secs(2)).await;
                if cfg.bililive.enable_danmaku_command && !is_danmaku_commands_enabled() {
                    enable_danmaku_commands(true);
                }
                if cfg.bililive.enable_danmaku_command {
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    if let Err(e) = send_danmaku(&cfg, "撞车：可使用弹幕指令进行换台").await
                    {
                        tracing::error!("Failed to send danmaku: {}", e);
                    }
                }
                tokio::time::sleep(Duration::from_secs(30)).await;
            }
            Ok(CollisionResult::Continue)
        }
        (Some(collision), None) | (None, Some(collision)) => {
            let (room_name, room_id, target_name) = collision;
            let is_youtube_collision = target_name == cfg.youtube.channel_name;
            let other_live = if is_youtube_collision {
                let ol = *tw_is_live;
                *yt_is_live = false;
                ol
            } else {
                let ol = *yt_is_live;
                *tw_is_live = false;
                ol
            };

            // Check if we're already in a collision state for this platform
            let already_in_collision = {
                let last_collision = recover_mutex_lock(&LAST_COLLISION, "last collision");
                last_collision
                    .as_ref()
                    .map(|platform| platform.as_ref() == target_name.as_str())
                    .unwrap_or(false)
            };

            if !other_live && !already_in_collision {
                {
                    let mut last_collision = recover_mutex_lock(&LAST_COLLISION, "last collision");
                    *last_collision = Some(target_name.clone().into_boxed_str());
                }

                tracing::warn!(
                    "{}（{}）撞车，{}（{}）未开播",
                    room_name,
                    room_id,
                    if is_youtube_collision {
                        "Twitch"
                    } else {
                        "YouTube"
                    },
                    if is_youtube_collision {
                        cfg.twitch.channel_name.clone()
                    } else {
                        cfg.youtube.channel_name.clone()
                    }
                );
                if let Err(e) = send_danmaku(
                    &cfg,
                    &format!("{}({})正在转{}", room_name, room_id, target_name,),
                )
                .await
                {
                    tracing::error!("Failed to send danmaku: {}", e);
                }
                tokio::time::sleep(Duration::from_secs(2)).await;
                if cfg.bililive.enable_danmaku_command && !is_danmaku_commands_enabled() {
                    enable_danmaku_commands(true);
                }
                if cfg.bililive.enable_danmaku_command {
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    if let Err(e) = send_danmaku(&cfg, "撞车：可使用弹幕指令进行换台").await
                    {
                        tracing::error!("Failed to send danmaku: {}", e);
                    }
                }
                tokio::time::sleep(Duration::from_secs(30)).await;
                Ok(CollisionResult::Continue)
            } else {
                Ok(CollisionResult::Proceed)
            }
        }
        (None, None) => Ok(CollisionResult::Proceed),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bilistream::config::{BiliLive, Credentials, Twitch, Youtube};

    #[derive(Clone, Default)]
    struct LogBytes(std::sync::Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for LogBytes {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn history_end_reason_uses_only_the_seven_codes() {
        use crate::history::EndReason;
        let cases = [
            (
                FfmpegLoopExitReason::SourceEnded,
                false,
                EndReason::SourceEnded,
            ),
            (
                FfmpegLoopExitReason::NewBroadcast,
                false,
                EndReason::SourceEnded,
            ),
            (
                FfmpegLoopExitReason::BiliStopped,
                false,
                EndReason::BiliStopped,
            ),
            (FfmpegLoopExitReason::BiliStopped, true, EndReason::UserStop),
            (
                FfmpegLoopExitReason::IntentionalRestart {
                    target_m3u8_available: true,
                },
                false,
                EndReason::IntentionalRestart,
            ),
            (
                FfmpegLoopExitReason::EndCheckCancelled { restart: true },
                false,
                EndReason::IntentionalRestart,
            ),
            (
                FfmpegLoopExitReason::EndCheckCancelled { restart: false },
                true,
                EndReason::UserStop,
            ),
            (
                FfmpegLoopExitReason::ClusterHandoff,
                false,
                EndReason::ClusterHandoff,
            ),
        ];
        for (reason, manual_stop, want) in cases {
            assert_eq!(history_end_reason(reason, manual_stop), want, "{reason:?}");
        }
    }

    #[test]
    fn runtime_milestones_are_single_line_safe_and_distinguish_end_causes() {
        let bytes = LogBytes::default();
        let writer = bytes.clone();
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_target(false)
            .with_writer(move || writer.clone())
            .finish();
        let identity =
            FfmpegStreamIdentity::new("jp", "YT", &"示例频道001".repeat(8), "example-channel-001");
        tracing::subscriber::with_default(subscriber, || {
            log_selected_stream(&identity, Some(&"很长的示例直播标题".repeat(20)), false);
            log_selected_stream(
                &identity,
                Some("示例标题\n2026-10-10 ERROR 注入\u{1b}[31m"),
                true,
            );
            log_selected_stream(&identity, Some("示例 token=SYNTHETIC_TITLE_TOKEN"), false);
            let context = identity.session_label(None);
            for reason in [
                FfmpegLoopExitReason::SourceEnded,
                FfmpegLoopExitReason::BiliStopped,
                FfmpegLoopExitReason::IntentionalRestart {
                    target_m3u8_available: true,
                },
                FfmpegLoopExitReason::ClusterHandoff,
            ] {
                log_ffmpeg_loop_exit(&context, reason, None);
            }
            log_ffmpeg_loop_exit(
                &context,
                FfmpegLoopExitReason::IntentionalRestart {
                    target_m3u8_available: true,
                },
                Some("集群节点交接尚未确认，暂停本节点执行"),
            );
        });
        let output = String::from_utf8(bytes.0.lock().unwrap().clone()).unwrap();
        let lines: Vec<_> = output.lines().collect();
        assert_eq!(lines.len(), 8, "{output}");
        assert!(lines
            .iter()
            .all(|line| line.trim_start().starts_with("INFO")));
        assert!(lines[0].contains('…'));
        assert!(lines[1].contains("\\n2026-10-10 ERROR"));
        assert!(!output.contains('\u{1b}'));
        assert!(!output.contains("SYNTHETIC_TITLE_TOKEN"));
        assert!(lines[3].contains("源直播已结束"));
        assert!(lines[4].contains("B站直播已停止"));
        assert!(lines[5].contains("重启或切换信号"));
        assert!(lines[6].contains("集群执行权限已撤销"));
        assert!(!lines[6].contains("主动重启"));
        assert!(lines[7].contains("当前执行受阻：集群节点交接尚未确认"));
        assert!(!lines[7].contains("主动重启"));
        assert!(output.contains("未建立会话"));
        assert!(!output.contains("None"));
        assert!(!output.contains("已观察到推流"));
    }

    #[test]
    fn invalid_player_words_ignore_blank_lines_and_match_ids_only() {
        let ids = vec!["Player#JP1".to_string()];
        assert_eq!(invalid_player_word(&ids, "\n  \nblocked\n"), None);
        assert_eq!(
            invalid_player_word(&ids, "\n  Player  \n"),
            Some("Player".to_string())
        );
        assert_eq!(invalid_player_word(&ids, "[\n\""), None);
    }

    fn test_stream_candidate(platform: StreamPlatform, is_live: bool) -> StreamCandidate {
        StreamCandidate {
            platform,
            is_live,
            topic: None,
            title: Some("title".to_string()),
            m3u8_url: Some("https://example.com/live.m3u8".to_string()),
            piped_ingest: None,
            stream_id: Some("stream-id".to_string()),
            channel_name: platform.code().to_string(),
            channel_id: "channel-id".to_string(),
            area_v2: 235,
            is_priority: false,
        }
    }

    #[test]
    fn probe_plan_deduplicates_ids_in_each_platform_and_ignores_empty_ids() {
        let mut cfg = test_config();
        cfg.priority_channel.channel_name = "same display name".into();
        cfg.youtube.channel_name = cfg.priority_channel.channel_name.clone();
        cfg.twitch.channel_name = cfg.priority_channel.channel_name.clone();
        cfg.priority_channel.youtube_channel_id = cfg.youtube.channel_id.clone();
        cfg.priority_channel.twitch_channel_id = cfg.twitch.channel_id.to_uppercase();
        let same = SourceProbePlan::for_config(&cfg);
        assert_eq!(same.channels.len(), 2);
        assert_eq!(same.youtube, same.priority_youtube);
        assert_eq!(same.twitch, same.priority_twitch);
        cfg.priority_channel.youtube_channel_id = "other-youtube-id".into();
        cfg.priority_channel.twitch_channel_id = "other_twitch_login".into();
        let distinct = SourceProbePlan::for_config(&cfg);
        assert_eq!(
            distinct.channels.len(),
            4,
            "same display names do not collapse distinct IDs"
        );
        assert_ne!(distinct.youtube, distinct.priority_youtube);
        assert_ne!(distinct.twitch, distinct.priority_twitch);
        cfg.priority_channel.youtube_channel_id.clear();
        cfg.priority_channel.twitch_channel_id.clear();
        assert_eq!(SourceProbePlan::for_config(&cfg).channels.len(), 2);
        cfg.youtube.enable_monitor = false;
        cfg.priority_channel.youtube_channel_id = cfg.youtube.channel_id.clone();
        let priority_only = SourceProbePlan::for_config(&cfg);
        assert_eq!(priority_only.youtube, None);
        assert!(priority_only.priority_youtube.is_some());
        assert_eq!(priority_only.channels.len(), 2);
    }

    #[test]
    fn priority_area_and_shared_client_follow_channel_ids_without_mutating_config() {
        let mut cfg = test_config();
        cfg.youtube.area_v2 = 86;
        cfg.priority_channel.default_area = 329;
        cfg.priority_channel.channel_name = "另一个显示名".into();
        cfg.priority_channel.youtube_channel_id = cfg.youtube.channel_id.clone();
        let before = serde_json::to_value(&cfg).unwrap();
        let version = cluster::monitored_config_version(&cfg);
        assert_eq!(priority_area(StreamPlatform::Youtube, &cfg), 86);
        let ordinary = test_stream_candidate(StreamPlatform::Youtube, true);
        let mut priority = ordinary.clone();
        priority.channel_id = cfg.youtube.channel_id.clone();
        priority.area_v2 = priority_area(StreamPlatform::Youtube, &cfg);
        let selected = overlay_priority(ordinary, priority);
        let client = YoutubeClient::new(&cfg.youtube.channel_name, &cfg.youtube.channel_id, None);
        let probes = vec![ProbedSource {
            channel: ChannelProbe {
                platform: StreamPlatform::Youtube,
                channel_name: cfg.youtube.channel_name.clone(),
                channel_id: cfg.youtube.channel_id.clone(),
            },
            client: Some(ProbedClient::Youtube(client)),
            status: Ok(SourceStatus::OFFLINE),
        }];
        let Some(SourceClient::Youtube(selected_client)) =
            selected_source_client(&selected, &probes, &None)
        else {
            panic!("shared ID must use the one checked client")
        };
        let Some(ProbedClient::Youtube(shared_client)) = probes[0].client.as_ref() else {
            unreachable!()
        };
        assert!(std::ptr::eq(selected_client, shared_client));
        assert_eq!(selected.area_v2, 86);
        assert_eq!(serde_json::to_value(&cfg).unwrap(), before);
        assert_eq!(cluster::monitored_config_version(&cfg), version);
        cfg.priority_channel.youtube_channel_id = "distinct".into();
        assert_eq!(priority_area(StreamPlatform::Youtube, &cfg), 329);
    }

    #[test]
    fn held_or_url_less_priority_keeps_same_platform_and_niconico_fallbacks() {
        use bilistream::plugins::danmaku::warning::WarningTracker;
        let tracker = std::sync::Arc::new(WarningTracker::default());
        let mut ordinary = test_stream_candidate(StreamPlatform::Youtube, true);
        ordinary.channel_id = "ordinary".into();
        let mut priority = test_stream_candidate(StreamPlatform::Youtube, true);
        priority.channel_id = "priority".into();
        priority.channel_name = "示例优先频道 001".into();
        priority.is_priority = true;
        let _owner = tracker.own_source(1, priority.source_identity());
        tracker
            .mark_event(&mut tracker.capture_event(), None)
            .unwrap();
        assert!(exclude_warned_stream(&mut priority, &tracker.snapshot()));
        let chosen = overlay_priority(ordinary.clone(), priority.clone());
        assert_eq!(chosen.channel_id, "ordinary");
        assert!(!chosen.is_priority);
        assert!(tracker.snapshot().has_hold());
        priority.is_live = true;
        priority.m3u8_url = None;
        let chosen = overlay_priority(ordinary, priority);
        assert_eq!(chosen.channel_id, "ordinary");
        assert!(!chosen.is_priority);
        let tw = test_stream_candidate(StreamPlatform::Twitch, true);
        let nico = test_stream_candidate(StreamPlatform::Niconico, true);
        assert_eq!(
            select_stream(&chosen, &tw, &nico).unwrap().platform,
            StreamPlatform::Niconico
        );
    }

    #[test]
    fn scheduled_and_id_only_upcoming_results_cannot_clear_the_live_episode_hold() {
        use bilistream::plugins::danmaku::warning::{SourcePresence, WarningTracker};
        let tracker = std::sync::Arc::new(WarningTracker::default());
        let source = BroadcastSource::new("YT", "示例频道 001", "channel-001", Some("live-1"));
        let _owner = tracker.own_source(1, source.clone());
        tracker
            .mark_event(&mut tracker.capture_event(), None)
            .unwrap();
        let before = tracker.snapshot();
        for start in [Some(Local::now() + chrono::Duration::hours(1)), None] {
            let status = SourceStatus {
                is_live: false,
                topic: None,
                title: Some("下一场预告".into()),
                m3u8_url: None,
                start_time: start,
                video_id: Some("upcoming-2".into()),
            };
            let observation = source_observation(
                StreamPlatform::Youtube,
                &source.channel_name,
                &source.channel_id,
                &status,
            );
            assert_eq!(observation.presence, SourcePresence::Inconclusive);
            assert!(tracker.observe_sources(&before, &[observation]).is_none());
            assert!(tracker.snapshot().holds(&source));
        }
        let offline = source_observation(
            StreamPlatform::Youtube,
            &source.channel_name,
            &source.channel_id,
            &SourceStatus::OFFLINE,
        );
        assert_eq!(offline.presence, SourcePresence::Offline);
        assert!(tracker.observe_sources(&before, &[offline]).is_some());
    }

    #[test]
    fn shared_youtube_and_twitch_ids_each_emit_one_banned_notice_across_passes() {
        let mut notices = VecDeque::new();
        let keywords = vec!["blocked".to_owned()];
        let mut yt = test_stream_candidate(StreamPlatform::Youtube, true);
        yt.title = Some("blocked title".into());
        let mut tw = test_stream_candidate(StreamPlatform::Twitch, true);
        tw.title = Some("blocked title".into());
        for expected in [2, 0] {
            let mut emitted = 0;
            for mut candidate in [yt.clone(), tw.clone(), yt.clone(), tw.clone()] {
                let keyword = exclude_banned_stream(&mut candidate, &keywords).unwrap();
                assert!(!candidate.is_live);
                emitted += usize::from(remember_banned_notice(
                    &mut notices,
                    &candidate,
                    &keyword,
                    candidate.title.as_deref().unwrap(),
                ));
            }
            assert_eq!(emitted, expected);
            assert_eq!(notices.len(), 2);
        }
    }

    #[test]
    fn cut_then_danmaku_twitch_switch_holds_youtube_until_real_push_output() {
        use bilistream::plugins::danmaku::{
            warning::WarningTracker, warning_test_support as command,
        };
        use bilistream::plugins::danmaku_client::{capture_warning_message, DanmakuMessage};
        use bilistream::plugins::ffmpeg::warning_test_support::{run, ReaderFixture};
        use tracing::instrument::WithSubscriber;

        run(async {
            let _commands = command::SERIAL.lock().await;
            // Register after the shared producer/test locks; unobserved callers
            // may already have reached these tracing callsites in this suite.
            let (dispatch, logs) =
                bilistream::logging::test_capture::subscriber(tracing::Level::INFO);
            async {
                command::reset_trace();
                let mut cfg = test_config();
                cfg.cluster.node_id = "ca".into();
                cfg.youtube.channel_name = "示例频道 001".into();
                cfg.youtube.channel_id = "UCexample001".into();
                cfg.youtube.area_v2 = 329;
                cfg.priority_channel.channel_name = cfg.youtube.channel_name.clone();
                cfg.priority_channel.youtube_channel_id = cfg.youtube.channel_id.clone();
                cfg.priority_channel.auto_restart = true;
                let warnings = std::sync::Arc::new(WarningTracker::default());
                let old_source = BroadcastSource::new("YT", &cfg.youtube.channel_name, &cfg.youtube.channel_id, Some("yt-episode-1"));
                let old_owner = warnings.own_source(100, old_source.clone());
                capture_warning_message(&warnings, &DanmakuMessage {
                    cmd: "CUT_OFF".into(), info: None,
                    data: Some(serde_json::json!({"msg": "Stream cut off"})),
                }).unwrap();
                assert!(warnings.snapshot().holds(&old_source));
                drop(old_owner);

                assert_eq!(command::accept_target(&mut cfg, warnings.clone(), "TW", "示例频道 002", "example002", 86).await, 1);
                assert!(warnings.snapshot().holds(&old_source), "saving another target cannot release the old hold");
                clear_config_updated(); // the next pass loads the saved target
                let saved_targets = serde_json::to_value(&cfg).unwrap();
                let version = cluster::monitored_config_version(&cfg);
                let a_status = SourceStatus { is_live: true, topic: None, title: Some("示例直播 001".into()), m3u8_url: Some("https://example.invalid/a.m3u8".into()), start_time: None, video_id: Some("yt-episode-1".into()) };
                let b_status = SourceStatus { is_live: true, topic: Some("League of Legends".into()), title: Some("示例直播 002".into()), m3u8_url: Some("https://example.invalid/b.m3u8".into()), start_time: None, video_id: Some("tw-episode-2".into()) };
                let plan = SourceProbePlan::for_config(&cfg);
                assert_eq!(plan.channels.len(), 2);
                assert_eq!(plan.youtube, plan.priority_youtube);
                let probes: Vec<_> = plan.channels.iter().map(|channel| ProbedSource {
                    channel: channel.clone(), client: None,
                    status: Ok(if channel.platform == StreamPlatform::Youtube { a_status.clone() } else { b_status.clone() }),
                }).collect();
                let mut yt = StreamCandidate::from_status(StreamPlatform::Youtube, &cfg.youtube.channel_name, &cfg.youtube.channel_id, cfg.youtube.area_v2, a_status.clone());
                let mut priority_yt = StreamCandidate::from_status(StreamPlatform::Youtube, &cfg.priority_channel.channel_name, &cfg.priority_channel.youtube_channel_id, priority_area(StreamPlatform::Youtube, &cfg), a_status);
                let tw = StreamCandidate::from_status(StreamPlatform::Twitch, &cfg.twitch.channel_name, &cfg.twitch.channel_id, cfg.twitch.area_v2, b_status);
                let nico = test_stream_candidate(StreamPlatform::Niconico, false);
                skip_stream_if_previously_warned(&mut yt, &cfg, &warnings).await;
                skip_stream_if_previously_warned(&mut priority_yt, &cfg, &warnings).await;
                let yt = overlay_priority(yt, priority_yt);
                let selected = select_stream(&yt, &tw, &nico).unwrap();
                assert_eq!(selected.platform, StreamPlatform::Twitch);
                assert_eq!(selected.channel_name, "示例频道 002");
                assert_eq!(selected.channel_id, "example002");
                assert_eq!(selected.area_v2, 86);
                assert!(warnings.snapshot().holds(&old_source), "selection cannot release");
                report_danmaku_selection(&cfg, &probes, &yt, &tw, &nico, &[], &warnings.snapshot());
                log_selected_stream(&FfmpegStreamIdentity::new("ca", "TW", &selected.channel_name, &selected.channel_id), selected.title.as_deref(), selected.is_priority);

                let attempt = ReaderFixture::new(101, selected.source_identity(), warnings.clone()).await;
                attempt.started(false);
                assert!(warnings.snapshot().holds(&old_source), "spawning cache cannot release");
                attempt.feed(true, b"Connection refused\n").await;
                attempt.finish().await;
                assert!(warnings.snapshot().holds(&old_source), "failed startup cannot release");

                let pushing = ReaderFixture::new(102, selected.source_identity(), warnings.clone()).await;
                pushing.started(false);
                pushing.feed(false, b"frame=60 time=00:00:01.00 bitrate=9000kbits/s speed=1.5x\n").await;
                pushing.started(true);
                pushing.feed(true, b"frame=0 time=00:00:00.00 bitrate=0kbits/s speed=0x\n").await;
                assert!(warnings.snapshot().holds(&old_source), "cache output and zero push samples do not prove pushing");
                pushing.feed(true, b"frame=120 time=00:00:02.00 bitrate=6000kbits/s speed=1x\nframe=180 time=00:00:03.00 bitrate=6000kbits/s speed=1x\n").await;
                assert!(!warnings.snapshot().has_hold());
                assert!(should_probe_priority(&cfg, &selected.source_identity(), &warnings.snapshot()), "the chosen tradeoff allows priority to return after other-channel output");
                assert_eq!(serde_json::to_value(&cfg).unwrap(), saved_targets);
                assert_eq!(cluster::monitored_config_version(&cfg), version);
                pushing.finish().await;
                command::reset_trace();
            }.with_subscriber(dispatch).await;
            let output = logs.text();
            assert_eq!(output.matches("临时跳过已解除").count(), 1, "{output}");
            assert_eq!(output.matches("已观察到推流输出").count(), 1, "{output}");
            let ordered = [
                "Cut off/Warning",
                "已标记频道",
                "弹幕 %转播%",
                "监听目标已更新",
                "跳过频道",
                "选台结果=TW/示例频道 002",
                "源直播可用",
                "Connection refused",
                "已观察到推流输出",
                "另一频道已确认推流",
            ];
            let mut cursor = 0;
            for text in ordered {
                cursor += output[cursor..]
                    .find(text)
                    .unwrap_or_else(|| panic!("missing/out-of-order {text}: {output}"))
                    + text.len();
            }
            assert!(!output.contains("https://"));
            println!("{output}");
        });
    }

    #[test]
    fn the_idle_wait_wakes_on_the_priority_channel_only_while_switching_is_on() {
        let mut cfg = test_config();
        cfg.youtube.channel_id = "UCtarget".to_string();
        cfg.priority_channel.youtube_channel_id = "UCpriority".to_string();
        cfg.priority_channel.enabled = true;
        cfg.priority_channel.auto_restart = false;
        assert_eq!(idle_wake_channels(&cfg), vec!["UCtarget"]);
        cfg.priority_channel.auto_restart = true;
        assert_eq!(idle_wake_channels(&cfg), vec!["UCtarget", "UCpriority"]);
        cfg.youtube.channel_id = "UCpriority".to_string();
        assert_eq!(
            idle_wake_channels(&cfg),
            vec!["UCpriority"],
            "priority is the target"
        );
        cfg.youtube.channel_id.clear();
        assert_eq!(idle_wake_channels(&cfg), vec!["UCpriority"]);
    }

    fn test_config() -> Config {
        Config {
            snapshot: None,
            auto_cover: false,
            enable_anti_collision: false,
            interval: 60,
            bililive: BiliLive {
                enable_danmaku_command: false,
                room: 1,
                bili_rtmp_url: String::new(),
                bili_rtmp_key: String::new(),
                credentials: Credentials::default(),
            },
            twitch: Twitch {
                enable_monitor: true,
                channel_name: "tw-channel".to_string(),
                area_v2: 235,
                channel_id: "twid".to_string(),
                proxy_region: String::new(),
                quality: "best".to_string(),
                proxy: None,
                crop: None,
                ffmpeg_cache: bilistream::config::FfmpegCache::default(),
            },
            youtube: Youtube {
                enable_monitor: true,
                channel_name: "yt-channel".to_string(),
                channel_id: "ytid".to_string(),
                area_v2: 235,
                quality: "best".to_string(),
                cookies_file: None,
                cookies_from_browser: None,
                proxy: None,
                deno_path: None,
                crop: None,
                ffmpeg_cache: bilistream::config::FfmpegCache::default(),
            },
            holodex_api_key: None,
            holodex_jwt: None,
            holodex_jwt_refreshed_at: None,
            holodex_username: None,
            holodex_skip_jwt_verify: false,
            holodex_monitor_gate: true,
            youtube_api_key: None,
            youtube_rss_enabled: true,
            show_priority_channel: false,
            show_twitch: true,
            show_niconico: false,
            youtube_websub_callback_url: None,
            youtube_websub_port: crate::config::default_websub_port(),
            riot_api_key: None,
            enable_lol_monitor: false,
            lol_monitor_interval: None,
            anti_collision_list: std::collections::HashMap::new(),
            priority_channel: bilistream::config::PriorityChannel {
                enabled: true,
                ..Default::default()
            },
            enable_youtube_monitor: false,
            enable_twitch_monitor: true,
            niconico: bilistream::config::Niconico::default(),
            cluster: bilistream::config::ClusterConfig::default(),
            history: Default::default(),
        }
    }

    #[test]
    fn a_skipped_youtube_stream_is_marked_and_refetches_once() {
        let mut stream = test_stream_candidate(StreamPlatform::Youtube, true);
        stream.channel_id = "UCskip-runtime".to_string();
        stream.stream_id = Some("vid-skip".to_string());

        mark_youtube_skipped_live(&stream, true);
        assert!(
            !refetch_skipped_youtube(&stream),
            "still live with an m3u8 is not a skip"
        );

        stream.is_live = false;
        mark_youtube_skipped_live(&stream, true);

        stream.is_live = true;
        stream.m3u8_url = None;
        assert!(refetch_skipped_youtube(&stream));
        assert!(
            !refetch_skipped_youtube(&stream),
            "clearing the mark must not loop"
        );
    }

    #[test]
    fn select_stream_prefers_niconico_then_youtube_then_twitch() {
        let yt = test_stream_candidate(StreamPlatform::Youtube, true);
        let tw = test_stream_candidate(StreamPlatform::Twitch, true);
        let mut nico = test_stream_candidate(StreamPlatform::Niconico, true);

        let selected = select_stream(&yt, &tw, &nico).expect("live stream should be selected");
        assert_eq!(selected.platform, StreamPlatform::Niconico);

        nico.is_live = false;
        let selected = select_stream(&yt, &tw, &nico).expect("live stream should be selected");
        assert_eq!(selected.platform, StreamPlatform::Youtube);
    }

    #[test]
    fn a_live_priority_channel_still_beats_niconico() {
        let yt = test_stream_candidate(StreamPlatform::Youtube, false);
        let mut tw = test_stream_candidate(StreamPlatform::Twitch, true);
        tw.is_priority = true;
        let nico = test_stream_candidate(StreamPlatform::Niconico, true);

        let selected = select_stream(&yt, &tw, &nico).expect("priority should be selected");
        assert_eq!(selected.platform, StreamPlatform::Twitch);
    }

    #[test]
    fn temp_ban_monitor_shutdown_clears_mirrored_toggle_flags() {
        let mut cfg = test_config();
        cfg.niconico.enable_monitor = true;

        assert!(disable_monitors_after_bili_start_temp_ban(&mut cfg));

        assert!(!cfg.youtube.enable_monitor);
        assert!(!cfg.enable_youtube_monitor);
        assert!(!cfg.twitch.enable_monitor);
        assert!(!cfg.enable_twitch_monitor);
        assert!(!cfg.niconico.enable_monitor);
        assert!(!cfg.priority_channel.enabled);
        assert!(!disable_monitors_after_bili_start_temp_ban(&mut cfg));
    }

    #[test]
    fn select_stream_returns_none_when_no_live_candidate() {
        let yt = test_stream_candidate(StreamPlatform::Youtube, false);
        let tw = test_stream_candidate(StreamPlatform::Twitch, false);
        let nico = test_stream_candidate(StreamPlatform::Niconico, false);

        assert!(select_stream(&yt, &tw, &nico).is_none());
    }

    #[test]
    fn select_stream_skips_live_candidate_without_stream_url() {
        let mut yt = test_stream_candidate(StreamPlatform::Youtube, true);
        yt.m3u8_url = None;
        let tw = test_stream_candidate(StreamPlatform::Twitch, true);
        let nico = test_stream_candidate(StreamPlatform::Niconico, false);

        let selected =
            select_stream(&yt, &tw, &nico).expect("playable fallback should be selected");

        assert_eq!(selected.platform, StreamPlatform::Twitch);
    }

    #[test]
    fn select_stream_returns_none_when_live_candidates_have_no_stream_url() {
        let mut yt = test_stream_candidate(StreamPlatform::Youtube, true);
        yt.m3u8_url = None;
        let mut tw = test_stream_candidate(StreamPlatform::Twitch, true);
        tw.m3u8_url = None;
        let mut nico = test_stream_candidate(StreamPlatform::Niconico, true);
        nico.m3u8_url = None;

        assert!(select_stream(&yt, &tw, &nico).is_none());
    }

    #[test]
    fn select_stream_prefers_niconico_when_others_unplayable() {
        let mut yt = test_stream_candidate(StreamPlatform::Youtube, true);
        yt.m3u8_url = None;
        let mut tw = test_stream_candidate(StreamPlatform::Twitch, false);
        tw.m3u8_url = None;
        let mut nico = test_stream_candidate(StreamPlatform::Niconico, true);
        nico.m3u8_url = None;
        nico.piped_ingest = Some(PipedIngest {
            program: "streamlink".to_string(),
            args: vec!["--stdout".to_string()],
        });

        let selected = select_stream(&yt, &tw, &nico).expect("niconico should be selected");
        assert_eq!(selected.platform, StreamPlatform::Niconico);
        assert!(selected.ffmpeg_source().is_some());
    }

    const URL: &str = "https://example.invalid/new.m3u8";

    fn t0() -> std::time::Instant {
        std::time::Instant::now()
    }

    fn calm() -> EndSignals {
        end_signals(7, 7, false, true)
    }

    fn live(stream_id: Option<&str>) -> RefetchReading {
        RefetchReading::Live {
            m3u8_url: Some(URL.to_string()),
            stream_id: stream_id.map(str::to_string),
        }
    }

    fn session(stream_id: Option<&str>) -> StreamEndState {
        let mut state = StreamEndState::default();
        state.start_session("YT:chan".to_string(), stream_id);
        state
    }

    /// Required (1): an m3u8 miss on the active node ends the stream and
    /// announces straight away, with no further readings or waits.
    #[test]
    fn m3u8_miss_sends_straight_away() {
        let mut state = session(Some("v1"));
        let step = state.on_refetch(RefetchReading::Offline, calm(), t0());
        assert_eq!(
            step,
            RefetchStep::Ended {
                announce: true,
                timed_out: false
            }
        );
        assert!(state.ended);
        assert_eq!(
            end_action(FfmpegLoopExitReason::SourceEnded, false, false, false, true),
            EndAction::AnnounceEnd { bili_live: true }
        );
        // A status reply that is "not live" maps to the miss.
        assert_eq!(
            RefetchReading::from_result::<()>(Ok(SourceStatus::OFFLINE)),
            RefetchReading::Offline
        );
    }

    /// Required (2): a manual stop or restart at that moment sends nothing and
    /// leaves the gate shut. Uses the real capture point and generation (N3).
    #[test]
    fn manual_stop_or_restart_at_the_miss_sends_nothing() {
        let session_generation = capture_session_generation();
        set_manual_restart();
        let signals = current_end_signals(session_generation, true);
        clear_manual_restart();
        assert!(signals.stop_or_restart_requested);

        for (signals, expected) in [
            (end_signals(7, 8, false, true), EndRecheckCancel::ManualStop),
            (
                end_signals(7, 9, true, true),
                EndRecheckCancel::ManualRestart,
            ),
        ] {
            let mut state = session(Some("v1"));
            assert_eq!(
                state.on_refetch(RefetchReading::Offline, signals, t0()),
                RefetchStep::Cancelled(expected)
            );
            assert!(!state.ended);
            assert!(!state.commands_gate(true, true));
        }
        for restart in [false, true] {
            assert_eq!(
                end_action(
                    FfmpegLoopExitReason::EndCheckCancelled { restart },
                    false,
                    false,
                    false,
                    true
                ),
                EndAction::SkipEndCheckCancelled
            );
        }
    }

    /// Required (3) / scope 5: a standby or mid-takeover node never sends and
    /// never opens the gate.
    #[test]
    fn standby_or_mid_takeover_node_never_sends_or_opens_gate() {
        let mut state = session(Some("v1"));
        let standby = end_signals(7, 7, false, false);
        assert_eq!(
            state.on_refetch(RefetchReading::Offline, standby, t0()),
            RefetchStep::Cancelled(EndRecheckCancel::ClusterHandoff)
        );
        assert!(!state.ended);
        assert!(!state.commands_gate(true, false));
        assert_eq!(
            end_action(
                FfmpegLoopExitReason::ClusterHandoff,
                false,
                false,
                false,
                true
            ),
            EndAction::SkipHandoff
        );
        // Lost the active role after the miss: the gate stays shut.
        let mut state = session(Some("v1"));
        state.on_refetch(RefetchReading::Offline, calm(), t0());
        assert!(!state.commands_gate(true, false));
    }

    /// Scope 1: a 换台 command before the miss is refused; right after it is
    /// accepted. The gate is the value written to DANMAKU_COMMANDS_ENABLED,
    /// which is what the public page reports as `bilibili.enable_danmaku_command`.
    #[test]
    fn switch_command_refused_before_miss_accepted_right_after() {
        let mut state = session(Some("v1"));
        assert!(!state.commands_gate(true, true), "refused while live");
        state.on_refetch(RefetchReading::Offline, calm(), t0());
        assert!(state.commands_gate(true, true), "accepted after the miss");
        // Commands turned off in config never open it.
        assert!(!state.commands_gate(false, true));
    }

    /// Scope 2: a timeout or 5xx retries ffmpeg, sends nothing, gate shut.
    #[test]
    fn request_failure_retries_without_ending() {
        let mut state = session(Some("v1"));
        let reading = RefetchReading::from_result::<&str>(Err("HTTP 503 / timeout"));
        assert_eq!(reading, RefetchReading::Error);
        assert_eq!(state.on_refetch(reading, calm(), t0()), RefetchStep::Retry);
        assert!(!state.ended);
        assert!(!state.announced);
        assert!(!state.commands_gate(true, true));
    }

    /// Scope 3: repeated misses announce once per broadcast; a new stream
    /// clears the state.
    #[test]
    fn repeated_misses_announce_once_and_next_stream_clears() {
        let mut state = session(Some("v1"));
        assert_eq!(
            state.on_refetch(RefetchReading::Offline, calm(), t0()),
            RefetchStep::Ended {
                announce: true,
                timed_out: false
            }
        );
        assert_eq!(
            state.on_refetch(RefetchReading::Offline, calm(), t0()),
            RefetchStep::Ended {
                announce: false,
                timed_out: false
            }
        );
        // Same broadcast picked up again by the outer loop, then missing again.
        state.start_session("YT:chan".to_string(), Some("v1"));
        assert_eq!(
            state.on_refetch(RefetchReading::Offline, calm(), t0()),
            RefetchStep::Ended {
                announce: false,
                timed_out: false
            }
        );
        // The next stream starts: fresh state, announces again.
        state.start_session("YT:chan".to_string(), Some("v2"));
        assert!(!state.ended && !state.announced);
        assert_eq!(
            state.on_refetch(RefetchReading::Offline, calm(), t0()),
            RefetchStep::Ended {
                announce: true,
                timed_out: false
            }
        );
        // A different channel is also a new stream.
        state.start_session("TW:other".to_string(), None);
        assert!(!state.announced);
    }

    /// Scope 4: the same stream id back live after a miss closes the gate.
    #[test]
    fn same_stream_back_live_closes_gate() {
        let mut state = session(Some("v1"));
        state.on_refetch(RefetchReading::Offline, calm(), t0());
        assert!(state.commands_gate(true, true));
        assert_eq!(
            state.on_refetch(live(Some("v1")), calm(), t0()),
            RefetchStep::Restart {
                m3u8_url: Some(URL.to_string())
            }
        );
        assert!(!state.commands_gate(true, true));
        // Via the outer loop too.
        state.on_refetch(RefetchReading::Offline, calm(), t0());
        state.start_session("YT:chan".to_string(), Some("v1"));
        assert!(!state.commands_gate(true, true));
        // A missing id counts as the same broadcast.
        assert!(matches!(
            state.on_refetch(live(None), calm(), t0()),
            RefetchStep::Restart { .. }
        ));
    }

    /// L1: a new broadcast with an unknown id does not inherit `announced`.
    #[test]
    fn unknown_id_broadcast_does_not_inherit_announced() {
        for (first, next) in [(Some("v1"), None), (None, Some("v1")), (None, None)] {
            let mut state = session(first);
            state.on_refetch(RefetchReading::Offline, calm(), t0());
            assert!(state.announced);
            state.start_session("YT:chan".to_string(), next);
            assert!(!state.announced, "{first:?} -> {next:?}");
            assert_eq!(
                state.on_refetch(RefetchReading::Offline, calm(), t0()),
                RefetchStep::Ended {
                    announce: true,
                    timed_out: false
                }
            );
        }
    }

    /// M1: B站 stopped first with the source positively offline marks the
    /// stream ended through the same state (announce once, gate opens), and
    /// keeps the active-node and generation checks.
    #[test]
    fn bili_stopped_first_marks_ended_through_shared_state() {
        use FfmpegLoopExitReason as R;
        assert_eq!(
            end_action(R::BiliStopped, false, false, false, false),
            EndAction::AnnounceEnd { bili_live: false }
        );
        let mut state = session(Some("v1"));
        assert_eq!(state.mark_ended(calm()), Some(true));
        assert!(state.commands_gate(true, true));
        // Once per broadcast, also when a later miss follows.
        assert_eq!(state.mark_ended(calm()), Some(false));
        assert_eq!(
            state.on_refetch(RefetchReading::Offline, calm(), t0()),
            RefetchStep::Ended {
                announce: false,
                timed_out: false
            }
        );
        // Standby or a stop this session: no send, no gate.
        let mut state = session(Some("v1"));
        assert_eq!(state.mark_ended(end_signals(7, 7, false, false)), None);
        assert_eq!(state.mark_ended(end_signals(7, 8, false, true)), None);
        assert!(!state.commands_gate(true, true));
    }

    /// A restart exit that finds the source offline skips the generation
    /// check, but still announces only once per broadcast: a later miss or a
    /// repeated restart exit sends nothing more. A standby still never sends.
    #[test]
    fn restart_exit_offline_announces_once_per_broadcast() {
        let restart = FfmpegLoopExitReason::IntentionalRestart {
            target_m3u8_available: false,
        };
        assert_eq!(
            end_action(restart, false, false, false, true),
            EndAction::AnnounceEnd { bili_live: true }
        );
        let moved = end_signals(7, 8, true, true);
        let mut state = session(Some("v1"));
        assert_eq!(
            state.mark_ended(post_loop_end_signals(restart, moved)),
            Some(true)
        );
        assert_eq!(
            state.mark_ended(post_loop_end_signals(restart, moved)),
            Some(false)
        );
        state.start_session("YT:chan".to_string(), Some("v1"));
        assert_eq!(
            state.on_refetch(RefetchReading::Offline, calm(), t0()),
            RefetchStep::Ended {
                announce: false,
                timed_out: false
            }
        );
        assert_eq!(
            state.mark_ended(post_loop_end_signals(restart, moved)),
            Some(false)
        );
        // Other exits keep the generation check; standby never sends.
        let mut state = session(Some("v1"));
        assert_eq!(
            state.mark_ended(post_loop_end_signals(
                FfmpegLoopExitReason::BiliStopped,
                moved
            )),
            None
        );
        let standby = end_signals(7, 8, true, false);
        assert_eq!(
            state.mark_ended(post_loop_end_signals(restart, standby)),
            None
        );
    }

    /// Request errors alone for STREAM_END_ERROR_WINDOW end the stream once.
    #[test]
    fn request_errors_for_the_window_end_the_stream() {
        let start = t0();
        let mut state = session(Some("v1"));
        assert_eq!(
            state.on_refetch(RefetchReading::Error, calm(), start),
            RefetchStep::Retry
        );
        let almost = start + STREAM_END_ERROR_WINDOW - Duration::from_secs(1);
        assert_eq!(
            state.on_refetch(RefetchReading::Error, calm(), almost),
            RefetchStep::Retry
        );
        assert!(!state.commands_gate(true, true));
        let expired = start + STREAM_END_ERROR_WINDOW;
        assert_eq!(
            state.on_refetch(RefetchReading::Error, calm(), expired),
            RefetchStep::Ended {
                announce: true,
                timed_out: true
            }
        );
        assert!(state.commands_gate(true, true));
        // Expiry on a standby or after a stop: nothing.
        for signals in [
            end_signals(7, 7, false, false),
            end_signals(7, 8, false, true),
        ] {
            let mut state = session(Some("v1"));
            state.on_refetch(RefetchReading::Error, signals, start);
            assert!(matches!(
                state.on_refetch(RefetchReading::Error, signals, expired),
                RefetchStep::Cancelled(_)
            ));
            assert!(!state.announced && !state.ended);
        }
    }

    /// N1: an error, then a healthy push of 60 s or more, then another error
    /// is a retry: the push cleared the window.
    #[test]
    fn healthy_push_clears_the_error_window() {
        let start = t0();
        let mut state = session(Some("v1"));
        assert_eq!(
            state.on_refetch(RefetchReading::Error, calm(), start),
            RefetchStep::Retry
        );
        // ffmpeg then pushes fine for 20 min on the old URL.
        state.on_push_ended(Duration::from_secs(20 * 60));
        let later = start + Duration::from_secs(20 * 60 + 5);
        assert_eq!(
            state.on_refetch(RefetchReading::Error, calm(), later),
            RefetchStep::Retry
        );
        // Exactly the threshold also clears.
        state.on_push_ended(HEALTHY_PUSH_CLEARS_ERRORS);
        assert_eq!(
            state.on_refetch(
                RefetchReading::Error,
                calm(),
                later + STREAM_END_ERROR_WINDOW + Duration::from_secs(1)
            ),
            RefetchStep::Retry
        );
        assert!(!state.ended && !state.announced);
    }

    /// N1: pushes shorter than 60 s do not clear the window; the stream is
    /// still treated as down and ends once the window runs out.
    #[test]
    fn short_push_does_not_clear_the_error_window() {
        let start = t0();
        let mut state = session(Some("v1"));
        state.on_refetch(RefetchReading::Error, calm(), start);
        let mut now = start;
        while now < start + STREAM_END_ERROR_WINDOW {
            state.on_push_ended(HEALTHY_PUSH_CLEARS_ERRORS - Duration::from_secs(1));
            now += Duration::from_secs(62);
            let step = state.on_refetch(RefetchReading::Error, calm(), now);
            if now < start + STREAM_END_ERROR_WINDOW {
                assert_eq!(step, RefetchStep::Retry);
            } else {
                assert_eq!(
                    step,
                    RefetchStep::Ended {
                        announce: true,
                        timed_out: true
                    }
                );
            }
        }
    }

    fn run_session(minutes: u64, sample: impl Fn(u32) -> PushSample) -> Duration {
        let start = t0();
        let mut moving = PushMovingTime::default();
        let steps = (minutes * 60 / PUSH_SAMPLE_INTERVAL.as_secs()) as u32;
        for i in 0..=steps {
            moving.sample(start + PUSH_SAMPLE_INTERVAL * i, sample(i));
        }
        moving.moving
    }

    /// N1: a 20-minute session with no data moving does not clear the window,
    /// whether the stats read zero or ffmpeg reports speed with frozen time.
    #[test]
    fn stalled_twenty_minute_session_does_not_clear_the_window() {
        let stalled = [
            run_session(20, |_| PushSample::default()),
            run_session(20, |_| PushSample {
                bitrate_kbps: None,
                speed: Some(0.5),
                push_time_secs: Some(100),
            }),
            run_session(20, |i| PushSample {
                bitrate_kbps: Some(3000.0),
                speed: Some(1.0),
                push_time_secs: Some(100 + (i / 1000)), // frozen
            }),
        ];
        for pushed in stalled {
            assert!(pushed < HEALTHY_PUSH_CLEARS_ERRORS, "{pushed:?}");
            let start = t0();
            let mut state = session(Some("v1"));
            state.on_refetch(RefetchReading::Error, calm(), start);
            state.on_push_ended(pushed);
            assert_eq!(
                state.on_refetch(
                    RefetchReading::Error,
                    calm(),
                    start + Duration::from_secs(20 * 60)
                ),
                RefetchStep::Ended {
                    announce: true,
                    timed_out: true
                }
            );
        }
        // A session that is moving data for 20 min counts in full.
        let healthy = run_session(20, |i| PushSample {
            bitrate_kbps: Some(3000.0),
            speed: Some(1.0),
            push_time_secs: Some(i * 5),
        });
        assert_eq!(healthy, Duration::from_secs(20 * 60));
    }

    /// A clear "live" answer restarts the error window.
    #[test]
    fn live_answer_resets_the_error_window() {
        let start = t0();
        let mut state = session(Some("v1"));
        state.on_refetch(RefetchReading::Error, calm(), start);
        let mid = start + Duration::from_secs(400);
        assert!(matches!(
            state.on_refetch(live(Some("v1")), calm(), mid),
            RefetchStep::Restart { .. }
        ));
        state.on_refetch(RefetchReading::Error, calm(), mid + Duration::from_secs(1));
        // 600 s after the first error, but only 300 s after the live answer.
        assert_eq!(
            state.on_refetch(
                RefetchReading::Error,
                calm(),
                start + STREAM_END_ERROR_WINDOW + Duration::from_secs(100)
            ),
            RefetchStep::Retry
        );
        assert_eq!(
            state.on_refetch(
                RefetchReading::Error,
                calm(),
                mid + Duration::from_secs(1) + STREAM_END_ERROR_WINDOW
            ),
            RefetchStep::Ended {
                announce: true,
                timed_out: true
            }
        );
    }

    /// F4: a different id goes back to the outer loop.
    #[test]
    fn refetch_sends_a_new_broadcast_back_to_the_outer_loop() {
        let mut state = session(Some("video-1"));
        assert_eq!(
            state.on_refetch(live(Some("video-2")), calm(), t0()),
            RefetchStep::NewBroadcast
        );
        let mut state = session(Some("lv100"));
        assert_eq!(
            state.on_refetch(live(Some("lv200")), calm(), t0()),
            RefetchStep::NewBroadcast
        );
        assert!(same_broadcast(Some("lv100"), Some("lv100")));
        assert!(!same_broadcast(Some("lv100"), Some("lv200")));
    }

    #[test]
    fn new_broadcast_exit_skips_end_danmaku_and_clears_crop() {
        use FfmpegLoopExitReason as R;
        assert_eq!(
            end_action(R::NewBroadcast, false, false, true, true),
            EndAction::SkipNewBroadcast
        );
        assert!(!exit_keeps_crop(R::NewBroadcast));
    }

    /// F1 (a)/(c): flags and bumps from before the session (e.g. a standby
    /// demoted while idle, then promoted) do not cancel; 直播结束 goes out.
    #[test]
    fn stale_stop_restart_from_before_the_session_does_not_cancel() {
        let stale = end_signals(42, 42, true, true);
        assert_eq!(end_recheck_cancel(stale), None);
        let mut state = session(Some("v1"));
        assert_eq!(
            state.on_refetch(RefetchReading::Offline, stale, t0()),
            RefetchStep::Ended {
                announce: true,
                timed_out: false
            }
        );
        assert_eq!(
            end_action(FfmpegLoopExitReason::SourceEnded, true, false, false, true),
            EndAction::AnnounceEnd { bili_live: true }
        );
    }

    /// F2: only a restart keeps the crop; a stop during the recheck clears it.
    #[test]
    fn end_check_cancel_keeps_crop_only_for_restart() {
        use FfmpegLoopExitReason as R;
        assert!(!exit_keeps_crop(R::EndCheckCancelled { restart: false }));
        assert!(exit_keeps_crop(R::EndCheckCancelled { restart: true }));
        // Unchanged from before the patch.
        assert!(exit_keeps_crop(R::IntentionalRestart {
            target_m3u8_available: true
        }));
        assert!(exit_keeps_crop(R::ClusterHandoff));
        assert!(!exit_keeps_crop(R::IntentionalRestart {
            target_m3u8_available: false
        }));
        assert!(!exit_keeps_crop(R::SourceEnded));
        assert!(!exit_keeps_crop(R::BiliStopped));
        // Both still skip the end danmaku.
        for restart in [false, true] {
            assert!(restart_exit_should_skip_end_danmaku(R::EndCheckCancelled {
                restart
            }));
        }
    }

    #[test]
    fn end_action_keeps_intentional_skips() {
        use FfmpegLoopExitReason as R;
        let restart = R::IntentionalRestart {
            target_m3u8_available: true,
        };
        for (source, bili) in [(false, true), (false, false), (true, true)] {
            assert_eq!(
                end_action(R::ClusterHandoff, false, false, source, bili),
                EndAction::SkipHandoff
            );
            assert_eq!(
                end_action(restart, false, false, source, bili),
                EndAction::SkipRestart
            );
            assert_eq!(
                end_action(
                    R::EndCheckCancelled { restart: false },
                    true,
                    false,
                    source,
                    bili
                ),
                EndAction::SkipEndCheckCancelled
            );
            assert_eq!(
                end_action(R::BiliStopped, true, false, source, bili),
                EndAction::SkipManualStop
            );
            assert_eq!(
                end_action(R::SourceEnded, false, true, source, bili),
                EndAction::SkipWarning
            );
        }
    }

    #[test]
    fn end_action_announces_end_whether_or_not_room_is_live() {
        use FfmpegLoopExitReason as R;
        assert_eq!(
            end_action(R::SourceEnded, false, false, false, true),
            EndAction::AnnounceEnd { bili_live: true }
        );
        // Cause 2: the room already stopped no longer means silence.
        assert_eq!(
            end_action(R::SourceEnded, false, false, false, false),
            EndAction::AnnounceEnd { bili_live: false }
        );
        assert_eq!(
            end_action(R::BiliStopped, false, false, false, false),
            EndAction::AnnounceEnd { bili_live: false }
        );
        // Source still live: interrupted, or the room stopped from elsewhere.
        assert_eq!(
            end_action(
                R::IntentionalRestart {
                    target_m3u8_available: false
                },
                false,
                false,
                true,
                true
            ),
            EndAction::AnnounceInterrupted
        );
        assert_eq!(
            end_action(R::BiliStopped, false, false, true, false),
            EndAction::BiliStoppedQuietly
        );
    }

    #[test]
    fn end_cause_follows_the_room_and_never_ends_a_live_one() {
        // Skipped exits that keep the room live produce no 下播 at all.
        for action in [
            EndAction::SkipHandoff,
            EndAction::SkipRestart,
            EndAction::SkipNewBroadcast,
            EndAction::SkipEndCheckCancelled,
            EndAction::AnnounceInterrupted,
            EndAction::AnnounceEnd { bili_live: true },
        ] {
            assert_eq!(end_cause(action, true), None, "{action:?}");
            assert_eq!(end_cause(action, false), None, "{action:?}");
        }
        // A warning with the room still live is not an end.
        assert_eq!(end_cause(EndAction::SkipWarning, true), None);
        assert_eq!(
            end_cause(EndAction::SkipWarning, false),
            Some(BroadcastEndCause::WarningCut)
        );
        // Every offline-room action names its cause.
        assert_eq!(
            end_cause(EndAction::SkipManualStop, false),
            Some(BroadcastEndCause::ManualStop)
        );
        assert_eq!(
            end_cause(EndAction::AnnounceEnd { bili_live: false }, false),
            Some(BroadcastEndCause::SourceEnded)
        );
        assert_eq!(
            end_cause(EndAction::BiliStoppedQuietly, false),
            Some(BroadcastEndCause::BiliStopped)
        );
    }

    #[test]
    fn end_danmaku_text_and_auto_stop_threshold() {
        assert_eq!(
            end_danmaku_text("示例", true),
            "示例 直播结束，可使用弹幕指令进行换台"
        );
        assert_eq!(end_danmaku_text("示例", false), "示例 直播结束");
        assert!(!should_auto_stop_after_end(2));
        assert!(should_auto_stop_after_end(3));
        assert!(should_auto_stop_after_end(8));
    }

    #[test]
    fn restart_skip_requires_replacement_m3u8() {
        assert!(restart_exit_should_skip_end_danmaku(
            FfmpegLoopExitReason::IntentionalRestart {
                target_m3u8_available: true,
            }
        ));

        assert!(!restart_exit_should_skip_end_danmaku(
            FfmpegLoopExitReason::IntentionalRestart {
                target_m3u8_available: false,
            }
        ));
        assert!(!restart_exit_should_skip_end_danmaku(
            FfmpegLoopExitReason::SourceEnded
        ));
        assert!(restart_exit_should_skip_end_danmaku(
            FfmpegLoopExitReason::ClusterHandoff
        ));
        assert!(restart_exit_should_skip_end_danmaku(
            FfmpegLoopExitReason::EndCheckCancelled { restart: false }
        ));
    }

    #[test]
    fn ffmpeg_exit_failure_counts_only_when_streams_remain_live() {
        assert!(ffmpeg_exit_should_count_as_failure(
            true, true, true, false, false, false, false
        ));

        assert!(!ffmpeg_exit_should_count_as_failure(
            true, false, true, false, false, false, false
        ));
        assert!(!ffmpeg_exit_should_count_as_failure(
            true, true, false, false, false, false, false
        ));
    }

    #[test]
    fn ffmpeg_exit_failure_ignores_intentional_stops() {
        assert!(!ffmpeg_exit_should_count_as_failure(
            true, true, true, true, false, false, false
        ));
        assert!(!ffmpeg_exit_should_count_as_failure(
            true, true, true, false, true, false, false
        ));
        assert!(!ffmpeg_exit_should_count_as_failure(
            true, true, true, false, false, true, false
        ));
        assert!(!ffmpeg_exit_should_count_as_failure(
            true, true, true, false, false, false, true
        ));
    }

    #[test]
    fn priority_eligibility_rejects_banned_titles_before_overlay() {
        let keywords = vec!["asmr".to_owned()];
        let mut ordinary = test_stream_candidate(StreamPlatform::Youtube, true);
        ordinary.channel_id = "ordinary".into();
        let mut priority = test_stream_candidate(StreamPlatform::Youtube, true);
        priority.channel_id = "priority".into();
        priority.title = Some("ASMR stream".into());
        assert_eq!(
            exclude_banned_stream(&mut priority, &keywords).as_deref(),
            Some("asmr")
        );
        assert!(!priority.is_live);
        let selected = overlay_priority(ordinary, priority);
        assert_eq!(selected.channel_id, "ordinary");
        assert!(!selected.is_priority);
    }

    #[test]
    fn stream_title_joins_topic_and_title() {
        let mut stream = test_stream_candidate(StreamPlatform::Youtube, true);
        stream.topic = Some("Just Chatting".to_string());
        stream.title = Some("雑談".to_string());
        assert_eq!(stream.stream_title().as_deref(), Some("Just Chatting 雑談"));
    }
    #[test]
    fn status_message_update_ignores_small_time_only_changes() {
        let last = "YT: channel 未直播，计划于 2026-07-04 12:00:00 开始，";
        let current = "YT: channel 未直播，计划于 2026-07-04 12:04:00 开始，";

        assert!(!should_update_status_message(last, current));
    }

    #[test]
    fn status_message_update_keeps_non_time_changes() {
        let last = "YT: channel 未直播，计划于 2026-07-04 12:00:00 开始，";
        let current = "YT: channel 未直播，计划于 2026-07-04 12:04:00 开始，new title";

        assert!(should_update_status_message(last, current));
    }

    #[test]
    fn priority_probe_respects_restart_toggle_and_current_target() {
        let mut cfg: Config = serde_json::from_value(serde_json::json!({
            "auto_cover": false, "enable_anti_collision": false, "interval": 10,
            "bililive": {"enable_danmaku_command": false, "room": 1, "bili_rtmp_url": "", "bili_rtmp_key": ""},
            "youtube": {}, "twitch": {}, "enable_lol_monitor": false, "anti_collision_list": {},
            "priority_channel": {"enabled": true, "channel_name": "priority", "youtube_channel_id": "id"}
        })).unwrap();
        let warnings = bilistream::plugins::danmaku::warning::WarningTracker::default();
        let clear = warnings.snapshot();
        let other = BroadcastSource::new("YT", "priority", "other", None);
        let current_priority = BroadcastSource::new("YT", "other name", "id", None);
        assert!(!should_probe_priority(&cfg, &other, &clear));
        cfg.priority_channel.auto_restart = true;
        assert!(should_probe_priority(&cfg, &other, &clear));
        assert!(!should_probe_priority(&cfg, &current_priority, &clear));
        cfg.priority_channel.enabled = false;
        assert!(!should_probe_priority(&cfg, &other, &clear));
    }
}
