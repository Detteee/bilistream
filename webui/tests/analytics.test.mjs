import test from 'node:test';
import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import {
  DASH, stopLiveDataToBili, biliTotals, aggregateAudience, sessionDuration, expectedResolution, customRange,
  missingNodeText, streamHeat, spectrumColor, broadcastGroups, biliStateLabel, rangeAudience, watchedSourceLabel, orDash, STOP_LIVE_DATA_FIELDS,
  cleanNotice,
} from '../src/js/analytics-format.js';
import {
  createAnalyticsLoader, renderChunkError, CHUNK_ERROR_TITLE, CHUNK_ERROR_BODY, CHUNK_ERROR_BUTTON,
} from '../src/js/analytics-loader.js';
import { historyRoute } from '../mock/history-fixtures.mjs';

const fixture = async name => JSON.parse(await readFile(new URL(`./fixtures/${name}`, import.meta.url), 'utf8'));
const DAY = 86400;
const NOW = 1_790_000_000;

test('StopLiveData allowlist is code + six fields, with no revenue', () => {
  assert.deepEqual(Object.keys(STOP_LIVE_DATA_FIELDS).sort(), ['AddFans', 'DanmuNum', 'LiveTime', 'MaxOnline', 'NewFansClub', 'WatchedCount']);
  const bili = stopLiveDataToBili({ code: 0, data: { WatchedCount: 1, DanmuNum: 2, AddFans: 3, NewFansClub: 4, MaxOnline: 5, LiveTime: 9, HamsterRmb: 12345 } });
  assert.deepEqual(bili, { state: 'ok', watched: 1, danmu: 2, add_fans: 3, new_fans_club: 4, max_online: 5 });
  assert.doesNotMatch(JSON.stringify(bili), /hamster|rmb|live_time|LiveTime/i, 'no revenue and LiveTime never surfaces');
});

test('the valid live-session fixture maps to totals; duration never comes from LiveTime', async () => {
  const reply = await fixture('stoplivedata_live_session.json');
  const bili = stopLiveDataToBili(reply);
  assert.deepEqual(bili, { state: 'ok', watched: 469, danmu: 457, add_fans: 0, new_fans_club: 0, max_online: 2765 });
  const session = { started_at: NOW - 3600, ended_at: NOW, bili };
  assert.equal(sessionDuration(session), 3600, 'our own start/end, not LiveTime 17434');
  assert.equal(sessionDuration({ started_at: NOW - 60, ended_at: null }, NOW), 60, 'open session runs to now');
  const t = biliTotals(bili);
  assert.equal(t.max_online, 2765, 'MaxOnline stays in the payload so a bad reply can still be rejected');
});

test('the whole unknown-key fixture (WatchedCount positive, others -999999) is a failed summary: every total is --', async () => {
  const reply = await fixture('stoplivedata_unknown_key.json');
  assert.ok(reply.data.WatchedCount > 0 && reply.data.AddFans === -999999, 'fixture loaded whole');
  const bili = stopLiveDataToBili(reply);
  assert.equal(bili.state, 'failed');
  const t = biliTotals(bili);
  for (const key of ['watched', 'danmu', 'add_fans', 'new_fans_club', 'max_online']) {
    assert.equal(t[key], null, key);
    assert.equal(orDash(t[key]), DASH, `${key} renders --`);
  }
  // Same if a backend ever passed the sentinels through as an "ok" row.
  const raw = { state: 'ok', online_peak: 120, watched: 1167856, danmu: 0, add_fans: -999999, new_fans_club: 0, max_online: 0 };
  const guarded = biliTotals(raw);
  assert.equal(guarded.state, 'failed');
  assert.equal(guarded.watched, null, 'the positive WatchedCount is not shown either');
  assert.equal(guarded.online_peak, 120, '在线峰值 comes from our samples and stays');
  const agg = aggregateAudience([{ bili: raw }]);
  assert.equal(agg.watched, null);
  assert.equal(agg.max_online, null);
  assert.doesNotMatch(JSON.stringify([t, guarded, agg]), /999999|1167856|1,167,856/);
});

