//! Shared typed facts. Producers sanitize text before emitting; consumers
//! independently interpret events without delaying the producer.

/// The seven 结束原因 codes (design §5.4/§5.5, §9 addendum). An open
/// session has no reason.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EndReason {
    SourceEnded,
    BiliStopped,
    UserStop,
    IntentionalRestart,
    ClusterHandoff,
    ProcessRestart,
    Aborted,
}

impl EndReason {
    pub const ALL: [EndReason; 7] = [
        Self::SourceEnded,
        Self::BiliStopped,
        Self::UserStop,
        Self::IntentionalRestart,
        Self::ClusterHandoff,
        Self::ProcessRestart,
        Self::Aborted,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::SourceEnded => "source_ended",
            Self::BiliStopped => "bili_stopped",
            Self::UserStop => "user_stop",
            Self::IntentionalRestart => "intentional_restart",
            Self::ClusterHandoff => "cluster_handoff",
            Self::ProcessRestart => "process_restart",
            Self::Aborted => "aborted",
        }
    }

    pub fn parse(code: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|r| r.as_str() == code)
    }
}

/// Why the Bilibili relay session ended. Distinct from the history session's
/// `EndReason`: this one is keyed to the room going offline and carries the
/// two causes the history codes cannot express (Bilibili 警告/切断 and the
/// idle-room auto stop). It travels in `EventKind::BroadcastEnded` and in the
/// synced notify ledger, so it derives serde.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BroadcastEndCause {
    /// Bilibili 警告/切断 stopped the room.
    WarningCut,
    /// 主动停播 (WebUI or danmaku command).
    ManualStop,
    /// 源直播已结束，房间按自动停播设置主动关播（满阈值立即，否则等一个检测间隔）.
    AutoStop,
    /// 源直播结束：房间当场确认，或房间在换台窗口里随后自行下播.
    SourceEnded,
    /// 违禁词(主播名或玩家ID)命中，本进程主动停播.
    ContentRejected,
    /// The room went offline without this node stopping it. After its source
    /// ended and nothing replaced it the cause is `SourceEnded` instead.
    BiliStopped,
}

impl BroadcastEndCause {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::WarningCut => "warning_cut",
            Self::ManualStop => "manual_stop",
            Self::AutoStop => "auto_stop",
            Self::SourceEnded => "source_ended",
            Self::ContentRejected => "content_rejected",
            Self::BiliStopped => "bili_stopped",
        }
    }

    /// The fixed `结束原因` label for this cause. A source end names the
    /// channel instead; see [`Self::notice_label`].
    pub fn label(self) -> &'static str {
        match self {
            Self::WarningCut => "B站警告/切断",
            Self::ManualStop => "主动停播",
            Self::AutoStop => "源直播结束后自动停播",
            Self::SourceEnded => "源直播结束",
            Self::ContentRejected => "检测到违禁词停播",
            Self::BiliStopped => "B站直播间下播",
        }
    }

    /// The 下播 notice's `结束原因` text. A source end names the channel that
    /// went offline — 「天帝フォルテ下播」 — because that is the cause the
    /// operator saw; every other cause is a room fact with its fixed label.
    /// `last_source` is the session's newest source, empty when unknown.
    pub fn notice_label(self, last_source: &str) -> String {
        match self {
            Self::SourceEnded if !last_source.is_empty() => format!("{last_source}下播"),
            _ => self.label().to_string(),
        }
    }
}

/// Bilibili's 警告/切断 reason as shown after 结束原因 in the 下播 notice.
/// Never serialized: the synced ledger keeps only the cause code.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EndDetail {
    /// 「{违规原因}（{处罚结果}）」 or the reason alone; cleaned and capped.
    pub text: String,
    /// 0 = no reason, 2–3 = dialog title, 4–5 = plain `msg`, 6–7 = the
    /// labelled 违规原因 row; +1 when a 处罚结果 row is present. A higher
    /// rank may replace a lower one for the same cut, never the reverse.
    pub rank: u8,
}

/// StopLiveData totals after the allowlist. No revenue field exists.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct BiliTotals {
    /// B站 `LiveTime`: seconds of live time reported by the room summary.
    /// Never used as a duration — the relay session's own clock is authoritative.
    pub live_time_secs: Option<i64>,
    pub add_fans: Option<i64>,
    pub new_fans_club: Option<i64>,
    pub danmu_num: Option<i64>,
    pub max_online: Option<i64>,
    pub watched_count: Option<i64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Severity {
    Info,
    Warn,
    Error,
}

