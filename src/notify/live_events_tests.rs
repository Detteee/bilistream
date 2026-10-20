//! Addendum A/D/E rev 3 gate tests: send ladder, delivery records, crashes,
//! failover, fallback codes, ambiguous-retry limit, no leaks.

use super::*;
use crate::events::BroadcastEndCause;
use std::collections::VecDeque;
use std::sync::Mutex;

const NOW: u64 = 1_800_000_000;
const MSG_ID: &str = "ROBOT1.0_SAMPLEmsgid0000000000000000";

fn ok() -> ApiResult {
    ApiResult {
        ok: true,
        http_status: 200,
        code: None,
        trace_id: None,
    }
}
fn code(code: i64) -> ApiResult {
    ApiResult {
        ok: false,
        http_status: 400,
        code: Some(code),
        trace_id: None,
    }
}
fn network() -> ApiResult {
    ApiResult {
        ok: false,
        http_status: 0,
        code: None,
        trace_id: None,
    }
}
fn server_error() -> ApiResult {
    ApiResult {
        ok: false,
        http_status: 502,
        code: None,
        trace_id: None,
    }
}

#[derive(Default)]
struct FakeQq {
    replies: Mutex<VecDeque<ApiResult>>,
    /// `(mode, msg_id, text)` per request.
    sent: Mutex<Vec<(&'static str, Option<String>, String)>>,
}

impl FakeQq {
    fn script(replies: impl IntoIterator<Item = ApiResult>) -> Self {
        let fake = FakeQq::default();
        fake.replies.lock().unwrap().extend(replies);
        fake
    }
    fn modes(&self) -> Vec<&'static str> {
        self.sent.lock().unwrap().iter().map(|s| s.0).collect()
    }
    fn count(&self) -> usize {
        self.sent.lock().unwrap().len()
    }
    fn text(&self, index: usize) -> String {
        self.sent.lock().unwrap()[index].2.clone()
    }
    fn msg_id(&self, index: usize) -> Option<String> {
        self.sent.lock().unwrap()[index].1.clone()
    }
}

#[async_trait::async_trait]
impl LiveTransport for FakeQq {
    async fn send(&self, request: LiveSend<'_>) -> ApiResult {
        self.sent.lock().unwrap().push((
            request.mode.as_str(),
            request.msg_id.map(str::to_string),
            request.text.to_string(),
        ));
        self.replies.lock().unwrap().pop_front().unwrap_or_else(ok)
    }
}

/// Synced state as committed. `fail` holds 1-based commit numbers that
/// fail; `land_then_fail` ones land first (a crash right after the write).
#[derive(Default)]
struct FakeCommit {
    committed: Mutex<Vec<NotifyLedger>>,
    calls: Mutex<usize>,
    fail: Vec<usize>,
    land_then_fail: Vec<usize>,
}

impl FakeCommit {
    fn last(&self) -> NotifyLedger {
        self.committed
            .lock()
            .unwrap()
            .last()
            .cloned()
            .unwrap_or_default()
    }
    fn snapshots(&self) -> Vec<NotifyLedger> {
        self.committed.lock().unwrap().clone()
    }
}

#[async_trait::async_trait]
impl LiveCommit for FakeCommit {
    async fn commit(&self, ledger: &NotifyLedger) -> io::Result<()> {
        let call = {
            let mut calls = self.calls.lock().unwrap();
            *calls += 1;
            *calls
        };
        if self.fail.contains(&call) {
            return Err(io::Error::other("disk full"));
        }
        self.committed.lock().unwrap().push(ledger.clone());
        if self.land_then_fail.contains(&call) {
            return Err(io::Error::other("crash"));
        }
        Ok(())
    }
}

fn info(key: &str) -> LiveSource {
    LiveSource {
        stream_key: key.into(),
        platform: "YouTube".into(),
        channel: "某频道".into(),
        title: "周末杂谈".into(),
    }
}

fn started() -> NotifyLedger {
    let mut ledger = NotifyLedger {
        local: "a".into(),
        ..Default::default()
    };
    ledger.live.source(&info("YT:UC1"), NOW);
    ledger
}

fn admin_window(ledger: &mut NotifyLedger, at: u64) -> PassiveTarget {
    ledger.record_admin_message(at);
    PassiveTarget {
        msg_id: MSG_ID.into(),
        received_at: at,
        replies_used: 0,
    }
}

async fn run(
    ledger: &mut NotifyLedger,
    passive: Option<&mut PassiveTarget>,
    term: Option<u64>,
    now: u64,
    qq: &FakeQq,
    commit: &FakeCommit,
) -> DriveReport {
    let node = ledger.local.clone();
    let mut driver = Driver {
        ledger,
        passive,
        term,
        node: &node,
        sender_name: &node,
        enabled: true,
        now,
    };
    drive(&mut driver, qq, commit).await
}

fn state(ledger: &NotifyLedger, key: LiveEventKey) -> Option<DeliveryState> {
    ledger.live.delivery.get(&key).map(|record| record.state)
}