test('-101 (not logged in), pending and missing summaries all show --', () => {
  assert.equal(stopLiveDataToBili({ code: -101, message: '账号未登录' }).state, 'failed');
  const pending = biliTotals({ state: 'pending', online_peak: 50, watched: null });
  assert.equal(pending.state, 'pending');
  assert.equal(biliStateLabel(pending.state), '结束后汇总');
  assert.equal(orDash(pending.watched), DASH);
  assert.equal(biliStateLabel(biliTotals({ state: 'skipped' }).state), '无 B 站汇总');
  assert.equal(biliTotals(null).state, 'none');
  const valid = biliTotals({ state: 'ok', watched: 10, danmu: 1, add_fans: 0, new_fans_club: 0 });
  assert.equal(valid.max_online, null, 'MaxOnline missing → tile omitted');
  assert.equal(valid.add_fans, 0, 'a real 0 is not --');
});

test('range totals sum only valid summaries; 最大在线 only when present', () => {
  const agg = aggregateAudience([
    { bili: { state: 'ok', online_peak: 30, watched: 100, danmu: 5, add_fans: 1, new_fans_club: 0, max_online: null } },
    { bili: { state: 'ok', online_peak: 90, watched: 50, danmu: 5, add_fans: 2, new_fans_club: 1, max_online: 120 } },
    { bili: { state: 'failed', online_peak: 200 } },
    { bili: { state: 'pending', online_peak: 10 } },
  ]);
  assert.deepEqual(agg, { sessions: 4, summarized: 2, pending: 1, online_peak: 200, watched: 150, danmu: 10, add_fans: 3, new_fans_club: 1, max_online: 120 });
  assert.equal(aggregateAudience([{ bili: { state: 'failed' } }]).watched, null);
});

test('watched fallback has explicit sample provenance while summary status and other missing totals remain', () => {
  for (const state of ['pending', 'failed', 'skipped']) {
    const sample = biliTotals({ state, watched: 0, watched_source: 'sample', online_peak: 12 });
    assert.equal(sample.state, state);
    assert.equal(sample.watched, 0);
    assert.equal(watchedSourceLabel(sample.watched_source), '最后采样');
    for (const field of ['danmu', 'add_fans', 'new_fans_club', 'max_online']) assert.equal(sample[field], null);
    assert.equal(biliTotals({ state, watched: 777 }).watched, null, 'no inferred sample provenance');
  }
  assert.equal(biliStateLabel('failed'), '汇总获取失败');
  const sentinel = biliTotals({ state: 'ok', watched: 1167856, watched_source: 'summary', add_fans: -999999 });
  assert.equal(sentinel.watched, null);
  assert.equal(sentinel.state, 'failed');
  assert.equal(biliTotals({ state: 'failed', watched: -1, watched_source: 'sample' }).watched, null);
});

test('range audience consumes already deduplicated server totals and labels mixed sample coverage', () => {
  const result = rangeAudience({ watched: 607, watched_source: 'mixed', danmu: 9, online_peak: 45 });
  assert.equal(result.watched, 607);
  assert.equal(watchedSourceLabel(result.watched_source), '汇总与采样合计');
  assert.equal(result.danmu, 9);
  assert.equal(result.add_fans, null);
  assert.equal(rangeAudience({ watched: -999999 }).watched, null);
  assert.equal(rangeAudience(null).watched, null);
});

test('custom range: raw samples only when ≤ 3 days and inside retention.raw_days', () => {
  const retention = { raw_days: 7 };
  assert.equal(expectedResolution(NOW - 3 * DAY, NOW, retention, NOW), 'raw');
  assert.equal(expectedResolution(NOW - 3 * DAY - 1, NOW, retention, NOW), '1m', 'longer than 3 days');
  assert.equal(expectedResolution(NOW - 9 * DAY, NOW - 7 * DAY, retention, NOW), '1m', 'short but outside the raw window');
  assert.equal(expectedResolution(NOW - 2 * DAY, NOW, { raw_days: 1 }, NOW), '1m', 'follows the configured raw_days');
  assert.equal(expectedResolution(NOW - DAY, NOW, null, NOW), '1m', 'no retention known → rollups');
  // The mock answers with the same rule.
  const ok = data => ({ success: true, data });
  const tier = (from, to) => historyRoute('/api/history/stats', new URLSearchParams({ from, to, max_points: 100 }), 'populated', ok, NOW)[1].data.resolution;
  assert.equal(tier(NOW - 3 * DAY, NOW), 'raw');
  assert.equal(tier(NOW - 9 * DAY, NOW - 7.5 * DAY), '1m');
});