impl Severity {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Info => "info",
            Self::Warn => "warn",
            Self::Error => "error",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NodeState {
    Up,
    Degraded,
    Down,
    Draining,
    Unknown,
}

impl NodeState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Up => "up",
            Self::Degraded => "degraded",
            Self::Down => "down",
            Self::Draining => "draining",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum BiliSummaryPhase {
    #[default]
    Pending,
    Claimed,
    Failed,
    Done,
    Skipped,
}

/// Complete durable mirror of a signed Bilibili book entry. The key never
/// enters an admin response or a diagnostic.
#[derive(Clone, PartialEq, Eq)]
pub struct BiliLiveRecord {
    pub room: u64,
    pub live_key: String,
    pub started_at: u64,
    pub started_by: String,
    pub stopped_at: Option<u64>,
    pub summary_phase: BiliSummaryPhase,
    pub summary_node: Option<String>,
    pub summary_at: u64,
    pub summary_generation: u64,
    pub totals: Option<BiliTotals>,
}

impl std::fmt::Debug for BiliLiveRecord {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BiliLiveRecord")
            .field("room", &self.room)
            .field("live_key", &"<redacted>")
            .field("summary_phase", &self.summary_phase)
            .finish_non_exhaustive()
    }
}

#[derive(Clone)]
pub enum EventKind {
    /// Notifier intake for the existing once-per-broadcast policy. History
    /// records BiliLiveStarted/Stopped instead, avoiding duplicate facts.
    BroadcastStarted {
        stream_key: String,
        platform: String,
        channel: String,
        title: String,
    },
    /// The Bilibili room went offline: the one 下播 boundary of a relay
    /// session (a source switch never ends it).
    BroadcastEnded {
        cause: BroadcastEndCause,
        /// Bilibili's own 警告/切断 reason for a `WarningCut`, already
        /// cleaned and capped by the danmaku client. In-process only.
        detail: Option<EndDetail>,
    },
    ProcessStarted {
        version: &'static str,
    },
    SessionStarted {
        seq: u64,
        platform: &'static str,
        channel_id: String,
        channel_name: String,
        title: Option<String>,
        live_key: Option<String>,
        /// YouTube video id, Twitch stream id, or Niconico program id.
        source_id: Option<String>,
        area_v2: Option<u64>,
        room: u64,
    },
    /// A key adopted after the session started (GetInfo fallback).
    SessionLiveKey {
        seq: u64,
        live_key: String,
    },
    SessionEnded {
        seq: u64,
        reason: EndReason,
    },
    FfmpegStarted {
        seq: u64,
        run: u64,
    },
    FfmpegExited {
        seq: u64,
        run: u64,
        code: Option<i32>,
        success: bool,
        counted_failure: bool,
        diagnostic: Option<String>,
    },
    FfmpegStuck {
        seq: u64,
        run: u64,
        reason: &'static str,
    },
    StatsSample {
        seq: u64,
        bitrate_kbps: (f32, f32, f32),
        fps: Option<f32>,
        speed: Option<f32>,
    },
    BiliCounts {
        /// Captured at observation time, never inferred at dequeue time.
        seq: u64,
        room: u64,
        online: Option<u64>,
        watched: Option<u64>,
    },
    BiliLiveState {
        record: BiliLiveRecord,
    },
    BiliLiveStarted {
        live_key: String,
        room: u64,
    },
    BiliLiveStopped {
        live_key: String,
        room: u64,
    },
    /// `totals: None` = failed fetch; the row's totals stay NULL.
    BiliLiveSummary {
        live_key: String,
        totals: Option<BiliTotals>,
    },
    /// Live session for which no key could be found: 无 B 站汇总.
    BiliLiveSkipped {
        live_key: String,
    },
    BiliStartFailed {
        seq: Option<u64>,
        error: String,
    },
    /// `BILI_START_TEMP_BAN`: Bilibili refused a new live for now, so this
    /// node switched its monitors off. Its own kind keeps it out of the
    /// 警告/切断 count; the detail names the cause.
    BiliStartRestricted {
        detail: String,
    },
    /// `msg` is Bilibili's reason (cleaned, capped) or the fixed fallback;
    /// `rank` orders how detailed it is (see [`EndDetail::rank`]) so the
    /// writer keeps one row per warning or cut.
    /// `merge`: the frame takes part in the writer's 60 s window (CUT_OFF
    /// with CUT_OFF_V2; an identical repeated WARNING). Others always get a row.
    /// `stop`: the frame stopped the room. A stopping warning is stored as
    /// `bili.warning.stop` (the session then derives `end_warning_stop`); a
    /// warning that did not stop stays `bili.warning`. Every warning frame
    /// today stops the room.
    BiliWarning {
        seq: Option<u64>,
        room: u64,
        msg: String,
        rank: u8,
        merge: bool,
        stop: bool,
    },
    BiliCutOff {
        seq: Option<u64>,
        room: u64,
        msg: String,
        rank: u8,
        merge: bool,
    },
    RoomLocked {
        room: u64,
        until: i64,
    },
    NodeState {
        node_id: String,
        state: NodeState,
        reason: String,
        initial: bool,
        minority: bool,
    },
    /// Only recorder observations extend cluster coverage, never a writer
    /// heartbeat while observer authority is absent.
    RecorderHeartbeat,
    RecorderStopped,
    OwnerChanged {
        from: Option<String>,
        to: Option<String>,
        failover: bool,
    },
    QuorumChanged {
        has_quorum: bool,
    },
    LocalFault {
        reason: String,
        latched: bool,
    },
    NotifySent {
        key: String,
        dedup: String,
        phase: &'static str,
        mode: Option<&'static str>,
        ok: bool,
        code: Option<i64>,
        trace_id: Option<String>,
        test: bool,
        suppressed: u32,
        undelivered: u32,
    },
}

