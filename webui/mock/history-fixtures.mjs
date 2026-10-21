import { stopLiveDataToBili } from '../src/js/analytics-format.js';
// Synthetic /api/history/* responses for the 统计 view. Shapes follow
// Architect's contract (.trellis/tasks/10-11-analytics-dashboard/design.md §9).
// All data is fake, deterministic for a given `now`, and labelled 示例.
// Modes (GET /mock/history-mode?mode=…): populated | empty | missing | error | partial
// | clamped (W15 clamped_to_days) | partial-nodes (W16 partial_nodes)
// | clamped-partial (both, plus missing_nodes on cluster scope).
export const HISTORY_MODES = ['populated', 'empty', 'missing', 'error', 'partial', 'clamped', 'partial-nodes', 'clamped-partial'];
export const CLAMP_DAYS = 90;
export const PARTIAL_NODES = [{ node: 'tokyo-1', clamped_to_days: 30 }, { node: 'sg-3', clamped_to_days: 14 }];
// Session / event ids are opaque `node:boot:n` strings (contract): never numbers.
const BOOT = { local: '3f9a1c2b', 'tokyo-1': '7d01e5aa' };
export const sid = (node, n) => `${node}:${BOOT[node] || '00000000'}:${n}`;
const tail = id => Number(String(id).split(':').pop()) || 0;
const MISSING = [{ node_id: 'sg-3', reason: 'timeout', detail: 'no response in 3000 ms' }];

const DAY = 86400;
const PLATFORMS = ['YT', 'TW', 'NC'];
const CHANNELS = [
  { platform: 'YT', channel_id: 'UC1111111111111111111111', channel_name: '示例频道 001' },
  { platform: 'TW', channel_id: 'demo_games', channel_name: '示例频道 002' },
  { platform: 'YT', channel_id: 'UC3333333333333333333333', channel_name: '示例频道 003' },
];

function rng(seed) {
  let s = seed >>> 0 || 1;
  return () => ((s = (s * 1664525 + 1013904223) >>> 0) / 2 ** 32);
}

const RETENTION = { raw_days: 7, rollup_days: 90, event_days: 180, event_rows: 200000 };

export function historyStatus(mode, now) {
  const empty = mode === 'empty';
  return {
    enabled: true, schema_version: 1,
    oldest_at: empty ? null : now - 21 * DAY, recording_since: now - 21 * DAY, db_bytes: empty ? 24576 : 18_874_368,
    rows: empty ? { sessions: 0, runs: 0, events: 0, samples_raw: 0, samples_1m: 0, notify_sends: 0 }
      : { sessions: 46, runs: 61, events: 312, samples_raw: 60480, samples_1m: 30240, notify_sends: 9 },
    dropped: { history: 0, notify: 0 },
    writer: { state: 'running', last_batch_at: now - 4, last_error: null },
    retention: RETENTION, recorder: 'primary',
  };
}

// Two sessions a day, the latest still live. Unix seconds throughout.
function allSessions(now) {
  const rand = rng(Math.floor(now / DAY));
  const out = [];
  let id = 1;
  for (let day = 20; day >= 0; day--) {
    for (const hour of [3, 13]) {
      const started_at = Math.floor(now / DAY) * DAY - day * DAY + hour * 3600 + Math.floor(rand() * 1800);
      if (started_at > now - 600) continue;
      const live = day === 0 && hour === 13 || started_at + 3 * 3600 > now;
      const ended_at = live ? null : started_at + 3600 + Math.floor(rand() * 4 * 3600);
      const ch = CHANNELS[(id - 1) % CHANNELS.length];
      const failures = rand() < 0.12 ? 1 : 0;
      const restarts = failures + (rand() < 0.2 ? 1 : 0);
      const gaps = [];
      if (restarts) {
        const a = started_at + 1800;
        gaps.push([a, a + 45, failures ? 'failed' : 'reconnecting']);
      }
      const dur = (ended_at ?? now) - started_at;
      out.push({
        id: sid(id % 5 === 0 ? 'tokyo-1' : 'local', id), n: id, eid: `local-${id}`, node: id % 5 === 0 ? 'tokyo-1' : 'local', ...ch,
        title: `${ch.channel_name} | 示例直播 ${id}`, started_at, ended_at, duration_secs: dur,
        end_reason: live ? null : failures ? (id % 2 ? 'aborted' : 'process_restart') : CLEAN_END[id % CLEAN_END.length], runs: restarts + 1, restarts, failures, stuck: 0, gaps,
        median_bitrate_kbps: 5800 + Math.floor(rand() * 600),
        bili: mockBili(live, rand),
      });
      id++;
    }
  }
  out.reverse();
  splitSwitchedLive(out);
  restartedLive(out);
  return out;
}

