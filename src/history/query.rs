//! Read API queries (design §9). Every function reads one node's DB and
//! returns mergeable parts; `merge_*` combine nodes for `scope=cluster`.
//!
//! Caller-supplied values are always bound. Table, column and metric names are
//! interpolated from internal constants or the allowlisted `METRICS` ids (see
//! `stats`), never from request text — keep it that way when adding a query.

use super::event::{metric_id, EndReason, METRICS};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap};

const DAY: i64 = 86_400;
pub(crate) const MAX_POINTS: usize = 2000;
pub(crate) const MAX_RANGE_DAYS: u64 = 90;
/// Upper bound on the returned spectrum length. Hitting it used to stop the
/// series silently while `kpis` still totalled the whole range; the response now
/// carries `buckets_truncated` so a caller can tell a short series from a
/// truncated one.
pub(crate) const MAX_BUCKETS: usize = 400;
const DEFAULT_POINTS: usize = 1000;

/// Half-open overlap between a relay session and the queried window.
///
/// `?1` is `from`, `?2` is `to`, `?3` is `now`; a row without `ended_at` counts
/// as still running at `now`. The list, KPI and calendar paths share this one
/// clause: they used to spell it four ways with `>` and `>=` mixed, so a
/// session starting exactly at `from` could appear in the KPIs but not in the
/// list (and vice versa).
const SESSION_OVERLAP: &str = "started_at<?2 AND coalesce(ended_at, ?3)>?1";

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Query {
    pub from: i64,
    pub to: i64,
    #[serde(default)]
    pub bucket: Option<String>,
    #[serde(default)]
    pub tz_offset_min: i64,
    #[serde(default)]
    pub calendar: bool,
    #[serde(default)]
    pub channel: Option<String>,
    #[serde(default)]
    pub platform: Option<String>,
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub limit: usize,
    #[serde(default)]
    pub cursor: Option<(i64, String)>,
    #[serde(default)]
    pub kinds: Vec<String>,
    #[serde(default)]
    pub severities: Vec<String>,
    #[serde(default)]
    pub subject: Option<String>,
    #[serde(default)]
    pub metrics: Vec<String>,
    #[serde(default)]
    pub session: Option<String>,
    #[serde(default)]
    pub max_points: usize,
    /// `raw` or `1m`, chosen once by the coordinating node.
    #[serde(default)]
    pub tier: Option<String>,
    /// Neighbour lookup around a session `(started_at, eid)`.
    #[serde(default)]
    pub around: Option<(i64, String)>,
    /// Design §11: the requested range was cut to `rollup_days`. Set by
    /// `parse` only; never sent to or read from a peer.
    #[serde(skip)]
    pub clamped_to_days: Option<u64>,
    /// Node-local read limits must not change the coordinator's chart grid,
    /// summary buckets or the preceding comparison period.
    #[serde(skip)]
    pub query_from: Option<i64>,
    #[serde(skip)]
    pub retained_from: Option<i64>,
    /// First complete minute covered by raw retention on this node. Older
    /// intervals use rollups, even when the coordinator requested raw data.
    #[serde(skip)]
    pub raw_from: Option<i64>,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct BadRequest(pub String);

fn int(params: &HashMap<String, String>, key: &str) -> Result<Option<i64>, BadRequest> {
    match params.get(key).map(|v| v.trim()).filter(|v| !v.is_empty()) {
        None => Ok(None),
        Some(v) => v
            .parse::<i64>()
            .map(Some)
            .map_err(|_| BadRequest(format!("{key} 参数无效"))),
    }
}

fn list(params: &HashMap<String, String>, key: &str) -> Vec<String> {
    params
        .get(key)
        .map(|v| {
            v.split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

pub(crate) fn encode_cursor(at: i64, eid: &str) -> String {
    format!("{at}~{eid}")
}

fn decode_cursor(raw: &str) -> Option<(i64, String)> {
    let (at, eid) = raw.split_once('~')?;
    let at = at.parse().ok()?;
    (!eid.is_empty() && eid.len() <= 200).then(|| (at, eid.to_string()))
}

/// Validates and clamps the common params. `rollup_days` bounds the range.
pub(crate) fn parse(
    params: &HashMap<String, String>,
    now: i64,
    rollup_days: u64,
    default_limit: usize,
) -> Result<Query, BadRequest> {
    let to = int(params, "to")?.unwrap_or(now).min(now + 60);
    let mut from = int(params, "from")?.unwrap_or(to - DAY);
    if from >= to {
        return Err(BadRequest("from 必须早于 to".into()));
    }
    let floor = to - rollup_days.max(1) as i64 * DAY;
    let clamped_to_days = (from < floor).then_some(rollup_days.max(1));
    from = from.max(floor);
    let tz_offset_min = int(params, "tz_offset_min")?.unwrap_or(0);
    if !(-720..=840).contains(&tz_offset_min) {
        return Err(BadRequest("tz_offset_min 超出范围".into()));
    }
    let bucket = params.get("bucket").filter(|b| !b.is_empty()).cloned();
    if let Some(b) = &bucket {
        if !matches!(b.as_str(), "hour" | "6h" | "day") {
            return Err(BadRequest("bucket 参数无效".into()));
        }
    }
    let platform = params.get("platform").filter(|p| !p.is_empty()).cloned();
    if let Some(p) = &platform {
        if !matches!(p.as_str(), "YT" | "TW" | "NC") {
            return Err(BadRequest("platform 参数无效".into()));
        }
    }
    let limit = int(params, "limit")?
        .map(|l| l.clamp(1, 500) as usize)
        .unwrap_or(default_limit);
    let cursor = match params.get("cursor").filter(|c| !c.is_empty()) {
        None => None,
        Some(raw) => Some(decode_cursor(raw).ok_or_else(|| BadRequest("cursor 无效".into()))?),
    };
    let metrics = list(params, "metrics");
    if let Some(bad) = metrics.iter().find(|m| metric_id(m).is_none()) {
        return Err(BadRequest(format!(
            "未知指标 {}",
            crate::logging::safe_text(bad, 20)
        )));
    }
    let max_points = int(params, "max_points")?
        .map(|p| p.clamp(2, MAX_POINTS as i64) as usize)
        .unwrap_or(DEFAULT_POINTS);
    let text = |key: &str| {
        params
            .get(key)
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty() && v.len() <= 200)
    };
    Ok(Query {
        from,
        to,
        bucket,
        tz_offset_min,
        calendar: params.get("calendar").is_some_and(|v| v == "1"),
        channel: text("channel"),
        platform,
        id: text("id"),
        limit,
        cursor,
        kinds: list(params, "kind"),
        severities: list(params, "severity"),
        subject: text("subject"),
        metrics,
        session: text("session"),
        max_points,
        tier: None,
        around: None,
        clamped_to_days,
        query_from: None,
        retained_from: None,
        raw_from: None,
    })
}

/// Design §11 (e3797697): a peer whose own `rollup_days` is smaller than the
/// coordinator's cuts the range to it and reports how far it clamped.
pub(crate) fn clamp_peer(q: &mut Query, rollup_days: u64) -> Option<u64> {
    let days = rollup_days.max(1);
    let floor = q.to - days as i64 * DAY;
    q.query_from.get_or_insert(q.from);
    q.retained_from = Some(floor);
    (q.from < floor).then(|| {
        q.from = floor;
        days
    })
}

/// Design §11: the signed peer route accepts only a query the coordinating
/// node could have built with `parse` (plus `tier`/`around`). Anything else
/// is a 400 before the database is opened.
pub(crate) fn validate_peer(q: &Query, now: i64) -> Result<(), BadRequest> {
    let bad = |m: &str| Err(BadRequest(m.into()));
    if q.from >= q.to || q.to > now + 600 || q.to.checked_sub(q.from).is_none() {
        return bad("range");
    }
    if !(-720..=840).contains(&q.tz_offset_min) {
        return bad("tz_offset_min");
    }
    if q.bucket
        .as_deref()
        .is_some_and(|b| !matches!(b, "hour" | "6h" | "day"))
    {
        return bad("bucket");
    }
    if q.platform
        .as_deref()
        .is_some_and(|p| !matches!(p, "YT" | "TW" | "NC"))
    {
        return bad("platform");
    }
    if q.tier
        .as_deref()
        .is_some_and(|t| !matches!(t, "raw" | "1m"))
    {
        return bad("tier");
    }
    if !(1..=500).contains(&q.limit) || !(2..=MAX_POINTS).contains(&q.max_points) {
        return bad("limit");
    }
    if q.metrics.iter().any(|m| metric_id(m).is_none()) || q.metrics.len() > 32 {
        return bad("metrics");
    }
    if q.kinds.len() > 32 || q.severities.len() > 8 {
        return bad("filters");
    }
    let long = |v: &Option<String>| v.as_ref().is_some_and(|v| v.is_empty() || v.len() > 200);
    if long(&q.channel) || long(&q.id) || long(&q.subject) || long(&q.session) {
        return bad("text");
    }
    let long_list = |v: &[String]| v.iter().any(|s| s.is_empty() || s.len() > 200);
    if long_list(&q.kinds) || long_list(&q.severities) {
        return bad("filters");
    }
    let bad_cursor = |c: &Option<(i64, String)>| {
        c.as_ref()
            .is_some_and(|(_, eid)| eid.is_empty() || eid.len() > 200)
    };
    if bad_cursor(&q.cursor) || bad_cursor(&q.around) {
        return bad("cursor");
    }
    Ok(())
}

/// Raw only if the whole range is inside raw retention and ≤ 3 days.
pub(crate) fn choose_tier(from: i64, to: i64, now: i64, raw_days: u64) -> &'static str {
    if to - from <= 3 * DAY && from >= now - raw_days as i64 * DAY {
        "raw"
    } else {
        "1m"
    }
}

// ---------------------------------------------------------------- sessions

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct BiliItem {
    pub state: String,
    pub watched: Option<i64>,
    #[serde(default)]
    pub watched_source: Option<String>,
    pub online_peak: Option<f64>,
    pub danmu: Option<i64>,
    pub add_fans: Option<i64>,
    pub new_fans_club: Option<i64>,
    pub max_online: Option<i64>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct SessionItem {
    pub id: String,
    pub eid: String,
    pub node: String,
    pub platform: String,
    pub channel_id: String,
    pub channel_name: String,
    pub title: Option<String>,
    #[serde(default)]
    pub area_v2: Option<u64>,
    pub started_at: i64,
    pub ended_at: Option<i64>,
    pub duration_secs: i64,
    pub end_reason: Option<String>,
    pub runs: i64,
    pub restarts: i64,
    pub failures: i64,
    pub stuck: i64,
    pub gaps: Vec<(i64, i64, String)>,
    pub median_bitrate_kbps: Option<f64>,
    /// Public source broadcast id (YouTube video, Twitch stream, Niconico program).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_id: Option<String>,
    /// Audience 场次. `live:` plus a hash of the Bilibili live, otherwise
    /// `src:{platform}:{source_id}`, otherwise `session:{eid}`. Never the raw live key.
    #[serde(default)]
    pub broadcast_id: String,
    pub bili: Option<BiliItem>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct SessionsPage {
    pub items: Vec<SessionItem>,
    pub prev_id: Option<String>,
    pub next_id: Option<String>,
    pub next_cursor: Option<String>,
    pub missing_nodes: Vec<super::MissingNode>,
    /// Peer-internal: neighbours around `Query::around`, and has-more.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub neighbours: Option<Neighbours>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub more: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct Neighbours {
    pub prev: Option<(i64, String)>,
    pub next: Option<(i64, String)>,
}

struct Row {
    id: i64,
    eid: String,
    node: String,
    platform: String,
    channel_id: String,
    channel_name: String,
    title: Option<String>,
    started_at: i64,
    ended_at: Option<i64>,
    end_reason: Option<String>,
    runs: i64,
    failures: i64,
    stuck: i64,
    live_key: Option<String>,
    area_v2: Option<u64>,
    source_id: Option<String>,
}

const SESSION_COLS: &str = "id,eid,node,platform,channel_id,channel_name,title,started_at,ended_at,end_reason,runs,failures,stuck,live_key,area_v2,source_id";

fn row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Row> {
    Ok(Row {
        id: r.get(0)?,
        eid: r.get(1)?,
        node: r.get(2)?,
        platform: r.get(3)?,
        channel_id: r.get(4)?,
        channel_name: r.get(5)?,
        title: r.get(6)?,
        started_at: r.get(7)?,
        ended_at: r.get(8)?,
        end_reason: r.get(9)?,
        runs: r.get(10)?,
        failures: r.get(11)?,
        stuck: r.get(12)?,
        live_key: r.get(13)?,
        area_v2: r.get(14)?,
        source_id: r.get(15)?,
    })
}

fn broadcast_id(
    platform: &str,
    live_key: &Option<String>,
    source_id: &Option<String>,
    eid: &str,
) -> String {
    if let Some(key) = live_key.as_deref().filter(|key| !key.is_empty()) {
        return format!("live:{}", key_hash(key));
    }
    if let Some(id) = source_id.as_deref().filter(|id| !id.is_empty()) {
        let platform = match platform {
            "TW" | "NC" => platform,
            _ => "YT",
        };
        return format!("src:{platform}:{id}");
    }
    format!("session:{eid}")
}

struct RunRow {
    started_at: i64,
    ended_at: Option<i64>,
    success: Option<bool>,
    counted_failure: bool,
    diagnostic: Option<String>,
}

fn runs_of(conn: &Connection, session: i64) -> rusqlite::Result<Vec<RunRow>> {
    let mut stmt = conn.prepare_cached(
        "SELECT started_at,ended_at,success,coalesce(counted_failure,0),diagnostic FROM runs WHERE session_id=?1 ORDER BY started_at,id",
    )?;
    let rows = stmt.query_map([session], |r| {
        Ok(RunRow {
            started_at: r.get(0)?,
            ended_at: r.get(1)?,
            success: r.get(2)?,
            counted_failure: r.get::<_, i64>(3)? != 0,
            diagnostic: r.get(4)?,
        })
    })?;
    rows.collect()
}

/// Every run of every relay session the window selects, in one pass.
///
/// `load_sessions` used to call [`runs_of`] once per row, which is an N+1 read
/// on the summary path. The subquery repeats the shared overlap clause, so the
/// run set is exactly the one the caller would have collected row by row, and
/// the ordering (`started_at, id`) matches `runs_of`.
///
/// A subquery rather than a join: the shared clause names `started_at` and
/// `ended_at` unqualified, and both `runs` and `sessions` carry those columns.
fn runs_for_range(
    conn: &Connection,
    from: i64,
    to: i64,
    now: i64,
) -> rusqlite::Result<HashMap<i64, Vec<RunRow>>> {
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT r.session_id,r.started_at,r.ended_at,r.success,coalesce(r.counted_failure,0),r.diagnostic
         FROM runs r WHERE r.session_id IN (
             SELECT id FROM sessions WHERE pruned=0 AND {SESSION_OVERLAP}
         )
         ORDER BY r.session_id,r.started_at,r.id"
    ))?;
    let rows = stmt.query_map(params![from, to, now], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            RunRow {
                started_at: r.get(1)?,
                ended_at: r.get(2)?,
                success: r.get(3)?,
                counted_failure: r.get::<_, i64>(4)? != 0,
                diagnostic: r.get(5)?,
            },
        ))
    })?;
    let mut out: HashMap<i64, Vec<RunRow>> = HashMap::new();
    for row in rows {
        let (session_id, run) = row?;
        out.entry(session_id).or_default().push(run);
    }
    Ok(out)
}

