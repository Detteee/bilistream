//! Read-only QQ commands (notification format spec §7): exactly `status`, `event`,
//! `quota` and `help`. Case-insensitive with an optional leading `/`; no
//! aliases, no short forms, no Chinese words or full-width punctuation.
//!
//! Only the bound admin openid gets an answer; every other sender, and any
//! other text, is ignored without a reply. Answers are passive replies
//! (msg_id) sent by the gateway-owning node, rate-limited, and building one
//! never changes notifier state.

use super::event::{EventKind, Severity};
use super::format::{duration_text, month_day, short_time, utc8_time, RenderContext, MAX_CHARS};
use super::ledger::{NotifyLedger, ACTIVE_PER_DAY};
use super::observer::ClusterDigest;
use super::sink::{PassiveTarget, PASSIVE_MAX_REPLIES, PASSIVE_WINDOW_SECS};

/// At most one answer per this many seconds, and `MAX_PER_HOUR` an hour.
pub const MIN_GAP_SECS: u64 = 3;
pub const MAX_PER_HOUR: usize = 20;
const MAX_NODES: usize = 6;
const MAX_ENTRIES: usize = 5;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cmd {
    Status,
    Event,
    Quota,
    Help,
}

/// Exact command words only, case-insensitive, optional leading `/`
/// ("status", "/Status", " help ").
pub fn parse(content: &str) -> Option<Cmd> {
    let text = content.trim();
    let text = text
        .strip_prefix('/')
        .unwrap_or(text)
        .trim()
        .to_ascii_lowercase();
    match text.as_str() {
        "status" => Some(Cmd::Status),
        "event" => Some(Cmd::Event),
        "quota" => Some(Cmd::Quota),
        "help" => Some(Cmd::Help),
        _ => None,
    }
}

/// Sliding-window limiter for answers.
#[derive(Default)]
pub struct Limiter {
    sent: std::collections::VecDeque<u64>,
}

impl Limiter {
    pub fn allow(&mut self, now: u64) -> bool {
        while self
            .sent
            .front()
            .is_some_and(|at| now.saturating_sub(*at) >= 3600)
        {
            self.sent.pop_front();
        }
        if self.sent.len() >= MAX_PER_HOUR
            || self
                .sent
                .back()
                .is_some_and(|at| now.saturating_sub(*at) < MIN_GAP_SECS)
        {
            return false;
        }
        self.sent.push_back(now);
        true
    }
}

/// An open incident as the status/event replies show it.
#[derive(Clone, Debug)]
pub struct OpenEntry {
    pub kind: EventKind,
    pub node: String,
    pub since: u64,
    pub severity: Severity,
}

/// Everything a reply may read. Borrowed, never mutated.
pub struct View<'a> {
    pub now: u64,
    pub ctx: &'a RenderContext,
    pub digest: &'a ClusterDigest,
    pub ledger: &'a NotifyLedger,
    pub open: Vec<OpenEntry>,
    pub passive: Option<&'a PassiveTarget>,
    pub live: Vec<(String, Option<String>)>,
    pub gateway_online: bool,
    /// A membership change paused execution (not a fault; Clerk N6 keeps it
    /// apart from a node's drain).
    pub membership_paused: bool,
}

fn marker(severity: Severity) -> &'static str {
    match severity {
        Severity::Critical => "🔴",
        Severity::Warning => "🟠",
        Severity::Info => "🔵",
    }
}

fn cut(lines: Vec<String>) -> String {
    let text = lines.join("\n");
    if text.chars().count() <= MAX_CHARS {
        return text;
    }
    let mut out: String = text.chars().take(MAX_CHARS - 1).collect();
    out.push('…');
    out
}

pub fn reply(cmd: Cmd, view: &View<'_>) -> String {
    match cmd {
        Cmd::Status => status(view),
        Cmd::Event => events(view),
        Cmd::Quota => quota(view),
        Cmd::Help => help(),
    }
}

