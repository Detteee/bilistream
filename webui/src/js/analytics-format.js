// analytics-format.js — pure helpers for the 统计 view. No DOM, no network,
// no uPlot. History timestamps from /api/history/* are unix seconds; every
// helper here takes seconds unless its name says otherwise.

// Self-contained on purpose: this file is bundled into the lazy analytics.js
// chunk, and importing an app module that main.js also uses would make
// esbuild emit a third shared chunk, which build.mjs rejects.
export function localHourOffset(date = new Date()) {
  return Math.round(-date.getTimezoneOffset() / 60);
}

export const DASH = '--';

const countFmt = new Intl.NumberFormat('zh-CN', { maximumFractionDigits: 0 });
const oneDecimal = new Intl.NumberFormat('zh-CN', { maximumFractionDigits: 1 });
const pad = n => String(n).padStart(2, '0');
const isNum = value => typeof value === 'number' && Number.isFinite(value);

// `--` for anything the API left empty (null, undefined, NaN, ±Infinity).
export function orDash(value, fmt = formatCount) {
  return isNum(value) ? fmt(value) : DASH;
}

export function formatCount(value) {
  return isNum(value) ? countFmt.format(Math.round(value)) : DASH;
}

export function formatHours(secs) {
  return isNum(secs) ? oneDecimal.format(Math.max(0, secs) / 3600) : DASH;
}

export function formatPercent(ratio) {
  return isNum(ratio) ? `${oneDecimal.format(ratio * 100)}%` : DASH;
}

export function formatMonthDay(sec) {
  const d = new Date(sec * 1000);
  return `${pad(d.getMonth() + 1)}-${pad(d.getDate())}`;
}

export function formatClockShort(sec) {
  const d = new Date(sec * 1000);
  return `${pad(d.getHours())}:${pad(d.getMinutes())}`;
}

// `MM-DD HH:mm`, with the year in front once it is more than a year old.
export function formatAbsolute(sec, nowSec = Date.now() / 1000) {
  if (!isNum(sec)) return DASH;
  const base = `${formatMonthDay(sec)} ${formatClockShort(sec)}`;
  return nowSec - sec > 365 * 86400 ? `${new Date(sec * 1000).getFullYear()}-${base}` : base;
}

export function formatRelative(sec, nowSec = Date.now() / 1000) {
  if (!isNum(sec)) return DASH;
  const diff = Math.max(0, nowSec - sec);
  if (diff < 60) return '刚刚';
  if (diff < 3600) return `${Math.floor(diff / 60)} 分钟前`;
  if (diff < 86400) return `${Math.floor(diff / 3600)} 小时前`;
  return formatAbsolute(sec, nowSec);
}

export function formatRange(from, to, nowSec = Date.now() / 1000) {
  return `${formatAbsolute(from, nowSec)} ～ ${formatAbsolute(to, nowSec)}`;
}

export function formatOffset(offset = localHourOffset()) {
  return `(UTC${offset >= 0 ? '+' : '−'}${Math.abs(offset)})`;
}

// KPI change against prev_kpis. `invert` for metrics where lower is better
// (failures, recovery time).
export function formatDelta(value, unit = '', invert = false) {
  if (!isNum(value)) return { text: DASH, tone: '' };
  // Sign and tone follow the value as printed (one decimal), so 0.04 is ±0.
  const shown = Math.round(value * 10) / 10;
  const sign = shown > 0 ? '+' : shown < 0 ? '−' : '±';
  const good = invert ? shown < 0 : shown > 0;
  return { text: `${sign}${oneDecimal.format(Math.abs(shown))}${unit}`, tone: shown === 0 ? '' : good ? 'good' : 'bad' };
}

export function resolutionLabel(resolution) {
  return resolution === 'raw' ? '原始采样' : '每分钟汇总';
}

const DAY = 86400;
// W9 table. `range` is { preset: '24h' | '7d' | '30d' | 'custom', from, to }.
export function bucketFor(range) {
  if (range?.preset === '24h') return { bucket: 'hour', label: '每小时' };
  if (range?.preset === '7d') return { bucket: '6h', label: '每 6 小时' };
  if (range?.preset === '30d') return { bucket: 'day', label: '每天' };
  return isNum(range?.to) && isNum(range?.from) && range.to - range.from > 3 * DAY ? { bucket: 'day', label: '每天' } : { bucket: 'hour', label: '每小时' };
}