#[tokio::test]
async fn active_first_then_never_again_for_the_same_generation() {
    let mut ledger = started();
    let qq = FakeQq::default();
    let commit = FakeCommit::default();
    let report = run(&mut ledger, None, Some(1), NOW, &qq, &commit).await;
    assert_eq!(
        report.delivered,
        vec![(LiveKind::LiveStart, Channel::Active)]
    );
    assert_eq!(qq.modes(), vec!["active"]);
    // Plain active: no msg_id, no is_wakeup.
    assert_eq!(qq.msg_id(0), None);
    assert_eq!(
        qq.text(0),
        "🟢 开始转播 某频道\nYouTube · 周末杂谈\n发送节点：a"
    );
    // The sending record was committed before the request, delivered after.
    let committed = commit.snapshots();
    assert_eq!(
        state(&committed[0], LiveEventKey::start()),
        Some(DeliveryState::Sending)
    );
    assert_eq!(committed[0].live.delivery[&LiveEventKey::start()].term, 1);
    assert_eq!(
        state(&committed[1], LiveEventKey::start()),
        Some(DeliveryState::Delivered)
    );
    // Restart re-calling start-live keeps the counter: nothing new is due.
    assert_eq!(
        ledger.live.source(&info("YT:UC1"), NOW + 60),
        SourceOutcome::Same(1)
    );
    run(&mut ledger, None, Some(2), NOW + 61, &qq, &commit).await;
    assert_eq!(qq.count(), 1);
    // 下播 once, then the next broadcast clears the records.
    assert!(ledger
        .live
        .end(NOW + 3 * 3600 + 12 * 60, BroadcastEndCause::SourceEnded));
    assert!(!ledger
        .live
        .end(NOW + 4 * 3600, BroadcastEndCause::SourceEnded));
    run(&mut ledger, None, Some(2), NOW + 4 * 3600, &qq, &commit).await;
    assert_eq!(qq.count(), 2);
    assert_eq!(
        qq.text(1),
        "⚪ 本场转播已结束\n本场从 某频道 开始，共持续 3 小时 12 分钟。\n结束原因：某频道下播\n发送节点：a"
    );
    run(&mut ledger, None, Some(2), NOW + 4 * 3600 + 5, &qq, &commit).await;
    assert_eq!(qq.count(), 2);
    assert_eq!(
        ledger.live.source(&info("YT:UC1"), NOW + 5 * 3600),
        SourceOutcome::Started(2)
    );
    assert!(ledger.live.delivery.is_empty());
    run(&mut ledger, None, Some(2), NOW + 5 * 3600, &qq, &commit).await;
    assert_eq!(qq.count(), 3);
}

#[tokio::test]
async fn no_permission_and_rate_limit_fall_back_to_passive() {
    for refusal in [CODE_NO_PERMISSION, CODE_RATE_LIMITED] {
        let mut ledger = started();
        let mut target = admin_window(&mut ledger, NOW - 60);
        let qq = FakeQq::script([code(refusal), ok()]);
        let commit = FakeCommit::default();
        let report = run(&mut ledger, Some(&mut target), Some(1), NOW, &qq, &commit).await;
        assert_eq!(qq.modes(), vec!["active", "passive"], "code {refusal}");
        assert_eq!(qq.msg_id(1).as_deref(), Some(MSG_ID));
        assert_eq!(
            report.delivered,
            vec![(LiveKind::LiveStart, Channel::Passive)]
        );
        assert_eq!(target.replies_used, 1);
        // Each channel started with its own sending write (rule 12).
        let sendings: Vec<Channel> = commit
            .snapshots()
            .iter()
            .filter_map(|l| l.live.delivery.get(&LiveEventKey::start()))
            .filter(|r| r.state == DeliveryState::Sending)
            .map(|r| r.channel)
            .collect();
        assert_eq!(sendings, vec![Channel::Active, Channel::Passive]);
        let push = &ledger.live.active_push["a"];
        let expected = if refusal == CODE_NO_PERMISSION {
            "no_permission"
        } else {
            "rate_limited"
        };
        assert_eq!(push.last_result.as_deref(), Some(expected));
        assert_eq!(push.refused_since, Some(NOW));
        assert_eq!(report.refusal_began.map(|r| r.0), Some(refusal));
        // No wakeup spent.
        assert_eq!(ledger.wakeups_left(NOW), Some(4));
    }
}

#[tokio::test]
async fn passive_closed_codes_go_to_wakeup_without_retrying_passive() {
    for closed in PASSIVE_CLOSED_CODES {
        let mut ledger = started();
        let mut target = admin_window(&mut ledger, NOW - 60);
        let qq = FakeQq::script([code(CODE_NO_PERMISSION), code(closed), ok()]);
        let commit = FakeCommit::default();
        let report = run(&mut ledger, Some(&mut target), Some(1), NOW, &qq, &commit).await;
        assert_eq!(
            qq.modes(),
            vec!["active", "passive", "wakeup"],
            "code {closed}"
        );
        assert_eq!(
            report.delivered,
            vec![(LiveKind::LiveStart, Channel::Wakeup)]
        );
        assert!(!target.usable(NOW), "window closed after {closed}");
        assert_eq!(ledger.wakeups_left(NOW), Some(3));
    }
}

#[tokio::test]
async fn wakeup_keeps_the_alert_reserve_and_then_the_notice_waits() {
    let mut ledger = started();
    // Day 4 after the admin's message: only 3-7 and 7-30 are open.
    ledger.record_admin_message(NOW - 4 * 86_400);
    assert_eq!(ledger.wakeups_left(NOW), Some(2));
    let qq = FakeQq::script([code(CODE_NO_PERMISSION)]);
    let commit = FakeCommit::default();
    let report = run(&mut ledger, None, Some(1), NOW, &qq, &commit).await;
    assert_eq!(qq.modes(), vec!["active"]);
    assert_eq!(report.skipped, vec![LiveKind::LiveStart]);
    assert_eq!(ledger.live.pending(), vec!["live_start"]);
    assert_eq!(ledger.wakeups_left(NOW), Some(2), "reserve untouched");
    // A failover alert could still use those periods.
    assert!(ledger.wakeup_available(NOW));
    // Nothing more while no window is open and the periodic active retry
    // (rev 5 item 21) is not due yet.
    run(&mut ledger, None, Some(1), NOW + 300, &qq, &commit).await;
    assert_eq!(qq.count(), 1);
    // The admin writes: active is tried first again (still refused), then
    // the held note goes out once as a passive reply.
    qq.replies
        .lock()
        .unwrap()
        .push_back(code(CODE_NO_PERMISSION));
    let mut target = admin_window(&mut ledger, NOW + 3 * 3600);
    let report = run(
        &mut ledger,
        Some(&mut target),
        Some(1),
        NOW + 3 * 3600,
        &qq,
        &commit,
    )
    .await;
    assert_eq!(
        report.delivered,
        vec![(LiveKind::LiveStart, Channel::Passive)]
    );
    assert_eq!(qq.modes(), vec!["active", "active", "passive"]);
    let text = qq.text(2);
    assert!(
        text.starts_with("🟢 开始转播 某频道\nYouTube · 周末杂谈"),
        "{text}"
    );
    assert!(text.contains("补发：开始于 "), "{text}");
    assert!(text.ends_with("发送节点：a"));
    assert!(ledger.live.pending().is_empty());
    run(
        &mut ledger,
        Some(&mut target),
        Some(1),
        NOW + 3 * 3600 + 5,
        &qq,
        &commit,
    )
    .await;
    assert_eq!(qq.count(), 3);
}

