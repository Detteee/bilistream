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
use super::ledger::{IncidentMark, NotifyLedger, ACTIVE_PER_DAY};
use super::observer::{ClusterDigest, NodeSlot};
use super::sink::{PassiveTarget, PASSIVE_MAX_REPLIES, PASSIVE_WINDOW_SECS};

/// At most one answer per this many seconds, and `MAX_PER_HOUR` an hour.
pub const MIN_GAP_SECS: u64 = 3;
pub const MAX_PER_HOUR: usize = 20;
const MAX_NODES: usize = 6;
const MAX_ENTRIES: usize = 5;
/// A row's subject (node or channel name) keeps the node column's budget.
const SUBJECT_COLS: usize = super::format::NODE_COLS;

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
    pub live: Vec<StatusChannel>,
    /// The resolved watch list. A channel's episode leaves one mark per linked
    /// account beside the channel's own, so the event reply needs the groups
    /// to name the channel once instead of repeating it per account.
    pub watch: &'a [super::live::ResolvedWatchChannel],
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
    let body = match cmd {
        Cmd::Status => status(view),
        Cmd::Event => events(view),
        Cmd::Quota => quota(view),
        Cmd::Help => help(),
    };
    super::format::with_sender(&body, &view.ctx.node_name(&view.ctx.reporter))
}

/// One live source in a status reply. `platforms` holds the raw platform
/// keys (`youtube`, `twitch`, `niconico`); an empty name means the only
/// available identity was a raw YouTube channel id.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StatusChannel {
    pub name: String,
    pub platforms: Vec<String>,
}

fn status(view: &View<'_>) -> String {
    let worst = view.open.iter().map(|entry| entry.severity).max();
    let headline = match worst {
        Some(Severity::Critical) => "🔴 有故障",
        Some(Severity::Warning) => "🟠 有警告",
        _ => "✅ 目前正常",
    };
    let mut lines = vec![headline.to_string()];
    lines.extend(cluster_summary(view));
    if let Some(line) = live_summary(view) {
        lines.push(line);
    }
    if !view.open.is_empty() {
        lines.push(format!("还有 {} 件没恢复：", view.open.len()));
        for entry in view.open.iter().take(MAX_ENTRIES) {
            lines.push(format!(
                "{} {} {}，已经 {}",
                marker(entry.severity),
                view.ctx.node_name(&entry.node),
                entry.kind.title(),
                duration_text(view.now.saturating_sub(entry.since))
            ));
        }
    }
    lines.push(notify_summary(view));
    cut(lines)
}

fn cluster_summary(view: &View<'_>) -> Vec<String> {
    let ctx = view.ctx;
    let digest = view.digest;
    if !digest.enabled {
        return vec!["这台机器单独运行".to_string()];
    }
    let mut lines = Vec::new();
    if view.membership_paused {
        lines.push("正在调整集群成员，转播先暂停，这不是故障".to_string());
    }
    let shown: Vec<&NodeSlot> = digest.nodes.iter().take(MAX_NODES).collect();
    let owner = digest.owner.as_deref();
    let calm = owner.is_some()
        && !view.membership_paused
        && shown
            .iter()
            .all(|node| node.healthy && !node.draining && node.fresh && !node.stale);
    if calm {
        let owner_name = ctx.node_name(owner.unwrap_or_default());
        let standbys: Vec<String> = shown
            .iter()
            .filter(|node| Some(node.id.as_str()) != owner)
            .map(|node| ctx.node_name(&node.id))
            .collect();
        lines.push(if standbys.is_empty() {
            format!("{owner_name} 正在转播")
        } else {
            format!("{owner_name} 正在转播，{} 待命", name_list(&standbys))
        });
    } else {
        lines.push(match owner {
            Some(owner) => format!("{} 正在转播", ctx.node_name(owner)),
            None => "还没有确定哪台在转播".to_string(),
        });
        for node in &shown {
            if owner == Some(node.id.as_str()) {
                continue;
            }
            lines.push(format!("{} {}", ctx.node_name(&node.id), node_state(node)));
        }
    }
    if digest.nodes.len() > MAX_NODES {
        lines.push(format!("还有 {} 台没列出", digest.nodes.len() - MAX_NODES));
    }
    lines
}

fn node_state(node: &NodeSlot) -> String {
    if node.draining {
        "正在排空，不是故障".to_string()
    } else if node.healthy {
        "待命".to_string()
    } else if node.stale || !node.fresh {
        format!("故障，{}", super::event::reason_text(&node.reason))
    } else {
        format!("备用，{}", super::event::reason_text(&node.reason))
    }
}

fn name_list(names: &[String]) -> String {
    match names {
        [] => String::new(),
        [one] => one.clone(),
        [one, two] => format!("{one} 和 {two}"),
        _ => {
            let (last, rest) = names.split_last().expect("at least three names");
            format!("{} 和 {last}", rest.join("、"))
        }
    }
}

