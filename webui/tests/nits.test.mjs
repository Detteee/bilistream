import test from 'node:test';
import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import vm from 'node:vm';
import { activePushView, nodeSignature } from '../src/js/notify-state.js';
import { mergeRetention } from '../src/js/analytics-format.js';

const src = name => readFile(new URL(`../src/${name}`, import.meta.url), 'utf8');
const n = (className, text, extra = {}, children = []) => ({ className, textContent: text, title: extra.title || '', dataset: extra.tone ? { tone: extra.tone } : {}, children });

test('N1: unknown last_result shows — like null, with a comment saying so', async () => {
  assert.deepEqual(activePushView({ active_push: { last_result: 'quota_v9', month_sent: 3 } }), { segments: ['—'], tone: '' });
  assert.deepEqual(activePushView({ active_push: { last_result: null } }), { segments: ['—'], tone: '' });
  assert.match(await src('js/notify-state.js'), /unknown \(future\) last_result shows — like null[\s\S]*counts as "not ok"/);
});

test('N2: status signature is stable for identical rows and changes on text, tone, class or title', () => {
  const rows = () => [n('cluster-node-meta-row', 'x', {}, [n('l', '主动推送'), n('v', '可用 · 本月 3 条')])];
  assert.equal(rows().map(nodeSignature).join('\n'), rows().map(nodeSignature).join('\n'));
  const base = nodeSignature(n('r', '', {}, [n('v', 'a')]));
  assert.notEqual(nodeSignature(n('r', '', {}, [n('v', 'b')])), base);
  assert.notEqual(nodeSignature(n('r', '', {}, [n('v', 'a', { tone: 'warn' })])), base);
  assert.notEqual(nodeSignature(n('r', '', {}, [n('v2', 'a')])), base);
  assert.notEqual(nodeSignature(n('r', '', {}, [n('v', 'a', { title: 't' })])), base);
  assert.equal(nodeSignature(null), '');
});

test('N2: renderStatus skips the DOM rebuild when the signature is unchanged', async () => {
  const js = await src('js/notify.js');
  assert.match(js, /const sig = rows\.map\(nodeSignature\)\.join\('\\n'\);\s*if \(sig === statusSig\) return;\s*statusSig = sig;\s*box\.replaceChildren\(\.\.\.rows\);/);
});

test('N3: events-only reloads refresh W15/W16', async () => {
  const js = await src('js/analytics.js');
  const notices = new Map();
  let change;
  let response = { clamped_to_days: 14, partial_nodes: [{ node: 'ny', clamped_to_days: 7 }] };
  const context = vm.createContext({
    AbortController, mergeRetention, mounted: true, eventsController: null, view: { range: {} },
    $: id => {
      if (id === 'analytics-event-filter') return { addEventListener: (_event, fn) => { change = fn; } };
      if (!notices.has(id)) notices.set(id, { textContent: '', hidden: true });
      return notices.get(id);
    },
    history: async () => response, eventParams: r => r, renderEvents() {}, setError: (_key, e) => { throw e; },
  });
  const helpers = js.slice(js.indexOf('let retentionSeen ='), js.indexOf('function applyMissing('));
  const handler = js.slice(js.indexOf("  $('analytics-event-filter').addEventListener"), js.indexOf("  for (const input of root.querySelectorAll('input[name=\"analytics-quality\"]'))"));
  vm.runInContext(`${helpers}\n${handler}`, context);
  await change();
  assert.match(notices.get('analytics-clamp-notice').textContent, /14 天/);
  assert.match(notices.get('analytics-partial-notice').textContent, /ny.*7 天/);
  response = {};
  await change();
  assert.equal(notices.get('analytics-clamp-notice').hidden, true);
  assert.equal(notices.get('analytics-partial-notice').hidden, true);
});

test('N4: explicit neutral rule for the info notice using CSS variables', async () => {
  const css = await src('styles.css');
  const m = css.match(/\.analytics \.analytics-notice\[data-tone="info"\] \{([^}]*)\}/);
  assert.ok(m);
  assert.match(m[1], /border-left-color: var\(--line\)/);
  for (const decl of m[1].split(';').map(s => s.trim()).filter(Boolean)) assert.match(decl, /: var\(--[\w-]+\)$/);
});

// B1: rows like QQ 网关 are a dot element followed by a bare text node.
const textNode = text => ({ nodeType: 3, textContent: text });
const dotRow = text => {
  const dot = { nodeType: 1, className: 'status-indicator status-live', textContent: '', dataset: {}, title: '', childNodes: [] };
  const t = textNode(text);
  const value = { nodeType: 1, className: 'cluster-node-meta-value', dataset: {}, title: '', childNodes: [dot, t], children: [dot], textContent: text };
  return { nodeType: 1, className: 'cluster-node-meta-row', dataset: {}, title: '', childNodes: [value], children: [value], textContent: `QQ 网关${text}` };
};

test('B1: a dot element plus a bare text node — changing the text changes the signature', () => {
  assert.notEqual(nodeSignature(dotRow('已连接 · 2 小时 0 分钟')), nodeSignature(dotRow('已连接 · 2 小时 3 分钟')));
  assert.equal(nodeSignature(dotRow('已连接 · 2 小时 0 分钟')), nodeSignature(dotRow('已连接 · 2 小时 0 分钟')));
  // Even a tree that only exposes element children still sees the text via textContent.
  const legacy = text => ({ className: 'v', dataset: {}, title: '', children: [{ className: 'dot', textContent: '', dataset: {}, title: '' }], textContent: text });
  assert.notEqual(nodeSignature(legacy('a')), nodeSignature(legacy('b')));
});

test('B1: push-ok uptime 2 小时 0 分钟 → 2 小时 3 分钟 after 185 s changes the row signature', async () => {
  const { createNotifyState } = await import('../mock/notify-fixtures.mjs');
  const { formatDuration } = await import('../src/js/notify-state.js');
  let t = 1_790_000_000;
  const mock = createNotifyState(() => t);
  assert.ok(mock.setMode('push-ok'));
  const since = mock.route('GET', '/api/notify/status')[1].data.gateway.since;
  const at = now => `已连接 · ${formatDuration(now - since)}`;
  assert.equal(at(t), '已连接 · 2 小时 0 分钟');
  const before = nodeSignature(dotRow(at(t)));
  const later = t + 185;
  assert.equal(at(later), '已连接 · 2 小时 3 分钟');
  assert.notEqual(nodeSignature(dotRow(at(later))), before);
});
