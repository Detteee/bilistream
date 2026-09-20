import test from 'node:test';
import assert from 'node:assert/strict';
import { createConfigPatch } from '../dist/js/config-draft.js';

// The renderer's null-safe public contract needs only the named output nodes.
const nodes = new Map();
function createMockElement() {
  const classes = new Set();
  const children = [];
  const style = {
    setProperty(name, value) {
      this[name] = value;
    },
  };
  return {
    dataset: {},
    style,
    children,
    textContent: '',
    checked: false,
    offsetWidth: 1,
    classList: {
      add: name => classes.add(name),
      remove: name => classes.delete(name),
      contains: name => classes.has(name),
      toggle(name, force) { if (force ?? !classes.has(name)) classes.add(name); else classes.delete(name); },
    },
    replaceChildren(...next) {
      children.length = 0;
      children.push(...next);
    },
    appendChild(child) {
      children.push(child);
      return child;
    },
  };
}
function instrumentDigitHost(element) {
  element.rebuilds = 0;
  element.animAdds = 0;
  element.animRemoves = 0;
  element.reflows = 0;
  const width = element.offsetWidth;
  Object.defineProperty(element, 'offsetWidth', {
    configurable: true,
    get() {
      element.reflows += 1;
      return width;
    },
  });
  const replaceChildren = element.replaceChildren;
  const add = element.classList.add;
  const remove = element.classList.remove;
  element.replaceChildren = (...next) => {
    element.rebuilds += 1;
    replaceChildren.apply(element, next);
  };
  element.classList.add = name => {
    if (name === 'is-animating') {
      element.animAdds += 1;
    }
    add(name);
  };
  element.classList.remove = name => {
    if (name === 'is-animating') {
      element.animRemoves += 1;
    }
    remove(name);
  };
  return element;
}
globalThis.window = {};
globalThis.document = {
  addEventListener() {},
  getElementById: id => nodes.get(id) || null,
  createElement() {
    return createMockElement();
  },
};
function node(id) {
  const element = createMockElement();
  nodes.set(id, element);
  return element;
}
function digitText(element) {
  return element.children.map(child => child.textContent).join('');
}
function glyphNodes(element) {
  return element.children.filter(child => child.classList.contains('t-digit'));
}
function suffixNode(element) {
  return element.children.find(child => child.classList.contains('t-digit-suffix')) || null;
}
function glyphText(element, animating) {
  return glyphNodes(element)
    .filter(child => child.classList.contains('is-animating') === animating)
    .map(child => child.textContent)
    .join('');
}
const cards = await import('../dist/js/status-cards.js');
const { setAnimatedDigits } = await import('../dist/js/dom.js');
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

test('animated digits rebuild on change and skip unchanged text', () => {
  const host = instrumentDigitHost(createMockElement());
  setAnimatedDigits(null, '1.00x');
  setAnimatedDigits(host, '1.00x');
  assert.equal(digitText(host), '1.00x');
  assert.equal(host.children.length, 5);
  assert.equal(host.children[0].classList.contains('t-digit'), true);
  assert.equal(host.children[0].dataset.stagger, undefined);
  assert.equal(host.children[1].dataset.stagger, '1');
  assert.equal(host.children[1].style['--digit-i'], '1');
  assert.equal(host.classList.contains('t-digit-group'), true);
  assert.equal(glyphText(host, true), '1.00x');
  assert.equal(glyphText(host, false), '');
  assert.equal(host.rebuilds, 1);
  assert.equal(host.reflows, 1);

  setAnimatedDigits(host, '1.00x');
  assert.equal(host.rebuilds, 1);
  assert.equal(host.reflows, 1);
  assert.equal(digitText(host), '1.00x');

  setAnimatedDigits(host, '1.01x');
  assert.equal(digitText(host), '1.01x');
  assert.equal(glyphText(host, false), '1.0');
  assert.equal(glyphText(host, true), '1x');
  assert.equal(host.rebuilds, 2);
  assert.equal(host.reflows, 2);

  setAnimatedDigits(host, '3.01 Mb/s');
  setAnimatedDigits(host, '3.12 Mb/s');
  assert.equal(digitText(host), '3.12 Mb/s');
  assert.equal(glyphText(host, false), '3.');
  assert.equal(glyphText(host, true), '12');
  assert.equal(suffixNode(host)?.textContent, ' Mb/s');
  assert.equal(suffixNode(host)?.classList.contains('t-digit'), false);
  assert.equal(host.rebuilds, 4);
});

