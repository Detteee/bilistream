// analytics.js — the 统计 view (webui-design.md W1–W14). Lazy chunk: main.js
// loads it with import('./analytics.js') when 统计 is opened.
//
// Imports only uPlot and its own pure helpers. App modules that hold state
// (api.js auth, state.js) arrive through `deps`, so esbuild keeps the admin
// page at exactly two files (build.mjs enforces this).
// DOM: createElement/textContent only; dynamic sizes through CSSOM (CSP).

/*! uPlot v1.6.32 (μPlot) | Copyright (c) 2025, Leon Sorokin | MIT License | https://github.com/leeoniya/uPlot */
// uPlot's own header is a plain /** */ comment that esbuild drops, so the
// notice above (a legal comment) keeps the MIT attribution in the bundle.
import uPlot from 'uplot';
import {
  DASH, orDash, formatCount, formatHours, formatPercent, formatAbsolute, formatRelative, formatRange,
  formatDelta, resolutionLabel, bucketFor, presetRange, retentionLine, coverageLine, stripCells, sparkPath,
  ringArcs, biliTotals, biliStateLabel, aggregateAudience, sessionDuration, formatSessionDuration,
  expectedResolution, customRange, missingNodeText, heatLevel, platformKey, platformName, reasonLabel,
  formatMonthDay, formatClockShort, localHourOffset, endReasonLabel, neighbourLabel,
} from './analytics-format.js';

const SVG_NS = 'http://www.w3.org/2000/svg';
const DAY = 86400;
const FRESH_MS = 30_000;
const STALE_MS = 600_000;
const CACHE_MAX = 20;
const AUDIENCE_HINT = '数据来自 B 站非公开接口，可能缺失。在线人数有上限，仅供参考。';
const NOT_SUPPORTED = '当前后端版本不支持统计，请升级 bilistream。';

let deps = null;
let root = null;
let mounted = false;
let generation = 0;
let autoTimer = 0;
let liveTimer = 0;
let liveInFlight = false;
let hoverPause = false;
let themeObserver = null;
const cache = new Map();
const charts = new Map();
const view = { status: null, sessions: [], range: null, eventsCursor: null, detailId: null };

const prefs = {
  range: readPref('analyticsRange', { preset: '7d' }),
  scope: readPref('analyticsScope', ''),
  quality: readPref('analyticsQuality', 'live'),
};

function readPref(key, fallback) {
  try {
    const raw = localStorage.getItem(key);
    return raw === null ? fallback : JSON.parse(raw);
  } catch { return fallback; }
}
function writePref(key, value) {
  try { localStorage.setItem(key, JSON.stringify(value)); } catch { /* storage unavailable */ }
}

// ---------- DOM helpers ----------
function el(tag, className, text) {
  const node = document.createElement(tag);
  if (className) node.className = className;
  if (text !== undefined && text !== null) node.textContent = text;
  return node;
}
function svg(tag, attrs = {}) {
  const node = document.createElementNS(SVG_NS, tag);
  for (const [key, value] of Object.entries(attrs)) node.setAttribute(key, String(value));
  return node;
}
const $ = id => root.querySelector(`#${id}`);
const moduleEl = key => root.querySelector(`[data-module="${key}"]`);
const bodyOf = key => moduleEl(key)?.querySelector('.panel__body');

function setBody(key, ...children) {
  const body = bodyOf(key);
  if (!body) return;
  destroyChartsIn(body);
  body.setAttribute('aria-busy', 'false');
  body.replaceChildren(...children);
}
function setEmpty(key, text, action) {
  const box = el('div', 'empty-state', text);
  if (action) box.append(action);
  setBody(key, box);
}
function setLoading(key) {
  const body = bodyOf(key);
  if (!body || body.dataset.loaded) return; // refresh keeps old data (W4)
  body.setAttribute('aria-busy', 'true');
  body.replaceChildren(el('div', 'empty-state', '正在加载…'));
}
function setError(key, error) {
  const retry = el('button', 'btn-secondary compact-btn analytics-module-retry', '重试');
  retry.type = 'button';
  retry.addEventListener('click', () => refresh({ force: true }));
  setEmpty(key, `加载失败：${errorText(error)}`, retry);
}
function markLoaded(key) {
  const body = bodyOf(key);
  if (body) body.dataset.loaded = '1';
}
function errorText(error) {
  return (error && error.message) || '未知错误';
}
function valueWithUnit(className, value, unit) {
  const node = el('span', className, value);
  if (unit && value !== DASH) node.append(el('small', '', unit));
  return node;
}
function dataTable(head, rows) {
  const details = el('details', 'analytics-data');
  details.append(el('summary', '', '查看数据表'));
  const scroll = el('div', 'analytics-data-scroll');
  const table = el('table');
  const thead = el('thead');
  const tr = el('tr');
  for (const h of head) tr.append(el('th', '', h));
  thead.append(tr);
  const tbody = el('tbody');
  for (const row of rows) {
    const line = el('tr');
    for (const cell of row) line.append(el('td', '', cell));
    tbody.append(line);
  }
  table.append(thead, tbody);
  scroll.append(table);
  details.append(scroll);
  return details;
}

// ---------- data ----------
function resolvedRange() {
  const r = prefs.range;
  if (r?.preset === 'custom' && Number.isFinite(r.from) && Number.isFinite(r.to)) return { ...r };
  return presetRange(['24h', '7d', '30d'].includes(r?.preset) ? r.preset : '7d');
}
function scopeParam() {
  return prefs.scope ? { scope: prefs.scope } : {};
}
function query(params) {
  const search = new URLSearchParams();
  for (const [key, value] of Object.entries(params)) {
    if (value !== undefined && value !== null && value !== '') search.set(key, String(value));
  }
  return search.toString();
}
class NotSupported extends Error {}

// Stale-while-revalidate (W14 P5): fresh 30 s, stale ≤ 10 min (render, then
// revalidate), at most 20 entries. Keys round `to` to the minute so a preset
// keeps its key for a while.
async function history(name, params, onRevalidated) {
  const path = `/api/history/${name}?${query(params)}`;
  const hit = cache.get(path);
  const age = hit ? Date.now() - hit.at : Infinity;
  const fetchFresh = () => deps.getJson(path).then(result => {
    const data = result?.data ?? null;
    cache.delete(path);
    cache.set(path, { at: Date.now(), data });
    while (cache.size > CACHE_MAX) cache.delete(cache.keys().next().value);
    return data;
  }, error => {
    if (error?.status === 404) throw new NotSupported(NOT_SUPPORTED);
    throw error;
  });
  if (hit && age < FRESH_MS) return hit.data;
  if (hit && age < STALE_MS && onRevalidated) {
    fetchFresh().then(onRevalidated, () => {});
    return hit.data;
  }
  return fetchFresh();
}
function roundedRange(r) {
  return { from: Math.floor(r.from / 60) * 60, to: Math.floor(r.to / 60) * 60 };
}

