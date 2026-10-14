import assert from 'node:assert/strict';
import { test } from 'node:test';
import {
  DASH, orDash, formatCount, formatHours, formatPercent, formatAbsolute, formatRelative, formatRange,
  formatOffset, formatDelta, resolutionLabel, bucketFor, presetRange, retentionLine, coverageLine,
  stripCells, sparkPath, ringArcs,
} from '../src/js/analytics-format.js';

// Local-time constructors keep these tests independent of the runner's zone.
const at = (...parts) => new Date(...parts).getTime() / 1000;
const NOW = at(2026, 9, 11, 16, 30);

test('orDash and number formatters show -- for missing values', () => {
  for (const missing of [null, undefined, NaN, Infinity]) {
    assert.equal(orDash(missing), DASH);
    assert.equal(formatCount(missing), DASH);
    assert.equal(formatHours(missing), DASH);
    assert.equal(formatPercent(missing), DASH);
  }
  assert.equal(orDash(0), '0');
  assert.equal(formatCount(1234567.4), '1,234,567');
  assert.equal(formatHours(5400), '1.5');
  assert.equal(formatPercent(0.9876), '98.8%');
  assert.equal(orDash(7200, formatHours), '2');
});

test('absolute and relative times use MM-DD HH:mm in local time', () => {
  assert.equal(formatAbsolute(at(2026, 9, 1, 9, 5), NOW), '10-01 09:05');
  assert.equal(formatAbsolute(at(2025, 0, 2, 3, 4), NOW), '2025-01-02 03:04');
  assert.equal(formatRelative(NOW - 30, NOW), '刚刚');
  assert.equal(formatRelative(NOW - 125, NOW), '2 分钟前');
  assert.equal(formatRelative(NOW - 3 * 3600, NOW), '3 小时前');
  assert.equal(formatRelative(at(2026, 9, 9, 8, 0), NOW), '10-09 08:00');
  assert.equal(formatRelative(NOW + 60, NOW), '刚刚');
  assert.equal(formatRelative(null, NOW), DASH);
  assert.equal(formatRange(at(2026, 9, 10, 16, 30), NOW, NOW), '10-10 16:30 ～ 10-11 16:30');
});

test('offset label and coverage line', () => {
  assert.equal(formatOffset(8), '(UTC+8)');
  assert.equal(formatOffset(-5), '(UTC−5)');
  assert.equal(formatOffset(0), '(UTC+0)');
  assert.equal(coverageLine(null, NOW), '尚未开始记录');
  assert.equal(coverageLine(at(2026, 9, 1, 0, 0), NOW, 'tokyo-1', 8), '10-01 00:00 ～ 10-11 16:30 (UTC+8) · tokyo-1');
});

test('formatDelta signs and tones', () => {
  assert.deepEqual(formatDelta(2.34, ' 小时'), { text: '+2.3 小时', tone: 'good' });
  assert.deepEqual(formatDelta(-3, ' 次', true), { text: '−3 次', tone: 'good' });
  assert.deepEqual(formatDelta(4, ' 次', true), { text: '+4 次', tone: 'bad' });
  assert.deepEqual(formatDelta(0, '%'), { text: '±0%', tone: '' });
  assert.deepEqual(formatDelta(null), { text: DASH, tone: '' });
});

test('resolution, buckets and preset ranges follow the W9 table', () => {
  assert.equal(resolutionLabel('raw'), '原始采样');
  assert.equal(resolutionLabel('1m'), '每分钟汇总');
  assert.equal(bucketFor({ preset: '24h' }).bucket, 'hour');
  assert.equal(bucketFor({ preset: '7d' }).bucket, '6h');
  assert.equal(bucketFor({ preset: '30d' }).bucket, 'day');
  assert.equal(bucketFor({ preset: 'custom', from: 0, to: 3 * 86400 }).bucket, 'hour');
  assert.equal(bucketFor({ preset: 'custom', from: 0, to: 3 * 86400 + 1 }).bucket, 'day');
  assert.deepEqual(presetRange('7d', 1_000_000), { preset: '7d', from: 1_000_000 - 7 * 86400, to: 1_000_000 });
  assert.throws(() => presetRange('1y'));
});

