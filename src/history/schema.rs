//! Versioned history schema. Separate file, never
//! through `Store`, never backed up or synced.

use rusqlite::{Connection, OpenFlags};
use std::path::{Path, PathBuf};

pub(crate) const SCHEMA_VERSION: i64 = 3;

pub(crate) const V1: &str = r#"
CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS boots (
  boot_id INTEGER PRIMARY KEY, node TEXT NOT NULL, started_at INTEGER NOT NULL,
  last_seen_at INTEGER NOT NULL, version TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS sessions (
  id INTEGER PRIMARY KEY, eid TEXT NOT NULL UNIQUE,
  node TEXT NOT NULL, boot_id INTEGER NOT NULL, seq INTEGER NOT NULL,
  platform TEXT NOT NULL CHECK (platform IN ('YT','TW','NC')),
  channel_id TEXT NOT NULL, channel_name TEXT NOT NULL, title TEXT,
  started_at INTEGER NOT NULL, last_seen_at INTEGER NOT NULL, ended_at INTEGER,
  end_reason TEXT CHECK (end_reason IS NULL OR end_reason IN
    ('source_ended','bili_stopped','user_stop','intentional_restart','cluster_handoff','process_restart','aborted')),
  runs INTEGER NOT NULL DEFAULT 0, failures INTEGER NOT NULL DEFAULT 0, stuck INTEGER NOT NULL DEFAULT 0,
  live_key TEXT,
  UNIQUE (node, boot_id, seq));
CREATE INDEX IF NOT EXISTS sessions_node_time ON sessions (node, started_at);
CREATE INDEX IF NOT EXISTS sessions_open ON sessions (node) WHERE ended_at IS NULL;
CREATE TABLE IF NOT EXISTS runs (
  id INTEGER PRIMARY KEY, eid TEXT NOT NULL UNIQUE,
  session_id INTEGER NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
  run INTEGER NOT NULL, started_at INTEGER NOT NULL, last_seen_at INTEGER NOT NULL, ended_at INTEGER,
  exit_code INTEGER, success INTEGER, counted_failure INTEGER, diagnostic TEXT, stuck_reason TEXT);
CREATE INDEX IF NOT EXISTS runs_session ON runs (session_id, started_at);
-- No revenue column, by user decision. LiveTime is kept raw only.
CREATE TABLE IF NOT EXISTS bili_lives (
  live_key TEXT PRIMARY KEY, node TEXT NOT NULL, room INTEGER NOT NULL,
  started_at INTEGER NOT NULL, ended_at INTEGER,
  summary_state TEXT NOT NULL DEFAULT 'pending' CHECK (summary_state IN ('pending','ok','failed','skipped')),
  bili_live_time_raw INTEGER, add_fans INTEGER, danmu_num INTEGER, watched_count INTEGER,
  new_fans_club INTEGER, max_online INTEGER) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS samples_raw (
  metric INTEGER NOT NULL, node TEXT NOT NULL, ts INTEGER NOT NULL, session_id INTEGER,
  min REAL NOT NULL, max REAL NOT NULL, sum REAL NOT NULL, count INTEGER NOT NULL,
  PRIMARY KEY (metric, node, ts)) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS samples_1m (
  metric INTEGER NOT NULL, node TEXT NOT NULL, bucket INTEGER NOT NULL, session_id INTEGER,
  min REAL NOT NULL, max REAL NOT NULL, sum REAL NOT NULL, count INTEGER NOT NULL,
  PRIMARY KEY (metric, node, bucket)) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS samples_1h (
  metric INTEGER NOT NULL, node TEXT NOT NULL, bucket INTEGER NOT NULL, session_id INTEGER,
  min REAL NOT NULL, max REAL NOT NULL, sum REAL NOT NULL, count INTEGER NOT NULL,
  PRIMARY KEY (metric, node, bucket)) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS node_segments (
  id INTEGER PRIMARY KEY, eid TEXT NOT NULL UNIQUE, observer TEXT NOT NULL, subject TEXT NOT NULL,
  state TEXT NOT NULL, reason TEXT NOT NULL, started_at INTEGER NOT NULL,
  last_seen_at INTEGER NOT NULL, ended_at INTEGER);
CREATE TABLE IF NOT EXISTS events (
  id INTEGER PRIMARY KEY, eid TEXT NOT NULL UNIQUE, at INTEGER NOT NULL, node TEXT NOT NULL,
  kind TEXT NOT NULL, subject TEXT, severity TEXT NOT NULL CHECK (severity IN ('info','warn','error')),
  title TEXT NOT NULL, detail TEXT NOT NULL, session_id INTEGER);
CREATE INDEX IF NOT EXISTS events_time ON events (at);
CREATE TABLE IF NOT EXISTS notify_sends (
  id INTEGER PRIMARY KEY, eid TEXT NOT NULL UNIQUE, at INTEGER NOT NULL, node TEXT NOT NULL,
  event_key TEXT NOT NULL, phase TEXT NOT NULL, ok INTEGER NOT NULL, code INTEGER,
  suppressed INTEGER NOT NULL, undelivered INTEGER NOT NULL);
CREATE INDEX IF NOT EXISTS notify_sends_time ON notify_sends (at);
"#;

// Samples are time-first for bounded range maintenance. Session 0 means
// unknown, never a real sessions.id; it also preserves old mixed aggregates
// without assigning their values to whichever session happened to be last.
const V2: &str = r#"
ALTER TABLE sessions ADD COLUMN area_v2 INTEGER;
ALTER TABLE sessions ADD COLUMN room INTEGER;
ALTER TABLE sessions ADD COLUMN pruned INTEGER NOT NULL DEFAULT 0;
ALTER TABLE node_segments ADD COLUMN initial INTEGER NOT NULL DEFAULT 0;
ALTER TABLE node_segments ADD COLUMN minority INTEGER NOT NULL DEFAULT 0;
CREATE INDEX node_segments_subject_time ON node_segments(subject,started_at);
CREATE INDEX node_segments_open ON node_segments(observer,subject) WHERE ended_at IS NULL;
CREATE INDEX sessions_end ON sessions(ended_at) WHERE ended_at IS NOT NULL;
CREATE INDEX runs_end ON runs(ended_at) WHERE ended_at IS NOT NULL;
CREATE INDEX events_session_time ON events(session_id,at);
ALTER TABLE notify_sends ADD COLUMN dedup_key TEXT NOT NULL DEFAULT '';
ALTER TABLE notify_sends ADD COLUMN mode TEXT;
ALTER TABLE notify_sends ADD COLUMN trace_id TEXT;
ALTER TABLE notify_sends ADD COLUMN test INTEGER NOT NULL DEFAULT 0;
ALTER TABLE bili_lives ADD COLUMN started_by TEXT NOT NULL DEFAULT '';
ALTER TABLE bili_lives ADD COLUMN summary_phase TEXT NOT NULL DEFAULT 'pending';
ALTER TABLE bili_lives ADD COLUMN summary_node TEXT;
ALTER TABLE bili_lives ADD COLUMN summary_at INTEGER NOT NULL DEFAULT 0;
ALTER TABLE bili_lives ADD COLUMN summary_generation INTEGER NOT NULL DEFAULT 0;
UPDATE bili_lives SET started_by=node,
 summary_phase=CASE summary_state WHEN 'ok' THEN 'done' ELSE summary_state END;
CREATE INDEX bili_lives_end ON bili_lives(ended_at);

ALTER TABLE samples_raw RENAME TO samples_raw_v1;
ALTER TABLE samples_1m RENAME TO samples_1m_v1;
ALTER TABLE samples_1h RENAME TO samples_1h_v1;
CREATE TABLE samples_raw (
  metric INTEGER NOT NULL, node TEXT NOT NULL, ts INTEGER NOT NULL,
  session_id INTEGER NOT NULL DEFAULT 0,
  min REAL NOT NULL, max REAL NOT NULL, sum REAL NOT NULL, count INTEGER NOT NULL,
  PRIMARY KEY(ts,metric,node,session_id)) WITHOUT ROWID;
CREATE TABLE samples_1m (
  metric INTEGER NOT NULL, node TEXT NOT NULL, bucket INTEGER NOT NULL,
  session_id INTEGER NOT NULL DEFAULT 0,
  min REAL NOT NULL, max REAL NOT NULL, sum REAL NOT NULL, count INTEGER NOT NULL,
  PRIMARY KEY(bucket,metric,node,session_id)) WITHOUT ROWID;
CREATE TABLE samples_1h (
  metric INTEGER NOT NULL, node TEXT NOT NULL, bucket INTEGER NOT NULL,
  session_id INTEGER NOT NULL DEFAULT 0,
  min REAL NOT NULL, max REAL NOT NULL, sum REAL NOT NULL, count INTEGER NOT NULL,
  PRIMARY KEY(bucket,metric,node,session_id)) WITHOUT ROWID;
INSERT INTO samples_raw
 SELECT metric,node,ts,CASE WHEN count>1 AND EXISTS(
   SELECT 1 FROM sessions s WHERE s.node=r.node AND s.id<>r.session_id
    AND s.started_at<=r.ts AND coalesce(s.ended_at,r.ts)>=r.ts
 ) THEN 0 ELSE coalesce(session_id,0) END,min,max,sum,count FROM samples_raw_v1 r;
INSERT INTO samples_1m SELECT metric,node,bucket,0,min,max,sum,count FROM samples_1m_v1;
INSERT INTO samples_1h SELECT metric,node,bucket,0,min,max,sum,count FROM samples_1h_v1;

-- Rebuild only complete recoverable aggregates. A partially pruned minute
-- or a v1 sum that cannot be reconstructed remains a session-unknown row.
CREATE TEMP TABLE reconstructable AS
 SELECT r.metric,r.node,r.ts-r.ts%60 AS bucket
 FROM samples_raw r JOIN samples_1m_v1 m
  ON m.metric=r.metric AND m.node=r.node AND m.bucket=r.ts-r.ts%60
 GROUP BY r.metric,r.node,r.ts-r.ts%60
 HAVING sum(r.count)=m.count
  AND abs(sum(r.sum)-m.sum)<=0.000001*max(1,abs(m.sum))
  AND min(r.min)=m.min AND max(r.max)=m.max;
DELETE FROM samples_1m WHERE (metric,node,bucket) IN (SELECT metric,node,bucket FROM reconstructable);
INSERT INTO samples_1m
 SELECT r.metric,r.node,r.ts-r.ts%60,r.session_id,min(r.min),max(r.max),sum(r.sum),sum(r.count)
 FROM samples_raw r JOIN reconstructable c
  ON c.metric=r.metric AND c.node=r.node AND c.bucket=r.ts-r.ts%60
 GROUP BY r.metric,r.node,r.ts-r.ts%60,r.session_id;
INSERT OR REPLACE INTO meta(key,value)
 SELECT 'legacy_unattributed_before',cast(coalesce(max(bucket)+60,0) AS TEXT)
 FROM samples_1m WHERE session_id=0;
DROP TABLE reconstructable;
DROP TABLE samples_raw_v1;
DROP TABLE samples_1m_v1;
DROP TABLE samples_1h_v1;
INSERT OR REPLACE INTO meta(key,value)
 SELECT 'session_id_highwater',cast(coalesce(max(id),0) AS TEXT) FROM sessions;
"#;

/// Store one timestamp/session frame instead of repeating its identity for
/// five metrics. Equal extrema are represented by NULL and reconstructed
/// exactly from sum/count; unusual extrema and fractional values stay intact.
/// The writable view preserves the existing metric-row query/test contract.
fn compact_raw(conn: &Connection) -> rusqlite::Result<()> {
    const METRICS: [(i64, &str); 5] = [(1, "b"), (2, "f"), (3, "s"), (10, "o"), (11, "w")];
    let columns = METRICS
        .iter()
        .map(|(_, p)| format!("{p}_min REAL,{p}_max REAL,{p}_sum REAL,{p}_count INTEGER"))
        .collect::<Vec<_>>()
        .join(",");
    conn.execute_batch(&format!(
        "ALTER TABLE samples_raw RENAME TO samples_raw_v2;
         CREATE TABLE sample_frames(ts INTEGER NOT NULL,node TEXT NOT NULL,session_id INTEGER NOT NULL DEFAULT 0,
         {columns},PRIMARY KEY(ts,node,session_id)) WITHOUT ROWID;"
    ))?;
    let projections = METRICS
        .iter()
        .map(|(metric, p)| {
            format!(
        "SELECT {metric} AS metric,node,ts,session_id,coalesce({p}_min,{p}_sum/{p}_count) AS min,
         coalesce({p}_max,{p}_sum/{p}_count) AS max,{p}_sum AS sum,{p}_count AS count
         FROM sample_frames WHERE {p}_count IS NOT NULL"
    )
        })
        .collect::<Vec<_>>()
        .join(" UNION ALL ");
    conn.execute_batch(&format!("CREATE VIEW samples_raw AS {projections}"))?;
    for (metric, p) in METRICS {
        conn.execute_batch(&format!(
            "CREATE TRIGGER samples_raw_insert_{p} INSTEAD OF INSERT ON samples_raw WHEN NEW.metric={metric}
             BEGIN
             INSERT INTO sample_frames(ts,node,session_id,{p}_min,{p}_max,{p}_sum,{p}_count)
             VALUES(NEW.ts,NEW.node,coalesce(NEW.session_id,0),nullif(NEW.min,1.0*NEW.sum/NEW.count),nullif(NEW.max,1.0*NEW.sum/NEW.count),NEW.sum,NEW.count)
             ON CONFLICT(ts,node,session_id) DO UPDATE SET
             {p}_min=nullif(coalesce(min(coalesce({p}_min,{p}_sum/{p}_count),NEW.min),NEW.min),(coalesce({p}_sum,0)+NEW.sum)/(coalesce({p}_count,0)+NEW.count)),
             {p}_max=nullif(coalesce(max(coalesce({p}_max,{p}_sum/{p}_count),NEW.max),NEW.max),(coalesce({p}_sum,0)+NEW.sum)/(coalesce({p}_count,0)+NEW.count)),
             {p}_sum=coalesce({p}_sum,0)+NEW.sum,{p}_count=coalesce({p}_count,0)+NEW.count;
             END;
             CREATE TRIGGER samples_raw_delete_{p} INSTEAD OF DELETE ON samples_raw WHEN OLD.metric={metric}
             BEGIN
             UPDATE sample_frames SET {p}_min=NULL,{p}_max=NULL,{p}_sum=NULL,{p}_count=NULL
             WHERE ts=OLD.ts AND node=OLD.node AND session_id=OLD.session_id;
             DELETE FROM sample_frames WHERE ts=OLD.ts AND node=OLD.node AND session_id=OLD.session_id
             AND b_count IS NULL AND f_count IS NULL AND s_count IS NULL AND o_count IS NULL AND w_count IS NULL;
             END;"
        ))?;
    }
    conn.execute_batch(
        "INSERT INTO samples_raw SELECT * FROM samples_raw_v2; DROP TABLE samples_raw_v2;
         ALTER TABLE sessions ADD COLUMN watched_count INTEGER;
         ALTER TABLE sessions ADD COLUMN watched_at INTEGER;
         UPDATE sessions SET
          watched_count=(SELECT cast(max AS INTEGER) FROM samples_raw r WHERE r.session_id=sessions.id AND r.metric=11 AND r.ts>=sessions.started_at AND r.ts<=coalesce(sessions.ended_at,sessions.last_seen_at) ORDER BY ts DESC LIMIT 1),
          watched_at=(SELECT max(ts) FROM samples_raw r WHERE r.session_id=sessions.id AND r.metric=11 AND r.ts>=sessions.started_at AND r.ts<=coalesce(sessions.ended_at,sessions.last_seen_at));"
    )
}

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
            let _ = std::fs::remove_file(&aside);
            std::fs::rename(path, &aside).map_err(|_| {
                rusqlite::Error::SqliteFailure(
                    rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CANTOPEN),
                    Some("无法保留损坏的统计数据库".into()),
                )
            })?;
            for ext in ["-wal", "-shm"] {
                let _ = std::fs::remove_file(path.with_file_name(format!(
                    "{}{ext}",
                    path.file_name().unwrap_or_default().to_string_lossy()
                )));
            }
            tracing::warn!("统计数据库损坏，已改名保留并重新创建");
            open_rw_once(path)
        }
        Err(error) => Err(error),
    }
}

fn corrupt_name(path: &Path) -> PathBuf {
    path.with_extension("sqlite.corrupt")
}

fn is_corrupt(error: &rusqlite::Error) -> bool {
    matches!(
        error.sqlite_error_code(),
        Some(rusqlite::ErrorCode::DatabaseCorrupt | rusqlite::ErrorCode::NotADatabase)
    )
}

fn open_rw_once(path: &Path) -> rusqlite::Result<Connection> {
    let conn = Connection::open(path)?;
    let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if version == 0 {
        let tables: i64 = conn.query_row("SELECT count(*) FROM sqlite_master", [], |r| r.get(0))?;
        if tables == 0 {
            conn.execute_batch("PRAGMA auto_vacuum=INCREMENTAL;")?;
        }
    } else if version > SCHEMA_VERSION {
        return Err(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_READONLY),
            Some("统计数据库版本较新".into()),
        ));
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
            tx.execute_batch(V1)?;
        }
        if version <= 1 {
            tx.execute_batch(V2)?;
        }
        if version <= 2 {
            compact_raw(&tx)?;
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