export function presetRange(preset, nowSec = Math.floor(Date.now() / 1000)) {
  const span = { '24h': DAY, '7d': 7 * DAY, '30d': 30 * DAY }[preset];
  if (!span) throw new Error(`unknown range preset: ${preset}`);
  return { preset, from: nowSec - span, to: nowSec };
}

// Retention copy from /api/history/status `retention`.
export function retentionLine(retention) {
  const r = retention || {};
  return `保留期限：原始采样 ${orDash(r.raw_days)} 天 · 每分钟汇总 ${orDash(r.rollup_days)} 天 · 事件 ${orDash(r.event_days)} 天（最多 ${orDash(r.event_rows)} 条），可在系统设置中调整。`;
}

export function coverageLine(from, to, scopeLabel = '全部节点', offset = localHourOffset(), nowSec = to) {
  if (!isNum(from)) return '尚未开始记录';
  return `${formatRange(from, to, nowSec)} ${formatOffset(offset)} · ${scopeLabel}`;
}

// Relay health strip (W8/W9). Worst state per cell wins:
// fail > warn > live > idle > gap. Cells entirely before `coverageFrom`
// are 'gap' (未记录). An open session (ended_at null) runs to `to`.
const RANK = { gap: 0, idle: 1, live: 2, warn: 3, fail: 4 };
export function stripCells(sessions, from, to, count = 48, coverageFrom = from) {
  const width = (to - from) / count;
  const cells = Array.from({ length: count }, (_, i) => {
    const start = from + i * width;
    return { start, end: start + width, state: isNum(coverageFrom) && start + width <= coverageFrom ? 'gap' : 'idle' };
  });
  const paint = (a, b, state) => {
    if (!(b > a)) return;
    const first = Math.max(0, Math.floor((a - from) / width));
    const last = Math.min(count - 1, Math.ceil((b - from) / width) - 1);
    for (let i = first; i <= last; i++) {
      const cell = cells[i];
      if (cell.end > a && cell.start < b && RANK[state] > RANK[cell.state]) cell.state = state;
    }
  };
  for (const session of Array.isArray(sessions) ? sessions : []) {
    const start = session.started_at;
    if (!isNum(start)) continue;
    const end = isNum(session.ended_at) ? session.ended_at : to;
    paint(Math.max(from, start), Math.min(to, end), 'live');
    for (const gap of Array.isArray(session.gaps) ? session.gaps : []) {
      if (!Array.isArray(gap) || !isNum(gap[0])) continue;
      const [a, b, kind] = gap;
      paint(Math.max(from, a), Math.min(to, isNum(b) ? b : end), kind === 'failed' ? 'fail' : 'warn');
    }
  }
  return cells;
}

// SVG path for a sparkline. Keeps the min and max of each pixel column so
// spikes survive downsampling, never emits more than `maxPoints` points, and
// breaks the line at null samples.
export function sparkPath(values, min, max, w, h, maxPoints = 240) {
  const list = Array.isArray(values) ? values : [];
  if (!list.length || !(w > 0) || !(h > 0)) return '';
  let lo = isNum(min) ? min : Infinity;
  let hi = isNum(max) ? max : -Infinity;
  if (!isNum(min) || !isNum(max)) {
    for (const v of list) {
      if (!isNum(v)) continue;
      if (!isNum(min) && v < lo) lo = v;
      if (!isNum(max) && v > hi) hi = v;
    }
  }
  if (!isNum(lo) || !isNum(hi)) return '';
  const span = hi - lo || 1;
  const columns = Math.max(1, Math.min(maxPoints >> 1, Math.floor(w)));
  const step = list.length > 1 ? w / (list.length - 1) : 0;
  const y = v => +(h - ((Math.min(hi, Math.max(lo, v)) - lo) / span) * h).toFixed(2);
  const points = [];
  if (list.length <= maxPoints) {
    list.forEach((v, i) => points.push(isNum(v) ? [+(i * step).toFixed(2), y(v)] : null));
  } else {
    const per = list.length / columns;
    for (let c = 0; c < columns; c++) {
      const slice = list.slice(Math.floor(c * per), Math.floor((c + 1) * per)).filter(isNum);
      if (!slice.length) { points.push(null); continue; }
      const x = +((c / (columns - 1 || 1)) * w).toFixed(2);
      const a = Math.min(...slice), b = Math.max(...slice);
      points.push([x, y(a)]);
      if (b !== a) points.push([x, y(b)]);
    }
  }
  let d = '', pen = false;
  for (const p of points) {
    if (!p) { pen = false; continue; }
    d += `${pen ? 'L' : 'M'}${p[0]} ${p[1]}`;
    pen = true;
  }
  return d;
}

