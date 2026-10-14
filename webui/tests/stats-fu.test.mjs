import test from 'node:test';
import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { clampNoticeText, historyQuery, mergeRetention, partialNodesText, EVENTS_EMPTY } from '../src/js/analytics-format.js';
import { historyRoute, CLAMP_DAYS, PARTIAL_NODES } from '../mock/history-fixtures.mjs';

const ok = data => ({ success: true, data });
const NOW = 1_790_000_000;
const call = (name, query, mode) => historyRoute(`/api/history/${name}`, new URLSearchParams(historyQuery(query)), mode, ok, NOW)[1].data;
const range = { from: NOW - 7 * 86400, to: NOW };
const src = name => readFile(new URL(`../src/${name}`, import.meta.url), 'utf8');

test('W15: clamp notice only when clamped_to_days is present', () => {
  assert.equal(clampNoticeText(90), '所选范围超出保留期，仅显示最近 90 天');
  for (const v of [undefined, null, 0, -3, 1.5, '90']) assert.equal(clampNoticeText(v), null);
  assert.deepEqual(mergeRetention([{ missing_nodes: [] }, {}]), { clamped: null, partial: null });
  assert.equal(mergeRetention([{ clamped_to_days: 90 }, { clamped_to_days: 60 }]).clamped, '所选范围超出保留期，仅显示最近 60 天');
});

test('W16: partial notice only when partial_nodes is non-empty; nodes joined with 、', () => {
  assert.equal(partialNodesText(undefined), null);
  assert.equal(partialNodesText([]), null);
  assert.equal(partialNodesText([{ node: 'osaka-2', clamped_to_days: 30 }]), '部分节点保留期较短：osaka-2 仅含最近 30 天');
  assert.equal(partialNodesText([{ node: 'osaka-2', clamped_to_days: 30 }, { node: 'sg-3', clamped_to_days: 14 }]),
    '部分节点保留期较短：osaka-2 仅含最近 30 天、sg-3 仅含最近 14 天');
  assert.equal(partialNodesText([{ node: '', clamped_to_days: 3 }, { node: 'x' }]), null);
  const m = mergeRetention([{ partial_nodes: [{ node: 'a', clamped_to_days: 30 }] }, { partial_nodes: [{ node: 'a', clamped_to_days: 20 }, { node: 'b', clamped_to_days: 7 }] }]);
  assert.equal(m.partial, '部分节点保留期较短：a 仅含最近 20 天、b 仅含最近 7 天');
  assert.equal(m.clamped, null, 'W16 alone does not show W15');
});

test('W15 then W16 in the markup, neutral, after the missing/error notice', async () => {
  const html = await src('index.html');
  const a = html.indexOf('id="analytics-notice"'), c = html.indexOf('id="analytics-clamp-notice"'), p = html.indexOf('id="analytics-partial-notice"');
  assert.ok(a > 0 && c > a && p > c);
  assert.match(html, /id="analytics-clamp-notice" data-tone="info" role="status" hidden/);
  assert.match(html, /id="analytics-partial-notice" data-tone="info" role="status" hidden/);
  const js = await src('js/analytics.js');
  assert.match(js, /\[\['analytics-clamp-notice', clamped\], \['analytics-partial-notice', partial\]\]/);
  assert.ok(!/innerHTML/.test(js));
});

test('empty 最近事件 reads 暂无事件记录, never 运行正常', async () => {
  assert.equal(EVENTS_EMPTY, '暂无事件记录');
  const all = (await src('js/analytics.js')) + (await src('index.html'));
  assert.ok(!all.includes('运行正常'));
  assert.ok(!all.includes('此时间范围内没有事件'));
  assert.match(all, /setEmpty\('events', EVENTS_EMPTY\)/);
  assert.match(all, /data-module="events" data-empty="暂无事件记录"/);
  assert.equal(call('events', range, 'empty').items.length, 0);
});

test('historyQuery encodes string ids and round-trips them', () => {
  for (const id of ['jp:3f9a1c2b:12', 'a b/c?d&e=f#g:1', '东京:boot:7']) {
    const qs = historyQuery({ id, cursor: id, scope: '', from: 1, severity: 'warn,error' });
    assert.ok(!/[: /#?]/.test(qs), qs);
    const back = Object.fromEntries(new URLSearchParams(qs));
    assert.equal(back.id, id);
    assert.equal(back.cursor, id);
    assert.equal(back.severity, 'warn,error');
    assert.ok(!('scope' in back));
  }
});

test('mock string ids: list → detail → paging, compared with String()', () => {
  const list = call('sessions', { ...range, limit: 500 }, 'populated').items;
  assert.ok(list.length > 4);
  assert.ok(list.every(s => typeof s.id === 'string' && /^[\w-]+:[0-9a-f]{8}:\d+$/.test(s.id)));
  const target = list[3];
  const detail = call('sessions', { id: target.id }, 'populated');
  assert.equal(String(detail.items[0].id), String(target.id));
  assert.equal(typeof detail.prev_id, 'string');
  const ev = call('events', { ...range, limit: 5 }, 'populated');
  assert.ok(ev.items.every(e => typeof e.id === 'string'));
  assert.ok(ev.items.filter(e => e.session_id != null).every(e => typeof e.session_id === 'string'));
  const next = call('events', { ...range, limit: 5, cursor: ev.next_cursor }, 'populated');
  assert.ok(next.items.length && !ev.items.some(e => e.id === next.items[0].id));
});

test('severities are exactly info / warn / error (mock and view)', async () => {
  const all = call('events', { from: NOW - 30 * 86400, to: NOW, limit: 500 }, 'populated').items;
  assert.deepEqual([...new Set(all.map(e => e.severity))].sort(), ['error', 'info', 'warn']);
  const js = await src('js/analytics.js');
  const used = new Set([...js.matchAll(/severity === '(\w+)'/g)].map(m => m[1]));
  assert.ok(used.size && [...used].every(s => ['info', 'warn', 'error'].includes(s)));
});

test('mock modes clamped / partial-nodes / clamped-partial', () => {
  for (const name of ['summary', 'sessions', 'events', 'stats']) {
    const c = call(name, range, 'clamped');
    assert.equal(c.clamped_to_days, CLAMP_DAYS);
    assert.ok(!('partial_nodes' in c));
    const p = call(name, range, 'partial-nodes');
    assert.deepEqual(p.partial_nodes, PARTIAL_NODES);
    assert.ok(!('clamped_to_days' in p));
    const both = call(name, range, 'clamped-partial');
    assert.ok(both.clamped_to_days && both.partial_nodes.length && both.missing_nodes.length);
    const plain = call(name, range, 'populated');
    assert.ok(!('clamped_to_days' in plain) && !('partial_nodes' in plain));
  }
  const m = mergeRetention(['summary', 'sessions'].map(n => call(n, range, 'clamped-partial')));
  assert.equal(m.clamped, '所选范围超出保留期，仅显示最近 90 天');
  assert.equal(m.partial, '部分节点保留期较短：tokyo-1 仅含最近 30 天、sg-3 仅含最近 14 天');
});