// One Bilibili live carried two channels (a priority switch mid-live), so
// 本场详情 has a 场 with two 段 and a 中断 mark between them. The split keeps
// both halves inside the original window: total relay time, failures and
// counts are unchanged, only the channel of the second half differs.
function splitSwitchedLive(sessions) {
  // Newest finished relay with room for two halves, so the 场 lands inside the
  // default 7-day range the 统计 view opens with.
  const at = sessions.findIndex(s => s.ended_at && s.ended_at - s.started_at >= 2 * 3600);
  if (at < 0) return;
  const whole = sessions[at];
  const mid = whole.started_at + Math.floor((whole.ended_at - whole.started_at) / 2);
  const other = CHANNELS[(whole.n + 1) % CHANNELS.length];
  const first = { ...whole, ended_at: mid, duration_secs: mid - whole.started_at, end_reason: 'source_ended', broadcast_id: `live:${whole.n}`, gaps: [] };
  const second = {
    ...whole, id: sid(whole.node, 900 + whole.n), n: 900 + whole.n, eid: `local-${900 + whole.n}`,
    ...other, title: `${other.channel_name} | 示例直播 ${whole.n}`, started_at: mid,
    duration_secs: whole.ended_at - mid, end_reason: 'source_ended', broadcast_id: `live:${whole.n}`, gaps: [],
  };
  sessions.splice(at, 1, first, second);
}

// One Bilibili live whose relay restarted on another node (local → tokyo-1)
// without switching channel: one 段, but the row boundary is still one 中断.
// This is the shape production hit — a 场 with a restart and no channel chain.
function restartedLive(sessions) {
  const at = sessions.findIndex(s => s.channel_name === '示例频道 003'
    && s.ended_at && s.ended_at - s.started_at >= 2 * 3600);
  if (at < 0) return;
  const whole = sessions[at];
  const mid = whole.started_at + Math.floor((whole.ended_at - whole.started_at) * 0.6);
  // The first half keeps the relay's own gap (a restart inside the row), so the
  // 场 carries both kinds of 中断: one gap and one node handover.
  const first = { ...whole, ended_at: mid, duration_secs: mid - whole.started_at, end_reason: 'process_restart', broadcast_id: `live:${whole.n}` };
  const second = {
    ...whole, id: sid('tokyo-1', 800 + whole.n), n: 800 + whole.n, eid: 'local-800', node: 'tokyo-1',
    started_at: mid, duration_secs: whole.ended_at - mid, broadcast_id: `live:${whole.n}`, gaps: [],
  };
  sessions.splice(at, 1, first, second);
}

// sessions[].bili as the backend stores it (design §9): StopLiveData totals go
// through the same allowlist/validity rule as the backend (stopLiveDataToBili),
// so a sentinel reply becomes a failed summary with NULL totals. online_peak
// comes from our own danmaku samples and is set even while pending or failed.
// No revenue field exists anywhere.
function mockBili(live, rand) {
  const online_peak = 40 + Math.floor(rand() * 300);
  if (live) return { state: 'pending', online_peak, watched: null, danmu: null, add_fans: null, new_fans_club: null, max_online: null };
  const roll = rand();
  if (roll < 0.05) return { state: 'skipped', online_peak, watched: null, danmu: null, add_fans: null, new_fans_club: null, max_online: null };
  const reply = roll < 0.12
    ? { code: 0, data: { LiveTime: -999999, AddFans: -999999, NewFansClub: 0, DanmuNum: 0, MaxOnline: 0, WatchedCount: 1167856 } }
    : { code: 0, data: { LiveTime: 3600, AddFans: Math.floor(rand() * 60), NewFansClub: Math.floor(rand() * 6), DanmuNum: Math.floor(rand() * 2500), WatchedCount: 800 + Math.floor(rand() * 4000), ...(rand() < 0.3 ? { MaxOnline: online_peak + Math.floor(rand() * 50) } : {}) } };
  return { ...stopLiveDataToBili(reply), online_peak };
}