/// Gaps between runs: after a counted failure → failed, else reconnecting.
fn gaps(runs: &[RunRow], session_end: Option<i64>) -> Vec<(i64, i64, String)> {
    let mut out = Vec::new();
    for (i, run) in runs.iter().enumerate() {
        let Some(end) = run.ended_at else { continue };
        let next = runs
            .get(i + 1)
            .map(|n| n.started_at)
            .or(if run.counted_failure {
                session_end
            } else {
                None
            });
        if let Some(next) = next {
            if next > end {
                let kind = if run.counted_failure {
                    "failed"
                } else {
                    "reconnecting"
                };
                out.push((end, next, kind.to_string()));
            }
        }
        if out.len() >= 50 {
            break;
        }
    }
    out
}

fn median_bitrate(
    conn: &Connection,
    session: i64,
    from: i64,
    to: i64,
) -> rusqlite::Result<Option<f64>> {
    let mut values: Vec<f64> = Vec::new();
    for (table, col) in [("samples_raw", "ts"), ("samples_1m", "bucket")] {
        let sql = if table == "samples_raw" {
            "SELECT b_sum/b_count FROM sample_frames WHERE session_id=?1 AND b_count>0 AND ts>=?2 AND ts<=?3".to_string()
        } else {
            format!("SELECT sum/count FROM {table} WHERE metric=1 AND session_id=?1 AND count>0 AND {col}>=?2 AND {col}<=?3")
        };
        let mut stmt = conn.prepare_cached(&sql)?;
        values = stmt
            .query_map(params![session, from.div_euclid(60) * 60, to], |r| {
                r.get::<_, f64>(0)
            })?
            .collect::<rusqlite::Result<_>>()?;
        if !values.is_empty() {
            break;
        }
    }
    if values.is_empty() {
        return Ok(None);
    }
    values.sort_by(|a, b| a.total_cmp(b));
    let mid = values.len() / 2;
    let m = if values.len().is_multiple_of(2) {
        (values[mid - 1] + values[mid]) / 2.0
    } else {
        values[mid]
    };
    Ok(Some(m.round()))
}