fn live_summary(view: &View<'_>) -> Option<String> {
    if view.live.is_empty() {
        return None;
    }
    let shown: Vec<String> = view
        .live
        .iter()
        .take(3)
        .map(|channel| channel_phrase(channel, view.ctx))
        .collect();
    let mut line = format!("正在直播：{}", shown.join("、"));
    if view.live.len() > 3 {
        line.push_str(&format!("，还有 {} 个", view.live.len() - 3));
    }
    Some(line)
}

fn channel_phrase(channel: &StatusChannel, ctx: &RenderContext) -> String {
    let labels = channel
        .platforms
        .iter()
        .map(|platform| platform_label(platform))
        .collect::<Vec<_>>()
        .join("、");
    let name = channel.name.trim();
    if name.is_empty() {
        if channel.platforms.len() == 1 {
            format!("一个 {labels} 频道")
        } else {
            format!("一个频道（{labels}）")
        }
    } else {
        format!("{}（{labels}）", ctx.value(name, 24))
    }
}

fn platform_label(platform: &str) -> &'static str {
    match platform.to_ascii_lowercase().as_str() {
        "youtube" | "yt" => "YouTube",
        "twitch" | "tw" => "Twitch",
        "niconico" | "nico" => "Niconico",
        _ => "其他",
    }
}