// Only the seven backend end_reason codes (design.md §5.4/§5.5). Counted
// failures end as aborted / process_restart; others rotate through the rest.
export const END_REASONS = Object.freeze(['source_ended', 'bili_stopped', 'user_stop', 'intentional_restart', 'cluster_handoff', 'process_restart', 'aborted']);
const CLEAN_END = ['source_ended', 'source_ended', 'bili_stopped', 'user_stop', 'intentional_restart', 'cluster_handoff'];

const overlaps = (s, from, to) => s.started_at < to && (s.ended_at ?? to) > from;

function kpis(list, from, to) {
  const live = list.reduce((sum, s) => sum + Math.max(0, Math.min(to, s.ended_at ?? to) - Math.max(from, s.started_at)), 0);
  const failures = list.reduce((n, s) => n + s.failures, 0);
  const done = list.filter(s => s.bili?.state === 'ok');
  const sum = key => done.length ? done.reduce((n, s) => n + s.bili[key], 0) : null;
  const peaks = list.map(s => s.bili?.online_peak).filter(Number.isFinite);
  const maxes = done.map(s => s.bili.max_online).filter(Number.isFinite);
  return {
    live_secs: live, availability: list.length ? 1 - failures * 45 / Math.max(1, live) : null, sessions: list.length, failures,
    recover_secs_avg: failures ? 45 : null, success_rate: list.length ? (list.length - failures) / list.length : null,
    notify_sent: failures, notify_failed: 0,
    watched: sum('watched'), online_peak: peaks.length ? Math.max(...peaks) : null,
    danmu: sum('danmu'), add_fans: sum('add_fans'), new_fans_club: sum('new_fans_club'), max_online: maxes.length ? Math.max(...maxes) : null,
  };
}

function summary(q, now, partial) {
  const { from, to } = q;
  const size = { hour: 3600, '6h': 6 * 3600, day: DAY }[q.bucket] || 3600;
  const sessions = allSessions(now);
  const inRange = sessions.filter(s => overlaps(s, from, to));
  const buckets = [];
  for (let start = from; start < to && buckets.length < 400; start += size) {
    const end = start + size;
    const live_secs = Object.fromEntries(PLATFORMS.map(p => [p, 0]));
    const hit = inRange.filter(s => overlaps(s, start, end));
    for (const s of hit) live_secs[s.platform] += Math.min(end, s.ended_at ?? now) - Math.max(start, s.started_at);
    const failures = hit.reduce((n, s) => n + s.failures, 0);
    buckets.push({ start, live_secs, sessions: hit.length, failures, recover_secs_avg: failures ? 45 : null, notify_sent: failures, notify_failed: 0 });
  }
  const span = to - from;
  const prev = sessions.filter(s => overlaps(s, from - span, from));
  const calendar = q.bucket === 'day' || q.calendar === '1'
    ? Array.from({ length: 91 }, (_, i) => {
      const day = Math.floor(now / DAY) * DAY - (90 - i) * DAY;
      const hit = sessions.filter(s => overlaps(s, day, day + DAY));
      return { day, live_secs: hit.reduce((n, s) => n + Math.min(day + DAY, s.ended_at ?? now) - Math.max(day, s.started_at), 0), failures: hit.reduce((n, s) => n + s.failures, 0) };
    })
    : [];
  const k = kpis(inRange, from, to);
  return {
    buckets, kpis: k, prev_kpis: kpis(prev, from - span, from), calendar,
    reliability: { clean: inRange.length - k.failures, counted_failures: k.failures, planned: 1, reasons: k.failures ? [{ class: 'source_eof', count: k.failures }] : [] },
    per_node: [
      { node_id: 'local', live_secs: Math.round(k.live_secs * 0.8), failures: k.failures, down_secs: 0, last_seen_at: now },
      { node_id: 'tokyo-1', live_secs: Math.round(k.live_secs * 0.2), failures: 0, down_secs: 600, last_seen_at: now - 3 },
    ],
    coverage: { from: now - 21 * DAY, per_node: { local: now - 21 * DAY, 'tokyo-1': now - 10 * DAY } },
    missing_nodes: partial ? MISSING : [],
  };
}

// Keyset page. Returns null for a cursor that names no row (route → 400).
function page(items, q, fallback) {
  const limit = Math.max(1, Math.min(500, Math.trunc(Number(q.limit)) || fallback));
  let start = 0;
  if (q.cursor) {
    const at = items.findIndex(item => String(item.id) === q.cursor);
    if (at < 0) return null;
    start = at + 1;
  }
  const slice = items.slice(start, start + limit);
  return { items: slice, next_cursor: start + limit < items.length ? String(slice.at(-1).id) : null };
}

