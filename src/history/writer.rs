//! The single writer (design §5.3): maps events to rows, one transaction per
//! batch. Runs on its own OS thread; tests drive it directly.

use super::event::{EndReason, Event, EventKind};
use rusqlite::{params, Connection};
use std::collections::HashMap;

pub(crate) struct Writer {
    pub(crate) conn: Connection,
    pub(crate) node: String,
    pub(crate) boot_id: i64,
    counter: u64,
    /// seq → sessions.id for this boot.
    sessions: HashMap<u64, i64>,
    /// (seq, run) → runs.id
    runs: HashMap<(u64, u64), i64>,
    /// Session that samples without a seq (B站 counts) attach to.
    open_session: Option<i64>,
    /// Verbatim secrets (`bili_rtmp_key`, rtmp query) scrubbed from text.
    secrets: Vec<String>,
}

impl Writer {
    pub(crate) fn new(conn: Connection, node: &str, boot_id: i64) -> Self {
        Self {
            conn,
            node: node.to_string(),
            boot_id,
            counter: 0,
            sessions: HashMap::new(),
            runs: HashMap::new(),
            open_session: None,
            secrets: Vec::new(),
        }
    }

    pub(crate) fn set_secrets(&mut self, secrets: Vec<String>) {
        self.secrets = secrets.into_iter().filter(|s| s.len() >= 4).collect();
    }

    /// Replaces verbatim secrets before any TEXT is bound (design §5.7).
    pub(crate) fn scrub(&self, text: &str) -> String {
        let mut out = text.to_string();
        for secret in &self.secrets {
            if out.contains(secret.as_str()) {
                out = out.replace(secret.as_str(), "[已隐藏]");
            }
        }
        out
    }

    fn eid(&mut self) -> String {
        self.counter += 1;
        format!("{}:{:x}:{}", self.node, self.boot_id, self.counter)
    }

    /// One transaction for the whole batch. A failing row is skipped so one
    /// bad event never loses the batch.
    pub(crate) fn commit(&mut self, batch: &[Event]) -> rusqlite::Result<usize> {
        self.conn.execute_batch("BEGIN IMMEDIATE")?;
        let mut written = 0;
        for event in batch {
            match self.apply(event) {
                Ok(()) => written += 1,
                Err(error) => tracing::debug!("统计事件写入跳过: {error}"),
            }
        }
        match self.conn.execute_batch("COMMIT") {
            Ok(()) => Ok(written),
            Err(error) => {
                let _ = self.conn.execute_batch("ROLLBACK");
                Err(error)
            }
        }
    }

    fn session_row(&self, seq: u64) -> Option<i64> {
        self.sessions.get(&seq).copied()
    }