fn online_peak(
    conn: &Connection,
    session: i64,
    from: i64,
    to: i64,
) -> rusqlite::Result<Option<f64>> {
    conn.query_row(
        "SELECT max(m) FROM (SELECT max(coalesce(o_max,o_sum/o_count)) AS m FROM sample_frames WHERE o_count IS NOT NULL AND session_id=?1 AND ts>=?2 AND ts<?3
         UNION ALL SELECT max(max) FROM samples_1m WHERE metric=10 AND session_id=?1 AND bucket>=?2 AND bucket<?3)",
        params![session, from, to],
        |r| r.get(0),
    )
}

pub(crate) struct LiveRow {
    pub state: String,
    pub watched: Option<i64>,
    pub danmu: Option<i64>,
    pub add_fans: Option<i64>,
    pub new_fans_club: Option<i64>,
    pub max_online: Option<i64>,
}

pub(crate) fn live_row(conn: &Connection, key: &str) -> rusqlite::Result<Option<LiveRow>> {
    conn.query_row(
        "SELECT summary_phase,watched_count,danmu_num,add_fans,new_fans_club,max_online FROM bili_lives WHERE live_key=?1",
        [key],
        |r| {
            // The stored phase is the authority; this is the only place the
            // read-side vocabulary (`ok`, and `claimed` read as still pending)
            // survives, so `kpis` and the WebUI payload keep their values.
            let phase: String = r.get(0)?;
            Ok(LiveRow {
                state: match phase.as_str() {
                    "done" => "ok".into(),
                    "claimed" => "pending".into(),
                    other => other.to_string(),
                },
                watched: r.get(1)?,
                danmu: r.get(2)?,
                add_fans: r.get(3)?,
                new_fans_club: r.get(4)?,
                max_online: r.get(5)?,
            })
        },
    )
    .optional()
}

fn bili_item(conn: &Connection, row: &Row, now: i64) -> rusqlite::Result<BiliItem> {
    // The first minute bucket may begin before the session itself; session
    // identity makes it safe to include that whole bucket for this detail.
    let online_peak = online_peak(
        conn,
        row.id,
        row.started_at.div_euclid(60) * 60,
        row.ended_at.unwrap_or(now).saturating_add(1),
    )?;
    let mut item = BiliItem {
        state: if row.ended_at.is_none() {
            "pending"
        } else {
            "skipped"
        }
        .into(),
        online_peak,
        ..Default::default()
    };
    if let Some(key) = &row.live_key {
        match live_row(conn, key)? {
            Some(live) => {
                item.state = live.state.clone();
                // Totals only for a valid summary; NULL otherwise.
                if live.state == "ok" {
                    item.watched = live.watched;
                    item.watched_source = live.watched.map(|_| "summary".into());
                    item.danmu = live.danmu;
                    item.add_fans = live.add_fans;
                    item.new_fans_club = live.new_fans_club;
                    item.max_online = live.max_online;
                }
            }
            None => item.state = "pending".into(),
        }
    }
    if item.watched.is_none() {
        item.watched = watched_sample(conn, row.id)?.map(|(_, value)| value);
        item.watched_source = item.watched.map(|_| "sample".into());
    }
    Ok(item)
}

fn watched_sample(conn: &Connection, session: i64) -> rusqlite::Result<Option<(i64, i64)>> {
    conn.query_row("SELECT watched_at,watched_count FROM sessions WHERE id=?1 AND watched_at IS NOT NULL AND watched_count IS NOT NULL",
        [session],|r|Ok((r.get(0)?,r.get(1)?))).optional()
}

fn item(conn: &Connection, row: Row, now: i64) -> rusqlite::Result<SessionItem> {
    let runs = runs_of(conn, row.id)?;
    let bili = Some(bili_item(conn, &row, now)?);
    let broadcast_id = broadcast_id(&row.platform, &row.live_key, &row.source_id, &row.eid);
    Ok(SessionItem {
        id: row.eid.clone(),
        gaps: gaps(&runs, row.ended_at),
        median_bitrate_kbps: median_bitrate(
            conn,
            row.id,
            row.started_at,
            row.ended_at.unwrap_or(now),
        )?,
        duration_secs: (row.ended_at.unwrap_or(now) - row.started_at).max(0),
        end_reason: row
            .end_reason
            .filter(|_| row.ended_at.is_some())
            .map(|r| EndReason::parse(&r).map_or(r, |r| r.as_str().to_string())),
        restarts: (row.runs - 1).max(0),
        eid: row.eid,
        node: row.node,
        platform: row.platform,
        channel_id: row.channel_id,
        channel_name: row.channel_name,
        title: row.title,
        area_v2: row.area_v2,
        started_at: row.started_at,
        ended_at: row.ended_at,
        runs: row.runs,
        failures: row.failures,
        stuck: row.stuck,
        source_id: row.source_id,
        broadcast_id,
        bili,
    })
}

fn neighbour(
    conn: &Connection,
    at: i64,
    eid: &str,
    older: bool,
) -> rusqlite::Result<Option<(i64, String)>> {
    let sql = if older {
        "SELECT started_at,eid FROM sessions WHERE pruned=0 AND (started_at<?1 OR (started_at=?1 AND eid<?2)) ORDER BY started_at DESC, eid DESC LIMIT 1"
    } else {
        "SELECT started_at,eid FROM sessions WHERE pruned=0 AND (started_at>?1 OR (started_at=?1 AND eid>?2)) ORDER BY started_at, eid LIMIT 1"
    };
    conn.query_row(sql, params![at, eid], |r| Ok((r.get(0)?, r.get(1)?)))
        .optional()
}

pub(crate) fn sessions(conn: &Connection, q: &Query, now: i64) -> rusqlite::Result<SessionsPage> {
    if let Some((at, eid)) = &q.around {
        return Ok(SessionsPage {
            neighbours: Some(Neighbours {
                prev: neighbour(conn, *at, eid, true)?,
                next: neighbour(conn, *at, eid, false)?,
            }),
            ..Default::default()
        });
    }
    if let Some(id) = &q.id {
        let found = conn
            .query_row(
                &format!("SELECT {SESSION_COLS} FROM sessions WHERE eid=?1 AND pruned=0"),
                [id],
                row,
            )
            .optional()?;
        let Some(found) = found else {
            return Ok(SessionsPage::default());
        };
        let (at, eid) = (found.started_at, found.eid.clone());
        let prev = neighbour(conn, at, &eid, true)?;
        let next = neighbour(conn, at, &eid, false)?;
        return Ok(SessionsPage {
            items: vec![item(conn, found, now)?],
            prev_id: prev.as_ref().map(|p| p.1.clone()),
            next_id: next.as_ref().map(|n| n.1.clone()),
            neighbours: Some(Neighbours { prev, next }),
            ..Default::default()
        });
    }
    let (cur_at, cur_eid) = q.cursor.clone().unwrap_or((i64::MAX, String::new()));
    let has_cursor = q.cursor.is_some();
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT {SESSION_COLS} FROM sessions
         WHERE pruned=0 AND {SESSION_OVERLAP}
           AND (?4 IS NULL OR platform=?4) AND (?5 IS NULL OR channel_id=?5)
           AND (?7=0 OR started_at<?8 OR (started_at=?8 AND eid<?9))
         ORDER BY started_at DESC, eid DESC LIMIT ?6"
    ))?;
    let rows: Vec<Row> = stmt
        .query_map(
            params![
                q.from,
                q.to,
                now,
                q.platform,
                q.channel,
                (q.limit + 1) as i64,
                has_cursor as i64,
                cur_at,
                cur_eid
            ],
            row,
        )?
        .collect::<rusqlite::Result<_>>()?;
    let more = rows.len() > q.limit;
    let mut items = Vec::new();
    for r in rows.into_iter().take(q.limit) {
        items.push(item(conn, r, now)?);
    }
    let next_cursor = more
        .then(|| items.last().map(|s| encode_cursor(s.started_at, &s.eid)))
        .flatten();
    Ok(SessionsPage {
        items,
        next_cursor,
        more,
        ..Default::default()
    })
}

/// Lists: union by eid, newest first, `limit` after the merge.
pub(crate) fn merge_sessions(pages: Vec<SessionsPage>, limit: usize) -> SessionsPage {
    let mut by_eid: BTreeMap<String, SessionItem> = BTreeMap::new();
    let mut more = false;
    let mut prev: Option<(i64, String)> = None;
    let mut next: Option<(i64, String)> = None;
    for page in pages {
        more |= page.more;
        if let Some(n) = page.neighbours {
            if let Some(p) = n.prev {
                if prev.as_ref().is_none_or(|cur| p > *cur) {
                    prev = Some(p);
                }
            }
            if let Some(x) = n.next {
                if next.as_ref().is_none_or(|cur| x < *cur) {
                    next = Some(x);
                }
            }
        }
        for item in page.items {
            by_eid.entry(item.eid.clone()).or_insert(item);
        }
    }
    let mut items: Vec<SessionItem> = by_eid.into_values().collect();
    items.sort_by(|a, b| (b.started_at, &b.eid).cmp(&(a.started_at, &a.eid)));
    more |= items.len() > limit;
    items.truncate(limit);
    let next_cursor = more
        .then(|| items.last().map(|s| encode_cursor(s.started_at, &s.eid)))
        .flatten();
    SessionsPage {
        items,
        prev_id: prev.map(|p| p.1),
        next_id: next.map(|n| n.1),
        next_cursor,
        ..Default::default()
    }
}

