//! Addendum A/D/E rev 3 gate tests: send ladder, delivery records, crashes,
//! failover, fallback codes, ambiguous-retry limit, no leaks.

use super::*;
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

fn info(key: &str) -> LiveStartInfo {
    LiveStartInfo {
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
    ledger.live.start(&info("YT:UC1"), NOW);
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
        enabled: true,
        now,
    };
    drive(&mut driver, qq, commit).await
}

fn state(ledger: &NotifyLedger, kind: LiveKind) -> Option<DeliveryState> {
    ledger.live.delivery.get(&kind).map(|record| record.state)
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
    assert_eq!(qq.text(0), "开播 · 周末杂谈 · 来源 YouTube/某频道");
    // The sending record was committed before the request, delivered after.
    let committed = commit.snapshots();
    assert_eq!(
        state(&committed[0], LiveKind::LiveStart),
        Some(DeliveryState::Sending)
    );
    assert_eq!(committed[0].live.delivery[&LiveKind::LiveStart].term, 1);
    assert_eq!(
        state(&committed[1], LiveKind::LiveStart),
        Some(DeliveryState::Delivered)
    );
    // Restart re-calling start-live keeps the counter: nothing new is due.
    assert_eq!(ledger.live.start(&info("YT:UC1"), NOW + 60), 1);
    run(&mut ledger, None, Some(2), NOW + 61, &qq, &commit).await;
    assert_eq!(qq.count(), 1);
    // 下播 once, then the next broadcast clears the records.
    assert!(ledger.live.end(NOW + 3 * 3600 + 12 * 60));
    assert!(!ledger.live.end(NOW + 4 * 3600));
    run(&mut ledger, None, Some(2), NOW + 4 * 3600, &qq, &commit).await;
    assert_eq!(qq.count(), 2);
    assert_eq!(qq.text(1), "下播 · 本场 3小时12分 · 来源 某频道");
    run(&mut ledger, None, Some(2), NOW + 4 * 3600 + 5, &qq, &commit).await;
    assert_eq!(qq.count(), 2);
    assert_eq!(ledger.live.start(&info("YT:UC1"), NOW + 5 * 3600), 2);
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
            .filter_map(|l| l.live.delivery.get(&LiveKind::LiveStart))
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
    // Nothing more while no window is open.
    run(&mut ledger, None, Some(1), NOW + 600, &qq, &commit).await;
    assert_eq!(qq.count(), 1);
    // The admin writes: the pending note goes out once, as a passive reply.
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
    let text = qq.text(1);
    assert!(text.starts_with("（补发）开播 · 周末杂谈"), "{text}");
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
    assert_eq!(qq.count(), 2);
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
        state(&ledger, LiveKind::LiveStart),
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
        state(&ledger, LiveKind::LiveStart),
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
        state(&ledger, LiveKind::LiveStart),
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
        state(&ledger, LiveKind::LiveStart),
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
        state(&restarted, LiveKind::LiveStart),
        Some(DeliveryState::Sending)
    );
    let commit = FakeCommit::default();
    run(&mut restarted, None, Some(2), NOW + 30, &qq, &commit).await;
    assert_eq!(qq.count(), 1, "the notice is not lost");
    assert_eq!(
        state(&restarted, LiveKind::LiveStart),
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
    assert_eq!(state(&b, LiveKind::LiveStart), Some(DeliveryState::Sending));
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
    assert_eq!(
        state(&b, LiveKind::LiveStart),
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
        state(&b, LiveKind::LiveStart),
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
        enabled: false,
        now: NOW,
    };
    drive(&mut driver, &qq, &commit).await;
    assert_eq!(qq.count(), 0);
    assert_eq!(
        state(&ledger, LiveKind::LiveStart),
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
    ledger.live.end(NOW + 100);
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
    };
    let mut a = started();
    a.live
        .write(LiveKind::LiveStart, record(DeliveryState::Sending, NOW));
    let mut b = a.clone();
    b.live.write(
        LiveKind::LiveStart,
        record(DeliveryState::Delivered, NOW + 1),
    );
    b.live.end(NOW + 50);
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
        abc.delivery[&LiveKind::LiveStart].state,
        DeliveryState::Delivered
    );
    assert_eq!(abc.broadcast.ended_at, Some(NOW + 50));
    assert_eq!(abc.term, 7);
    // A newer generation replaces older records wholesale (rule 14).
    let mut next = b.clone();
    next.live.start(&info("YT:UC1"), NOW + 7200);
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
    let text = render(LiveKind::LiveStart, &broadcast, false);
    assert!(
        !text.contains("://") && !text.contains("example.com"),
        "{text}"
    );
    broadcast.ended_at = Some(broadcast.started_at + 47 * 60);
    assert_eq!(
        render(LiveKind::LiveEnd, &broadcast, false),
        "下播 · 本场 47分 · 来源 某频道"
    );
}