fn status(view: &View<'_>) -> String {
    let ctx = view.ctx;
    let worst = view.open.iter().map(|entry| entry.severity).max();
    let (head, word) = match worst {
        Some(Severity::Critical) => ("🔴", "有故障"),
        Some(Severity::Warning) => ("🟠", "有警告"),
        _ => ("✅", "正常"),
    };
    let mut lines = vec![
        format!("{head} bilistream 状态 · {word}"),
        format!("时间 {} (UTC+8)", utc8_time(view.now)),
    ];
    let reporter = ctx.node_name(&ctx.reporter);
    lines.push(match view.digest.owner.as_deref() {
        Some(owner) => format!("活跃节点 {} · 报告 {reporter}", ctx.node_name(owner)),
        None if !view.digest.enabled => format!("单节点模式 · 报告 {reporter}"),
        None => format!("活跃节点 未确定 · 报告 {reporter}"),
    });
    if view.digest.enabled {
        let healthy = view.digest.nodes.iter().filter(|node| node.healthy).count();
        lines.push(format!("节点 {healthy}/{} 正常", view.digest.nodes.len()));
        if view.membership_paused {
            lines.push("成员变更中（暂停执行，非故障）".to_string());
        }
        for node in view.digest.nodes.iter().take(MAX_NODES) {
            let role = if view.digest.owner.as_deref() == Some(node.id.as_str()) {
                "活跃"
            } else {
                "备用"
            };
            let health = if node.draining {
                "正在排空（非故障）".to_string()
            } else if node.healthy {
                "正常".to_string()
            } else if node.stale || !node.fresh {
                format!("故障：{}", super::event::reason_text(&node.reason))
            } else {
                format!("降级：{}", super::event::reason_text(&node.reason))
            };
            lines.push(format!("· {} {role} {health}", ctx.node_name(&node.id)));
        }
        if view.digest.nodes.len() > MAX_NODES {
            lines.push(format!(
                "另有 {} 个节点",
                view.digest.nodes.len() - MAX_NODES
            ));
        }
    }
    if !view.live.is_empty() {
        let names: Vec<String> = view
            .live
            .iter()
            .take(3)
            .map(|(name, _)| ctx.value(name, 20))
            .collect();
        lines.push(format!("直播中 {}", names.join("、")));
    }
    if !view.open.is_empty() {
        lines.push(format!("未恢复 {}：", view.open.len()));
        for entry in view.open.iter().take(MAX_ENTRIES) {
            lines.push(format!(
                "· {} {} {} · 已持续 {}",
                marker(entry.severity),
                entry.kind.title(),
                ctx.node_name(&entry.node),
                duration_text(view.now.saturating_sub(entry.since))
            ));
        }
    }
    lines.push(format!(
        "通知 网关{} · 召回额度剩余 {} · 未送达 {}",
        if view.gateway_online {
            "在线"
        } else {
            "离线"
        },
        view.ledger
            .wakeups_left(view.now)
            .map(|n| n.to_string())
            .unwrap_or_else(|| "未知".into()),
        view.ledger.undelivered_pending()
    ));
    lines.push("回复 help 查看命令".to_string());
    cut(lines)
}

fn events(view: &View<'_>) -> String {
    let ctx = view.ctx;
    let mut lines = vec![
        "📋 bilistream 事件".to_string(),
        format!("时间 {} (UTC+8)", utc8_time(view.now)),
    ];
    if view.open.is_empty() {
        lines.push("当前没有未恢复的事件".to_string());
    } else {
        lines.push(format!("未恢复 {}：", view.open.len()));
        for entry in view.open.iter().take(MAX_ENTRIES) {
            lines.push(format!(
                "· {} {} {} · {} 起",
                marker(entry.severity),
                entry.kind.title(),
                ctx.node_name(&entry.node),
                short_time(entry.since, view.now)
            ));
        }
    }
    let mut recent: Vec<_> = view
        .ledger
        .incidents
        .iter()
        .filter(|mark| view.now.saturating_sub(mark.notified_at) <= 86_400)
        .collect();
    recent.sort_by_key(|mark| std::cmp::Reverse(mark.notified_at));
    if !recent.is_empty() {
        lines.push("最近 24 小时已发送：".to_string());
        for mark in recent.iter().take(MAX_ENTRIES) {
            let mut parts = mark.key.splitn(3, ':');
            let kind = parts.next().and_then(EventKind::from_key);
            let node = parts.next().unwrap_or_default();
            let title = kind.map(|k| k.title()).unwrap_or("通知");
            let subject = if kind == Some(EventKind::StreamLive) {
                ctx.value(mark.key.splitn(3, ':').nth(2).unwrap_or_default(), 20)
            } else {
                ctx.node_name(node)
            };
            let mut line = format!(
                "· {title} {subject} · {} 发送",
                short_time(mark.notified_at, view.now)
            );
            if let Some(at) = mark.resolved_at.filter(|at| *at >= mark.notified_at) {
                line.push_str(&format!("，{} 恢复", short_time(at, view.now)));
            }
            lines.push(line);
        }
    }
    let pending = view.ledger.undelivered_pending();
    if pending > 0 {
        lines.push(format!("另有 {pending} 条通知未送达"));
    }
    cut(lines)
}

