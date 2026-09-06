import test from 'node:test';
import assert from 'node:assert/strict';
import { createConfigPatch } from '../dist/js/config-draft.js';

// The renderer's null-safe public contract needs only the named output nodes.
const nodes = new Map();
globalThis.window = {};
globalThis.document = { addEventListener() {}, getElementById: id => nodes.get(id) || null };
function node(id) {
  const classes = new Set();
  const element = { dataset: {}, style: {}, textContent: '', checked: false,
    classList: {
      add: name => classes.add(name), remove: name => classes.delete(name),
      contains: name => classes.has(name),
      toggle(name, force) { if (force ?? !classes.has(name)) classes.add(name); else classes.delete(name); },
    },
  };
  nodes.set(id, element);
  return element;
}
const cards = await import('../dist/js/status-cards.js');
const api = await import('../dist/js/api.js');
const { saveBooleanToggle } = await import('../dist/js/toggle-save.js');
const { applyMonitorToggleConfigState } = await import('../dist/js/state.js');

test('settings edits submit only changed fields and their loaded values', () => {
  const baseline = { interval: 30, enable_danmaku_command: true, api_key: 'old', keywords: ['a'] };
  const edited = { ...baseline, interval: 75 };
  assert.deepEqual(createConfigPatch(edited, baseline), { interval: 75, expected: { interval: 30 } });
  assert.equal(createConfigPatch(baseline, baseline), null);
  assert.throws(() => createConfigPatch(edited, null), /加载配置/);
  assert.deepEqual(createConfigPatch({ keywords: [] }, baseline), { keywords: [], expected: { keywords: ['a'] } });
});

test('room stays live after handoff, but local network panel disappears', () => {
  const panel = node('bili-network-panel');
  node('bili-network-quality');
  cards.renderBiliNetworkPanel({ is_live: true, ffmpeg_running: true, stream_speed: 1 });
  assert.equal(panel.classList.contains('hidden'), false);
  cards.renderBiliNetworkPanel({ is_live: true, ffmpeg_running: false, stream_speed: 0, stream_quality: '流畅' });
  assert.equal(panel.classList.contains('hidden'), true);
  assert.equal(cards.isBiliNetworkLive(), false);
  assert.equal(cards.getBiliNetworkQuality(), null);
});

test('running stalled publisher stays visible; old quality is not reused', () => {
  const panel = node('bili-network-panel');
  const quality = node('bili-network-quality');
  cards.renderBiliNetworkPanel({ ffmpeg_running: true, stream_speed: 0 });
  assert.equal(panel.classList.contains('hidden'), false);
  assert.equal(quality.textContent, '卡顿');
  cards.renderBiliNetworkPanel({ ffmpeg_running: true });
  assert.equal(quality.textContent, '等待推流数据');
});

test('authenticated request times out and a later refresh succeeds', async t => {
  let calls = 0;
  t.mock.method(globalThis, 'fetch', (_url, { signal }) => {
    calls += 1;
    if (calls > 1) return Promise.resolve(new Response('{"success":true}'));
    return new Promise((_resolve, reject) => signal.addEventListener('abort', () => reject(signal.reason)));
  });
  await assert.rejects(api.getJson('/api/status', { timeoutMs: 5 }), /连接超时/);
  assert.deepEqual(await api.getJson('/api/status'), { success: true });
  assert.equal(calls, 2);
});

test('deadline includes a stalled body and writes are not automatically retried', async t => {
  let calls = 0;
  t.mock.method(globalThis, 'fetch', (_url, { signal }) => {
    calls += 1;
    return Promise.resolve({ status: 200, ok: true, text: () => new Promise((_resolve, reject) => {
      signal.addEventListener('abort', () => reject(signal.reason));
    }) });
  });
  await assert.rejects(api.postJsonApi('/api/config', {}, { timeoutMs: 5 }), /刷新确认操作结果/);
  assert.equal(calls, 1);
});

test('concurrent clicks serialize and polling cannot overwrite pending intent', async () => {
  const toggle = node('test-toggle');
  toggle.checked = true;
  const pending = [];
  const saved = [];
  const save = value => new Promise(resolve => pending.push({ value, resolve }));
  const work = saveBooleanToggle('test-toggle', false, save, value => saved.push(value));
  toggle.checked = false;
  saveBooleanToggle('test-toggle', false, save, value => saved.push(value));
  applyMonitorToggleConfigState(toggle, 'test-toggle', true);
  assert.equal(toggle.checked, false);
  assert.equal(pending.length, 1);
  pending[0].resolve({ success: true });
  await new Promise(resolve => setTimeout(resolve, 0));
  assert.deepEqual(pending.map(item => item.value), [true, false]);
  pending[1].resolve({ success: true });
  await work;
  assert.deepEqual(saved, [true, false]);
  assert.equal(toggle.checked, false);
});
