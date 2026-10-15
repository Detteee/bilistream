//! Notification-copy addendum: natural events, truthful recovery and sender.

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
        "🔴 sg-3 已失去连接\n心跳超时。异常已持续 1 分钟。\n发送节点：tokyo-1"
    );
    let mut md = ctx();
    md.markdown = true;
    let text = render(
        &[item(EventKind::NodeDown, "sg-3", "heartbeat_timeout")],
        &md,
    );
    assert!(
        text.starts_with("**🔴 sg-3 已失去连接**\n- 心跳超时。"),
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
    assert!(text.starts_with("🔴 大阪二号机 已失去连接"), "{text}");
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
        single.starts_with("🔴 转播节点已从 tokyo-1 切换到 osaka-2\n"),
        "{single}"
    );
    assert!(single.contains("tokyo-1 心跳超时。"));
    assert!(single.ends_with("发送节点：osaka-2"));

    let mut down = item(EventKind::NodeDown, "tokyo-1", "heartbeat_timeout");
    down.incident_at = NOW - 120;
    let degraded = item(EventKind::NodeDegraded, "sg-3", "network_unstable");
    let text = render(&[failover, down, degraded], &c);
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines[0], "🔴 转播节点已从 tokyo-1 切换到 osaka-2");
    assert_eq!(lines[1], "最早发生于 2026-10-11 15:02 (UTC+8)");
    assert_eq!(lines[2], "tokyo-1 已失去连接（心跳超时）。");
    assert_eq!(lines[3], "· 🟠 sg-3 运行异常：网络不稳定");
    assert_eq!(text.matches("心跳超时").count(), 1);
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
    assert!(render(&same, &ctx()).starts_with("🔴 sg-3 有 2 项异常"));
}

#[test]
fn the_500_char_cap_keeps_first_delayed_time_sender_and_undelivered_lines() {
    let mut items = Vec::new();
    for i in 0..5 {
        let mut it = item(EventKind::NodeDegraded, &format!("node-{i}"), "");
        it.detail = Some("x".repeat(200));
        it.queued_at = NOW - 600;
        it.incident_at = NOW - 660;
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
    assert!(lines[1].starts_with("发生于 "));
    assert_eq!(
        lines[lines.len() - 2],
        "另有 3 条通知未送达（主动消息无权限）"
    );
    assert_eq!(*lines.last().unwrap(), "发送节点：tokyo-1");

    // A single huge reason: the bracketed raw text goes first, then a hard
    // cut of the middle; the fixed lines survive.
    let mut one = item(EventKind::NodeDegraded, "sg-3", "");
    one.detail = Some("错".repeat(400));
    one.queued_at = NOW - 600;
    one.incident_at = NOW - 660;
    let text = render(&[one], &c);
    assert!(text.chars().count() <= MAX_CHARS);
    assert!(text.starts_with("🟠 sg-3 运行异常\n发生于 "));
    assert!(text.ends_with("另有 3 条通知未送达（主动消息无权限）\n发送节点：tokyo-1"));
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
        text.starts_with("（无多数派）🔴 osaka-2 无法连接多数节点"),
        "{text}"
    );
    assert!(text.contains("目前无法确认其他节点的状态"));
    assert!(text.ends_with("\n发送节点：osaka-2"));

    let mut resolved = item(EventKind::NodeDown, "sg-3", "");
    resolved.incident_at = NOW - 480;
    resolved.item = ItemKind::Resolved { duration_secs: 480 };
    resolved.suppressed = 2;
    let text = render(&[resolved], &ctx());
    assert!(text.starts_with("✅ sg-3 已恢复连接\n本次异常持续 8 分钟，期间重复 2 次"));
    assert!(text.contains("当前转播节点仍为 tokyo-1"));
    assert!(!text.contains("恢复转播"));

    let mut flap = item(EventKind::NodeDown, "sg-3", "");
    flap.flapping = Some(3);
    assert!(
        render(&[flap], &ctx()).starts_with("🟠 sg-3 的状态反复变化\n10 分钟内已出现 3 次异常。")
    );

    let latched = item(EventKind::NodeDegraded, "sg-3", "network_isolated");
    assert!(render(&[latched], &ctx()).starts_with("🟠 sg-3 已暂停转播\n网络隔离。"));
}

#[test]
fn held_resolved_items_ride_along() {
    let mut held = item(EventKind::NodeDown, "tokyo-1", "");
    held.item = ItemKind::Resolved { duration_secs: 480 };
    held.incident_at = NOW - 1080;
    held.queued_at = NOW - 570; // queued after the hold; recovery itself was 14:54
    let mut c = ctx();
    c.held = vec![held];
    let text = render(
        &[item(EventKind::NodeDegraded, "sg-3", "network_unstable")],
        &c,
    );
    assert!(
        text.contains("· ✅ tokyo-1 已恢复连接（异常持续 8 分钟，14:54 恢复）"),
        "{text}"
    );
}

#[test]
fn go_live_and_test_messages() {
    let mut live = item(EventKind::StreamLive, "youtube", "UCabc");
    live.extra.live = Some(LiveInfo {
        platforms: vec!["youtube".into(), "twitch".into()],
        name: "主频道".into(),
        title: Some("【歌枠】Singing https://youtu.be/fixture0001".into()),
        ..Default::default()
    });
    let text = render(&[live], &ctx());
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines[0], "🟢 主频道 开播了");
    assert_eq!(
        lines[1],
        "YouTube、Twitch · 【歌枠】Singing youtu[.]be/fixture0001"
    );
    assert_eq!(lines[2], "发送节点：tokyo-1");
    let test = render(&[item(EventKind::Test, "tokyo-1", "")], &ctx());
    assert!(test.starts_with("🧪 通知测试\n这是一条 bilistream 测试消息"));
    assert!(test.ends_with("发送节点：tokyo-1"));
}

