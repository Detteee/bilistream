//! Versioned history schema. Separate file, never
//! through `Store`, never backed up or synced.

use rusqlite::{Connection, OpenFlags};
use std::path::{Path, PathBuf};

pub(crate) const SCHEMA_VERSION: i64 = 8;

/// Older versions this build opens: 28a20792's v6 and the shipped v7. Both
/// upgrade by adding the v8 prune index and setting `user_version`; no row is
/// rewritten, so an interrupted upgrade cannot lose history.
const UPGRADABLE_VERSION: i64 = 6;

/// The v8 schema: the `sqlite_master` SQL of a v6 database built by
/// 28a20792's V1..V6 chain, verbatim and in creation order. Statements
/// rewritten by `ALTER TABLE` keep the text SQLite stored for them (the added
/// columns appended in place, the renamed `"bili_lives"` quoted). The guard
/// test compares a fresh file with `fixtures/history-v6-28a20792.sqlite`.
///
/// `samples_raw` is a writable view over `sample_frames`: one frame per
/// timestamp/session instead of one row per metric. Equal extrema are stored
/// as NULL and reconstructed exactly from sum/count. Sample `session_id` 0
/// means unknown, never a real `sessions.id`; `legacy_unattributed_before`
/// marks where such rows (kept from pre-v2 data) end and is still read.
///
/// `bili_lives_room_started` is v8: the prune asks, per row, whether a newer
/// live started in the same room, and without it that question is a full scan
/// of `bili_lives` per candidate row.
const V8_BASELINE: &str = r#"
CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL) WITHOUT ROWID;
CREATE TABLE boots (
  boot_id INTEGER PRIMARY KEY, node TEXT NOT NULL, started_at INTEGER NOT NULL,
  last_seen_at INTEGER NOT NULL, version TEXT NOT NULL);
CREATE TABLE sessions (
  id INTEGER PRIMARY KEY, eid TEXT NOT NULL UNIQUE,
  node TEXT NOT NULL, boot_id INTEGER NOT NULL, seq INTEGER NOT NULL,
  platform TEXT NOT NULL CHECK (platform IN ('YT','TW','NC')),
  channel_id TEXT NOT NULL, channel_name TEXT NOT NULL, title TEXT,
  started_at INTEGER NOT NULL, last_seen_at INTEGER NOT NULL, ended_at INTEGER,
  end_reason TEXT CHECK (end_reason IS NULL OR end_reason IN
    ('source_ended','bili_stopped','user_stop','intentional_restart','cluster_handoff','process_restart','aborted')),
  runs INTEGER NOT NULL DEFAULT 0, failures INTEGER NOT NULL DEFAULT 0, stuck INTEGER NOT NULL DEFAULT 0,
  live_key TEXT, area_v2 INTEGER, room INTEGER, pruned INTEGER NOT NULL DEFAULT 0, watched_count INTEGER, watched_at INTEGER, source_id TEXT,
  UNIQUE (node, boot_id, seq));
CREATE INDEX sessions_node_time ON sessions (node, started_at);
CREATE INDEX sessions_open ON sessions (node) WHERE ended_at IS NULL;
CREATE TABLE runs (
  id INTEGER PRIMARY KEY, eid TEXT NOT NULL UNIQUE,
  session_id INTEGER NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
  run INTEGER NOT NULL, started_at INTEGER NOT NULL, last_seen_at INTEGER NOT NULL, ended_at INTEGER,
  exit_code INTEGER, success INTEGER, counted_failure INTEGER, diagnostic TEXT, stuck_reason TEXT);
CREATE INDEX runs_session ON runs (session_id, started_at);
CREATE TABLE node_segments (
  id INTEGER PRIMARY KEY, eid TEXT NOT NULL UNIQUE, observer TEXT NOT NULL, subject TEXT NOT NULL,
  state TEXT NOT NULL, reason TEXT NOT NULL, started_at INTEGER NOT NULL,
  last_seen_at INTEGER NOT NULL, ended_at INTEGER, initial INTEGER NOT NULL DEFAULT 0, minority INTEGER NOT NULL DEFAULT 0);