test('meter speed and bitrate pop in; time stays plain text', () => {
  const panel = node('bili-network-panel');
  const cacheMeter = node('bili-network-cache-meter');
  const cacheRate = instrumentDigitHost(node('bili-network-cache-rate'));
  const cacheSpeed = instrumentDigitHost(node('bili-network-cache-speed-ratio'));
  const cacheTime = node('bili-network-cache-time');
  const rate = instrumentDigitHost(node('bili-network-push-rate'));
  const speed = instrumentDigitHost(node('bili-network-push-speed-ratio'));
  const time = node('bili-network-push-time');
  const live = {
    ffmpeg_running: true,
    hls_cache_active: true,
    stream_bitrate_kbps: 1500,
    stream_speed: 1,
    stream_time_secs: 10,
    stream_fps: 30,
    stream_cache_bitrate_kbps: 800,
    stream_cache_speed: 1.02,
    stream_cache_time_secs: 9,
  };
  cards.renderBiliNetworkPanel(live);
  assert.equal(panel.classList.contains('hidden'), false);
  assert.equal(cacheMeter.style.display, '');
  assert.equal(digitText(rate), '1.50 Mb/s');
  assert.equal(glyphNodes(rate).map(child => child.textContent).join(''), '1.50');
  assert.equal(suffixNode(rate)?.textContent, ' Mb/s');
  assert.equal(digitText(speed), '1.00x');
  assert.equal(suffixNode(speed), null);
  assert.equal(digitText(cacheRate), '800 Kb/s');
  assert.equal(glyphNodes(cacheRate).map(child => child.textContent).join(''), '800');
  assert.equal(suffixNode(cacheRate)?.textContent, ' Kb/s');
  assert.equal(digitText(cacheSpeed), '1.02x');
  assert.equal(time.textContent, '0:10 · 30.0 fps');
  assert.equal(cacheTime.textContent, '0:09');
  assert.equal(time.children.length, 0);
  assert.equal(cacheTime.children.length, 0);
  assert.equal(rate.rebuilds, 1);
  assert.equal(speed.rebuilds, 1);
  assert.equal(cacheRate.rebuilds, 1);
  assert.equal(cacheSpeed.rebuilds, 1);
  assert.equal(speed.dataset.tone, 'ok');
  assert.equal(cacheSpeed.dataset.tone, 'ok');

  cards.renderBiliNetworkPanel({ ...live, stream_time_secs: 11, stream_cache_time_secs: 10 });
  assert.equal(time.textContent, '0:11 · 30.0 fps');
  assert.equal(cacheTime.textContent, '0:10');
  assert.equal(rate.rebuilds, 1);
  assert.equal(speed.rebuilds, 1);
  assert.equal(cacheRate.rebuilds, 1);
  assert.equal(cacheSpeed.rebuilds, 1);

  cards.renderBiliNetworkPanel({
    ...live,
    stream_bitrate_kbps: 2000,
    stream_speed: 0.95,
    stream_time_secs: 11,
    stream_cache_bitrate_kbps: 900,
    stream_cache_speed: 0.93,
    stream_cache_time_secs: 10,
  });
  assert.equal(digitText(rate), '2.00 Mb/s');
  assert.equal(suffixNode(rate)?.textContent, ' Mb/s');
  assert.equal(digitText(speed), '0.95x');
  assert.equal(digitText(cacheRate), '900 Kb/s');
  assert.equal(suffixNode(cacheRate)?.textContent, ' Kb/s');
  assert.equal(digitText(cacheSpeed), '0.93x');
  assert.equal(rate.rebuilds, 2);
  assert.equal(speed.rebuilds, 2);
  assert.equal(cacheRate.rebuilds, 2);
  assert.equal(cacheSpeed.rebuilds, 2);
  assert.equal(speed.dataset.tone, 'warn');
  assert.equal(cacheSpeed.dataset.tone, 'danger');
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
