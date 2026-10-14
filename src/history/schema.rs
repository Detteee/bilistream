//! `history.sqlite` schema v1 (design §5.1/§5.2). Separate file, never
//! through `Store`, never backed up or synced.

use rusqlite::{Connection, OpenFlags};
use std::path::{Path, PathBuf};

pub(crate) const SCHEMA_VERSION: i64 = 1;

const V1: &str = r#"
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
            let _ = std::fs::rename(path, &aside);
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
    let mode: i64 = conn.query_row("PRAGMA auto_vacuum", [], |r| r.get(0))?;
    if mode != 2 {
        tracing::warn!("统计数据库未使用增量清理模式 (auto_vacuum={mode})");
    }
    if version < SCHEMA_VERSION {
        conn.execute_batch(&format!(
            "BEGIN; {V1} PRAGMA user_version={SCHEMA_VERSION}; COMMIT;"
        ))?;
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
