// analytics-format.js — pure helpers for the 统计 view. No DOM, no network,
// no uPlot. History timestamps from /api/history/* are unix seconds; every
// helper here takes seconds unless its name says otherwise.

import { localHourOffset } from './format.js';

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
