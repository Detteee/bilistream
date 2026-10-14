//! Retention, rollup and vacuum (design §5.6). Each step is its own short
//! transaction; raw rows are deleted only below the rollup watermark.

use crate::config::HistoryRetention;
use rusqlite::{params, Connection, OptionalExtension};

const DAY: i64 = 86_400;
const BATCH: i64 = 5_000;
const MAX_BATCHES: usize = 4;

pub(crate) fn watermark(conn: &Connection, key: &str) -> rusqlite::Result<Option<i64>> {
    conn.query_row("SELECT value FROM meta WHERE key=?1", [key], |r| {
        r.get::<_, String>(0)
    })
    .optional()
    .map(|v| v.and_then(|v| v.parse().ok()))
}

/// Raw → 1 min over the half-open range `[wm, floor(now - 120, 60))`. The
/// watermark moves in the same transaction, so a range is never added twice.
pub(crate) fn rollup_1m(conn: &Connection, now: i64) -> rusqlite::Result<Option<(i64, i64)>> {
    let end = (now - 120).div_euclid(60) * 60;
    let start = match watermark(conn, "rollup_1m_watermark")? {
        Some(wm) => wm,
        None => {
            let first: Option<i64> =
                conn.query_row("SELECT min(ts) FROM samples_raw", [], |r| r.get(0))?;
            match first {
                Some(ts) => ts.div_euclid(60) * 60,
                None => return Ok(None),
            }
        }
    };
    if start >= end {
        return Ok(None);
    }
    let tx = conn.unchecked_transaction()?;
    tx.execute(
        "INSERT INTO samples_1m (metric,node,bucket,session_id,min,max,sum,count)
         SELECT metric,node,ts-(ts%60),max(session_id),min(min),max(max),sum(sum),sum(count)
         FROM samples_raw WHERE ts>=?1 AND ts<?2 GROUP BY 1,2,3 ORDER BY 1,2,3
         ON CONFLICT(metric,node,bucket) DO UPDATE SET min=min(samples_1m.min,excluded.min),
           max=max(samples_1m.max,excluded.max), sum=samples_1m.sum+excluded.sum, count=samples_1m.count+excluded.count",
        params![start, end],
    )?;
    tx.execute(
        "INSERT INTO meta (key,value) VALUES ('rollup_1m_watermark',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
        [end.to_string()],
    )?;
    tx.commit()?;
    Ok(Some((start, end)))
}

/// Deletes raw rows with `ts < min(watermark, now - raw_days)` in bounded
/// batches. Nothing is deleted before the first rollup.
pub(crate) fn prune_raw(conn: &Connection, now: i64, raw_days: u64) -> rusqlite::Result<usize> {
    let Some(wm) = watermark(conn, "rollup_1m_watermark")? else {
        return Ok(0);
    };
    let cutoff = wm.min(now - raw_days as i64 * DAY);
    let mut total = 0;
    for _ in 0..MAX_BATCHES {
        let n = conn.execute(
            "DELETE FROM samples_raw WHERE (metric,node,ts) IN (SELECT metric,node,ts FROM samples_raw WHERE ts<?1 LIMIT ?2)",
            params![cutoff, BATCH],
        )?;
        total += n;
        if (n as i64) < BATCH {
            break;
        }
    }
    Ok(total)
}

fn delete_batched(conn: &Connection, sql: &str, arg: i64) -> rusqlite::Result<usize> {
    let mut total = 0;
    for _ in 0..MAX_BATCHES {
        let n = conn.execute(sql, params![arg, BATCH])?;
        total += n;
        if (n as i64) < BATCH {
            break;
        }
    }
    Ok(total)
}

fn cap_rows(conn: &Connection, table: &str, cap: u64) -> rusqlite::Result<usize> {
    let count: i64 = conn.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))?;
    let over = (count - cap as i64).max(0).min(BATCH * MAX_BATCHES as i64);
    if over == 0 {
        return Ok(0);
    }
    conn.execute(
        &format!("DELETE FROM {table} WHERE id IN (SELECT id FROM {table} ORDER BY id LIMIT ?1)"),
        [over],
    )
}

#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct TickReport {
    pub rolled: Option<(i64, i64)>,
    pub raw_deleted: usize,
    pub rollup_deleted: usize,
    pub other_deleted: usize,
}

/// One maintenance tick (every 60 s on the writer thread).
pub(crate) fn tick(
    conn: &Connection,
    now: i64,
    r: &HistoryRetention,
) -> rusqlite::Result<TickReport> {
    let mut report = TickReport {
        rolled: rollup_1m(conn, now)?,
        ..Default::default()
    };
    report.raw_deleted = prune_raw(conn, now, r.raw_days)?;
    report.rollup_deleted = delete_batched(
        conn,
        "DELETE FROM samples_1m WHERE (metric,node,bucket) IN (SELECT metric,node,bucket FROM samples_1m WHERE bucket<?1 LIMIT ?2)",
        now - r.rollup_days as i64 * DAY,
    )?;
    let age = now - r.event_days as i64 * DAY;
    for (table, col) in [
        ("events", "at"),
        ("notify_sends", "at"),
        ("node_segments", "started_at"),
    ] {
        report.other_deleted += delete_batched(
            conn,
            &format!(
                "DELETE FROM {table} WHERE id IN (SELECT id FROM {table} WHERE {col}<?1 LIMIT ?2)"
            ),
            age,
        )?;
        report.other_deleted += cap_rows(conn, table, r.event_rows)?;
    }
    // Sessions (and their runs, by cascade) and closed B站 lives by age.
    report.other_deleted += delete_batched(
        conn,
        "DELETE FROM sessions WHERE id IN (SELECT id FROM sessions WHERE ended_at IS NOT NULL AND ended_at<?1 LIMIT ?2)",
        age,
    )?;
    report.other_deleted += cap_rows(conn, "runs", r.event_rows)?;
    conn.execute(
        "DELETE FROM bili_lives WHERE ended_at IS NOT NULL AND ended_at<?1",
        [age],
    )?;
    // An open B站 live older than 24 h never got a stop: no summary.
    conn.execute(
        "UPDATE bili_lives SET summary_state='skipped' WHERE summary_state='pending' AND ended_at IS NULL AND started_at<?1",
        [now - DAY],
    )?;
    let free: i64 = conn.query_row("PRAGMA freelist_count", [], |r| r.get(0))?;
    if free > 256 {
        // Never without an argument or 0: that drains the whole freelist.
        conn.execute_batch("PRAGMA incremental_vacuum(256);")?;
    }
    let _ = conn.execute_batch("PRAGMA wal_checkpoint(PASSIVE);");
    Ok(report)
}
