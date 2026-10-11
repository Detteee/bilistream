//! Format tests against the alert format spec (§3–§6, §10, §11 examples).

use super::*;
use crate::notify::event::{dedup_key, ItemExtra, LiveInfo};

// 2026-10-11 15:04:00 UTC+8
const NOW: u64 = 1_791_702_240;

fn item(kind: EventKind, node: &str, variant: &str) -> OutItem {
    OutItem {
        key: dedup_key(kind, node, variant),
        kind,
        item: ItemKind::Alert,
        node: node.into(),
        detail: None,
        incident_at: NOW - 60,
        queued_at: NOW,
        not_before: NOW,
        suppressed: 0,
        flapping: None,
        extra: ItemExtra::default(),
    }
}

fn ctx() -> RenderContext {
    RenderContext {
        now: NOW,
        reporter: "tokyo-1".into(),
        active_owner: Some("tokyo-1".into()),
        ..Default::default()
    }
}

#[test]
fn example_1_node_down() {
    let text = render(
        &[item(EventKind::NodeDown, "sg-3", "heartbeat_timeout")],
        &ctx(),
    );
    assert_eq!(
        text,
        "🔴 节点故障 · sg-3\n时间 2026-10-11 15:04 (UTC+8)\n原因 心跳超时（已持续 1 分钟）\n当前活跃节点 tokyo-1 · 报告 tokyo-1"
    );
    let mut md = ctx();
    md.markdown = true;
    let text = render(
        &[item(EventKind::NodeDown, "sg-3", "heartbeat_timeout")],
        &md,
    );
    assert!(
        text.starts_with("**🔴 节点故障 · sg-3**\n- 时间 "),
        "{text}"
    );
}

#[test]
fn display_name_prefers_the_configured_name_and_is_cut_to_24_columns() {
    let mut c = ctx();
    c.names.insert("n1".into(), "大阪二号机".into());
    c.names.insert("n2".into(), "  ".into());
    c.names
        .insert("n3".into(), "a-very-long-node-name-that-keeps-going".into());
    assert_eq!(c.node_name("n1"), "大阪二号机");
    assert_eq!(c.node_name("n2"), "n2");
    assert!(UnicodeWidthStr::width(c.node_name("n3").as_str()) <= NODE_COLS);
    let text = render(&[item(EventKind::NodeDown, "n1", "")], &c);
    assert!(text.starts_with("🔴 节点故障 · 大阪二号机"), "{text}");
}

#[test]
fn failover_summary_title_and_single_message() {
    let mut failover = item(EventKind::Failover, "osaka-2", "tokyo-1");
    failover.extra.old_owner = Some("tokyo-1".into());
    failover.detail = Some("心跳超时".into());
    let mut c = ctx();
    c.active_owner = Some("osaka-2".into());
    c.reporter = "osaka-2".into();
    let single = render(std::slice::from_ref(&failover), &c);
    assert!(
        single.starts_with("🔴 故障转移 · tokyo-1 → osaka-2\n"),
        "{single}"
    );
    assert!(single.contains("原因 tokyo-1 心跳超时"));
    assert!(single.contains("推流已由 osaka-2 接管"));

    let mut down = item(EventKind::NodeDown, "tokyo-1", "heartbeat_timeout");
    down.incident_at = NOW - 120;
    let degraded = item(EventKind::NodeDegraded, "sg-3", "network_unstable");
    let text = render(&[failover, down, degraded], &c);
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines[0], "🔴 故障转移 · tokyo-1 → osaka-2");
    assert_eq!(lines[1], "时间 2026-10-11 15:04 (UTC+8) · 开始于 15:02");
    assert_eq!(lines[3], "· 🔴 节点故障 tokyo-1 · 心跳超时");
    assert_eq!(lines[4], "· 🟠 节点降级 sg-3 · 网络不稳定");
}

#[test]
fn summary_shows_at_most_five_entries_worst_first() {
    let items: Vec<OutItem> = (0..7)
        .map(|i| item(EventKind::NodeDown, &format!("n{i}"), ""))
        .collect();
    let text = render(&items, &ctx());
    assert!(text.starts_with("🔴 集群异常（7 条）"));
    assert_eq!(text.lines().filter(|l| l.starts_with("· ")).count(), 5);
    assert!(text.contains("另有 2 条"));
    let same: Vec<OutItem> = vec![
        item(EventKind::NodeDown, "sg-3", ""),
        item(EventKind::SelfCheckFailing, "sg-3", ""),
    ];
    assert!(render(&same, &ctx()).starts_with("🔴 节点故障 · sg-3（2 项）"));
}

#[test]
fn the_500_char_cap_keeps_first_time_and_undelivered_lines() {
    let mut items = Vec::new();
    for i in 0..5 {
        let mut it = item(EventKind::NodeDegraded, &format!("node-{i}"), "");
        it.detail = Some("x".repeat(200));
        items.push(it);
    }
    let mut c = ctx();
    c.undelivered = 3;
    c.undelivered_reason = Some("主动消息无权限".into());
    let text = render(&items, &c);
    assert!(
        text.chars().count() <= MAX_CHARS,
        "{}",
        text.chars().count()
    );
    let lines: Vec<&str> = text.lines().collect();
    assert!(lines[0].starts_with("🟠 集群异常"));
    assert!(lines[1].starts_with("时间 "));
    assert_eq!(
        *lines.last().unwrap(),
        "另有 3 条通知未送达（主动消息无权限）"
    );

    // A single huge reason: the bracketed raw text goes first, then a hard
    // cut of the middle; the fixed lines survive.
    let mut one = item(EventKind::NodeDegraded, "sg-3", "");
    one.detail = Some("错".repeat(400));
    let text = render(&[one], &c);
    assert!(text.chars().count() <= MAX_CHARS);
    assert!(text.starts_with("🟠 节点降级 · sg-3\n时间 "));
    assert!(text.ends_with("另有 3 条通知未送达（主动消息无权限）"));
}

