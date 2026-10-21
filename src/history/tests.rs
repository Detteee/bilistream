//! 统计 history backend tests (design §12). Fixtures are the real,
//! key-stripped captures copied byte-for-byte into `src/history/fixtures/`
//! (capture mode writes them to `.trellis/fixtures/`, see `capture.rs`).

use super::enrich::{parse_room_live_key, parse_stop_live_data, StopLiveDataError};
use super::event::{BiliTotals, EndReason, Event, EventKind};
use super::live_book::{BiliLiveBook, BiliLiveMark, SummaryMark, SummaryPhase};
use super::query::{self, Query};
use super::{maintain, schema, writer::Writer};
use crate::config::HistoryRetention;
use crate::webui::api::history::{self as api, PeerRequest, Peers, Scope};
use serde_json::{json, Value};
use std::collections::HashMap;

const UNKNOWN_KEY: &str = include_str!("fixtures/stoplivedata_unknown_key.json");
const LIVE_SESSION: &str = include_str!("fixtures/stoplivedata_live_session.json");
const ROOM_GET_INFO: &str = include_str!("fixtures/room_get_info.json");
const DAY: i64 = 86_400;
const T0: i64 = 1_791_000_000;

#[path = "completion_tests.rs"]
mod completion;

#[path = "performance_tests.rs"]
mod performance;

fn tmp(name: &str) -> std::path::PathBuf {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "bilistream-history-{name}-{}-{}-{}",
        std::process::id(),
        crate::storage::now(),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn writer(name: &str) -> (Writer, std::path::PathBuf) {
    writer_node(name, "local")
}

fn writer_node(name: &str, node: &str) -> (Writer, std::path::PathBuf) {
    let dir = tmp(name);
    let path = dir.join("history.sqlite");
    let conn = schema::open_rw(&path).unwrap();
    let mut w = Writer::new(conn, node, 7);
    w.apply(&ev(T0 - 10, EventKind::ProcessStarted { version: "test" }))
        .unwrap();
    w.set_secrets(vec!["SYNTHKEY-0123456789".into()]);
    (w, path)
}

fn ev(at: i64, kind: EventKind) -> Event {
    Event {
        at: at as u64,
        kind,
    }
}

fn session(w: &mut Writer, seq: u64, at: i64, live_key: Option<&str>) {
    session_parts(w, seq, at, live_key, None);
}

fn session_parts(
    w: &mut Writer,
    seq: u64,
    at: i64,
    live_key: Option<&str>,
    source_id: Option<&str>,
) {
    w.apply(&ev(
        at,
        EventKind::SessionStarted {
            area_v2: None,
            room: 1,
            seq,
            platform: "YT",
            channel_id: "UC1".into(),
            channel_name: "示例频道".into(),
            title: Some("示例直播".into()),
            live_key: live_key.map(str::to_string),
            source_id: source_id.map(str::to_string),
        },
    ))
    .unwrap();
    w.apply(&ev(at, EventKind::FfmpegStarted { seq, run: seq * 10 }))
        .unwrap();
}

fn end(w: &mut Writer, seq: u64, at: i64, reason: EndReason) {
    w.apply(&ev(at, EventKind::SessionEnded { seq, reason }))
        .unwrap();
}

fn q(from: i64, to: i64) -> Query {
    Query {
        from,
        to,
        limit: 50,
        max_points: 1000,
        ..Default::default()
    }
}

fn sessions(w: &Writer, now: i64) -> query::SessionsPage {
    query::sessions(&w.conn, &q(T0 - DAY, now), now).unwrap()
}

/// Insert a relay session row directly so a boundary can be placed exactly.
fn raw_session(w: &Writer, eid: &str, seq: i64, started_at: i64, ended_at: Option<i64>) {
    w.conn
        .execute(
            "INSERT INTO sessions (eid,node,boot_id,seq,platform,channel_id,channel_name,
             started_at,last_seen_at,ended_at)
             VALUES (?1,'local',1,?4,'YT','chan','name',?2,coalesce(?3,?2),?3)",
            rusqlite::params![eid, started_at, ended_at, seq],
        )
        .unwrap();
}

/// A still-running relay session that began before the window must still report
/// its in-window ffmpeg-run events. Its bookends stay hidden by design: the
/// feed keeps only events whose timestamp falls inside `[from, to]`.
#[test]
fn an_open_relay_session_reports_its_in_window_run_events() {
    let (w, _) = writer("open-relay-run-events");
    let from = T0;
    let to = T0 + 10 * DAY;
    raw_session(&w, "open-before-from", 1, from - DAY, None);
    let session_id: i64 = w
        .conn
        .query_row(
            "SELECT id FROM sessions WHERE eid='open-before-from'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    w.conn
        .execute(
            "INSERT INTO runs (eid,session_id,run,started_at,last_seen_at,ended_at,exit_code,
             success,counted_failure,diagnostic)
             VALUES ('open-before-from/1',?1,1,?2,?3,?3,1,0,1,'ffmpeg 退出')",
            rusqlite::params![session_id, from + 60, from + 120],
        )
        .unwrap();

    let feed = query::events(&w.conn, &q(from, to)).unwrap();
    let kinds: Vec<(&str, i64)> = feed.items.iter().map(|e| (e.kind.as_str(), e.at)).collect();
    assert!(
        feed.items.iter().any(|e| e.kind == "stream.failed"),
        "the in-window failure of a still-running session must be reported: {kinds:?}"
    );
}

/// The list and the KPI/calendar paths must agree about the window boundary.
/// They used to spell the overlap predicate four ways with `>` and `>=` mixed,
/// so a session ending exactly at `from` was summarised but not listed.
#[test]
fn the_window_boundary_is_the_same_for_the_list_and_the_summary() {
    let (w, _) = writer("overlap-boundary");
    let from = T0;
    let to = T0 + 10 * DAY;
    // Starts exactly at `from`: inside the half-open window.
    raw_session(&w, "starts-at-from", 1, from, Some(from + 600));
    // Ends exactly at `from`: outside it.
    raw_session(&w, "ends-at-from", 2, from - 600, Some(from));

    let page = query::sessions(&w.conn, &q(from, to), to).unwrap();
    let listed: Vec<&str> = page.items.iter().map(|s| s.eid.as_str()).collect();
    let summed = query::load_sessions(&w.conn, from, to, to).unwrap();

    assert!(listed.contains(&"starts-at-from"), "{listed:?}");
    assert!(!listed.contains(&"ends-at-from"), "{listed:?}");
    assert_eq!(
        listed.len(),
        summed.len(),
        "the summary path ({}) must load exactly the rows the list shows ({listed:?})",
        summed.len()
    );
}

/// D3 (decided 2026-10-19): B站 live-window totals belong to the *live window*,
/// not to the query window. `kpis()` reads the whole `bili_lives` row for every
/// in-range relay session carrying that `live_key`, so two overlapping ranges
/// each report the same cumulative figures — the short window does not get a
/// "proportional share", because a cumulative counter has no meaningful share.
///
/// This is intended behaviour, not a clipping bug: the UI shows one live
/// window's totals once and must not add ranges together. The test pins that so
/// a future "fix" that clips to `[from, to)` has to argue with a test that says
/// why it should not.
#[test]
fn bili_live_window_totals_repeat_across_overlapping_queries() {
    let (mut w, _) = writer("bili-live-window-totals");
    session(&mut w, 1, T0, Some("K1"));
    end(&mut w, 1, T0 + 3600, EndReason::SourceEnded);
    let totals = parse_stop_live_data(&serde_json::from_str(LIVE_SESSION).unwrap()).ok();
    w.apply(&ev(
        T0 + 3700,
        EventKind::BiliLiveSummary {
            live_key: "K1".into(),
            totals,
        },
    ))
    .unwrap();

    let render = |from: i64, to: i64| {
        let parts = query::summary(&w.conn, &q(from, to), "local", to).unwrap();
        query::render_summary(&parts, &[])
    };
    // The whole live window.
    let whole = render(T0, T0 + 4000);
    // Overlaps the live window by its last 600 s (a sixth of it).
    let tail = render(T0 + 3000, T0 + 7000);

    for key in ["watched", "danmu", "add_fans", "max_online"] {
        assert_eq!(
            whole["kpis"][key], tail["kpis"][key],
            "{key} is a property of the live window, so a partially overlapping query must report it unchanged"
        );
    }
    assert_eq!(whole["kpis"]["watched"], 469);
    assert_eq!(
        tail["kpis"]["watched"], 469,
        "not 469/6: counters are not clipped"
    );
    assert_eq!(whole["kpis"]["danmu"], 457);
}

// ------------------------------------------------------------- schema

#[test]
fn new_file_is_incremental_vacuum_and_corrupt_file_is_recreated() {
    let dir = tmp("schema");
    let path = dir.join("history.sqlite");
    let conn = schema::open_rw(&path).unwrap();
    let mode: i64 = conn
        .query_row("PRAGMA auto_vacuum", [], |r| r.get(0))
        .unwrap();
    let version: i64 = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!((mode, version), (2, schema::SCHEMA_VERSION));
    drop(conn);
    std::fs::write(
        &path,
        b"this is not a sqlite database at all, just junk bytes....",
    )
    .unwrap();
    let conn = schema::open_rw(&path).unwrap();
    let version: i64 = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(version, schema::SCHEMA_VERSION);
    assert!(path.with_extension("sqlite.corrupt").exists());
}

#[test]
fn read_path_indexes_exist_after_migration() {
    let dir = tmp("indexes");
    let conn = schema::open_rw(&dir.join("history.sqlite")).unwrap();
    let indexes: String = conn
        .query_row(
            "SELECT group_concat(name, ' ') FROM sqlite_master WHERE type='index'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    for index in [
        "sessions_start",
        "sample_frames_session",
        "node_segments_start",
    ] {
        assert!(indexes.contains(index), "missing {index} in {indexes}");
    }
}

#[test]
fn a_fresh_database_is_v7_without_the_dead_rollup_table() {
    let dir = tmp("fresh-v7");
    let conn = schema::open_rw(&dir.join("history.sqlite")).unwrap();
    let version: i64 = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(version, 7);
    for table in ["samples_1h", "bili_lives_v6"] {
        let count: i64 = conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE name=?1",
                [table],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 0, "{table} is not part of the baseline");
    }
    let columns: String = conn
        .query_row(
            "SELECT group_concat(name,' ') FROM pragma_table_info('bili_lives')",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(!columns.contains("summary_state"), "{columns}");
    assert!(columns.contains("live_time_secs"), "{columns}");
    let meta: Vec<(String, String)> = conn
        .prepare("SELECT key,value FROM meta ORDER BY key")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(
        meta,
        vec![
            ("legacy_unattributed_before".into(), "0".into()),
            ("session_id_highwater".into(), "0".into()),
        ]
    );
}

// ------------------------------------------- v7 baseline guard (R7)

/// A v6 database written by 28a20792's own `schema::open_rw` and writer
/// (see `fixtures/README.md`).
const V6_FIXTURE: &[u8] = include_bytes!("fixtures/history-v6-28a20792.sqlite");

fn v6_fixture(name: &str) -> std::path::PathBuf {
    let path = tmp(name).join("history.sqlite");
    std::fs::write(&path, V6_FIXTURE).unwrap();
    path
}

fn rows(conn: &rusqlite::Connection, sql: &str) -> Vec<String> {
    let mut stmt = conn.prepare(sql).unwrap();
    let width = stmt.column_count();
    stmt.query_map([], |r| {
        (0..width)
            .map(|i| {
                r.get::<_, rusqlite::types::Value>(i)
                    .map(|v| format!("{v:?}"))
            })
            .collect::<rusqlite::Result<Vec<_>>>()
            .map(|cells| cells.join("|"))
    })
    .unwrap()
    .collect::<rusqlite::Result<_>>()
    .unwrap()
}

/// Everything that defines the schema, in a stable order.
fn schema_shape(conn: &rusqlite::Connection) -> Vec<String> {
    let mut shape = vec![
        format!("auto_vacuum={:?}", rows(conn, "PRAGMA auto_vacuum")),
        format!("user_version={:?}", rows(conn, "PRAGMA user_version")),
    ];
    shape.extend(rows(
        conn,
        "SELECT type,name,tbl_name,sql FROM sqlite_master ORDER BY type,name",
    ));
    let objects = rows(
        conn,
        "SELECT name FROM sqlite_master WHERE type IN ('table','view') ORDER BY name",
    );
    for object in objects {
        let name = object.trim_start_matches("Text(\"").trim_end_matches("\")");
        for (pragma, sql) in [
            ("table_xinfo", format!("SELECT * FROM pragma_table_xinfo('{name}') ORDER BY cid")),
            ("index_list", format!("SELECT name,\"unique\",origin,partial FROM pragma_index_list('{name}') ORDER BY name")),
            ("foreign_key_list", format!("SELECT * FROM pragma_foreign_key_list('{name}') ORDER BY id,seq")),
        ] {
            shape.extend(rows(conn, &sql).into_iter().map(|row| format!("{name} {pragma} {row}")));
        }
        for index in rows(
            conn,
            &format!("SELECT name FROM pragma_index_list('{name}') ORDER BY name"),
        ) {
            let index = index.trim_start_matches("Text(\"").trim_end_matches("\")");
            shape.extend(
                rows(
                    conn,
                    &format!("SELECT * FROM pragma_index_xinfo('{index}') ORDER BY seqno"),
                )
                .into_iter()
                .map(|row| format!("{index} index_xinfo {row}")),
            );
        }
    }
    shape
}

/// Every stored row of every table (views excluded), in a stable order.
fn contents(conn: &rusqlite::Connection) -> Vec<String> {
    let mut out = Vec::new();
    for table in rows(
        conn,
        "SELECT name FROM sqlite_master WHERE type='table' ORDER BY name",
    ) {
        let table = table.trim_start_matches("Text(\"").trim_end_matches("\")");
        let width: usize = conn
            .query_row(
                &format!("SELECT count(*) FROM pragma_table_info('{table}')"),
                [],
                |r| r.get::<_, i64>(0),
            )
            .unwrap() as usize;
        let order = (1..=width)
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join(",");
        out.extend(
            rows(conn, &format!("SELECT * FROM \"{table}\" ORDER BY {order}"))
                .into_iter()
                .map(|row| format!("{table}: {row}")),
        );
    }
    out
}

#[test]
fn the_v6_fixture_is_what_28a20792_wrote() {
    let path = v6_fixture("fixture-shape");
    let conn = rusqlite::Connection::open(&path).unwrap();
    let version: i64 = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(version, 6);
    for table in [
        "boots",
        "sessions",
        "runs",
        "bili_lives",
        "sample_frames",
        "samples_1m",
        "node_segments",
        "events",
        "notify_sends",
        "meta",
    ] {
        let n: i64 = conn
            .query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
            .unwrap();
        assert!(n > 0, "the fixture has rows in {table}");
    }
}

#[test]
fn a_fresh_v7_database_has_exactly_the_schema_of_a_bumped_28a20792_v6() {
    let fresh = schema::open_rw(&tmp("fresh-shape").join("history.sqlite")).unwrap();
    let bumped = schema::open_rw(&v6_fixture("bumped-shape")).unwrap();
    let (fresh, bumped) = (schema_shape(&fresh), schema_shape(&bumped));
    assert!(fresh.iter().any(|row| row.contains("sample_frames")));
    assert!(fresh.contains(&"user_version=[\"Integer(7)\"]".to_string()));
    assert!(fresh.contains(&"auto_vacuum=[\"Integer(2)\"]".to_string()));
    assert_eq!(fresh, bumped);
}

#[test]
fn the_v6_to_v7_bump_changes_only_user_version() {
    let path = v6_fixture("bump-rows");
    let before = contents(&rusqlite::Connection::open(&path).unwrap());
    assert!(before.len() > 30, "{} rows", before.len());
    let conn = schema::open_rw(&path).unwrap();
    assert_eq!(contents(&conn), before);
    let version: i64 = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(version, 7);
    drop(conn);
    // Reopening a v7 file is a no-op too.
    assert_eq!(contents(&schema::open_rw(&path).unwrap()), before);
}

#[test]
fn a_bumped_28a20792_database_is_written_and_read_through_the_normal_apis() {
    let path = v6_fixture("bumped-use");
    let conn = schema::open_rw(&path).unwrap();
    let mut w = Writer::new(conn, "fixture-a", 4);
    w.apply(&ev(T0 + 900, EventKind::ProcessStarted { version: "test" }))
        .unwrap();
    session(&mut w, 1, T0 + 1_000, Some("synthetic-live-key-0002"));
    w.apply(&ev(
        T0 + 1_060,
        EventKind::StatsSample {
            seq: 1,
            bitrate_kbps: (4000.0, 4400.0, 4200.0),
            fps: Some(30.0),
            speed: Some(1.0),
        },
    ))
    .unwrap();
    end(&mut w, 1, T0 + 1_100, EndReason::SourceEnded);
    let page = sessions(&w, T0 + 1_200);
    let names: Vec<String> = page
        .items
        .iter()
        .map(|item| serde_json::to_value(item).unwrap()["channel_name"].to_string())
        .collect();
    assert_eq!(page.items.len(), 3, "{names:?}");
    for name in ["示例频道", "示例频道二"] {
        assert!(
            names.iter().any(|n| n == &format!("\"{name}\"")),
            "{name} from the fixture in {names:?}"
        );
    }
    let highwater: i64 = w
        .conn
        .query_row(
            "SELECT cast(value AS INTEGER) FROM meta WHERE key='session_id_highwater'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(highwater, 3, "new session ids continue after the fixture's");
    let stats = query::stats(&w.conn, &q(T0, T0 + 1_200), "raw", None).unwrap();
    let points = stats.series["bitrate"]
        .iter()
        .filter(|c| c.is_some())
        .count();
    assert!(points >= 2, "fixture and new samples are both readable");
}

fn assert_refused(path: &std::path::Path, needle: &str) {
    let before = std::fs::read(path).unwrap();
    let error = schema::open_rw(path).unwrap_err();
    assert!(error.to_string().contains(needle), "{error}");
    assert!(!matches!(
        error.sqlite_error_code(),
        Some(rusqlite::ErrorCode::DatabaseCorrupt | rusqlite::ErrorCode::NotADatabase)
    ));
    assert_eq!(
        std::fs::read(path).unwrap(),
        before,
        "the file is untouched"
    );
    assert!(!path.with_extension("sqlite.corrupt").exists());
}

#[test]
fn versions_one_to_five_are_refused_and_left_untouched() {
    for version in 1..=5 {
        let path = v6_fixture(&format!("refuse-v{version}"));
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.pragma_update(None, "user_version", version).unwrap();
            conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))
                .unwrap();
        }
        assert_refused(&path, &format!("v{version}"));
    }
}