// Success ring: stroke-dasharray pairs for an SVG circle of the given
// circumference. `rate` is null when nothing happened in the range.
export function ringArcs(ok, fail, circumference = 100) {
  const good = isNum(ok) && ok > 0 ? ok : 0;
  const bad = isNum(fail) && fail > 0 ? fail : 0;
  const total = good + bad;
  if (!total) return { rate: null, ok: `0 ${circumference}`, fail: `0 ${circumference}`, failOffset: 0 };
  const okLen = +((good / total) * circumference).toFixed(3);
  const failLen = +(circumference - okLen).toFixed(3);
  return { rate: good / total, ok: `${okLen} ${+(circumference - okLen).toFixed(3)}`, fail: `${failLen} ${okLen}`, failOffset: -okLen };
}

// ---------------------------------------------------------------------------
// Bilibili audience (W3 "Audience caveats", implement.md Phase 1 step 5).

// StopLiveData fields kept by the backend. Everything else in the reply
// (HamsterRmb and any future field) is dropped. LiveTime is kept raw only and
// never used for duration.
export const STOP_LIVE_DATA_FIELDS = Object.freeze({
  LiveTime: 'live_time', AddFans: 'add_fans', NewFansClub: 'new_fans_club',
  DanmuNum: 'danmu', MaxOnline: 'max_online', WatchedCount: 'watched',
});
// Count fields the UI shows as session totals (max_online is separate: it is
// shown only when present, marked 未验证).
export const BILI_TOTAL_KEYS = Object.freeze(['watched', 'danmu', 'add_fans', 'new_fans_club']);
const EMPTY_TOTALS = Object.freeze({ watched: null, danmu: null, add_fans: null, new_fans_club: null, max_online: null });

// Mirrors the backend rule for a raw StopLiveData reply: valid only when
// code == 0 and no kept field is negative (an unknown live_key answers code 0
// with -999999 sentinels); -101 = not logged in. A failed reply yields NULL
// totals. Used by the mock and as the reference for Scout's parser tests.
export function stopLiveDataToBili(reply) {
  const data = reply && typeof reply.data === 'object' && reply.data ? reply.data : {};
  const kept = {};
  for (const [field, key] of Object.entries(STOP_LIVE_DATA_FIELDS)) {
    if (field in data) kept[key] = data[field];
  }
  const values = Object.values(kept);
  const valid = reply?.code === 0 && values.every(v => isNum(v) && v >= 0);
  if (!valid) return { state: 'failed', ...EMPTY_TOTALS };
  const out = { state: 'ok' };
  for (const key of [...BILI_TOTAL_KEYS, 'max_online']) out[key] = isNum(kept[key]) ? kept[key] : null;
  return out;
}

// Defensive reading of `sessions[].bili` from /api/history/sessions. Totals
// are trusted only for state 'ok' with no negative or non-numeric count; any
// sentinel (-999999) or other negative turns the whole summary into a failed
// one, so the UI never prints a raw sentinel. online_peak comes from our own
// danmaku samples, so it survives a failed or pending summary.
export function biliTotals(bili) {
  if (!bili || typeof bili !== 'object') return { state: 'none', online_peak: null, ...EMPTY_TOTALS };
  const peak = isNum(bili.online_peak) && bili.online_peak >= 0 ? bili.online_peak : null;
  const fields = [...BILI_TOTAL_KEYS, 'max_online'];
  const bad = fields.some(key => bili[key] != null && !(isNum(bili[key]) && bili[key] >= 0));
  const state = ['pending', 'ok', 'failed', 'skipped'].includes(bili.state) ? bili.state : 'failed';
  if (state !== 'ok' || bad) return { state: state === 'ok' ? 'failed' : state, online_peak: peak, ...EMPTY_TOTALS };
  const out = { state, online_peak: peak };
  for (const key of fields) out[key] = isNum(bili[key]) ? bili[key] : null;
  return out;
}

// Sub-label for a totals tile: pending → 结束后汇总; no key → 无 B 站汇总.
export function biliStateLabel(state) {
  if (state === 'pending') return '结束后汇总';
  if (state === 'skipped') return '无 B 站汇总';
  return '';
}