// ---------- head / notice ----------
function showNotice(text, tone = '', nodes = null) {
  const notice = $('analytics-notice');
  notice.hidden = !text;
  notice.dataset.tone = tone;
  notice.setAttribute('role', tone === 'danger' ? 'alert' : 'status');
  if (!nodes) { notice.textContent = text || ''; return; }
  // Partial data: node names carry the server's detail only as a tooltip.
  notice.replaceChildren(document.createTextNode('部分数据缺失：'));
  nodes.forEach((node, index) => {
    if (index) notice.append(document.createTextNode('；'));
    const name = el('span', '', node.node_id);
    if (node.title) name.title = node.title;
    notice.append(name, document.createTextNode(` ${node.reason}`));
  });
  notice.append(document.createTextNode('，以下统计不含这些节点。'));
}
function applyMissing(missing) {
  const info = missingNodeText(missing);
  if (info) showNotice(info.text, 'warn', info.nodes);
}

const KPI_DEFS = [
  { key: 'live_secs', name: '推流时长', unit: '小时', fmt: formatHours, delta: (a, b) => (a - b) / 3600, dUnit: ' 小时' },
  { key: 'availability', name: '推流可用率', unit: '', fmt: formatPercent, delta: (a, b) => (a - b) * 100, dUnit: '%' },
  { key: 'failures', name: '异常重启', unit: '次', fmt: formatCount, delta: (a, b) => a - b, dUnit: '', invert: true, tone: 'warn' },
  { key: 'recover_secs_avg', name: '平均恢复', unit: '秒', fmt: formatCount, delta: (a, b) => a - b, dUnit: ' 秒', invert: true },
  { key: 'sessions', name: '开播场次', unit: '场', fmt: formatCount, delta: (a, b) => a - b, dUnit: '' },
  { key: 'notify_sent', name: 'QQ 告警', unit: '条', fmt: formatCount, delta: null },
];
function deltaPrefix(r) {
  if (r.preset === '7d') return '较上周';
  if (r.preset === '24h') return '较前一天';
  return '较上一时段';
}
function renderKpis(summary, r, empty = false) {
  const grid = $('analytics-kpis');
  grid.setAttribute('aria-busy', 'false');
  const k = summary?.kpis || {};
  const prev = summary?.prev_kpis || {};
  const tiles = KPI_DEFS.map(def => {
    const tile = el('div', 'analytics-kpi discovery-tile--ok');
    const name = el('span', 'discovery-tile-name');
    name.append(el('span', 'discovery-tile-dot'), document.createTextNode(def.name));
    name.firstChild.setAttribute('aria-hidden', 'true');
    const value = empty ? DASH : orDash(k[def.key], def.fmt);
    tile.append(name, valueWithUnit('analytics-kpi-value', value, def.unit));
    let sub;
    if (empty) sub = el('span', 'analytics-kpi-delta', '开始记录后显示');
    else if (!def.delta) sub = el('span', 'analytics-kpi-delta', Number.isFinite(k.notify_failed) && k.notify_failed > 0 ? `${formatCount(k.notify_failed)} 条未送达` : '全部送达');
    else {
      const ok = Number.isFinite(k[def.key]) && Number.isFinite(prev[def.key]);
      const d = formatDelta(ok ? def.delta(k[def.key], prev[def.key]) : null, def.dUnit, def.invert);
      sub = el('span', 'analytics-kpi-delta', `${deltaPrefix(r)} ${d.text}`);
      if (d.tone) sub.dataset.tone = d.tone;
    }
    tile.append(sub);
    const series = (summary?.buckets || []).map(b => kpiBucketValue(def.key, b));
    if (!empty && series.some(Number.isFinite)) {
      const s = svg('svg', { class: 'analytics-spark analytics-kpi-spark', viewBox: '0 0 120 22', preserveAspectRatio: 'none', 'aria-hidden': 'true' });
      s.dataset.tone = 'brand';
      s.append(svg('path', { d: sparkPath(series, null, null, 120, 22) }));
      tile.append(s);
    }
    return tile;
  });
  grid.replaceChildren(...tiles);
}
function kpiBucketValue(key, b) {
  if (key === 'live_secs') return Object.values(b.live_secs || {}).reduce((a, v) => a + (Number(v) || 0), 0);
  return Number.isFinite(b[key]) ? b[key] : null;
}

// ---------- 2 转播状态 ----------
function renderStrips(sessions, r, coverageFrom) {
  const groups = new Map();
  for (const s of sessions) {
    const key = `${s.platform}:${s.channel_id}`;
    if (!groups.has(key)) groups.set(key, { platform: s.platform, name: s.channel_name || s.channel_id || DASH, list: [] });
    groups.get(key).list.push(s);
  }
  if (!groups.size) return setEmpty('strip', '此时间范围内没有转播。', thirtyDaysButton());
  const hours = (r.to - r.from) / 48 / 3600;
  $('analytics-strip-sub').textContent = `每格 ${hours >= 1 ? formatCount(Math.round(hours * 10) / 10) : formatCount(hours * 60)} ${hours >= 1 ? '小时' : '分钟'} · 按频道`;
  const list = el('ol', 'analytics-strips');
  const tableRows = [];
  for (const g of groups.values()) {
    const cells = stripCells(g.list, r.from, r.to, 48, coverageFrom);
    const failures = g.list.reduce((n, s) => n + (Number(s.failures) || 0), 0);
    let live = 0, down = 0;
    for (const s of g.list) {
      const a = Math.max(r.from, s.started_at), b = Math.min(r.to, s.ended_at ?? r.to);
      if (b > a) live += b - a;
      for (const [ga, gb, kind] of s.gaps || []) if (kind === 'failed') down += Math.max(0, Math.min(b, gb ?? b) - Math.max(a, ga));
    }
    const uptime = live > 0 ? (live - down) / live : null;
    const summaryText = failures ? `异常 ${formatCount(failures)} 次` : '无异常';
    const row = el('li', 'analytics-strip-row');
    const name = el('span', 'analytics-strip-name', g.name);
    const small = el('small');
    const dot = el('span', 'analytics-platform-dot');
    dot.dataset.platform = platformKey(g.platform);
    dot.setAttribute('aria-hidden', 'true');
    small.append(dot, document.createTextNode(platformName(g.platform)));
    name.append(small);
    const strip = el('span', 'analytics-strip');
    strip.setAttribute('role', 'img');
    strip.setAttribute('aria-label', `${g.name}，${platformName(g.platform)}：可用率 ${formatPercent(uptime)}，${summaryText}`);
    for (const cell of cells) {
      const c = el('span', 'analytics-strip-cell');
      c.dataset.state = cell.state;
      const hit = g.list.find(s => s.started_at < cell.end && (s.ended_at ?? r.to) > cell.start);
      if (hit) c.addEventListener('click', () => openDetail(hit.id));
      strip.append(c);
    }
    const sum = el('span', 'analytics-strip-sum', formatPercent(uptime));
    sum.append(el('small', '', summaryText));
    row.append(name, strip, sum);
    list.append(row);
    tableRows.push([g.name, platformName(g.platform), formatPercent(uptime), formatCount(failures)]);
  }
  const legend = el('ul', 'analytics-legend');
  for (const [state, label] of [['live', '转播中'], ['warn', '重连'], ['fail', '异常中断'], ['idle', '源未开播'], ['gap', '未记录']]) {
    const li = el('li');
    const sw = el('span', 'analytics-strip-cell analytics-swatch');
    sw.dataset.state = state;
    li.append(sw, document.createTextNode(label));
    legend.append(li);
  }
  const fig = el('figure', 'analytics-chart');
  fig.append(list, legend, dataTable(['频道', '平台', '可用率', '异常'], tableRows));
  setBody('strip', fig);
  markLoaded('strip');
}
function thirtyDaysButton() {
  const b = el('button', 'btn-ghost compact-btn analytics-more', '查看 30 天');
  b.type = 'button';
  b.addEventListener('click', () => selectRange({ preset: '30d' }));
  return b;
}

