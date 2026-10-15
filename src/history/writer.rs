//! The single writer (design §5.3): maps events to rows, one transaction per
//! batch. Runs on its own OS thread; tests drive it directly.

use super::event::{BiliLiveRecord, BiliSummaryPhase, EndReason, Event, EventKind, Severity};
use rusqlite::{params, Connection, OptionalExtension};
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
    /// Compatibility association for legacy Generic events only. Samples
    /// always use their captured boot/sequence identity.
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
        crate::logging::safe_text(&out, 240)
    }

    fn eid(&mut self) -> String {
        self.counter += 1;
        format!("{}:{:x}:{}", self.node, self.boot_id, self.counter)
    }

    /// One transaction for the whole batch, including writer-side identity
    /// maps. A failed write must not leave a run counter or a cached row id
    /// referring to a row that SQLite rolled back.
    #[cfg(test)]
    pub(crate) fn commit(&mut self, batch: &[Event]) -> rusqlite::Result<usize> {
        self.commit_refs(batch.iter())
    }

    pub(crate) fn commit_refs<'a>(
        &mut self,
        batch: impl IntoIterator<Item = &'a Event>,
    ) -> rusqlite::Result<usize> {
        self.conn.execute_batch("BEGIN IMMEDIATE")?;
        let saved_sessions = self.sessions.clone();
        let saved_runs = self.runs.clone();
        let saved_open = self.open_session;
        let saved_counter = self.counter;
        let mut written = 0;
        for event in batch {
            if let Err(error) = self.apply(event) {
                let _ = self.conn.execute_batch("ROLLBACK");
                self.sessions = saved_sessions;
                self.runs = saved_runs;
                self.open_session = saved_open;
                self.counter = saved_counter;
                return Err(error);
            }
            written += 1;
        }
        match self.conn.execute_batch("COMMIT") {
            Ok(()) => Ok(written),
            Err(error) => {
                let _ = self.conn.execute_batch("ROLLBACK");
                self.sessions = saved_sessions;
                self.runs = saved_runs;
                self.open_session = saved_open;
                self.counter = saved_counter;
                Err(error)
            }
        }
    }

    fn session_row(&self, seq: u64) -> Option<i64> {
        self.sessions.get(&seq).copied()
    }

    fn captured_session(&self, seq: u64, room: u64, at: i64) -> rusqlite::Result<Option<i64>> {
        self.conn
            .query_row(
                "SELECT id FROM sessions WHERE node=?1 AND boot_id=?2 AND seq=?3 AND room=?4
             AND started_at<=?5 AND (ended_at IS NULL OR ended_at>=?5)",
                params![
                    self.node,
                    self.boot_id,
                    seq as i64,
                    room.min(i64::MAX as u64) as i64,
                    at
                ],
                |r| r.get(0),
            )
            .optional()
    }

    fn record_event(
        &mut self,
        at: i64,
        kind: &str,
        subject: Option<&str>,
        severity: Severity,
        title: &str,
        detail: &str,
        sid: Option<i64>,
    ) -> rusqlite::Result<()> {
        let eid = self.eid();
        self.conn.execute(
            "INSERT INTO events(eid,at,node,kind,subject,severity,title,detail,session_id)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
            params![
                eid,
                at,
                self.node,
                kind,
                subject.map(|s| self.scrub(s)),
                severity.as_str(),
                self.scrub(title),
                self.scrub(detail),
                sid
            ],
        )?;
        Ok(())
    }

    pub(crate) fn apply(&mut self, event: &Event) -> rusqlite::Result<()> {
        let at = event.at as i64;
        match &event.kind {
            EventKind::BroadcastStarted { .. } | EventKind::BroadcastEnded => {}
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
                area_v2,
                room,
            } => {
                let eid = self.eid();
                let platform = match *platform {
                    "YT" | "TW" | "NC" => *platform,
                    _ => "YT",
                };
                let previous: i64 = self.conn.query_row(
                    "SELECT max(coalesce((SELECT cast(value AS INTEGER) FROM meta WHERE key='session_id_highwater'),0),coalesce((SELECT max(id) FROM sessions),0))",
                    [], |r| r.get(0),
                )?;
                let id = previous
                    .checked_add(1)
                    .ok_or(rusqlite::Error::InvalidQuery)?;
                self.conn.execute(
                    "INSERT INTO sessions (eid,node,boot_id,seq,platform,channel_id,channel_name,title,started_at,last_seen_at,live_key,area_v2,room,id)
                     VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?9,?10,?11,?12,?13)",
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
                        live_key,
                        area_v2.map(|v| v.min(i64::MAX as u64) as i64),
                        (*room).min(i64::MAX as u64) as i64,
                        id,
                    ],
                )?;
                self.conn.execute(
                    "INSERT INTO meta(key,value) VALUES ('session_id_highwater',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
                    [id.to_string()],
                )?;
                self.sessions.insert(*seq, id);
                self.open_session = Some(id);
            }
            EventKind::SessionLiveKey { seq, live_key } => {
                // GetInfo may finish after this session closes or another
                // session starts. The captured boot/sequence is authoritative.
                self.conn.execute(
                    "UPDATE sessions SET live_key=?4 WHERE node=?1 AND boot_id=?2 AND seq=?3 AND live_key IS NULL AND pruned=0",
                    params![self.node, self.boot_id, *seq as i64, live_key],
                )?;
            }
            EventKind::SessionEnded { seq, reason } => {
                if let Some(id) = self.sessions.remove(seq) {
                    close_session(&self.conn, id, at, *reason)?;
                    self.runs.retain(|(run_seq, _), _| run_seq != seq);
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
                let Some(id) = self.session_row(*seq) else {
                    return Ok(());
                };
                let sid = Some(id);
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
            EventKind::BiliCounts {
                seq,
                room,
                online,
                watched,
            } => {
                let Some(id) = self.captured_session(*seq, *room, at)? else {
                    return Ok(());
                };
                let sid = Some(id);
                if let Some(v) = online {
                    let v = *v as f64;
                    self.sample(10, at, sid, v, v, v)?;
                }
                if let Some(v) = watched {
                    let value = *v as f64;
                    self.sample(11, at, sid, value, value, value)?;
                    self.conn.execute(
                        "UPDATE sessions SET watched_count=?2,watched_at=?3 WHERE id=?1 AND (watched_at IS NULL OR watched_at<=?3)",
                        params![id,(*v).min(i64::MAX as u64) as i64,at],
                    )?;
                }
            }
            EventKind::BiliLiveState { record } => {
                self.persist_live_record(record)?;
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
                self.record_event(
                    at,
                    kind,
                    subject.as_deref(),
                    *severity,
                    title,
                    detail,
                    self.open_session,
                )?;
            }
            EventKind::NotifySent {
                key,
                dedup,
                phase,
                mode,
                ok,
                code,
                trace_id,
                test,
                suppressed,
                undelivered,
            } => {
                let eid = self.eid();
                self.conn.execute(
                    "INSERT INTO notify_sends (eid,at,node,event_key,phase,ok,code,suppressed,undelivered,dedup_key,mode,trace_id,test) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)",
                    params![eid, at, self.node, self.scrub(key), phase, *ok, code, suppressed, undelivered, self.scrub(dedup), mode, trace_id.as_deref().map(|t| self.scrub(t)), *test],
                )?;
            }
            EventKind::FfmpegStuck { seq, run, reason } => {
                let found: Option<(i64, i64)> = self
                    .conn
                    .query_row(
                        "SELECT r.id,s.id FROM runs r JOIN sessions s ON s.id=r.session_id
                     WHERE s.node=?1 AND s.boot_id=?2 AND s.seq=?3 AND r.run=?4",
                        params![self.node, self.boot_id, *seq as i64, *run as i64],
                        |r| Ok((r.get(0)?, r.get(1)?)),
                    )
                    .optional()?;
                if let Some((rid, sid)) = found {
                    let changed = self.conn.execute(
                        "UPDATE runs SET stuck_reason=?2 WHERE id=?1 AND (stuck_reason IS NULL OR (stuck_reason='idle_drain' AND ?2<>'idle_drain'))",
                        params![rid, self.scrub(reason)],
                    )?;
                    // idle_drain is the normal end-of-source buffer drain.
                    if changed > 0 && *reason != "idle_drain" {
                        self.conn
                            .execute("UPDATE sessions SET stuck=stuck+1 WHERE id=?1", [sid])?;
                        self.record_event(
                            at,
                            "ffmpeg.stuck",
                            None,
                            Severity::Warn,
                            "推流进度停滞",
                            reason,
                            Some(sid),
                        )?;
                    }
                }
            }
            EventKind::BiliStartFailed { seq, error } => {
                self.record_event(
                    at,
                    "bili.start_failed",
                    None,
                    Severity::Error,
                    "B站开播失败",
                    error,
                    seq.and_then(|s| self.session_row(s)),
                )?;
            }
            EventKind::BiliWarning { seq, room, msg }
            | EventKind::BiliCutOff { seq, room, msg } => {
                let cut = matches!(&event.kind, EventKind::BiliCutOff { .. });
                self.record_event(
                    at,
                    if cut { "bili.cut_off" } else { "bili.warning" },
                    Some(&room.to_string()),
                    if cut { Severity::Error } else { Severity::Warn },
                    if cut {
                        "B站切断推流"
                    } else {
                        "B站直播警告"
                    },
                    msg,
                    seq.and_then(|s| self.session_row(s)),
                )?;
            }
            EventKind::RoomLocked { room, until } => {
                self.record_event(
                    at,
                    "bili.room_locked",
                    Some(&room.to_string()),
                    Severity::Error,
                    "B站直播间锁定",
                    &format!("until={until}"),
                    None,
                )?;
            }
            EventKind::NodeState {
                node_id,
                state,
                reason,
                initial,
                minority,
            } => {
                let previous: Option<(i64,String,String,bool)> = self.conn.query_row(
                    "SELECT id,state,reason,minority FROM node_segments WHERE observer=?1 AND subject=?2 AND ended_at IS NULL ORDER BY id DESC LIMIT 1",
                    params![self.node, node_id], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?)),
                ).optional()?;
                let same = previous.as_ref().is_some_and(|(_, s, r, m)| {
                    s == state.as_str() && r == reason && *m == *minority
                }) && !*initial;
                if same {
                    self.conn.execute(
                        "UPDATE node_segments SET last_seen_at=max(last_seen_at,?2) WHERE id=?1",
                        params![previous.unwrap().0, at],
                    )?;
                } else {
                    self.conn.execute(
                        "UPDATE node_segments SET ended_at=CASE WHEN ?3 THEN last_seen_at ELSE max(started_at,?4) END WHERE observer=?1 AND subject=?2 AND ended_at IS NULL",
                        params![self.node,node_id,*initial,at],
                    )?;
                    let eid = self.eid();
                    self.conn.execute(
                        "INSERT INTO node_segments(eid,observer,subject,state,reason,started_at,last_seen_at,initial,minority) VALUES (?1,?2,?3,?4,?5,?6,?6,?7,?8)",
                        params![eid,self.node,self.scrub(node_id),state.as_str(),self.scrub(reason),at,*initial,*minority],
                    )?;
                    self.record_event(
                        at,
                        "cluster.node_state",
                        Some(node_id),
                        if *initial
                            || *minority
                            || !matches!(
                                state,
                                crate::events::NodeState::Down | crate::events::NodeState::Degraded
                            )
                        {
                            Severity::Info
                        } else {
                            Severity::Warn
                        },
                        &format!("{node_id} · {}", state.as_str()),
                        reason,
                        None,
                    )?;
                }
            }
            EventKind::RecorderHeartbeat => {
                self.conn.execute("UPDATE node_segments SET last_seen_at=max(last_seen_at,?2) WHERE observer=?1 AND ended_at IS NULL",params![self.node,at])?;
            }
            EventKind::RecorderStopped => {
                self.conn.execute("UPDATE node_segments SET ended_at=last_seen_at WHERE observer=?1 AND ended_at IS NULL",[&self.node])?;
            }
            EventKind::OwnerChanged { from, to, failover } => {
                self.record_event(
                    at,
                    "cluster.owner_changed",
                    to.as_deref(),
                    if *failover {
                        Severity::Warn
                    } else {
                        Severity::Info
                    },
                    if *failover {
                        "集群故障转移"
                    } else {
                        "集群活跃节点变更"
                    },
                    &format!(
                        "{} → {}",
                        from.as_deref().unwrap_or("—"),
                        to.as_deref().unwrap_or("—")
                    ),
                    None,
                )?;
            }
            EventKind::QuorumChanged { has_quorum } => {
                self.record_event(
                    at,
                    "cluster.quorum",
                    None,
                    if *has_quorum {
                        Severity::Info
                    } else {
                        Severity::Warn
                    },
                    if *has_quorum {
                        "集群多数派恢复"
                    } else {
                        "集群暂无多数派"
                    },
                    "",
                    None,
                )?;
            }
            EventKind::LocalFault { reason, latched } => {
                self.record_event(
                    at,
                    "cluster.local_fault",
                    None,
                    if *latched {
                        Severity::Warn
                    } else {
                        Severity::Info
                    },
                    if *latched {
                        "本节点异常"
                    } else {
                        "本节点恢复"
                    },
                    reason,
                    None,
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
            "INSERT INTO samples_raw (metric,node,ts,session_id,min,max,sum,count) VALUES (?1,?2,?3,?4,?5,?6,?7,1)",
            params![metric, self.node, ts, sid.unwrap_or(0), min, max, mean],
        )?;
        // A sample delayed beyond the rollup grace is applied to the durable
        // bucket in the same transaction. Later maintenance starts after the
        // watermark, so this late observation is neither lost nor counted twice.
        self.conn.execute(
            "INSERT INTO samples_1m(metric,node,bucket,session_id,min,max,sum,count)
             SELECT ?1,?2,?3-?3%60,?4,?5,?6,?7,1
             WHERE ?3<coalesce((SELECT cast(value AS INTEGER) FROM meta WHERE key='rollup_1m_watermark'),0)
             ON CONFLICT(bucket,metric,node,session_id) DO UPDATE SET min=min(samples_1m.min,excluded.min),max=max(samples_1m.max,excluded.max),sum=samples_1m.sum+excluded.sum,count=samples_1m.count+1",
            params![metric,self.node,ts,sid.unwrap_or(0),min,max,mean],
        )?;
        Ok(())
    }

    fn persist_live_record(&self, record: &BiliLiveRecord) -> rusqlite::Result<()> {
        use super::live_book::{BiliLiveBook, BiliLiveMark};
        let mut merged = BiliLiveBook::default();
        let previous = self
            .conn
            .query_row(
                &format!("SELECT {LIVE_RECORD_COLS} FROM bili_lives WHERE live_key=?1"),
                [&record.live_key],
                live_record_row,
            )
            .optional()?;
        if let Some(previous) = previous.as_ref().filter(|p| p.room == record.room) {
            merged
                .entries
                .push(BiliLiveMark::from_record(previous.clone()));
        }
        merged.join(&BiliLiveBook {
            entries: vec![BiliLiveMark::from_record(record.clone())],
        });
        let record = merged
            .entries
            .first()
            .ok_or(rusqlite::Error::InvalidQuery)?
            .record();
        if previous.as_ref() == Some(&record) {
            return Ok(());
        }
        let (phase, state) = match record.summary_phase {
            BiliSummaryPhase::Pending => ("pending", "pending"),
            BiliSummaryPhase::Claimed => ("claimed", "pending"),
            BiliSummaryPhase::Failed => ("failed", "failed"),
            BiliSummaryPhase::Done => ("done", "ok"),
            BiliSummaryPhase::Skipped => ("skipped", "skipped"),
        };
        let totals = record.totals.as_ref().filter(|_| phase == "done");
        self.conn.execute(
            "INSERT INTO bili_lives(live_key,node,room,started_at,ended_at,summary_state,started_by,summary_phase,summary_node,summary_at,summary_generation,bili_live_time_raw,add_fans,danmu_num,watched_count,new_fans_club,max_online)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17)
             ON CONFLICT(live_key) DO UPDATE SET room=excluded.room,started_at=excluded.started_at,ended_at=excluded.ended_at,summary_state=excluded.summary_state,started_by=excluded.started_by,summary_phase=excluded.summary_phase,summary_node=excluded.summary_node,summary_at=excluded.summary_at,summary_generation=excluded.summary_generation,bili_live_time_raw=excluded.bili_live_time_raw,add_fans=excluded.add_fans,danmu_num=excluded.danmu_num,watched_count=excluded.watched_count,new_fans_club=excluded.new_fans_club,max_online=excluded.max_online",
            params![record.live_key,self.node,record.room as i64,record.started_at as i64,record.stopped_at.map(|t|t as i64),state,self.scrub(&record.started_by),phase,record.summary_node.as_deref().map(|n|self.scrub(n)),record.summary_at as i64,record.summary_generation as i64,totals.and_then(|t|t.live_time_raw),totals.and_then(|t|t.add_fans),totals.and_then(|t|t.danmu_num),totals.and_then(|t|t.watched_count),totals.and_then(|t|t.new_fans_club),totals.and_then(|t|t.max_online)],
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

    /// Periodic writer heartbeat: advances open sessions/runs even without samples.
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

const LIVE_RECORD_COLS: &str = "room,live_key,started_at,started_by,ended_at,summary_phase,summary_node,summary_at,summary_generation,bili_live_time_raw,add_fans,danmu_num,watched_count,new_fans_club,max_online,summary_state";

fn live_record_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<BiliLiveRecord> {
    let state: String = row.get(15)?;
    let phase: String = row.get(5)?;
    let summary_phase = match (phase.as_str(), state.as_str()) {
        (_, "ok") => BiliSummaryPhase::Done,
        (_, "skipped") => BiliSummaryPhase::Skipped,
        (_, "failed") => BiliSummaryPhase::Failed,
        ("claimed", _) => BiliSummaryPhase::Claimed,
        _ => BiliSummaryPhase::Pending,
    };
    let totals = if summary_phase == BiliSummaryPhase::Done {
        Some(super::event::BiliTotals {
            live_time_raw: row.get(9)?,
            add_fans: row.get(10)?,
            danmu_num: row.get(11)?,
            watched_count: row.get(12)?,
            new_fans_club: row.get(13)?,
            max_online: row.get(14)?,
        })
    } else {
        None
    };
    Ok(BiliLiveRecord {
        room: row.get::<_, i64>(0)?.max(0) as u64,
        live_key: row.get(1)?,
        started_at: row.get::<_, i64>(2)?.max(0) as u64,
        started_by: row.get(3)?,
        stopped_at: row.get::<_, Option<i64>>(4)?.map(|v| v.max(0) as u64),
        summary_phase,
        summary_node: row.get(6)?,
        summary_at: row.get::<_, i64>(7)?.max(0) as u64,
        summary_generation: row.get::<_, i64>(8)?.max(0) as u64,
        totals,
    })
}

/// Restore a bounded durable book on the writer thread. It never travels
/// through admin read APIs and never returns credential material in errors.
pub(crate) fn load_live_book(
    conn: &Connection,
) -> rusqlite::Result<super::live_book::BiliLiveBook> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {LIVE_RECORD_COLS} FROM bili_lives WHERE room>0 ORDER BY started_at DESC,live_key LIMIT ?1"
    ))?;
    let entries = stmt
        .query_map([super::live_book::MAX_ENTRIES as i64], live_record_row)?
        .map(|r| r.map(super::live_book::BiliLiveMark::from_record))
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(super::live_book::BiliLiveBook { entries })
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
/// open (the room can outlive us). The restored book and claim lease recover
/// pending summary work; restarting must not turn an unfinished fetch into
/// a permanent failure.
pub(crate) fn crash_sweep(
    conn: &Connection,
    node: &str,
    boot_id: i64,
    _now: i64,
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
    Ok(())
}