// ------------------------------------------------------------------ events

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct EventItem {
    pub id: String,
    pub eid: String,
    pub at: i64,
    pub node: String,
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    pub title: String,
    pub detail: String,
    pub severity: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notify: Option<NotifyAttempt>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct NotifyAttempt {
    pub event_key: String,
    pub dedup_key: String,
    pub phase: String,
    pub mode: Option<String>,
    pub ok: bool,
    pub code: Option<i64>,
    pub trace_id: Option<String>,
    pub test: bool,
    pub suppressed: u32,
    pub undelivered: u32,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct EventsPage {
    pub items: Vec<EventItem>,
    pub next_cursor: Option<String>,
    pub missing_nodes: Vec<super::MissingNode>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub more: bool,
}

pub(crate) fn end_reason_label(code: &str) -> &'static str {
    match code {
        "source_ended" => "正常结束（源已下播）",
        "bili_stopped" => "正常结束（B站 已下播）",
        "user_stop" => "正常结束（手动停止）",
        "intentional_restart" => "计划内重启",
        "cluster_handoff" => "节点交接（不计入）",
        "process_restart" => "异常退出（进程重启）",
        "aborted" => "异常退出（中断）",
        _ => "其他",
    }
}

fn kind_matches(kinds: &[String], kind: &str) -> bool {
    kinds.is_empty()
        || kinds.iter().any(|k| {
            kind == k
                || kind
                    .strip_prefix(k.as_str())
                    .is_some_and(|r| r.starts_with('.'))
        })
}

pub(crate) fn events(conn: &Connection, q: &Query) -> rusqlite::Result<EventsPage> {
    let mut all: Vec<EventItem> = Vec::new();
    // Stored events.
    {
        let mut stmt = conn.prepare_cached(
            "SELECT e.eid,e.at,e.node,e.kind,s.eid,e.title,e.detail,e.severity,e.subject FROM events e
             LEFT JOIN sessions s ON s.id=e.session_id WHERE e.at>=?1 AND e.at<=?2",
        )?;
        let rows = stmt.query_map(params![q.from, q.to], |r| {
            Ok((
                EventItem {
                    id: r.get(0)?,
                    eid: r.get(0)?,
                    at: r.get(1)?,
                    node: r.get(2)?,
                    kind: r.get(3)?,
                    session_id: r.get(4)?,
                    title: r.get(5)?,
                    detail: r.get(6)?,
                    severity: r.get(7)?,
                    notify: None,
                },
                r.get::<_, Option<String>>(8)?,
            ))
        })?;
        for row in rows {
            let (ev, subject) = row?;
            if q.subject.is_some() && subject != q.subject {
                continue;
            }
            all.push(ev);
        }
    }
    // Relay lifecycle, derived from sessions and runs (not stored twice).
    {
        let mut stmt = conn.prepare_cached(
            "SELECT id,eid,node,channel_id,channel_name,title,started_at,ended_at,end_reason FROM sessions
             WHERE pruned=0 AND ((started_at>=?1 AND started_at<=?2) OR (ended_at>=?1 AND ended_at<=?2)
                OR (started_at<?1 AND (ended_at IS NULL OR ended_at>?2)))",
        )?;
        type S = (
            i64,
            String,
            String,
            String,
            String,
            Option<String>,
            i64,
            Option<i64>,
            Option<String>,
        );
        let rows: Vec<S> = stmt
            .query_map(params![q.from, q.to], |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    r.get(6)?,
                    r.get(7)?,
                    r.get(8)?,
                ))
            })?
            .collect::<rusqlite::Result<_>>()?;
        for (id, eid, node, channel_id, name, title, started, ended, reason) in rows {
            if q.subject
                .as_ref()
                .is_some_and(|s| *s != channel_id && *s != node)
            {
                continue;
            }
            let base =
                |suffix: &str, at: i64, kind: &str, title: String, detail: String, sev: &str| {
                    EventItem {
                        id: format!("{eid}/{suffix}"),
                        eid: format!("{eid}/{suffix}"),
                        at,
                        node: node.clone(),
                        kind: kind.into(),
                        session_id: Some(eid.clone()),
                        title,
                        detail,
                        severity: sev.into(),
                        notify: None,
                    }
                };
            // The third arm above also selects relay sessions that span the
            // whole window; `ended_at IS NULL` keeps a still-running session in
            // that set. Their bookends lie outside `[from,to]` and would be
            // dropped by the retain at the end of this function, so only emit
            // them when they are in range. Their in-window ffmpeg-run events
            // are still reported below, and those are the ones a reader wants
            // for a long-running stream.
            let in_window = |at: i64| at >= q.from && at <= q.to;
            if in_window(started) {
                all.push(base(
                    "start",
                    started,
                    "stream.started",
                    format!("开始转播：{name}"),
                    title.clone().unwrap_or_default(),
                    "info",
                ));
            }
            if let Some(end) = ended.filter(|end| in_window(*end)) {
                let code = reason.clone().unwrap_or_default();
                let bad = matches!(code.as_str(), "aborted" | "process_restart");
                all.push(base(
                    "end",
                    end,
                    "stream.ended",
                    format!("转播结束：{name}"),
                    format!(
                        "时长 {} 分钟 · {}",
                        (end - started).max(0) / 60,
                        end_reason_label(&code)
                    ),
                    if bad { "warn" } else { "info" },
                ));
            }
            let runs = runs_of(conn, id)?;
            for (i, run) in runs.iter().enumerate() {
                let Some(end) = run.ended_at else { continue };
                let last = i + 1 == runs.len();
                if last && !run.counted_failure {
                    continue;
                }
                let detail = run
                    .diagnostic
                    .clone()
                    .unwrap_or_else(|| "ffmpeg 退出".into());
                let ev = if run.counted_failure {
                    base(
                        &format!("run{i}"),
                        end,
                        "stream.failed",
                        format!("转播失败：{name}"),
                        detail,
                        "error",
                    )
                } else {
                    base(
                        &format!("run{i}"),
                        end,
                        "stream.reconnecting",
                        format!("重新连接：{name}"),
                        detail,
                        "warn",
                    )
                };
                all.push(ev);
            }
        }
    }
    // QQ sends.
    {
        let mut stmt = conn.prepare_cached(
            "SELECT eid,at,node,event_key,ok,code,dedup_key,phase,mode,trace_id,test,suppressed,undelivered FROM notify_sends WHERE at>=?1 AND at<=?2",
        )?;
        let rows = stmt.query_map(params![q.from, q.to], |r| {
            let ok: bool = r.get(4)?;
            let code: Option<i64> = r.get(5)?;
            let key: String = r.get(3)?;
            Ok(EventItem {
                id: r.get(0)?,
                eid: r.get(0)?,
                at: r.get(1)?,
                node: r.get(2)?,
                kind: "notify.sent".into(),
                session_id: None,
                title: if ok {
                    format!("QQ 通知已送达：{key}")
                } else {
                    format!(
                        "QQ 通知未送达（{}）",
                        code.map_or("--".into(), |c| c.to_string())
                    )
                },
                detail: String::new(),
                severity: if ok { "info" } else { "warn" }.into(),
                notify: Some(NotifyAttempt {
                    event_key: key,
                    dedup_key: r.get(6)?,
                    phase: r.get(7)?,
                    mode: r.get(8)?,
                    ok,
                    code,
                    trace_id: r.get(9)?,
                    test: r.get(10)?,
                    suppressed: r.get(11)?,
                    undelivered: r.get(12)?,
                }),
            })
        })?;
        if q.subject.is_none() {
            for row in rows {
                all.push(row?);
            }
        }
    }
    all.retain(|e| {
        e.at >= q.from
            && e.at <= q.to
            && kind_matches(&q.kinds, &e.kind)
            && (q.severities.is_empty() || q.severities.contains(&e.severity))
            && q.cursor
                .as_ref()
                .is_none_or(|(at, eid)| (e.at, &e.eid) < (*at, eid))
    });
    Ok(page_events(all, q.limit))
}

fn page_events(mut all: Vec<EventItem>, limit: usize) -> EventsPage {
    all.sort_by(|a, b| (b.at, &b.eid).cmp(&(a.at, &a.eid)));
    all.dedup_by(|a, b| a.eid == b.eid);
    let more = all.len() > limit;
    all.truncate(limit);
    let next_cursor = more
        .then(|| all.last().map(|e| encode_cursor(e.at, &e.eid)))
        .flatten();
    EventsPage {
        items: all,
        next_cursor,
        more,
        ..Default::default()
    }
}

pub(crate) fn merge_events(pages: Vec<EventsPage>, limit: usize) -> EventsPage {
    let mut more = false;
    let mut all = Vec::new();
    for page in pages {
        more |= page.more;
        all.extend(page.items);
    }
    let mut merged = page_events(all, limit);
    if more && merged.next_cursor.is_none() {
        merged.next_cursor = merged.items.last().map(|e| encode_cursor(e.at, &e.eid));
    }
    merged.more = false;
    merged
}