#[test]
fn long_errors_are_cut_in_the_middle() {
    let raw = format!("{} (exit status: 1)", "a".repeat(300));
    let detail = clean_detail(&raw);
    assert!(detail.ends_with("(exit status: 1)"), "{detail}");
    assert!(detail.contains('…'));
    assert!(UnicodeWidthStr::width(detail.as_str()) <= 141);
    assert_eq!(
        clean_detail("open  /usr/local/bin/ffmpeg   failed"),
        "open ffmpeg failed"
    );
}

#[test]
fn links_are_delinked_and_tags_neutralised() {
    assert_eq!(
        delink("GET https://api.live.bilibili.com/x failed"),
        "GET api[.]live[.]bilibili[.]com/x failed"
    );
    assert_eq!(delink("peer 10.0.0.5 down"), "peer 10[.]0[.]0[.]5 down");
    assert_eq!(delink("ffmpeg 6.1 exit 1.5"), "ffmpeg 6.1 exit 1.5");
    assert_eq!(sanitize("<@all> hi", 40), "＜@all＞ hi");
    let mut it = item(EventKind::NodeDegraded, "sg-3", "");
    it.detail = Some("see example.com <qqbot-at-user/>".into());
    let text = render(&[it], &ctx());
    assert!(text.contains("example[.]com ＜qqbot-at-user/＞"), "{text}");
    assert!(!text.contains("example.com"));
}

#[test]
fn labels_resolved_flapping_and_latched_titles() {
    let mut c = ctx();
    c.label = Some("无多数派".into());
    c.active_owner = None;
    c.reporter = "osaka-2".into();
    let text = render(&[item(EventKind::QuorumLost, "osaka-2", "")], &c);
    assert!(
        text.starts_with("（无多数派）🔴 本节点失去多数派 · osaka-2"),
        "{text}"
    );
    assert!(text.contains("\n报告 osaka-2"));

    let mut resolved = item(EventKind::NodeDown, "sg-3", "");
    resolved.item = ItemKind::Resolved { duration_secs: 480 };
    resolved.suppressed = 2;
    assert!(render(&[resolved], &ctx())
        .starts_with("✅ 已恢复 · 节点故障 · sg-3（持续 8 分钟，期间重复 2 次）"));

    let mut flap = item(EventKind::NodeDown, "sg-3", "");
    flap.flapping = Some(3);
    assert!(render(&[flap], &ctx())
        .starts_with("🟠 节点状态反复 · 节点故障 · sg-3（10 分钟内出现 3 次）"));

    let latched = item(EventKind::NodeDegraded, "sg-3", "network_isolated");
    assert!(render(&[latched], &ctx()).starts_with("🟠 节点自我隔离 · sg-3"));
}

#[test]
fn held_resolved_items_ride_along() {
    let mut held = item(EventKind::NodeDown, "tokyo-1", "");
    held.item = ItemKind::Resolved { duration_secs: 480 };
    held.queued_at = NOW - 600; // 14:54
    let mut c = ctx();
    c.held = vec![held];
    let text = render(
        &[item(EventKind::NodeDegraded, "sg-3", "network_unstable")],
        &c,
    );
    assert!(
        text.contains("· ✅ 节点故障 tokyo-1 已恢复（8 分钟，14:54 恢复）"),
        "{text}"
    );
}

#[test]
fn go_live_and_test_messages() {
    let mut live = item(EventKind::StreamLive, "youtube", "UCabc");
    live.extra.live = Some(LiveInfo {
        platform: "youtube".into(),
        name: "主频道".into(),
        title: Some("【歌枠】Singing https://youtu.be/x".into()),
    });
    let text = render(&[live], &ctx());
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines[0], "🟢 开播 · 主频道");
    assert_eq!(lines[2], "平台 YouTube");
    assert_eq!(lines[3], "标题 【歌枠】Singing youtu[.]be/x");
    let test = render(&[item(EventKind::Test, "tokyo-1", "")], &ctx());
    assert!(test.starts_with("🧪 通知测试 · tokyo-1\n时间 "));
    assert!(test.ends_with("发送节点 tokyo-1 · 当前活跃节点 tokyo-1"));
}

#[test]
fn cta_lines_follow_the_wakeup_cycle_and_only_when_they_fit() {
    let mut ledger = NotifyLedger::default();
    ledger.record_admin_message(NOW - 3600);
    let cta = wakeup_cta(&ledger, NOW);
    assert!(cta.starts_with("召回提醒还剩 3 次（下次 "), "{cta}");
    // Last period of the cycle: the last reminder.
    let late = NOW - 3600 + 8 * 86_400;
    assert!(wakeup_cta(&ledger, late).starts_with("这是本周期最后一次提醒"));
    assert_eq!(with_cta("a", Some("b"), false), "a\nb");
    assert_eq!(with_cta("a", Some("b"), true), "a\n- b");
    let long = "x".repeat(495);
    assert_eq!(with_cta(&long, Some("回复任意消息"), false), long);
    assert!(last_reply_cta().contains("再发一条消息"));
}

#[test]
fn minimal_keeps_the_undelivered_count() {
    let text = minimal(&[item(EventKind::NodeDown, "a", "")], 2);
    assert!(text.contains("严重 1 条"));
    assert!(text.contains("另有 2 条通知未送达"));
}