test('custom range validation uses the rollup retention', () => {
  const now = Math.floor(new Date(2026, 9, 14, 12).getTime() / 1000);
  const r = customRange('2026-10-12', '2026-10-13', 90, now);
  assert.equal(r.preset, 'custom');
  assert.equal(r.to - r.from, 2 * DAY);
  assert.equal(customRange('2026-10-14', '2026-10-14', 90, now).to, now, 'clamped to now');
  assert.equal(customRange('2026-01-01', '2026-10-13', 90, now).error, '最长 90 天（按当前保留设置）');
  assert.ok(customRange('2026-10-13', '2026-10-12', 90, now).error);
  assert.ok(customRange('', '2026-10-12', 90, now).error);
});

test('missing nodes: grouped copy, unknown reason falls back, detail only as title', () => {
  const info = missingNodeText([
    { node_id: 'sg-3', reason: 'timeout', detail: 'peer request timed out after 3000 ms' },
    { node_id: 'sg-4', reason: 'timeout' },
    { node_id: 'osaka-2', reason: 'unsupported' },
    { node_id: 'x', reason: 'weird' },
  ]);
  assert.equal(info.text, '部分数据缺失：sg-3、sg-4 未响应；osaka-2 版本过旧，不支持统计；x 读取失败，以下统计不含这些节点。');
  assert.doesNotMatch(info.text, /3000 ms/);
  assert.equal(info.nodes[0].title, 'peer request timed out after 3000 ms');
  assert.equal(missingNodeText([]), null);
});

test('calendar heat is a 0–24 hour spectrum, not a relative more/less step', () => {
  assert.equal(streamHeat(0), 0);
  assert.equal(streamHeat(null), 0);
  assert.equal(streamHeat(12 * 3600), 0.5);
  assert.equal(streamHeat(24 * 3600), 1);
  assert.equal(streamHeat(30 * 3600), 1);
  assert.equal(spectrumColor(0), 'rgb(164 233 249)');
  assert.equal(spectrumColor(0.5), 'rgb(197 174 242)');
  assert.equal(spectrumColor(1), 'rgb(133 120 234)');
  assert.notEqual(spectrumColor(0.25), spectrumColor(0.75));
});

test('audience sessions group ffmpeg pushes by Bilibili live or source id', () => {
  const broadcasts = broadcastGroups([
    { id: 'a', broadcast_id: 'live:aa', started_at: 10, channel_name: '频道', platform: 'YT', source_id: 'vid1', bili: { state: 'ok', watched: 100, danmu: 4, add_fans: 1, online_peak: 20 } },
    { id: 'b', broadcast_id: 'live:aa', started_at: 50, channel_name: '频道', platform: 'YT', source_id: 'vid1', bili: { state: 'ok', watched: 100, danmu: 4, add_fans: 1, online_peak: 40 } },
    { id: 'c', broadcast_id: 'src:TW:99', started_at: 80, channel_name: '其他', platform: 'TW', source_id: '99', bili: { state: 'pending', online_peak: 5, watched: 3, watched_source: 'sample' } },
    { id: 'd', broadcast_id: 'src:TW:99', started_at: 90, channel_name: '其他', platform: 'TW', source_id: '99', bili: { state: 'pending', online_peak: 8, watched: 7, watched_source: 'sample' } },
  ]);
  assert.equal(broadcasts.length, 2);
  assert.equal(broadcasts[0].id, 'src:TW:99');
  assert.equal(broadcasts[0].bili.online_peak, 8);
  assert.equal(broadcasts[0].bili.watched, 7);
  assert.equal(broadcasts[0].bili.danmu, null);
  assert.equal(broadcasts[1].id, 'live:aa');
  assert.equal(broadcasts[1].segments.length, 1, 'one channel, one 段');
  assert.equal(broadcasts[1].segments[0].eids, 'a,b', 'both sessions chart together');
  assert.equal(broadcasts[1].bili.watched, 100);
  assert.equal(broadcasts[1].bili.online_peak, 40);
  assert.deepEqual(broadcasts[1].channels, ['频道']);
});

