//! Bounded retention, rollups and space reclamation. Each SQL mutation is
//! its own short transaction; the durable watermark always precedes deletion.

use crate::config::HistoryRetention;
use rusqlite::{params, Connection, OptionalExtension};

const DAY: i64 = 86_400;
pub(crate) const BATCH: i64 = 2_000;
const MAX_BATCHES: usize = 4;
const MIB: u64 = 1024 * 1024;

pub(crate) fn watermark(conn: &Connection, key: &str) -> rusqlite::Result<Option<i64>> {
    conn.query_row("SELECT value FROM meta WHERE key=?1", [key], |r| {
        r.get::<_, String>(0)
    })
    .optional()
    .map(|v| v.and_then(|v| v.parse().ok()))
}

fn set_value(conn: &Connection, key: &str, value: i64) -> rusqlite::Result<()> {
    conn.execute("INSERT INTO meta(key,value) VALUES (?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value",params![key,value.to_string()])?;
    Ok(())
}

fn floor(conn: &Connection, key: &str, value: i64) -> rusqlite::Result<()> {
    set_value(conn, key, watermark(conn, key)?.unwrap_or(0).max(value))
}

fn cutoff(now: i64, days: u64) -> i64 {
    now.saturating_sub(days.min(i64::MAX as u64 / DAY as u64) as i64 * DAY)
}

/// Process a bounded chronological interval, rather than the whole backlog.
/// Buckets retain session identity. Replacing an already reconstructed bucket
/// is safe because this range is still represented in raw storage in full.
pub(crate) fn rollup_1m(conn: &Connection, now: i64) -> rusqlite::Result<Option<(i64, i64)>> {
    if !admit_maintenance(conn)? {
        return Ok(None);
    }
    let target = now.saturating_sub(120).div_euclid(60) * 60;
    let previous = watermark(conn, "rollup_1m_watermark")?;
    let first: Option<i64> = conn.query_row(
        "SELECT min(ts) FROM sample_frames WHERE ts>=?1 AND ts<?2",
        params![previous.unwrap_or(i64::MIN), target],
        |r| r.get(0),
    )?;
    let Some(first) = first else {
        if let Some(previous) = previous.filter(|w| *w < target) {
            set_value(conn, "rollup_1m_watermark", target)?;
            return Ok(Some((previous, target)));
        }
        return Ok(None);
    };
    let start = previous.unwrap_or(first.div_euclid(60) * 60);
    let boundary: Option<i64> = conn.query_row(
        "SELECT ts FROM sample_frames WHERE ts>=?1 AND ts<?2 ORDER BY ts,node,session_id LIMIT 1 OFFSET ?3",
        params![start,target,BATCH/5-1], |r|r.get(0),
    ).optional()?;
    // At normal sampling cadence a minute contains at most 42 rows. Do not
    // divide a minute: the watermark only represents completed buckets.
    let end = boundary.map_or(target, |t| {
        (t.div_euclid(60) * 60)
            .max(first.div_euclid(60) * 60 + 60)
            .min(target)
    });
    if end <= start {
        return Ok(None);
    }
    let tx = conn.unchecked_transaction()?;
    tx.execute(
        "INSERT INTO samples_1m(metric,node,bucket,session_id,min,max,sum,count)
         SELECT metric,node,ts-ts%60,session_id,min(min),max(max),sum(sum),sum(count)
         FROM samples_raw WHERE ts>=?1 AND ts<?2 GROUP BY ts-ts%60,metric,node,session_id
         ON CONFLICT(bucket,metric,node,session_id) DO UPDATE SET min=excluded.min,max=excluded.max,sum=excluded.sum,count=excluded.count",
        params![start,end],
    )?;
    set_value(&tx, "rollup_1m_watermark", end)?;
    tx.commit()?;
    Ok(Some((start, end)))
}