// ------------------------------------------------------------------- stats

/// One bucket: (min, max, sum, count).
pub(crate) type Cell = (f64, f64, f64, i64);

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct StatsParts {
    pub resolution: String,
    pub start: i64,
    pub step: i64,
    pub n: usize,
    /// metric → per bucket (min, max, sum, count).
    pub series: BTreeMap<String, Vec<Option<Cell>>>,
    #[serde(default)]
    pub partial_before: Option<i64>,
}

pub(crate) fn stats_layout(from: i64, to: i64, tier: &str, max_points: usize) -> (i64, usize) {
    let base = if tier == "raw" { 10 } else { 60 };
    let points = max_points.clamp(2, MAX_POINTS) as i64;
    let span = to.saturating_sub(from).max(1);
    let step = base.max(span.saturating_add(points - 2) / (points - 1));
    let n = (span / step + 1).min(points) as usize;
    (step, n)
}

pub(crate) fn empty_stats(q: &Query, tier: &str) -> StatsParts {
    let start = q.query_from.unwrap_or(q.from);
    let (step, n) = stats_layout(start, q.to, tier, q.max_points);
    let metrics: Vec<String> = if q.metrics.is_empty() {
        super::event::METRICS
            .iter()
            .map(|(name, _)| name.to_string())
            .collect()
    } else {
        q.metrics.clone()
    };
    StatsParts {
        resolution: tier.into(),
        start,
        step,
        n,
        series: metrics
            .into_iter()
            .map(|name| (name, vec![None; n]))
            .collect(),
        partial_before: None,
    }
}

/// Old peers may send an absent store as a default part. It has no grid
/// and must never become the layout for populated nodes.
pub(crate) fn stats_matches_query(p: &StatsParts, q: &Query) -> bool {
    if p.n == 0 {
        return p.series.is_empty();
    }
    let tier = q.tier.as_deref().unwrap_or("raw");
    let start = q.query_from.unwrap_or(q.from);
    let (step, n) = stats_layout(start, q.to, tier, q.max_points);
    matches!(p.resolution.as_str(), "raw" | "1m")
        && p.start == start
        && p.step == step
        && p.n == n
        && p.series.values().all(|v| v.len() == n)
}

pub(crate) fn stats(
    conn: &Connection,
    q: &Query,
    tier: &str,
    node: Option<&str>,
) -> rusqlite::Result<StatsParts> {
    let mut out = empty_stats(q, tier);
    out.partial_before = super::maintain::watermark(conn, "samples_pruned_before")?;
    if q.session.is_some() {
        out.partial_before = out.partial_before.max(
            super::maintain::watermark(conn, "legacy_unattributed_before")?.filter(|v| *v > 0),
        );
    }
    let stored_raw_floor = super::maintain::watermark(conn, "raw_pruned_before")?
        .map(|v| (v + 59).div_euclid(60) * 60);
    let ranges = if tier == "raw" {
        let raw_from = q
            .raw_from
            .unwrap_or(q.from)
            .max(stored_raw_floor.unwrap_or(q.from))
            .max(q.from)
            .min(q.to);
        if raw_from > q.from {
            out.resolution = "1m".into();
        }
        vec![
            ("samples_1m", "bucket", q.from, raw_from),
            ("samples_raw", "ts", raw_from, q.to),
        ]
    } else {
        let wm = super::maintain::watermark(conn, "rollup_1m_watermark")?
            .unwrap_or(conn.query_row(
                "SELECT coalesce(max(bucket)+60,?1) FROM samples_1m",
                [q.from],
                |r| r.get(0),
            )?)
            .max(q.from)
            .min(q.to);
        vec![
            ("samples_1m", "bucket", q.from, wm),
            ("samples_raw", "ts", wm, q.to),
        ]
    };
    // Scan each retained interval once and return only the requested chart
    // cells. Pulling every metric row across the SQLite boundary scales with
    // months of samples even when the caller asks for a thousand points.
    let metrics = out
        .series
        .keys()
        .filter_map(|name| metric_id(name))
        .map(|id| id.to_string())
        .collect::<Vec<_>>()
        .join(",");
    if metrics.is_empty() {
        return Ok(out);
    }
    for &(table, col, from, to) in &ranges {
        if from >= to {
            continue;
        }
        let mut stmt = conn.prepare_cached(&format!(
            "SELECT metric,({col}-?5)/?6 AS slot,min(min),max(max),sum(sum),sum(count)
             FROM {table} WHERE metric IN ({metrics}) AND {col}>=?1 AND {col}<?2 AND (?3 IS NULL OR node=?3)
             AND (?4 IS NULL OR session_id=(SELECT id FROM sessions WHERE eid=?4 AND pruned=0))
             GROUP BY metric,slot"
        ))?;
        let rows = stmt.query_map(
            params![from, to, node, q.session, out.start, out.step],
            |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, f64>(2)?,
                    r.get::<_, f64>(3)?,
                    r.get::<_, f64>(4)?,
                    r.get::<_, i64>(5)?,
                ))
            },
        )?;
        for row in rows {
            let (metric, slot, min, max, sum, count) = row?;
            let Some((name, _)) = METRICS.iter().find(|(_, id)| *id == metric) else {
                continue;
            };
            let Some(col_data) = out.series.get_mut(*name) else {
                continue;
            };
            let Ok(i) = usize::try_from(slot) else {
                continue;
            };
            if i >= out.n {
                continue;
            }
            col_data[i] = Some(match col_data[i] {
                None => (min, max, sum, count),
                Some((a, b, c, d)) => (a.min(min), b.max(max), c + sum, d + count),
            });
        }
    }
    Ok(out)
}

pub(crate) fn merge_stats(parts: Vec<StatsParts>) -> Option<StatsParts> {
    let mut iter = parts.into_iter().filter(|p| p.n > 0);
    let mut out = iter.next()?;
    for p in iter {
        if p.step != out.step || p.start != out.start || p.n != out.n {
            // The coordinator validates each source first and reports it in
            // missing_nodes. Never claim a mismatched part was merged.
            return None;
        }
        if p.resolution == "1m" {
            out.resolution = p.resolution;
        }
        out.partial_before = out.partial_before.max(p.partial_before);
        for (name, col) in p.series {
            let mine = out
                .series
                .entry(name)
                .or_insert_with(|| vec![None; col.len()]);
            for (a, b) in mine.iter_mut().zip(col) {
                *a = match (*a, b) {
                    (None, x) | (x, None) => x,
                    (Some((a1, b1, c1, d1)), Some((a2, b2, c2, d2))) => {
                        Some((a1.min(a2), b1.max(b2), c1 + c2, d1 + d2))
                    }
                };
            }
        }
    }
    Some(out)
}

fn round2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

/// uPlot column layout; gaps are null.
pub(crate) fn render_stats(parts: &StatsParts) -> serde_json::Value {
    let ts: Vec<i64> = (0..parts.n)
        .map(|i| parts.start + i as i64 * parts.step)
        .collect();
    let mut series = serde_json::Map::new();
    for (name, col) in &parts.series {
        let pick = |f: &dyn Fn(&Cell) -> f64| -> Vec<Option<f64>> {
            col.iter()
                .map(|c| c.as_ref().filter(|c| c.3 > 0).map(|c| round2(f(c))))
                .collect()
        };
        series.insert(
            name.clone(),
            serde_json::json!({
                "mean": pick(&|c| c.2 / c.3 as f64),
                "min": pick(&|c| c.0),
                "max": pick(&|c| c.1),
            }),
        );
    }
    serde_json::json!({ "resolution": parts.resolution, "ts": ts, "series": series, "coverage": {"partial_before":parts.partial_before} })
}