function sessionsResponse(q, now, partial) {
  const all = allSessions(now);
  const missing_nodes = partial ? MISSING : [];
  if (q.id) {
    const i = all.findIndex(s => String(s.id) === q.id);
    return { items: i < 0 ? [] : [all[i]], next_cursor: null, missing_nodes };
  }
  let list = all.filter(s => overlaps(s, q.from, q.to));
  if (q.platform) list = list.filter(s => s.platform === q.platform);
  if (q.channel) list = list.filter(s => s.channel_id === q.channel);
  const paged = page(list, q, 50);
  return paged && { ...paged, missing_nodes };
}

function eventsResponse(q, now, partial) {
  const items = [];
  let id = 1000;
  for (const s of allSessions(now)) {
    if (s.ended_at) items.push({ id: sid(s.node, id++), eid: `ev-${id}`, at: s.ended_at, node: s.node, kind: 'stream.ended', session_id: s.id, title: `转播结束：${s.channel_name}`, detail: `时长 ${Math.round(s.duration_secs / 60)} 分钟`, severity: 'info' });
    for (const [a, , kind] of s.gaps) {
      items.push({ id: sid(s.node, id++), eid: `ev-${id}`, at: a, node: s.node, kind: kind === 'failed' ? 'stream.failed' : 'stream.reconnecting', title: kind === 'failed' ? `转播失败：${s.channel_name}` : `重新连接：${s.channel_name}`, detail: 'ffmpeg 退出（示例）', severity: kind === 'failed' ? 'error' : 'warn' });
      if (kind === 'failed') items.push({ id: sid(s.node, id++), eid: `ev-${id}`, at: a + 5, node: s.node, kind: 'notify.sent', title: `QQ 告警已送达：转播失败 ${s.channel_name}`, detail: '', severity: 'info' });
    }
    items.push({ id: sid(s.node, id++), eid: `ev-${id}`, at: s.started_at, node: s.node, kind: 'stream.started', title: `开始转播：${s.channel_name}`, detail: s.title, severity: 'info' });
  }
  items.push({ id: sid('tokyo-1', id++), eid: `ev-${id}`, at: now - 2 * DAY, node: 'tokyo-1', kind: 'cluster.failover', title: '故障转移 · local → tokyo-1', detail: 'local 心跳超时 20 秒（示例）', severity: 'warn' });
  const kinds = q.kind ? q.kind.split(',') : null;
  const list = items
    .filter(e => e.at >= q.from && e.at <= q.to && (!kinds || kinds.some(k => e.kind === k || e.kind.startsWith(`${k}.`))))
    .filter(e => !q.severity || q.severity.split(',').includes(e.severity))
    .sort((a, b) => b.at - a.at || tail(b.id) - tail(a.id));
  const paged = page(list, q, 20);
  return paged && { ...paged, missing_nodes: partial ? MISSING : [] };
}

function statsResponse(q, now, partial) {
  let { from, to } = q;
  if (q.session) {
    // `session` is a comma list: one 段 may be relayed by several sessions
    // (interruptions, restarts), and its samples must cover the whole 段.
    const ids = String(q.session).split(',');
    const members = allSessions(now).filter(x => ids.includes(String(x.id)));
    if (!members.length) return { resolution: 'raw', ts: [], series: {}, missing_nodes: partial ? MISSING : [] };
    from = Math.min(...members.map(m => m.started_at));
    to = Math.max(...members.map(m => m.ended_at ?? now));
  }
  const resolution = to - from <= 3 * DAY && from >= now - RETENTION.raw_days * DAY ? 'raw' : '1m';
  const points = Math.max(2, Math.min(2000, Math.trunc(Number(q.max_points)) || 1000));
  // At most `points` samples: count = floor(span / step) + 1 ≤ points.
  const step = Math.max(resolution === 'raw' ? 10 : 60, Math.ceil((to - from) / (points - 1)));
  const sessions = allSessions(now);
  const metrics = (q.metrics || 'bitrate,fps,speed,online,watched').split(',');
  const base = { bitrate: 6000, fps: 60, speed: 1, online: 120, watched: 900 };
  const jitter = { bitrate: 300, fps: 1.5, speed: 0.03, online: 40, watched: 0 };
  const ts = [];
  const series = Object.fromEntries(metrics.map(m => [m, { mean: [], min: [], max: [] }]));
  const rand = rng(from);
  for (let t = from; t <= to && ts.length < points; t += step) {
    ts.push(t);
    const s = sessions.find(x => t >= x.started_at && t < (x.ended_at ?? now));
    for (const m of metrics) {
      const col = series[m];
      if (!col) continue;
      if (!s || !(m in base)) { col.mean.push(null); col.min.push(null); col.max.push(null); continue; }
      const v = m === 'watched' ? base.watched + Math.floor((t - s.started_at) / 6) : base[m] + (rand() - 0.5) * 2 * jitter[m];
      const spread = jitter[m] * 0.6;
      col.mean.push(+v.toFixed(2)); col.min.push(+(v - spread).toFixed(2)); col.max.push(+(v + spread).toFixed(2));
    }
  }
  return { resolution, ts, series, missing_nodes: partial ? MISSING : [] };
}