// ---------- 3 推流时长 ----------
function renderHours(summary, r) {
  const buckets = summary?.buckets || [];
  const b = bucketFor(r);
  $('analytics-hours-sub').textContent = `${b.label} · 按来源平台`;
  const total = x => ['YT', 'TW', 'NC'].reduce((n, p) => n + (Number(x.live_secs?.[p]) || 0), 0);
  if (!buckets.some(x => total(x) > 0)) return setEmpty('hours', '此时间范围内没有转播。', thirtyDaysButton());
  const max = Math.max(1, ...buckets.map(total));
  const bars = el('div', 'analytics-bars');
  bars.style.setProperty('--n', String(buckets.length));
  bars.setAttribute('role', 'img');
  const sums = { YT: 0, TW: 0, NC: 0 };
  for (const bucket of buckets) {
    const col = el('div', 'analytics-bar');
    for (const p of ['YT', 'TW', 'NC']) {
      const secs = Number(bucket.live_secs?.[p]) || 0;
      sums[p] += secs;
      if (!secs) continue;
      const seg = el('span');
      seg.dataset.platform = platformKey(p);
      seg.style.height = `${(secs / max) * 100}%`;
      col.append(seg);
    }
    bars.append(col);
  }
  const all = sums.YT + sums.TW + sums.NC;
  const top = Object.entries(sums).sort((x, y) => y[1] - x[1])[0];
  bars.setAttribute('aria-label', `本范围推流 ${formatHours(all)} 小时，${platformName(top[0])} 占 ${formatPercent(top[1] / all)}`);
  const legend = el('ul', 'analytics-legend');
  for (const p of ['YT', 'TW', 'NC']) {
    const li = el('li');
    const sw = el('span', 'analytics-swatch');
    sw.dataset.platform = platformKey(p);
    li.append(sw, document.createTextNode(platformName(p)));
    legend.append(li);
  }
  const label = x => (b.bucket === 'day' ? formatMonthDay(x.start) : `${formatMonthDay(x.start)} ${formatClockShort(x.start)}`);
  const fig = el('figure', 'analytics-chart');
  fig.append(bars, legend, dataTable(['时段', 'YouTube (小时)', 'Twitch (小时)', 'Niconico (小时)'],
    buckets.map(x => [label(x), formatHours(x.live_secs?.YT), formatHours(x.live_secs?.TW), formatHours(x.live_secs?.NC)])));
  setBody('hours', fig);
  markLoaded('hours');
}

// ---------- 4 观众 ----------
function audienceTiles(t, { sessions = null, summarizedSub = '' } = {}) {
  const tiles = el('div', 'analytics-stats');
  tiles.setAttribute('aria-label', '观众汇总');
  const pendingSub = biliStateLabel(t.state);
  const add = (name, value, unit, sub) => {
    const tile = el('div', 'analytics-stat');
    tile.append(el('span', 'analytics-stat-name', name), valueWithUnit('analytics-stat-value', value, unit), el('span', 'analytics-stat-sub', sub || ''));
    tiles.append(tile);
  };
  add('在线峰值', orDash(t.online_peak), '人', '在线（有上限）');
  add('看过人数', orDash(t.watched), '人', pendingSub || summarizedSub);
  add('弹幕', orDash(t.danmu), '条', pendingSub);
  add('新增粉丝', orDash(t.add_fans), '人', pendingSub);
  add('新增粉丝团', orDash(t.new_fans_club), '人', pendingSub);
  // 最大在线 only when present (MaxOnline unverified upstream); omitted otherwise.
  if (Number.isFinite(t.max_online)) add('最大在线', formatCount(t.max_online), '人', '未验证');
  void sessions;
  return tiles;
}
function renderAudience(stats, r) {
  const select = $('analytics-aud-session');
  const chosen = view.sessions.find(s => String(s.id) === select.value);
  const keep = select.value;
  select.replaceChildren(new Option('全部场次（本范围）', ''));
  for (const s of view.sessions) select.append(new Option(`${formatAbsolute(s.started_at)} ${s.channel_name || s.channel_id || ''}`, String(s.id)));
  select.value = chosen ? keep : '';
  syncDetailButton();
  if (!view.sessions.length) return setEmpty('audience', '此时间范围内没有转播。', thirtyDaysButton());
  let totals;
  let sub = '';
  if (chosen) totals = biliTotals(chosen.bili);
  else {
    const agg = aggregateAudience(view.sessions);
    totals = { ...agg, state: agg.summarized ? 'ok' : agg.pending ? 'pending' : 'failed' };
    sub = agg.summarized ? `${formatCount(agg.summarized)} 场合计` : '';
  }
  const children = [audienceTiles(totals, { summarizedSub: sub })];
  const online = stats?.series?.online;
  const watched = stats?.series?.watched;
  if (stats?.ts?.length && (online || watched)) {
    const caption = `${formatRange(r.from, r.to)} · ${resolutionLabel(stats.resolution || expectedResolution(r.from, r.to, view.status?.retention))}`;
    children.push(chartFigure('audience', caption, stats, [
      { key: 'online', label: '在线（有上限）', tone: '--brand' },
      { key: 'watched', label: '看过（累计）', tone: '--teal' },
    ], r.to - r.from, `在线峰值 ${orDash(totals.online_peak)}（有上限），看过人数 ${orDash(totals.watched)}。`));
  } else if (totals.state === 'none' || !Number.isFinite(totals.online_peak)) {
    children.push(el('p', 'empty-state', '此时间范围内没有 B 站观众数据。'));
  }
  children.push(el('small', 'settings-help-text', AUDIENCE_HINT));
  setBody('audience', ...children);
  markLoaded('audience');
  mountPendingCharts(bodyOf('audience'));
}