fn notify_summary(view: &View<'_>) -> String {
    let gateway = if view.gateway_online {
        "通知网关在线"
    } else {
        "通知网关离线"
    };
    let quota = match view.ledger.wakeups_left(view.now) {
        Some(left) => format!("主动召回还剩 {left} 次"),
        None => "主动召回次数还不确定".to_string(),
    };
    let mut line = format!("{gateway}，{quota}");
    let pending = view.ledger.undelivered_pending();
    if pending > 0 {
        line.push_str(&format!("，还有 {pending} 条没送达"));
    }
    line
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
    let recent = recent_entries(view);
    if !recent.is_empty() {
        lines.push("最近 24 小时已发送：".to_string());
        for entry in recent.iter().take(MAX_ENTRIES) {
            let mut line = format!(
                "· {} {} · {} 发送",
                entry.title,
                entry.subject,
                short_time(entry.at, view.now)
            );
            if let Some(at) = entry.resolved_at {
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

/// One line of 最近 24 小时已发送. A watched channel's episode leaves a mark for
/// the channel plus one per linked account, so those marks are one line: the
/// channel's name and the newest notification time of the group.
struct RecentEntry {
    /// The watched channel's ledger key, or the mark's own key for everything
    /// else, so the marks of one episode meet here.
    group: String,
    title: &'static str,
    subject: String,
    at: u64,
    resolved_at: Option<u64>,
}

fn recent_entries(view: &View<'_>) -> Vec<RecentEntry> {
    let channels: Vec<(String, &str, Vec<String>)> = view
        .watch
        .iter()
        .map(|channel| (channel.key(), channel.name.as_str(), channel.episode_keys()))
        .collect();
    let mut marks: Vec<&IncidentMark> = view
        .ledger
        .incidents
        .iter()
        .filter(|mark| view.now.saturating_sub(mark.notified_at) <= 86_400)
        .collect();
    marks.sort_by_key(|mark| std::cmp::Reverse(mark.notified_at));
    let mut entries: Vec<RecentEntry> = Vec::new();
    for mark in marks {
        let mut parts = mark.key.splitn(3, ':');
        let kind = parts.next().and_then(EventKind::from_key);
        let node = parts.next().unwrap_or_default();
        let variant = parts.next().unwrap_or_default();
        let resolved_at = mark.resolved_at.filter(|at| *at >= mark.notified_at);
        let watched = channels
            .iter()
            .find(|(_, _, keys)| keys.iter().any(|key| key == &mark.key));
        let (group, subject) = match (kind, watched) {
            (Some(EventKind::StreamLive), Some((group, name, _))) => {
                (group.clone(), view.ctx.value(name, SUBJECT_COLS))
            }
            (Some(EventKind::StreamLive), None) => {
                (mark.key.clone(), view.ctx.value(variant, SUBJECT_COLS))
            }
            _ => (mark.key.clone(), view.ctx.node_name(node)),
        };
        let title = kind.map_or("通知", |kind| kind.title());
        match entries.iter_mut().find(|entry| entry.group == group) {
            Some(entry) => {
                if mark.notified_at > entry.at {
                    entry.at = mark.notified_at;
                    entry.resolved_at = resolved_at;
                } else if mark.notified_at == entry.at {
                    // The episode's marks resolve together; keep the latest.
                    entry.resolved_at = entry.resolved_at.max(resolved_at);
                }
            }
            None => entries.push(RecentEntry {
                group,
                title,
                subject,
                at: mark.notified_at,
                resolved_at,
            }),
        }
    }
    entries
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
            live: vec![StatusChannel {
                name: "主频道".into(),
                platforms: vec!["youtube".into()],
            }],
            watch: &[],
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
        assert!(status.starts_with("🟠 有警告"), "{status}");
        assert!(status.contains("osaka-2 正在转播"));
        assert!(status.contains("sg-3 备用，网络不稳定"));
        assert!(status.contains("正在直播：主频道（YouTube）"));
        assert!(status.contains("🟠 sg-3 节点降级，已经 41 分钟"));
        assert!(!status.contains("UC-"));
        assert!(!status.contains("回复 help"));
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
            let text = reply(cmd, &v);
            assert!(text.chars().count() <= MAX_CHARS);
            assert!(text.ends_with("发送节点：osaka-2"));
            assert_eq!(text.matches("发送节点：").count(), 1);
        }
        assert_eq!(ledger, before);
    }

    #[test]
    fn one_live_episode_is_one_event_line() {
        // 2026-10-22: Kamito (YouTube + Twitch) and 橘ひなの (YouTube) each went
        // live once, but the ledger holds a mark per linked account beside the
        // channel mark, so the reply listed one episode three times and twice.
        let now = 1_791_702_240;
        let ctx = RenderContext {
            reporter: "jp".into(),
            ..Default::default()
        };
        let digest = ClusterDigest {
            enabled: true,
            owner: Some("jp".into()),
            nodes: vec![NodeSlot {
                id: "jp".into(),
                healthy: true,
                fresh: true,
                ..Default::default()
            }],
            ..Default::default()
        };
        let watch = vec![
            super::super::live::ResolvedWatchChannel {
                kind: "configured",
                name: "Kamito".into(),
                accounts: vec![
                    ("youtube", "UC_kamito".into()),
                    ("twitch", "kamito_jp".into()),
                ],
            },
            super::super::live::ResolvedWatchChannel {
                kind: "configured",
                name: "橘ひなの".into(),
                accounts: vec![("youtube", "UC_hinano".into())],
            },
        ];
        let mut ledger = NotifyLedger::default();
        for key in watch.iter().flat_map(|channel| channel.episode_keys()) {
            ledger.record_notified(&key, now - 3_600);
        }
        let mut v = view(&ctx, &digest, &ledger, None);
        v.open.clear();
        v.watch = &watch;
        let text = reply(Cmd::Event, &v);
        assert_eq!(text.matches("· 开播 Kamito · ").count(), 1, "{text}");
        assert_eq!(text.matches("· 开播 橘ひなの · ").count(), 1, "{text}");
        assert!(!text.contains("UC_"), "{text}");
        assert!(!text.contains("kamito_jp"), "{text}");
        assert!(!text.contains("configured:"), "{text}");
        assert_eq!(text.matches("· 开播 ").count(), 2, "{text}");
    }

    #[test]
    fn a_calm_status_names_live_channels_and_skips_raw_ids() {
        let mut names = std::collections::HashMap::new();
        names.insert("ca".into(), "CA".into());
        names.insert("jp".into(), "JP".into());
        names.insert("ny".into(), "US".into());
        let ctx = RenderContext {
            reporter: "ca".into(),
            names,
            ..Default::default()
        };
        let digest = ClusterDigest {
            enabled: true,
            owner: Some("ca".into()),
            nodes: vec![
                NodeSlot {
                    id: "ny".into(),
                    healthy: true,
                    fresh: true,
                    ..Default::default()
                },
                NodeSlot {
                    id: "ca".into(),
                    healthy: true,
                    fresh: true,
                    ..Default::default()
                },
                NodeSlot {
                    id: "jp".into(),
                    healthy: true,
                    fresh: true,
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let ledger = NotifyLedger::default();
        let mut status_view = view(&ctx, &digest, &ledger, None);
        status_view.open.clear();
        status_view.live = vec![
            StatusChannel {
                name: "紡木こかげ".into(),
                platforms: vec!["youtube".into()],
            },
            StatusChannel {
                name: "夢野あかり".into(),
                platforms: vec!["twitch".into()],
            },
            StatusChannel {
                name: String::new(),
                platforms: vec!["youtube".into()],
            },
        ];
        let status = reply(Cmd::Status, &status_view);
        assert!(status.starts_with("✅ 目前正常"), "{status}");
        assert!(status.contains("CA 正在转播，US 和 JP 待命"), "{status}");
        assert!(
            status.contains(
                "正在直播：紡木こかげ（YouTube）、夢野あかり（Twitch）、一个 YouTube 频道"
            ),
            "{status}"
        );
        assert!(!status.contains("UC-"), "{status}");
        assert!(!status.contains("节点 3/3"), "{status}");
        assert!(!status.contains("未送达"), "{status}");
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
            watch: &[],
            gateway_online: true,
            membership_paused: false,
        };
        let text = reply(Cmd::Status, &v);
        assert!(text.contains("osaka-2 正在排空，不是故障"), "{text}");
        assert!(!text.contains("成员变更中"), "{text}");
        v.membership_paused = true;
        assert!(reply(Cmd::Status, &v).contains("正在调整集群成员，转播先暂停，这不是故障"));
        let quota = reply(Cmd::Quota, &v);
        assert!(
            quota.contains("召回额度 未知（尚未收到管理员消息）"),
            "{quota}"
        );
    }
}