test('retention line reads the API values', () => {
  const line = '保留期限：原始采样 7 天 · 每分钟汇总 90 天 · 事件 180 天（最多 200,000 条），可在系统设置中调整。';
  assert.equal(retentionLine({ raw_days: 7, rollup_days: 90, event_days: 180, event_rows: 200000 }), line);
  assert.match(retentionLine(null), /原始采样 -- 天/);
});

test('stripCells paints worst state per cell and hatches before coverage', () => {
  const from = 0, to = 4800;
  const cells = stripCells([
    { started_at: 1000, ended_at: 2000, gaps: [[1500, 1550, 'reconnecting']] },
    { started_at: 3000, ended_at: null, gaps: [[3100, 3150, 'failed'], [3120, 3130, 'reconnecting']] },
  ], from, to, 48, 500);
  assert.equal(cells.length, 48);
  assert.deepEqual(cells.slice(0, 6).map(c => c.state), ['gap', 'gap', 'gap', 'gap', 'gap', 'idle']);
  assert.equal(cells[10].state, 'live');
  assert.equal(cells[15].state, 'warn');
  assert.equal(cells[19].state, 'live');
  assert.equal(cells[20].state, 'idle');
  assert.equal(cells[31].state, 'fail');
  assert.equal(cells[47].state, 'live', 'open session runs to the end of the range');
});

test('stripCells clips sessions outside the range and ignores junk', () => {
  const cells = stripCells([{ started_at: -500, ended_at: 50 }, { started_at: null }, { started_at: 9000, ended_at: 9900 }], 0, 4800);
  assert.equal(cells[0].state, 'live');
  assert.ok(cells.slice(1).every(c => c.state === 'idle'));
  assert.ok(stripCells(null, 0, 4800).every(c => c.state === 'idle'));
});

test('sparkPath scales, breaks on null and keeps spikes when downsampling', () => {
  assert.equal(sparkPath([0, 10], 0, 10, 100, 20), 'M0 20L100 0');
  assert.equal(sparkPath([0, null, 10], 0, 10, 100, 20), 'M0 20M100 0');
  assert.equal(sparkPath([], 0, 1, 100, 20), '');
  const values = Array.from({ length: 5000 }, (_, i) => (i === 2500 ? 100 : 1));
  const d = sparkPath(values, 0, 100, 120, 30);
  assert.ok(d.split(/[ML]/).filter(Boolean).length <= 240);
  assert.match(d, / 0(?=L|M|$)/, 'the spike reaches the top edge');
});

test('ringArcs splits a circumference between ok and fail', () => {
  assert.deepEqual(ringArcs(3, 1), { rate: 0.75, ok: '75 25', fail: '25 75', failOffset: -75 });
  assert.equal(ringArcs(0, 0).rate, null);
  assert.equal(ringArcs(5, null).rate, 1);
});

test('formatDelta rounds before choosing sign and tone', () => {
  assert.deepEqual(formatDelta(0.04, ' 小时'), { text: '±0 小时', tone: '' });
  assert.deepEqual(formatDelta(-0.04, ' 小时'), { text: '±0 小时', tone: '' });
  assert.deepEqual(formatDelta(0.05, '%'), { text: '+0.1%', tone: 'good' });
});

test('stripCells skips malformed gaps', () => {
  const cells = stripCells([{ started_at: 0, ended_at: 4800, gaps: [null, [null, 60, 'failed'], 'x', [100, 'y', 'failed']] }], 0, 4800);
  assert.equal(cells[0].state, 'live');
  assert.equal(cells[1].state, 'fail', 'a gap with no end runs to the session end');
  assert.ok(cells.every(c => c.state !== 'gap'));
});

test('bucketFor tolerates a missing or partial range', () => {
  assert.equal(bucketFor(undefined).bucket, 'hour');
  assert.equal(bucketFor({ preset: 'custom' }).bucket, 'hour');
});

test('sparkPath handles very large arrays without min/max', () => {
  const big = Array.from({ length: 200000 }, (_, i) => i % 97);
  assert.ok(sparkPath(big, null, null, 120, 30).startsWith('M'));
  assert.equal(sparkPath([null, null], null, null, 10, 10), '');
});