function syncDetailButton() {
  const button = $('analytics-aud-detail');
  if (button) button.hidden = !$('analytics-aud-session').value;
}

// ---------- uPlot ----------
function cssVar(name) {
  return getComputedStyle(root || document.documentElement).getPropertyValue(name).trim();
}
function mix(color, alpha) {
  return `color-mix(in srgb, ${color} ${Math.round(alpha * 100)}%, transparent)`;
}
function chartFigure(id, caption, stats, series, span, summary) {
  const fig = el('figure', 'analytics-chart analytics-section');
  const cap = el('figcaption', 'analytics-caption', caption);
  cap.id = `analytics-${id}-cap`;
  const host = el('div', 'analytics-uplot');
  host.id = `analytics-${id}-chart`;
  host.tabIndex = 0;
  host.setAttribute('role', 'img');
  host.setAttribute('aria-labelledby', cap.id);
  const sr = el('p', 'sr-only', summary);
  sr.id = `analytics-${id}-sum`;
  host.setAttribute('aria-describedby', sr.id);
  const live = el('p', 'sr-only');
  live.setAttribute('aria-live', 'polite');
  host._pending = { id, stats, series, span, live };
  const rows = stats.ts.map((t, i) => [formatAbsolute(t), ...series.map(s => orDash(stats.series?.[s.key]?.mean?.[i]))]);
  fig.append(cap, host, sr, live, dataTable(['时间', ...series.map(s => s.label)], rows.length > 500 ? rows.filter((_, i) => i % Math.ceil(rows.length / 500) === 0) : rows));
  return fig;
}
function mountPendingCharts(container) {
  if (!container) return;
  for (const host of container.querySelectorAll('.analytics-uplot')) {
    if (host._pending) mountChart(host, host._pending);
  }
}
function uplotOptions(series, span, width) {
  const font = `11px ${cssVar('--font-sans') || 'sans-serif'}`;
  const axis = { stroke: cssVar('--text-mute'), grid: { stroke: cssVar('--line'), width: 1 }, ticks: { stroke: cssVar('--line') }, font };
  const timeFmt = (u, splits) => splits.map(s => (span <= DAY ? formatClockShort(s) : formatMonthDay(s)));
  const scales = { x: { time: true } };
  const uSeries = [{}];
  const bands = [];
  const axes = [{ ...axis, values: timeFmt }];
  series.forEach((s, index) => {
    const color = cssVar(s.tone) || cssVar('--brand');
    const scale = index === 0 ? 'y' : `y${index}`;
    scales[scale] = {};
    axes.push({ ...axis, scale, side: index === 0 ? 3 : 1, size: 56, grid: index === 0 ? axis.grid : { show: false },
      values: (u, splits) => splits.map(v => formatCount(v)) });
    uSeries.push({ label: s.label, scale, stroke: color, width: 1.5, points: { show: false }, spanGaps: false });
    if (s.band) {
      uSeries.push({ label: `${s.label} 最小`, scale, stroke: 'transparent', points: { show: false } });
      uSeries.push({ label: `${s.label} 最大`, scale, stroke: 'transparent', points: { show: false } });
      bands.push({ series: [uSeries.length - 1, uSeries.length - 2], fill: mix(color, 0.18) });
    }
  });
  return {
    width, height: 180, legend: { show: false }, cursor: { drag: { x: true, y: false } },
    scales, axes, series: uSeries, bands,
  };
}
function mountChart(host, spec) {
  charts.get(host)?.chart.destroy();
  const { stats, series, span, live } = spec;
  const data = [stats.ts];
  for (const s of series) {
    const col = stats.series?.[s.key] || {};
    data.push(col.mean || stats.ts.map(() => null));
    if (s.band) data.push(col.min || stats.ts.map(() => null), col.max || stats.ts.map(() => null));
  }
  const chart = new uPlot(uplotOptions(series, span, Math.max(240, host.clientWidth || 480)), data, host);
  for (const canvas of host.querySelectorAll('canvas')) canvas.setAttribute('aria-hidden', 'true');
  let frame = 0;
  const resize = new ResizeObserver(() => {
    cancelAnimationFrame(frame);
    frame = requestAnimationFrame(() => { if (host.clientWidth) chart.setSize({ width: host.clientWidth, height: 180 }); });
  });
  resize.observe(host);
  let announce = 0;
  host.onkeydown = event => {
    const step = event.key === 'ArrowRight' ? 1 : event.key === 'ArrowLeft' ? -1 : 0;
    if (!step || !stats.ts.length) return;
    event.preventDefault();
    const idx = Math.max(0, Math.min(stats.ts.length - 1, (chart.cursor.idx ?? -1) + step));
    chart.setCursor({ left: chart.valToPos(stats.ts[idx], 'x'), top: 10 });
    clearTimeout(announce);
    announce = setTimeout(() => {
      live.textContent = `${formatAbsolute(stats.ts[idx])}：${series.map(s => `${s.label} ${orDash(stats.series?.[s.key]?.mean?.[idx])}`).join('，')}`;
    }, 250);
  };
  charts.set(host, { chart, resize, spec });
  host._pending = null;
}
function destroyChartsIn(container) {
  for (const [host, entry] of charts) {
    if (container.contains(host)) {
      entry.chart.destroy();
      entry.resize.disconnect();
      charts.delete(host);
    }
  }
}
function rebuildCharts() {
  for (const [host, entry] of [...charts]) {
    entry.chart.destroy();
    entry.resize.disconnect();
    charts.delete(host);
    if (host.isConnected) mountChart(host, entry.spec);
  }
}

