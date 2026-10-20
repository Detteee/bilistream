# `history-v6-28a20792.sqlite`

This is a history database at schema **v6**, written by commit
`28a20792242676ce39b5a71e0539b19930120bd5` itself. Every value in it is synthetic.

- sha256: `ac514528b80dbc43e137e001fc019dea81be713c674f5d113d9ce0a75cedbaf9`
- size: 155648 bytes
- `user_version` 6, `auto_vacuum` 2 (incremental), WAL mode, checkpointed with `PRAGMA wal_checkpoint(TRUNCATE)`. There is no `-wal`/`-shm` file.
- It has rows in `boots`, `sessions` (one ended YouTube session and one open Twitch session), `runs`, `bili_lives` (with a summary), `sample_frames` (behind the `samples_raw` view), `samples_1m`, `node_segments`, `events`, `notify_sends` and `meta`.

## How it was made

1. The database was built in a throwaway clone at exactly `28a20792`, which was never committed.
2. It was written by that commit's real `schema::open_rw` and `Writer::commit` and `heartbeat`, followed by `maintain::rollup_1m`.
3. It was then checkpointed.

The generator was an ignored test appended to that clone's `src/history/tests.rs`:

```diff
diff --git a/src/history/tests.rs b/src/history/tests.rs
index 7bcbf1c..2fbb87a 100644
--- a/src/history/tests.rs
+++ b/src/history/tests.rs
@@ -2841,3 +2841,52 @@ fn observations_for_a_cleared_session_are_dropped_not_reinserted() {
         assert_eq!(count(&w, table), 0, "{table} was resurrected");
     }
 }
+
+/// Throwaway generator for the v6 fixture (never committed in this worktree).
+#[test]
+#[ignore]
+fn generate_v6_fixture() {
+    use super::event::Severity;
+    let _ = Severity::Info;
+    let path = std::path::PathBuf::from(std::env::var("FIXTURE_OUT").unwrap());
+    let _ = std::fs::remove_file(&path);
+    let conn = schema::open_rw(&path).unwrap();
+    let mut w = Writer::new(conn, "fixture-a", 3);
+    w.set_secrets(vec!["SYNTHKEY-0123456789".into()]);
+    let t = T0;
+    let kinds = vec![
+        ev(t - 10, EventKind::ProcessStarted { version: "28a20792" }),
+        ev(t, EventKind::NodeState { node_id: "fixture-b".into(), state: super::event::NodeState::Up, reason: "heartbeat".into(), initial: true, minority: false }),
+        ev(t, EventKind::QuorumChanged { has_quorum: true }),
+        ev(t + 1, EventKind::SessionStarted { seq: 1, platform: "YT", channel_id: "UCsynthetic0001".into(), channel_name: "示例频道".into(), title: Some("示例直播".into()), live_key: Some("synthetic-live-key-0001".into()), source_id: Some("synthvideo01".into()), area_v2: Some(235), room: 1001 }),
+        ev(t + 1, EventKind::BiliLiveStarted { live_key: "synthetic-live-key-0001".into(), room: 1001 }),
+        ev(t + 2, EventKind::FfmpegStarted { seq: 1, run: 1 }),
+        ev(t + 60, EventKind::StatsSample { seq: 1, bitrate_kbps: (5800.0, 6200.0, 6000.0), fps: Some(60.0), speed: Some(1.0) }),
+        ev(t + 65, EventKind::BiliCounts { seq: 1, room: 1001, online: Some(120), watched: Some(345) }),
+        ev(t + 120, EventKind::StatsSample { seq: 1, bitrate_kbps: (5000.0, 7000.0, 6100.0), fps: Some(59.5), speed: Some(0.99) }),
+        ev(t + 125, EventKind::BiliCounts { seq: 1, room: 1001, online: Some(150), watched: Some(400) }),
+        ev(t + 130, EventKind::BiliWarning { seq: Some(1), room: 1001, msg: "示例警告 SYNTHKEY-0123456789".into() }),
+        ev(t + 140, EventKind::FfmpegExited { seq: 1, run: 1, code: Some(1), success: false, counted_failure: true, diagnostic: Some("synthetic diagnostic".into()) }),
+        ev(t + 142, EventKind::FfmpegStarted { seq: 1, run: 2 }),
+        ev(t + 180, EventKind::StatsSample { seq: 1, bitrate_kbps: (6000.0, 6000.0, 6000.0), fps: Some(60.0), speed: Some(1.0) }),
+        ev(t + 200, EventKind::FfmpegStuck { seq: 1, run: 2, reason: "no_progress" }),
+        ev(t + 210, EventKind::LocalFault { reason: "synthetic fault".into(), latched: false }),
+        ev(t + 240, EventKind::FfmpegExited { seq: 1, run: 2, code: Some(0), success: true, counted_failure: false, diagnostic: None }),
+        ev(t + 241, EventKind::SessionEnded { seq: 1, reason: EndReason::SourceEnded }),
+        ev(t + 242, EventKind::BiliLiveStopped { live_key: "synthetic-live-key-0001".into(), room: 1001 }),
+        ev(t + 250, EventKind::BiliLiveSummary { live_key: "synthetic-live-key-0001".into(), totals: Some(BiliTotals { live_time_secs: Some(241), add_fans: Some(3), new_fans_club: Some(1), danmu_num: Some(42), max_online: Some(150), watched_count: Some(400) }) }),
+        ev(t + 260, EventKind::NotifySent { key: "stream.live:configured:示例频道".into(), dedup: "synthetic-dedup-1".into(), phase: "fire", mode: Some("active"), ok: true, code: Some(0), trace_id: Some("synthetic-trace-1".into()), test: false, suppressed: 0, undelivered: 0 }),
+        ev(t + 300, EventKind::SessionStarted { seq: 2, platform: "TW", channel_id: "synthlogin".into(), channel_name: "示例频道二".into(), title: None, live_key: None, source_id: Some("synthstream02".into()), area_v2: None, room: 1001 }),
+        ev(t + 301, EventKind::FfmpegStarted { seq: 2, run: 3 }),
+        ev(t + 330, EventKind::StatsSample { seq: 2, bitrate_kbps: (3000.0, 3500.0, 3200.0), fps: Some(30.0), speed: None }),
+        ev(t + 340, EventKind::NodeState { node_id: "fixture-b".into(), state: super::event::NodeState::Degraded, reason: "synthetic".into(), initial: false, minority: false }),
+        ev(t + 350, EventKind::OwnerChanged { from: Some("fixture-b".into()), to: Some("fixture-a".into()), failover: false }),
+    ];
+    w.commit(&kinds).unwrap();
+    w.heartbeat((t + 360) as u64).unwrap();
+    maintain::rollup_1m(&w.conn, t + 420).unwrap();
+    drop(w);
+    let conn = rusqlite::Connection::open(&path).unwrap();
+    conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(())).unwrap();
+    drop(conn);
+}
```

It was run as:

```
FIXTURE_OUT=/path/history-v6-28a20792.sqlite cargo test --locked --lib generate_v6_fixture -- --ignored
```

## Tests that use it

The R7 guard tests in `src/history/tests.rs` use it. Each test copies the bytes to a temporary directory first, so the fixture itself is never opened in place.