CREATE TABLE events (
  id INTEGER PRIMARY KEY, eid TEXT NOT NULL UNIQUE, at INTEGER NOT NULL, node TEXT NOT NULL,
  kind TEXT NOT NULL, subject TEXT, severity TEXT NOT NULL CHECK (severity IN ('info','warn','error')),
  title TEXT NOT NULL, detail TEXT NOT NULL, session_id INTEGER);
CREATE INDEX events_time ON events (at);
CREATE TABLE notify_sends (
  id INTEGER PRIMARY KEY, eid TEXT NOT NULL UNIQUE, at INTEGER NOT NULL, node TEXT NOT NULL,
  event_key TEXT NOT NULL, phase TEXT NOT NULL, ok INTEGER NOT NULL, code INTEGER,
  suppressed INTEGER NOT NULL, undelivered INTEGER NOT NULL, dedup_key TEXT NOT NULL DEFAULT '', mode TEXT, trace_id TEXT, test INTEGER NOT NULL DEFAULT 0);
CREATE INDEX notify_sends_time ON notify_sends (at);
CREATE INDEX node_segments_subject_time ON node_segments(subject,started_at);
CREATE INDEX node_segments_open ON node_segments(observer,subject) WHERE ended_at IS NULL;
CREATE INDEX sessions_end ON sessions(ended_at) WHERE ended_at IS NOT NULL;
CREATE INDEX runs_end ON runs(ended_at) WHERE ended_at IS NOT NULL;
CREATE INDEX events_session_time ON events(session_id,at);
CREATE TABLE samples_1m (
  metric INTEGER NOT NULL, node TEXT NOT NULL, bucket INTEGER NOT NULL,
  session_id INTEGER NOT NULL DEFAULT 0,
  min REAL NOT NULL, max REAL NOT NULL, sum REAL NOT NULL, count INTEGER NOT NULL,
  PRIMARY KEY(bucket,metric,node,session_id)) WITHOUT ROWID;
CREATE TABLE sample_frames(ts INTEGER NOT NULL,node TEXT NOT NULL,session_id INTEGER NOT NULL DEFAULT 0,
         b_min REAL,b_max REAL,b_sum REAL,b_count INTEGER,f_min REAL,f_max REAL,f_sum REAL,f_count INTEGER,s_min REAL,s_max REAL,s_sum REAL,s_count INTEGER,o_min REAL,o_max REAL,o_sum REAL,o_count INTEGER,w_min REAL,w_max REAL,w_sum REAL,w_count INTEGER,PRIMARY KEY(ts,node,session_id)) WITHOUT ROWID;
CREATE VIEW samples_raw AS SELECT 1 AS metric,node,ts,session_id,coalesce(b_min,b_sum/b_count) AS min,
         coalesce(b_max,b_sum/b_count) AS max,b_sum AS sum,b_count AS count
         FROM sample_frames WHERE b_count IS NOT NULL UNION ALL SELECT 2 AS metric,node,ts,session_id,coalesce(f_min,f_sum/f_count) AS min,
         coalesce(f_max,f_sum/f_count) AS max,f_sum AS sum,f_count AS count
         FROM sample_frames WHERE f_count IS NOT NULL UNION ALL SELECT 3 AS metric,node,ts,session_id,coalesce(s_min,s_sum/s_count) AS min,
         coalesce(s_max,s_sum/s_count) AS max,s_sum AS sum,s_count AS count
         FROM sample_frames WHERE s_count IS NOT NULL UNION ALL SELECT 10 AS metric,node,ts,session_id,coalesce(o_min,o_sum/o_count) AS min,
         coalesce(o_max,o_sum/o_count) AS max,o_sum AS sum,o_count AS count
         FROM sample_frames WHERE o_count IS NOT NULL UNION ALL SELECT 11 AS metric,node,ts,session_id,coalesce(w_min,w_sum/w_count) AS min,
         coalesce(w_max,w_sum/w_count) AS max,w_sum AS sum,w_count AS count
         FROM sample_frames WHERE w_count IS NOT NULL;