fn quota(view: &View<'_>) -> String {
    let mut lines = vec![
        "📊 bilistream 通知额度".to_string(),
        format!("时间 {} (UTC+8)", utc8_time(view.now)),
    ];
    match view.passive.filter(|target| target.usable(view.now)) {
        Some(target) => lines.push(format!(
            "被动回复 剩余 {} 次（至 {}）",
            PASSIVE_MAX_REPLIES.saturating_sub(target.replies_used),
            short_time(target.received_at + PASSIVE_WINDOW_SECS, view.now)
        )),
        None => lines.push("被动回复 无可用窗口（发任意消息即可打开）".to_string()),
    }
    lines.push(format!(
        "主动消息 今日 {}/{}",
        view.ledger.active_today(view.now),
        ACTIVE_PER_DAY
    ));
    let wakeups = match view.ledger.wakeups_left(view.now) {
        Some(left) => {
            let next = view
                .ledger
                .wakeup_after_use(view.now)
                .and_then(|(_, next)| next)
                .map(|at| format!("，下一周期 {} 起", month_day(at)))
                .unwrap_or_default();
            format!("召回额度 本周期剩余 {left} 次{next}")
        }
        None => "召回额度 未知（尚未收到管理员消息）".to_string(),
    };
    lines.push(wakeups);
    lines.push(format!("未送达 {}", view.ledger.undelivered_pending()));
    lines.push("发送任意消息会刷新被动回复窗口和召回周期".to_string());
    cut(lines)
}

