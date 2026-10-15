//! Regression coverage for the completed storage and event contracts.

use super::*;
use rusqlite::params;

fn flush_shared(path: &std::path::Path) -> std::sync::Arc<crate::history::Shared> {
    use std::sync::{atomic::AtomicU64, Arc, Mutex};
    Arc::new(crate::history::Shared {
        path: path.to_path_buf(),
        recording_since: T0 as u64,
        dropped: AtomicU64::new(0),
        status: Mutex::new(crate::history::WriterStatus {
            state: "running",
            ..Default::default()
        }),
        cfg: Mutex::new(crate::config::HistoryConfig::default()),
        enabled: true,
        secrets: Mutex::new(None),
    })
}

#[tokio::test]
async fn queued_shutdown_facts_are_committed_before_flush_ack_and_reopen() {
    use rusqlite::hooks::{AuthAction, AuthContext, Authorization, TransactionOperation};
    use std::time::Duration;
    let (w, path) = writer("shutdown-flush");
    let shared = flush_shared(&path);
    let bus = crate::events::Bus::new();
    let data = bus.register("history", 1024, |_| true);
    bus.seal();
    bus.emit_at(
        T0 as u64,
        EventKind::SessionStarted {
            seq: 1,
            platform: "YT",
            channel_id: "c".into(),
            channel_name: "n".into(),
            title: None,
            live_key: None,
            source_id: None,
            area_v2: None,
            room: 1,
        },
    );
    bus.emit_at(T0 as u64, EventKind::FfmpegStarted { seq: 1, run: 10 });
    for n in 0..500 {
        bus.emit_at(
            (T0 + n) as u64,
            EventKind::Generic {
                kind: "queued",
                subject: None,
                severity: Severity::Info,
                title: "queued".into(),
                detail: String::new(),
            },
        );
    }
    bus.emit_at(
        (T0 + 600) as u64,
        EventKind::SessionEnded {
            seq: 1,
            reason: EndReason::SourceEnded,
        },
    );
    let (flush, controls) = tokio::sync::mpsc::channel(1);
    let (ack, mut received) = tokio::sync::oneshot::channel();
    flush.try_send(ack).unwrap();
    // Pause the real writer at its first transaction. Prequeueing the flush
    // makes an implementation which acknowledges before draining observable:
    // it would acknowledge first, then enter this transaction via its normal
    // loop. No scheduler delay or post-shutdown draining can hide that bug.
    let (entered, transaction_started) = tokio::sync::oneshot::channel();
    let (resume, resumed) = std::sync::mpsc::channel();
    let mut entered = Some(entered);
    w.conn
        .authorizer(Some(move |context: AuthContext<'_>| {
            if matches!(
                context.action,
                AuthAction::Transaction {
                    operation: TransactionOperation::Begin
                }
            ) {
                if let Some(entered) = entered.take() {
                    let _ = entered.send(());
                    let _ = resumed.recv_timeout(Duration::from_secs(2));
                }
            }
            Authorization::Allow
        }))
        .unwrap();
    let worker =
        std::thread::spawn(move || crate::history::run_writer(w, data, controls, shared, false));
    tokio::time::timeout(Duration::from_secs(1), transaction_started)
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(
        received.try_recv(),
        Err(tokio::sync::oneshot::error::TryRecvError::Empty)
    ));
    resume.send(()).unwrap();
    assert!(tokio::time::timeout(Duration::from_secs(2), received)
        .await
        .unwrap()
        .unwrap());
    let read = schema::open_ro(&path).unwrap();
    assert_eq!(
        read.query_row("SELECT count(*) FROM events", [], |row| row
            .get::<_, i64>(0))
            .unwrap(),
        500,
        "the entire queued prefix is committed before acknowledgement"
    );
    assert_eq!(
        read.query_row("SELECT ended_at FROM sessions", [], |row| row
            .get::<_, i64>(0))
            .unwrap(),
        T0 + 600
    );
    drop(read);
    drop(bus);
    drop(flush);
    tokio::task::spawn_blocking(move || worker.join())
        .await
        .unwrap()
        .unwrap();
    let conn = schema::open_rw(&path).unwrap();
    let mut next = Writer::new(conn, "local", 8);
    next.apply(&ev(
        T0 + 601,
        EventKind::ProcessStarted {
            version: "reopened",
        },
    ))
    .unwrap();
    let (end, reason): (i64, String) = next
        .conn
        .query_row("SELECT ended_at,end_reason FROM sessions", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap();
    assert_eq!((end, reason.as_str()), (T0 + 600, "source_ended"));
    assert_eq!(count(&next, "events"), 500);
    assert_eq!(
        next.conn
            .query_row("SELECT ended_at FROM runs", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        T0 + 600
    );
}

#[tokio::test]
async fn failed_or_closed_writer_never_acknowledges_a_successful_flush() {
    use std::sync::{atomic::Ordering, Arc};
    use std::time::Duration;
    let (w, path) = writer("failed-shutdown-flush");
    w.conn
        .execute_batch("CREATE TRIGGER fail_flush BEFORE INSERT ON events BEGIN SELECT RAISE(ABORT,'injected history write failure'); END;")
        .unwrap();
    let shared = flush_shared(&path);
    let observed = Arc::clone(&shared);
    let bus = crate::events::Bus::new();
    let data = bus.register("history", 1024, |_| true);
    bus.seal();
    bus.emit_at(
        T0 as u64,
        EventKind::Generic {
            kind: "queued",
            subject: None,
            severity: Severity::Info,
            title: "queued".into(),
            detail: String::new(),
        },
    );
    let (flush, controls) = tokio::sync::mpsc::channel(1);
    let (ack, received) = tokio::sync::oneshot::channel();
    flush.try_send(ack).unwrap();
    let worker =
        std::thread::spawn(move || crate::history::run_writer(w, data, controls, shared, false));
    assert!(!tokio::time::timeout(Duration::from_secs(2), received)
        .await
        .unwrap()
        .unwrap());
    assert_eq!(observed.dropped.load(Ordering::Relaxed), 1);
    assert_eq!(observed.status.lock().unwrap().state, "error");
    assert!(!crate::history::request_flush(&flush, Duration::from_secs(2)).await);
    let read = schema::open_ro(&path).unwrap();
    assert_eq!(
        read.query_row("SELECT count(*) FROM events", [], |row| row
            .get::<_, i64>(0))
            .unwrap(),
        0,
        "failed flush rolls back its rows"
    );
    drop(read);
    drop(bus);
    tokio::task::spawn_blocking(move || worker.join())
        .await
        .unwrap()
        .unwrap();
    assert!(!crate::history::request_flush(&flush, Duration::from_secs(2)).await);
}

#[tokio::test]
async fn stalled_writer_flush_times_out_without_blocking_the_other_consumer() {
    use std::time::Duration;
    let bus = crate::events::Bus::new();
    let _stalled = bus.register("history", 1, |_| true);
    let mut notify = bus.register("notify", 2, |_| true);
    bus.seal();
    bus.emit_at(1, EventKind::QuorumChanged { has_quorum: true });
    assert_eq!(notify.try_recv().unwrap().at, 1);
    // The writer holds its control receiver but deliberately never polls it.
    let (flush, _controls) = tokio::sync::mpsc::channel(1);
    let waiter = tokio::spawn(async move {
        crate::history::request_flush(&flush, Duration::from_millis(25)).await
    });
    tokio::task::yield_now().await;
    bus.emit_at(2, EventKind::QuorumChanged { has_quorum: false });
    assert_eq!(notify.try_recv().unwrap().at, 2);
    assert!(!tokio::time::timeout(Duration::from_secs(1), waiter)
        .await
        .unwrap()
        .unwrap());
    assert_eq!(
        bus.dropped(),
        vec![("unsealed", 0), ("history", 1), ("notify", 0)]
    );
}

#[test]
fn v1_migration_preserves_raw_accumulators_and_unknown_legacy_rollups() {
    let dir = tmp("v1-migration");
    let path = dir.join("history.sqlite");
    let old = rusqlite::Connection::open(&path).unwrap();
    old.execute_batch("PRAGMA auto_vacuum=INCREMENTAL;")
        .unwrap();
    old.execute_batch(schema::V1).unwrap();
    old.execute_batch(
        "INSERT INTO sessions(id,eid,node,boot_id,seq,platform,channel_id,channel_name,started_at,last_seen_at,ended_at,end_reason)
         VALUES(1,'old:1:1','local',1,1,'YT','c','name',120,170,170,'source_ended');
         INSERT INTO samples_raw VALUES(1,'local',130,1,0.25,3.5,3.75,2);
         INSERT INTO samples_1m VALUES(1,'local',120,1,0.25,3.5,3.75,2);
         INSERT INTO samples_1m VALUES(1,'local',60,1,1,7,12,3);
         PRAGMA user_version=1;",
    ).unwrap();
    drop(old);
    let conn = schema::open_rw(&path).unwrap();
    let raw: (i64, f64, f64, f64, i64) = conn
        .query_row(
            "SELECT session_id,min,max,sum,count FROM samples_raw WHERE ts=130",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .unwrap();
    assert_eq!(raw, (1, 0.25, 3.5, 3.75, 2));
    let known: i64 = conn
        .query_row(
            "SELECT session_id FROM samples_1m WHERE bucket=120",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let unknown: (i64, f64, i64) = conn
        .query_row(
            "SELECT session_id,sum,count FROM samples_1m WHERE bucket=60",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(known, 1);
    assert_eq!(unknown, (0, 12.0, 3));
    assert_eq!(
        conn.query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        schema::SCHEMA_VERSION
    );
    drop(conn);
    let conn = schema::open_rw(&path).unwrap();
    assert_eq!(
        conn.query_row("SELECT sum(count) FROM samples_raw", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        2
    );
}

#[test]
fn compact_frames_keep_independent_metrics_and_session_identity() {
    let (w, _) = writer("compact-frame");
    for (metric, sid, min, max, sum, count) in [
        (1, 1, 1.25, 2.75, 4.0, 2),
        (1, 1, 0.5, 5.5, 6.0, 2),
        (2, 1, 60.0, 60.0, 60.0, 1),
        (1, 2, 99.0, 99.0, 99.0, 1),
    ] {
        w.conn.execute("INSERT INTO samples_raw(metric,node,ts,session_id,min,max,sum,count) VALUES(?1,'local',?2,?3,?4,?5,?6,?7)",params![metric,T0,sid,min,max,sum,count]).unwrap();
    }
    let a: (f64, f64, f64, i64) = w
        .conn
        .query_row(
            "SELECT min,max,sum,count FROM samples_raw WHERE metric=1 AND session_id=1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .unwrap();
    assert_eq!(a, (0.5, 5.5, 10.0, 4));
    assert_eq!(count(&w, "sample_frames"), 2);
    assert_eq!(count(&w, "samples_raw"), 3);
    w.conn
        .execute(
            "DELETE FROM samples_raw WHERE metric=1 AND session_id=1",
            [],
        )
        .unwrap();
    assert_eq!(count(&w, "samples_raw"), 2);
    assert_eq!(count(&w, "sample_frames"), 2);
}

#[test]
fn v1_same_second_mixed_session_samples_remain_unattributed() {
    let dir = tmp("v1-mixed");
    let path = dir.join("history.sqlite");
    let old = rusqlite::Connection::open(&path).unwrap();
    old.execute_batch(schema::V1).unwrap();
    old.execute_batch(
        "INSERT INTO sessions(id,eid,node,boot_id,seq,platform,channel_id,channel_name,started_at,last_seen_at,ended_at,end_reason)
         VALUES(1,'a','local',1,1,'YT','a','a',120,130,130,'source_ended'),
               (2,'b','local',1,2,'YT','b','b',130,140,140,'source_ended');
         INSERT INTO samples_raw VALUES(1,'local',130,2,10,20,30,2);
         INSERT INTO samples_1m VALUES(1,'local',120,2,10,20,30,2);
         PRAGMA user_version=1;",
    ).unwrap();
    drop(old);
    let conn = schema::open_rw(&path).unwrap();
    for table in ["samples_raw", "samples_1m"] {
        let values: (i64, f64, i64) = conn
            .query_row(
                &format!("SELECT session_id,sum,count FROM {table}"),
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(
            values,
            (0, 30.0, 2),
            "mixed v1 data has no provable session owner"
        );
    }
    let mut req = q(120, 180);
    req.session = Some("b".into());
    let filtered = query::stats(&conn, &req, "1m", None).unwrap();
    assert!(filtered.series["bitrate"].iter().all(Option::is_none));
    assert_eq!(filtered.partial_before, Some(180));
}

#[test]
fn newer_schema_is_refused_without_replacing_existing_history() {
    let (w, path) = writer("newer-schema");
    w.conn
        .pragma_update(None, "user_version", schema::SCHEMA_VERSION + 1)
        .unwrap();
    drop(w);
    let before = std::fs::read(&path).unwrap();
    assert!(schema::open_rw(&path).is_err());
    assert_eq!(std::fs::read(&path).unwrap(), before);
    assert!(!path.with_extension("sqlite.corrupt").exists());
}

#[test]
fn failed_batch_rolls_back_rows_and_writer_identity_maps() {
    let (mut w, _) = writer("atomic-batch");
    w.conn.execute_batch("CREATE TRIGGER fail_event BEFORE INSERT ON events WHEN NEW.kind='injected' BEGIN SELECT RAISE(ABORT,'injected write failure'); END;").unwrap();
    let start = ev(
        T0,
        EventKind::SessionStarted {
            seq: 1,
            platform: "YT",
            channel_id: "c".into(),
            channel_name: "n".into(),
            title: None,
            live_key: None,
            source_id: None,
            area_v2: None,
            room: 1,
        },
    );
    let run = ev(T0, EventKind::FfmpegStarted { seq: 1, run: 10 });
    let rejected = ev(
        T0,
        EventKind::Generic {
            kind: "injected",
            subject: None,
            severity: Severity::Info,
            title: "failure".into(),
            detail: String::new(),
        },
    );
    assert!(w.commit(&[start.clone(), run.clone(), rejected]).is_err());
    assert_eq!(count(&w, "sessions"), 0);
    assert_eq!(count(&w, "runs"), 0);
    w.commit(&[start, run]).unwrap();
    let (id, eid, runs): (i64, String, i64) = w
        .conn
        .query_row("SELECT id,eid,runs FROM sessions", [], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })
        .unwrap();
    assert_eq!((id, eid.as_str(), runs), (1, "local:7:1", 1));
    end(&mut w, 1, T0 + 10, EndReason::SourceEnded);
    assert_eq!(sessions(&w, T0 + 20).items[0].duration_secs, 10);
}

#[test]
fn delayed_counts_use_captured_session_and_late_samples_update_one_rollup() {
    let (mut w, _) = writer("late-capture");
    let base = T0.div_euclid(60) * 60;
    session(&mut w, 1, base, None);
    w.commit(&[ev(
        base + 10,
        EventKind::StatsSample {
            seq: 1,
            bitrate_kbps: (90.0, 100.0, 110.0),
            fps: None,
            speed: None,
        },
    )])
    .unwrap();
    maintain::rollup_1m(&w.conn, base + 600).unwrap();
    w.commit(&[ev(
        base + 15,
        EventKind::StatsSample {
            seq: 1,
            bitrate_kbps: (180.0, 200.0, 220.0),
            fps: None,
            speed: None,
        },
    )])
    .unwrap();
    maintain::rollup_1m(&w.conn, base + 660).unwrap();
    let accum: (f64, f64, f64, i64) = w
        .conn
        .query_row(
            "SELECT min,max,sum,count FROM samples_1m WHERE metric=1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .unwrap();
    assert_eq!(accum, (90.0, 220.0, 300.0, 2));
    end(&mut w, 1, base + 30, EndReason::SourceEnded);
    session(&mut w, 2, base + 30, None);
    w.commit(&[
        ev(
            base + 20,
            EventKind::BiliCounts {
                seq: 1,
                room: 1,
                online: Some(42),
                watched: Some(55),
            },
        ),
        ev(
            base + 25,
            EventKind::BiliCounts {
                seq: 1,
                room: 999,
                online: Some(900),
                watched: Some(900),
            },
        ),
        ev(
            base + 40,
            EventKind::BiliCounts {
                seq: 1,
                room: 1,
                online: Some(999),
                watched: Some(999),
            },
        ),
    ])
    .unwrap();
    let rows: Vec<(i64, i64, f64)> = w
        .conn
        .prepare(
            "SELECT metric,session_id,sum FROM samples_raw WHERE metric IN (10,11) ORDER BY metric",
        )
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(rows, vec![(10, 1, 42.0), (11, 1, 55.0)]);
}

#[test]
fn retention_hides_incomplete_groups_preserves_actives_and_never_reuses_ids() {
    let (mut w, _) = writer("retained-groups");
    session(&mut w, 1, T0 - 20 * DAY, None);
    end(&mut w, 1, T0 - 19 * DAY, EndReason::SourceEnded);
    w.conn
        .execute_batch(
            "WITH RECURSIVE x(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM x WHERE n<10000)
         INSERT INTO runs(eid,session_id,run,started_at,last_seen_at,ended_at)
         SELECT 'old-run-'||n,1,n,1,2,2 FROM x;",
        )
        .unwrap();
    session(&mut w, 2, T0 - 20 * DAY, Some("active-key"));
    let retention = HistoryRetention {
        event_days: 1,
        event_rows: 1,
        ..Default::default()
    };
    maintain::tick(&w.conn, T0, &retention).unwrap();
    assert_eq!(
        w.conn
            .query_row("SELECT pruned FROM sessions WHERE seq=1", [], |r| r
                .get::<_, i64>(0))
            .unwrap(),
        1
    );
    let page = sessions(&w, T0 + 1);
    assert_eq!(page.items.len(), 1);
    assert_eq!(page.items[0].id, "local:7:3");
    assert_eq!(page.items[0].ended_at, None);
    assert_eq!(page.items[0].runs, 1);
    assert!(query::retention_floor(&w.conn).unwrap().is_some());
    for _ in 0..4 {
        maintain::tick(&w.conn, T0, &retention).unwrap();
    }
    assert_eq!(count(&w, "sessions"), 1);
    assert_eq!(count(&w, "runs"), 1);
    end(&mut w, 2, T0 + 1, EndReason::UserStop);
    maintain::tick(&w.conn, T0 + 10 * DAY, &retention).unwrap();
    assert_eq!(count(&w, "sessions"), 0);
    session(&mut w, 3, T0 + 11 * DAY, None);
    assert_eq!(
        w.conn
            .query_row("SELECT id FROM sessions", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        3
    );
}

#[test]
fn event_book_boot_and_reserved_rollup_retention_is_bounded() {
    let (mut w, _) = writer("table-retention");
    session(&mut w, 1, T0 - 10 * DAY, Some("active-key"));
    w.conn.execute_batch(
        "WITH RECURSIVE x(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM x WHERE n<9000)
         INSERT INTO events(eid,at,node,kind,severity,title,detail) SELECT 'old-'||n,n,'local','test','info','','' FROM x;
         WITH RECURSIVE x(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM x WHERE n<10)
         INSERT INTO boots(boot_id,node,started_at,last_seen_at,version) SELECT n+100,'local',n,n,'test' FROM x;
         INSERT INTO bili_lives(live_key,node,room,started_at,ended_at) VALUES
         ('active-key','local',1,1,NULL),('old-closed','local',1,1,2),('recent-closed','local',1,100,101),('live-24x7','local',2,1,NULL);
         INSERT INTO samples_1h VALUES(1,'local',0,0,1,1,1,1);",
    ).unwrap();
    let retention = HistoryRetention {
        event_rows: 1,
        event_days: 1,
        rollup_days: 1,
        ..Default::default()
    };
    maintain::tick(&w.conn, T0, &retention).unwrap();
    assert_eq!(
        count(&w, "events"),
        1000,
        "four batches of 2000 rows, not the full backlog"
    );
    assert_eq!(count(&w, "boots"), 1, "current referenced boot survives");
    assert_eq!(count(&w, "samples_1h"), 0);
    let keys: Vec<String> = w
        .conn
        .prepare("SELECT live_key FROM bili_lives ORDER BY live_key")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(keys, vec!["active-key", "live-24x7"]);
    assert!(query::retention_floor(&w.conn).unwrap().is_some());
}

#[test]
fn pinned_reader_pauses_writes_and_checkpoint_resumes_within_file_budget() {
    let (mut w, path) = writer("pinned-reader");
    session(&mut w, 1, T0, None);
    let retention = HistoryRetention {
        max_mb: 16,
        ..Default::default()
    };
    maintain::configure_limits(&w.conn, &retention).unwrap();
    w.conn
        .execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
        .unwrap();
    let reader = schema::open_ro(&path).unwrap();
    reader
        .execute_batch("BEGIN; SELECT count(*) FROM events;")
        .unwrap();
    let mut paused = false;
    for batch in 0..500 {
        if !maintain::admit_write(&w.conn, &retention).unwrap() {
            paused = true;
            break;
        }
        let events: Vec<_> = (0..200)
            .map(|offset| {
                ev(
                    T0 + batch * 200 + offset,
                    EventKind::Generic {
                        kind: "cap-test",
                        subject: None,
                        severity: Severity::Info,
                        title: "t".repeat(240),
                        detail: "d".repeat(240),
                    },
                )
            })
            .collect();
        w.commit(&events).unwrap();
    }
    assert!(paused, "a pinned WAL cannot grow without limit");
    let space = maintain::space(&w.conn, &retention).unwrap();
    assert!(space.db_bytes + space.wal_bytes <= space.limit_bytes);
    assert_eq!(
        w.conn
            .query_row("SELECT ended_at FROM sessions", [], |r| r
                .get::<_, Option<i64>>(0))
            .unwrap(),
        None
    );
    reader.execute_batch("ROLLBACK").unwrap();
    assert!(maintain::admit_write(&w.conn, &retention).unwrap());
    assert_eq!(maintain::space(&w.conn, &retention).unwrap().wal_bytes, 0);
    end(&mut w, 1, T0 + 10, EndReason::UserStop);
}

#[test]
fn pinned_reader_bounds_wal_across_a_complete_maintenance_tick() {
    let (mut w, path) = writer("pinned-maintenance");
    session(&mut w, 1, T0, None);
    let retention = HistoryRetention {
        max_mb: 16,
        event_days: 1,
        ..Default::default()
    };
    let mut batch = 0;
    while maintain::space(&w.conn, &retention).unwrap().db_bytes < 11 * 1024 * 1024 + 512 * 1024 {
        let events: Vec<_> = (0..200)
            .map(|offset| {
                ev(
                    T0 - 200 * DAY + batch * 200 + offset,
                    EventKind::Generic {
                        kind: "cap-test",
                        subject: None,
                        severity: Severity::Info,
                        title: "t".repeat(240),
                        detail: "d".repeat(240),
                    },
                )
            })
            .collect();
        w.commit(&events).unwrap();
        let attempts: Vec<_> = (0..200)
            .map(|offset| {
                ev(
                    T0 - 200 * DAY + batch * 200 + offset,
                    EventKind::NotifySent {
                        key: "k".repeat(120),
                        dedup: "d".repeat(240),
                        phase: "fire",
                        mode: Some("active"),
                        ok: true,
                        code: None,
                        trace_id: Some("t".repeat(80)),
                        test: false,
                        suppressed: 0,
                        undelivered: 0,
                    },
                )
            })
            .collect();
        w.commit(&attempts).unwrap();
        batch += 1;
    }
    maintain::configure_limits(&w.conn, &retention).unwrap();
    w.conn
        .execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
        .unwrap();
    let reader = schema::open_ro(&path).unwrap();
    reader
        .execute_batch("BEGIN; SELECT count(*) FROM events;")
        .unwrap();
    let initial_events = count(&w, "events");
    maintain::tick(&w.conn, T0, &retention).unwrap();
    let space = maintain::space(&w.conn, &retention).unwrap();
    assert!(
        space.db_bytes + space.wal_bytes <= space.limit_bytes,
        "one maintenance tick exceeded the combined cap: {space:?}"
    );
    assert!(
        count(&w, "events") < initial_events,
        "bounded cleanup still makes progress"
    );
    assert_eq!(count(&w, "sessions"), 1, "active session survives cleanup");
    reader.execute_batch("ROLLBACK").unwrap();
    assert!(maintain::admit_write(&w.conn, &retention).unwrap());
    for _ in 0..20 {
        maintain::tick(&w.conn, T0, &retention).unwrap();
        if count(&w, "events") == 0 && count(&w, "notify_sends") == 0 {
            break;
        }
    }
    assert_eq!(
        count(&w, "events"),
        0,
        "cleanup resumes after the reader releases"
    );
    assert_eq!(count(&w, "notify_sends"), 0, "every table resumes cleanup");
}

#[test]
#[ignore = "exports sanitized real writer/query responses for compiled-browser acceptance"]
fn export_browser_history_fixture() {
    let (mut w, path) = writer_node("browser-fixture", "ca");
    let now = crate::storage::now() as i64;
    let start = now - 1200;
    w.commit(&[
        ev(
            start,
            EventKind::NodeState {
                node_id: "ca".into(),
                state: crate::events::NodeState::Up,
                reason: "healthy".into(),
                initial: true,
                minority: false,
            },
        ),
        ev(
            start,
            EventKind::NodeState {
                node_id: "jp".into(),
                state: crate::events::NodeState::Up,
                reason: "healthy".into(),
                initial: true,
                minority: false,
            },
        ),
        ev(
            start,
            EventKind::SessionStarted {
                seq: 1,
                platform: "YT",
                channel_id: "example-channel".into(),
                channel_name: "示例频道".into(),
                title: Some("画面与声音测试".into()),
                live_key: Some("synthetic-browser-live".into()),
                source_id: None,
                area_v2: Some(235),
                room: 1,
            },
        ),
        ev(start, EventKind::FfmpegStarted { seq: 1, run: 10 }),
    ])
    .unwrap();
    for offset in (0..600).step_by(5) {
        let at = start + offset;
        if offset % 10 == 0 && !(300..320).contains(&offset) {
            let bitrate = if (280..300).contains(&offset) {
                600.0
            } else {
                3200.0
            };
            w.apply(&ev(
                at,
                EventKind::StatsSample {
                    seq: 1,
                    bitrate_kbps: (bitrate - 100.0, bitrate, bitrate + 100.0),
                    fps: Some(30.0),
                    speed: Some(1.0),
                },
            ))
            .unwrap();
        }
        w.apply(&ev(
            at,
            EventKind::BiliCounts {
                seq: 1,
                room: 1,
                online: Some(100 + (offset % 100) as u64),
                watched: Some((offset + 1) as u64),
            },
        ))
        .unwrap();
        if offset == 300 {
            w.apply(&ev(
                at,
                EventKind::FfmpegExited {
                    seq: 1,
                    run: 10,
                    code: Some(1),
                    success: false,
                    counted_failure: true,
                    diagnostic: Some("connection reset".into()),
                },
            ))
            .unwrap();
            w.apply(&ev(
                at,
                EventKind::NodeState {
                    node_id: "jp".into(),
                    state: crate::events::NodeState::Down,
                    reason: "heartbeat_stale".into(),
                    initial: false,
                    minority: false,
                },
            ))
            .unwrap();
            w.apply(&ev(
                at,
                EventKind::NotifySent {
                    key: "node.down".into(),
                    dedup: "node:jp:1".into(),
                    phase: "fire",
                    mode: Some("active"),
                    ok: false,
                    code: Some(40034105),
                    trace_id: Some("synthetic-active-attempt".into()),
                    test: false,
                    suppressed: 0,
                    undelivered: 1,
                },
            ))
            .unwrap();
            w.apply(&ev(
                at + 1,
                EventKind::NotifySent {
                    key: "node.down".into(),
                    dedup: "node:jp:1".into(),
                    phase: "fire",
                    mode: Some("passive"),
                    ok: true,
                    code: None,
                    trace_id: Some("synthetic-passive-attempt".into()),
                    test: false,
                    suppressed: 0,
                    undelivered: 0,
                },
            ))
            .unwrap();
        }
        if offset == 320 {
            w.apply(&ev(at, EventKind::FfmpegStarted { seq: 1, run: 11 }))
                .unwrap();
        }
        if offset == 420 {
            w.apply(&ev(
                at,
                EventKind::NodeState {
                    node_id: "jp".into(),
                    state: crate::events::NodeState::Up,
                    reason: "healthy".into(),
                    initial: false,
                    minority: false,
                },
            ))
            .unwrap();
            w.apply(&ev(
                at,
                EventKind::NotifySent {
                    key: "node.up".into(),
                    dedup: "node:jp:1".into(),
                    phase: "resolve",
                    mode: Some("active"),
                    ok: true,
                    code: None,
                    trace_id: Some("synthetic-resolution".into()),
                    test: false,
                    suppressed: 0,
                    undelivered: 0,
                },
            ))
            .unwrap();
        }
    }
    w.commit(&[
        ev(
            start + 600,
            EventKind::FfmpegExited {
                seq: 1,
                run: 11,
                code: Some(0),
                success: true,
                counted_failure: false,
                diagnostic: None,
            },
        ),
        ev(
            start + 600,
            EventKind::SessionEnded {
                seq: 1,
                reason: EndReason::SourceEnded,
            },
        ),
        ev(
            start + 601,
            EventKind::BiliLiveSummary {
                live_key: "synthetic-browser-live".into(),
                totals: Some(BiliTotals {
                    watched_count: Some(750),
                    danmu_num: Some(80),
                    add_fans: Some(7),
                    new_fans_club: Some(2),
                    max_online: Some(210),
                    live_time_raw: Some(600),
                }),
            },
        ),
    ])
    .unwrap();
    session(&mut w, 2, now - 300, Some("synthetic-failed-live"));
    w.apply(&ev(
        now - 20,
        EventKind::BiliCounts {
            seq: 2,
            room: 1,
            online: Some(35),
            watched: Some(120),
        },
    ))
    .unwrap();
    end(&mut w, 2, now - 10, EndReason::UserStop);
    w.apply(&ev(
        now - 5,
        EventKind::BiliLiveSummary {
            live_key: "synthetic-failed-live".into(),
            totals: None,
        },
    ))
    .unwrap();
    w.apply(&ev(now, EventKind::RecorderHeartbeat)).unwrap();
    let mut query = q(now - DAY, now + 1);
    query.max_points = 200;
    let summary = query::render_summary(&query::summary(&w.conn, &query, "ca", now).unwrap(), &[]);
    assert_eq!(summary["kpis"]["failures"], 1);
    assert_eq!(summary["kpis"]["notify_sent"], 3);
    assert_eq!(summary["kpis"]["watched"], 870);
    let page = query::sessions(&w.conn, &query, now).unwrap();
    let mut cfg = cluster_cfg();
    cfg.cluster.node_id = "ca".into();
    let mut status = api::status_value(&cfg);
    status["rows"] = query::status_rows(&w.conn).unwrap();
    status["oldest_at"] = json!(start);
    status["recording_since"] = json!(start);
    status["db_bytes"] = json!(
        maintain::space(&w.conn, &HistoryRetention::default())
            .unwrap()
            .db_bytes
    );
    status["writer"] = json!({"state":"running","last_batch_at":now,"last_error":null});
    status["recorder"] = json!("primary");
    status["cluster_nodes"] = json!(["ca", "jp"]);
    let directory = std::path::Path::new("/tmp/bilistream-completion-history-fixture");
    std::fs::create_dir_all(directory).unwrap();
    for (name, data) in [
        ("status", status),
        ("summary", summary),
        ("sessions", serde_json::to_value(&page).unwrap()),
        (
            "events",
            serde_json::to_value(query::events(&w.conn, &query).unwrap()).unwrap(),
        ),
        (
            "stats",
            query::render_stats(&query::stats(&w.conn, &query, "raw", None).unwrap()),
        ),
    ] {
        let bytes = serde_json::to_vec_pretty(&json!({"success":true,"data":data})).unwrap();
        assert!(!String::from_utf8_lossy(&bytes).contains("synthetic-browser-live"));
        assert!(!String::from_utf8_lossy(&bytes).contains("synthetic-failed-live"));
        std::fs::write(directory.join(format!("{name}.json")), bytes).unwrap();
    }
    for item in page.items {
        let mut session_query = query.clone();
        session_query.session = Some(item.id.clone());
        session_query.from = item.started_at;
        session_query.to = item.ended_at.unwrap_or(now).max(item.started_at + 1);
        let data =
            query::render_stats(&query::stats(&w.conn, &session_query, "raw", None).unwrap());
        std::fs::write(
            directory.join(format!("stats-{}.json", item.id.replace(':', "_"))),
            serde_json::to_vec_pretty(&json!({"success":true,"data":data})).unwrap(),
        )
        .unwrap();
    }
    std::fs::write(
        directory.join("meta.json"),
        serde_json::to_vec_pretty(&json!({"from":query.from,"to":query.to,"now":now,"db":path}))
            .unwrap(),
    )
    .unwrap();
    eprintln!("sanitized history fixture: {}", directory.display());
}