CREATE TRIGGER samples_raw_insert_b INSTEAD OF INSERT ON samples_raw WHEN NEW.metric=1
             BEGIN
             INSERT INTO sample_frames(ts,node,session_id,b_min,b_max,b_sum,b_count)
             VALUES(NEW.ts,NEW.node,coalesce(NEW.session_id,0),nullif(NEW.min,1.0*NEW.sum/NEW.count),nullif(NEW.max,1.0*NEW.sum/NEW.count),NEW.sum,NEW.count)
             ON CONFLICT(ts,node,session_id) DO UPDATE SET
             b_min=nullif(coalesce(min(coalesce(b_min,b_sum/b_count),NEW.min),NEW.min),(coalesce(b_sum,0)+NEW.sum)/(coalesce(b_count,0)+NEW.count)),
             b_max=nullif(coalesce(max(coalesce(b_max,b_sum/b_count),NEW.max),NEW.max),(coalesce(b_sum,0)+NEW.sum)/(coalesce(b_count,0)+NEW.count)),
             b_sum=coalesce(b_sum,0)+NEW.sum,b_count=coalesce(b_count,0)+NEW.count;
             END;
CREATE TRIGGER samples_raw_delete_b INSTEAD OF DELETE ON samples_raw WHEN OLD.metric=1
             BEGIN
             UPDATE sample_frames SET b_min=NULL,b_max=NULL,b_sum=NULL,b_count=NULL
             WHERE ts=OLD.ts AND node=OLD.node AND session_id=OLD.session_id;
             DELETE FROM sample_frames WHERE ts=OLD.ts AND node=OLD.node AND session_id=OLD.session_id
             AND b_count IS NULL AND f_count IS NULL AND s_count IS NULL AND o_count IS NULL AND w_count IS NULL;
             END;
CREATE TRIGGER samples_raw_insert_f INSTEAD OF INSERT ON samples_raw WHEN NEW.metric=2
             BEGIN
             INSERT INTO sample_frames(ts,node,session_id,f_min,f_max,f_sum,f_count)
             VALUES(NEW.ts,NEW.node,coalesce(NEW.session_id,0),nullif(NEW.min,1.0*NEW.sum/NEW.count),nullif(NEW.max,1.0*NEW.sum/NEW.count),NEW.sum,NEW.count)
             ON CONFLICT(ts,node,session_id) DO UPDATE SET
             f_min=nullif(coalesce(min(coalesce(f_min,f_sum/f_count),NEW.min),NEW.min),(coalesce(f_sum,0)+NEW.sum)/(coalesce(f_count,0)+NEW.count)),
             f_max=nullif(coalesce(max(coalesce(f_max,f_sum/f_count),NEW.max),NEW.max),(coalesce(f_sum,0)+NEW.sum)/(coalesce(f_count,0)+NEW.count)),
             f_sum=coalesce(f_sum,0)+NEW.sum,f_count=coalesce(f_count,0)+NEW.count;
             END;
CREATE TRIGGER samples_raw_delete_f INSTEAD OF DELETE ON samples_raw WHEN OLD.metric=2
             BEGIN
             UPDATE sample_frames SET f_min=NULL,f_max=NULL,f_sum=NULL,f_count=NULL
             WHERE ts=OLD.ts AND node=OLD.node AND session_id=OLD.session_id;
             DELETE FROM sample_frames WHERE ts=OLD.ts AND node=OLD.node AND session_id=OLD.session_id
             AND b_count IS NULL AND f_count IS NULL AND s_count IS NULL AND o_count IS NULL AND w_count IS NULL;
             END;
CREATE TRIGGER samples_raw_insert_s INSTEAD OF INSERT ON samples_raw WHEN NEW.metric=3
             BEGIN
             INSERT INTO sample_frames(ts,node,session_id,s_min,s_max,s_sum,s_count)
             VALUES(NEW.ts,NEW.node,coalesce(NEW.session_id,0),nullif(NEW.min,1.0*NEW.sum/NEW.count),nullif(NEW.max,1.0*NEW.sum/NEW.count),NEW.sum,NEW.count)
             ON CONFLICT(ts,node,session_id) DO UPDATE SET
             s_min=nullif(coalesce(min(coalesce(s_min,s_sum/s_count),NEW.min),NEW.min),(coalesce(s_sum,0)+NEW.sum)/(coalesce(s_count,0)+NEW.count)),
             s_max=nullif(coalesce(max(coalesce(s_max,s_sum/s_count),NEW.max),NEW.max),(coalesce(s_sum,0)+NEW.sum)/(coalesce(s_count,0)+NEW.count)),
             s_sum=coalesce(s_sum,0)+NEW.sum,s_count=coalesce(s_count,0)+NEW.count;
             END;