    pub(crate) fn apply(&mut self, event: &Event) -> rusqlite::Result<()> {
        let at = event.at as i64;
        match &event.kind {
            EventKind::ProcessStarted { version } => {
                self.conn.execute(
                    "INSERT OR REPLACE INTO boots (boot_id, node, started_at, last_seen_at, version) VALUES (?1,?2,?3,?3,?4)",
                    params![self.boot_id, self.node, at, version],
                )?;
                crash_sweep(&self.conn, &self.node, self.boot_id, at)?;
            }
            EventKind::SessionStarted {
                seq,
                platform,
                channel_id,
                channel_name,
                title,
                live_key,
            } => {
                let eid = self.eid();
                let platform = match *platform {
                    "YT" | "TW" | "NC" => *platform,
                    _ => "YT",
                };
                self.conn.execute(
                    "INSERT INTO sessions (eid,node,boot_id,seq,platform,channel_id,channel_name,title,started_at,last_seen_at,live_key)
                     VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?9,?10)",
                    params![
                        eid,
                        self.node,
                        self.boot_id,
                        *seq as i64,
                        platform,
                        self.scrub(channel_id),
                        self.scrub(channel_name),
                        title.as_deref().map(|t| self.scrub(t)),
                        at,
                        live_key
                    ],
                )?;
                let id = self.conn.last_insert_rowid();
                self.sessions.insert(*seq, id);
                self.open_session = Some(id);
            }
            EventKind::SessionLiveKey { seq, live_key } => {
                if let Some(id) = self.session_row(*seq) {
                    self.conn.execute(
                        "UPDATE sessions SET live_key=?2 WHERE id=?1 AND live_key IS NULL",
                        params![id, live_key],
                    )?;
                }
            }
            EventKind::SessionEnded { seq, reason } => {
                if let Some(id) = self.sessions.remove(seq) {
                    close_session(&self.conn, id, at, *reason)?;
                    if self.open_session == Some(id) {
                        self.open_session = None;
                    }
                }
            }
            EventKind::FfmpegStarted { seq, run } => {
                if let Some(sid) = self.session_row(*seq) {
                    let eid = self.eid();
                    self.conn.execute(
                        "INSERT INTO runs (eid,session_id,run,started_at,last_seen_at) VALUES (?1,?2,?3,?4,?4)",
                        params![eid, sid, *run as i64, at],
                    )?;
                    let rid = self.conn.last_insert_rowid();
                    self.runs.insert((*seq, *run), rid);
                    self.conn.execute(
                        "UPDATE sessions SET runs=runs+1, last_seen_at=max(last_seen_at,?2) WHERE id=?1",
                        params![sid, at],
                    )?;
                }
            }
            EventKind::FfmpegExited {
                seq,
                run,
                code,
                success,
                counted_failure,
                diagnostic,
            } => {
                if let Some(rid) = self.runs.remove(&(*seq, *run)) {
                    self.conn.execute(
                        "UPDATE runs SET ended_at=?2, last_seen_at=?2, exit_code=?3, success=?4, counted_failure=?5, diagnostic=?6 WHERE id=?1",
                        params![rid, at, code, *success, *counted_failure, diagnostic.as_deref().map(|d| self.scrub(d))],
                    )?;
                    if let Some(sid) = self.session_row(*seq) {
                        self.conn.execute(
                            "UPDATE sessions SET failures=failures+?2, last_seen_at=max(last_seen_at,?3) WHERE id=?1",
                            params![sid, *counted_failure as i64, at],
                        )?;
                    }
                }
            }
            EventKind::StatsSample {
                seq,
                bitrate_kbps,
                fps,
                speed,
            } => {
                let sid = self.session_row(*seq);
                let (min, mean, max) = *bitrate_kbps;
                self.sample(1, at, sid, min as f64, max as f64, mean as f64)?;
                if let Some(fps) = fps {
                    self.sample(2, at, sid, *fps as f64, *fps as f64, *fps as f64)?;
                }
                if let Some(speed) = speed {
                    self.sample(3, at, sid, *speed as f64, *speed as f64, *speed as f64)?;
                }
                if let Some(sid) = sid {
                    self.conn.execute(
                        "UPDATE sessions SET last_seen_at=max(last_seen_at,?2) WHERE id=?1",
                        params![sid, at],
                    )?;
                    self.conn.execute(
                        "UPDATE runs SET last_seen_at=max(last_seen_at,?2) WHERE session_id=?1 AND ended_at IS NULL",
                        params![sid, at],
                    )?;
                }
                self.bump_boot(at)?;
            }
            EventKind::BiliCounts { online, watched } => {
                let sid = self.open_session;
                if let Some(v) = online {
                    let v = *v as f64;
                    self.sample(10, at, sid, v, v, v)?;
                }
                if let Some(v) = watched {
                    let v = *v as f64;
                    self.sample(11, at, sid, v, v, v)?;
                }
            }
            EventKind::BiliLiveStarted { live_key, room } => {
                self.conn.execute(
                    "INSERT INTO bili_lives (live_key,node,room,started_at) VALUES (?1,?2,?3,?4)
                     ON CONFLICT(live_key) DO UPDATE SET started_at=min(started_at, excluded.started_at)",
                    params![live_key, self.node, *room as i64, at],
                )?;
            }
            EventKind::BiliLiveStopped { live_key, room } => {
                self.conn.execute(
                    "INSERT INTO bili_lives (live_key,node,room,started_at,ended_at) VALUES (?1,?2,?3,?4,?4)
                     ON CONFLICT(live_key) DO UPDATE SET ended_at=coalesce(ended_at, excluded.ended_at)",
                    params![live_key, self.node, *room as i64, at],
                )?;
            }
            EventKind::BiliLiveSummary { live_key, totals } => {
                self.conn.execute(
                    "INSERT OR IGNORE INTO bili_lives (live_key,node,room,started_at) VALUES (?1,?2,0,?3)",
                    params![live_key, self.node, at],
                )?;
                match totals {
                    Some(t) => self.conn.execute(
                        "UPDATE bili_lives SET summary_state='ok', bili_live_time_raw=?2, add_fans=?3, danmu_num=?4,
                         watched_count=?5, new_fans_club=?6, max_online=?7 WHERE live_key=?1",
                        params![live_key, t.live_time_raw, t.add_fans, t.danmu_num, t.watched_count, t.new_fans_club, t.max_online],
                    )?,
                    None => self.conn.execute(
                        "UPDATE bili_lives SET summary_state='failed', bili_live_time_raw=NULL, add_fans=NULL, danmu_num=NULL,
                         watched_count=NULL, new_fans_club=NULL, max_online=NULL WHERE live_key=?1 AND summary_state<>'ok'",
                        params![live_key],
                    )?,
                };
            }
            EventKind::BiliLiveSkipped { live_key } => {
                self.conn.execute(
                    "UPDATE bili_lives SET summary_state='skipped' WHERE live_key=?1 AND summary_state='pending'",
                    params![live_key],
                )?;
            }
            EventKind::Generic {
                kind,
                subject,
                severity,
                title,
                detail,
            } => {
                let eid = self.eid();
                self.conn.execute(
                    "INSERT INTO events (eid,at,node,kind,subject,severity,title,detail,session_id) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
                    params![eid, at, self.node, kind, subject.as_deref().map(|s| self.scrub(s)), severity.as_str(), self.scrub(title), self.scrub(detail), self.open_session],
                )?;
            }
            EventKind::NotifySent {
                key,
                phase,
                ok,
                code,
                suppressed,
                undelivered,
            } => {
                let eid = self.eid();
                self.conn.execute(
                    "INSERT INTO notify_sends (eid,at,node,event_key,phase,ok,code,suppressed,undelivered) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
                    params![eid, at, self.node, key, phase, *ok, code, suppressed, undelivered],
                )?;
            }
        }
        Ok(())
    }

    fn sample(
        &self,
        metric: i64,
        ts: i64,
        sid: Option<i64>,
        min: f64,
        max: f64,
        mean: f64,
    ) -> rusqlite::Result<()> {
        if !(min.is_finite() && max.is_finite() && mean.is_finite()) {
            return Ok(());
        }
        self.conn.execute(
            "INSERT INTO samples_raw (metric,node,ts,session_id,min,max,sum,count) VALUES (?1,?2,?3,?4,?5,?6,?7,1)
             ON CONFLICT(metric,node,ts) DO UPDATE SET min=min(min,excluded.min), max=max(max,excluded.max), sum=sum+excluded.sum, count=count+1",
            params![metric, self.node, ts, sid, min, max, mean],
        )?;
        Ok(())
    }

    fn bump_boot(&self, at: i64) -> rusqlite::Result<()> {
        self.conn.execute(
            "UPDATE boots SET last_seen_at=max(last_seen_at,?2) WHERE boot_id=?1",
            params![self.boot_id, at],
        )?;
        Ok(())
    }

    /// Writer heartbeat (30 s): keeps crash-closing accurate when idle.
    pub(crate) fn heartbeat(&self, at: u64) -> rusqlite::Result<()> {
        let at = at as i64;
        self.bump_boot(at)?;
        if let Some(sid) = self.open_session {
            self.conn.execute(
                "UPDATE sessions SET last_seen_at=max(last_seen_at,?2) WHERE id=?1 AND ended_at IS NULL",
                params![sid, at],
            )?;
            self.conn.execute(
                "UPDATE runs SET last_seen_at=max(last_seen_at,?2) WHERE session_id=?1 AND ended_at IS NULL",
                params![sid, at],
            )?;
        }
        Ok(())
    }
}