#[test]
fn version_zero_with_tables_is_refused_and_left_untouched() {
    let path = tmp("refuse-v0").join("history.sqlite");
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL) WITHOUT ROWID;
             INSERT INTO meta VALUES ('synthetic','1');",
        )
        .unwrap();
    }
    assert_refused(&path, "没有版本号");
}

#[test]
fn a_version_newer_than_seven_is_refused_and_left_untouched() {
    let path = v6_fixture("refuse-v8");
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.pragma_update(None, "user_version", 8).unwrap();
        conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))
            .unwrap();
    }
    assert_refused(&path, "较新");
}

/// A copy of a WAL database taken while its writer is still open, as a
/// crash would leave it: `setup` is checkpointed into the main file, `tail`
/// stays as uncheckpointed frames in `-wal`. `-shm` is copied only when
/// `keep_shm`.
fn crash_left_wal(
    name: &str,
    from_fixture: bool,
    setup: &str,
    tail: &str,
    keep_shm: bool,
) -> std::path::PathBuf {
    let staging = if from_fixture {
        v6_fixture(&format!("{name}-staging"))
    } else {
        tmp(&format!("{name}-staging")).join("history.sqlite")
    };
    let target = tmp(name).join("history.sqlite");
    let side = |p: &std::path::Path, ext: &str| p.with_file_name(format!("history.sqlite{ext}"));
    let conn = rusqlite::Connection::open(&staging).unwrap();
    conn.pragma_update(None, "journal_mode", "WAL").unwrap();
    conn.execute_batch(setup).unwrap();
    conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))
        .unwrap();
    conn.pragma_update(None, "wal_autocheckpoint", 0).unwrap();
    conn.execute_batch(tail).unwrap();
    std::fs::copy(&staging, &target).unwrap();
    std::fs::copy(side(&staging, "-wal"), side(&target, "-wal")).unwrap();
    if keep_shm {
        std::fs::copy(side(&staging, "-shm"), side(&target, "-shm")).unwrap();
    }
    drop(conn);
    assert!(
        std::fs::metadata(side(&target, "-wal")).unwrap().len() > 0,
        "the copy carries uncheckpointed WAL frames"
    );
    target
}

/// Every file in the database's directory with its bytes.
fn dir_bytes(path: &std::path::Path) -> Vec<(String, Vec<u8>)> {
    let mut out: Vec<_> = std::fs::read_dir(path.parent().unwrap())
        .unwrap()
        .map(|e| {
            let e = e.unwrap();
            (
                e.file_name().to_string_lossy().into_owned(),
                std::fs::read(e.path()).unwrap(),
            )
        })
        .collect();
    out.sort();
    out
}

fn assert_refused_with_wal(path: &std::path::Path, needle: &str, shm: bool) {
    let before = dir_bytes(path);
    let names: Vec<&str> = before.iter().map(|(n, _)| n.as_str()).collect();
    let mut expected = vec!["history.sqlite", "history.sqlite-wal"];
    if shm {
        expected.push("history.sqlite-shm");
    }
    expected.sort();
    assert_eq!(names, expected);
    let error = schema::open_rw(path).unwrap_err();
    assert!(error.to_string().contains(needle), "{error}");
    assert!(!matches!(
        error.sqlite_error_code(),
        Some(rusqlite::ErrorCode::DatabaseCorrupt | rusqlite::ErrorCode::NotADatabase)
    ));
    // Main file, -wal and -shm (or its absence) are byte-identical, and
    // nothing (.corrupt, -journal, a new -shm) was created.
    let after = dir_bytes(path);
    let changed: Vec<&str> = after
        .iter()
        .filter(|file| !before.contains(file))
        .map(|(name, _)| name.as_str())
        .chain(
            before
                .iter()
                .filter(|(name, _)| !after.iter().any(|(n, _)| n == name))
                .map(|(name, _)| name.as_str()),
        )
        .collect();
    assert!(
        changed.is_empty(),
        "{needle}: changed, created or removed {changed:?}"
    );
}

#[test]
fn a_refused_file_with_a_crash_left_wal_is_left_byte_identical() {
    let tail = "INSERT INTO meta(key,value) VALUES ('synthetic_wal_row','1');";
    for shm in [false, true] {
        let path = crash_left_wal(
            &format!("refuse-wal-v5-{shm}"),
            true,
            "PRAGMA user_version=5;",
            tail,
            shm,
        );
        assert_refused_with_wal(&path, "v5", shm);
    }
    let path = crash_left_wal(
        "refuse-wal-v0",
        false,
        "CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL) WITHOUT ROWID;
         INSERT INTO meta VALUES ('synthetic','1');",
        tail,
        false,
    );
    assert_refused_with_wal(&path, "没有版本号", false);
    let path = crash_left_wal("refuse-wal-v8", true, "PRAGMA user_version=8;", tail, true);
    assert_refused_with_wal(&path, "较新", true);
}

#[test]
fn a_v6_file_with_a_crash_left_wal_upgrades_with_the_wal_applied() {
    for shm in [false, true] {
        let tail = "INSERT INTO meta(key,value) VALUES ('synthetic_wal_row','1');";
        let path = crash_left_wal(&format!("upgrade-wal-{shm}"), true, "", tail, shm);
        let expected = {
            let staging = v6_fixture(&format!("upgrade-wal-{shm}-expected"));
            let conn = rusqlite::Connection::open(&staging).unwrap();
            conn.execute_batch(tail).unwrap();
            contents(&conn)
        };
        let conn = schema::open_rw(&path).unwrap();
        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, 7);
        let got = contents(&conn);
        assert!(got.iter().any(|row| row.contains("synthetic_wal_row")));
        assert_eq!(got, expected);
    }
}

#[test]
fn live_row_reads_the_outcome_from_the_stored_phase() {
    let dir = tmp("live-row-phase");
    let conn = schema::open_rw(&dir.join("history.sqlite")).unwrap();
    conn.execute_batch(
        "INSERT INTO bili_lives(live_key,node,room,started_at,summary_phase,watched_count) VALUES
           ('k-done','local',1,1,'done',5),
           ('k-claimed','local',1,1,'claimed',NULL),
           ('k-failed','local',1,1,'failed',NULL),
           ('k-skipped','local',1,1,'skipped',NULL),
           ('k-pending','local',1,1,'pending',NULL);",
    )
    .unwrap();
    let state = |key: &str| query::live_row(&conn, key).unwrap().unwrap().state;
    // The read-side vocabulary is unchanged even though only the phase is stored.
    assert_eq!(state("k-done"), "ok");
    assert_eq!(state("k-claimed"), "pending");
    assert_eq!(state("k-failed"), "failed");
    assert_eq!(state("k-skipped"), "skipped");
    assert_eq!(state("k-pending"), "pending");
    assert_eq!(
        query::live_row(&conn, "k-done").unwrap().unwrap().watched,
        Some(5)
    );
}

#[test]
fn a_truncated_bucket_series_says_so() {
    let (w, _) = writer("bucket-cap");
    let to = T0 + 90 * DAY;
    let mut wide = q(to - 90 * DAY, to);
    wide.bucket = Some("hour".into());
    let summary = query::summary(&w.conn, &wide, "local", to).unwrap();
    assert_eq!(summary.buckets.len(), query::MAX_BUCKETS);
    assert!(
        summary.buckets_truncated,
        "a series stopped by the cap must not look complete; kpis still totals the whole range"
    );

    let mut narrow = q(to - DAY, to);
    narrow.bucket = Some("hour".into());
    let narrow = query::summary(&w.conn, &narrow, "local", to).unwrap();
    assert!(narrow.buckets.len() < query::MAX_BUCKETS);
    assert!(!narrow.buckets_truncated);
}

#[test]
fn schema_has_no_revenue_column_and_only_seven_end_reasons() {
    let (w, _) = writer("revenue-schema");
    let sql: String = w
        .conn
        .query_row(
            "SELECT group_concat(sql, ' ') FROM sqlite_master",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let lower = sql.to_lowercase();
    assert!(!lower.contains("hamster") && !lower.contains("revenue") && !lower.contains("rmb"));
    for code in EndReason::ALL {
        assert!(sql.contains(&format!("'{}'", code.as_str())));
    }
    w.conn
        .execute(
            "INSERT INTO sessions (eid,node,boot_id,seq,platform,channel_id,channel_name,started_at,last_seen_at)
             VALUES ('x','local',1,99,'YT','c','n',1,1)",
            [],
        )
        .unwrap();
    assert!(w
        .conn
        .execute("UPDATE sessions SET end_reason='failed' WHERE eid='x'", [])
        .is_err());
    assert_eq!(EndReason::ALL.len(), 7);
    assert!(EndReason::parse("failed").is_none());
}

// ------------------------------------------------------- StopLiveData

#[test]
fn unknown_key_fixture_loaded_whole_is_a_failed_fetch() {
    let reply: Value = serde_json::from_str(UNKNOWN_KEY).unwrap();
    assert_eq!(reply["code"], 0, "the trap: code 0 with sentinels");
    assert!(reply["data"]["WatchedCount"].as_i64().unwrap() > 0);
    assert_eq!(
        parse_stop_live_data(&reply),
        Err(StopLiveDataError::Sentinel("LiveTime"))
    );
}

#[test]
fn live_session_fixture_is_valid_and_keeps_only_the_allowlist() {
    let mut reply: Value = serde_json::from_str(LIVE_SESSION).unwrap();
    reply["data"]["HamsterRmb"] = json!(12345);
    reply["data"]["SomethingElse"] = json!("x");
    let totals = parse_stop_live_data(&reply).unwrap();
    assert_eq!(
        totals,
        BiliTotals {
            live_time_secs: Some(17434),
            add_fans: Some(0),
            new_fans_club: Some(0),
            danmu_num: Some(457),
            max_online: Some(2765),
            watched_count: Some(469),
        }
    );
    let text = serde_json::to_string(&totals).unwrap();
    assert!(!text.contains("Hamster") && !text.contains("12345"));
    let redacted = super::enrich::redact_stop_live_data(&reply);
    assert!(redacted["data"].get("HamsterRmb").is_none());
    assert!(redacted["data"].get("SomethingElse").is_none());
    assert_eq!(redacted["data"].as_object().unwrap().len(), 6);
}

#[test]
fn stop_live_data_failure_paths() {
    assert_eq!(
        parse_stop_live_data(&json!({"code": -101, "message": "账号未登录"})),
        Err(StopLiveDataError::Code(-101))
    );
    assert_eq!(
        parse_stop_live_data(&json!({"code": 0})),
        Err(StopLiveDataError::NoData)
    );
    assert_eq!(
        parse_stop_live_data(&json!({"code": 0, "data": {"DanmuNum": 3, "AddFans": -1}})),
        Err(StopLiveDataError::Sentinel("AddFans"))
    );
    // Missing fields in a valid reply are NULL for that field only.
    let partial = parse_stop_live_data(&json!({"code": 0, "data": {"DanmuNum": 3}})).unwrap();
    assert_eq!(partial.danmu_num, Some(3));
    assert_eq!(partial.max_online, None);
}

#[tokio::test]
async fn fetch_retries_once_after_five_minutes_then_gives_up() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;
    let calls = AtomicUsize::new(0);
    let ok_second = super::enrich::fetch_with_retry(
        || {
            let n = calls.fetch_add(1, Ordering::SeqCst);
            async move {
                if n == 0 {
                    Err("timeout".to_string())
                } else {
                    Ok(serde_json::from_str::<Value>(LIVE_SESSION).unwrap())
                }
            }
        },
        Duration::from_millis(5),
        Duration::from_millis(30),
    )
    .await;
    assert_eq!(ok_second.unwrap().danmu_num, Some(457));
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    let started = std::time::Instant::now();
    let sentinel_twice = super::enrich::fetch_with_retry(
        || async { Ok(serde_json::from_str::<Value>(UNKNOWN_KEY).unwrap()) },
        Duration::from_millis(5),
        Duration::from_millis(30),
    )
    .await;
    assert!(sentinel_twice.is_none());
    assert!(started.elapsed() >= Duration::from_millis(35));
    assert_eq!(super::enrich::RETRY_DELAY, Duration::from_secs(300));
}