#[tokio::test]
async fn ambiguous_results_retry_active_three_times_then_fall_back() {
    let mut ledger = started();
    let mut target = admin_window(&mut ledger, NOW - 60);
    let qq = FakeQq::script([network(), server_error(), network(), ok()]);
    let commit = FakeCommit::default();
    run(&mut ledger, Some(&mut target), Some(1), NOW, &qq, &commit).await;
    assert_eq!(qq.modes(), vec!["active"]);
    assert_eq!(
        state(&ledger, LiveEventKey::start()),
        Some(DeliveryState::Sending)
    );
    assert_eq!(ledger.live.next_retry_at(), Some(NOW + 20));
    // Too early for attempt 2.
    run(
        &mut ledger,
        Some(&mut target),
        Some(1),
        NOW + 5,
        &qq,
        &commit,
    )
    .await;
    assert_eq!(qq.count(), 1);
    run(
        &mut ledger,
        Some(&mut target),
        Some(1),
        NOW + 20,
        &qq,
        &commit,
    )
    .await;
    assert_eq!(qq.count(), 2);
    // Attempt 3 is the last active one, then the passive budget.
    run(
        &mut ledger,
        Some(&mut target),
        Some(1),
        NOW + 60,
        &qq,
        &commit,
    )
    .await;
    assert_eq!(qq.modes(), vec!["active", "active", "active", "passive"]);
    assert_eq!(
        state(&ledger, LiveEventKey::start()),
        Some(DeliveryState::Delivered)
    );
    // Ambiguous results never touched active_push or a wakeup.
    assert!(!ledger.live.active_push.contains_key("a"));
    assert_eq!(ledger.wakeups_left(NOW + 60), Some(4));
}

#[tokio::test]
async fn the_two_minute_window_ends_the_retries_early() {
    let mut ledger = started();
    let qq = FakeQq::script([network()]);
    let commit = FakeCommit::default();
    run(&mut ledger, None, Some(1), NOW, &qq, &commit).await;
    // The worker was busy: attempt 2 is due only after the window closed.
    run(
        &mut ledger,
        None,
        Some(1),
        NOW + AMBIGUOUS_WINDOW_SECS,
        &qq,
        &commit,
    )
    .await;
    assert_eq!(qq.count(), 1, "no active attempt after 2 minutes");
    // No passive window, no wakeup cycle known: pending, never silent.
    assert_eq!(ledger.live.pending(), vec!["live_start"]);
}

#[tokio::test]
async fn a_failed_sending_write_means_no_send() {
    let mut ledger = started();
    let qq = FakeQq::default();
    let commit = FakeCommit {
        fail: vec![1],
        ..Default::default()
    };
    let report = run(&mut ledger, None, Some(1), NOW, &qq, &commit).await;
    assert!(report.commit_failed);
    assert_eq!(qq.count(), 0);
    assert_eq!(
        state(&ledger, LiveEventKey::start()),
        None,
        "no record claims a send"
    );
    // The next round writes again and sends.
    run(&mut ledger, None, Some(1), NOW + 1, &qq, &commit).await;
    assert_eq!(qq.count(), 1);
    // Also on a fallback channel: a refused active, then the passive write fails.
    let mut ledger = started();
    let mut target = admin_window(&mut ledger, NOW - 60);
    let qq = FakeQq::script([code(CODE_NO_PERMISSION)]);
    let commit = FakeCommit {
        fail: vec![2],
        ..Default::default()
    };
    run(&mut ledger, Some(&mut target), Some(1), NOW, &qq, &commit).await;
    assert_eq!(qq.modes(), vec!["active"]);
    assert_eq!(target.replies_used, 0);
    assert_eq!(
        state(&ledger, LiveEventKey::start()),
        Some(DeliveryState::Refused)
    );
}

#[tokio::test]
async fn crash_between_the_sending_write_and_the_send() {
    let mut ledger = started();
    let qq = FakeQq::default();
    // Commit 1 lands, then the process dies before the request.
    let commit = FakeCommit {
        land_then_fail: vec![1],
        ..Default::default()
    };
    run(&mut ledger, None, Some(1), NOW, &qq, &commit).await;
    assert_eq!(qq.count(), 0);
    // Restart: memory is gone, the synced state says `sending` at term 1.
    let mut restarted = commit.last();
    restarted.local = "a".into();
    assert_eq!(
        state(&restarted, LiveEventKey::start()),
        Some(DeliveryState::Sending)
    );
    let commit = FakeCommit::default();
    run(&mut restarted, None, Some(2), NOW + 30, &qq, &commit).await;
    assert_eq!(qq.count(), 1, "the notice is not lost");
    assert_eq!(
        state(&restarted, LiveEventKey::start()),
        Some(DeliveryState::Delivered)
    );
}

#[tokio::test]
async fn crash_between_accept_and_the_delivered_write_sends_one_duplicate_at_most() {
    let mut ledger = started();
    let qq = FakeQq::default();
    // Commit 2 is the delivered write: the process dies before it lands.
    let commit = FakeCommit {
        fail: vec![2],
        ..Default::default()
    };
    run(&mut ledger, None, Some(1), NOW, &qq, &commit).await;
    assert_eq!(qq.count(), 1);
    let mut b = commit.last();
    b.local = "b".into();
    assert_eq!(
        state(&b, LiveEventKey::start()),
        Some(DeliveryState::Sending)
    );
    let commit_b = FakeCommit::default();
    run(&mut b, None, Some(2), NOW + 30, &qq, &commit_b).await;
    assert_eq!(qq.count(), 2, "one accepted duplicate");
    for t in [60, 600, 3600] {
        run(&mut b, None, Some(2), NOW + t, &qq, &commit_b).await;
    }
    assert_eq!(qq.count(), 2, "delivered is never sent again");
}