test('a broadcast that switched channel names every channel it carried', () => {
  // Priority switch mid-live: the B站 live id stays, the source channel and
  // video change. The option label must name both channels, not one channel
  // with both video ids.
  const [b] = broadcastGroups([
    { id: 'a', broadcast_id: 'live:aa', started_at: 10, channel_name: '频道甲', channel_id: 'UC1', source_id: 'vid1', bili: { state: 'ok', watched: 5 } },
    { id: 'b', broadcast_id: 'live:aa', started_at: 60, channel_name: '频道乙', channel_id: 'UC2', source_id: 'vid2', bili: { state: 'ok', watched: 9 } },
    { id: 'c', broadcast_id: 'live:aa', started_at: 90, channel_name: '频道甲', channel_id: 'UC1', source_id: 'vid3', bili: { state: 'ok', watched: 9 } },
  ]);
  assert.deepEqual(b.channels, ['频道甲', '频道乙'], 'each channel once, in relay order');
  assert.equal(b.id, 'live:aa');
  assert.deepEqual(b.segments.map(s => s.channelName), ['频道甲', '频道乙', '频道甲'],
    'A→B→A is three 段, so 场 and 段 never mean the same thing');
  assert.deepEqual(b.segments.map(s => s.eids), ['a', 'b', 'c']);
  const [nameless] = broadcastGroups([
    { id: 'x', broadcast_id: 'live:bb', started_at: 5, channel_name: '', channel_id: 'UC9', bili: { state: 'none' } },
  ]);
  assert.deepEqual(nameless.channels, ['UC9'], 'a nameless session falls back to its channel id');
});

test('mock sessions carry no revenue and never leak sentinels', () => {
  const ok = data => ({ success: true, data });
  const [, body] = historyRoute('/api/history/sessions', new URLSearchParams({ from: NOW - 30 * DAY, to: NOW, limit: 500 }), 'populated', ok, NOW);
  const text = JSON.stringify(body);
  assert.doesNotMatch(text, /HamsterRmb|revenue|收益|-999999/);
  const states = new Set(body.data.items.map(s => s.bili.state));
  assert.ok(states.has('ok') && states.has('pending'));
  for (const s of body.data.items) {
    assert.ok(Number.isFinite(s.bili.online_peak), 'online_peak from samples even when pending/failed');
    if (s.bili.state !== 'ok') assert.equal(s.bili.watched, null);
  }
});

// ---------- chunk loader + error state (tiny fake DOM) ----------
class FakeNode {
  constructor(tag, doc) { this.tagName = tag.toUpperCase(); this.ownerDocument = doc; this.children = []; this.attributes = {}; this.dataset = {}; this.listeners = {}; this.textContent = ''; this.hidden = false; this.className = ''; this.id = ''; this.type = ''; }
  append(...nodes) { for (const n of nodes) { n.parentNode = this; this.children.push(n); } }
  setAttribute(k, v) { this.attributes[k] = String(v); }
  getAttribute(k) { return this.attributes[k] ?? null; }
  addEventListener(type, fn) { (this.listeners[type] ||= []).push(fn); }
  click() { for (const fn of this.listeners.click || []) fn({ type: 'click' }); }
  focus() { this.ownerDocument.activeElement = this; }
  find(pred) { for (const c of this.children) { if (pred(c)) return c; const hit = c.find(pred); if (hit) return hit; } return null; }
}
function fakeView() {
  const doc = { activeElement: null, createElement: tag => new FakeNode(tag, doc) };
  const view = new FakeNode('section', doc);
  const shell = new FakeNode('section', doc);
  view.append(shell);
  return { doc, view, shell };
}

test('chunk-load failure shows the agreed error state with one focused reload button', async () => {
  const { doc, view, shell } = fakeView();
  let reloads = 0;
  const loader = createAnalyticsLoader({
    importer: () => Promise.reject(new TypeError('Failed to fetch dynamically imported module')),
    getView: () => view, deps: {}, reload: () => { reloads++; },
  });
  assert.equal(await loader.mount(), null);
  assert.equal(loader.failed, true);
  assert.equal(shell.hidden, true, 'module shells are hidden');
  const panel = view.children.at(-1);
  assert.equal(panel.getAttribute('role'), 'alert');
  assert.equal(panel.find(n => n.tagName === 'H2').textContent, '统计模块加载失败');
  assert.equal(panel.find(n => n.tagName === 'P').textContent, '页面可能已更新，请重新加载后再试。');
  const buttons = [];
  panel.find(n => { if (n.tagName === 'BUTTON') buttons.push(n); return false; });
  assert.equal(buttons.length, 1, 'single button, no 重试');
  assert.equal(buttons[0].textContent, '重新加载页面');
  assert.match(buttons[0].className, /analytics-reload-btn/, 'class carries the ≥44px min-height rule');
  assert.equal(doc.activeElement, buttons[0], 'focus moves to 重新加载页面');
  buttons[0].click();
  assert.equal(reloads, 1, 'the button reloads the page');
  assert.equal(await loader.mount(), null, 'a second activation does not stack another panel');
  assert.equal(view.children.length, 2);
  assert.deepEqual([CHUNK_ERROR_TITLE, CHUNK_ERROR_BODY, CHUNK_ERROR_BUTTON], ['统计模块加载失败', '页面可能已更新，请重新加载后再试。', '重新加载页面']);
});