#[test]
fn failed_summary_leaves_totals_null_and_duration_is_our_own() {
    let (mut w, _) = writer("summary-null");
    session(&mut w, 1, T0, Some("K1"));
    w.apply(&ev(
        T0,
        EventKind::BiliLiveStarted {
            live_key: "K1".into(),
            room: 5,
        },
    ))
    .unwrap();
    end(&mut w, 1, T0 + 3600, EndReason::BiliStopped);
    // Session closed before any summary: pending.
    let page = sessions(&w, T0 + 4000);
    let s = &page.items[0];
    assert_eq!(s.duration_secs, 3600);
    assert_eq!(s.end_reason.as_deref(), Some("bili_stopped"));
    assert_eq!(s.bili.as_ref().unwrap().state, "pending");
    // Unknown-key reply → failed, every total NULL.
    let reply: Value = serde_json::from_str(UNKNOWN_KEY).unwrap();
    let totals = parse_stop_live_data(&reply).ok();
    w.apply(&ev(
        T0 + 3605,
        EventKind::BiliLiveSummary {
            live_key: "K1".into(),
            totals,
        },
    ))
    .unwrap();
    let nulls: (String, Option<i64>, Option<i64>, Option<i64>) = w
        .conn
        .query_row(
            "SELECT summary_phase, watched_count, live_time_secs, add_fans FROM bili_lives WHERE live_key='K1'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .unwrap();
    assert_eq!(nulls, ("failed".into(), None, None, None));
    let bili = sessions(&w, T0 + 4000).items[0].bili.clone().unwrap();
    assert_eq!(bili.state, "failed");
    assert_eq!(
        (bili.watched, bili.danmu, bili.add_fans),
        (None, None, None)
    );
    // A valid summary later (retry) fills totals; LiveTime is never the duration.
    let totals = parse_stop_live_data(&serde_json::from_str(LIVE_SESSION).unwrap()).ok();
    w.apply(&ev(
        T0 + 3900,
        EventKind::BiliLiveSummary {
            live_key: "K1".into(),
            totals,
        },
    ))
    .unwrap();
    let s = sessions(&w, T0 + 4000).items[0].clone();
    let bili = s.bili.unwrap();
    assert_eq!(
        (bili.state.as_str(), bili.watched, bili.danmu),
        ("ok", Some(469), Some(457))
    );
    assert_eq!(s.duration_secs, 3600, "never LiveTime (17434)");
    let body = serde_json::to_string(&sessions(&w, T0 + 4000)).unwrap();
    assert!(!body.contains("K1"), "live_key never in API output");
    assert!(!body.contains("Hamster") && !body.contains("live_time"));
}

#[test]
fn online_peak_comes_from_our_samples() {
    let (mut w, _) = writer("online-peak");
    session(&mut w, 1, T0, None);
    for (i, v) in [120u64, 340, 200].into_iter().enumerate() {
        w.apply(&ev(
            T0 + 10 * i as i64,
            EventKind::BiliCounts {
                seq: 1,
                room: 1,
                online: Some(v),
                watched: None,
            },
        ))
        .unwrap();
    }
    let bili = sessions(&w, T0 + 60).items[0].bili.clone().unwrap();
    assert_eq!(bili.online_peak, Some(340.0));
}

// ------------------------------------------------------------ live_key

#[test]
fn missing_live_key_is_pending_while_open_then_skipped() {
    let (mut w, _) = writer("no-key");
    session(&mut w, 1, T0, None);
    assert_eq!(
        sessions(&w, T0 + 60).items[0].bili.as_ref().unwrap().state,
        "pending"
    );
    end(&mut w, 1, T0 + 600, EndReason::SourceEnded);
    let bili = sessions(&w, T0 + 700).items[0].bili.clone().unwrap();
    assert_eq!(bili.state, "skipped");
    assert_eq!(bili.watched, None);
}

#[test]
fn getinfo_fixture_has_no_usable_key_and_a_real_one_is_adopted() {
    let reply: Value = serde_json::from_str(ROOM_GET_INFO).unwrap();
    assert_eq!(
        parse_room_live_key(&reply),
        None,
        "redacted placeholder is no key"
    );
    let mut with_key = reply.clone();
    with_key["data"]["live_key"] = json!("synthetic-live-key-1");
    assert_eq!(
        parse_room_live_key(&with_key).as_deref(),
        Some("synthetic-live-key-1")
    );
    let mut offline = with_key.clone();
    offline["data"]["live_status"] = json!(0);
    assert_eq!(parse_room_live_key(&offline), None);
    let captured = super::enrich::redact_room_info(&with_key).to_string();
    assert!(!captured.contains("synthetic-live-key-1") && !captured.contains("uid"));
}

#[test]
fn session_adopts_key_from_getinfo_after_start() {
    let (mut w, _) = writer("adopt");
    session(&mut w, 1, T0, None);
    w.apply(&ev(
        T0 + 5,
        EventKind::SessionLiveKey {
            seq: 1,
            live_key: "G1".into(),
        },
    ))
    .unwrap();
    let key: Option<String> = w
        .conn
        .query_row("SELECT live_key FROM sessions", [], |r| r.get(0))
        .unwrap();
    assert_eq!(key.as_deref(), Some("G1"));
}

#[test]
fn audience_broadcast_follows_bilibili_live_or_source_id() {
    let (mut w, _) = writer("broadcast");
    session_parts(&mut w, 1, T0, Some("synthetic-live-a"), Some("video-a"));
    end(&mut w, 1, T0 + 30, EndReason::IntentionalRestart);
    session_parts(
        &mut w,
        2,
        T0 + 40,
        Some("synthetic-live-a"),
        Some("video-a"),
    );
    end(&mut w, 2, T0 + 80, EndReason::SourceEnded);
    session_parts(&mut w, 3, T0 + 100, None, Some("video-b"));
    end(&mut w, 3, T0 + 120, EndReason::ProcessRestart);
    session_parts(&mut w, 4, T0 + 130, None, Some("video-b"));
    end(&mut w, 4, T0 + 150, EndReason::SourceEnded);
    let page = sessions(&w, T0 + 200);
    let text = serde_json::to_string(&page).unwrap();
    assert!(!text.contains("synthetic-live-a"), "{text}");
    let live: std::collections::HashSet<_> = page
        .items
        .iter()
        .filter(|s| s.source_id.as_deref() == Some("video-a"))
        .map(|s| s.broadcast_id.as_str())
        .collect();
    let source: std::collections::HashSet<_> = page
        .items
        .iter()
        .filter(|s| s.source_id.as_deref() == Some("video-b"))
        .map(|s| s.broadcast_id.as_str())
        .collect();
    assert_eq!(live.len(), 1, "{text}");
    assert!(live.iter().next().unwrap().starts_with("live:"), "{live:?}");
    assert_eq!(source, std::collections::HashSet::from(["src:YT:video-b"]));
}

fn mark(key: &str, started: u64, phase: SummaryPhase, node: Option<&str>) -> BiliLiveMark {
    BiliLiveMark {
        room: 5,
        live_key: key.into(),
        started_at: started,
        started_by: "a".into(),
        stopped_at: None,
        summary: SummaryMark {
            phase,
            node: node.map(str::to_string),
            at: 0,
            totals: None,
            generation: 0,
        },
    }
}

#[test]
fn live_book_merge_is_order_independent_bounded_and_redacted() {
    let a = BiliLiveBook {
        entries: vec![mark("K", 100, SummaryPhase::Claimed, Some("b"))],
    };
    let mut stopped = mark("K", 90, SummaryPhase::Claimed, Some("a"));
    stopped.stopped_at = Some(200);
    let b = BiliLiveBook {
        entries: vec![stopped],
    };
    let mut ab = a.clone();
    ab.join(&b);
    let mut ba = b.clone();
    ba.join(&a);
    assert_eq!(ab, ba);
    let m = &ab.entries[0];
    assert_eq!((m.started_at, m.stopped_at), (90, Some(200)));
    assert_eq!(
        m.summary.node.as_deref(),
        Some("a"),
        "ties by lower node id"
    );
    assert!(super::live_book::should_back_off(&ab, 5, "K", "b"));
    assert!(!super::live_book::should_back_off(&ab, 5, "K", "a"));
    let mut many = BiliLiveBook::default();
    for i in 0..12 {
        many.join(&BiliLiveBook {
            entries: vec![mark(&format!("K{i}"), i, SummaryPhase::Pending, None)],
        });
    }
    assert_eq!(many.entries.len(), super::live_book::MAX_ENTRIES);
    assert_eq!(many.open_key(5).unwrap().live_key, "K11");
    assert!(
        !format!("{many:?}").contains("K11"),
        "Debug never prints the key"
    );
}

#[test]
fn heartbeat_book_is_stripped_from_the_browser_cluster_view() {
    super::live_book::reset_for_test();
    super::live_book::record_started(5, "SECRET-LIVE-KEY", "local", 1);
    let book = super::live_book::heartbeat_book().unwrap();
    assert_eq!(book.entries.len(), 1);
    let mut node = crate::cluster::empty_node("a", "a", "", 0, true, 1);
    node.bili_live = Some(book);
    assert!(
        serde_json::to_string(&node)
            .unwrap()
            .contains("SECRET-LIVE-KEY"),
        "peers get it"
    );
    node.bili_live = None;
    assert!(!serde_json::to_string(&node)
        .unwrap()
        .contains("SECRET-LIVE-KEY"));
    super::live_book::reset_for_test();
}

// ---------------------------------------------------------- crash close

#[test]
fn crash_sweep_closes_older_boot_sessions_as_process_restart() {
    let (mut w, path) = writer("crash");
    session(&mut w, 1, T0, None);
    w.apply(&ev(
        T0 + 120,
        EventKind::StatsSample {
            seq: 1,
            bitrate_kbps: (5000.0, 6000.0, 7000.0),
            fps: Some(60.0),
            speed: Some(1.0),
        },
    ))
    .unwrap();
    drop(w);
    let mut w2 = Writer::new(schema::open_rw(&path).unwrap(), "local", 8);
    w2.apply(&ev(T0 + 999, EventKind::ProcessStarted { version: "test" }))
        .unwrap();
    let s = sessions(&w2, T0 + 1000).items[0].clone();
    assert_eq!(s.end_reason.as_deref(), Some("process_restart"));
    assert_eq!(s.ended_at, Some(T0 + 120), "ended_at = last_seen_at");
    assert_eq!(s.median_bitrate_kbps, Some(6000.0));
}

#[test]
fn every_end_reason_round_trips_and_open_session_has_none() {
    let (mut w, _) = writer("reasons");
    for (i, reason) in EndReason::ALL.into_iter().enumerate() {
        let seq = i as u64 + 1;
        session(&mut w, seq, T0 + i as i64 * 100, None);
        end(&mut w, seq, T0 + i as i64 * 100 + 50, reason);
    }
    session(&mut w, 99, T0 + 5000, None);
    let page = sessions(&w, T0 + 6000);
    let open = page.items.iter().find(|s| s.ended_at.is_none()).unwrap();
    assert_eq!(open.end_reason, None);
    let mut codes: Vec<_> = page
        .items
        .iter()
        .filter_map(|s| s.end_reason.clone())
        .collect();
    codes.sort();
    let mut want: Vec<_> = EndReason::ALL
        .iter()
        .map(|r| r.as_str().to_string())
        .collect();
    want.sort();
    assert_eq!(codes, want);
}

// ----------------------------------------------------------- redaction

#[test]
fn rtmp_key_never_reaches_db_bytes_or_api_bodies() {
    let (mut w, path) = writer("redact");
    w.apply(&ev(
        T0,
        EventKind::SessionStarted {
            area_v2: None,
            room: 1,
            seq: 1,
            platform: "TW",
            channel_id: "c".into(),
            channel_name: "name SYNTHKEY-0123456789".into(),
            title: Some("rtmp://live/x?key=SYNTHKEY-0123456789".into()),
            live_key: None,
            source_id: None,
        },
    ))
    .unwrap();
    w.apply(&ev(
        T0,
        EventKind::BiliWarning {
            seq: Some(1),
            room: 1,
            msg: "警告 SYNTHKEY-0123456789".into(),
            rank: 4,
            merge: true,
            stop: true,
        },
    ))
    .unwrap();
    let mut qq = q(T0 - 10, T0 + 10);
    qq.limit = 20;
    let bodies = format!(
        "{}{}",
        serde_json::to_string(&sessions(&w, T0 + 5)).unwrap(),
        serde_json::to_string(&query::events(&w.conn, &qq).unwrap()).unwrap()
    );
    assert!(bodies.contains("[已隐藏]"));
    drop(w);
    for file in [path.clone(), path.with_extension("sqlite-wal")] {
        if let Ok(bytes) = std::fs::read(&file) {
            assert!(
                !String::from_utf8_lossy(&bytes).contains("SYNTHKEY-0123456789"),
                "{file:?}"
            );
        }
    }
    assert!(!bodies.contains("SYNTHKEY-0123456789"));
}

// ------------------------------------------------- retention and rollup

fn raw(w: &Writer, ts: i64, v: f64) {
    w.conn
        .execute(
            "INSERT INTO samples_raw (metric,node,ts,session_id,min,max,sum,count) VALUES (1,'local',?1,0,?2,?2,?2,1)",
            rusqlite::params![ts, v],
        )
        .unwrap();
}

fn count(w: &Writer, table: &str) -> i64 {
    w.conn
        .query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
        .unwrap()
}

#[test]
fn rollup_is_half_open_idempotent_and_exact() {
    let (w, _) = writer("rollup");
    let base = T0 - T0 % 60;
    for i in 0..12 {
        raw(&w, base + i * 10, 100.0 + i as f64); // two full minutes
    }
    // now = base + 120 + 120 → end = floor(now-120, 60) = base+120: both minutes.
    let now = base + 240;
    assert_eq!(
        maintain::rollup_1m(&w.conn, now).unwrap(),
        Some((base, base + 120))
    );
    assert_eq!(
        maintain::rollup_1m(&w.conn, now).unwrap(),
        None,
        "watermark moved"
    );
    let rows: Vec<(i64, f64, f64, f64, i64)> = w
        .conn
        .prepare("SELECT bucket,min,max,sum,count FROM samples_1m ORDER BY bucket")
        .unwrap()
        .query_map([], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
        })
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(
        rows,
        vec![
            (base, 100.0, 105.0, 615.0, 6),
            (base + 60, 106.0, 111.0, 651.0, 6)
        ]
    );
    // A row at the boundary (base+120) belongs to the next range only.
    raw(&w, base + 120, 1.0);
    assert_eq!(maintain::rollup_1m(&w.conn, base + 240).unwrap(), None);
    assert_eq!(
        maintain::rollup_1m(&w.conn, base + 300).unwrap(),
        Some((base + 120, base + 180))
    );
    assert_eq!(count(&w, "samples_1m"), 3);
}

#[test]
fn raw_is_deleted_only_below_watermark_and_past_retention() {
    let (w, _) = writer("prune");
    let r = HistoryRetention::default();
    let now = T0;
    let old = now - 8 * DAY;
    raw(&w, old, 1.0);
    raw(&w, now - DAY, 2.0);
    // No rollup yet: nothing may be deleted even though it is past 7 d.
    assert_eq!(maintain::prune_raw(&w.conn, now, r.raw_days).unwrap(), 0);
    assert_eq!(count(&w, "samples_raw"), 2);
    // Crash between rollup and delete: watermark committed, raw survives,
    // and the next tick does not add the range twice.
    maintain::rollup_1m(&w.conn, now).unwrap();
    let before = count(&w, "samples_1m");
    let report = maintain::tick(&w.conn, now, &r).unwrap();
    assert_eq!(report.rolled, None);
    assert_eq!(count(&w, "samples_1m"), before);
    assert_eq!(report.raw_deleted, 1, "only the 8-day-old row");
    assert_eq!(count(&w, "samples_raw"), 1);
}

#[test]
fn rollups_and_events_expire_at_their_edges() {
    let (w, _) = writer("expire");
    let r = HistoryRetention {
        event_rows: 3,
        ..Default::default()
    };
    let now = T0;
    for (bucket, v) in [(now - 90 * DAY - 60, 1.0), (now - 90 * DAY, 2.0)] {
        w.conn
            .execute(
                "INSERT INTO samples_1m (metric,node,bucket,min,max,sum,count) VALUES (1,'local',?1,?2,?2,?2,1)",
                rusqlite::params![bucket, v],
            )
            .unwrap();
    }
    for i in 0..5 {
        w.conn
            .execute(
                "INSERT INTO events (eid,at,node,kind,severity,title,detail) VALUES (?1,?2,'local','x','info','t','')",
                rusqlite::params![format!("e{i}"), now - 10 + i],
            )
            .unwrap();
    }
    w.conn
        .execute(
            "INSERT INTO events (eid,at,node,kind,severity,title,detail) VALUES ('old',?1,'local','x','info','t','')",
            [now - 181 * DAY],
        )
        .unwrap();
    maintain::tick(&w.conn, now, &r).unwrap();
    let left: i64 = w
        .conn
        .query_row("SELECT min(bucket) FROM samples_1m", [], |r| r.get(0))
        .unwrap();
    assert_eq!(left, now - 90 * DAY, "exactly 90 d kept, older dropped");
    assert_eq!(count(&w, "events"), 3, "age first, then the row cap");
    let oldest: String = w
        .conn
        .query_row("SELECT eid FROM events ORDER BY id LIMIT 1", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(oldest, "e2");
}

// ------------------------------------------------------------- queries

fn params(pairs: &[(&str, &str)]) -> HashMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

#[test]
fn params_are_validated_and_clamped() {
    let p = query::parse(
        &params(&[
            ("severity", "warn,error"),
            ("max_points", "99999"),
            ("limit", "9999"),
        ]),
        T0,
        90,
        20,
    )
    .unwrap();
    assert_eq!(p.severities, vec!["warn", "error"]);
    assert_eq!(p.max_points, query::MAX_POINTS);
    assert_eq!(p.limit, 500);
    assert_eq!(p.to - p.from, DAY);
    let long = query::parse(
        &params(&[("from", "0"), ("to", &T0.to_string())]),
        T0,
        90,
        20,
    )
    .unwrap();
    assert_eq!(long.to - long.from, 90 * DAY);
    for bad in [
        params(&[("from", "10"), ("to", "5")]),
        params(&[("cursor", "nope")]),
        params(&[("metrics", "bitrate,revenue")]),
        params(&[("tz_offset_min", "9999")]),
        params(&[("bucket", "week")]),
    ] {
        assert!(query::parse(&bad, T0, 90, 20).is_err(), "{bad:?}");
    }
}

#[test]
fn stats_tier_points_and_mean() {
    assert_eq!(query::choose_tier(T0 - 3 * DAY, T0, T0, 7), "raw");
    assert_eq!(query::choose_tier(T0 - 3 * DAY - 1, T0, T0, 7), "1m");
    assert_eq!(query::choose_tier(T0 - 8 * DAY, T0 - 7 * DAY, T0, 7), "1m");
    for points in [2usize, 7, 240, 1000, 2000] {
        let (_, n) = query::stats_layout(T0 - 90 * DAY, T0, "1m", points);
        assert!(n <= points);
    }
    let (w, _) = writer("stats");
    raw(&w, T0, 10.0);
    raw(&w, T0 + 10, 30.0);
    let mut qq = q(T0, T0 + 20);
    qq.metrics = vec!["bitrate".into()];
    qq.max_points = 2;
    let parts = query::stats(&w.conn, &qq, "raw", None).unwrap();
    let out = query::render_stats(&parts);
    assert_eq!(out["series"]["bitrate"]["mean"][0], json!(20.0));
    assert_eq!(out["series"]["bitrate"]["min"][0], json!(10.0));
    assert_eq!(out["series"]["bitrate"]["max"][0], json!(30.0));
}

#[test]
fn sessions_gaps_cursor_and_neighbours() {
    let (mut w, _) = writer("sessions");
    session(&mut w, 1, T0, None);
    w.apply(&ev(
        T0 + 100,
        EventKind::FfmpegExited {
            seq: 1,
            run: 10,
            code: Some(1),
            success: false,
            counted_failure: true,
            diagnostic: Some("http error 403".into()),
        },
    ))
    .unwrap();
    w.apply(&ev(T0 + 145, EventKind::FfmpegStarted { seq: 1, run: 11 }))
        .unwrap();
    end(&mut w, 1, T0 + 600, EndReason::SourceEnded);
    session(&mut w, 2, T0 + 1000, None);
    end(&mut w, 2, T0 + 1100, EndReason::UserStop);
    session(&mut w, 3, T0 + 2000, None);
    let now = T0 + 2100;
    let first = &sessions(&w, now).items;
    assert_eq!(first.len(), 3);
    let s1 = first.iter().find(|s| s.started_at == T0).unwrap();
    assert_eq!(s1.gaps, vec![(T0 + 100, T0 + 145, "failed".to_string())]);
    assert_eq!((s1.runs, s1.restarts, s1.failures), (2, 1, 1));
    let mut paged = q(T0 - DAY, now);
    paged.limit = 2;
    let p1 = query::sessions(&w.conn, &paged, now).unwrap();
    assert_eq!(p1.items.len(), 2);
    let cursor = p1.next_cursor.clone().unwrap();
    paged.cursor = Some({
        let (a, b) = cursor.split_once('~').unwrap();
        (a.parse().unwrap(), b.to_string())
    });
    let p2 = query::sessions(&w.conn, &paged, now).unwrap();
    assert_eq!(p2.items.len(), 1);
    assert_eq!(p2.next_cursor, None);
    let mut by_id = q(T0 - DAY, now);
    by_id.id = Some(first[1].id.clone());
    let one = query::sessions(&w.conn, &by_id, now).unwrap();
    assert_eq!(one.items.len(), 1, "one id reads one session");
    // A channel segment lists every session it was relayed by in one call.
    by_id.id = Some(format!("{},{}", first[1].id, first[0].id));
    let both = query::sessions(&w.conn, &by_id, now).unwrap();
    let mut ids: Vec<&str> = both.items.iter().map(|s| s.eid.as_str()).collect();
    ids.sort_unstable();
    let mut want = vec![first[0].id.as_str(), first[1].id.as_str()];
    want.sort_unstable();
    assert_eq!(ids, want);
}

#[test]
fn events_filter_by_comma_severity_and_kind_prefix() {
    let (mut w, _) = writer("events");
    session(&mut w, 1, T0, None);
    w.apply(&ev(
        T0 + 100,
        EventKind::FfmpegExited {
            seq: 1,
            run: 10,
            code: Some(1),
            success: false,
            counted_failure: true,
            diagnostic: None,
        },
    ))
    .unwrap();
    w.apply(&ev(T0 + 110, EventKind::FfmpegStarted { seq: 1, run: 11 }))
        .unwrap();
    end(&mut w, 1, T0 + 600, EndReason::Aborted);
    w.apply(&ev(
        T0 + 50,
        EventKind::NotifySent {
            dedup: "test-key".into(),
            mode: Some("active"),
            trace_id: None,
            test: false,
            key: "node.down".into(),
            phase: "fire",
            ok: false,
            code: Some(40034105),
            suppressed: 0,
            undelivered: 1,
        },
    ))
    .unwrap();
    let mut qq = q(T0 - 10, T0 + 1000);
    qq.limit = 20;
    qq.severities = vec!["warn".into(), "error".into()];
    let page = query::events(&w.conn, &qq).unwrap();
    let kinds: Vec<_> = page
        .items
        .iter()
        .map(|e| (e.kind.as_str(), e.severity.as_str()))
        .collect();
    assert_eq!(
        kinds,
        vec![
            ("stream.ended", "warn"),
            ("stream.failed", "error"),
            ("notify.sent", "warn")
        ]
    );
    assert!(page.items[0].session_id.is_some());
    assert!(page.items[2].session_id.is_none(), "session_id is optional");
    assert!(page.items[2].title.contains("40034105"));
    qq.severities.clear();
    qq.kinds = vec!["stream".into()];
    assert!(query::events(&w.conn, &qq)
        .unwrap()
        .items
        .iter()
        .all(|e| e.kind.starts_with("stream.")));
}

// ------------------------------------------------------- cluster scope

struct MockPeers(HashMap<String, Result<Value, &'static str>>);

impl Peers for MockPeers {
    fn fetch<'a>(
        &'a self,
        node: &'a str,
        _req: &'a PeerRequest,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Value, super::MissingNode>> + Send + 'a>,
    > {
        Box::pin(async move {
            match self.0.get(node) {
                Some(Ok(v)) => Ok(v.clone()),
                Some(Err(reason)) => Err(api::missing(node, reason, "mock")),
                None => Err(api::missing(node, "unreachable", "mock")),
            }
        })
    }
}

/// The local DB behind the same seam, answering synchronously.
struct LocalConn(std::sync::Mutex<rusqlite::Connection>, i64);

impl Peers for LocalConn {
    fn fetch<'a>(
        &'a self,
        node: &'a str,
        req: &'a PeerRequest,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Value, super::MissingNode>> + Send + 'a>,
    > {
        let out = api::local_parts(&self.0.lock().unwrap(), req, node, self.1)
            .map_err(|e| api::missing(node, "error", &e.to_string()));
        Box::pin(async move { out })
    }
}

fn cluster_cfg() -> crate::config::Config {
    let mut cfg = crate::test_support::config("local", 0);
    cfg.cluster.enabled = true;
    cfg.cluster.node_id = "local".into();
    cfg.cluster.peers = ["tokyo-1", "sg-3"]
        .iter()
        .map(|id| crate::config::ClusterPeer {
            node_id: id.to_string(),
            name: String::new(),
            api_url: format!("https://{id}.example"),
            priority: 0,
        })
        .collect();
    cfg
}

#[tokio::test]
async fn cluster_scope_merges_peers_and_reports_missing_nodes() {
    let (mut w, path) = writer("cluster");
    session(&mut w, 1, T0, None);
    end(&mut w, 1, T0 + 600, EndReason::ClusterHandoff);
    let now = T0 + 1000;
    // tokyo-1 answers with its own session (same shape a peer route returns).
    let peer_page = query::SessionsPage {
        items: vec![query::SessionItem {
            area_v2: None,
            id: "tokyo-1:1:1".into(),
            eid: "tokyo-1:1:1".into(),
            node: "tokyo-1".into(),
            platform: "TW".into(),
            channel_id: "c".into(),
            channel_name: "n".into(),
            title: None,
            started_at: T0 + 700,
            ended_at: None,
            duration_secs: 300,
            end_reason: None,
            runs: 1,
            restarts: 0,
            failures: 0,
            stuck: 0,
            gaps: vec![],
            median_bitrate_kbps: None,
            source_id: None,
            end_warning_stop: false,
            broadcast_id: String::new(),
            bili: None,
            end_cut_off: false,
            end_detail: None,
        }],
        ..Default::default()
    };
    let peers = MockPeers(HashMap::from([
        (
            "tokyo-1".to_string(),
            Ok(serde_json::to_value(&peer_page).unwrap()),
        ),
        ("sg-3".to_string(), Err("timeout")),
    ]));
    let local = LocalConn(std::sync::Mutex::new(schema::open_ro(&path).unwrap()), now);
    drop(w);
    let cfg = cluster_cfg();
    let window = params(&[("from", &(T0 - 10).to_string()), ("to", &now.to_string())]);
    let data = api::answer(&cfg, "sessions", &window, &peers, &local, now)
        .await
        .unwrap();
    assert_eq!(data["items"].as_array().unwrap().len(), 2);
    assert_eq!(
        data["items"][0]["node"], "tokyo-1",
        "newest first across nodes"
    );
    assert_eq!(
        data["missing_nodes"],
        json!([{"node_id": "sg-3", "reason": "timeout", "detail": "mock"}])
    );
    assert!(data.get("neighbours").is_none() && data.get("more").is_none());
    // scope=local: [] even with a dead peer.
    let mut local_only = window.clone();
    local_only.insert("scope".into(), "local".into());
    for name in ["summary", "sessions", "events", "stats"] {
        let data = api::answer(&cfg, name, &local_only, &peers, &local, now)
            .await
            .unwrap();
        assert_eq!(data["missing_nodes"], json!([]), "{name}");
        let data = api::answer(&cfg, name, &window, &peers, &local, now)
            .await
            .unwrap();
        assert!(
            data["missing_nodes"]
                .as_array()
                .unwrap()
                .iter()
                .any(|m| m["node_id"] == "sg-3"),
            "{name}"
        );
    }
    // Unknown node → 400.
    let mut unknown = window.clone();
    unknown.insert("scope".into(), "nope".into());
    assert_eq!(
        api::answer(&cfg, "summary", &unknown, &peers, &local, now)
            .await
            .unwrap_err()
            .0,
        400
    );
    assert_eq!(api::resolve_scope(&cfg, None).unwrap(), Scope::Cluster);
    // The same peer answering twice is idempotent (union by eid).
    let merged = query::merge_sessions(vec![peer_page.clone(), peer_page], 50);
    assert_eq!(merged.items.len(), 1);
}

#[test]
fn peer_errors_map_to_normative_reasons() {
    use crate::cluster::peer_call::PeerCallError::{Refused, Unreachable};
    let r = |e| api::classify("n", &e).reason;
    assert_eq!(r(Unreachable("节点请求超时".into())), "timeout");
    assert_eq!(r(Unreachable("connection refused".into())), "unreachable");
    assert_eq!(r(Refused("HTTP 401".into())), "auth");
    assert_eq!(r(Refused("HTTP 404".into())), "version");
    assert_eq!(r(Refused("HTTP 500".into())), "error");
}

#[test]
fn summary_kpis_and_revenue_free_output() {
    let (mut w, _) = writer("summary");
    session(&mut w, 1, T0, Some("K1"));
    w.apply(&ev(
        T0 + 100,
        EventKind::FfmpegExited {
            seq: 1,
            run: 10,
            code: Some(1),
            success: false,
            counted_failure: true,
            diagnostic: Some("http error 403".into()),
        },
    ))
    .unwrap();
    w.apply(&ev(T0 + 160, EventKind::FfmpegStarted { seq: 1, run: 11 }))
        .unwrap();
    end(&mut w, 1, T0 + 3600, EndReason::SourceEnded);
    let totals = parse_stop_live_data(&serde_json::from_str(LIVE_SESSION).unwrap()).ok();
    w.apply(&ev(
        T0 + 3700,
        EventKind::BiliLiveSummary {
            live_key: "K1".into(),
            totals,
        },
    ))
    .unwrap();
    let mut qq = q(T0, T0 + 4000);
    qq.bucket = Some("hour".into());
    let parts = query::summary(&w.conn, &qq, "local", T0 + 4000).unwrap();
    let out = query::render_summary(&query::merge_summary(vec![parts.clone(), parts]), &[]);
    // Two copies of the same live session (by key hash) count once.
    assert_eq!(out["kpis"]["watched"], 469);
    assert_eq!(out["kpis"]["danmu"], 457);
    assert_eq!(out["kpis"]["recover_secs_avg"], 60);
    assert_eq!(out["reliability"]["reasons"][0]["class"], "http error 403");
    assert_eq!(out["missing_nodes"], json!([]));
    let text = out.to_string();
    assert!(!text.contains("Hamster") && !text.contains("revenue") && !text.contains("live_time"));
    assert!(!text.contains("K1"));
}

// -------------------------------------------------------------- capture

#[test]
fn capture_writes_only_to_the_git_ignored_fixtures_dir() {
    let repo = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let dir = super::capture::fixtures_dir(repo);
    assert!(dir.ends_with(".trellis/fixtures"));
    let probe = dir.join("stoplivedata-probe.json");
    let status = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["check-ignore", "-q", "--no-index"])
        .arg(&probe)
        .status();
    if let Ok(status) = status {
        assert!(status.success(), "{probe:?} must be git-ignored");
    }
    let root = tmp("capture");
    let reply: Value = serde_json::from_str(LIVE_SESSION).unwrap();
    let file = super::capture::write_to(
        &root,
        "stoplivedata",
        &super::enrich::redact_stop_live_data(&reply),
    )
    .unwrap();
    assert!(file.starts_with(super::capture::fixtures_dir(&root)));
    let text = std::fs::read_to_string(file).unwrap();
    assert!(text.contains("DanmuNum") && !text.contains("Hamster"));
    assert!(!super::capture::is_enabled_for_test(), "off by default");
}

// ------------------------------------------------------- 401 per route

#[tokio::test]
async fn every_history_route_is_401_without_a_session() {
    crate::install_crypto_provider();
    let dir = tmp("auth");
    let store = crate::storage::Store::open(dir.join("data"), dir.join("key")).unwrap();
    store.write("config.json", json!({})).unwrap();
    let auth = crate::webui::listen::AuthState::open(store.clone(), Some("synthetic-pass".into()))
        .unwrap();
    let app = crate::webui::server::build_app(crate::AppState::new(), auth);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let _ = axum::serve(
            listener,
            app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await;
    });
    let http = reqwest::Client::builder().no_proxy().build().unwrap();
    for route in ["status", "summary", "sessions", "events", "stats"] {
        let url = format!("http://{addr}/api/history/{route}?from=1&to=2");
        let response = http.get(&url).send().await.unwrap();
        assert_eq!(response.status(), 401, "/api/history/{route}");
        assert!(!crate::webui::listen::is_public_api_path(&format!(
            "/api/history/{route}"
        )));
    }
    // The peer route refuses browsers outright.
    assert!(crate::cluster::peer_call::browser_forbidden(
        "/api/cluster/v1/history"
    ));
    server.abort();
    drop(store);
    let _ = std::fs::remove_dir_all(dir);
}