#[tokio::test]
async fn failover_mid_retry_sends_once() {
    let mut a = started();
    let qq = FakeQq::script([network()]);
    let commit_a = FakeCommit::default();
    run(&mut a, None, Some(1), NOW, &qq, &commit_a).await;
    assert_eq!(qq.count(), 1);
    assert!(qq.text(0).ends_with("发送节点：a"));
    // A loses the gateway: no term, no retry.
    run(&mut a, None, None, NOW + 20, &qq, &commit_a).await;
    assert_eq!(qq.count(), 1);
    // B joins A's heartbeat ledger and takes term 2.
    let mut b = NotifyLedger {
        local: "b".into(),
        ..Default::default()
    };
    b.join(&a);
    b.live.term += 1;
    let term_b = b.live.term;
    let commit_b = FakeCommit::default();
    run(&mut b, None, Some(term_b), NOW + 25, &qq, &commit_b).await;
    assert_eq!(qq.count(), 2);
    assert!(qq.text(1).ends_with("发送节点：b"));
    assert_eq!(
        state(&b, LiveEventKey::start()),
        Some(DeliveryState::Delivered)
    );
    // A sees B's record and, even holding the gateway again, sends nothing.
    a.join(&b);
    a.live.term += 1;
    let term_a = a.live.term;
    run(&mut a, None, Some(term_a), NOW + 60, &qq, &commit_a).await;
    assert_eq!(qq.count(), 2);
    // A stale A heartbeat cannot downgrade B's delivered record.
    let stale = commit_a.snapshots()[0].clone();
    b.join(&stale);
    assert_eq!(
        state(&b, LiveEventKey::start()),
        Some(DeliveryState::Delivered)
    );
}

#[tokio::test]
async fn an_old_version_peer_ledger_without_live_state_changes_nothing() {
    // Mixed versions: an older node re-advertises a ledger with no `live`
    // field. Joining it must neither clear a delivered record nor re-open it.
    let mut ledger = started();
    let qq = FakeQq::default();
    let commit = FakeCommit::default();
    run(&mut ledger, None, Some(1), NOW, &qq, &commit).await;
    let mut old: serde_json::Value = serde_json::to_value(&ledger).unwrap();
    old.as_object_mut().unwrap().remove("live");
    let old: NotifyLedger = serde_json::from_value(old).unwrap();
    ledger.join(&old);
    run(&mut ledger, None, Some(2), NOW + 60, &qq, &commit).await;
    assert_eq!(qq.count(), 1);
}

#[tokio::test]
async fn switch_off_marks_the_notice_off_and_nothing_is_sent() {
    let mut ledger = started();
    let qq = FakeQq::default();
    let commit = FakeCommit::default();
    let node = ledger.local.clone();
    let mut driver = Driver {
        ledger: &mut ledger,
        passive: None,
        term: Some(1),
        node: &node,
        sender_name: &node,
        enabled: false,
        now: NOW,
    };
    drive(&mut driver, &qq, &commit).await;
    assert_eq!(qq.count(), 0);
    assert_eq!(
        state(&ledger, LiveEventKey::start()),
        Some(DeliveryState::Off)
    );
    // Turning it back on later does not resend an old broadcast's notice.
    run(&mut ledger, None, Some(1), NOW + 60, &qq, &commit).await;
    assert_eq!(qq.count(), 0);
}

#[tokio::test]
async fn active_push_counts_the_shanghai_month_and_tracks_refusals() {
    let mut ledger = started();
    let qq = FakeQq::script([ok()]);
    let commit = FakeCommit::default();
    run(&mut ledger, None, Some(1), NOW, &qq, &commit).await;
    let push = ledger.live.active_push["a"].clone();
    assert_eq!(push.last_result.as_deref(), Some("ok"));
    assert_eq!(push.month_sent, 1);
    assert_eq!(push.refused_since, None);
    let view = push.view(NOW);
    assert_eq!(view["month_sent"], 1);
    assert_eq!(view["last_at"], "2027-01-15T16:00:00+08:00");
    // 40 days later the month rolled over: the view reads 0.
    assert_eq!(push.view(NOW + 40 * 86_400)["month_sent"], 0);
    // Refusals keep the first date until the next ok.
    ledger.live.end(NOW + 100, BroadcastEndCause::SourceEnded);
    let qq = FakeQq::script([code(CODE_RATE_LIMITED)]);
    let report = run(&mut ledger, None, Some(1), NOW + 200, &qq, &commit).await;
    assert_eq!(
        report.refusal_began,
        Some((CODE_RATE_LIMITED, NOW + 200, 1))
    );
    assert_eq!(ledger.live.active_push["a"].refused_since, Some(NOW + 200));
    assert_eq!(
        ActivePush::empty_view()["last_result"],
        serde_json::Value::Null
    );
    // Month boundary is Shanghai time: 2027-01-31 16:00Z is already February.
    assert_eq!(shanghai_month(1_801_670_400), 202702);
}