#[test]
fn cta_lines_follow_the_wakeup_cycle_and_only_when_they_fit() {
    let mut ledger = NotifyLedger::default();
    ledger.record_admin_message(NOW - 3600);
    let cta = wakeup_cta(&ledger, NOW);
    assert!(cta.starts_with("召回消息还剩 3 次（下次 "), "{cta}");
    // Last period of the cycle: the last reminder.
    let late = NOW - 3600 + 8 * 86_400;
    assert!(wakeup_cta(&ledger, late).starts_with("这是本周期最后一次召回消息"));
    assert_eq!(with_cta("a", Some("b"), false), "a\nb");
    assert_eq!(with_cta("a", Some("b"), true), "a\n- b");
    assert_eq!(
        with_cta("notice\n发送节点：a", Some("reply"), false),
        "notice\nreply\n发送节点：a"
    );
    assert_eq!(
        with_cta("**notice**\n- 发送节点：a", Some("reply"), true),
        "**notice**\n- reply\n- 发送节点：a"
    );
    let long = "x".repeat(495);
    assert_eq!(with_cta(&long, Some("回复任意消息"), false), long);
    assert_eq!(
        last_reply_cta(),
        "本轮可回复次数已用完。回复任意消息即可恢复正常推送。"
    );
}

#[test]
fn minimal_keeps_the_undelivered_count() {
    let mut c = ctx();
    c.undelivered = 2;
    let text = minimal(&[item(EventKind::NodeDown, "a", "")], &c);
    assert!(text.contains("严重 1 条"));
    assert!(text.contains("另有 2 条通知未送达"));
    assert!(text.ends_with("发送节点：tokyo-1"));
}

#[test]
fn eligible_senders_share_one_footer_without_a_delegation_label() {
    let mut c = ctx();
    c.active_owner = Some("ca".into());
    c.names.insert("tokyo-1".into(), "日本节点".into());
    let one = item(EventKind::NodeDegraded, "ca", "ffmpeg_repeated_failures");
    let primary = render(std::slice::from_ref(&one), &c);
    c.label = Some("（代发）".into()); // older callers cannot reintroduce it
    let delegate = render(&[one], &c);
    assert_eq!(primary, delegate);
    assert!(!delegate.contains("代发"));
    assert_eq!(delegate.matches("发送节点：").count(), 1);
    assert!(delegate.ends_with("发送节点：日本节点"));
}

#[test]
fn failover_and_isolation_explain_the_shared_cause_once() {
    let mut failover = item(EventKind::Failover, "b", "a");
    failover.extra.old_owner = Some("a".into());
    failover.detail = Some("ffmpeg 反复失败".into());
    let isolation = item(EventKind::NodeDegraded, "a", "ffmpeg_repeated_failures");
    let text = render(&[failover, isolation], &ctx());
    assert!(text.starts_with("🔴 转播节点已从 a 切换到 b"));
    assert!(text.contains("a 因ffmpeg 反复失败暂停了转播。"));
    assert_eq!(text.matches("ffmpeg 反复失败").count(), 1);
    assert_eq!(text.matches("切换到").count(), 1);
}

#[test]
fn delayed_recovery_reports_the_clear_time_not_the_confirmation_time() {
    let mut resolved = item(EventKind::NodeDown, "a", "");
    resolved.incident_at = NOW - 1200;
    resolved.item = ItemKind::Resolved { duration_secs: 480 };
    resolved.queued_at = NOW - 690;
    let text = render(&[resolved], &ctx());
    assert!(text.contains("恢复于 2026-10-11 14:52 (UTC+8)"));
    assert!(!text.contains("全部恢复"));
}

#[test]
fn a_long_hostile_message_keeps_a_safe_sender_footer() {
    let sender = "<qqbot-at-user/>\nhttps://example.com secret=super-secret";
    let text = with_sender(&"字".repeat(600), sender);
    assert!(text.chars().count() <= MAX_CHARS);
    assert_eq!(text.lines().count(), 2);
    assert!(text.lines().last().unwrap().starts_with("发送节点："));
    assert!(!text.contains("<qqbot-at-user") && !text.contains("https://"));
    assert!(!text.contains("super-secret"));
}

#[test]
fn zero_width_names_do_not_overflow_the_sender_or_message_budget() {
    let name = format!("a{}", "\u{0301}".repeat(1000));
    let mut c = ctx();
    c.names.insert("tokyo-1".into(), name.clone());
    c.names.insert("a".into(), name.clone());
    let rendered = render(&[item(EventKind::NodeDown, "a", "heartbeat_timeout")], &c);
    for text in [rendered, with_sender(&"body".repeat(500), &name)] {
        assert!(text.chars().count() <= MAX_CHARS);
        let footer = text.lines().last().unwrap();
        assert!(footer.starts_with("发送节点："));
        assert!(footer.chars().count() <= NODE_COLS + 5);
    }
}

#[test]
fn delinking_does_not_hide_stream_or_proxy_credentials_from_redaction() {
    for raw in [
        "rtmp://push.invalid/live/synthetic-rtmp-key",
        "http://user:synthetic-proxy-password@proxy.invalid:8080",
        "https://media.invalid/synthetic-path?token=synthetic-token",
        "rtmp: / /malformed/synthetic-head\nsynthetic-tail Broken pipe",
    ] {
        let text = sanitize(raw, 160);
        assert!(!text.contains("synthetic-"), "{text}");
        assert!(!text.contains("://"), "{text}");
        assert_eq!(text.lines().count(), 1);
    }
}