// ------------------------------------------- Revision 1 (design §11)

#[test]
fn clamped_to_days_is_set_only_when_parse_cut_the_range() {
    let long = query::parse(
        &params(&[("from", "0"), ("to", &T0.to_string())]),
        T0,
        90,
        20,
    )
    .unwrap();
    assert_eq!(long.clamped_to_days, Some(90));
    let exact = (T0 - 90 * DAY).to_string();
    let edge = query::parse(
        &params(&[("from", &exact), ("to", &T0.to_string())]),
        T0,
        90,
        20,
    )
    .unwrap();
    assert_eq!(
        edge.clamped_to_days, None,
        "exactly rollup_days is not a clamp"
    );
    let short = query::parse(&params(&[]), T0, 90, 20).unwrap();
    assert_eq!(short.clamped_to_days, None);
    // Never travels to a peer.
    let wire = serde_json::to_value(&long).unwrap();
    assert!(wire.get("clamped_to_days").is_none());
}

#[tokio::test]
async fn clamped_to_days_is_in_the_response_only_when_clamped() {
    let (mut w, path) = writer("clamp");
    session(&mut w, 1, T0, None);
    end(&mut w, 1, T0 + 600, EndReason::SourceEnded);
    drop(w);
    let now = T0 + 1000;
    let local = LocalConn(std::sync::Mutex::new(schema::open_ro(&path).unwrap()), now);
    let peers = MockPeers(HashMap::new());
    let cfg = crate::test_support::config("local", 0);
    let rollup = cfg.history.retention.rollup_days;
    let long = params(&[("from", "0"), ("to", &now.to_string()), ("scope", "local")]);
    let short = params(&[
        ("from", &(T0 - 10).to_string()),
        ("to", &now.to_string()),
        ("scope", "local"),
    ]);
    for name in ["summary", "sessions", "events", "stats"] {
        let data = api::answer(&cfg, name, &long, &peers, &local, now)
            .await
            .unwrap();
        assert_eq!(data["clamped_to_days"], json!(rollup), "{name} clamped");
        let data = api::answer(&cfg, name, &short, &peers, &local, now)
            .await
            .unwrap();
        assert!(data.get("clamped_to_days").is_none(), "{name} not clamped");
    }
}

#[test]
fn missing_live_key_logs_no_value_and_session_totals_stay_null() {
    let reply: Value = serde_json::from_str(ROOM_GET_INFO).unwrap();
    let mut placeholder = reply.clone();
    placeholder["data"]["up_session"] = json!("<redacted:str>");
    let (dispatch, logs) = crate::logging::test_capture::subscriber(tracing::Level::DEBUG);
    tracing::dispatcher::with_default(&dispatch, || {
        assert_eq!(super::room_key_or_note(&reply), None);
        assert_eq!(super::room_key_or_note(&placeholder), None);
        super::note_live_key_unavailable("getinfo_failed");
    });
    let text = logs.text();
    assert_eq!(text.matches("未取到 live_key").count(), 3, "{text}");
    // The operator log stops at INFO: a missing key at the end of a live is
    // normal, so the note stays a DEBUG diagnostic instead of a log row.
    let (dispatch, quiet) = crate::logging::test_capture::subscriber(tracing::Level::INFO);
    tracing::dispatcher::with_default(&dispatch, || {
        super::note_live_key_unavailable("getinfo_no_key");
    });
    assert!(
        !quiet.text().contains("未取到 live_key"),
        "{}",
        quiet.text()
    );
    assert!(!text.contains("redacted") && !text.contains("up_session") && !text.contains("{"));
    // The session keeps no key and has NULL StopLiveData totals.
    let (mut w, _) = writer("no-key-null");
    session(&mut w, 1, T0, None);
    end(&mut w, 1, T0 + 600, EndReason::SourceEnded);
    let key: Option<String> = w
        .conn
        .query_row("SELECT live_key FROM sessions", [], |r| r.get(0))
        .unwrap();
    assert_eq!(key, None);
    let lives: i64 = w
        .conn
        .query_row("SELECT count(*) FROM bili_lives", [], |r| r.get(0))
        .unwrap();
    assert_eq!(lives, 0);
    let bili = sessions(&w, T0 + 700).items[0].bili.clone().unwrap();
    assert_eq!(bili.watched, None);
    let item = serde_json::to_value(&bili).unwrap();
    for (k, v) in item.as_object().unwrap() {
        if k != "state" {
            assert!(v.is_null(), "{k} must be null without a live_key");
        }
    }
}

#[test]
fn string_ids_round_trip_percent_encoded_in_query_and_path() {
    use percent_encoding::{percent_decode_str, utf8_percent_encode, NON_ALPHANUMERIC};
    let (mut w, _) = writer("ids");
    session(&mut w, 1, T0, None);
    let id = sessions(&w, T0 + 60).items[0].id.clone();
    assert_eq!(id.split(':').count(), 3, "node:boot:n");
    let enc = utf8_percent_encode(&id, NON_ALPHANUMERIC).to_string();
    assert!(enc.contains("%3A") && !enc.contains(':'));
    // Query value.
    let parsed: HashMap<String, String> =
        serde_urlencoded::from_str(&format!("id={enc}&session={enc}")).unwrap();
    assert_eq!(parsed["id"], id);
    let qq = query::parse(&parsed, T0 + 60, 90, 50).unwrap();
    let page = query::sessions(
        &w.conn,
        &Query {
            from: T0 - DAY,
            ..qq.clone()
        },
        T0 + 60,
    )
    .unwrap();
    assert_eq!(page.items.len(), 1);
    assert_eq!(page.items[0].id, id);
    // Path segment.
    assert_eq!(percent_decode_str(&enc).decode_utf8().unwrap(), id);
    // And through JSON (the peer body).
    let req = PeerRequest {
        name: "sessions".into(),
        query: qq,
    };
    let back: PeerRequest = serde_json::from_str(&serde_json::to_string(&req).unwrap()).unwrap();
    assert_eq!(back.query.id.as_deref(), Some(id.as_str()));
}

fn all_ids(path: &std::path::Path) -> Vec<String> {
    let conn = schema::open_ro(path).unwrap();
    let mut out = Vec::new();
    for sql in ["SELECT eid FROM sessions", "SELECT eid FROM events"] {
        let mut st = conn.prepare(sql).unwrap();
        out.extend(
            st.query_map([], |r| r.get::<_, String>(0))
                .unwrap()
                .map(Result::unwrap),
        );
    }
    out
}