#[test]
fn live_join_is_commutative_associative_and_idempotent() {
    let record = |state, at| DeliveryRecord {
        state,
        term: 1,
        channel: Channel::Active,
        at,
        seq: 0,
        attempts: 1,
        first_at: NOW,
        held: false,
    };
    let mut a = started();
    a.live
        .write(LiveEventKey::start(), record(DeliveryState::Sending, NOW));
    let mut b = a.clone();
    b.live.write(
        LiveEventKey::start(),
        record(DeliveryState::Delivered, NOW + 1),
    );
    b.live.end(NOW + 50, BroadcastEndCause::SourceEnded);
    let mut c = a.clone();
    c.live.term = 7;
    c.live.active_push.insert("c".into(), ActivePush::default());
    let mut abc = a.live.clone();
    abc.join(&b.live);
    abc.join(&c.live);
    let mut cba = c.live.clone();
    cba.join(&b.live);
    cba.join(&a.live);
    assert_eq!(abc, cba);
    let mut twice = abc.clone();
    twice.join(&abc);
    assert_eq!(twice, abc);
    assert_eq!(
        abc.delivery[&LiveEventKey::start()].state,
        DeliveryState::Delivered
    );
    assert_eq!(abc.broadcast.ended_at, Some(NOW + 50));
    assert_eq!(abc.term, 7);
    // A newer generation replaces older records wholesale (rule 14).
    let mut next = b.clone();
    next.live.source(&info("YT:UC1"), NOW + 7200);
    abc.join(&next.live);
    assert_eq!(abc.broadcast.generation, 2);
    assert!(abc.delivery.is_empty());
}

#[tokio::test]
async fn ledger_and_records_carry_no_openid_or_msg_id() {
    let openid = "E4F0A1B2C3D4E5F60718293A4B5C6D7E";
    let mut ledger = started();
    let mut target = admin_window(&mut ledger, NOW - 60);
    let qq = FakeQq::script([code(CODE_NO_PERMISSION), ok()]);
    let commit = FakeCommit::default();
    run(&mut ledger, Some(&mut target), Some(1), NOW, &qq, &commit).await;
    let mut snapshots = commit.snapshots();
    snapshots.push(ledger.clone());
    for snapshot in &snapshots {
        let json = serde_json::to_string(snapshot).unwrap();
        for needle in [
            openid,
            MSG_ID,
            "msg_id",
            "openid",
            "last_admin_msg_id",
            "passive_replies_used",
        ] {
            assert!(!json.contains(needle), "{needle} in {json}");
        }
    }
    let quota = quota_view(&ledger, Some(&target), NOW);
    let keys: Vec<&String> = quota.as_object().unwrap().keys().collect();
    assert_eq!(
        keys,
        vec![
            "passive_window_open",
            "wakeup_next_close_at",
            "wakeup_remaining",
            "wakeup_reserve"
        ]
    );
    assert!(!quota.to_string().contains(MSG_ID));
}

#[test]
fn texts_carry_no_links_or_revenue() {
    let mut broadcast = started().live.broadcast;
    broadcast.title = "看 https://example.com 吧".into();
    let text = render(LiveEventKey::start(), &broadcast, false, "a");
    assert!(
        !text.contains("://") && !text.contains("example.com"),
        "{text}"
    );
    broadcast.ended_at = Some(broadcast.started_at + 47 * 60);
    assert_eq!(
        render(LiveEventKey::end(), &broadcast, false, "a"),
        "⚪ 本场转播已结束\n本场从 某频道 开始，共持续 47 分钟。\n发送节点：a"
    );
}

/// M1 (rule 17): an unknown definite 4xx on the active send is not a
/// fallback trigger; it counts toward the 3 attempts in 2 minutes first.
#[tokio::test]
async fn an_unknown_4xx_on_active_counts_toward_the_retry_limit_before_falling_back() {
    let mut ledger = started();
    let mut target = admin_window(&mut ledger, NOW - 60);
    // 40034005 is a passive-closed code: on active it is "other" too.
    let qq = FakeQq::script([code(11_111), code(40034005), code(22_222), ok()]);
    let commit = FakeCommit::default();
    run(&mut ledger, Some(&mut target), Some(1), NOW, &qq, &commit).await;
    assert_eq!(qq.modes(), vec!["active"], "no fallback on the first 4xx");
    assert_eq!(
        state(&ledger, LiveEventKey::start()),
        Some(DeliveryState::Sending)
    );
    assert_eq!(target.replies_used, 0, "passive budget untouched");
    for at in [NOW + 20, NOW + 60] {
        run(&mut ledger, Some(&mut target), Some(1), at, &qq, &commit).await;
    }
    assert_eq!(qq.modes(), vec!["active", "active", "active", "passive"]);
    assert_eq!(
        state(&ledger, LiveEventKey::start()),
        Some(DeliveryState::Delivered)
    );
    // Not a refusal: active_push is unchanged.
    assert!(!ledger.live.active_push.contains_key("a"));
}

/// L1 (rev 5 item 20): a source switch during one Bilibili broadcast sends a
/// 换台, never a second 开播, and exactly one 下播 that names the source chain
/// and the end cause when the room goes offline.
#[tokio::test]
async fn a_source_switch_mid_broadcast_sends_a_switch_and_one_end() {
    let mut ledger = started();
    let qq = FakeQq::default();
    let commit = FakeCommit::default();
    run(&mut ledger, None, Some(1), NOW, &qq, &commit).await;
    assert_eq!(qq.count(), 1);
    let generation = ledger.live.broadcast.generation;
    let mut other = info("TW:other");
    other.channel = "后来切换的频道".into();
    assert_eq!(
        ledger.live.source(&other, NOW + 600),
        SourceOutcome::Switched(generation, 1)
    );
    assert_eq!(
        ledger.live.broadcast.stream_key, "YT:UC1",
        "first source kept"
    );
    assert_eq!(ledger.live.broadcast.current_key, "TW:other");
    run(&mut ledger, None, Some(1), NOW + 601, &qq, &commit).await;
    assert_eq!(qq.count(), 2, "one 换台, no second 开播");
    assert_eq!(
        qq.text(1),
        "🟢换台 后来切换的频道\nYouTube · 周末杂谈\n发送节点：a"
    );
    // Re-selecting the source already relayed changes nothing.
    assert_eq!(
        ledger.live.source(&other, NOW + 700),
        SourceOutcome::Same(generation)
    );
    run(&mut ledger, None, Some(1), NOW + 701, &qq, &commit).await;
    assert_eq!(qq.count(), 2, "same source: no notice");
    assert!(ledger.live.end(NOW + 3600, BroadcastEndCause::WarningCut));
    assert!(
        !ledger.live.end(NOW + 3601, BroadcastEndCause::ManualStop),
        "ends once, the first cause wins"
    );
    run(&mut ledger, None, Some(1), NOW + 3602, &qq, &commit).await;
    run(&mut ledger, None, Some(1), NOW + 3700, &qq, &commit).await;
    assert_eq!(qq.count(), 3);
    let ended = qq.text(2);
    assert!(ended.starts_with("⚪ 本场转播已结束"));
    assert!(
        ended.contains("本场 某频道 -> 后来切换的频道，共持续 1 小时 0 分钟。"),
        "{ended}"
    );
    assert!(ended.contains("结束原因：B站警告/切断"), "{ended}");
    assert!(ended.ends_with("发送节点：a"), "{ended}");
    // After it ended, a start is a new broadcast again.
    assert_eq!(
        ledger.live.source(&info("TW:other"), NOW + 7200),
        SourceOutcome::Started(generation + 1)
    );
}