/// No raw row is discarded before its aggregate is durably committed.
pub(crate) fn prune_raw(conn: &Connection, now: i64, raw_days: u64) -> rusqlite::Result<usize> {
    let Some(wm) = watermark(conn, "rollup_1m_watermark")? else {
        return Ok(0);
    };
    let before = wm.min(cutoff(now, raw_days));
    let mut total = 0;
    for _ in 0..MAX_BATCHES {
        if !admit_maintenance(conn)? {
            break;
        }
        let tx = conn.unchecked_transaction()?;
        let count: i64 = tx.query_row(
            "SELECT coalesce(sum((b_count IS NOT NULL)+(f_count IS NOT NULL)+(s_count IS NOT NULL)+(o_count IS NOT NULL)+(w_count IS NOT NULL)),0)
             FROM (SELECT * FROM sample_frames WHERE ts<?1 ORDER BY ts,node,session_id LIMIT ?2)",
            params![before,BATCH/5], |r|r.get(0),
        )?;
        let frames = tx.execute(
            "DELETE FROM sample_frames WHERE (ts,node,session_id) IN
             (SELECT ts,node,session_id FROM sample_frames WHERE ts<?1 ORDER BY ts,node,session_id LIMIT ?2)",
            params![before,BATCH/5],
        )?;
        if frames > 0 {
            floor(&tx, "raw_pruned_before", before)?;
        }
        tx.commit()?;
        total += count as usize;
        if frames < (BATCH / 5) as usize {
            break;
        }
    }
    Ok(total)
}

fn delete_samples(
    conn: &Connection,
    table: &str,
    col: &str,
    before: i64,
    key: &str,
) -> rusqlite::Result<usize> {
    let mut total = 0;
    for _ in 0..MAX_BATCHES {
        if !admit_maintenance(conn)? {
            break;
        }
        let tx = conn.unchecked_transaction()?;
        let n=tx.execute(&format!(
            "DELETE FROM {table} WHERE ({col},metric,node,session_id) IN
             (SELECT {col},metric,node,session_id FROM {table} WHERE {col}<?1 ORDER BY {col},metric,node,session_id LIMIT ?2)"
        ),params![before,BATCH])?;
        if n > 0 {
            floor(&tx, key, before)?;
        }
        tx.commit()?;
        total += n;
        if n < BATCH as usize {
            break;
        }
    }
    Ok(total)
}

fn delete_rows(
    conn: &Connection,
    table: &str,
    col: &str,
    before: i64,
    cap: u64,
    closed_only: bool,
    key: &str,
) -> rusqlite::Result<usize> {
    let mut total = 0;
    let closed = if closed_only {
        "AND ended_at IS NOT NULL"
    } else {
        ""
    };
    for _ in 0..MAX_BATCHES {
        if !admit_maintenance(conn)? {
            break;
        }
        let count: i64 =
            conn.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))?;
        let over = count
            .saturating_sub(cap.min(i64::MAX as u64) as i64)
            .max(0)
            .min(BATCH);
        let tx = conn.unchecked_transaction()?;
        let ids: Vec<(i64, i64)> = {
            let mut stmt = tx.prepare(&format!(
                "SELECT id,{col} FROM {table} WHERE {col}<?1 {closed} ORDER BY {col},id LIMIT ?2"
            ))?;
            let mut ids = stmt
                .query_map(params![before, BATCH], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            if ids.is_empty() && over > 0 {
                let mut stmt = tx.prepare(&format!(
                    "SELECT id,{col} FROM {table} WHERE 1=1 {closed} ORDER BY {col},id LIMIT ?1"
                ))?;
                ids = stmt
                    .query_map([over], |r| Ok((r.get(0)?, r.get(1)?)))?
                    .collect::<rusqlite::Result<_>>()?;
            }
            ids
        };
        if ids.is_empty() {
            break;
        }
        let mut delete = tx.prepare_cached(&format!("DELETE FROM {table} WHERE id=?1"))?;
        for (id, _) in &ids {
            delete.execute([id])?;
        }
        drop(delete);
        floor(
            &tx,
            key,
            ids.iter()
                .map(|(_, t)| *t)
                .max()
                .unwrap_or(0)
                .saturating_add(1),
        )?;
        tx.commit()?;
        total += ids.len();
    }
    Ok(total)
}

