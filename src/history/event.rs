//! History events (design §2, history-only subset). Producers call
//! `history::emit`; it never blocks or awaits.

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

/// StopLiveData totals after the allowlist. No revenue field exists.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct BiliTotals {
    /// `LiveTime`, stored raw only; never used as a duration.
    pub live_time_raw: Option<i64>,
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

#[derive(Debug)]
pub enum EventKind {
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
    StatsSample {
        seq: u64,
        bitrate_kbps: (f32, f32, f32),
        fps: Option<f32>,
        speed: Option<f32>,
    },
    BiliCounts {
        online: Option<u64>,
        watched: Option<u64>,
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
    Generic {
        kind: &'static str,
        subject: Option<String>,
        severity: Severity,
        title: String,
        detail: String,
    },
    NotifySent {
        key: String,
        phase: &'static str,
        ok: bool,
        code: Option<i64>,
        suppressed: u32,
        undelivered: u32,
    },
}

#[derive(Debug)]
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