/// A superseded 换台 never sends and never rides along in heartbeats: only the
/// newest switch record survives the session.
#[tokio::test]
async fn only_the_newest_switch_record_survives() {
    let mut ledger = started();
    let qq = FakeQq::default();
    let commit = FakeCommit::default();
    run(&mut ledger, None, Some(1), NOW, &qq, &commit).await;
    let mut first = info("TW:first");
    first.channel = "第一个".into();
    let mut second = info("YT:second");
    second.channel = "第二个".into();
    ledger.live.source(&first, NOW + 100);
    run(&mut ledger, None, Some(1), NOW + 101, &qq, &commit).await;
    assert_eq!(qq.count(), 2);
    assert_eq!(
        ledger.live.delivery[&LiveEventKey::switch(1)].state,
        DeliveryState::Delivered
    );
    // The next switch supersedes the record still in flight.
    ledger.live.source(&second, NOW + 200);
    assert_eq!(ledger.live.broadcast.switch_seq, 2);
    let keys: Vec<String> = ledger
        .live
        .delivery
        .keys()
        .map(|key| key.as_str())
        .collect();
    assert_eq!(keys, vec!["live_start"], "the superseded switch is dropped");
    run(&mut ledger, None, Some(1), NOW + 201, &qq, &commit).await;
    assert_eq!(qq.count(), 3, "only the newest switch is sent");
    assert_eq!(qq.text(1), "🟢换台 第一个\nYouTube · 周末杂谈\n发送节点：a");
    assert_eq!(qq.text(2), "🟢换台 第二个\nYouTube · 周末杂谈\n发送节点：a");
    let keys: Vec<String> = ledger
        .live
        .delivery
        .keys()
        .map(|key| key.as_str())
        .collect();
    assert_eq!(keys, vec!["live_start", "live_switch:2"]);
}

/// The 换台 continues the 开播: while the session's opening notice is still
/// queued, a switch never overtakes it.
#[tokio::test]
async fn a_switch_waits_for_the_opening_notice() {
    let mut ledger = started();
    ledger.record_admin_message(NOW - 60);
    // Refused active, then the one usable wakeup refused too: held.
    let qq = FakeQq::script([code(CODE_NO_PERMISSION), code(40054010)]);
    let commit = FakeCommit::default();
    run(&mut ledger, None, Some(1), NOW, &qq, &commit).await;
    assert_eq!(ledger.live.pending(), vec!["live_start"]);
    let mut other = info("TW:other");
    other.channel = "后来切换的频道".into();
    ledger.live.source(&other, NOW + 300);
    assert_eq!(ledger.live.due_events(), vec![LiveEventKey::start()]);
    run(&mut ledger, None, Some(1), NOW + 301, &qq, &commit).await;
    assert_eq!(qq.count(), 2, "the held 开播 is not displaced by the 换台");
}

/// Every cause names itself once and survives the ledger's serde round trip.
#[test]
fn every_end_cause_names_itself_and_round_trips() {
    let all = [
        BroadcastEndCause::WarningCut,
        BroadcastEndCause::ManualStop,
        BroadcastEndCause::AutoStop,
        BroadcastEndCause::SourceEnded,
        BroadcastEndCause::ContentRejected,
        BroadcastEndCause::BiliStopped,
    ];
    let mut labels: Vec<&str> = all.iter().map(|cause| cause.label()).collect();
    labels.sort_unstable();
    labels.dedup();
    assert_eq!(labels.len(), all.len(), "one label per cause");
    for cause in all {
        let json = serde_json::to_string(&cause).unwrap();
        assert_eq!(json, format!("\"{}\"", cause.as_str()));
        assert_eq!(
            serde_json::from_str::<BroadcastEndCause>(&json).unwrap(),
            cause
        );
    }
}

/// The task's exact copy: 开播 names the first source, 换台 the one the room
/// moved to inside the same session, and 下播 names the chain, the total
/// duration and the cause.
#[test]
fn the_notice_copy_is_the_task_copy() {
    let mut ledger = started();
    assert_eq!(
        render(LiveEventKey::start(), &ledger.live.broadcast, false, "JP"),
        "🟢 开始转播 某频道\nYouTube · 周末杂谈\n发送节点：JP"
    );
    let mut other = info("TW:kamito");
    other.platform = "Twitch".into();
    other.channel = "Kamito".into();
    other.title = "League of Legends".into();
    ledger.live.source(&other, NOW + 3 * 3600);
    assert_eq!(
        render(LiveEventKey::switch(1), &ledger.live.broadcast, false, "JP"),
        "🟢换台 Kamito\nTwitch · League of Legends\n发送节点：JP"
    );
    ledger
        .live
        .end(NOW + 27 * 3600, BroadcastEndCause::WarningCut);
    assert_eq!(
        render(LiveEventKey::end(), &ledger.live.broadcast, false, "JP"),
        "⚪ 本场转播已结束\n本场 某频道 -> Kamito，共持续 1 天 3 小时。\n结束原因：B站警告/切断\n发送节点：JP"
    );
}