/// Evict sessions as complete groups. A group being removed is hidden from
/// reads before its runs are deleted, so a half-pruned session cannot report
/// an invented success rate or availability. Active sessions are protected.
fn prune_sessions(
    conn: &Connection,
    now: i64,
    r: &HistoryRetention,
    pressure: bool,
) -> rusqlite::Result<usize> {
    if !admit_maintenance(conn)? {
        return Ok(0);
    }
    let age = cutoff(now, r.event_days);
    let sessions: i64 = conn.query_row("SELECT count(*) FROM sessions", [], |r| r.get(0))?;
    let runs: i64 = conn.query_row("SELECT count(*) FROM runs", [], |r| r.get(0))?;
    let cap = r.event_rows.min(i64::MAX as u64) as i64;
    let over = sessions > cap || runs > cap || pressure;
    let mut total = 0;
    {
        let tx = conn.unchecked_transaction()?;
        let marks: Vec<(i64, i64)> = {
            let mut stmt = tx.prepare(
                "SELECT id,ended_at FROM sessions WHERE ended_at IS NOT NULL AND pruned=0
                 AND (ended_at<?1 OR ?2) ORDER BY ended_at,id LIMIT 32",
            )?;
            let rows = stmt
                .query_map(params![age, over], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<rusqlite::Result<_>>()?;
            rows
        };
        for (id, end) in marks {
            tx.execute("UPDATE sessions SET pruned=1 WHERE id=?1", [id])?;
            floor(&tx, "sessions_pruned_before", end.saturating_add(1))?;
        }
        tx.commit()?;
    }
    for _ in 0..MAX_BATCHES {
        if !admit_maintenance(conn)? {
            break;
        }
        let tx = conn.unchecked_transaction()?;
        let n=tx.execute(
            "DELETE FROM runs WHERE id IN (SELECT r.id FROM runs r JOIN sessions s ON s.id=r.session_id WHERE s.pruned=1 LIMIT ?1)",[BATCH],
        )?;
        total += n;
        total += tx.execute(
            "DELETE FROM sessions WHERE id IN (SELECT s.id FROM sessions s WHERE s.pruned=1
             AND NOT EXISTS(SELECT 1 FROM runs r WHERE r.session_id=s.id) LIMIT ?1)",
            [BATCH],
        )?;
        tx.commit()?;
        if n < BATCH as usize {
            break;
        }
    }
    Ok(total)
}

fn prune_book_and_boots(
    conn: &Connection,
    now: i64,
    r: &HistoryRetention,
    pressure: bool,
) -> rusqlite::Result<usize> {
    if !admit_maintenance(conn)? {
        return Ok(0);
    }
    let age = cutoff(now, r.event_days);
    let cap = if pressure {
        0
    } else {
        r.event_rows.min(i64::MAX as u64) as i64
    };
    let excess = |table: &str| -> rusqlite::Result<i64> {
        let count: i64 =
            conn.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))?;
        Ok(count.saturating_sub(cap).max(0).min(BATCH))
    };
    let lives_over = excess("bili_lives")?;
    let boots_over = excess("boots")?;
    let tx = conn.unchecked_transaction()?;
    let lives: Vec<(String, i64)> = {
        let mut stmt = tx.prepare(
        "SELECT b.live_key,coalesce(b.ended_at,b.started_at) FROM bili_lives b
         WHERE NOT EXISTS(SELECT 1 FROM sessions s WHERE s.live_key=b.live_key AND s.ended_at IS NULL)
         AND (b.ended_at IS NOT NULL OR EXISTS(SELECT 1 FROM bili_lives n WHERE n.room=b.room AND n.started_at>b.started_at))
         ORDER BY coalesce(b.ended_at,b.started_at),b.live_key LIMIT ?1",
        )?;
        let rows = stmt
            .query_map([BATCH], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<rusqlite::Result<Vec<(String, i64)>>>()?;
        rows.into_iter()
            .enumerate()
            .filter_map(|(i, row)| (row.1 < age || (i as i64) < lives_over).then_some(row))
            .collect()
    };
    for (key, at) in &lives {
        tx.execute("DELETE FROM bili_lives WHERE live_key=?1", [key])?;
        floor(&tx, "bili_pruned_before", at.saturating_add(1))?;
    }
    // Keep only boots still referenced by retained sessions or recent process
    // observations. Removing them cannot change any retained session identity.
    let boots = tx.execute(
        "DELETE FROM boots WHERE boot_id IN (SELECT b.boot_id FROM boots b WHERE (b.last_seen_at<?1 OR ?3>0)
         AND NOT EXISTS(SELECT 1 FROM sessions s WHERE s.boot_id=b.boot_id AND s.node=b.node)
         AND b.boot_id<>(SELECT boot_id FROM boots ORDER BY started_at DESC,boot_id DESC LIMIT 1)
         ORDER BY b.last_seen_at LIMIT ?2)",
        params![age, if boots_over > 0 { boots_over } else { BATCH }, boots_over],
    )?;
    tx.commit()?;
    Ok(lives.len() + boots)
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Space {
    pub db_bytes: u64,
    pub wal_bytes: u64,
    pub limit_bytes: u64,
    pub wal_limit_bytes: u64,
    pub pressure: bool,
}

