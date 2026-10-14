import test from 'node:test';
import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import vm from 'node:vm';
import { createHistoryClient, historyStateNotice, localHistoryScope } from '../src/js/analytics-history.js';
import * as format from '../src/js/analytics-format.js';

const tick = () => new Promise(resolve => setImmediate(resolve));
const deferred = () => {
  let resolve, reject;
  const promise = new Promise((a, b) => { resolve = a; reject = b; });
  return { promise, resolve, reject };
};
const busy = () => Object.assign(new Error('统计查询繁忙'), { status: 429 });

test('all history reads including detail and revalidation share two slots', async () => {
  let active = 0, peak = 0, now = 0;
  const calls = [];
  const client = createHistoryClient(async path => {
    active++;
    peak = Math.max(peak, active);
    if (active > 2) throw busy();
    calls.push(path);
    await tick();
    active--;
    return { data: { path } };
  }, { now: () => now });
  const names = ['summary', 'sessions', 'stats', 'events'];
  await Promise.all(names.map(name => client.request(name)));
  assert.equal(peak, 2);
  now = 31_000;
  const cached = [];
  const results = await Promise.all([
    ...names.map(name => client.request(name, {}, { onCached: data => cached.push(data) })),
    client.request('sessions', { id: 'peer:1:1' }),
    client.request('stats', { session: 'peer:1:1' }),
  ]);
  assert.equal(cached.length, 4);
  assert.equal(results.length, 6);
  assert.equal(calls.length, 10);
  assert.equal(peak, 2, 'detail reads cannot bypass the refresh queue');
});

test('superseded queued reads are removed while active reads retain their slots', async () => {
  const releases = [];
  const calls = [];
  let active = 0, peak = 0;
  const client = createHistoryClient(async path => {
    const release = deferred();
    releases.push(release);
    calls.push(path);
    peak = Math.max(peak, ++active);
    await release.promise;
    active--;
    return { data: path };
  });
  const old = new AbortController();
  const requests = ['summary', 'sessions', 'stats', 'events'].map(name => client.request(name, { from: 1 }, { signal: old.signal }));
  const settled = Promise.allSettled(requests);
  assert.equal(calls.length, 2);
  old.abort();
  assert.ok((await settled).every(r => r.status === 'rejected' && r.reason.name === 'AbortError'));
  const latest = client.request('summary', { from: 2 });
  assert.equal(calls.length, 2, 'a cancelled HTTP read may still occupy the server');
  releases[0].resolve();
  await tick();
  assert.equal(calls.length, 3);
  assert.match(calls[2], /from=2/);
  releases[1].resolve();
  releases[2].resolve();
  await latest;
  assert.equal(peak, 2);
  assert.ok(!calls.some(path => /stats|events/.test(path)), 'obsolete pending jobs never hit the server');
});

test('deduplication keeps a shared request alive for its remaining consumer', async () => {
  const reply = deferred();
  let calls = 0;
  const client = createHistoryClient(async () => { calls++; return reply.promise; });
  const controller = new AbortController();
  const old = client.request('stats', {}, { signal: controller.signal });
  const rejection = assert.rejects(old, { name: 'AbortError' });
  const current = client.request('stats');
  controller.abort();
  reply.resolve({ data: { value: 7 } });
  await rejection;
  assert.deepEqual(await current, { value: 7 });
  assert.equal(calls, 1);
});

test('another client consuming admission gets bounded 429 recovery', async () => {
  let attempts = 0;
  const waits = [];
  const client = createHistoryClient(async () => {
    if (++attempts < 3) throw busy();
    return { data: { value: 12 } };
  }, { delay: async ms => { waits.push(ms); } });
  assert.deepEqual(await client.request('summary'), { value: 12 });
  assert.deepEqual(waits, [250, 500]);
  let refused = 0;
  const blocked = createHistoryClient(async () => { refused++; throw busy(); }, { delay: async () => {} });
  await assert.rejects(blocked.request('events'), { status: 429 });
  assert.equal(refused, 5, 'persistent overload cannot retry indefinitely');
});