// ---------- 5 推流质量 ----------
function renderQualityRange(stats, r) {
  $('analytics-quality-sub').textContent = resolutionLabel(stats?.resolution || expectedResolution(r.from, r.to, view.status?.retention));
  if (!stats?.ts?.length || !stats.series?.bitrate) return setEmpty('quality', '此时间范围内没有推流采样。');
  const fig = chartFigure('quality', `${formatRange(r.from, r.to)} · ${resolutionLabel(stats.resolution)}`, stats, [
    { key: 'bitrate', label: '码率 (kbps)', tone: '--brand', band: true },
    { key: 'fps', label: '帧率', tone: '--teal' },
  ], r.to - r.from, '本范围推流码率与帧率，最小–最大值以浅色带显示。');
  setBody('quality', fig, el('small', 'settings-help-text', '丢帧（ffmpeg drop=）尚未采集。'));
  markLoaded('quality');
  mountPendingCharts(bodyOf('quality'));
}
async function refreshLive() {
  if (!mounted || prefs.quality !== 'live' || hoverPause || liveInFlight || !deps.isDashboardVisible()) return;
  liveInFlight = true;
  try {
    const result = await deps.getJson('/api/network-status');
    if (!mounted || prefs.quality !== 'live') return;
    renderLive(result?.data || {});
  } catch (error) {
    setError('quality', error);
  } finally { liveInFlight = false; }
}
function renderLive(n) {
  $('analytics-quality-sub').textContent = hoverPause ? '已暂停（移开鼠标后继续）' : '最近 60 秒 · 悬停时暂停';
  if (!n.ffmpeg_running) return setEmpty('quality', '当前没有转播。开始转播后显示实时码率、帧率和速度。');
  const rows = el('div', 'analytics-sparks');
  const hist = Array.isArray(n.stream_bitrate_history) ? n.stream_bitrate_history.slice(-60) : [];
  const row = (label, values, tone, value) => {
    const line = el('div', 'analytics-spark-row');
    const s = svg('svg', { class: 'analytics-spark', viewBox: '0 0 300 36', preserveAspectRatio: 'none', 'aria-hidden': 'true' });
    s.dataset.tone = tone;
    const d = sparkPath(values, 0, null, 300, 36);
    if (d) s.append(svg('path', { d }));
    line.append(el('span', '', label), s, el('span', 'analytics-spark-value', value));
    rows.append(line);
  };
  row('码率', hist, 'brand', deps.formatNetworkRate(n.stream_bitrate_kbps));
  row('帧率', [], 'teal', deps.formatFps(n.stream_fps));
  row('速度', [], 'ok', deps.formatSpeedRatio(n.stream_speed));
  setBody('quality', rows, el('small', 'settings-help-text', '来自 ffmpeg 实时统计，停止推流后清空。'));
}
function startLive() {
  clearInterval(liveTimer);
  liveTimer = 0;
  if (prefs.quality === 'live') {
    refreshLive();
    liveTimer = setInterval(refreshLive, 1000);
  }
}

// ---------- 6 推流可靠性 ----------
function renderReliability(summary) {
  const rel = summary?.reliability;
  const k = summary?.kpis || {};
  if (!rel || !(rel.clean + rel.counted_failures + rel.planned > 0)) return setEmpty('reliability', '暂无推流会话。转播结束后显示成功率和故障原因。');
  const arcs = ringArcs(rel.clean, rel.counted_failures, 2 * Math.PI * 52);
  const ring = svg('svg', { class: 'analytics-ring', viewBox: '0 0 128 128', role: 'img', 'aria-label': `成功率 ${formatPercent(arcs.rate)}` });
  ring.append(
    svg('circle', { class: 'ring-track', cx: 64, cy: 64, r: 52 }),
    svg('circle', { class: 'ring-ok', cx: 64, cy: 64, r: 52, 'stroke-dasharray': arcs.ok, transform: 'rotate(-90 64 64)' }),
    svg('circle', { class: 'ring-fail', cx: 64, cy: 64, r: 52, 'stroke-dasharray': arcs.fail, 'stroke-dashoffset': arcs.failOffset, transform: 'rotate(-90 64 64)' }),
  );
  const t = svg('text', { x: 64, y: 66 });
  t.textContent = formatPercent(arcs.rate);
  const sub = svg('text', { x: 64, y: 84, class: 'ring-sub' });
  sub.textContent = `${formatCount(rel.clean + rel.counted_failures)} 个会话`;
  ring.append(t, sub);
  const meta = el('dl', 'cluster-node-meta');
  for (const [label, value] of [['正常结束', formatCount(rel.clean)], ['异常退出', formatCount(rel.counted_failures)], ['计划内重启', formatCount(rel.planned)], ['平均恢复', k.recover_secs_avg == null ? DASH : `${formatCount(k.recover_secs_avg)} 秒`]]) {
    const row = el('div', 'cluster-node-meta-row');
    row.append(el('dt', '', label), el('dd', '', value));
    meta.append(row);
  }
  const rate = el('div', 'analytics-rate');
  rate.append(ring, meta);
  const children = [rate];
  const reasons = Array.isArray(rel.reasons) ? rel.reasons.filter(x => x.count > 0) : [];
  const cap = el('p', 'analytics-caption analytics-section', '故障原因（异常退出与告警，按次数）');
  if (!reasons.length) children.push(cap, el('p', 'empty-state', '此时间范围内没有故障。'));
  else {
    const max = Math.max(...reasons.map(x => x.count));
    const list = el('ol', 'analytics-hbars');
    for (const reason of reasons.sort((a, b) => b.count - a.count)) {
      const li = el('li', 'analytics-hbar');
      const track = el('span', 'analytics-hbar-track');
      const fill = el('span', 'analytics-hbar-fill');
      fill.dataset.tone = 'danger';
      fill.style.width = `${(reason.count / max) * 100}%`;
      track.append(fill);
      li.append(el('span', 'analytics-hbar-label', reasonLabel(reason.class)), track, el('span', 'analytics-hbar-count', formatCount(reason.count)));
      list.append(li);
    }
    children.push(cap, list);
  }
  setBody('reliability', ...children);
  markLoaded('reliability');
}