fn help() -> String {
    [
        "🔵 bilistream 指令（只读）",
        "status：集群节点、直播和通知通道",
        "event：未恢复和最近 24 小时的事件",
        "quota：被动回复、主动消息和召回剩余次数",
        "help：显示本说明",
        "发送任意消息都会刷新回复窗口，积压的通知会借此送达。",
    ]
    .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::notify::observer::NodeSlot;

    fn view<'a>(
        ctx: &'a RenderContext,
        digest: &'a ClusterDigest,
        ledger: &'a NotifyLedger,
        passive: Option<&'a PassiveTarget>,
    ) -> View<'a> {
        View {
            now: 1_791_702_240,
            ctx,
            digest,
            ledger,
            open: vec![OpenEntry {
                kind: EventKind::NodeDegraded,
                node: "sg-3".into(),
                since: 1_791_702_240 - 41 * 60,
                severity: Severity::Warning,
            }],
            passive,
            live: vec![("主频道".into(), None)],
            gateway_online: true,
            membership_paused: false,
        }
    }

    #[test]
    fn only_exact_words_are_commands() {
        assert_eq!(parse("status"), Some(Cmd::Status));
        assert_eq!(parse("/Status"), Some(Cmd::Status));
        assert_eq!(parse(" /EVENT "), Some(Cmd::Event));
        assert_eq!(parse("event"), Some(Cmd::Event));
        assert_eq!(parse("QUOTA"), Some(Cmd::Quota));
        assert_eq!(parse("help"), Some(Cmd::Help));
        assert_eq!(parse("/help"), Some(Cmd::Help));
        // No aliases, short forms, old words, Chinese or full-width forms.
        for word in [
            "s",
            "alert",
            "alerts",
            "events",
            "?",
            "？",
            "状态",
            "/事件",
            "额度",
            "帮助",
            "通知",
            "状态呢",
            "ｓｔａｔｕｓ",
            "／status",
            "status?",
            "hi",
            "482913",
            "//status",
        ] {
            assert_eq!(parse(word), None, "{word}");
        }
    }

    #[test]
    fn limiter_spaces_and_caps_answers() {
        let mut limiter = Limiter::default();
        assert!(limiter.allow(100));
        assert!(!limiter.allow(101));
        let mut at = 104;
        for _ in 1..MAX_PER_HOUR {
            assert!(limiter.allow(at));
            at += MIN_GAP_SECS;
        }
        assert!(!limiter.allow(at + 10));
        assert!(limiter.allow(100 + 3600 + 1));
    }

    #[test]
    fn replies_render_within_the_limit_without_mutation() {
        let ctx = RenderContext {
            reporter: "osaka-2".into(),
            ..Default::default()
        };
        let digest = ClusterDigest {
            enabled: true,
            owner: Some("osaka-2".into()),
            nodes: vec![
                NodeSlot {
                    id: "osaka-2".into(),
                    healthy: true,
                    fresh: true,
                    ..Default::default()
                },
                NodeSlot {
                    id: "sg-3".into(),
                    healthy: false,
                    fresh: true,
                    reason: "network_unstable".into(),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let mut ledger = NotifyLedger::default();
        ledger.record_notified("cluster.node_down:tokyo-1", 1_791_702_240 - 600);
        ledger.record_resolved("cluster.node_down:tokyo-1", 1_791_702_240 - 120);
        let before = ledger.clone();
        let passive = PassiveTarget {
            msg_id: "m".into(),
            received_at: 1_791_702_240 - 10,
            replies_used: 1,
        };
        let v = view(&ctx, &digest, &ledger, Some(&passive));
        let status = reply(Cmd::Status, &v);
        assert!(
            status.starts_with("🟠 bilistream 状态 · 有警告"),
            "{status}"
        );
        assert!(status.contains("节点 1/2 正常"));
        assert!(status.contains("· sg-3 备用 降级：网络不稳定"));
        assert!(status.contains("直播中 主频道"));
        assert!(status.contains("· 🟠 节点降级 sg-3 · 已持续 41 分钟"));
        let events = reply(Cmd::Event, &v);
        assert!(
            events.contains("· 节点故障 tokyo-1 · 14:54 发送，15:02 恢复"),
            "{events}"
        );
        let quota = reply(Cmd::Quota, &v);
        assert!(quota.contains("被动回复 剩余 3 次"), "{quota}");
        assert!(quota.contains("主动消息 今日 0/200"));
        assert!(reply(Cmd::Help, &v).contains("quota"));
        assert!(reply(Cmd::Help, &v).contains("event："));
        assert!(!reply(Cmd::Help, &v).contains("alert"));
        for cmd in [Cmd::Status, Cmd::Event, Cmd::Quota, Cmd::Help] {
            assert!(reply(cmd, &v).chars().count() <= MAX_CHARS);
        }
        assert_eq!(ledger, before);
    }

    #[test]
    fn a_drain_and_a_membership_pause_are_labelled_apart() {
        // Clerk N6 / copy review row 5.
        let ctx = RenderContext {
            now: 1_791_702_240,
            reporter: "tokyo-1".into(),
            ..Default::default()
        };
        let digest = ClusterDigest {
            enabled: true,
            local: "tokyo-1".into(),
            owner: Some("tokyo-1".into()),
            nodes: vec![
                crate::notify::observer::NodeSlot {
                    id: "tokyo-1".into(),
                    healthy: true,
                    fresh: true,
                    ..Default::default()
                },
                crate::notify::observer::NodeSlot {
                    id: "osaka-2".into(),
                    healthy: true,
                    fresh: true,
                    draining: true,
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let ledger = NotifyLedger::default();
        let mut v = View {
            now: 1_791_702_240,
            ctx: &ctx,
            digest: &digest,
            ledger: &ledger,
            open: Vec::new(),
            passive: None,
            live: Vec::new(),
            gateway_online: true,
            membership_paused: false,
        };
        let text = reply(Cmd::Status, &v);
        assert!(text.contains("osaka-2 备用 正在排空（非故障）"), "{text}");
        assert!(!text.contains("成员变更中"), "{text}");
        v.membership_paused = true;
        assert!(reply(Cmd::Status, &v).contains("成员变更中（暂停执行，非故障）"));
        let quota = reply(Cmd::Quota, &v);
        assert!(
            quota.contains("召回额度 未知（尚未收到管理员消息）"),
            "{quota}"
        );
    }
}
