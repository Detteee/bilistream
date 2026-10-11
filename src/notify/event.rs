//! Typed notifier events (design §2–§3). Phase 1 is cluster node status only.

use serde::{Deserialize, Serialize};

/// Stable event keys. The string form is the config toggle key and the
/// prefix of every dedup key, so it must never change once released.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum EventKind {
    #[serde(rename = "cluster.node_down")]
    NodeDown,
    #[serde(rename = "cluster.node_degraded")]
    NodeDegraded,
    #[serde(rename = "cluster.failover")]
    Failover,
    #[serde(rename = "cluster.quorum_lost")]
    QuorumLost,
    #[serde(rename = "cluster.self_check_failing")]
    SelfCheckFailing,
    /// A channel on the 开播通知关注列表 went live (info, never 召回).
    #[serde(rename = "stream.live")]
    StreamLive,
    #[serde(rename = "notify.test")]
    Test,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Info,
    Warning,
    Critical,
}

impl EventKind {
    pub const PHASE1: [EventKind; 5] = [
        EventKind::NodeDown,
        EventKind::NodeDegraded,
        EventKind::Failover,
        EventKind::QuorumLost,
        EventKind::SelfCheckFailing,
    ];

    pub fn from_key(key: &str) -> Option<Self> {
        Self::PHASE1
            .into_iter()
            .chain([EventKind::StreamLive, EventKind::Test])
            .find(|kind| kind.key() == key)
    }

    pub fn key(self) -> &'static str {
        match self {
            EventKind::NodeDown => "cluster.node_down",
            EventKind::NodeDegraded => "cluster.node_degraded",
            EventKind::Failover => "cluster.failover",
            EventKind::QuorumLost => "cluster.quorum_lost",
            EventKind::SelfCheckFailing => "cluster.self_check_failing",
            EventKind::StreamLive => "stream.live",
            EventKind::Test => "notify.test",
        }
    }

    pub fn severity(self) -> Severity {
        match self {
            EventKind::NodeDown | EventKind::Failover | EventKind::QuorumLost => Severity::Critical,
            EventKind::NodeDegraded | EventKind::SelfCheckFailing => Severity::Warning,
            EventKind::StreamLive | EventKind::Test => Severity::Info,
        }
    }

    /// Stateful events open an incident that a later `Clear` resolves.
    /// Stateless ones (failover, test) are one-shot.
    pub fn stateful(self) -> bool {
        !matches!(
            self,
            EventKind::Failover | EventKind::Test | EventKind::StreamLive
        )
    }

    pub fn title(self) -> &'static str {
        match self {
            EventKind::NodeDown => "节点故障",
            EventKind::NodeDegraded => "节点降级",
            EventKind::Failover => "故障转移",
            EventKind::QuorumLost => "本节点失去多数派",
            EventKind::SelfCheckFailing => "节点公网自检失败",
            EventKind::StreamLive => "开播",
            EventKind::Test => "通知测试",
        }
    }
}

/// Go-live details for `stream.live`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LiveInfo {
    /// `youtube` | `twitch` | `niconico`.
    pub platform: String,
    /// Display name from the watch list.
    pub name: String,
    pub title: Option<String>,
}

/// Event-specific fields the renderer needs beyond node and detail.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ItemExtra {
    /// `cluster.failover`: the owner that failed (`node` is the new owner).
    pub old_owner: Option<String>,
    pub live: Option<LiveInfo>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Fire,
    /// The condition cleared. A notified incident gets a resolved message.
    Clear,
}

#[derive(Clone, Debug)]
pub struct AlertEvent {
    pub kind: EventKind,
    pub phase: Phase,
    /// Node the event is about (`node_id`).
    pub node: String,
    /// Extra dedup discriminator, e.g. the degraded reason. Empty when unused.
    pub variant: String,
    /// One line, already passed through `logging::safe_text`.
    pub detail: Option<String>,
    /// Unix seconds.
    pub at: u64,
    pub extra: ItemExtra,
}

impl AlertEvent {
    pub fn new(kind: EventKind, phase: Phase, node: &str, at: u64) -> Self {
        Self {
            kind,
            phase,
            node: node.to_string(),
            variant: String::new(),
            detail: None,
            at,
            extra: ItemExtra::default(),
        }
    }

    pub fn with_extra(mut self, extra: ItemExtra) -> Self {
        self.extra = extra;
        self
    }

    pub fn with_variant(mut self, variant: &str) -> Self {
        self.variant = variant.to_string();
        self
    }

    pub fn with_detail(mut self, detail: impl AsRef<str>) -> Self {
        self.detail = Some(super::format::clean_detail(detail.as_ref()));
        self
    }

    /// Identical on every node, so ledgers from different authorities agree.
    pub fn dedup_key(&self) -> String {
        dedup_key(self.kind, &self.node, &self.variant)
    }
}

/// A currently-bad condition (level-triggered, design review §0.2). The
/// observer produces the full set on every reconcile; a missing key means the
/// condition cleared.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Condition {
    pub kind: EventKind,
    pub node: String,
    pub variant: String,
    pub detail: Option<String>,
}

impl Condition {
    pub fn new(kind: EventKind, node: &str) -> Self {
        Self {
            kind,
            node: node.to_string(),
            variant: String::new(),
            detail: None,
        }
    }

    pub fn with_variant(mut self, variant: &str) -> Self {
        self.variant = variant.to_string();
        self
    }

    pub fn with_detail(mut self, detail: impl AsRef<str>) -> Self {
        self.detail = Some(super::format::clean_detail(detail.as_ref()));
        self
    }

    pub fn dedup_key(&self) -> String {
        dedup_key(self.kind, &self.node, &self.variant)
    }
}

pub fn dedup_key(kind: EventKind, node: &str, variant: &str) -> String {
    if variant.is_empty() {
        format!("{}:{}", kind.key(), node)
    } else {
        format!("{}:{}:{}", kind.key(), node, variant)
    }
}

/// Human wording for a cluster health reason (design §10).
pub fn reason_text(reason: &str) -> String {
    match reason {
        "heartbeat_timeout" => "心跳超时".to_string(),
        "api_unreachable" => "节点 API 无法连接".to_string(),
        "waiting_for_heartbeat" => "等待心跳（节点未响应）".to_string(),
        "stream_metrics_degraded" => "推流指标异常".to_string(),
        "network_unstable" => "网络不稳定".to_string(),
        "ffmpeg_repeated_failures" => "ffmpeg 反复失败".to_string(),
        "external_api_unreachable" => "外部 API 无法访问".to_string(),
        "network_isolated" => "网络隔离".to_string(),
        "node_fault_latched" => "节点故障已锁定".to_string(),
        other => crate::logging::safe_text(other, 60),
    }
}