// ---------- 7 开播日历 ----------
function renderCalendar(summary) {
  const days = Array.isArray(summary?.calendar) ? summary.calendar : [];
  if (!days.length) return setEmpty('calendar', '记录满一天后显示开播日历。');
  const max = Math.max(0, ...days.map(d => Number(d.live_secs) || 0));
  const coverage = summary?.coverage?.from;
  const pad = (new Date(days[0].day * 1000).getDay() + 6) % 7; // rows Monday..Sunday
  const grid = el('div', 'analytics-heatmap');
  grid.style.setProperty('--weeks', String(Math.ceil((days.length + pad) / 7)));
  grid.setAttribute('role', 'img');
  const total = days.reduce((n, d) => n + (Number(d.live_secs) || 0), 0);
  grid.setAttribute('aria-label', `近 13 周推流 ${formatHours(total)} 小时，${formatCount(days.filter(d => d.failures > 0).length)} 天有异常`);
  for (const label of ['一', '', '三', '', '五', '', '日']) grid.append(el('span', 'analytics-heat-day', label));
  for (let i = 0; i < pad; i++) grid.append(el('span'));
  for (const d of days) {
    const cell = el('span', 'analytics-heat');
    cell.dataset.level = String(heatLevel(d.live_secs, max));
    if (d.failures > 0) cell.dataset.fail = '';
    if (Number.isFinite(coverage) && d.day + DAY <= coverage) cell.dataset.gap = '';
    cell.title = `${formatMonthDay(d.day)} · ${formatHours(d.live_secs)} 小时${d.failures > 0 ? ` · 异常 ${d.failures} 次` : ''}`;
    grid.append(cell);
  }
  const wrap = el('div', 'analytics-heatmap-wrap');
  wrap.append(grid);
  const legend = el('ul', 'analytics-legend');
  for (const [label, attrs] of [['少', { level: '1' }], ['多', { level: '4' }], ['有异常', { fail: '' }], ['未记录', { gap: '' }]]) {
    const li = el('li');
    const sw = el('span', 'analytics-heat');
    Object.assign(sw.dataset, attrs);
    li.append(sw, document.createTextNode(label));
    legend.append(li);
  }
  const fig = el('figure', 'analytics-chart');
  fig.append(wrap, legend, dataTable(['日期', '推流时长 (小时)', '异常'], days.map(d => [formatMonthDay(d.day), formatHours(d.live_secs), formatCount(d.failures)])));
  setBody('calendar', fig);
  markLoaded('calendar');
}

// ---------- 8 节点对比 ----------
function renderNodes(summary) {
  const nodes = Array.isArray(summary?.per_node) ? summary.per_node : [];
  const cluster = nodes.length > 1;
  moduleEl('nodes').hidden = !cluster;
  const scope = $('analytics-scope');
  scope.hidden = !cluster;
  if (cluster) {
    const keep = prefs.scope;
    scope.replaceChildren(new Option('全部节点', ''), ...nodes.map(n => new Option(n.node_id, n.node_id)));
    scope.value = nodes.some(n => n.node_id === keep) ? keep : '';
  }
  if (!cluster) return;
  const missing = new Map((summary.missing_nodes || []).map(m => [m.node_id, missingNodeText([m]).nodes[0]]));
  const total = nodes.reduce((n, x) => n + (Number(x.live_secs) || 0), 0);
  const span = view.range.to - view.range.from;
  const table = el('table', 'analytics-table');
  const head = ['节点', '状态', '在线率', '推流时长', '推流占比', '异常重启', '最近心跳'];
  const thead = el('thead');
  const hr = el('tr');
  for (const h of head) hr.append(el('th', '', h));
  thead.append(hr);
  const tbody = el('tbody');
  for (const n of nodes) {
    const tr = el('tr');
    const miss = missing.get(n.node_id);
    const status = el('span', `cluster-badge${miss ? ' sync-warning' : ''}`, miss ? miss.reason : '正常');
    if (miss?.title) status.title = miss.title;
    const cells = [
      el('td', '', n.node_id), el('td'),
      el('td', '', miss ? DASH : formatPercent(span > 0 && Number.isFinite(n.down_secs) ? 1 - n.down_secs / span : null)),
      el('td', '', miss ? DASH : `${formatHours(n.live_secs)} 小时`),
      el('td', '', miss ? DASH : formatPercent(total > 0 ? n.live_secs / total : null)),
      el('td', '', miss ? DASH : orDash(n.failures)),
      el('td', '', formatRelative(n.last_seen_at)),
    ];
    cells[1].append(status);
    cells.forEach((cell, i) => { if (i) cell.dataset.label = head[i]; });
    tr.append(...cells);
    tbody.append(tr);
  }
  table.append(thead, tbody);
  const scroll = el('div', 'analytics-data-scroll');
  scroll.append(table);
  setBody('nodes', scroll, el('small', 'settings-help-text', '「--」表示未知而非 0。'));
  markLoaded('nodes');
}

// ---------- 9 最近事件 ----------
function eventParams(r, cursor) {
  const anomalies = $('analytics-event-filter').value !== 'all';
  return { ...roundedRange(r), limit: 20, cursor, severity: anomalies ? 'warn,error' : undefined, ...scopeParam() };
}
function eventRow(ev) {
  const li = el('li', 'analytics-event');
  const time = el('time', '', formatRelative(ev.at));
  if (Number.isFinite(ev.at)) {
    time.dateTime = new Date(ev.at * 1000).toISOString();
    time.title = formatAbsolute(ev.at);
  }
  const tone = ev.severity === 'error' ? 'analytics-dot-danger' : ev.severity === 'warn' ? 'status-scheduled' : 'status-live';
  const dot = el('span', `status-indicator ${tone}`);
  dot.setAttribute('aria-hidden', 'true');
  const title = el('span', 'analytics-event-title');
  title.append(el('span', 'sr-only', ev.severity === 'error' ? '异常：' : ev.severity === 'warn' ? '警告：' : '正常：'), document.createTextNode(ev.title || ev.kind || DASH));
  if (ev.detail) title.append(el('small', '', ev.detail));
  li.append(time, dot, title);
  if (ev.node && moduleEl('nodes') && !moduleEl('nodes').hidden) li.append(el('span', 'cluster-badge', ev.node));
  if (ev.session_id != null) {
    li.tabIndex = 0;
    li.addEventListener('click', () => openDetail(ev.session_id));
    li.addEventListener('keydown', e => { if (e.key === 'Enter') openDetail(ev.session_id); });
  }
  return li;
}
function renderEvents(data, append = false) {
  const items = Array.isArray(data?.items) ? data.items : [];
  let list = bodyOf('events')?.querySelector('.analytics-events');
  if (!append || !list) {
    if (!items.length) return setEmpty('events', '此时间范围内没有事件。');
    list = el('ol', 'analytics-events');
    setBody('events', list);
  }
  list.append(...items.map(eventRow));
  view.eventsCursor = data?.next_cursor ?? null;
  bodyOf('events').querySelector('.analytics-more')?.remove();
  if (view.eventsCursor) {
    const more = el('button', 'btn-ghost compact-btn analytics-more', '加载更多');
    more.type = 'button';
    more.addEventListener('click', async () => {
      more.disabled = true;
      try { renderEvents(await history('events', eventParams(view.range, view.eventsCursor)), true); } catch (error) { more.disabled = false; more.textContent = `加载失败：${errorText(error)}`; }
    });
    bodyOf('events').append(more);
  }
  markLoaded('events');
}