pub(crate) fn space(conn: &Connection, r: &HistoryRetention) -> rusqlite::Result<Space> {
    let page_size: u64 = conn.query_row("PRAGMA page_size", [], |r| r.get(0))?;
    let pages: u64 = conn.query_row("PRAGMA page_count", [], |r| r.get(0))?;
    let limit_bytes = r.max_mb.max(1).saturating_mul(MIB);
    // max_mb is the combined DB+WAL budget; reserve a bounded part for WAL.
    let wal_limit_bytes = (limit_bytes / 8).clamp(64 * 1024, 16 * MIB);
    let wal_bytes = conn
        .path()
        .map(|p| std::path::PathBuf::from(format!("{p}-wal")))
        .and_then(|p| std::fs::metadata(p).ok())
        .map(|m| m.len())
        .unwrap_or(0);
    let db_bytes = page_size.saturating_mul(pages).max(
        conn.path()
            .and_then(|p| std::fs::metadata(p).ok())
            .map(|m| m.len())
            .unwrap_or(0),
    );
    Ok(Space {
        db_bytes,
        wal_bytes,
        limit_bytes,
        wal_limit_bytes,
        pressure: db_bytes.saturating_add(wal_bytes) >= limit_bytes
            || db_bytes >= limit_bytes.saturating_sub(wal_limit_bytes.saturating_mul(2)),
    })
}

pub(crate) fn configure_limits(conn: &Connection, r: &HistoryRetention) -> rusqlite::Result<Space> {
    let s = space(conn, r)?;
    let page_size: u64 = conn.query_row("PRAGMA page_size", [], |r| r.get(0))?;
    // Reserve twice the admission threshold: one part for existing WAL,
    // one for the next bounded transaction while a reader pins its snapshot.
    let pages = (s
        .limit_bytes
        .saturating_sub(s.wal_limit_bytes.saturating_mul(2))
        / page_size)
        .max(32);
    // SQLite refuses page growth beyond this bound. If a pre-existing file
    // exceeds it, retention/vacuum first makes room without deleting actives.
    let _: u64 = conn.query_row(&format!("PRAGMA max_page_count={pages}"), [], |r| r.get(0))?;
    conn.pragma_update(None, "journal_size_limit", s.wal_limit_bytes as i64)?;
    conn.pragma_update(
        None,
        "wal_autocheckpoint",
        (s.wal_limit_bytes / page_size / 4).max(1) as i64,
    )?;
    Ok(s)
}

fn checkpoint(conn: &Connection, truncate: bool) -> rusqlite::Result<bool> {
    // A reader holding an old WAL snapshot must not stall the writer for the
    // normal two-second busy timeout. Leave it for the next bounded tick.
    conn.busy_timeout(std::time::Duration::ZERO)?;
    let result = conn.query_row(
        if truncate {
            "PRAGMA wal_checkpoint(TRUNCATE)"
        } else {
            "PRAGMA wal_checkpoint(PASSIVE)"
        },
        [],
        |r| r.get::<_, i64>(0),
    );
    conn.busy_timeout(std::time::Duration::from_secs(2))?;
    result.map(|busy| busy == 0)
}