test('the reload button is at least 44px tall in the stylesheet', async () => {
  const css = await readFile(new URL('../src/styles.css', import.meta.url), 'utf8');
  assert.match(css, /\.analytics \.analytics-reload-btn\s*\{[^}]*min-height:\s*44px/);
  const js = await readFile(new URL('../src/js/analytics-loader.js', import.meta.url), 'utf8');
  assert.doesNotMatch(js, /重试/);
  assert.doesNotMatch(js, /innerHTML|insertAdjacentHTML/);
});

test('loader imports once (prefetch is memoised), injects deps, and skips mount if the user left', async () => {
  const { view } = fakeView();
  let imports = 0;
  const calls = [];
  const module = { mountAnalytics: (v, d) => calls.push(['mount', v, d]), unmountAnalytics: () => calls.push(['unmount']), refreshIfVisible: () => calls.push(['refresh']) };
  let active = true;
  const deps = { getJson: () => {} };
  const loader = createAnalyticsLoader({ importer: async () => { imports++; return module; }, getView: () => view, deps, reload: () => {}, isActive: () => active });
  loader.prefetch();
  loader.prefetch();
  await loader.mount();
  assert.equal(imports, 1);
  assert.deepEqual(calls[0], ['mount', view, deps]);
  loader.refreshIfVisible();
  loader.unmount();
  assert.deepEqual(calls.slice(1).map(c => c[0]), ['refresh', 'unmount']);
  active = false;
  await loader.mount();
  assert.equal(calls.filter(c => c[0] === 'mount').length, 1, 'no mount after leaving 统计');
});

test('a prefetch failure is silent and surfaces only on mount', async () => {
  const { view } = fakeView();
  const loader = createAnalyticsLoader({ importer: () => Promise.reject(new Error('offline')), getView: () => view, deps: {}, reload: () => {} });
  loader.prefetch();
  await new Promise(resolve => setTimeout(resolve, 0));
  assert.equal(view.children.length, 1, 'nothing rendered by a prefetch');
  await loader.mount();
  assert.equal(view.children.length, 2);
});

test('renderChunkError works on its own', () => {
  const { view, doc } = fakeView();
  renderChunkError(view, { doc, reload: () => {} });
  assert.equal(view.getAttribute('aria-busy'), 'false');
});