test('stale data paints once and revalidation errors remain visible to the caller', async () => {
  let now = 0, failed = false;
  const client = createHistoryClient(async () => {
    if (failed) throw new Error('peer timed out');
    return { data: { value: 10 } };
  }, { now: () => now });
  await client.request('summary');
  now = 31_000;
  failed = true;
  const cached = [];
  await assert.rejects(client.request('summary', {}, { onCached: data => cached.push(data) }), /peer timed out/);
  assert.deepEqual(cached, [{ value: 10 }]);
});

test('clearing cache prevents an old in-flight response replacing a forced refresh', async () => {
  const oldReply = deferred();
  let calls = 0;
  const client = createHistoryClient(async () => ++calls === 1 ? oldReply.promise : { data: 'new' });
  const old = client.request('summary');
  client.clear();
  assert.equal(await client.request('summary'), 'new');
  oldReply.resolve({ data: 'old' });
  await old;
  assert.equal(await client.request('summary'), 'new');
  assert.equal(calls, 2);
});

const source = await readFile(new URL('../src/js/analytics.js', import.meta.url), 'utf8');
const refreshSource = source.slice(source.indexOf('async function refresh('), source.indexOf('\nfunction selectRange('));
const historySource = source.slice(source.indexOf('async function history('), source.indexOf('\nfunction roundedRange('));
const NOW = 1_790_000_000;

test('the real uPlot options draw isolated retained samples without joining gaps', () => {
  const optionsSource = source.slice(source.indexOf('function uplotOptions('), source.indexOf('\nfunction mountChart('));
  const options = vm.runInNewContext(`${optionsSource}\nuplotOptions([{ label: '码率', tone: '--brand' }], 600, 600)`, {
    ...format, DAY: 86400, cssVar: () => '#00aa00', mix: x => x,
  });
  const data = [[0, 10, 20, 30, 40, 50, 60, 70], [null, 17, null, null, 29, null, 8, 9]];
  const series = options.series[1];
  assert.equal(series.spanGaps, false);
  assert.deepEqual(series.points.filter({ data }, 1), [1, 4]);
  assert.equal(series.points.size, 4);
  assert.equal(series.points.fill, '#00aa00');
  assert.equal(format.isolatedPointIndices([null, null]), null);
});

// Run the real orchestration and real request client. Rendering is observed
// at the DOM boundary, so a correct helper alone cannot hide skipped routes
// or an obsolete refresh painting the page.
function refreshHarness(getJson, statusScope = '') {
  const painted = { notices: [], errors: [], summaries: [], sessions: [], stats: [], events: [] };
  const elements = new Map();
  const element = id => {
    if (!elements.has(id)) elements.set(id, {
      disabled: false, textContent: '', dataset: {}, clientWidth: 800,
      querySelector() { return null; }, append() {},
    });
    return elements.get(id);
  };
  const ctx = vm.createContext({
    ...format, AbortController, createHistoryClient, historyStateNotice, localHistoryScope,
    mounted: true, generation: 0, refreshController: null, eventsController: null,
    historyClient: createHistoryClient(getJson), prefs: { scope: statusScope, quality: 'range' },
    view: { sessions: [], stats: null }, retentionSeen: [], eventsRetention: null,
    NotSupported: class NotSupported extends Error {}, NOT_SUPPORTED: 'unsupported',
    $: element, bodyOf: element,
    el: () => ({ dataset: {}, setAttribute() {} }),
    resolvedRange: () => ({ from: NOW - 600, to: NOW, preset: 'custom' }),
    roundedRange: r => ({ from: r.from, to: r.to }),
    scopeParam: () => statusScope ? { scope: statusScope } : {},
    eventParams: r => ({ from: r.from, to: r.to, ...(statusScope ? { scope: statusScope } : {}) }),
    setLoading() {}, restoreOrder() {}, resetRetention() {}, applyRetention() {},
    showNotice: (text, tone) => { painted.notices.push({ text, tone }); },
    applyMissing: missing => { painted.missing = missing; },
    setError: (key, error) => { painted.errors.push({ key, message: error.message }); },
    emptyAll: text => { painted.empty = text; },
    renderKpis: summary => { painted.summaries.push(summary); },
    renderHours() {}, renderReliability() {}, renderCalendar() {}, renderNodes() {},
    renderStrips: sessions => { painted.sessions.push(sessions); },
    renderAudience: stats => { painted.stats.push(stats); },
    renderQualityRange() {},
    renderEvents: data => { painted.events.push(data); },
    errorText: error => error.message,
  });
  vm.runInContext(`${historySource}\n${refreshSource}\nglobalThis.refresh = refresh;`, ctx);
  return { ctx, painted, elements, refresh: ctx.refresh };
}