// Range totals over sessions. A field is summed only over sessions whose
// summary is valid; with none it stays null (`--`). max_online is the largest
// present value, or null so the 最大在线 tile is omitted.
export function aggregateAudience(sessions) {
  const list = (Array.isArray(sessions) ? sessions : []).map(s => biliTotals(s?.bili));
  const ok = list.filter(t => t.state === 'ok');
  const sum = key => {
    const vals = ok.map(t => t[key]).filter(isNum);
    return vals.length ? vals.reduce((a, b) => a + b, 0) : null;
  };
  const peaks = list.map(t => t.online_peak).filter(isNum);
  const maxes = ok.map(t => t.max_online).filter(isNum);
  return {
    sessions: list.length, summarized: ok.length, pending: list.filter(t => t.state === 'pending').length,
    online_peak: peaks.length ? Math.max(...peaks) : null,
    watched: sum('watched'), danmu: sum('danmu'), add_fans: sum('add_fans'), new_fans_club: sum('new_fans_club'),
    max_online: maxes.length ? Math.max(...maxes) : null,
  };
}

// Session duration from bilistream's own start/end times (never LiveTime).
// An open session runs to `nowSec`.
export function sessionDuration(session, nowSec = Date.now() / 1000) {
  const start = session?.started_at;
  if (!isNum(start)) return null;
  const end = isNum(session.ended_at) ? session.ended_at : nowSec;
  return Math.max(0, end - start);
}

export function formatSessionDuration(secs) {
  if (!isNum(secs)) return DASH;
  const total = Math.max(0, Math.round(secs));
  const h = Math.floor(total / 3600), m = Math.floor((total % 3600) / 60);
  if (h) return `${h} 小时 ${m} 分`;
  if (m) return `${m} 分钟`;
  return `${total} 秒`;
}

// Tier the server should answer with (design §9, W3): raw only when the range
// is ≤ 3 days AND lies entirely inside retention.raw_days; otherwise 1-minute
// rollups. Used for the caption until stats.resolution arrives, and to flag a
// server answer that disagrees.
export function expectedResolution(from, to, retention, nowSec = Date.now() / 1000) {
  const rawDays = retention?.raw_days;
  if (!isNum(from) || !isNum(to) || !isNum(rawDays) || !(to > from)) return '1m';
  return to - from <= 3 * DAY && from >= nowSec - rawDays * DAY ? 'raw' : '1m';
}

// Custom range from two <input type="date"> values (local days, end day
// inclusive). Errors use the W5 copy.
export function customRange(fromDate, toDate, rollupDays, nowSec = Math.floor(Date.now() / 1000)) {
  const parse = text => {
    const m = /^(\d{4})-(\d{2})-(\d{2})$/.exec(text || '');
    return m ? new Date(Number(m[1]), Number(m[2]) - 1, Number(m[3])).getTime() / 1000 : NaN;
  };
  const from = parse(fromDate);
  const endDay = parse(toDate);
  if (!isNum(from) || !isNum(endDay)) return { error: '请选择开始日期和结束日期' };
  const to = Math.min(nowSec, endDay + DAY);
  if (!(to > from)) return { error: '开始日期须早于结束日期' };
  const limit = isNum(rollupDays) ? rollupDays : 90;
  if (to - from > limit * DAY || from < nowSec - limit * DAY) return { error: `最长 ${limit} 天（按当前保留设置）` };
  return { preset: 'custom', from, to };
}

// Partial-data line (W4/W9). `detail` is server text and goes only into title.
const MISSING_REASON = { timeout: '未响应', unreachable: '无法连接', auth_failed: '节点认证失败', unsupported: '版本过旧，不支持统计', error: '读取失败' };
export function missingNodeText(missing) {
  const list = Array.isArray(missing) ? missing.filter(m => m && m.node_id) : [];
  if (!list.length) return null;
  const groups = new Map();
  for (const m of list) {
    const copy = MISSING_REASON[m.reason] || '读取失败';
    if (!groups.has(copy)) groups.set(copy, []);
    groups.get(copy).push(m);
  }
  const parts = [...groups].map(([copy, nodes]) => `${nodes.map(n => n.node_id).join('、')} ${copy}`);
  return {
    text: `部分数据缺失：${parts.join('；')}，以下统计不含这些节点。`,
    nodes: list.map(m => ({ node_id: String(m.node_id), reason: MISSING_REASON[m.reason] || '读取失败', title: typeof m.detail === 'string' ? m.detail : '' })),
  };
}

