use super::*;
use crate::notify::event::Condition;

fn cfg() -> PolicyConfig {
    PolicyConfig::from_config(&NotifyConfig {
        confirm_secs: 60,
        recovery_hold_secs: 30,
        cooldown_secs: 1800,
        critical_cooldown_secs: 600,
        coalesce_secs: 10,
        ..Default::default()
    })
}

fn down(node: &str) -> (String, Condition) {
    let cond = Condition::new(EventKind::NodeDown, node).with_detail("心跳超时");
    (cond.dedup_key(), cond)
}

fn set(items: &[(String, Condition)]) -> BTreeMap<String, Condition> {
    items.iter().cloned().collect()
}

fn run(
    tracker: &mut Tracker,
    cfg: &PolicyConfig,
    from: u64,
    to: u64,
    active: &BTreeMap<String, Condition>,
    mode: Mode,
) -> Vec<OutItem> {
    let ledger = NotifyLedger::default();
    let mut out = Vec::new();
    for now in from..=to {
        tracker.reconcile(now, active, mode, cfg);
        tracker.tick(now, cfg, &ledger);
        let batch = tracker.take_batch(now, cfg);
        tracker.delivered(&batch, now);
        out.extend(batch);
    }
    out
}

#[test]
fn short_blip_never_sends() {
    let cfg = cfg();
    let mut tracker = Tracker::default();
    let bad = set(&[down("b")]);
    assert!(run(&mut tracker, &cfg, 0, 30, &bad, Mode::Normal).is_empty());
    assert!(run(&mut tracker, &cfg, 31, 200, &BTreeMap::new(), Mode::Normal).is_empty());
    assert_eq!(tracker.open_incidents(), 0);
}

#[test]
fn confirmed_down_sends_once_then_resolves_after_hold() {
    let cfg = cfg();
    let mut tracker = Tracker::default();
    let bad = set(&[down("b")]);
    let alerts = run(&mut tracker, &cfg, 0, 300, &bad, Mode::Normal);
    assert_eq!(alerts.len(), 1);
    assert_eq!(alerts[0].item, ItemKind::Alert);
    // Healthy for less than the hold: nothing; then exactly one resolved.
    assert!(run(&mut tracker, &cfg, 301, 320, &BTreeMap::new(), Mode::Normal).is_empty());
    let resolved = run(&mut tracker, &cfg, 321, 400, &BTreeMap::new(), Mode::Normal);
    assert_eq!(resolved.len(), 1);
    assert!(matches!(
        resolved[0].item,
        ItemKind::Resolved { duration_secs: 301 }
    ));
}

#[test]
fn bounce_inside_the_hold_keeps_one_incident() {
    let cfg = cfg();
    let mut tracker = Tracker::default();
    let bad = set(&[down("b")]);
    assert_eq!(run(&mut tracker, &cfg, 0, 100, &bad, Mode::Normal).len(), 1);
    assert!(run(&mut tracker, &cfg, 101, 110, &BTreeMap::new(), Mode::Normal).is_empty());
    assert!(run(&mut tracker, &cfg, 111, 200, &bad, Mode::Normal).is_empty());
    assert_eq!(tracker.open_incidents(), 1);
}

#[test]
fn flapping_node_gets_one_flap_warning() {
    let cfg = cfg();
    let mut tracker = Tracker::default();
    let bad = set(&[down("b")]);
    let mut out = Vec::new();
    // Three short appearances inside ten minutes, each shorter than confirm.
    for start in [0u64, 100, 200] {
        out.extend(run(
            &mut tracker,
            &cfg,
            start,
            start + 20,
            &bad,
            Mode::Normal,
        ));
        out.extend(run(
            &mut tracker,
            &cfg,
            start + 21,
            start + 99,
            &BTreeMap::new(),
            Mode::Normal,
        ));
    }
    let flaps: Vec<_> = out
        .iter()
        .filter(|item| item.flapping.is_some() && item.item == ItemKind::Alert)
        .collect();
    assert_eq!(flaps.len(), 1, "{out:?}");
    assert_eq!(flaps[0].severity(), Severity::Warning);
}

#[test]
fn freeze_holds_timers_and_keeps_open_incidents() {
    let cfg = cfg();
    let mut tracker = Tracker::default();
    let bad = set(&[down("b")]);
    // Frozen from the start: nothing opens.
    assert!(run(&mut tracker, &cfg, 0, 300, &bad, Mode::Freeze).is_empty());
    assert_eq!(tracker.open_incidents(), 0);
    assert_eq!(
        run(&mut tracker, &cfg, 301, 400, &bad, Mode::Normal).len(),
        1
    );
    // A freeze with the node gone does not resolve.
    assert!(run(&mut tracker, &cfg, 401, 600, &BTreeMap::new(), Mode::Freeze).is_empty());
    assert_eq!(tracker.open_incidents(), 1);
}