test('admin markup: tabs are 总览…统计 then 通知, each with its view and no revenue copy', async () => {
  const html = await readFile(new URL('../src/index.html', import.meta.url), 'utf8');
  const tabs = [...html.matchAll(/class="tab[^"]*" id="tab-([a-z]+)"/g)].map(m => m[1]);
  assert.deepEqual(tabs, ['overview', 'manage', 'settings', 'logs', 'analytics', 'notify']);
  assert.match(html, /id="view-notify"/);
  assert.match(html, /id="tab-notify" data-view="notify"/);
  assert.match(html, /<symbol id="i-chart"/);
  assert.match(html, /id="view-analytics"/);
  const main = await readFile(new URL('../src/js/main.js', import.meta.url), 'utf8');
  assert.match(main, /const VIEW_IDS = \['overview', 'manage', 'settings', 'logs', 'analytics', 'notify'\]/);
  assert.match(main, /import\('\.\/analytics\.js'\)/);
  const view = await readFile(new URL('../src/js/analytics.js', import.meta.url), 'utf8');
  for (const text of [html, view]) assert.doesNotMatch(text, /HamsterRmb|收益|revenue/i);
  assert.doesNotMatch(view, /innerHTML|insertAdjacentHTML|outerHTML/);
  assert.doesNotMatch(view, /from '\.\/(api|state|dom|format|overview)\.js'/, 'the chunk imports no app module');
  assert.doesNotMatch(view, /LiveTime|live_time|duration_secs/, 'duration is computed from started_at/ended_at only');
  assert.doesNotMatch(view, /openid/i, 'no admin openid in 统计');
});

// ---------- design review rev 2 ----------
import { endReasonLabel } from '../src/js/analytics-format.js';

test('B4: 结束原因 maps codes to zh-CN; unknown → 其他, never the raw code', async () => {
  assert.equal(endReasonLabel('source_ended'), '正常结束（源已下播）');
  assert.equal(endReasonLabel('bili_stopped'), '正常结束（B站 已下播）');
  assert.equal(endReasonLabel('user_stop'), '正常结束（手动停止）');
  assert.equal(endReasonLabel('intentional_restart'), '计划内重启');
  assert.equal(endReasonLabel('cluster_handoff'), '节点交接（不计入）');
  assert.equal(endReasonLabel('process_restart'), '异常退出（进程重启）');
  assert.equal(endReasonLabel('aborted'), '异常退出（中断）');
  for (const raw of ['failed', 'weird_code', 'toString', '__proto__', 'SOURCE_ENDED']) assert.equal(endReasonLabel(raw), '其他');
  assert.equal(endReasonLabel(null), DASH, 'still open');
  const view = await readFile(new URL('../src/js/analytics.js', import.meta.url), 'utf8');
  assert.match(view, /const last = segments\[segments\.length - 1\];/);
  assert.match(view, /const end = last\.end;/);
  assert.match(view, /\['结束原因', end\.label, end\.detail, end\.tone\]/);
  assert.match(view, /el\('dt', 'cluster-node-meta-label', label\)/);
  assert.match(view, /el\('dd', 'cluster-node-meta-value', value\)/);
  const css = await readFile(new URL('../src/styles.css', import.meta.url), 'utf8');
  assert.match(css, /\.analytics-detail-dialog \.cluster-node-meta-label \{ flex: 0 0 48px; \}/);
  assert.match(css, /\.analytics-detail-dialog \.cluster-node-meta-value\[data-tone="warn"\] \{ color: var\(--warn\); \}/);
  assert.match(css, /\.analytics-detail-dialog \.cluster-node-meta-value\[data-tone="danger"\] \{ color: var\(--danger\); \}/);
  assert.match(css, /\.analytics-detail-dialog \.cluster-node-meta-value small \{[^}]*color: var\(--text-mute\);/);
});

test('B3: 段 switches are 中断 marks in one 场 chart, not paging', async () => {
  const view = await readFile(new URL('../src/js/analytics.js', import.meta.url), 'utf8');
  const html = await readFile(new URL('../src/index.html', import.meta.url), 'utf8');
  const format = await readFile(new URL('../src/js/analytics-format.js', import.meta.url), 'utf8');
  // One 场, one page: the switch shows as a dashed warn line inside the plot.
  assert.match(view, /function segmentMarks\(group\) \{/);
  assert.match(view, /return group\.segments\.slice\(1\)\.map\(seg => seg\.started_at\)/);
  assert.match(view, /hooks: \{ draw: \[u => drawInterrupts\(u, marks\)\] \}/);
  assert.match(view, /ctx\.setLineDash\(\[4, 3\]\)/);
  assert.match(view, /cssVar\('--warn'\)/);
  // The word 中断 is a legend entry, never painted per line.
  assert.doesNotMatch(view, /fillText\('中断'/);
  assert.match(view, /el\('span', 'analytics-legend-mark'\), el\('span', '', '中断'\)/);
  // Paging and the tab strip are gone for good.
  assert.doesNotMatch(view, /analytics-detail-prev|analytics-detail-next|analytics-segment|renderSegmentTabs/);
  assert.doesNotMatch(html, /analytics-detail-prev|analytics-detail-next|analytics-detail-segments/);
  assert.doesNotMatch(format, /neighbourLabel/);
});

test('B1: 查看本场详情 button beside the 观众 select opens the selected session', async () => {
  const html = await readFile(new URL('../src/index.html', import.meta.url), 'utf8');
  assert.match(html, /id="analytics-aud-session"[^]*?<button class="[^"]*analytics-detail-open[^"]*" type="button" id="analytics-aud-detail" hidden>查看本场详情<\/button>/);
  const css = await readFile(new URL('../src/styles.css', import.meta.url), 'utf8');
  assert.match(css, /\.analytics \.analytics-detail-open \{ min-height: 44px; \}/);
  const view = await readFile(new URL('../src/js/analytics.js', import.meta.url), 'utf8');
  assert.match(view, /\$\('analytics-aud-detail'\)\.addEventListener\('click', \(\) => \{\s*const broadcast = selectedBroadcast\(\);\s*if \(broadcast\) openBroadcast\(broadcast, 0\);/);
});

test('B2: 查看日志 and dialog 关闭 get 44px targets at ≤560px', async () => {
  const css = await readFile(new URL('../src/responsive.css', import.meta.url), 'utf8');
  const block = css.slice(css.indexOf('@media (max-width: 560px)', css.indexOf('12b. Analytics')));
  assert.match(block.slice(0, block.indexOf('\n}')), /\.analytics-logs-btn,\s*\.analytics \.analytics-detail-close \{ min-height: 44px; min-width: 44px; \}/);
  const view = await readFile(new URL('../src/js/analytics.js', import.meta.url), 'utf8');
  assert.match(view, /'btn-secondary compact-btn analytics-logs-btn', '查看日志'/);
  const html = await readFile(new URL('../src/index.html', import.meta.url), 'utf8');
  assert.match(html, /class="[^"]*analytics-detail-close[^"]*" type="button" id="analytics-detail-close"/);
});

import { END_REASONS } from '../mock/history-fixtures.mjs';
test('mock end_reason uses only the seven backend codes, each mapped (open → --)', () => {
  assert.deepEqual([...END_REASONS].sort(), ['aborted', 'bili_stopped', 'cluster_handoff', 'intentional_restart', 'process_restart', 'source_ended', 'user_stop']);
  const ok = data => ({ success: true, data });
  const [, body] = historyRoute('/api/history/sessions', new URLSearchParams({ from: NOW - 30 * DAY, to: NOW, limit: 500 }), 'populated', ok, NOW);
  const seen = new Set();
  for (const s of body.data.items) {
    if (s.ended_at === null) { assert.equal(s.end_reason, null); assert.equal(endReasonLabel(s.end_reason), DASH); continue; }
    assert.ok(END_REASONS.includes(s.end_reason), `unexpected mock end_reason ${s.end_reason}`);
    assert.notEqual(endReasonLabel(s.end_reason), '其他');
    seen.add(s.end_reason);
  }
  assert.ok(seen.size >= 5, 'mock exercises most codes');
  assert.ok(body.data.items.some(s => s.failures > 0 && ['aborted', 'process_restart'].includes(s.end_reason)));
});

// ---------- clear history (W18/W19) ----------

test('W18: 清理历史 is a bare toolbar trash icon confirmed by its dialog', async () => {
  const html = await readFile(new URL('../src/index.html', import.meta.url), 'utf8');
  // It sits in the panel toolbar with the range and refresh controls as an
  // icon-only action: no extra row and no standing explainer copy.
  assert.doesNotMatch(html, /analytics-maintenance/, 'the separate maintenance row is gone');
  assert.doesNotMatch(html, /历史只保存在本节点/, 'no inline scope line');
  assert.match(html, /<div class="panel__actions">[\s\S]*id="analytics-clean-btn"[\s\S]*?<\/div>\s*<\/div>\s*<div class="panel__body">/);
  // The same trash glyph as the 裁剪 clear buttons in the status card.
  assert.match(html, /id="analytics-clean-btn" class="[^"]*btn-ghost[^"]*icon-btn[^"]*"[^>]*>[\s\S]{0,120}<use href="#i-trash" \/>\s*<\/svg>\s*<\/button>/, 'quiet ghost icon like the 裁剪 clear');
  assert.match(html, /id="analytics-clean-btn"[^>]*title="清理集群历史"[^>]*aria-label="清理集群历史"/);
  assert.match(html, /id="yt-crop-clear-btn"[\s\S]{0,200}<use href="#i-trash" \/>/, 'the crop clear keeps the shared symbol');
  const css = await readFile(new URL('../src/styles.css', import.meta.url), 'utf8');
  assert.match(css, /\.analytics \.panel__actions \.analytics-clean \{ margin-left: var\(--sp-2\); \}/);
  assert.match(css, /\.analytics \.panel__actions \.analytics-clean:hover:not\(:disabled\),[\s\S]{0,90}color: var\(--danger\)/, 'it turns red only on hover');
  assert.doesNotMatch(css, /\.analytics-maintenance/, 'the removed row left no dead style');
  const shared = await readFile(new URL('../src/shared.css', import.meta.url), 'utf8');
  assert.match(shared, /\.btn-ghost\s*\{/, 'the button reuses the shared ghost style');
  assert.match(shared, /\.icon-btn\.compact-btn \{[^}]*width: 30px/, 'the icon-only size comes from the shared rule');
  assert.match(shared, /\.btn-icon \{[^}]*width: 17px/, 'the glyph is sized by the shared button icon');
  const view = await readFile(new URL('../src/js/analytics.js', import.meta.url), 'utf8');
  assert.match(view, /const CLEAN_CONFIRM = '[^']*集群内所有节点[^']*无法撤销[^']*';/);
  assert.match(view, /if \(cleanBtn\.disabled \|\| !confirm\(CLEAN_CONFIRM\)\) return;/, 'the dialog is the only confirmation');
  assert.match(view, /await deps\.postJson\('\/api\/history\/clean', \{\}\)/);
  assert.match(view, /await refresh\(\{ force: true \}\)/);
  assert.match(view, /cleanBtn\.disabled = true;/);
  assert.match(view, /cleanBtn\.setAttribute\('aria-busy', 'true'\)/, 'the busy state is announced');
  assert.match(view, /cleanBtn\.removeAttribute\('aria-busy'\)/);
  assert.match(view, /cleanBtn\.disabled = false;/, 'the button is usable again after a failure too');
  assert.doesNotMatch(view, /cleanBtn\.textContent/, 'rewriting the button would drop the trash icon');
  assert.doesNotMatch(view, /analytics-clean-label/, 'there is no label span left to update');
  assert.match(view, /const report = cleanNotice\(reply\?\.data\);/);
  assert.match(view, /showNotice\(report\.text, report\.tone\)/);
  assert.match(view, /showNotice\(`清理失败：\$\{error\?\.message \|\| error\}`, 'danger'\)/);
  // The chunk gets its own modules through deps, so main.js must hand postJson over.
  const main = await readFile(new URL('../src/js/main.js', import.meta.url), 'utf8');
  assert.match(main, /import \{[^}]*postJson[^}]*\} from '\.\/api\.js';/);
  assert.match(main, /deps: \{[^}]*postJson[^}]*\}/, 'postJson reaches the chunk deps');
});

test('W18 mock: POST /api/history/clean reports every node that cleared', () => {
  const ok = data => ({ success: true, data });
  const [status, body] = historyRoute('/api/history/clean', new URLSearchParams(), 'populated', ok, NOW);
  assert.equal(status, 200);
  assert.ok(body.data.rows > 0, 'a populated archive reports rows');
  assert.deepEqual(body.data.nodes.map(n => n.node_id), ['local', 'tokyo-1'], 'each cleared node is named');
  assert.equal(body.data.failed.length, 0);
  assert.equal(body.data.clamped_to_days, undefined, 'maintenance is not range-decorated');
  const [, partial] = historyRoute('/api/history/clean', new URLSearchParams(), 'partial', ok, NOW);
  assert.equal(partial.data.failed[0].node_id, 'sg-3', 'a node that kept its rows is named');
  const [, empty] = historyRoute('/api/history/clean', new URLSearchParams(), 'empty', ok, NOW);
  assert.equal(empty.data.rows, 0, 'clearing an empty archive is a no-op, not an error');
});

test('W19: cleanNotice names the nodes that kept their rows', () => {
  const all = cleanNotice({ rows: 1280, nodes: [{ node_id: 'local', rows: 1000 }, { node_id: 'tokyo-1', rows: 280 }], failed: [] });
  assert.equal(all.tone, '');
  assert.match(all.text, /已清理 1280 行历史（2 台节点）/);
  assert.match(all.text, /从此刻起一起重新记录/);
  const partial = cleanNotice({ rows: 1000, nodes: [{ node_id: 'local', rows: 1000 }], failed: [{ node_id: 'sg-3', reason: '未响应' }] });
  assert.equal(partial.tone, 'danger');
  assert.match(partial.text, /sg-3 未响应/);
  assert.match(partial.text, /仍保留旧记录/);
  // A node that never answered has no reason of its own: still named, never hidden.
  assert.match(cleanNotice({ rows: 0, nodes: [], failed: [{ node_id: 'sg-3' }] }).text, /sg-3 未清理/);
  // An older backend answers only `rows`: the report still reads correctly.
  const legacy = cleanNotice({ rows: 12 });
  assert.equal(legacy.tone, '');
  assert.match(legacy.text, /1 台节点/);
  assert.equal(cleanNotice(null).tone, '');
});