// 4 heat levels relative to the busiest day in the window.
export function heatLevel(secs, max) {
  if (!isNum(secs) || secs <= 0 || !isNum(max) || max <= 0) return 0;
  return Math.max(1, Math.min(4, Math.ceil((secs / max) * 4)));
}

const PLATFORM_KEY = { YT: 'youtube', TW: 'twitch', NC: 'niconico' };
const PLATFORM_NAME = { YT: 'YouTube', TW: 'Twitch', NC: 'Niconico' };
export function platformKey(code) { return PLATFORM_KEY[code] || 'niconico'; }
export function platformName(code) { return PLATFORM_NAME[code] || String(code || DASH); }

// Failure-reason labels (W5 推流可靠性).
const REASON_LABEL = {
  connection_reset: '连接被重置', low_speed: '推流速度过低', stats_stalled: '统计停止更新', timestamp_frozen: '时间戳冻结',
  http_403: 'HTTP 403', bili_warning: 'B站 房间警告', bili_cut_off: 'B站 切断直播', start_failed: '开播失败',
  failover: '节点故障转移', api_unavailable: '外部 API 不可用',
};
export function reasonLabel(cls) { return REASON_LABEL[cls] || '其他'; }

// 结束原因 (单场详情). Codes from design.md §5.4 (SessionGuard mapping) and
// §5.5 (crash close); wording from webui-design.md W5 推流可靠性 terms
// (正常结束 / 异常退出 / 计划内重启 / 交接（不计入）). Unknown → 其他, never raw.
const END_REASON_LABEL = Object.freeze({
  source_ended: '正常结束（源已下播）',
  bili_stopped: '正常结束（B站 已下播）',
  user_stop: '正常结束（手动停止）',
  intentional_restart: '计划内重启',
  cluster_handoff: '节点交接（不计入）',
  process_restart: '异常退出（进程重启）',
  aborted: '异常退出（中断）',
});
export function endReasonLabel(code) {
  if (code === null || code === undefined || code === '') return DASH;
  return Object.hasOwn(END_REASON_LABEL, code) ? END_REASON_LABEL[code] : '其他';
}

// ◀/▶ accessible name (W10): 「上一场：{标题}」, falling back to the bare word.
export function neighbourLabel(word, title) {
  return typeof title === 'string' && title.trim() ? `${word}：${title.trim()}` : word;
}

// ---------- 统计 follow-up (W15 / W16, string ids) ----------
// Every key and value goes through encodeURIComponent so string ids such as
// `jp:3f9a1c2b:12` survive the path/query round-trip. Empty values are skipped.
export function historyQuery(params) {
  const parts = [];
  for (const [key, value] of Object.entries(params || {})) {
    if (value === undefined || value === null || value === '') continue;
    parts.push(`${encodeURIComponent(key)}=${encodeURIComponent(String(value))}`);
  }
  return parts.join('&');
}
const posInt = n => Number.isInteger(n) && n > 0;
// W15: the server clamped the requested range to its retention.
export function clampNoticeText(days) {
  return posInt(days) ? `所选范围超出保留期，仅显示最近 ${days} 天` : null;
}
// W16: some nodes keep less history than the range covers.
export function partialNodesText(list) {
  if (!Array.isArray(list)) return null;
  const segs = list.filter(p => p && typeof p.node === 'string' && p.node && posInt(p.clamped_to_days))
    .map(p => `${p.node} 仅含最近 ${p.clamped_to_days} 天`);
  return segs.length ? `部分节点保留期较短：${segs.join('、')}` : null;
}
// Merge retention hints from several /history responses: the tightest clamp
// wins, and per node the smallest day count wins (first-seen node order).
export function mergeRetention(datas) {
  let clamp = null;
  const nodes = new Map();
  for (const d of datas || []) {
    if (!d || typeof d !== 'object') continue;
    if (posInt(d.clamped_to_days)) clamp = clamp == null ? d.clamped_to_days : Math.min(clamp, d.clamped_to_days);
    for (const p of Array.isArray(d.partial_nodes) ? d.partial_nodes : []) {
      if (!p || typeof p.node !== 'string' || !p.node || !posInt(p.clamped_to_days)) continue;
      nodes.set(p.node, nodes.has(p.node) ? Math.min(nodes.get(p.node), p.clamped_to_days) : p.clamped_to_days);
    }
  }
  return { clamped: clampNoticeText(clamp), partial: partialNodesText([...nodes].map(([node, clamped_to_days]) => ({ node, clamped_to_days }))) };
}
export const EVENTS_EMPTY = '暂无事件记录';
