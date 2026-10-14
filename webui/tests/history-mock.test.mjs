import assert from 'node:assert/strict';
import { test } from 'node:test';
import { historyRoute } from '../mock/history-fixtures.mjs';

const ok = data => ({ success: true, data });
const NOW = 1_790_000_000;
const call = (name, query = {}, mode = 'populated') => historyRoute(`/api/history/${name}`, new URLSearchParams(query), mode, ok, NOW);

test('history fixtures follow the contract shapes', () => {
  const [, status] = call('status');
  assert.deepEqual(Object.keys(status.data.retention), ['raw_days', 'rollup_days', 'event_days', 'event_rows']);
  const [, summary] = call('summary', { from: NOW - 7 * 86400, to: NOW, bucket: '6h' });
  assert.equal(summary.data.buckets.length, 28);
  assert.deepEqual(Object.keys(summary.data.buckets[0].live_secs), ['YT', 'TW', 'NC']);
  assert.ok('watched' in summary.data.kpis && 'watched' in summary.data.prev_kpis);
  const [, sessions] = call('sessions', { from: NOW - 3 * 86400, to: NOW });
  for (const s of sessions.data.items) {
    assert.ok(Number.isInteger(s.started_at) && (s.ended_at === null || s.ended_at > s.started_at));
    assert.ok(!('HamsterRmb' in s) && !('revenue' in (s.bili || {})), 'no revenue field');
  }
  const [, stats] = call('stats', { from: NOW - 86400, to: NOW, max_points: 500 });
  assert.equal(stats.data.resolution, 'raw');
  assert.ok(stats.data.ts.length <= 500 && stats.data.ts.length > 400);
  const [, year] = call('stats', { from: NOW - 365 * 86400, to: NOW, max_points: 2000 });
  assert.ok(year.data.ts.length <= 2000, 'never more than max_points');
  assert.equal(stats.data.series.bitrate.mean.length, stats.data.ts.length);
});

test('history fixtures page events by keyset cursor, newest first', () => {
  const [, first] = call('events', { from: NOW - 30 * 86400, to: NOW, limit: 4 });
  const [, second] = call('events', { from: NOW - 30 * 86400, to: NOW, limit: 4, cursor: first.data.next_cursor });
  const all = [...first.data.items, ...second.data.items];
  assert.equal(new Set(all.map(e => e.id)).size, 8);
  assert.ok(all.every((e, i) => i === 0 || all[i - 1].at >= e.at));
});

test('history fixture modes cover empty, partial, missing and error', () => {
  assert.equal(call('status', {}, 'empty')[1].data.oldest_at, null);
  assert.deepEqual(call('sessions', {}, 'empty')[1].data.items, []);
  assert.deepEqual(call('summary', {}, 'partial')[1].data.missing_nodes, [{ node_id: 'sg-3', reason: 'timeout', detail: 'no response in 3000 ms' }]);
  assert.equal(call('events', {}, 'partial')[1].data.missing_nodes[0].reason, 'timeout');
  assert.equal(call('summary', {}, 'missing')[0], 404);
  assert.equal(call('events', {}, 'error')[0], 500);
  assert.equal(call('summary', { from: NOW, to: NOW - 1 })[0], 400);
  assert.equal(historyRoute('/api/status', new URLSearchParams(), 'populated', ok), null);
});

test('empty mode summary counts nothing and never goes negative', () => {
  const [code, body] = call('summary', { from: NOW - 7 * 86400, to: NOW, bucket: '6h' }, 'empty');
  assert.equal(code, 200);
  const d = body.data;
  assert.equal(d.kpis.sessions, 0);
  assert.equal(d.kpis.live_secs, 0);
  assert.equal(d.prev_kpis.sessions, 0);
  assert.deepEqual(d.buckets, []);
  assert.deepEqual(d.per_node, []);
  assert.equal(d.reliability.clean, 0);
  assert.equal(d.coverage.from, null);
  assert.deepEqual(d.missing_nodes, []);
});

test('paging clamps limit and rejects an unknown cursor', () => {
  const range = { from: NOW - 30 * 86400, to: NOW };
  assert.equal(call('events', { ...range, limit: -5 })[1].data.items.length, 1);
  assert.equal(call('events', { ...range, limit: 0 })[1].data.items.length, 20, '0 falls back to the default');
  assert.ok(call('events', { ...range, limit: 9999 })[1].data.items.length <= 500);
  assert.equal(call('events', { ...range, cursor: 'nope' })[0], 400);
  assert.equal(call('sessions', { ...range, cursor: 'nope' })[0], 400);
});

test('no session or summary field carries revenue', () => {
  const [, s] = call('sessions', { from: NOW - 30 * 86400, to: NOW, limit: 500 });
  const [, k] = call('summary', { from: NOW - 30 * 86400, to: NOW, bucket: 'day' });
  assert.doesNotMatch(JSON.stringify([s, k]), /HamsterRmb|revenue|hamster/i);
});

test('stats carries missing_nodes, and scope=local is always []', () => {
  assert.equal(call('stats', { from: NOW - 86400, to: NOW }, 'partial')[1].data.missing_nodes[0].node_id, 'sg-3');
  assert.deepEqual(call('stats', { from: NOW - 86400, to: NOW })[1].data.missing_nodes, []);
  for (const name of ['summary', 'sessions', 'events', 'stats']) {
    assert.deepEqual(call(name, { from: NOW - 86400, to: NOW, scope: 'local' }, 'partial')[1].data.missing_nodes, [], name);
  }
});