const EMPTY_KPIS = {
  live_secs: 0, availability: null, sessions: 0, failures: 0, recover_secs_avg: null, success_rate: null,
  notify_sent: 0, notify_failed: 0, watched: null, online_peak: null, danmu: null, add_fans: null, new_fans_club: null, max_online: null,
};
function emptySummary() {
  return {
    buckets: [], kpis: { ...EMPTY_KPIS }, prev_kpis: { ...EMPTY_KPIS }, calendar: [],
    reliability: { clean: 0, counted_failures: 0, planned: 0, reasons: [] },
    per_node: [], coverage: { from: null, per_node: {} }, missing_nodes: [],
  };
}

// Returns [status, body] for /api/history/* or null when the path is not one.
export function historyRoute(path, params, mode, ok, nowSec = Math.floor(Date.now() / 1000)) {
  if (!path.startsWith('/api/history/')) return null;
  if (mode === 'missing') return [404, { success: false, message: `Unknown route: ${path}` }];
  if (mode === 'error') return [500, { success: false, message: '示例错误：history.sqlite 无法打开' }];
  const q = Object.fromEntries(params);
  q.to = Number(q.to) || nowSec;
  q.from = Number(q.from) || q.to - DAY;
  if (!(q.from < q.to)) return [400, { success: false, message: 'from 必须早于 to' }];
  // missing_nodes only applies to cluster scope; scope=local is always [].
  const partial = (mode === 'partial' || mode === 'clamped-partial') && q.scope !== 'local';
  const empty = mode === 'empty';
  const name = path.slice('/api/history/'.length);
  if (name === 'status') return [200, ok(historyStatus(mode, nowSec))];
  // POST /api/history/clean is destructive maintenance: answer before the
  // clamped/partial decorations, since the caller re-reads everything after it.
  // Every node clears, so the report names each one; `partial` mode also shows
  // a node that kept its rows, which is what the danger notice is for.
  if (name === 'clean') {
    if (empty) return [200, ok({ rows: 0, nodes: [], failed: [] })];
    return [200, ok({
      rows: 1280,
      nodes: [{ node_id: 'local', rows: 1000 }, { node_id: 'tokyo-1', rows: 280 }],
      failed: partial ? [{ node_id: 'sg-3', reason: '正在转播，请先停止转播再清理历史' }] : [],
    })];
  }
  const base = ok;
  ok = data => base(data && typeof data === 'object' ? {
    ...data,
    ...(mode === 'clamped' || mode === 'clamped-partial' ? { clamped_to_days: CLAMP_DAYS } : {}),
    ...(mode === 'partial-nodes' || mode === 'clamped-partial' ? { partial_nodes: PARTIAL_NODES.map(p => ({ ...p })) } : {}),
  } : data);
  if (empty) {
    if (name === 'summary') return [200, ok(emptySummary())];
    if (name === 'sessions' || name === 'events') return [200, ok({ items: [], next_cursor: null, missing_nodes: [] })];
    if (name === 'stats') return [200, ok({ resolution: 'raw', ts: [], series: {}, missing_nodes: [] })];
  }
  if (name === 'summary') return [200, ok(summary(q, nowSec, partial))];
  if (name === 'sessions' || name === 'events') {
    const body = name === 'sessions' ? sessionsResponse(q, nowSec, partial) : eventsResponse(q, nowSec, partial);
    return body ? [200, ok(body)] : [400, { success: false, message: 'cursor 无效' }];
  }
  if (name === 'stats') return [200, ok(statsResponse(q, nowSec, partial))];
  return [404, { success: false, message: `Unknown route: ${path}` }];
}