CREATE TRIGGER samples_raw_delete_s INSTEAD OF DELETE ON samples_raw WHEN OLD.metric=3
             BEGIN
             UPDATE sample_frames SET s_min=NULL,s_max=NULL,s_sum=NULL,s_count=NULL
             WHERE ts=OLD.ts AND node=OLD.node AND session_id=OLD.session_id;
             DELETE FROM sample_frames WHERE ts=OLD.ts AND node=OLD.node AND session_id=OLD.session_id
             AND b_count IS NULL AND f_count IS NULL AND s_count IS NULL AND o_count IS NULL AND w_count IS NULL;
             END;
CREATE TRIGGER samples_raw_insert_o INSTEAD OF INSERT ON samples_raw WHEN NEW.metric=10
             BEGIN
             INSERT INTO sample_frames(ts,node,session_id,o_min,o_max,o_sum,o_count)
             VALUES(NEW.ts,NEW.node,coalesce(NEW.session_id,0),nullif(NEW.min,1.0*NEW.sum/NEW.count),nullif(NEW.max,1.0*NEW.sum/NEW.count),NEW.sum,NEW.count)
             ON CONFLICT(ts,node,session_id) DO UPDATE SET
             o_min=nullif(coalesce(min(coalesce(o_min,o_sum/o_count),NEW.min),NEW.min),(coalesce(o_sum,0)+NEW.sum)/(coalesce(o_count,0)+NEW.count)),
             o_max=nullif(coalesce(max(coalesce(o_max,o_sum/o_count),NEW.max),NEW.max),(coalesce(o_sum,0)+NEW.sum)/(coalesce(o_count,0)+NEW.count)),
             o_sum=coalesce(o_sum,0)+NEW.sum,o_count=coalesce(o_count,0)+NEW.count;
             END;
CREATE TRIGGER samples_raw_delete_o INSTEAD OF DELETE ON samples_raw WHEN OLD.metric=10
             BEGIN
             UPDATE sample_frames SET o_min=NULL,o_max=NULL,o_sum=NULL,o_count=NULL
             WHERE ts=OLD.ts AND node=OLD.node AND session_id=OLD.session_id;
             DELETE FROM sample_frames WHERE ts=OLD.ts AND node=OLD.node AND session_id=OLD.session_id
             AND b_count IS NULL AND f_count IS NULL AND s_count IS NULL AND o_count IS NULL AND w_count IS NULL;
             END;
CREATE TRIGGER samples_raw_insert_w INSTEAD OF INSERT ON samples_raw WHEN NEW.metric=11
             BEGIN
             INSERT INTO sample_frames(ts,node,session_id,w_min,w_max,w_sum,w_count)
             VALUES(NEW.ts,NEW.node,coalesce(NEW.session_id,0),nullif(NEW.min,1.0*NEW.sum/NEW.count),nullif(NEW.max,1.0*NEW.sum/NEW.count),NEW.sum,NEW.count)
             ON CONFLICT(ts,node,session_id) DO UPDATE SET
             w_min=nullif(coalesce(min(coalesce(w_min,w_sum/w_count),NEW.min),NEW.min),(coalesce(w_sum,0)+NEW.sum)/(coalesce(w_count,0)+NEW.count)),
             w_max=nullif(coalesce(max(coalesce(w_max,w_sum/w_count),NEW.max),NEW.max),(coalesce(w_sum,0)+NEW.sum)/(coalesce(w_count,0)+NEW.count)),
             w_sum=coalesce(w_sum,0)+NEW.sum,w_count=coalesce(w_count,0)+NEW.count;
             END;
CREATE TRIGGER samples_raw_delete_w INSTEAD OF DELETE ON samples_raw WHEN OLD.metric=11
             BEGIN
             UPDATE sample_frames SET w_min=NULL,w_max=NULL,w_sum=NULL,w_count=NULL
             WHERE ts=OLD.ts AND node=OLD.node AND session_id=OLD.session_id;
             DELETE FROM sample_frames WHERE ts=OLD.ts AND node=OLD.node AND session_id=OLD.session_id
             AND b_count IS NULL AND f_count IS NULL AND s_count IS NULL AND o_count IS NULL AND w_count IS NULL;
             END;