// ---------- 单场详情 ----------
async function openDetail(id) {
  const dialog = $('analytics-detail');
  view.detailId = id;
  const body = $('analytics-detail-body');
  destroyChartsIn(body);
  body.replaceChildren(el('div', 'empty-state', '正在加载…'));
  if (!dialog.open) dialog.showModal?.();
  $('analytics-detail-title').focus();
  try {
    const [res, stats] = await Promise.all([
      history('sessions', { id, ...scopeParam() }),
      history('stats', { session: id, metrics: 'bitrate,fps,speed,online,watched', max_points: Math.min(2000, Math.max(240, body.clientWidth || 600)), ...scopeParam() }),
    ]);
    if (view.detailId !== id) return;
    const s = res?.items?.[0];
    if (!s) { body.replaceChildren(el('div', 'empty-state', '未找到该场转播。')); return; }
    const now = Date.now() / 1000;
    $('analytics-detail-title').textContent = s.title || '单场详情';
    const meta = el('dl', 'cluster-node-meta');
    for (const [label, value] of [
      ['频道', `${s.channel_name || s.channel_id || DASH} · ${platformName(s.platform)}`], ['节点', s.node || DASH],
      ['时间', `${formatAbsolute(s.started_at)} ～ ${s.ended_at ? formatAbsolute(s.ended_at) : '进行中'}`],
      ['时长', formatSessionDuration(sessionDuration(s, now))], ['重启', orDash(s.restarts)], ['结束原因', endReasonLabel(s.end_reason)],
    ]) {
      const row = el('div', 'cluster-node-meta-row');
      row.append(el('dt', '', label), el('dd', '', value));
      meta.append(row);
    }
    const children = [meta, el('h3', 'analytics-caption analytics-section', '观众'), audienceTiles(biliTotals(s.bili))];
    if (stats?.ts?.length) {
      const span = sessionDuration(s, now) || DAY;
      children.push(chartFigure('detail-aud', `在线 / 看过 · ${resolutionLabel(stats.resolution)}`, stats, [
        { key: 'online', label: '在线（有上限）', tone: '--brand' }, { key: 'watched', label: '看过（累计）', tone: '--teal' },
      ], span, '本场在线人数与看过人数。'));
      children.push(chartFigure('detail-q', `推流质量 · ${resolutionLabel(stats.resolution)}`, stats, [
        { key: 'bitrate', label: '码率 (kbps)', tone: '--brand', band: true }, { key: 'fps', label: '帧率', tone: '--teal' },
      ], span, '本场推流码率与帧率。'));
    }
    children.push(el('small', 'settings-help-text', AUDIENCE_HINT));
    body.replaceChildren(...children);
    mountPendingCharts(body);
    const prev = $('analytics-detail-prev');
    const next = $('analytics-detail-next');
    prev.disabled = res.prev_id == null;
    next.disabled = res.next_id == null;
    prev.onclick = () => openDetail(res.prev_id);
    next.onclick = () => openDetail(res.next_id);
    const titleOf = sid => view.sessions.find(x => String(x.id) === String(sid))?.title;
    prev.setAttribute('aria-label', neighbourLabel('上一场', titleOf(res.prev_id)));
    next.setAttribute('aria-label', neighbourLabel('下一场', titleOf(res.next_id)));
  } catch (error) {
    body.replaceChildren(el('div', 'empty-state', `加载失败：${errorText(error)}`));
  }
}

// ---------- orchestration ----------
function emptyAll(text, tone = '') {
  showNotice(text, tone);
  renderKpis(null, view.range, true);
  for (const key of ['strip', 'hours', 'audience', 'reliability', 'calendar']) {
    setEmpty(key, moduleEl(key).dataset.empty);
  }
  moduleEl('nodes').hidden = true;
  if (prefs.quality === 'range') setEmpty('quality', moduleEl('quality').dataset.empty || '暂无推流采样。');
  // 实时 and 最近事件 work without history: move them up (W2).
  for (const key of ['quality', 'events']) moduleEl(key).classList.add('analytics-first');
  const head = moduleEl('overview');
  head.after(moduleEl('quality'), moduleEl('events'));
}
function restoreOrder() {
  const events = moduleEl('events');
  if (!events.classList.contains('analytics-first')) return;
  for (const key of ['quality', 'events']) moduleEl(key).classList.remove('analytics-first');
  moduleEl('audience').after(moduleEl('quality'));
  moduleEl('nodes').after(events);
}

async function refresh({ force = false } = {}) {
  if (!mounted) return;
  const gen = ++generation;
  if (force) cache.clear();
  const r = resolvedRange();
  view.range = r;
  const live = () => mounted && gen === generation;
  const button = $('analytics-refresh-btn');
  button.disabled = true;
  const done = () => { if (gen === generation) button.disabled = false; };
  for (const key of ['strip', 'hours', 'audience', 'reliability', 'calendar', 'events']) setLoading(key);
  showNotice('');

  let status;
  try {
    status = await history('status', {});
  } catch (error) {
    if (!live()) return;
    done();
    if (error instanceof NotSupported) { emptyAll(NOT_SUPPORTED); return; }
    showNotice(`无法读取统计数据：${errorText(error)}。实时数据不受影响。`, 'danger');
    const logs = el('button', 'btn-secondary compact-btn analytics-logs-btn', '查看日志');
    logs.type = 'button';
    logs.addEventListener('click', () => document.getElementById('tab-logs')?.click());
    $('analytics-notice').append(document.createTextNode(' '), logs);
    for (const key of ['strip', 'hours', 'audience', 'reliability', 'calendar', 'events']) setError(key, error);
    return;
  }
  if (!live()) return;
  view.status = status;
  $('analytics-retention').textContent = retentionLine(status?.retention);
  const scopeLabel = prefs.scope || '全部节点';
  if (!Number.isFinite(status?.oldest_at)) {
    $('analytics-coverage').textContent = '尚未开始记录';
    const since = Number.isFinite(status?.recording_since) ? formatAbsolute(status.recording_since) : '现在';
    emptyAll(`尚无历史数据。本节点从 ${since} 开始记录推流历史，第一场转播结束后这里会出现统计。「推流质量 · 实时」和「最近事件」现在就可用。`);
    history('events', eventParams(r)).then(d => live() && renderEvents(d), e => live() && setError('events', e));
    done();
    return;
  }
  restoreOrder();
  const coverageFrom = Math.max(r.from, status.oldest_at);
  $('analytics-coverage').textContent = coverageLine(coverageFrom, r.to, scopeLabel, localHourOffset());
  const span = roundedRange(r);
  const bucket = bucketFor(r).bucket;
  const width = Math.min(2000, Math.max(240, Math.round(bodyOf('audience')?.clientWidth || 800)));
  const tz = -new Date().getTimezoneOffset();
  const again = () => { if (live()) refresh(); };

  const summaryJob = history('summary', { ...span, bucket, tz_offset_min: tz, calendar: 1, ...scopeParam() }, again);
  const sessionsJob = history('sessions', { ...span, limit: 500, ...scopeParam() }, again);
  const statsJob = history('stats', { ...span, metrics: 'bitrate,fps,speed,online,watched', max_points: width, ...scopeParam() }, again);
  const eventsJob = history('events', eventParams(r), again);
  let failures = 0;
  const fail = (keys, error) => {
    if (!live()) return;
    failures++;
    keys.forEach(key => setError(key, error));
  };

  summaryJob.then(summary => {
    if (!live()) return;
    renderKpis(summary, r);
    renderHours(summary, r);
    renderReliability(summary);
    renderCalendar(summary);
    renderNodes(summary);
    applyMissing(summary?.missing_nodes);
  }, error => fail(['hours', 'reliability', 'calendar'], error));
  sessionsJob.then(data => {
    if (!live()) return;
    view.sessions = Array.isArray(data?.items) ? data.items : [];
    renderStrips(view.sessions, r, status.oldest_at);
    applyMissing(data?.missing_nodes);
  }, error => fail(['strip', 'audience'], error));
  Promise.all([sessionsJob, statsJob]).then(([, stats]) => {
    if (!live()) return;
    renderAudience(stats, r);
    if (prefs.quality === 'range') renderQualityRange(stats, r);
    view.stats = stats;
  }, error => { if (live()) { setError('audience', error); if (prefs.quality === 'range') setError('quality', error); } });
  eventsJob.then(data => live() && renderEvents(data), error => fail(['events'], error));
  Promise.allSettled([summaryJob, sessionsJob, statsJob, eventsJob]).then(() => {
    done();
    if (live() && failures >= 3) showNotice('无法读取统计数据：多个模块加载失败。实时数据不受影响。', 'danger');
  });
}