/// Only a source end names a channel: the session's newest source, because
/// that is the channel the operator saw go offline. Any other cause stays a
/// room fact, and an unknown source falls back to the generic label.
#[test]
fn a_source_end_names_the_channel_that_went_offline() {
    // The reported notice: 天帝フォルテ stopped, nobody pressed 停止直播, and
    // the room closed on its own later because there was no source left.
    let mut ledger = NotifyLedger {
        local: "CA".into(),
        ..Default::default()
    };
    let mut forte = info("YT:UCforte");
    forte.channel = "天帝フォルテ".into();
    ledger.live.source(&forte, NOW);
    assert!(ledger
        .live
        .end(NOW + 42 * 60, BroadcastEndCause::SourceEnded));
    assert_eq!(
        render(LiveEventKey::end(), &ledger.live.broadcast, false, "CA"),
        "⚪ 本场转播已结束\n本场从 天帝フォルテ 开始，共持续 42 分钟。\n结束原因：天帝フォルテ下播\n发送节点：CA"
    );
    // A session that switched names the source relayed last, not the first.
    let mut switched = started();
    let mut other = info("TW:kamito");
    other.channel = "Kamito".into();
    switched.live.source(&other, NOW + 60);
    switched
        .live
        .end(NOW + 42 * 60, BroadcastEndCause::SourceEnded);
    assert!(
        render(LiveEventKey::end(), &switched.live.broadcast, false, "CA")
            .contains("结束原因：Kamito下播")
    );
    // A room that went away while a source was still being relayed keeps the
    // room wording: nobody here stopped it, but no source ended either.
    let mut room = started();
    room.live.end(NOW + 60, BroadcastEndCause::BiliStopped);
    assert!(
        render(LiveEventKey::end(), &room.live.broadcast, false, "CA")
            .contains("结束原因：B站直播间下播")
    );
    // A session with no recorded source cannot name one.
    let mut unnamed = started();
    unnamed.live.broadcast.channel.clear();
    unnamed.live.end(NOW + 60, BroadcastEndCause::SourceEnded);
    assert!(
        render(LiveEventKey::end(), &unnamed.live.broadcast, false, "CA")
            .contains("结束原因：源直播结束")
    );
}

/// L2 (rev 5 item 21): once an active send is accepted again, a held
/// notice goes back to active first without waiting for the admin.
#[tokio::test]
async fn a_held_notice_goes_active_first_once_active_works_again() {
    let mut ledger = started();
    let qq = FakeQq::script([code(CODE_NO_PERMISSION)]);
    let commit = FakeCommit::default();
    run(&mut ledger, None, Some(1), NOW, &qq, &commit).await;
    assert_eq!(ledger.live.pending(), vec!["live_start"]);
    assert_eq!(
        ledger.live.next_retry_at(),
        Some(NOW + PENDING_ACTIVE_RETRY_SECS)
    );
    // Not due yet, and no active success seen: nothing.
    run(&mut ledger, None, Some(1), NOW + 60, &qq, &commit).await;
    assert_eq!(qq.count(), 1);
    // Another active send on this node was accepted.
    let push = ledger.live.active_push.get_mut("a").unwrap();
    push.last_result = Some("ok".into());
    push.last_at = NOW + 61;
    let report = run(&mut ledger, None, Some(1), NOW + 62, &qq, &commit).await;
    assert_eq!(
        report.delivered,
        vec![(LiveKind::LiveStart, Channel::Active)]
    );
    assert_eq!(qq.modes(), vec!["active", "active"]);
    assert!(qq.text(1).starts_with("🟢 开始转播 "));
    assert!(qq.text(1).contains("补发：开始于 "));
    assert!(ledger.live.pending().is_empty());
}

/// L2 failure path: the periodic active retry of a held notice is refused
/// again; it never spends a wakeup and stays held.
#[tokio::test]
async fn a_refused_periodic_retry_keeps_the_notice_held_without_a_wakeup() {
    let mut ledger = started();
    ledger.record_admin_message(NOW - 60);
    let wakeups = ledger.wakeups_left(NOW);
    // Active refused, then the wakeup fallback is refused too.
    let qq = FakeQq::script([code(CODE_NO_PERMISSION), code(40054010)]);
    let commit = FakeCommit::default();
    run(&mut ledger, None, Some(1), NOW, &qq, &commit).await;
    assert_eq!(qq.modes(), vec!["active", "wakeup"]);
    assert_eq!(ledger.live.pending(), vec!["live_start"]);
    let qq2 = FakeQq::script([code(CODE_NO_PERMISSION)]);
    let later = NOW + PENDING_ACTIVE_RETRY_SECS + 5;
    run(&mut ledger, None, Some(1), later, &qq2, &commit).await;
    assert_eq!(qq2.modes(), vec!["active"], "no second wakeup");
    assert_eq!(ledger.live.pending(), vec!["live_start"]);
    assert!(ledger.wakeups_left(later) >= wakeups.map(|w| w.saturating_sub(1)));
}

/// Nit (rev 5 item 23): a failed `delivered` write keeps the notice
/// delivered in memory and never sends it twice in this term.
#[tokio::test]
async fn a_failed_delivered_write_does_not_resend() {
    let mut ledger = started();
    let qq = FakeQq::default();
    // Commit 1 = sending, commit 2 = delivered (fails).
    let commit = FakeCommit {
        fail: vec![2],
        ..Default::default()
    };
    let report = run(&mut ledger, None, Some(1), NOW, &qq, &commit).await;
    assert_eq!(
        report.delivered,
        vec![(LiveKind::LiveStart, Channel::Active)]
    );
    assert_eq!(
        state(&ledger, LiveEventKey::start()),
        Some(DeliveryState::Delivered)
    );
    run(&mut ledger, None, Some(1), NOW + 30, &qq, &commit).await;
    assert_eq!(qq.count(), 1);
}