CREATE INDEX sessions_start ON sessions(started_at);
CREATE INDEX sample_frames_session ON sample_frames(session_id, ts);
CREATE INDEX node_segments_start ON node_segments(started_at);
CREATE TABLE "bili_lives" (
  live_key TEXT PRIMARY KEY, node TEXT NOT NULL, room INTEGER NOT NULL,
  started_at INTEGER NOT NULL, ended_at INTEGER,
  started_by TEXT NOT NULL DEFAULT '',
  summary_phase TEXT NOT NULL DEFAULT 'pending'
    CHECK (summary_phase IN ('pending','claimed','failed','done','skipped')),
  summary_node TEXT, summary_at INTEGER NOT NULL DEFAULT 0,
  summary_generation INTEGER NOT NULL DEFAULT 0,
  live_time_secs INTEGER, add_fans INTEGER, danmu_num INTEGER,
  watched_count INTEGER, new_fans_club INTEGER, max_online INTEGER) WITHOUT ROWID;
CREATE INDEX bili_lives_end ON bili_lives(ended_at);
CREATE INDEX bili_lives_room_started ON bili_lives(room, started_at);
INSERT INTO meta(key,value) VALUES ('legacy_unattributed_before','0'),('session_id_highwater','0');
"#;

fn pragmas(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL; PRAGMA busy_timeout=2000;
         PRAGMA foreign_keys=ON; PRAGMA temp_store=MEMORY; PRAGMA cache_size=-2000;",
    )
}

/// Opens (creating if needed) the read-write writer connection.
/// `auto_vacuum=INCREMENTAL` is set on a brand-new file before any table.
pub(crate) fn open_rw(path: &Path) -> rusqlite::Result<Connection> {
    match open_rw_once(path) {
        Ok(conn) => Ok(conn),
        Err(error) if is_corrupt(&error) => {
            let aside = corrupt_name(path);
            // A stale aside from an earlier corruption must not block this one.
            if let Err(error) = std::fs::remove_file(&aside) {
                if error.kind() != std::io::ErrorKind::NotFound {
                    return Err(cannot_keep(&error));
                }
            }
            std::fs::rename(path, &aside).map_err(|error| cannot_keep(&error))?;
            // The `-wal` may hold the newest rows of a database whose main file
            // is damaged, so it is kept beside the aside rather than deleted;
            // `-shm` goes with it because SQLite rebuilds it from the WAL.
            for ext in ["-wal", "-shm"] {
                let from = with_suffix(path, ext);
                if !from.exists() {
                    continue;
                }
                let to = with_suffix(&aside, ext);
                let _ = std::fs::remove_file(&to);
                if let Err(error) = std::fs::rename(&from, &to) {
                    tracing::warn!(
                        "统计数据库旁路文件无法保留：{}",
                        crate::logging::safe_diagnostic(&error.to_string())
                    );
                }
            }
            tracing::warn!(
                "统计数据库损坏，已改名保留为 {} 并重新创建",
                aside.display()
            );
            open_rw_once(path)
        }
        Err(error) => Err(error),
    }
}

fn corrupt_name(path: &Path) -> PathBuf {
    path.with_extension("sqlite.corrupt")
}

/// `history.sqlite` + `-wal` → `history.sqlite-wal`, never `with_extension`.
fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    path.with_file_name(format!("{name}{suffix}"))
}

fn cannot_keep(error: &std::io::Error) -> rusqlite::Error {
    rusqlite::Error::SqliteFailure(
        rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CANTOPEN),
        Some(format!("无法保留损坏的统计数据库：{error}")),
    )
}

fn is_corrupt(error: &rusqlite::Error) -> bool {
    matches!(
        error.sqlite_error_code(),
        Some(rusqlite::ErrorCode::DatabaseCorrupt | rusqlite::ErrorCode::NotADatabase)
    )
}

/// A file this build will not touch. Not a corruption code, so `open_rw`
/// never renames it aside.
fn refuse(message: String) -> rusqlite::Error {
    rusqlite::Error::SqliteFailure(
        rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_READONLY),
        Some(message),
    )
}