fn close_session(conn: &Connection, id: i64, at: i64, reason: EndReason) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE runs SET ended_at=?2 WHERE session_id=?1 AND ended_at IS NULL",
        params![id, at],
    )?;
    conn.execute(
        "UPDATE sessions SET ended_at=?2, last_seen_at=max(last_seen_at,?2), end_reason=?3 WHERE id=?1 AND ended_at IS NULL",
        params![id, at, reason.as_str()],
    )?;
    Ok(())
}

/// Design §5.5: close this node's rows from older boots. `bili_lives` stay
/// open (the room can outlive us), but a `pending` summary older than
/// 10 minutes is marked failed: the in-memory retry did not survive.
pub(crate) fn crash_sweep(
    conn: &Connection,
    node: &str,
    boot_id: i64,
    now: i64,
) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE runs SET ended_at=last_seen_at WHERE ended_at IS NULL AND session_id IN (SELECT id FROM sessions WHERE node=?1 AND boot_id<>?2)",
        params![node, boot_id],
    )?;
    conn.execute(
        "UPDATE sessions SET ended_at=last_seen_at, end_reason='process_restart' WHERE node=?1 AND boot_id<>?2 AND ended_at IS NULL",
        params![node, boot_id],
    )?;
    conn.execute(
        "UPDATE node_segments SET ended_at=last_seen_at WHERE observer=?1 AND ended_at IS NULL",
        params![node],
    )?;
    conn.execute(
        "UPDATE bili_lives SET summary_state='failed' WHERE summary_state='pending' AND ended_at IS NOT NULL AND ended_at < ?1",
        params![now - 600],
    )?;
    Ok(())
}