// ----------------------------------------------------------------- summary

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct LiveTotals {
    pub watched: Option<i64>,
    /// `summary` or `sample` whenever `watched` is set.
    pub watched_source: Option<String>,
    #[serde(default)]
    pub watched_at: Option<i64>,
    pub danmu: Option<i64>,
    pub add_fans: Option<i64>,
    pub new_fans_club: Option<i64>,
    pub max_online: Option<i64>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct KpiParts {
    pub live_secs: i64,
    pub down_secs: i64,
    /// 开播场次: distinct broadcasts started in the range. See `broadcasts`.
    pub sessions: i64,
    /// Broadcast ids started in the range (`live:<hash>`, `src:<platform>:<id>`
    /// or `session:<eid>`). Carried across nodes so one broadcast relayed by
    /// two of them counts once, the same way `lives` dedupes its totals.
    #[serde(default)]
    pub broadcasts: BTreeSet<String>,
    #[serde(default)]
    pub clean_runs: i64,
    pub failures: i64,
    pub recover_sum: i64,
    pub recover_n: i64,
    pub notify_sent: i64,
    pub notify_failed: i64,
    pub online_peak: Option<f64>,
    /// Valid B站 summaries keyed by a short hash of the live key, so a live
    /// session relayed by two nodes counts once. The key itself never leaves.
    pub lives: BTreeMap<String, LiveTotals>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct BucketParts {
    pub start: i64,
    pub live_secs: BTreeMap<String, i64>,
    pub sessions: i64,
    /// Broadcasts started in this bucket, for cross-node dedupe of `sessions`.
    #[serde(default)]
    pub broadcasts: BTreeSet<String>,
    pub failures: i64,
    pub recover_sum: i64,
    pub recover_n: i64,
    pub notify_sent: i64,
    pub notify_failed: i64,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct NodeParts {
    pub node_id: String,
    pub live_secs: i64,
    pub failures: i64,
    pub down_secs: i64,
    pub last_seen_at: Option<i64>,
    pub oldest_at: Option<i64>,
    #[serde(default)]
    pub node_observed_secs: Option<i64>,
    #[serde(default)]
    pub partial_before: Option<i64>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct NodeSegment {
    pub eid: String,
    pub observer: String,
    pub subject: String,
    pub state: String,
    pub reason: String,
    pub started_at: i64,
    pub last_seen_at: i64,
    pub ended_at: Option<i64>,
    pub initial: bool,
    pub minority: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct SummaryParts {
    #[serde(default)]
    pub source: Option<String>,
    pub buckets: Vec<BucketParts>,
    /// True when the spectrum stopped at [`MAX_BUCKETS`] before covering the
    /// requested range; `kpis` still totals the whole range.
    #[serde(default)]
    pub buckets_truncated: bool,
    pub kpis: KpiParts,
    pub prev_kpis: KpiParts,
    pub calendar: Vec<(i64, i64, i64)>,
    pub clean: i64,
    pub planned: i64,
    pub reasons: BTreeMap<String, i64>,
    pub per_node: Vec<NodeParts>,
    #[serde(default)]
    pub node_segments: Vec<NodeSegment>,
}

pub(crate) fn bucket_size(q: &Query) -> i64 {
    match q.bucket.as_deref() {
        Some("hour") => 3600,
        Some("6h") => 6 * 3600,
        Some("day") => DAY,
        _ if q.to - q.query_from.unwrap_or(q.from) <= 2 * DAY => 3600,
        _ if q.to - q.query_from.unwrap_or(q.from) <= 14 * DAY => 6 * 3600,
        _ => DAY,
    }
}

fn align(t: i64, size: i64, off: i64) -> i64 {
    (t + off).div_euclid(size) * size - off
}

fn clip(a: i64, b: i64, from: i64, to: i64) -> i64 {
    (b.min(to) - a.max(from)).max(0)
}

pub(crate) struct SumSession {
    id: i64,
    eid: String,
    platform: String,
    started: i64,
    ended: Option<i64>,
    end_reason: Option<String>,
    runs: Vec<RunRow>,
    fails: Vec<(i64, Option<String>)>,
    live_key: Option<String>,
    /// Stable identity of the broadcast this relay session carried: the same
    /// B站 `live_key`, source video id, or — with neither — the session itself.
    /// Reconnects, restarts and failovers keep the id, so 开播场次 counts
    /// broadcasts instead of ffmpeg pushes.
    broadcast: String,
    online_peak: Option<f64>,
}

pub(crate) fn load_sessions(
    conn: &Connection,
    from: i64,
    to: i64,
    now: i64,
) -> rusqlite::Result<Vec<SumSession>> {
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT {SESSION_COLS} FROM sessions WHERE pruned=0 AND {SESSION_OVERLAP}"
    ))?;
    let rows: Vec<Row> = stmt
        .query_map(params![from, to, now], row)?
        .collect::<rusqlite::Result<_>>()?;
    let mut runs_by_session = runs_for_range(conn, from, to, now)?;
    let mut out = Vec::new();
    for r in rows {
        let runs = runs_by_session.remove(&r.id).unwrap_or_default();
        let broadcast = broadcast_id(&r.platform, &r.live_key, &r.source_id, &r.eid);
        out.push(SumSession {
            broadcast,
            id: r.id,
            eid: r.eid,
            fails: runs
                .iter()
                .filter(|x| x.counted_failure)
                .filter_map(|x| x.ended_at.map(|e| (e, x.diagnostic.clone())))
                .collect(),
            online_peak: online_peak(
                conn,
                r.id,
                if from <= r.started_at {
                    r.started_at.div_euclid(60) * 60
                } else {
                    from
                },
                to.min(r.ended_at.unwrap_or(now).saturating_add(1)),
            )?,
            runs,
            platform: r.platform,
            started: r.started_at,
            ended: r.ended_at,
            end_reason: r.end_reason,
            live_key: r.live_key,
        });
    }
    Ok(out)
}

/// Runs can overlap in an incomplete/crash-recovered record. Count their
/// union inside this session, including the running tail up to now.
fn pushing_secs(s: &SumSession, from: i64, to: i64, now: i64) -> i64 {
    let from = from.max(s.started);
    let to = to.min(s.ended.unwrap_or(now)).min(now);
    let mut through = from;
    let mut total = 0;
    for run in &s.runs {
        let start = run.started_at.max(through);
        let end = run.ended_at.unwrap_or(to).min(to);
        total += (end - start).max(0);
        through = through.max(end);
    }
    total
}

/// A session ending is not a recovery. Only a subsequent run closes a
/// counted-failure recovery interval; do not use the capped display gaps.
fn recoveries(s: &SumSession) -> impl Iterator<Item = (i64, i64)> + '_ {
    s.runs.windows(2).filter_map(|runs| {
        let end = runs[0].ended_at?;
        let next = runs[1].started_at;
        (runs[0].counted_failure && next >= end).then_some((end, next))
    })
}

fn completed_runs(s: &SumSession, from: i64, to: i64) -> (i64, i64) {
    let mut clean = 0;
    let mut planned = 0;
    for (i, run) in s.runs.iter().enumerate() {
        let Some(end) = run.ended_at else { continue };
        if end < from || end >= to || run.counted_failure {
            continue;
        }
        let terminal_planned = i + 1 == s.runs.len()
            && matches!(
                s.end_reason.as_deref(),
                Some("intentional_restart" | "cluster_handoff")
            );
        if terminal_planned {
            planned += 1;
        } else if run.success == Some(true) {
            clean += 1;
        }
    }
    (clean, planned)
}

pub(crate) fn key_hash(key: &str) -> String {
    use sha2::{Digest, Sha256};
    let d = Sha256::digest(key.as_bytes());
    d.iter().take(8).map(|b| format!("{b:02x}")).collect()
}

fn kpis(
    conn: &Connection,
    list: &[SumSession],
    from: i64,
    to: i64,
    now: i64,
) -> rusqlite::Result<KpiParts> {
    let mut k = KpiParts::default();
    for s in list {
        let end = s.ended.unwrap_or(now);
        let duration = clip(s.started, end, from, to);
        k.live_secs += duration;
        k.down_secs += duration - pushing_secs(s, from, to, now);
        if s.started >= from && s.started < to {
            k.broadcasts.insert(s.broadcast.clone());
        }
        k.clean_runs += completed_runs(s, from, to).0;
        k.failures += s
            .fails
            .iter()
            .filter(|(at, _)| *at >= from && *at < to)
            .count() as i64;
        for (a, b) in recoveries(s) {
            if a >= from && a < to {
                k.recover_sum += b - a;
                k.recover_n += 1;
            }
        }
        if let Some(p) = s.online_peak {
            k.online_peak = Some(k.online_peak.map_or(p, |c: f64| c.max(p)));
        }
        let mut totals = LiveTotals::default();
        if let Some(key) = &s.live_key {
            if let Some(live) = live_row(conn, key)? {
                if live.state == "ok" {
                    totals = LiveTotals {
                        watched: live.watched,
                        watched_source: live.watched.map(|_| "summary".into()),
                        watched_at: None,
                        danmu: live.danmu,
                        add_fans: live.add_fans,
                        new_fans_club: live.new_fans_club,
                        max_online: live.max_online,
                    };
                }
            }
        }
        if totals.watched.is_none() {
            if let Some((at, value)) = watched_sample(conn, s.id)? {
                totals.watched = Some(value);
                totals.watched_source = Some("sample".into());
                totals.watched_at = Some(at);
            }
        }
        if totals != LiveTotals::default() {
            let key = s
                .live_key
                .as_ref()
                .map(|key| key_hash(key))
                .unwrap_or_else(|| format!("session:{}", s.eid));
            merge_live(k.lives.entry(key).or_default(), totals);
        }
    }
    let (sent, failed): (i64, i64) = conn.query_row(
        "SELECT count(*), coalesce(sum(1-ok),0) FROM notify_sends WHERE at>=?1 AND at<?2",
        params![from, to],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    k.notify_sent = sent;
    k.notify_failed = failed;
    k.sessions = k.broadcasts.len() as i64;
    Ok(k)
}

pub(crate) fn summary(
    conn: &Connection,
    q: &Query,
    node: &str,
    now: i64,
) -> rusqlite::Result<SummaryParts> {
    let list = load_sessions(conn, q.from, q.to, now)?;
    let size = bucket_size(q);
    let off = q.tz_offset_min * 60;
    let mut buckets = Vec::new();
    let mut start = align(q.from, size, off);
    let mut sends: Vec<(i64, bool)> = Vec::new();
    {
        let mut stmt =
            conn.prepare_cached("SELECT at, ok FROM notify_sends WHERE at>=?1 AND at<?2")?;
        for r in stmt.query_map(params![q.from, q.to], |r| Ok((r.get(0)?, r.get(1)?)))? {
            sends.push(r?);
        }
    }
    while start < q.to && buckets.len() < MAX_BUCKETS {
        let end = start + size;
        let from = start.max(q.from);
        let to = end.min(q.to);
        let mut b = BucketParts {
            start,
            live_secs: ["YT", "TW", "NC"]
                .iter()
                .map(|p| (p.to_string(), 0))
                .collect(),
            ..Default::default()
        };
        for s in &list {
            let d = clip(s.started, s.ended.unwrap_or(now), from, to);
            if d > 0 {
                *b.live_secs.entry(s.platform.clone()).or_default() += d;
            }
            if s.started >= from && s.started < to {
                b.broadcasts.insert(s.broadcast.clone());
            }
            b.failures += s
                .fails
                .iter()
                .filter(|(at, _)| *at >= from && *at < to)
                .count() as i64;
            for (a, z) in recoveries(s) {
                if a >= from && a < to {
                    b.recover_sum += z - a;
                    b.recover_n += 1;
                }
            }
        }
        for (at, ok) in &sends {
            if *at >= start && *at < end {
                b.notify_sent += 1;
                if !ok {
                    b.notify_failed += 1;
                }
            }
        }
        b.sessions = b.broadcasts.len() as i64;
        buckets.push(b);
        start = end;
    }
    let k = kpis(conn, &list, q.from, q.to, now)?;
    let prev_to = q.query_from.unwrap_or(q.from);
    let span = q.to - prev_to;
    let prev_from = (prev_to - span).max(q.retained_from.unwrap_or(i64::MIN));
    let prev_list = if prev_from < prev_to {
        load_sessions(conn, prev_from, prev_to, now)?
    } else {
        Vec::new()
    };
    let prev_kpis = kpis(conn, &prev_list, prev_from, prev_to, now)?;
    let mut reasons: BTreeMap<String, i64> = BTreeMap::new();
    for s in &list {
        for (at, diag) in &s.fails {
            if *at >= q.from && *at < q.to {
                *reasons
                    .entry(diag.clone().unwrap_or_else(|| "unknown".into()))
                    .or_default() += 1;
            }
        }
    }
    let clean = k.clean_runs;
    let planned = list.iter().map(|s| completed_runs(s, q.from, q.to).1).sum();
    let mut calendar = Vec::new();
    if q.calendar || q.bucket.as_deref() == Some("day") {
        let today = align(now, DAY, off);
        let first = today - 90 * DAY;
        let cal = load_sessions(conn, first, today + DAY, now)?;
        for i in 0..91 {
            let day = first + i * DAY;
            let live: i64 = cal
                .iter()
                .map(|s| clip(s.started, s.ended.unwrap_or(now), day, day + DAY))
                .sum();
            let fails: i64 = cal
                .iter()
                .map(|s| {
                    s.fails
                        .iter()
                        .filter(|(at, _)| *at >= day && *at < day + DAY)
                        .count() as i64
                })
                .sum();
            calendar.push((day, live, fails));
        }
    }
    let (last_seen, oldest): (Option<i64>, Option<i64>) = conn.query_row(
        "SELECT max(last_seen_at), min(started_at) FROM boots",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let partial_before = retention_floor(conn)?;
    let retained_oldest = oldest_at(conn)?.0.or(oldest);
    let per_node = vec![NodeParts {
        node_id: node.to_string(),
        live_secs: k.live_secs,
        failures: k.failures,
        // Node downtime is filled from recorder observations below. Relay
        // gaps remain in k.down_secs solely for push availability.
        down_secs: 0,
        last_seen_at: last_seen,
        oldest_at: retained_oldest.map(|at| {
            at.max(q.retained_from.unwrap_or(i64::MIN))
                .max(partial_before.unwrap_or(i64::MIN))
        }),
        partial_before,
        node_observed_secs: None,
    }];
    let buckets_truncated = start < q.to;
    let mut parts = SummaryParts {
        source: Some(node.to_string()),
        buckets,
        buckets_truncated,
        kpis: k,
        prev_kpis,
        calendar,
        clean,
        planned,
        reasons,
        per_node,
        node_segments: node_segments(conn, q)?,
    };
    fill_node_downtime(&mut parts);
    Ok(parts)
}

fn node_segments(conn: &Connection, q: &Query) -> rusqlite::Result<Vec<NodeSegment>> {
    let mut stmt = conn.prepare_cached(
        "SELECT eid,observer,subject,state,reason,max(started_at,?1),min(last_seen_at,?2),
          CASE WHEN ended_at IS NULL THEN NULL ELSE min(ended_at,?2) END,initial,minority
         FROM node_segments WHERE started_at<?2 AND coalesce(ended_at,last_seen_at)>?1
         ORDER BY started_at,eid",
    )?;
    let rows = stmt
        .query_map(params![q.from, q.to], |r| {
            Ok(NodeSegment {
                eid: r.get(0)?,
                observer: r.get(1)?,
                subject: r.get(2)?,
                state: r.get(3)?,
                reason: r.get(4)?,
                started_at: r.get(5)?,
                last_seen_at: r.get(6)?,
                ended_at: r.get(7)?,
                initial: r.get(8)?,
                minority: r.get(9)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

fn interval_union(intervals: &mut [(i64, i64)]) -> i64 {
    intervals.sort_unstable();
    let mut through = i64::MIN;
    let mut total = 0i64;
    for &(a, b) in intervals.iter() {
        total = total.saturating_add((b - a.max(through)).max(0));
        through = through.max(b);
    }
    total
}

fn fill_node_downtime(parts: &mut SummaryParts) {
    let mut observed: BTreeMap<String, Vec<(i64, i64)>> = BTreeMap::new();
    let mut down: BTreeMap<String, Vec<(i64, i64)>> = BTreeMap::new();
    for s in &parts.node_segments {
        let interval = (s.started_at, s.ended_at.unwrap_or(s.last_seen_at));
        if s.state != "unknown" {
            observed
                .entry(s.subject.clone())
                .or_default()
                .push(interval);
        }
        if s.state == "down" {
            down.entry(s.subject.clone()).or_default().push(interval);
        }
    }
    for subject in observed.keys() {
        if !parts.per_node.iter().any(|n| &n.node_id == subject) {
            parts.per_node.push(NodeParts {
                node_id: subject.clone(),
                ..Default::default()
            });
        }
    }
    for n in &mut parts.per_node {
        n.node_observed_secs = observed.get_mut(&n.node_id).map(|v| interval_union(v));
        n.down_secs = down
            .get_mut(&n.node_id)
            .map(|v| interval_union(v))
            .unwrap_or(0);
    }
    parts.per_node.sort_by(|a, b| a.node_id.cmp(&b.node_id));
}

fn merge_live(into: &mut LiveTotals, incoming: LiveTotals) {
    // A validated summary beats a sample; then the newer, then the larger.
    let rank = |l: &LiveTotals| {
        (
            l.watched.is_some() && l.watched_source.as_deref() != Some("sample"),
            l.watched_at,
            l.watched,
        )
    };
    if rank(&incoming) > rank(into) {
        into.watched = incoming.watched;
        into.watched_source = incoming.watched_source;
        into.watched_at = incoming.watched_at;
    }
    into.danmu = into.danmu.max(incoming.danmu);
    into.add_fans = into.add_fans.max(incoming.add_fans);
    into.new_fans_club = into.new_fans_club.max(incoming.new_fans_club);
    into.max_online = into.max_online.max(incoming.max_online);
}

fn merge_kpis(into: &mut KpiParts, k: KpiParts) {
    into.live_secs += k.live_secs;
    into.down_secs += k.down_secs;
    // Each node already counts its own broadcasts once; drop the ones another
    // node already reported so a failover does not look like two 开播.
    // Peers that predate `broadcasts` only contribute their count.
    let common = k.broadcasts.intersection(&into.broadcasts).count() as i64;
    into.sessions += k.sessions - common;
    into.broadcasts.extend(k.broadcasts);
    into.clean_runs += k.clean_runs;
    into.failures += k.failures;
    into.recover_sum += k.recover_sum;
    into.recover_n += k.recover_n;
    into.notify_sent += k.notify_sent;
    into.notify_failed += k.notify_failed;
    into.online_peak = match (into.online_peak, k.online_peak) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (a, b) => a.or(b),
    };
    for (key, live) in k.lives {
        merge_live(into.lives.entry(key).or_default(), live);
    }
}

pub(crate) fn merge_summary(parts: Vec<SummaryParts>) -> SummaryParts {
    let mut out = SummaryParts::default();
    let mut buckets: BTreeMap<i64, BucketParts> = BTreeMap::new();
    let mut cal: BTreeMap<i64, (i64, i64)> = BTreeMap::new();
    let mut sources = std::collections::HashSet::new();
    let mut segments: BTreeMap<String, NodeSegment> = BTreeMap::new();
    let mut nodes: BTreeMap<String, NodeParts> = BTreeMap::new();
    for p in parts {
        out.buckets_truncated |= p.buckets_truncated;
        if let Some(source) = p
            .source
            .clone()
            .or_else(|| (p.per_node.len() == 1).then(|| p.per_node[0].node_id.clone()))
        {
            if !sources.insert(source) {
                continue;
            }
        }
        for b in p.buckets {
            let e = buckets.entry(b.start).or_insert_with(|| BucketParts {
                start: b.start,
                ..Default::default()
            });
            for (k, v) in b.live_secs {
                *e.live_secs.entry(k).or_default() += v;
            }
            let common = b.broadcasts.intersection(&e.broadcasts).count() as i64;
            e.sessions += b.sessions - common;
            e.broadcasts.extend(b.broadcasts);
            e.failures += b.failures;
            e.recover_sum += b.recover_sum;
            e.recover_n += b.recover_n;
            e.notify_sent += b.notify_sent;
            e.notify_failed += b.notify_failed;
        }
        merge_kpis(&mut out.kpis, p.kpis);
        merge_kpis(&mut out.prev_kpis, p.prev_kpis);
        for (day, live, fails) in p.calendar {
            let e = cal.entry(day).or_default();
            e.0 += live;
            e.1 += fails;
        }
        out.clean += p.clean;
        out.planned += p.planned;
        for (k, v) in p.reasons {
            *out.reasons.entry(k).or_default() += v;
        }
        for n in p.per_node {
            let existing = nodes.entry(n.node_id.clone()).or_default();
            existing.node_id = n.node_id;
            existing.live_secs += n.live_secs;
            existing.failures += n.failures;
            existing.last_seen_at = existing.last_seen_at.max(n.last_seen_at);
            existing.oldest_at = match (existing.oldest_at, n.oldest_at) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (a, b) => a.or(b),
            };
            existing.partial_before = existing.partial_before.max(n.partial_before);
        }
        for s in p.node_segments {
            match segments.get_mut(&s.eid) {
                Some(old)
                    if s.last_seen_at > old.last_seen_at
                        || (old.ended_at.is_none() && s.ended_at.is_some()) =>
                {
                    *old = s
                }
                None => {
                    segments.insert(s.eid.clone(), s);
                }
                _ => {}
            }
        }
    }
    out.buckets = buckets.into_values().collect();
    out.calendar = cal.into_iter().map(|(d, (l, f))| (d, l, f)).collect();
    out.per_node = nodes.into_values().collect();
    out.node_segments = segments.into_values().collect();
    fill_node_downtime(&mut out);
    out
}

fn render_kpis(k: &KpiParts) -> serde_json::Value {
    let sum = |f: fn(&LiveTotals) -> Option<i64>| -> Option<i64> {
        let vals: Vec<i64> = k.lives.values().filter_map(f).collect();
        (!vals.is_empty()).then(|| vals.iter().sum())
    };
    let max_online = k.lives.values().filter_map(|l| l.max_online).max();
    let samples = k
        .lives
        .values()
        .filter(|l| l.watched.is_some() && l.watched_source.as_deref() == Some("sample"))
        .count();
    let summaries = k
        .lives
        .values()
        .filter(|l| l.watched.is_some() && l.watched_source.as_deref() != Some("sample"))
        .count();
    let watched_source = match (samples > 0, summaries > 0) {
        (true, true) => Some("mixed"),
        (true, false) => Some("sample"),
        (false, true) => Some("summary"),
        _ => None,
    };
    serde_json::json!({
        "live_secs": k.live_secs,
        "availability": (k.live_secs > 0).then(|| (k.live_secs - k.down_secs) as f64 / k.live_secs as f64),
        "sessions": k.sessions,
        "failures": k.failures,
        "recover_secs_avg": (k.recover_n > 0).then(|| k.recover_sum / k.recover_n),
        "success_rate": (k.clean_runs + k.failures > 0).then(|| k.clean_runs as f64 / (k.clean_runs + k.failures) as f64),
        "notify_sent": k.notify_sent,
        "notify_failed": k.notify_failed,
        "watched": sum(|l| l.watched),
        "watched_source":watched_source,
        "online_peak": k.online_peak,
        "danmu": sum(|l| l.danmu),
        "add_fans": sum(|l| l.add_fans),
        "new_fans_club": sum(|l| l.new_fans_club),
        "max_online": max_online,
    })
}

pub(crate) fn render_summary(
    p: &SummaryParts,
    missing: &[super::MissingNode],
) -> serde_json::Value {
    let buckets: Vec<_> = p
        .buckets
        .iter()
        .map(|b| {
            serde_json::json!({
                "start": b.start, "live_secs": b.live_secs, "sessions": b.sessions, "failures": b.failures,
                "recover_secs_avg": (b.recover_n > 0).then(|| b.recover_sum / b.recover_n),
                "notify_sent": b.notify_sent, "notify_failed": b.notify_failed,
            })
        })
        .collect();
    let calendar: Vec<_> = p
        .calendar
        .iter()
        .map(|(day, live, fails)| serde_json::json!({"day": day, "live_secs": live, "failures": fails}))
        .collect();
    let per_node: Vec<_> = p
        .per_node
        .iter()
        .map(|n| serde_json::json!({"node_id": n.node_id, "live_secs": n.live_secs, "failures": n.failures, "down_secs": n.node_observed_secs.map(|_|n.down_secs), "node_observed_secs":n.node_observed_secs, "last_seen_at": n.last_seen_at}))
        .collect();
    let per_cov: serde_json::Map<String, serde_json::Value> = p
        .per_node
        .iter()
        .map(|n| (n.node_id.clone(), serde_json::json!(n.oldest_at)))
        .collect();
    let from = p.per_node.iter().filter_map(|n| n.oldest_at).min();
    let partial: BTreeMap<_, _> = p
        .per_node
        .iter()
        .filter_map(|n| n.partial_before.map(|at| (n.node_id.clone(), at)))
        .collect();
    let reasons: Vec<_> = p
        .reasons
        .iter()
        .map(|(class, count)| serde_json::json!({"class": class, "count": count}))
        .collect();
    serde_json::json!({
        "buckets": buckets,
        "kpis": render_kpis(&p.kpis),
        "prev_kpis": render_kpis(&p.prev_kpis),
        "calendar": calendar,
        "reliability": {"clean": p.clean, "counted_failures": p.kpis.failures, "planned": p.planned, "reasons": reasons},
        "per_node": per_node,
        "coverage": {"from": from, "per_node": per_cov, "partial_before":partial},
        "node_segments": p.node_segments.iter().rev().take(500).collect::<Vec<_>>(),
        "node_segments_more": p.node_segments.len()>500,
        "missing_nodes": missing,
    })
}

// ------------------------------------------------------------------ status

pub(crate) fn status_rows(conn: &Connection) -> rusqlite::Result<serde_json::Value> {
    let count = |t: &str| -> rusqlite::Result<i64> {
        conn.query_row(&format!("SELECT count(*) FROM {t}"), [], |r| r.get(0))
    };
    // The metric view is a UNION over five projections. Count the occupied
    // cells in one frame scan instead of expanding every raw metric row.
    let raw: i64 = conn.query_row(
        "SELECT coalesce(sum((b_count IS NOT NULL)+(f_count IS NOT NULL)+(s_count IS NOT NULL)+(o_count IS NOT NULL)+(w_count IS NOT NULL)),0) FROM sample_frames",
        [], |row| row.get(0),
    )?;
    Ok(serde_json::json!({
        "sessions": count("sessions")?, "runs": count("runs")?, "events": count("events")?,
        "samples_raw": raw, "samples_1m": count("samples_1m")?, "notify_sends": count("notify_sends")?,
        "node_segments": count("node_segments")?, "bili_lives":count("bili_lives")?, "boots":count("boots")?,
    }))
}

pub(crate) fn oldest_at(conn: &Connection) -> rusqlite::Result<(Option<i64>, Option<i64>)> {
    conn.query_row(
        "SELECT (SELECT min(m) FROM (SELECT min(started_at) m FROM sessions WHERE pruned=0 UNION ALL SELECT min(ts) FROM sample_frames
                 UNION ALL SELECT min(bucket) FROM samples_1m UNION ALL SELECT min(at) FROM events UNION ALL SELECT min(at) FROM notify_sends UNION ALL SELECT min(started_at) FROM node_segments)),
                (SELECT min(started_at) FROM boots)",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )
}

pub(crate) fn retention_floor(conn: &Connection) -> rusqlite::Result<Option<i64>> {
    conn.query_row(
        "SELECT max(cast(value AS INTEGER)) FROM meta WHERE key IN ('sessions_pruned_before','events_pruned_before','notify_pruned_before','nodes_pruned_before','samples_pruned_before','bili_pruned_before')",
        [], |r|r.get(0),
    )
}