/// E rev 6 item 25 / Clerk (a): an unclear active retry on a held 开播
/// keeps it held and never spends a wakeup, so the periods kept for
/// alerts stay untouched across cycles.
#[tokio::test]
async fn an_unclear_active_retry_on_a_held_notice_stays_held_without_a_wakeup() {
    let mut ledger = started();
    // Day 2 after the admin's message: three periods open, two reserved.
    ledger.record_admin_message(NOW - 2 * 86_400);
    assert_eq!(ledger.wakeups_left(NOW), Some(3));
    // Active refused, the one usable wakeup refused too: held.
    let qq = FakeQq::script([code(CODE_NO_PERMISSION), code(40054010)]);
    let commit = FakeCommit::default();
    run(&mut ledger, None, Some(1), NOW, &qq, &commit).await;
    assert_eq!(qq.modes(), vec!["active", "wakeup"]);
    assert_eq!(ledger.live.pending(), vec!["live_start"]);
    let left = ledger.wakeups_left(NOW);
    // Periodic active retry: unclear three times within the window.
    let start = NOW + PENDING_ACTIVE_RETRY_SECS + 1;
    let qq2 = FakeQq::script([network(), server_error(), network()]);
    for at in [start, start + 20, start + 60] {
        run(&mut ledger, None, Some(1), at, &qq2, &commit).await;
    }
    assert_eq!(qq2.modes(), vec!["active", "active", "active"]);
    assert_eq!(ledger.live.pending(), vec!["live_start"], "still held");
    assert_eq!(ledger.wakeups_left(start + 60), left, "no wakeup spent");
    assert!(ledger.live.delivery[&LiveEventKey::start()].held);
    // A later cycle (another term after a failover) still never wakes.
    run(&mut ledger, None, Some(2), start + 61, &qq2, &commit).await;
    assert!(!qq2.modes().contains(&"wakeup"));
    assert!(ledger.wakeups_left(start + 61) >= Some(2), "reserve intact");
}

// ------------------------------------------- cutoff-reason (PRD R3)

fn reason(text: &str, rank: u8) -> Option<crate::events::EndDetail> {
    Some(crate::events::EndDetail {
        text: text.into(),
        rank,
    })
}

const V2_REASON: &str =
    "您本场直播存在挂机、录播等消极直播行为，因此直播被切断，请您及时整改（切断本场直播）";

#[test]
fn the_end_notice_carries_bilibilis_reason_after_the_fixed_label() {
    let mut ledger = started();
    assert!(ledger.live.end_with(
        NOW + 60,
        BroadcastEndCause::WarningCut,
        reason(V2_REASON, 7)
    ));
    let text = render(LiveEventKey::end(), &ledger.live.broadcast, false, "a");
    assert!(
        text.contains(&format!("\n结束原因：B站警告/切断 · {V2_REASON}\n")),
        "{text}"
    );
}

#[test]
fn a_later_v2_reason_upgrades_the_notice_and_a_plain_msg_never_downgrades_it() {
    let mut ledger = started();
    assert!(ledger.live.end_with(
        NOW + 60,
        BroadcastEndCause::WarningCut,
        reason("违反直播言论规范，请立即调整", 4)
    ));
    assert!(!ledger.live.end_with(
        NOW + 61,
        BroadcastEndCause::WarningCut,
        reason(V2_REASON, 7)
    ));
    assert!(!ledger.live.end_with(
        NOW + 62,
        BroadcastEndCause::WarningCut,
        reason("违反直播言论规范，请立即调整", 4)
    ));
    let text = render(LiveEventKey::end(), &ledger.live.broadcast, false, "a");
    assert!(text.contains(V2_REASON), "{text}");
    assert!(!text.contains("违反直播言论规范"), "{text}");
}

#[test]
fn a_reason_never_attaches_to_another_cause_and_never_reaches_the_wire() {
    let mut ledger = started();
    assert!(ledger.live.end_with(
        NOW + 60,
        BroadcastEndCause::BiliStopped,
        reason(V2_REASON, 7)
    ));
    assert!(ledger.live.broadcast.end_detail.is_none());
    let text = render(LiveEventKey::end(), &ledger.live.broadcast, false, "a");
    assert!(text.contains("结束原因：B站直播间下播\n"), "{text}");

    let mut ledger = started();
    ledger.live.end_with(
        NOW + 60,
        BroadcastEndCause::WarningCut,
        reason(V2_REASON, 7),
    );
    let wire = serde_json::to_string(&ledger).unwrap();
    assert!(
        !wire.contains("挂机") && !wire.contains("end_detail"),
        "{wire}"
    );
    // A peer's copy (or a restart) renders the fixed label alone.
    let peer: NotifyLedger = serde_json::from_str(&wire).unwrap();
    let text = render(LiveEventKey::end(), &peer.live.broadcast, false, "b");
    assert!(text.contains("结束原因：B站警告/切断\n"), "{text}");
    // Joining a peer's copy of the same session keeps the local reason.
    let mut mine = ledger.clone();
    mine.live.join(&peer.live);
    assert_eq!(mine.live.broadcast.end_detail, reason(V2_REASON, 7));
}

#[test]
fn the_end_notice_sanitizes_and_caps_the_reason() {
    let mut ledger = started();
    let long = format!("违规 https://evil.example/x {}", "很长".repeat(200));
    ledger
        .live
        .end_with(NOW + 60, BroadcastEndCause::WarningCut, reason(&long, 4));
    let text = render(LiveEventKey::end(), &ledger.live.broadcast, false, "a");
    let line = text
        .lines()
        .find(|l| l.starts_with("结束原因："))
        .unwrap()
        .to_string();
    assert!(
        !line.contains("://") && !line.contains("evil.example"),
        "{line}"
    );
    assert!(line.chars().count() <= "结束原因：B站警告/切断 · ".chars().count() + 160);
}

#[test]
fn the_bus_carries_the_reason_from_the_danmaku_client_to_the_ledger() {
    let event = crate::events::Event {
        at: NOW,
        kind: crate::events::EventKind::BroadcastEnded {
            cause: BroadcastEndCause::WarningCut,
            detail: reason(V2_REASON, 7),
        },
    };
    assert_eq!(
        crate::notify::bus::map(&event),
        Some(LiveSignal::Ended(
            BroadcastEndCause::WarningCut,
            reason(V2_REASON, 7)
        ))
    );
}