#[test]
fn minority_reports_only_itself_and_keeps_other_incidents() {
    let cfg = cfg();
    let mut tracker = Tracker::default();
    assert_eq!(
        run(&mut tracker, &cfg, 0, 100, &set(&[down("b")]), Mode::Normal).len(),
        1
    );
    let lost = Condition::new(EventKind::QuorumLost, "a").with_detail("无法联系 b、c");
    let minority = set(&[(lost.dedup_key(), lost)]);
    let out = run(&mut tracker, &cfg, 101, 300, &minority, Mode::Minority);
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].kind, EventKind::QuorumLost);
    // b's incident was neither resolved nor repeated.
    assert_eq!(tracker.open_incidents(), 2);
}

#[test]
fn ledger_from_another_sender_suppresses_duplicates() {
    let cfg = cfg();
    let mut tracker = Tracker::default();
    let bad = set(&[down("b")]);
    let mut ledger = NotifyLedger::default();
    ledger.record_notified("cluster.node_down:b", 50);
    for now in 0..=200 {
        tracker.reconcile(now, &bad, Mode::Normal, &cfg);
        tracker.tick(now, &cfg, &ledger);
        assert!(tracker.take_batch(now, &cfg).is_empty());
    }
    // The incident counts as notified: its resolution is still sent here.
    let mut out = Vec::new();
    for now in 201..=300 {
        tracker.reconcile(now, &BTreeMap::new(), Mode::Normal, &cfg);
        tracker.tick(now, &cfg, &ledger);
        out.extend(tracker.take_batch(now, &cfg));
    }
    assert_eq!(out.len(), 1);
    assert!(matches!(out[0].item, ItemKind::Resolved { .. }));
}

#[test]
fn coalesces_simultaneous_failures_and_puts_critical_first() {
    let cfg = cfg();
    let mut tracker = Tracker::default();
    let degraded = Condition::new(EventKind::NodeDegraded, "c")
        .with_variant("ffmpeg_repeated_failures")
        .with_detail("ffmpeg 反复失败");
    let active = set(&[down("b"), (degraded.dedup_key(), degraded)]);
    let ledger = NotifyLedger::default();
    let mut batches = Vec::new();
    for now in 0..=200 {
        tracker.reconcile(now, &active, Mode::Normal, &cfg);
        tracker.tick(now, &cfg, &ledger);
        let batch = tracker.take_batch(now, &cfg);
        if !batch.is_empty() {
            batches.push(batch);
        }
    }
    // The latched fault skips confirm, the down waits 60 s: two batches,
    // but never one message per tick.
    assert_eq!(batches.len(), 2);
    assert_eq!(batches[0][0].kind, EventKind::NodeDegraded);
    assert_eq!(batches[1][0].kind, EventKind::NodeDown);
}

#[test]
fn cooldown_folds_repeats_into_the_next_message() {
    let cfg = cfg();
    let mut tracker = Tracker::default();
    let bad = set(&[down("b")]);
    assert_eq!(run(&mut tracker, &cfg, 0, 100, &bad, Mode::Normal).len(), 1);
    assert_eq!(
        run(&mut tracker, &cfg, 101, 200, &BTreeMap::new(), Mode::Normal).len(),
        1
    );
    // Back down well inside the 600 s critical cooldown: suppressed.
    assert!(run(&mut tracker, &cfg, 201, 400, &bad, Mode::Normal).is_empty());
    assert_eq!(tracker.open_incidents(), 1);
}

#[test]
fn quiet_hours_delay_warnings_but_not_critical() {
    let mut cfg = cfg();
    cfg.quiet = parse_quiet_hours("23:00-07:00");
    // 2026-10-11 00:30 UTC+8 = 2026-10-10 16:30 UTC.
    let now = 1_791_621_000 - 1_791_621_000 % 86_400 + 16 * 3600 + 30 * 60;
    assert_eq!(cfg.release_at(Severity::Critical, now), now);
    let release = cfg.release_at(Severity::Warning, now);
    assert_eq!((release + 8 * 3600) % 86_400, 7 * 3600);
    assert_eq!(parse_quiet_hours("25:00-01:00"), None);
}

#[test]
fn disabled_events_are_ignored() {
    let mut cfg = cfg();
    cfg.events.insert("cluster.node_down".into(), false);
    let mut tracker = Tracker::default();
    assert!(run(&mut tracker, &cfg, 0, 300, &set(&[down("b")]), Mode::Normal).is_empty());
    assert!(cfg.event_enabled(EventKind::Test));
}