/// What the checkpointed main file says, read with plain file I/O.
enum Probe {
    /// No file, a short file, or not a SQLite header: leave it to the
    /// normal open (which creates, or renames a corrupt file aside).
    Open,
    Refuse(rusqlite::Error),
}

/// Decides refusals from the main file's 100-byte header and the page-1
/// `sqlite_master` b-tree header, before SQLite opens anything. Any SQLite
/// connection, even a read-only one, would read a crash-left `-wal`, may
/// create or write `-shm`, and on close checkpoints the WAL into the main
/// file. Reading the bytes directly leaves the main file, `-wal` and `-shm`
/// exactly as they were. The header can lag frames still in the WAL;
/// refusing on the checkpointed version is the conservative choice, and
/// accepted files are re-checked through the WAL by the normal open below.
fn probe_header(path: &Path) -> Probe {
    use std::io::Read;
    let mut header = [0u8; 108];
    let read = std::fs::File::open(path).and_then(|mut f| f.read_exact(&mut header));
    if read.is_err() || &header[..16] != b"SQLite format 3\0" {
        return Probe::Open;
    }
    let version = i64::from(u32::from_be_bytes([
        header[60], header[61], header[62], header[63],
    ]));
    // Page 1 holds the root of `sqlite_master`: an empty schema is a leaf
    // table page (0x0D) with no cells.
    let has_schema = header[100] != 0x0D || u16::from_be_bytes([header[103], header[104]]) != 0;
    match refusal(version, has_schema) {
        Some(error) => Probe::Refuse(error),
        None => Probe::Open,
    }
}

fn refusal(version: i64, has_schema: bool) -> Option<rusqlite::Error> {
    match version {
        0 if has_schema => Some(refuse(
            "统计数据库没有版本号，无法识别；请删除该统计数据库后重新创建".into(),
        )),
        0 | UPGRADABLE_VERSION | 7 | SCHEMA_VERSION => None,
        v if v > SCHEMA_VERSION => Some(refuse("统计数据库版本较新".into())),
        v => Some(refuse(format!(
            "统计数据库版本过旧 (v{v})，本版本只能升级 v6 与 v7；请先用上一版本打开一次完成升级，或删除该统计数据库后重新创建"
        ))),
    }
}

fn open_rw_once(path: &Path) -> rusqlite::Result<Connection> {
    if let Probe::Refuse(error) = probe_header(path) {
        return Err(error);
    }
    let conn = Connection::open(path)?;
    let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    // Same rules again on the WAL-applied view (the header probe only saw
    // the checkpointed file).
    let has_schema = version == 0
        && conn.query_row("SELECT count(*) FROM sqlite_master", [], |r| {
            r.get::<_, i64>(0)
        })? > 0;
    if let Some(error) = refusal(version, has_schema) {
        return Err(error);
    }
    if version == 0 {
        conn.execute_batch("PRAGMA auto_vacuum=INCREMENTAL;")?;
    }
    pragmas(&conn)?;
    // Reading the header alone misses damaged table/index pages. Run this
    // only at writer startup, off every producer and request path.
    let integrity: String = conn.query_row("PRAGMA quick_check(1)", [], |r| r.get(0))?;
    if integrity != "ok" {
        return Err(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CORRUPT),
            Some("统计数据库完整性检查失败".into()),
        ));
    }
    let mode: i64 = conn.query_row("PRAGMA auto_vacuum", [], |r| r.get(0))?;
    if mode != 2 {
        tracing::warn!("统计数据库未使用增量清理模式 (auto_vacuum={mode})");
    }
    if version < SCHEMA_VERSION {
        let tx = conn.unchecked_transaction()?;
        if version == 0 {
            tx.execute_batch(V8_BASELINE)?;
        } else if version < 8 {
            tx.execute_batch(
                "CREATE INDEX IF NOT EXISTS bili_lives_room_started \
                 ON bili_lives(room, started_at);",
            )?;
        }
        tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        tx.commit()?;
    }
    Ok(conn)
}

/// Read-only connection for one API query.
pub(crate) fn open_ro(path: &Path) -> rusqlite::Result<Connection> {
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    conn.execute_batch("PRAGMA busy_timeout=2000; PRAGMA cache_size=-2000;")?;
    Ok(conn)
}