/// A tick contains several independently committed cleanup batches. Admit
/// each one against the WAL allowance installed by configure_limits, not
/// just the first batch: a reader can pin every later commit in that tick.
/// Standalone migration/rollup callers without configured limits keep the
/// default SQLite journal policy (a negative journal_size_limit).
fn admit_maintenance(conn: &Connection) -> rusqlite::Result<bool> {
    let limit: i64 = conn.query_row("PRAGMA journal_size_limit", [], |row| row.get(0))?;
    if limit <= 0 {
        return Ok(true);
    }
    let wal_bytes = conn
        .path()
        .and_then(|path| std::fs::metadata(format!("{path}-wal")).ok())
        .map(|metadata| metadata.len())
        .unwrap_or(0);
    if wal_bytes >= limit as u64 {
        return checkpoint(conn, true);
    }
    Ok(true)
}

/// Before accepting another write batch, checkpoint a WAL above its allotted
/// budget. If a bounded read still pins it, pause history writes, never streams.
pub(crate) fn admit_write(conn: &Connection, r: &HistoryRetention) -> rusqlite::Result<bool> {
    let s = space(conn, r)?;
    if s.wal_bytes >= s.wal_limit_bytes || s.db_bytes.saturating_add(s.wal_bytes) >= s.limit_bytes {
        return checkpoint(conn, true);
    }
    Ok(true)
}

#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct TickReport {
    pub rolled: Option<(i64, i64)>,
    pub raw_deleted: usize,
    pub rollup_deleted: usize,
    pub other_deleted: usize,
    pub pressure: bool,
}

pub(crate) fn tick(
    conn: &Connection,
    now: i64,
    r: &HistoryRetention,
) -> rusqlite::Result<TickReport> {
    let s = configure_limits(conn, r)?;
    let mut report = TickReport {
        pressure: s.pressure,
        ..Default::default()
    };
    if !admit_write(conn, r)? {
        return Ok(report);
    }
    for _ in 0..MAX_BATCHES {
        let Some((a, b)) = rollup_1m(conn, now)? else {
            break;
        };
        report.rolled = Some((report.rolled.map_or(a, |(start, _)| start), b));
    }
    // Cap pressure first shortens raw storage, then minute history, then
    // complete oldest closed sessions/events. Active rows are never victims.
    let raw_days = if s.pressure {
        r.raw_days.min(1)
    } else {
        r.raw_days
    };
    report.raw_deleted = prune_raw(conn, now, raw_days)?;
    let rollup_days = if s.pressure && report.raw_deleted == 0 {
        r.rollup_days.min(14)
    } else {
        r.rollup_days
    };
    report.rollup_deleted = delete_samples(
        conn,
        "samples_1m",
        "bucket",
        cutoff(now, rollup_days),
        "samples_pruned_before",
    )?;
    report.rollup_deleted += delete_samples(
        conn,
        "samples_1h",
        "bucket",
        cutoff(now, r.rollup_days),
        "samples_pruned_before",
    )?;
    let age = cutoff(now, r.event_days);
    let evict = s.pressure && report.raw_deleted == 0 && report.rollup_deleted == 0;
    for (table, col, key, closed) in [
        ("events", "at", "events_pruned_before", false),
        ("notify_sends", "at", "notify_pruned_before", false),
        ("node_segments", "ended_at", "nodes_pruned_before", true),
    ] {
        report.other_deleted += delete_rows(
            conn,
            table,
            col,
            age,
            if evict { 0 } else { r.event_rows },
            closed,
            key,
        )?;
    }
    report.other_deleted += prune_sessions(conn, now, r, evict)?;
    report.other_deleted += prune_book_and_boots(conn, now, r, evict)?;
    let free: i64 = conn.query_row("PRAGMA freelist_count", [], |r| r.get(0))?;
    if free > 0 && admit_maintenance(conn)? {
        conn.execute_batch("PRAGMA incremental_vacuum(256)")?;
    }
    let truncate = s.pressure
        || s.wal_bytes >= s.wal_limit_bytes
        || watermark(conn, "checkpoint_at")?.is_none_or(|t| now.saturating_sub(t) >= 3600);
    if checkpoint(conn, truncate)? && truncate {
        set_value(conn, "checkpoint_at", now)?;
    }
    if admit_maintenance(conn)? {
        set_value(conn, "space_pressure", s.pressure as i64)?;
    }
    Ok(report)
}