#[test]
fn boot_part_never_repeats_after_restart_or_restore_from_backup() {
    // Per process start, random; never read back from the database.
    let boots: std::collections::HashSet<i64> = (0..256).map(|_| super::boot_id()).collect();
    assert_eq!(boots.len(), 256);
    assert!(boots.iter().all(|b| *b >= 0));

    let dir = tmp("restore");
    let path = dir.join("history.sqlite");
    let backup = dir.join("backup.sqlite");
    let start = |boot: i64, at: i64| {
        let mut w = Writer::new(schema::open_rw(&path).unwrap(), "local", boot);
        w.apply(&ev(at, EventKind::ProcessStarted { version: "test" }))
            .unwrap();
        w
    };
    let mut issued: Vec<String> = Vec::new();
    // Boot 1 writes, then the file is backed up.
    let mut w = start(super::boot_id(), T0);
    session(&mut w, 1, T0 + 1, None);
    end(&mut w, 1, T0 + 2, EndReason::SourceEnded);
    drop(w);
    issued.extend(all_ids(&path));
    std::fs::copy(&path, &backup).unwrap();
    // Restart: boot 2 writes more ids after the backup.
    let mut w = start(super::boot_id(), T0 + 10);
    session(&mut w, 1, T0 + 11, None);
    drop(w);
    issued.extend(all_ids(&path));
    // Restore the backup (boot 2's rows are gone), then boot 3 starts.
    for ext in ["sqlite-wal", "sqlite-shm"] {
        let _ = std::fs::remove_file(path.with_extension(ext));
    }
    std::fs::copy(&backup, &path).unwrap();
    let mut w = start(super::boot_id(), T0 + 20);
    session(&mut w, 1, T0 + 21, None);
    end(&mut w, 1, T0 + 22, EndReason::SourceEnded);
    drop(w);
    let issued: std::collections::HashSet<String> = issued.into_iter().collect();
    let after = all_ids(&path);
    let fresh: Vec<&String> = after.iter().filter(|id| !issued.contains(*id)).collect();
    assert!(!fresh.is_empty());
    let boot = |id: &str| id.split(':').nth(1).unwrap().to_string();
    let old_boots: std::collections::HashSet<String> = issued.iter().map(|i| boot(i)).collect();
    for id in fresh {
        assert!(
            !old_boots.contains(&boot(id)),
            "{id} reuses an earlier boot"
        );
    }
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn peer_body_must_be_a_valid_history_query() {
    let cfg = crate::test_support::config("local", 0);
    let now = T0;
    let good = || PeerRequest {
        name: "sessions".into(),
        query: q(T0 - DAY, T0),
    };
    assert_eq!(api::check_peer_request(&mut good(), &cfg, now), Ok(None));
    for name in ["summary", "events", "stats"] {
        let mut r = good();
        r.name = name.into();
        assert!(api::check_peer_request(&mut r, &cfg, now).is_ok(), "{name}");
    }
    // Revision 2: a range longer than this node's rollup_days is clamped,
    // not refused.
    let mut long = good();
    long.query.from = long.query.to - 400 * DAY;
    assert_eq!(api::check_peer_request(&mut long, &cfg, now), Ok(Some(90)));
    assert_eq!(long.query.to - long.query.from, 90 * DAY);
    let mut cases: Vec<PeerRequest> = Vec::new();
    for name in ["status", "delete", "rollup", "", "sessions;DROP"] {
        let mut r = good();
        r.name = name.into();
        cases.push(r);
    }
    type Edit = Box<dyn Fn(&mut Query)>;
    let edits: Vec<Edit> = vec![
        Box::new(|q| q.from = i64::MIN),
        Box::new(|q| q.from = q.to),
        Box::new(|q| q.to = T0 + DAY),
        Box::new(|q| q.limit = 0),
        Box::new(|q| q.limit = 100_000),
        Box::new(|q| q.max_points = 1_000_000),
        Box::new(|q| q.tier = Some("samples_raw".into())),
        Box::new(|q| q.bucket = Some("week".into())),
        Box::new(|q| q.platform = Some("XX".into())),
        Box::new(|q| q.metrics = vec!["revenue".into()]),
        Box::new(|q| q.tz_offset_min = 9999),
        Box::new(|q| q.id = Some("x".repeat(801))),
        Box::new(|q| q.session = Some("x".repeat(801))),
        Box::new(|q| q.cursor = Some((0, String::new()))),
    ];
    for edit in &edits {
        let mut r = good();
        edit(&mut r.query);
        cases.push(r);
    }
    for r in &mut cases {
        assert!(api::check_peer_request(r, &cfg, now).is_err(), "{r:?}");
    }
    // Unknown fields and non-JSON bodies never deserialize.
    for body in [
        r#"{"name":"sessions","query":{"from":1,"to":2},"sql":"DELETE"}"#,
        r#"{"name":"sessions","query":{"from":1,"to":2,"write":true}}"#,
        r#"{"name":"sessions"}"#,
        "not json",
    ] {
        assert!(serde_json::from_str::<PeerRequest>(body).is_err(), "{body}");
    }
}

fn db_digest(path: &std::path::Path) -> Vec<(String, i64)> {
    let conn = schema::open_ro(path).unwrap();
    let mut out = Vec::new();
    for t in [
        "sessions",
        "runs",
        "events",
        "samples_raw",
        "samples_1m",
        "bili_lives",
        "boots",
        "meta",
    ] {
        let n: i64 = conn
            .query_row(&format!("SELECT count(*) FROM {t}"), [], |r| r.get(0))
            .unwrap();
        out.push((t.to_string(), n));
    }
    let v: i64 = conn
        .query_row("PRAGMA schema_version", [], |r| r.get(0))
        .unwrap();
    out.push(("schema".into(), v));
    out
}

#[test]
fn peer_parts_are_read_only_and_expose_nothing_beyond_the_admin_view() {
    let (mut w, path) = writer("peer-ro");
    session(&mut w, 1, T0, Some("K1"));
    raw(&w, T0 + 30, 1.0);
    end(&mut w, 1, T0 + 600, EndReason::SourceEnded);
    let now = T0 + 1000;
    drop(w);
    let before = db_digest(&path);
    let bytes_before = std::fs::read(&path).unwrap();
    let conn = schema::open_ro(&path).unwrap();
    let mut peer: HashMap<&str, Value> = HashMap::new();
    for name in ["summary", "sessions", "events", "stats"] {
        let req = PeerRequest {
            name: name.into(),
            query: q(T0 - 10, now),
        };
        let v = api::local_parts(&conn, &req, "local", now).unwrap();
        let text = v.to_string();
        for secret in [
            "K1",
            "live_key",
            "openid",
            "SYNTHKEY",
            "cookie",
            "SESSDATA",
            "appsecret",
        ] {
            assert!(!text.contains(secret), "{name} leaks {secret}");
        }
        peer.insert(name, v);
    }
    // A write through the read-only connection is refused.
    assert!(conn.execute("DELETE FROM sessions", []).is_err());
    drop(conn);
    assert_eq!(db_digest(&path), before);
    assert_eq!(std::fs::read(&path).unwrap(), bytes_before);
    // Item rows a member gets are exactly what a logged-in admin sees.
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    let local = LocalConn(std::sync::Mutex::new(schema::open_ro(&path).unwrap()), now);
    let cfg = crate::test_support::config("local", 0);
    let window = params(&[
        ("from", &(T0 - 10).to_string()),
        ("to", &now.to_string()),
        ("scope", "local"),
    ]);
    let keys = |v: &Value| -> std::collections::BTreeSet<String> {
        v.as_object()
            .map(|m| m.keys().cloned().collect())
            .unwrap_or_default()
    };
    for name in ["sessions", "events"] {
        let admin = rt
            .block_on(api::answer(
                &cfg,
                name,
                &window,
                &MockPeers(HashMap::new()),
                &local,
                now,
            ))
            .unwrap();
        let a_items = admin["items"].as_array().unwrap();
        let p_items = peer[name]["items"].as_array().unwrap();
        assert_eq!(a_items.len(), p_items.len(), "{name}");
        for (a, p) in a_items.iter().zip(p_items) {
            assert_eq!(keys(p), keys(a), "{name}: same fields, no extra");
            assert_eq!(p, a, "{name}: same values");
        }
    }
}

#[test]
fn retention_deletes_old_rows_and_repeated_ticks_keep_the_db_bounded() {
    let (w, path) = writer("bounded");
    let r = HistoryRetention {
        raw_days: 1,
        rollup_days: 3,
        event_days: 4,
        event_rows: 500,
        ..Default::default()
    };
    let mut peaks: Vec<(i64, i64, i64)> = Vec::new();
    for day in 0..12i64 {
        let base = T0 + day * DAY;
        for i in 0..288 {
            raw(&w, base + i * 300, i as f64);
        }
        for i in 0..40 {
            w.conn
                .execute(
                    "INSERT INTO events (eid,at,node,kind,severity,title,detail) VALUES (?1,?2,'local','x','info','t','')",
                    rusqlite::params![format!("d{day}-{i}"), base + i * 60],
                )
                .unwrap();
        }
        // Several ticks per simulated day, as the writer would run them.
        for k in 1..=4 {
            maintain::tick(&w.conn, base + k * (DAY / 4), &r).unwrap();
        }
        let now = base + DAY;
        let min_raw: Option<i64> = w
            .conn
            .query_row("SELECT min(ts) FROM samples_raw", [], |r| r.get(0))
            .unwrap();
        let min_1m: Option<i64> = w
            .conn
            .query_row("SELECT min(bucket) FROM samples_1m", [], |r| r.get(0))
            .unwrap();
        let min_ev: Option<i64> = w
            .conn
            .query_row("SELECT min(at) FROM events", [], |r| r.get(0))
            .unwrap();
        assert!(
            min_raw.unwrap() >= now - r.raw_days as i64 * DAY,
            "day {day}: raw past raw_days"
        );
        assert!(
            min_1m.unwrap() >= now - r.rollup_days as i64 * DAY,
            "day {day}: 1m past rollup_days"
        );
        assert!(
            min_ev.unwrap() >= now - r.event_days as i64 * DAY,
            "day {day}: events past event_days"
        );
        let pages: i64 = w
            .conn
            .query_row("PRAGMA page_count", [], |r| r.get(0))
            .unwrap();
        peaks.push((
            count(&w, "samples_raw") + count(&w, "samples_1m"),
            count(&w, "events"),
            pages,
        ));
    }
    // After the longest horizon (event_days) the counts and file stop growing.
    let steady = &peaks[6..];
    let (rows0, ev0, pages0) = steady[0];
    for (i, (rows, ev, pages)) in steady.iter().enumerate() {
        assert!(
            *rows <= rows0 + 288,
            "step {i}: sample rows keep growing {peaks:?}"
        );
        assert!(
            *ev <= ev0.max(r.event_rows as i64),
            "step {i}: events keep growing"
        );
        assert!(
            *pages <= pages0 + pages0 / 10 + 8,
            "step {i}: file keeps growing {peaks:?}"
        );
    }
    drop(w);
    assert!(std::fs::metadata(&path).unwrap().len() > 0);
}

// ------------------------------------- Revision 2 (design §11 e3797697)

/// A peer node answering exactly as `peer_history` does, with its own
/// (smaller) rollup_days; or offline.
struct MixedPeers {
    small: std::sync::Mutex<rusqlite::Connection>,
    small_cfg: crate::config::Config,
    now: i64,
    offline: Vec<&'static str>,
}

impl Peers for MixedPeers {
    fn fetch<'a>(
        &'a self,
        node: &'a str,
        req: &'a PeerRequest,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Value, super::MissingNode>> + Send + 'a>,
    > {
        let out = if self.offline.contains(&node) {
            Err(api::missing(node, "unreachable", "mock"))
        } else {
            let mut req = req.clone();
            let clamped = api::check_peer_request(&mut req, &self.small_cfg, self.now).unwrap();
            api::local_parts(&self.small.lock().unwrap(), &req, node, self.now)
                .map(|v| api::with_peer_clamp(v, clamped))
                .map_err(|e| api::missing(node, "error", &e.to_string()))
        };
        Box::pin(async move { out })
    }
}

fn mixed(offline: Vec<&'static str>, now: i64) -> (MixedPeers, LocalConn) {
    let (mut w, path) = writer("mixed-local");
    session(&mut w, 1, T0, None);
    end(&mut w, 1, T0 + 600, EndReason::SourceEnded);
    drop(w);
    let (mut p, ppath) = writer("mixed-peer");
    session(&mut p, 1, T0 + 700, None);
    drop(p);
    let mut small_cfg = cluster_cfg();
    small_cfg.cluster.node_id = "tokyo-1".into();
    small_cfg.history.retention.rollup_days = 30;
    (
        MixedPeers {
            small: std::sync::Mutex::new(schema::open_ro(&ppath).unwrap()),
            small_cfg,
            now,
            offline,
        },
        LocalConn(std::sync::Mutex::new(schema::open_ro(&path).unwrap()), now),
    )
}

fn missing_ids(data: &Value) -> Vec<String> {
    data["missing_nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["node_id"].as_str().unwrap().to_string())
        .collect()
}

#[tokio::test]
async fn smaller_retention_peer_is_partial_not_missing_on_every_route() {
    let now = T0 + 1000;
    let (peers, local) = mixed(vec!["sg-3"], now);
    let cfg = cluster_cfg();
    assert_eq!(cfg.history.retention.rollup_days, 90);
    let to = now.to_string();
    // 400 d: both clamp. 60 d: only the peer clamps. 10 d: nobody does.
    let cases = [
        ((now - 400 * DAY).to_string(), Some(90), true),
        ((now - 60 * DAY).to_string(), None, true),
        ((now - 10 * DAY).to_string(), None, false),
    ];
    for (from, local_clamp, peer_clamps) in &cases {
        let window = params(&[("from", from), ("to", &to)]);
        for name in ["summary", "sessions", "events", "stats"] {
            let data = api::answer(&cfg, name, &window, &peers, &local, now)
                .await
                .unwrap();
            assert_eq!(
                data["clamped_to_days"],
                json!(local_clamp),
                "{name} {from}: local only"
            );
            if *peer_clamps {
                assert_eq!(
                    data["partial_nodes"],
                    json!([{"node": "tokyo-1", "clamped_to_days": 30}]),
                    "{name} {from}"
                );
            } else {
                assert!(
                    data.get("partial_nodes").is_none(),
                    "{name} {from}: no clamp, no key"
                );
            }
            assert_eq!(
                missing_ids(&data),
                vec!["sg-3"],
                "{name} {from}: partial peer not missing"
            );
        }
    }
}

#[tokio::test]
async fn offline_smaller_peer_is_only_in_missing_nodes() {
    let now = T0 + 1000;
    // tokyo-1 has the smaller rollup_days and is offline as well.
    let (peers, local) = mixed(vec!["tokyo-1", "sg-3"], now);
    let cfg = cluster_cfg();
    let window = params(&[("from", "0"), ("to", &now.to_string())]);
    for name in ["summary", "sessions", "events", "stats"] {
        let data = api::answer(&cfg, name, &window, &peers, &local, now)
            .await
            .unwrap();
        assert!(data.get("partial_nodes").is_none(), "{name}");
        assert_eq!(data["clamped_to_days"], json!(90), "{name}");
        assert_eq!(missing_ids(&data), vec!["tokyo-1", "sg-3"], "{name}");
    }
}

// ----------------------------------------- continuation correctness fixes

fn exited(w: &mut Writer, seq: u64, run: u64, at: i64, success: bool, counted_failure: bool) {
    w.apply(&ev(
        at,
        EventKind::FfmpegExited {
            seq,
            run,
            code: Some(if success { 0 } else { 1 }),
            success,
            counted_failure,
            diagnostic: counted_failure.then(|| "connection reset".into()),
        },
    ))
    .unwrap();
}

#[test]
fn kpis_count_run_time_completed_runs_and_all_notification_attempts() {
    let (mut w, _) = writer("metric-runs");
    session(&mut w, 1, T0, None);
    exited(&mut w, 1, 10, T0 + 100, false, true);
    w.apply(&ev(T0 + 160, EventKind::FfmpegStarted { seq: 1, run: 11 }))
        .unwrap();
    exited(&mut w, 1, 11, T0 + 600, true, false);
    end(&mut w, 1, T0 + 600, EndReason::SourceEnded);
    for (at, ok) in [(T0 + 150, false), (T0 + 160, true)] {
        w.apply(&ev(
            at,
            EventKind::NotifySent {
                dedup: "test-key".into(),
                mode: Some("active"),
                trace_id: None,
                test: false,
                key: "test".into(),
                phase: "firing",
                ok,
                code: None,
                suppressed: 0,
                undelivered: 0,
            },
        ))
        .unwrap();
    }
    let parts = query::summary(&w.conn, &q(T0, T0 + 601), "local", T0 + 1000).unwrap();
    let out = query::render_summary(&parts, &[]);
    assert_eq!(out["kpis"]["live_secs"], 600);
    assert_eq!(parts.kpis.down_secs, 60);
    assert_eq!(out["kpis"]["availability"], 0.9, "540 / 600, not 600 / 660");
    assert_eq!(
        out["kpis"]["success_rate"], 0.5,
        "one clean and one failed run in one session"
    );
    assert_eq!(out["reliability"]["clean"], 1);
    assert_eq!(out["reliability"]["counted_failures"], 1);
    assert_eq!(out["kpis"]["notify_sent"], 2);
    assert_eq!(out["kpis"]["notify_failed"], 1);
    assert_eq!(parts.buckets.iter().map(|b| b.notify_sent).sum::<i64>(), 2);
    assert_eq!(
        parts.buckets.iter().map(|b| b.notify_failed).sum::<i64>(),
        1
    );

    let clipped = query::summary(&w.conn, &q(T0 + 50, T0 + 200), "local", T0 + 1000).unwrap();
    assert_eq!(clipped.kpis.live_secs, 150);
    assert_eq!(clipped.kpis.down_secs, 60);
    assert_eq!(
        clipped.kpis.sessions, 0,
        "an overlapping session is not a new start"
    );
    assert_eq!(clipped.buckets.iter().map(|b| b.sessions).sum::<i64>(), 0);
    let after_failure =
        query::summary(&w.conn, &q(T0 + 110, T0 + 200), "local", T0 + 1000).unwrap();
    assert_eq!(after_failure.kpis.failures, 0);
    assert_eq!(after_failure.kpis.recover_n, 0);
    assert!(after_failure
        .buckets
        .iter()
        .all(|b| b.failures == 0 && b.recover_n == 0));
}

#[test]
fn starts_are_half_open_and_appear_only_in_their_start_bucket() {
    let (mut w, _) = writer("metric-starts");
    session(&mut w, 1, T0 - 100, None);
    end(&mut w, 1, T0 + 400, EndReason::SourceEnded);
    session(&mut w, 2, T0 + 3600, None);
    end(&mut w, 2, T0 + 4000, EndReason::SourceEnded);
    session(&mut w, 3, T0 + 7200, None);
    let mut window = q(T0, T0 + 7200);
    window.bucket = Some("hour".into());
    let parts = query::summary(&w.conn, &window, "local", T0 + 7300).unwrap();
    assert_eq!(parts.kpis.sessions, 1);
    assert_eq!(
        parts
            .buckets
            .iter()
            .map(|b| (b.start, b.sessions))
            .collect::<Vec<_>>(),
        vec![(T0, 0), (T0 + 3600, 1)]
    );
    assert_eq!(parts.kpis.live_secs, 800, "duration still includes overlap");
}

#[test]
fn opening_count_is_one_per_broadcast_not_per_relay_session() {
    let (mut w, _) = writer("metric-broadcasts");
    // The same B站 broadcast relayed twice: the relay drops and restarts under
    // the same live key. Two ffmpeg sessions, one 开播场次.
    session(&mut w, 1, T0, Some("K1"));
    end(&mut w, 1, T0 + 600, EndReason::SourceEnded);
    session(&mut w, 2, T0 + 700, Some("K1"));
    end(&mut w, 2, T0 + 1200, EndReason::SourceEnded);
    // A later broadcast under a new key is a second 场次.
    session(&mut w, 3, T0 + 3600, Some("K2"));
    let parts = query::summary(&w.conn, &q(T0, T0 + 7200), "local", T0 + 7300).unwrap();
    assert_eq!(parts.kpis.sessions, 2, "broadcasts, not ffmpeg sessions");
    assert_eq!(parts.buckets.iter().map(|b| b.sessions).sum::<i64>(), 2);
    let out = query::render_summary(&parts, &[]);
    assert_eq!(out["kpis"]["sessions"], 2);
    let text = out.to_string();
    assert!(
        !text.contains("K1") && !text.contains("K2"),
        "the live key never leaves"
    );
}

#[test]
fn opening_count_falls_back_to_the_source_id_then_the_session() {
    let (mut w, _) = writer("metric-broadcasts-source");
    // No B站 key: the relayed source video identifies the broadcast.
    session_parts(&mut w, 1, T0, None, Some("vid123"));
    end(&mut w, 1, T0 + 300, EndReason::SourceEnded);
    session_parts(&mut w, 2, T0 + 400, None, Some("vid123"));
    end(&mut w, 2, T0 + 900, EndReason::SourceEnded);
    session_parts(&mut w, 3, T0 + 1000, None, Some("vid456"));
    // With neither id the session is the only identity left.
    session(&mut w, 4, T0 + 2000, None);
    session(&mut w, 5, T0 + 2100, None);
    let parts = query::summary(&w.conn, &q(T0, T0 + 3600), "local", T0 + 3600).unwrap();
    assert_eq!(
        parts.kpis.sessions, 4,
        "vid123 twice, vid456, and two id-less relays"
    );
}

#[test]
fn one_broadcast_across_two_nodes_counts_once() {
    let (mut a, _) = writer_node("metric-broadcast-a", "tokyo-1");
    session(&mut a, 1, T0, Some("K1"));
    end(&mut a, 1, T0 + 600, EndReason::ClusterHandoff);
    let (mut b, _) = writer_node("metric-broadcast-b", "tokyo-2");
    session(&mut b, 1, T0 + 600, Some("K1"));
    end(&mut b, 1, T0 + 1200, EndReason::SourceEnded);
    let window = q(T0, T0 + 3600);
    let parts: Vec<_> = [(&a, "tokyo-1"), (&b, "tokyo-2")]
        .into_iter()
        .map(|(w, node)| query::summary(&w.conn, &window, node, T0 + 1300).unwrap())
        .collect();
    let merged = query::merge_summary(parts);
    assert_eq!(merged.kpis.sessions, 1, "one broadcast, two node sessions");
    assert_eq!(merged.kpis.live_secs, 1200, "duration still counts both");
    assert_eq!(merged.buckets.iter().map(|b| b.sessions).sum::<i64>(), 1);

    // A peer from before `broadcasts` contributes only its own count.
    let (mut old, _) = writer_node("metric-broadcast-old", "old-1");
    session(&mut old, 1, T0 + 30, Some("K9"));
    let mut legacy = query::summary(&old.conn, &window, "old-1", T0 + 1300).unwrap();
    legacy.kpis.sessions = 3;
    legacy.kpis.broadcasts.clear();
    let merged = query::merge_summary(vec![merged, legacy]);
    assert_eq!(
        merged.kpis.sessions, 4,
        "one broadcast plus the legacy count"
    );
}

#[test]
fn availability_and_recovery_do_not_use_the_fifty_gap_display_cap() {
    let (mut w, _) = writer("metric-many-gaps");
    session(&mut w, 1, T0, None);
    for i in 0..60u64 {
        exited(&mut w, 1, 10 + i, T0 + i as i64 * 20 + 10, false, true);
        w.apply(&ev(
            T0 + (i + 1) as i64 * 20,
            EventKind::FfmpegStarted {
                seq: 1,
                run: 11 + i,
            },
        ))
        .unwrap();
    }
    exited(&mut w, 1, 70, T0 + 1210, true, false);
    end(&mut w, 1, T0 + 1210, EndReason::SourceEnded);
    assert_eq!(sessions(&w, T0 + 1300).items[0].gaps.len(), 50);
    let parts = query::summary(&w.conn, &q(T0, T0 + 1300), "local", T0 + 1300).unwrap();
    assert_eq!(parts.kpis.down_secs, 600);
    assert_eq!(parts.kpis.recover_n, 60);
    assert_eq!(parts.kpis.recover_sum, 600);
    assert_eq!(
        query::render_summary(&parts, &[])["kpis"]["availability"],
        json!(610.0 / 1210.0)
    );
}

#[test]
fn non_failure_gaps_and_unrecovered_tails_are_downtime_but_not_recovery() {
    let (mut w, _) = writer("metric-open-tail");
    session(&mut w, 1, T0, None);
    exited(&mut w, 1, 10, T0 + 100, false, false);
    w.apply(&ev(T0 + 160, EventKind::FfmpegStarted { seq: 1, run: 11 }))
        .unwrap();
    exited(&mut w, 1, 11, T0 + 200, false, true);
    let open = query::summary(&w.conn, &q(T0, T0 + 300), "local", T0 + 300).unwrap();
    assert_eq!(open.kpis.live_secs, 300);
    assert_eq!(
        open.kpis.down_secs, 160,
        "includes both the non-failure gap and open tail"
    );
    assert_eq!(open.kpis.recover_n, 0);
    end(&mut w, 1, T0 + 400, EndReason::Aborted);
    let closed = query::summary(&w.conn, &q(T0, T0 + 500), "local", T0 + 500).unwrap();
    assert_eq!(closed.kpis.down_secs, 260);
    assert_eq!(
        closed.kpis.recover_n, 0,
        "session end is not a recovered push"
    );
    assert!(closed.buckets.iter().all(|b| b.recover_n == 0));
}

#[test]
fn delayed_exit_classification_keeps_the_captured_process_exit_time() {
    let (mut w, _) = writer("delayed-exit-classification");
    session(&mut w, 1, T0, None);
    let exited_at = T0 + 100;
    // Source/B站 checks finish later while the writer keeps receiving its
    // own heartbeat. The exit event carries the time captured after wait.
    w.heartbeat((T0 + 200) as u64).unwrap();
    exited(&mut w, 1, 10, exited_at, false, true);
    w.apply(&ev(T0 + 220, EventKind::FfmpegStarted { seq: 1, run: 11 }))
        .unwrap();
    exited(&mut w, 1, 11, T0 + 600, true, false);
    end(&mut w, 1, T0 + 600, EndReason::SourceEnded);
    let end: i64 = w
        .conn
        .query_row("SELECT ended_at FROM runs WHERE run=10", [], |r| r.get(0))
        .unwrap();
    assert_eq!(end, exited_at);
    let parts = query::summary(&w.conn, &q(T0, T0 + 601), "local", T0 + 601).unwrap();
    assert_eq!(parts.kpis.down_secs, 120);
    assert_eq!(parts.kpis.recover_sum, 120);
    assert_eq!(parts.kpis.recover_n, 1);
    assert_eq!(
        query::render_summary(&parts, &[])["kpis"]["availability"],
        0.8
    );
}

#[test]
fn reliability_excludes_open_unclassified_and_planned_terminal_runs() {
    let (mut w, _) = writer("metric-planned");
    session(&mut w, 1, T0, None);
    exited(&mut w, 1, 10, T0 + 100, false, true);
    w.apply(&ev(T0 + 160, EventKind::FfmpegStarted { seq: 1, run: 11 }))
        .unwrap();
    exited(&mut w, 1, 11, T0 + 400, true, false);
    end(&mut w, 1, T0 + 400, EndReason::SourceEnded);
    for (seq, reason) in [
        (2, EndReason::ClusterHandoff),
        (3, EndReason::IntentionalRestart),
    ] {
        let start = T0 + seq as i64 * 200 + 100;
        session(&mut w, seq, start, None);
        exited(&mut w, seq, seq * 10, start + 100, true, false);
        end(&mut w, seq, start + 100, reason);
    }
    session(&mut w, 4, T0 + 900, None);
    end(&mut w, 4, T0 + 950, EndReason::ProcessRestart);
    session(&mut w, 5, T0 + 960, None);
    let parts = query::summary(&w.conn, &q(T0, T0 + 1000), "local", T0 + 1000).unwrap();
    let out = query::render_summary(&parts, &[]);
    assert_eq!((parts.clean, parts.planned, parts.kpis.failures), (1, 2, 1));
    assert_eq!(out["kpis"]["success_rate"], 0.5);
}

struct NoLocalHistory;

impl Peers for NoLocalHistory {
    fn fetch<'a>(
        &'a self,
        node: &'a str,
        req: &'a PeerRequest,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Value, super::MissingNode>> + Send + 'a>,
    > {
        Box::pin(async move { Ok(api::empty_parts(req, node)) })
    }
}

#[tokio::test]
async fn absent_local_history_keeps_real_peer_sessions_summary_and_samples() {
    let now = T0 + 1000;
    let (mut w, path) = writer("peer-only-data");
    session(&mut w, 1, T0, None);
    end(&mut w, 1, T0 + 600, EndReason::SourceEnded);
    raw(&w, T0 + 10, 321.0);
    drop(w);
    let peer = LocalConn(std::sync::Mutex::new(schema::open_ro(&path).unwrap()), now);
    let mut cfg = cluster_cfg();
    cfg.cluster.peers.truncate(1);
    for scope in ["cluster", "tokyo-1"] {
        let window = params(&[
            ("from", &T0.to_string()),
            ("to", &now.to_string()),
            ("scope", scope),
        ]);
        let sessions = api::answer(&cfg, "sessions", &window, &peer, &NoLocalHistory, now)
            .await
            .unwrap();
        assert_eq!(sessions["items"].as_array().unwrap().len(), 1);
        let summary = api::answer(&cfg, "summary", &window, &peer, &NoLocalHistory, now)
            .await
            .unwrap();
        assert_eq!(summary["kpis"]["live_secs"], 600);
        let stats = api::answer(&cfg, "stats", &window, &peer, &NoLocalHistory, now)
            .await
            .unwrap();
        assert!(stats["series"]["bitrate"]["mean"]
            .as_array()
            .unwrap()
            .contains(&json!(321.0)));
        assert!(stats["missing_nodes"].as_array().unwrap().is_empty());
    }
}

#[tokio::test]
async fn mixed_retention_keeps_peer_samples_on_the_coordinator_grid() {
    let now = T0 + 1000;
    let (w, path) = writer("mixed-grid");
    let recent = (now - 10 * DAY).div_euclid(60) * 60;
    let expired = (now - 40 * DAY).div_euclid(60) * 60;
    for (at, value) in [(recent, 987.0), (expired, 123.0)] {
        w.conn.execute("INSERT INTO samples_1m (metric,node,bucket,min,max,sum,count) VALUES (1,'tokyo-1',?1,?2,?2,?2,1)", rusqlite::params![at, value]).unwrap();
    }
    drop(w);
    let mut cfg = cluster_cfg();
    cfg.cluster.peers.truncate(1);
    let mut small_cfg = cfg.clone();
    small_cfg.history.retention.rollup_days = 30;
    let peer = MixedPeers {
        small: std::sync::Mutex::new(schema::open_ro(&path).unwrap()),
        small_cfg,
        now,
        offline: vec![],
    };
    let from = now - 60 * DAY;
    let window = params(&[
        ("from", &from.to_string()),
        ("to", &now.to_string()),
        ("max_points", "1000"),
    ]);
    let stats = api::answer(&cfg, "stats", &window, &peer, &NoLocalHistory, now)
        .await
        .unwrap();
    let (step, _) = query::stats_layout(from, now, "1m", 1000);
    let i = ((recent - from) / step) as usize;
    assert_eq!(stats["ts"][i], from + i as i64 * step);
    assert_eq!(stats["series"]["bitrate"]["mean"][i], 987.0);
    assert!(
        !stats["series"]["bitrate"]["mean"]
            .as_array()
            .unwrap()
            .contains(&json!(123.0)),
        "policy still clamps SQL reads"
    );
    assert_eq!(
        stats["partial_nodes"],
        json!([{"node":"tokyo-1","clamped_to_days":30}])
    );
    assert_eq!(stats["missing_nodes"], json!([]));
}

#[tokio::test]
async fn shorter_raw_retention_uses_rollups_and_a_nonduplicated_raw_tail() {
    let now = T0 + 1000;
    let (w, path) = writer("mixed-raw-grid");
    let old = (now - 2 * DAY + 600).div_euclid(60) * 60;
    let recent = now - 600;
    raw(&w, old, 17.0);
    raw(&w, recent, 29.0);
    maintain::rollup_1m(&w.conn, now).unwrap();
    maintain::prune_raw(&w.conn, now, 1).unwrap();
    drop(w);
    let mut cfg = cluster_cfg();
    cfg.cluster.peers.truncate(1);
    let mut small_cfg = cfg.clone();
    small_cfg.history.retention.raw_days = 1;
    let peer = MixedPeers {
        small: std::sync::Mutex::new(schema::open_ro(&path).unwrap()),
        small_cfg,
        now,
        offline: vec![],
    };
    let from = now - 2 * DAY;
    let window = params(&[("from", &from.to_string()), ("to", &now.to_string())]);
    let data = api::answer(&cfg, "stats", &window, &peer, &NoLocalHistory, now)
        .await
        .unwrap();
    let (step, _) = query::stats_layout(from, now, "raw", 1000);
    assert_eq!(data["resolution"], "1m");
    assert_eq!(
        data["series"]["bitrate"]["mean"][((old - from) / step) as usize],
        17.0
    );
    assert_eq!(
        data["series"]["bitrate"]["mean"][((recent - from) / step) as usize],
        29.0
    );
    assert_eq!(data["missing_nodes"], json!([]));
    assert!(
        data.get("partial_nodes").is_none(),
        "all requested coverage is available at coarser resolution"
    );
    let mut req = PeerRequest {
        name: "stats".into(),
        query: q(from, now),
    };
    req.query.tier = Some("raw".into());
    api::check_peer_request(&mut req, &peer.small_cfg, now).unwrap();
    let parts = query::stats(&peer.small.lock().unwrap(), &req.query, "raw", None).unwrap();
    assert_eq!(
        parts.series["bitrate"]
            .iter()
            .flatten()
            .map(|cell| cell.3)
            .sum::<i64>(),
        2,
        "a retained raw sample must not also be counted through its rollup"
    );
}

#[test]
fn empty_stats_parts_never_define_the_merge_grid() {
    let (w, _) = writer("empty-stats-order");
    raw(&w, T0, 123.0);
    let part = query::stats(&w.conn, &q(T0, T0 + 60), "raw", None).unwrap();
    for parts in [
        vec![query::StatsParts::default(), part.clone()],
        vec![part.clone(), query::StatsParts::default()],
    ] {
        assert_eq!(query::merge_stats(parts), Some(part.clone()));
    }
}

#[tokio::test]
async fn a_peer_grid_that_does_not_match_the_query_is_reported_as_missing() {
    let now = T0 + 1000;
    let mut cfg = cluster_cfg();
    cfg.cluster.peers.truncate(1);
    let mut part = query::empty_stats(&q(T0 + 500, now), "raw");
    part.series.get_mut("bitrate").unwrap()[0] = Some((987.0, 987.0, 987.0, 1));
    let peers = MockPeers(HashMap::from([(
        "tokyo-1".into(),
        Ok(serde_json::to_value(part).unwrap()),
    )]));
    let window = params(&[("from", &T0.to_string()), ("to", &now.to_string())]);
    let data = api::answer(&cfg, "stats", &window, &peers, &NoLocalHistory, now)
        .await
        .unwrap();
    assert_eq!(data["missing_nodes"][0]["node_id"], "tokyo-1");
    assert_eq!(data["missing_nodes"][0]["reason"], "error");
}

#[test]
fn session_stats_and_online_peak_respect_session_and_range_boundaries() {
    let (mut w, _) = writer("session-stat-boundary");
    session(&mut w, 1, T0, None);
    w.apply(&ev(
        T0 + 10,
        EventKind::StatsSample {
            seq: 1,
            bitrate_kbps: (100.0, 100.0, 100.0),
            fps: None,
            speed: None,
        },
    ))
    .unwrap();
    w.apply(&ev(
        T0 + 10,
        EventKind::BiliCounts {
            seq: 1,
            room: 1,
            online: Some(900),
            watched: None,
        },
    ))
    .unwrap();
    w.apply(&ev(
        T0 + 30,
        EventKind::BiliCounts {
            seq: 1,
            room: 1,
            online: Some(200),
            watched: None,
        },
    ))
    .unwrap();
    end(&mut w, 1, T0 + 100, EndReason::SourceEnded);
    let id = sessions(&w, T0 + 101).items[0].id.clone();
    session(&mut w, 2, T0 + 100, None);
    w.apply(&ev(
        T0 + 110,
        EventKind::StatsSample {
            seq: 2,
            bitrate_kbps: (800.0, 800.0, 800.0),
            fps: None,
            speed: None,
        },
    ))
    .unwrap();
    let mut qq = q(T0, T0 + 200);
    qq.session = Some(id);
    let stats = query::render_stats(&query::stats(&w.conn, &qq, "raw", None).unwrap());
    let means = stats["series"]["bitrate"]["mean"].as_array().unwrap();
    assert!(means.contains(&json!(100.0)));
    assert!(
        !means.contains(&json!(800.0)),
        "a session query never includes its neighbour's samples"
    );
    // One channel segment can be relayed by several sessions (interruption,
    // restart); the chart then covers every member in one request.
    let second = sessions(&w, T0 + 200).items[0].id.clone();
    let mut both = q(T0, T0 + 200);
    both.session = Some(format!(
        "{first},{second}",
        first = qq.session.as_deref().unwrap()
    ));
    let stats = query::render_stats(&query::stats(&w.conn, &both, "raw", None).unwrap());
    let means = stats["series"]["bitrate"]["mean"].as_array().unwrap();
    assert!(means.contains(&json!(100.0)) && means.contains(&json!(800.0)));
    let summary = query::summary(&w.conn, &q(T0 + 20, T0 + 100), "local", T0 + 200).unwrap();
    assert_eq!(
        summary.kpis.online_peak,
        Some(200.0),
        "an earlier session peak outside the range is excluded"
    );
    let edge = query::render_stats(&query::stats(&w.conn, &q(T0, T0 + 10), "raw", None).unwrap());
    assert!(
        edge["series"]["bitrate"]["mean"]
            .as_array()
            .unwrap()
            .iter()
            .all(Value::is_null),
        "stats use a half-open interval"
    );
}

#[tokio::test]
async fn short_coordinator_retention_does_not_truncate_a_longer_peer() {
    let now = T0 + 1000;
    let old = (now - 5 * DAY).div_euclid(60) * 60;
    let recent = (now - 3600).div_euclid(60) * 60;
    let (mut local_writer, local_path) = writer("inverse-retention-local");
    session(&mut local_writer, 1, old, None);
    end(&mut local_writer, 1, old + 900, EndReason::SourceEnded);
    session(&mut local_writer, 2, recent, None);
    end(&mut local_writer, 2, recent + 300, EndReason::SourceEnded);
    raw(&local_writer, recent + 10, 321.0);
    maintain::rollup_1m(&local_writer.conn, now).unwrap();
    drop(local_writer);
    let (mut peer_writer, peer_path) = writer_node("inverse-retention-peer", "tokyo-1");
    session(&mut peer_writer, 1, old, None);
    end(&mut peer_writer, 1, old + 600, EndReason::SourceEnded);
    raw(&peer_writer, old + 10, 987.0);
    maintain::rollup_1m(&peer_writer.conn, now).unwrap();
    drop(peer_writer);
    let mut cfg = cluster_cfg();
    cfg.cluster.peers.truncate(1);
    cfg.history.retention.rollup_days = 1;
    let mut peer_cfg = cfg.clone();
    peer_cfg.history.retention.rollup_days = 7;
    let peer = MixedPeers {
        small: std::sync::Mutex::new(schema::open_ro(&peer_path).unwrap()),
        small_cfg: peer_cfg,
        now,
        offline: vec![],
    };
    let local = LocalConn(
        std::sync::Mutex::new(schema::open_ro(&local_path).unwrap()),
        now,
    );
    for scope in ["cluster", "tokyo-1"] {
        let window = params(&[
            ("from", &(now - 7 * DAY).to_string()),
            ("to", &now.to_string()),
            ("scope", scope),
        ]);
        let summary = api::answer(&cfg, "summary", &window, &peer, &local, now)
            .await
            .unwrap();
        assert_eq!(
            summary["kpis"]["live_secs"],
            if scope == "cluster" { 900 } else { 600 }
        );
        assert!(
            summary["buckets"]
                .as_array()
                .unwrap()
                .iter()
                .all(|b| b["start"].as_i64().unwrap() % (6 * 3600) == 0),
            "all sources retain the requested six-hour buckets"
        );
        let sessions = api::answer(&cfg, "sessions", &window, &peer, &local, now)
            .await
            .unwrap();
        assert_eq!(
            sessions["items"].as_array().unwrap().len(),
            if scope == "cluster" { 2 } else { 1 }
        );
        assert!(sessions["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["node"] == "tokyo-1" && s["started_at"] == old));
        let stats = api::answer(&cfg, "stats", &window, &peer, &local, now)
            .await
            .unwrap();
        assert!(stats["series"]["bitrate"]["mean"]
            .as_array()
            .unwrap()
            .contains(&json!(987.0)));
        assert_eq!(stats["ts"][0], now - 7 * DAY);
        for data in [summary, sessions, stats] {
            assert_eq!(data["missing_nodes"], json!([]));
            assert!(data.get("partial_nodes").is_none());
            if scope == "cluster" {
                assert_eq!(data["clamped_to_days"], 1);
                assert_eq!(data["clamp_scope"], "local");
            } else {
                assert!(
                    data.get("clamped_to_days").is_none(),
                    "remote scope never inherits local retention metadata"
                );
                assert!(data.get("clamp_scope").is_none());
            }
        }
    }
}

// --------------------------------------------------------- clear history

/// Every table the operator can see through the 统计 view, children first.
const CLEARED: [&str; 9] = [
    "runs",
    "events",
    "notify_sends",
    "node_segments",
    "samples_1m",
    "sample_frames",
    "bili_lives",
    "sessions",
    "meta",
];

#[test]
fn clear_empties_every_recorded_table_but_keeps_the_process_book() {
    let (mut w, path) = writer("clear");
    session(&mut w, 1, T0, Some("key-a"));
    w.apply(&ev(
        T0 + 60,
        EventKind::StatsSample {
            seq: 1,
            bitrate_kbps: (1000.0, 2000.0, 3000.0),
            fps: Some(30.0),
            speed: Some(1.0),
        },
    ))
    .unwrap();
    w.apply(&ev(
        T0 + 60,
        EventKind::BiliLiveStarted {
            live_key: "key-a".into(),
            room: 1,
        },
    ))
    .unwrap();
    w.apply(&ev(
        T0 + 60,
        EventKind::BiliStartFailed {
            seq: Some(1),
            error: "示例失败".into(),
        },
    ))
    .unwrap();
    end(&mut w, 1, T0 + 120, EndReason::BiliStopped);

    // The fixture must actually fill the tables the button claims to clear.
    for table in [
        "runs",
        "events",
        "sample_frames",
        "bili_lives",
        "sessions",
        "meta",
    ] {
        assert!(count(&w, table) > 0, "{table} fixture is empty");
    }
    let before: i64 = CLEARED.iter().map(|t| count(&w, t)).sum();

    let rows = maintain::clear(&w.conn).unwrap();
    assert_eq!(rows, before as u64, "the count covers every deleted row");
    for table in CLEARED {
        assert_eq!(count(&w, table), 0, "{table} still has rows");
    }
    assert_eq!(count(&w, "boots"), 1, "boots is process bookkeeping");

    // The writer's in-memory maps must forget the deleted ids, and session ids
    // restart at 1 because no rows are left to collide with.
    w.forget();
    session(&mut w, 1, T0 + DAY, Some("key-b"));
    end(&mut w, 1, T0 + DAY + 60, EndReason::BiliStopped);
    let id: i64 = w
        .conn
        .query_row("SELECT id FROM sessions", [], |r| r.get(0))
        .unwrap();
    assert_eq!(id, 1);
    assert_eq!((count(&w, "sessions"), count(&w, "runs")), (1, 1));
    assert_eq!(count(&w, "boots"), 1);

    // Everything is committed: a read-only connection sees the empty file.
    let ro = schema::open_ro(&path).unwrap();
    let sessions: i64 = ro
        .query_row("SELECT count(*) FROM sessions", [], |r| r.get(0))
        .unwrap();
    assert_eq!(sessions, 1);
}

#[test]
fn observations_for_a_cleared_session_are_dropped_not_reinserted() {
    let (mut w, _) = writer("clear-stale");
    session(&mut w, 1, T0, Some("key-a"));
    maintain::clear(&w.conn).unwrap();
    w.forget();
    // Samples and runs already queued (or re-sent) for the deleted session must
    // not resurrect rows or their foreign keys.
    for kind in [
        EventKind::StatsSample {
            seq: 1,
            bitrate_kbps: (1.0, 2.0, 3.0),
            fps: None,
            speed: None,
        },
        EventKind::BiliCounts {
            seq: 1,
            room: 1,
            online: Some(10),
            watched: Some(20),
        },
        EventKind::FfmpegStarted { seq: 1, run: 10 },
        EventKind::FfmpegExited {
            seq: 1,
            run: 10,
            code: Some(0),
            success: true,
            counted_failure: false,
            diagnostic: None,
        },
    ] {
        w.apply(&ev(T0 + 1, kind)).unwrap();
    }
    for table in ["sessions", "runs", "sample_frames", "bili_lives", "events"] {
        assert_eq!(count(&w, table), 0, "{table} was resurrected");
    }
}

#[test]
fn live_totals_from_a_28a20792_peer_merge_by_their_stated_source() {
    // 28a20792 always states `watched_source` beside `watched`; the merge keeps
    // the summary over a sample and carries the source it was given.
    let part = |node: &str, watched: i64, source: &str, at: i64| {
        let mut p = query::SummaryParts {
            source: Some(node.into()),
            ..Default::default()
        };
        p.kpis.lives.insert(
            "live:synthetic".into(),
            query::LiveTotals {
                watched: Some(watched),
                watched_source: Some(source.into()),
                watched_at: Some(at),
                ..Default::default()
            },
        );
        // Over the wire, exactly as a peer serializes it.
        serde_json::from_value::<query::SummaryParts>(serde_json::to_value(p).unwrap()).unwrap()
    };
    for parts in [
        vec![
            part("a", 900, "sample", T0 + 50),
            part("b", 700, "summary", T0),
        ],
        vec![
            part("b", 700, "summary", T0),
            part("a", 900, "sample", T0 + 50),
        ],
    ] {
        let merged = query::merge_summary(parts);
        let live = &merged.kpis.lives["live:synthetic"];
        assert_eq!(live.watched, Some(700));
        assert_eq!(live.watched_source.as_deref(), Some("summary"));
        assert_eq!(live.watched_at, Some(T0));
    }
    let merged = query::merge_summary(vec![
        part("a", 500, "sample", T0),
        part("b", 650, "sample", T0 + 30),
    ]);
    let live = &merged.kpis.lives["live:synthetic"];
    assert_eq!(live.watched, Some(650), "the newer sample wins");
    assert_eq!(live.watched_source.as_deref(), Some("sample"));
}

// ------------------------------------------- cutoff-reason (PRD R2/R5)

const V2_TEXT: &str =
    "您本场直播存在挂机、录播等消极直播行为，因此直播被切断，请您及时整改（切断本场直播）";
const PLAIN_TEXT: &str = "违反直播言论规范，请立即调整";

fn cut(w: &mut Writer, at: i64, seq: Option<u64>, msg: &str, rank: u8) {
    w.apply(&ev(
        at,
        EventKind::BiliCutOff {
            seq,
            room: 1,
            msg: msg.into(),
            rank,
            merge: true,
        },
    ))
    .unwrap();
}

fn warning(w: &mut Writer, at: i64, msg: &str, rank: u8) {
    w.apply(&ev(
        at,
        EventKind::BiliWarning {
            seq: Some(1),
            room: 1,
            msg: msg.into(),
            rank,
            merge: true,
            stop: true,
        },
    ))
    .unwrap();
}

/// A warning frame outside the merge window (dialog, mask).
fn warning_own_row(w: &mut Writer, at: i64, msg: &str, rank: u8) {
    w.apply(&ev(
        at,
        EventKind::BiliWarning {
            seq: Some(1),
            room: 1,
            msg: msg.into(),
            rank,
            merge: false,
            stop: true,
        },
    ))
    .unwrap();
}

fn violation_rows(w: &Writer, kind: &str) -> Vec<String> {
    let mut stmt = w
        .conn
        .prepare("SELECT detail FROM events WHERE kind=?1 ORDER BY id")
        .unwrap();
    stmt.query_map([kind], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}

#[test]
fn cut_off_then_v2_is_one_row_upgraded_to_the_labelled_reason() {
    let (mut w, _) = writer("cut-then-v2");
    session(&mut w, 1, T0, None);
    cut(&mut w, T0 + 100, Some(1), PLAIN_TEXT, 4);
    cut(&mut w, T0 + 101, Some(1), V2_TEXT, 7);
    assert_eq!(
        violation_rows(&w, "bili.cut_off"),
        vec![V2_TEXT.to_string()]
    );
}

#[test]
fn v2_then_cut_off_keeps_the_labelled_reason_and_one_row() {
    let (mut w, _) = writer("v2-then-cut");
    session(&mut w, 1, T0, None);
    cut(&mut w, T0 + 100, Some(1), V2_TEXT, 7);
    cut(&mut w, T0 + 100, Some(1), PLAIN_TEXT, 4);
    // A frame with no reason at all never adds or overwrites either.
    cut(&mut w, T0 + 102, Some(1), "直播间切断", 0);
    assert_eq!(
        violation_rows(&w, "bili.cut_off"),
        vec![V2_TEXT.to_string()]
    );
}

#[test]
fn v2_alone_is_one_row_and_a_later_cut_is_a_new_row() {
    let (mut w, _) = writer("v2-alone");
    session(&mut w, 1, T0, None);
    cut(&mut w, T0 + 100, Some(1), V2_TEXT, 7);
    assert_eq!(
        violation_rows(&w, "bili.cut_off"),
        vec![V2_TEXT.to_string()]
    );
    // Outside the window it is a different cut, even with the same text.
    cut(
        &mut w,
        T0 + 101 + super::VIOLATION_WINDOW_SECS,
        Some(1),
        V2_TEXT,
        7,
    );
    assert_eq!(violation_rows(&w, "bili.cut_off").len(), 2);
}

#[test]
fn repeated_warning_is_one_row_but_a_different_warning_is_kept() {
    let (mut w, _) = writer("warn-repeat");
    session(&mut w, 1, T0, None);
    warning(&mut w, T0 + 10, "图片内容不适宜，请立即调整", 4);
    warning(&mut w, T0 + 20, "图片内容不适宜，请立即调整", 4);
    assert_eq!(violation_rows(&w, "bili.warning.stop").len(), 1);
    // L4: a different warning, even one without text, keeps its own row.
    warning(&mut w, T0 + 21, "直播间警告", 0);
    warning(&mut w, T0 + 30, "禁止直播违禁游戏，请立即更换", 4);
    assert_eq!(
        violation_rows(&w, "bili.warning.stop"),
        vec![
            "图片内容不适宜，请立即调整".to_string(),
            "直播间警告".to_string(),
            "禁止直播违禁游戏，请立即更换".to_string()
        ]
    );
    // A warning never merges into or with a cut.
    assert!(violation_rows(&w, "bili.cut_off").is_empty());
}

#[test]
fn a_rolled_back_batch_forgets_its_violation_row() {
    let (mut w, _) = writer("cut-rollback");
    session(&mut w, 1, T0, None);
    let bad = ev(
        T0 + 101,
        EventKind::SessionStarted {
            area_v2: None,
            room: 1,
            seq: 1,
            platform: "YT",
            channel_id: "UC1".into(),
            channel_name: "dup".into(),
            title: None,
            live_key: None,
            source_id: None,
        },
    );
    let first = ev(
        T0 + 100,
        EventKind::BiliCutOff {
            seq: Some(1),
            room: 1,
            msg: PLAIN_TEXT.into(),
            rank: 4,
            merge: true,
        },
    );
    assert!(
        w.commit(&[first, bad]).is_err(),
        "duplicate seq fails the batch"
    );
    cut(&mut w, T0 + 102, Some(1), V2_TEXT, 7);
    let rows = violation_rows(&w, "bili.cut_off");
    assert_eq!(rows.last().map(String::as_str), Some(V2_TEXT), "{rows:?}");
    assert!(rows.len() <= 1, "{rows:?}");
}

fn only_session(w: &Writer, now: i64) -> Value {
    let page = sessions(w, now);
    assert_eq!(page.items.len(), 1);
    serde_json::to_value(&page.items[0]).unwrap()
}

#[test]
fn cut_off_session_with_v2_reason_has_exact_json_fields() {
    let (mut w, _) = writer("r5-v2");
    session(&mut w, 1, T0, None);
    cut(&mut w, T0 + 100, Some(1), V2_TEXT, 7);
    end(&mut w, 1, T0 + 105, EndReason::BiliStopped);
    let item = only_session(&w, T0 + 200);
    assert_eq!(item["end_reason"], json!("bili_stopped"));
    assert_eq!(item["end_cut_off"], json!(true));
    assert_eq!(item["end_detail"], json!(V2_TEXT));
    assert!(item["end_cut_off"].is_boolean() && item["end_detail"].is_string());
}

#[test]
fn cut_off_session_with_plain_msg_only() {
    let (mut w, _) = writer("r5-plain");
    session(&mut w, 1, T0, None);
    cut(&mut w, T0 + 100, Some(1), PLAIN_TEXT, 4);
    end(&mut w, 1, T0 + 105, EndReason::BiliStopped);
    let item = only_session(&w, T0 + 200);
    assert_eq!(item["end_cut_off"], json!(true));
    assert_eq!(item["end_detail"], json!(PLAIN_TEXT));
}

#[test]
fn cut_off_session_with_only_fallback_has_flag_but_no_detail() {
    let (mut w, _) = writer("r5-fallback");
    session(&mut w, 1, T0, None);
    cut(&mut w, T0 + 100, Some(1), "直播间切断", 0);
    end(&mut w, 1, T0 + 105, EndReason::BiliStopped);
    let item = only_session(&w, T0 + 200);
    assert_eq!(item["end_cut_off"], json!(true));
    assert!(item.get("end_detail").is_none(), "{item}");
}

/// Clerk follow-up: the warning stock text joins the three cut-off ones in
/// one shared list, so no stock text can surface as a session's reason.
#[test]
fn every_stock_fallback_is_never_a_session_detail() {
    for (index, stock) in crate::history::STOCK_FALLBACKS.iter().enumerate() {
        assert!(crate::history::is_stock_detail(stock), "{stock}");
        let (mut w, _) = writer(&format!("r5-stock-{index}"));
        session(&mut w, 1, T0, None);
        cut(&mut w, T0 + 100, Some(1), stock, 0);
        end(&mut w, 1, T0 + 105, EndReason::BiliStopped);
        let item = only_session(&w, T0 + 200);
        assert_eq!(item["end_cut_off"], json!(true), "{stock}: {item}");
        assert!(item.get("end_detail").is_none(), "{stock}: {item}");
    }
    // A real reason is still shown, so the filter is not a blanket removal.
    assert!(!crate::history::is_stock_detail(
        "图片内容不适宜，请立即调整"
    ));
}

#[test]
fn cut_off_session_v2_then_plain_msg_shows_v2() {
    let (mut w, _) = writer("r5-v2-plain");
    session(&mut w, 1, T0, None);
    cut(&mut w, T0 + 100, Some(1), V2_TEXT, 7);
    cut(&mut w, T0 + 101, Some(1), PLAIN_TEXT, 4);
    end(&mut w, 1, T0 + 105, EndReason::BiliStopped);
    let item = only_session(&w, T0 + 200);
    assert_eq!(item["end_detail"], json!(V2_TEXT));
}

#[test]
fn warning_only_or_non_bili_stopped_sessions_have_no_cut_fields() {
    // A warning stop sets the warning fields, never the cut ones.
    let (mut w, _) = writer("r5-warning");
    session(&mut w, 1, T0, None);
    warning(&mut w, T0 + 100, "图片内容不适宜，请立即调整", 4);
    end(&mut w, 1, T0 + 105, EndReason::BiliStopped);
    let item = only_session(&w, T0 + 200);
    assert!(item.get("end_cut_off").is_none(), "{item}");
    assert_eq!(item["end_warning_stop"], json!(true));
    assert_eq!(item["end_detail"], json!("图片内容不适宜，请立即调整"));

    let (mut w, _) = writer("r5-other-end");
    session(&mut w, 1, T0, None);
    cut(&mut w, T0 + 100, Some(1), V2_TEXT, 7);
    end(&mut w, 1, T0 + 105, EndReason::UserStop);
    let item = only_session(&w, T0 + 200);
    assert!(item.get("end_cut_off").is_none(), "{item}");
    // An open session has no end, so no end fields either.
    let (mut w, _) = writer("r5-open");
    session(&mut w, 1, T0, None);
    cut(&mut w, T0 + 100, Some(1), V2_TEXT, 7);
    let item = only_session(&w, T0 + 200);
    assert!(item.get("end_cut_off").is_none(), "{item}");
}

#[test]
fn multiple_cut_offs_show_the_latest_non_fallback_detail() {
    let (mut w, _) = writer("r5-multi");
    session(&mut w, 1, T0, None);
    cut(&mut w, T0 + 100, Some(1), PLAIN_TEXT, 4);
    cut(&mut w, T0 + 300, Some(1), V2_TEXT, 7);
    cut(&mut w, T0 + 500, Some(1), "直播间切断", 0);
    end(&mut w, 1, T0 + 505, EndReason::BiliStopped);
    let item = only_session(&w, T0 + 600);
    assert_eq!(violation_rows(&w, "bili.cut_off").len(), 3);
    assert_eq!(item["end_cut_off"], json!(true));
    assert_eq!(item["end_detail"], json!(V2_TEXT));
}

#[test]
fn a_cut_recorded_after_the_session_ended_is_not_its_end() {
    // PRD R5 window is [started_at, ended_at]: the danmaku client records the
    // cut on receipt, before it stops the room, so a later row is not this
    // session's end.
    let (mut w, _) = writer("r5-late");
    session(&mut w, 1, T0, None);
    end(&mut w, 1, T0 + 100, EndReason::BiliStopped);
    cut(&mut w, T0 + 103, Some(1), V2_TEXT, 7);
    let item = only_session(&w, T0 + 200);
    assert!(item.get("end_cut_off").is_none(), "{item}");
}

#[test]
fn the_ended_event_of_a_cut_off_session_uses_the_cut_off_label() {
    let (mut w, _) = writer("r5-label");
    session(&mut w, 1, T0, None);
    cut(&mut w, T0 + 100, Some(1), V2_TEXT, 7);
    end(&mut w, 1, T0 + 105, EndReason::BiliStopped);
    session(&mut w, 2, T0 + 1000, None);
    end(&mut w, 2, T0 + 1100, EndReason::BiliStopped);
    let page = query::events(&w.conn, &q(T0 - 10, T0 + 2000)).unwrap();
    let ended: Vec<&str> = page
        .items
        .iter()
        .filter(|e| e.kind == "stream.ended")
        .map(|e| e.detail.as_str())
        .collect();
    assert_eq!(ended.len(), 2, "{ended:?}");
    assert!(
        ended.iter().any(|d| d.ends_with("· B站 切断直播")),
        "{ended:?}"
    );
    assert!(
        ended
            .iter()
            .any(|d| d.ends_with("· 正常结束（B站 已下播）")),
        "{ended:?}"
    );
    assert_eq!(
        query::end_reason_label("bili_stopped"),
        "正常结束（B站 已下播）"
    );
}

#[tokio::test]
async fn cluster_merge_keeps_cut_fields_and_accepts_an_older_peer_without_them() {
    let (mut w, path) = writer("r5-cluster");
    session(&mut w, 1, T0, None);
    cut(&mut w, T0 + 100, Some(1), V2_TEXT, 7);
    end(&mut w, 1, T0 + 105, EndReason::BiliStopped);
    let now = T0 + 1000;
    // tokyo-1 is an older build: its session JSON has no end_* fields, and it
    // still sends the prev_id/next_id keys this build dropped. Extra keys must
    // keep decoding, so a mixed cluster degrades instead of failing.
    let old_peer = json!({
        "items": [{
            "id": "tokyo-1:1:1", "eid": "tokyo-1:1:1", "node": "tokyo-1",
            "platform": "TW", "channel_id": "c", "channel_name": "n", "title": null,
            "started_at": T0 + 700, "ended_at": T0 + 800, "duration_secs": 100,
            "end_reason": "bili_stopped", "runs": 1, "restarts": 0, "failures": 0,
            "stuck": 0, "gaps": [], "median_bitrate_kbps": null, "bili": null
        }],
        "prev_id": null, "next_id": null, "next_cursor": null, "missing_nodes": []
    });
    // sg-3 is a new build that cut off with a reason.
    let new_peer = json!({
        "items": [{
            "id": "sg-3:1:1", "eid": "sg-3:1:1", "node": "sg-3",
            "platform": "TW", "channel_id": "c", "channel_name": "n", "title": null,
            "started_at": T0 + 500, "ended_at": T0 + 600, "duration_secs": 100,
            "end_reason": "bili_stopped", "runs": 1, "restarts": 0, "failures": 0,
            "stuck": 0, "gaps": [], "median_bitrate_kbps": null, "bili": null,
            "end_cut_off": true, "end_detail": PLAIN_TEXT
        }],
        "next_cursor": null, "missing_nodes": []
    });
    let peers = MockPeers(HashMap::from([
        ("tokyo-1".to_string(), Ok(old_peer)),
        ("sg-3".to_string(), Ok(new_peer)),
    ]));
    let local = LocalConn(std::sync::Mutex::new(schema::open_ro(&path).unwrap()), now);
    drop(w);
    let cfg = cluster_cfg();
    let window = params(&[("from", &(T0 - 10).to_string()), ("to", &now.to_string())]);
    let data = api::answer(&cfg, "sessions", &window, &peers, &local, now)
        .await
        .unwrap();
    let items = data["items"].as_array().unwrap();
    assert_eq!(items.len(), 3, "{data}");
    let by_node = |node: &str| items.iter().find(|i| i["node"] == node).unwrap();
    let old = by_node("tokyo-1");
    assert!(
        old.get("end_cut_off").is_none()
            && old.get("end_warning_stop").is_none()
            && old.get("end_detail").is_none()
    );
    assert_eq!(old["end_reason"], json!("bili_stopped"));
    assert_eq!(by_node("sg-3")["end_cut_off"], json!(true));
    assert_eq!(by_node("sg-3")["end_detail"], json!(PLAIN_TEXT));
    assert_eq!(by_node("local")["end_cut_off"], json!(true));
    assert_eq!(by_node("local")["end_detail"], json!(V2_TEXT));
}

#[test]
fn session_item_serializes_end_fields_as_a_bare_bool_and_string() {
    let mut item: query::SessionItem = serde_json::from_value(json!({
        "id": "n:1:1", "eid": "n:1:1", "node": "n", "platform": "YT",
        "channel_id": "c", "channel_name": "n", "title": null,
        "started_at": 1, "ended_at": 2, "duration_secs": 1, "end_reason": "bili_stopped",
        "runs": 1, "restarts": 0, "failures": 0, "stuck": 0, "gaps": [],
        "median_bitrate_kbps": null, "bili": null
    }))
    .unwrap();
    assert!(!item.end_cut_off && !item.end_warning_stop && item.end_detail.is_none());
    let plain = serde_json::to_value(&item).unwrap();
    assert!(
        plain.get("end_cut_off").is_none()
            && plain.get("end_warning_stop").is_none()
            && plain.get("end_detail").is_none()
    );
    item.end_cut_off = true;
    item.end_detail = Some(PLAIN_TEXT.into());
    let out = serde_json::to_value(&item).unwrap();
    assert_eq!(out["end_cut_off"], Value::Bool(true));
    assert_eq!(out["end_detail"], Value::String(PLAIN_TEXT.into()));
    let back: query::SessionItem = serde_json::from_value(out).unwrap();
    assert_eq!(back, item);
    item.end_cut_off = false;
    item.end_warning_stop = true;
    let out = serde_json::to_value(&item).unwrap();
    assert_eq!(out["end_warning_stop"], Value::Bool(true));
    assert!(out.get("end_cut_off").is_none());
}

// ------------------------------------------- PRD ruling 4 (kinds)

#[test]
fn ruling4_a_warning_dialog_never_sets_the_cut_off_fields() {
    let (mut w, _) = writer("r4-dialog");
    session(&mut w, 1, T0, None);
    // The 2024-11-13 dialog as the danmaku client now records it.
    warning_own_row(
        &mut w,
        T0 + 100,
        "您本场直播存在挂机、录播等消极直播行为，请及时整改（警告）",
        7,
    );
    end(&mut w, 1, T0 + 105, EndReason::BiliStopped);
    assert_eq!(violation_rows(&w, "bili.warning.stop").len(), 1);
    assert!(violation_rows(&w, "bili.cut_off").is_empty());
    let item = only_session(&w, T0 + 200);
    assert!(item.get("end_cut_off").is_none(), "{item}");
    assert_eq!(item["end_warning_stop"], json!(true));
    assert_eq!(
        item["end_detail"],
        json!("您本场直播存在挂机、录播等消极直播行为，请及时整改（警告）")
    );
    let page = query::events(&w.conn, &q(T0 - 10, T0 + 300)).unwrap();
    let ended = page
        .items
        .iter()
        .find(|e| e.kind == "stream.ended")
        .unwrap();
    assert!(
        ended.detail.ends_with("· B站 违规警示 · 已自动停播"),
        "{}",
        ended.detail
    );
}

#[test]
fn l4_a_dialog_or_mask_right_after_a_warning_keeps_its_own_row() {
    let (mut w, _) = writer("l4-own-rows");
    session(&mut w, 1, T0, None);
    warning(&mut w, T0 + 100, "图片内容不适宜，请立即调整", 4);
    // Mask (rank 2) and dialog (rank 7) within the window: never merged,
    // never dropped, never an upgrade of the WARNING row.
    warning_own_row(&mut w, T0 + 101, "当前直播间涉嫌违规", 2);
    warning_own_row(
        &mut w,
        T0 + 102,
        "您本场直播存在挂机、录播等消极直播行为，请及时整改（警告）",
        7,
    );
    // Even identical text in a non-WARNING frame keeps its row.
    warning_own_row(&mut w, T0 + 103, "当前直播间涉嫌违规", 2);
    // The WARNING repeated verbatim is still dropped.
    warning(&mut w, T0 + 104, "图片内容不适宜，请立即调整", 4);
    assert_eq!(
        violation_rows(&w, "bili.warning.stop"),
        vec![
            "图片内容不适宜，请立即调整".to_string(),
            "当前直播间涉嫌违规".to_string(),
            "您本场直播存在挂机、录播等消极直播行为，请及时整改（警告）".to_string(),
            "当前直播间涉嫌违规".to_string(),
        ]
    );
    assert!(violation_rows(&w, "bili.cut_off").is_empty());
}

#[test]
fn l4_only_cut_off_with_v2_merges_and_a_dialog_cut_keeps_its_row() {
    let (mut w, _) = writer("l4-cut");
    session(&mut w, 1, T0, None);
    cut(&mut w, T0 + 100, Some(1), PLAIN_TEXT, 4);
    cut(&mut w, T0 + 100, Some(1), V2_TEXT, 7);
    // A dialog whose 处罚结果 contains 切断: bili.cut_off, its own row.
    w.apply(&ev(
        T0 + 101,
        EventKind::BiliCutOff {
            seq: Some(1),
            room: 1,
            msg: "存在挂机行为（切断本场直播）".into(),
            rank: 7,
            merge: false,
        },
    ))
    .unwrap();
    // A plain CUT_OFF after it still merges into the V2 row (dropped).
    cut(&mut w, T0 + 102, Some(1), PLAIN_TEXT, 4);
    assert_eq!(
        violation_rows(&w, "bili.cut_off"),
        vec![
            V2_TEXT.to_string(),
            "存在挂机行为（切断本场直播）".to_string()
        ]
    );
}

#[test]
fn l3_v2_reason_survives_a_restart_and_a_late_plain_cut_off_row() {
    let (mut w, path) = writer("l3-restart");
    session(&mut w, 1, T0, None);
    cut(&mut w, T0 + 100, Some(1), V2_TEXT, 7);
    end(&mut w, 1, T0 + 110, EndReason::BiliStopped);
    drop(w);
    // Restart: a fresh writer with empty in-memory state on the same file.
    let mut w = Writer::new(schema::open_rw(&path).unwrap(), "local", 8);
    cut(&mut w, T0 + 105, Some(1), PLAIN_TEXT, 4);
    assert_eq!(
        violation_rows(&w, "bili.cut_off"),
        vec![V2_TEXT.to_string(), PLAIN_TEXT.to_string()],
        "a duplicate plain row after a restart is accepted"
    );
    let item = only_session(&w, T0 + 200);
    assert_eq!(item["end_cut_off"], json!(true));
    assert_eq!(item["end_detail"], json!(V2_TEXT));
}

#[test]
fn ruling4_a_full_screen_mask_warning_never_sets_the_cut_off_fields() {
    // The mask as the danmaku client now records it: bili.warning, title only.
    let (mut w, _) = writer("r4-mask");
    session(&mut w, 1, T0, None);
    warning_own_row(&mut w, T0 + 100, "当前直播间涉嫌违规", 2);
    end(&mut w, 1, T0 + 105, EndReason::BiliStopped);
    assert_eq!(
        violation_rows(&w, "bili.warning.stop"),
        vec!["当前直播间涉嫌违规".to_string()]
    );
    assert!(violation_rows(&w, "bili.cut_off").is_empty());
    let item = only_session(&w, T0 + 200);
    assert!(item.get("end_cut_off").is_none(), "{item}");
    assert_eq!(item["end_warning_stop"], json!(true));
    assert_eq!(item["end_detail"], json!("当前直播间涉嫌违规"));
}

#[test]
fn warning_stop_without_a_real_reason_has_no_detail() {
    // A mask with no title is stored with the stock fallback; the flag is
    // still true, but the panel gets no stock text as the reason.
    let (mut w, _) = writer("r5-warn-stock");
    session(&mut w, 1, T0, None);
    warning(&mut w, T0 + 100, "直播间警告", 0);
    end(&mut w, 1, T0 + 105, EndReason::BiliStopped);
    let item = only_session(&w, T0 + 200);
    assert_eq!(item["end_warning_stop"], json!(true));
    assert!(item.get("end_detail").is_none(), "{item}");
}

#[test]
fn a_warning_stop_older_than_600_seconds_does_not_mark_the_session() {
    let (mut w, _) = writer("r5-warn-window");
    let window = query::WARNING_STOP_WINDOW_SECS;
    session(&mut w, 1, T0, None);
    warning(&mut w, T0 + 100, "图片内容不适宜，请立即调整", 4);
    // Exactly at the window edge still counts; one second earlier does not.
    end(&mut w, 1, T0 + 100 + window, EndReason::BiliStopped);
    let item = only_session(&w, T0 + 2000);
    assert_eq!(item["end_warning_stop"], json!(true), "{item}");

    let (mut w, _) = writer("r5-warn-too-old");
    session(&mut w, 1, T0, None);
    warning(&mut w, T0 + 100, "图片内容不适宜，请立即调整", 4);
    end(&mut w, 1, T0 + 100 + window + 1, EndReason::BiliStopped);
    let item = only_session(&w, T0 + 2000);
    assert!(item.get("end_warning_stop").is_none(), "{item}");
    assert!(item.get("end_detail").is_none(), "{item}");
}

#[test]
fn a_cut_wins_over_a_warning_stop() {
    let (mut w, _) = writer("r5-cut-wins");
    session(&mut w, 1, T0, None);
    warning(&mut w, T0 + 100, "图片内容不适宜，请立即调整", 4);
    cut(&mut w, T0 + 101, Some(1), V2_TEXT, 7);
    end(&mut w, 1, T0 + 105, EndReason::BiliStopped);
    let item = only_session(&w, T0 + 200);
    assert_eq!(item["end_cut_off"], json!(true));
    assert_eq!(item["end_detail"], json!(V2_TEXT));
    assert!(item.get("end_warning_stop").is_none(), "{item}");
}

#[test]
fn a_plain_warning_that_did_not_stop_never_marks_the_session() {
    // Older builds stored every warning as `bili.warning`; those rows read as
    // warnings that did not stop, so they never explain the 下播.
    let (mut w, _) = writer("r5-plain-warning");
    session(&mut w, 1, T0, None);
    w.apply(&ev(
        T0 + 100,
        EventKind::BiliWarning {
            seq: Some(1),
            room: 1,
            msg: "图片内容不适宜，请立即调整".into(),
            rank: 4,
            merge: true,
            stop: false,
        },
    ))
    .unwrap();
    end(&mut w, 1, T0 + 105, EndReason::BiliStopped);
    assert_eq!(
        violation_rows(&w, "bili.warning"),
        vec!["图片内容不适宜，请立即调整".to_string()]
    );
    let item = only_session(&w, T0 + 200);
    assert!(item.get("end_warning_stop").is_none(), "{item}");
    assert!(item.get("end_detail").is_none(), "{item}");
}

#[test]
fn ruling4_old_mask_cut_off_rows_are_read_as_stored() {
    // A mask row written before ruling 4 (bili.cut_off) is not rewritten.
    let (mut w, _) = writer("r4-old-mask");
    session(&mut w, 1, T0, None);
    cut(&mut w, T0 + 100, Some(1), "直播间涉嫌违规", 0);
    end(&mut w, 1, T0 + 105, EndReason::BiliStopped);
    let item = only_session(&w, T0 + 200);
    assert_eq!(item["end_cut_off"], json!(true));
    assert!(item.get("end_detail").is_none(), "{item}");
}

#[test]
fn ruling4_old_dialog_cut_off_rows_are_read_as_stored() {
    // A row written before ruling 4 (dialog as bili.cut_off) is not rewritten:
    // its session still reads as cut off, the title-only text as fallback.
    let (mut w, _) = writer("r4-old");
    session(&mut w, 1, T0, None);
    cut(&mut w, T0 + 100, Some(1), "直播间违规", 0);
    end(&mut w, 1, T0 + 105, EndReason::BiliStopped);
    let item = only_session(&w, T0 + 200);
    assert_eq!(item["end_cut_off"], json!(true));
    assert!(item.get("end_detail").is_none(), "{item}");
}

// ------------------------------------------- PRD ruling 3 (titles)

fn violation_titles(w: &Writer) -> Vec<(String, String)> {
    let mut stmt = w
        .conn
        .prepare("SELECT kind,title FROM events WHERE kind LIKE 'bili.%' ORDER BY id")
        .unwrap();
    stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}

#[test]
fn ruling3_new_rows_store_the_spaced_titles() {
    let (mut w, _) = writer("r3-titles");
    session(&mut w, 1, T0, None);
    warning(&mut w, T0 + 10, "图片内容不适宜，请立即调整", 4);
    cut(&mut w, T0 + 20, Some(1), V2_TEXT, 7);
    assert_eq!(
        violation_titles(&w),
        vec![
            ("bili.warning.stop".to_string(), "B站 房间警告".to_string()),
            ("bili.cut_off".to_string(), "B站 切断直播".to_string()),
        ]
    );
    // The half-width space is exact.
    assert!(violation_titles(&w)
        .iter()
        .all(|(_, t)| t.contains("B站 ") && !t.contains('\u{3000}') && !t.contains('\u{a0}')));
    let page = query::events(&w.conn, &q(T0 - 10, T0 + 100)).unwrap();
    let titles: Vec<&str> = page
        .items
        .iter()
        .filter(|e| e.kind.starts_with("bili."))
        .map(|e| e.title.as_str())
        .collect();
    assert_eq!(titles.len(), 2);
    assert!(titles.contains(&"B站 房间警告") && titles.contains(&"B站 切断直播"));
}

#[test]
fn ruling3_old_title_rows_still_work_because_kinds_decide() {
    let (mut w, _) = writer("r3-old-titles");
    session(&mut w, 1, T0, None);
    end(&mut w, 1, T0 + 105, EndReason::BiliStopped);
    // Rows as an older build wrote them; never rewritten.
    for (kind, title, detail, at) in [
        (
            "bili.warning",
            "B站直播警告",
            "图片内容不适宜，请立即调整",
            T0 + 50,
        ),
        ("bili.cut_off", "B站切断推流", PLAIN_TEXT, T0 + 100),
    ] {
        w.conn
            .execute(
                "INSERT INTO events(eid,at,node,kind,subject,severity,title,detail,session_id)
                 VALUES (?1,?2,'local',?3,'1','warn',?4,?5,NULL)",
                rusqlite::params![format!("old:{kind}"), at, kind, title, detail],
            )
            .unwrap();
    }
    let item = only_session(&w, T0 + 200);
    assert_eq!(item["end_cut_off"], json!(true));
    assert_eq!(item["end_detail"], json!(PLAIN_TEXT));
    let page = query::events(&w.conn, &q(T0 - 10, T0 + 300)).unwrap();
    let old: Vec<(&str, &str)> = page
        .items
        .iter()
        .filter(|e| e.kind.starts_with("bili."))
        .map(|e| (e.kind.as_str(), e.title.as_str()))
        .collect();
    assert_eq!(
        old,
        vec![
            ("bili.cut_off", "B站切断推流"),
            ("bili.warning", "B站直播警告")
        ],
        "newest first, titles as stored"
    );
    let ended = page
        .items
        .iter()
        .find(|e| e.kind == "stream.ended")
        .unwrap();
    assert!(ended.detail.ends_with("· B站 切断直播"), "{}", ended.detail);
    // A row titled like a cut but of kind bili.warning never marks a cut.
    let (mut w, _) = writer("r3-title-spoof");
    session(&mut w, 1, T0, None);
    end(&mut w, 1, T0 + 105, EndReason::BiliStopped);
    w.conn
        .execute(
            "INSERT INTO events(eid,at,node,kind,subject,severity,title,detail,session_id)
             VALUES ('x',?1,'local','bili.warning','1','warn','B站 切断直播','x',NULL)",
            [T0 + 100],
        )
        .unwrap();
    let item = only_session(&w, T0 + 200);
    assert!(item.get("end_cut_off").is_none(), "{item}");
}

#[test]
fn a_start_restricted_note_has_its_own_kind_and_title() {
    let (mut w, _) = writer("start-restricted");
    w.apply(&ev(
        T0,
        EventKind::BiliStartRestricted {
            detail: "限制开播 60031".into(),
        },
    ))
    .unwrap();
    let page = query::events(&w.conn, &q(T0 - 10, T0 + 10)).unwrap();
    let event = page
        .items
        .iter()
        .find(|event| event.kind == "bili.start_restricted")
        .unwrap();
    assert_eq!(event.title, "B站 限制开播，已关闭监控");
    assert_eq!(event.detail, "限制开播 60031");
    assert!(
        !page
            .items
            .iter()
            .any(|event| event.kind.starts_with("bili.warning")),
        "the restricted note never counts as a warning/cut"
    );
}