impl std::fmt::Debug for EventKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Some variants intentionally carry a private live key; even a
        // failed assertion or future diagnostic must not print that payload.
        let name = match self {
            Self::BroadcastStarted { .. } => "BroadcastStarted",
            Self::BroadcastEnded { .. } => "BroadcastEnded",
            Self::ProcessStarted { .. } => "ProcessStarted",
            Self::SessionStarted { .. } => "SessionStarted",
            Self::SessionLiveKey { .. } => "SessionLiveKey",
            Self::SessionEnded { .. } => "SessionEnded",
            Self::FfmpegStarted { .. } => "FfmpegStarted",
            Self::FfmpegExited { .. } => "FfmpegExited",
            Self::FfmpegStuck { .. } => "FfmpegStuck",
            Self::StatsSample { .. } => "StatsSample",
            Self::BiliCounts { .. } => "BiliCounts",
            Self::BiliLiveState { .. } => "BiliLiveState",
            Self::BiliLiveStarted { .. } => "BiliLiveStarted",
            Self::BiliLiveStopped { .. } => "BiliLiveStopped",
            Self::BiliLiveSummary { .. } => "BiliLiveSummary",
            Self::BiliLiveSkipped { .. } => "BiliLiveSkipped",
            Self::BiliStartFailed { .. } => "BiliStartFailed",
            Self::BiliStartRestricted { .. } => "BiliStartRestricted",
            Self::BiliWarning { .. } => "BiliWarning",
            Self::BiliCutOff { .. } => "BiliCutOff",
            Self::RoomLocked { .. } => "RoomLocked",
            Self::NodeState { .. } => "NodeState",
            Self::RecorderHeartbeat => "RecorderHeartbeat",
            Self::RecorderStopped => "RecorderStopped",
            Self::OwnerChanged { .. } => "OwnerChanged",
            Self::QuorumChanged { .. } => "QuorumChanged",
            Self::LocalFault { .. } => "LocalFault",
            Self::NotifySent { .. } => "NotifySent",
        };
        f.debug_struct(name).finish_non_exhaustive()
    }
}

#[derive(Clone, Debug)]
pub struct Event {
    pub at: u64,
    pub kind: EventKind,
}

/// Every metric in the sample tables; API names are the strings.
pub(crate) const METRICS: [(&str, i64); 5] = [
    ("bitrate", 1),
    ("fps", 2),
    ("speed", 3),
    ("online", 10),
    ("watched", 11),
];

pub(crate) fn metric_id(name: &str) -> Option<i64> {
    METRICS.iter().find(|(n, _)| *n == name).map(|(_, id)| *id)
}