const peerData = {
  summary: { kpis: { live_secs: 600 }, coverage: { from: NOW - 600 }, missing_nodes: [] },
  sessions: { items: [{ id: 'ny:boot:1', node: 'ny', started_at: NOW - 600 }], missing_nodes: [] },
  stats: { ts: [NOW - 100], series: { bitrate: { mean: [456] } }, missing_nodes: [] },
  events: { items: [], missing_nodes: [] },
};

test('empty or disabled local status never hides populated cluster and remote history', async () => {
  for (const scope of ['', 'ny']) for (const enabled of [true, false]) {
    const routes = [];
    let active = 0, peak = 0;
    const harness = refreshHarness(async path => {
      const route = new URL(path, 'http://test').pathname.split('/').pop();
      routes.push(route);
      peak = Math.max(peak, ++active);
      if (active > 2) throw busy();
      await tick();
      active--;
      return { data: route === 'status'
        ? { enabled, oldest_at: null, recording_since: null, node_id: 'jp', cluster_nodes: ['jp', 'ny'], writer: { state: enabled ? 'running' : 'stopped' } }
        : peerData[route] };
    }, scope);
    await harness.refresh();
    assert.deepEqual(routes.sort(), ['events', 'sessions', 'stats', 'status', 'summary']);
    assert.equal(peak, 2);
    assert.equal(harness.painted.summaries.at(-1).kpis.live_secs, 600);
    assert.equal(harness.painted.sessions.at(-1)[0].id, 'ny:boot:1');
    assert.equal(harness.painted.stats.at(-1).series.bitrate.mean[0], 456);
    assert.equal(harness.painted.empty, undefined);
    assert.deepEqual(harness.painted.errors, []);
    assert.ok(!harness.painted.notices.some(n => /开始记录|统计已关闭/.test(n.text)));
  }
});

test('local disabled and failed-writer states do not invent a recording start', async () => {
  for (const [enabled, state, expected] of [[false, 'stopped', /统计已关闭/], [true, 'error', /记录已暂停/], [true, 'running', /暂无历史数据/]]) {
    const harness = refreshHarness(async path => {
      const route = new URL(path, 'http://test').pathname.split('/').pop();
      return { data: route === 'status'
        ? { enabled, oldest_at: null, recording_since: null, node_id: 'jp', cluster_nodes: ['jp'], writer: { state } }
        : { items: [], kpis: {}, coverage: {}, series: {}, missing_nodes: [] } };
    }, 'local');
    await harness.refresh();
    assert.match(harness.painted.notices.at(-1).text, expected);
    assert.ok(!harness.painted.notices.some(n => /从.*开始记录/.test(n.text)));
  }
});

test('a superseded real refresh cannot paint old data or oversubscribe the server', async () => {
  const blocked = deferred();
  let active = 0, peak = 0, round = 1;
  const harness = refreshHarness(async path => {
    const route = new URL(path, 'http://test').pathname.split('/').pop();
    const startedRound = round;
    peak = Math.max(peak, ++active);
    if (route !== 'status' && startedRound === 1) await blocked.promise;
    active--;
    return { data: route === 'status'
      ? { enabled: true, oldest_at: null, node_id: 'jp', cluster_nodes: ['jp', 'ny'], writer: { state: 'running' } }
      : { ...peerData[route], round: startedRound } };
  });
  const old = harness.refresh();
  await tick();
  round = 2;
  const current = harness.refresh({ force: true });
  blocked.resolve();
  await Promise.all([old, current]);
  assert.equal(peak, 2);
  assert.ok(harness.painted.summaries.every(data => data.round === 2));
  assert.ok(harness.painted.events.every(data => data.round === 2));
  assert.deepEqual(harness.painted.errors, []);
});