function selectRange(range) {
  prefs.range = range;
  writePref('analyticsRange', range);
  for (const input of root.querySelectorAll('input[name="analytics-range"]')) input.checked = input.value === range.preset;
  $('analytics-custom').hidden = range.preset !== 'custom';
  refresh();
}

function startAutoRefresh() {
  clearInterval(autoTimer);
  autoTimer = 0;
  const secs = Number($('analytics-autorefresh').value);
  if (secs > 0) {
    autoTimer = setInterval(() => {
      if (!hoverPause && deps.isDashboardVisible() && !$('analytics-detail').open) refresh({ force: true });
    }, secs * 1000);
  }
}

function toDateInput(sec) {
  const d = new Date(sec * 1000);
  return `${d.getFullYear()}-${String(d.getMonth() + 1).padStart(2, '0')}-${String(d.getDate()).padStart(2, '0')}`;
}

function bindControls() {
  for (const input of root.querySelectorAll('input[name="analytics-range"]')) {
    input.checked = input.value === (prefs.range?.preset || '7d');
    input.addEventListener('change', () => {
      if (input.value === 'custom') {
        $('analytics-custom').hidden = false;
        const r = resolvedRange();
        $('analytics-from').value ||= toDateInput(r.from);
        $('analytics-to').value ||= toDateInput(r.to);
        return;
      }
      selectRange({ preset: input.value });
    });
  }
  $('analytics-custom').hidden = prefs.range?.preset !== 'custom';
  $('analytics-apply').addEventListener('click', () => {
    const result = customRange($('analytics-from').value, $('analytics-to').value, view.status?.retention?.rollup_days);
    $('analytics-custom-error').textContent = result.error || '';
    if (!result.error) selectRange(result);
  });
  $('analytics-refresh-btn').addEventListener('click', () => refresh({ force: true }));
  $('analytics-autorefresh').addEventListener('change', startAutoRefresh);
  $('analytics-scope').addEventListener('change', event => {
    prefs.scope = event.target.value;
    writePref('analyticsScope', prefs.scope);
    refresh();
  });
  $('analytics-aud-session').addEventListener('change', () => { renderAudience(view.stats, view.range); syncDetailButton(); });
  // Keyboard path to 单场详情 (W2, W10): a real button next to the select.
  $('analytics-aud-detail').addEventListener('click', () => {
    const id = $('analytics-aud-session').value;
    if (id) openDetail(id);
  });
  $('analytics-event-filter').addEventListener('change', async () => {
    try { renderEvents(await history('events', eventParams(view.range))); } catch (error) { setError('events', error); }
  });
  for (const input of root.querySelectorAll('input[name="analytics-quality"]')) {
    input.checked = input.value === prefs.quality;
    input.addEventListener('change', () => {
      prefs.quality = input.value;
      writePref('analyticsQuality', prefs.quality);
      bodyOf('quality').dataset.loaded = '';
      if (prefs.quality === 'range') {
        if (view.stats) renderQualityRange(view.stats, view.range);
        else setLoading('quality');
      }
      startLive();
    });
  }
  // Pause live updates and auto refresh while pointer or focus is on a chart.
  const chartTarget = target => target?.closest?.('.analytics-uplot, .analytics-sparks');
  root.addEventListener('pointerover', e => { if (chartTarget(e.target)) hoverPause = true; });
  root.addEventListener('pointerout', e => { if (chartTarget(e.target) && !chartTarget(e.relatedTarget)) hoverPause = false; });
  root.addEventListener('focusin', e => { if (chartTarget(e.target)) hoverPause = true; });
  root.addEventListener('focusout', e => { if (chartTarget(e.target)) hoverPause = false; });
  const dialog = $('analytics-detail');
  $('analytics-detail-close').addEventListener('click', () => dialog.close());
  dialog.addEventListener('close', () => { view.detailId = null; destroyChartsIn($('analytics-detail-body')); });
  dialog.addEventListener('keydown', event => {
    if (event.target.closest?.('.analytics-uplot')) return;
    if (event.key === 'ArrowLeft' && !$('analytics-detail-prev').disabled) $('analytics-detail-prev').click();
    if (event.key === 'ArrowRight' && !$('analytics-detail-next').disabled) $('analytics-detail-next').click();
  });
}

export function mountAnalytics(container, injected) {
  if (!root) {
    deps = injected;
    root = container;
    bindControls();
  }
  mounted = true;
  themeObserver?.disconnect();
  themeObserver = new MutationObserver(rebuildCharts);
  themeObserver.observe(document.documentElement, { attributes: true, attributeFilter: ['class'] });
  startLive();
  startAutoRefresh();
  refresh();
}

export function unmountAnalytics() {
  if (!mounted) return;
  mounted = false;
  generation++;
  clearInterval(liveTimer);
  clearInterval(autoTimer);
  liveTimer = autoTimer = 0;
  themeObserver?.disconnect();
  themeObserver = null;
  if ($('analytics-detail').open) $('analytics-detail').close();
}

export function refreshIfVisible() {
  if (mounted && deps.isDashboardVisible()) refresh();
}
