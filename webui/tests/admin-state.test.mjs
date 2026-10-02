import test from 'node:test';
import assert from 'node:assert/strict';
import { createConfigPatch } from '../src/js/config-draft.js';
import { activeRoomLock, biliRoomStats, formatLiveClock, formatLiveDuration, formatLockEnd } from '../src/js/format.js';

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
    hasAttribute() { return false; },
    setAttribute() {},
    closest() { return null; },
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
  querySelector: selector => ({ '.card[data-platform="priority"]': nodes.get('priority-channel-card'), '.card[data-platform="twitch"]': nodes.get('twitch-card'), '.card[data-platform="niconico"]': nodes.get('niconico-card') })[selector] || null,
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
const cards = await import('../src/js/status-cards.js');
const { setAnimatedDigits } = await import('../src/js/dom.js');
const api = await import('../src/js/api.js');
const { saveBooleanToggle } = await import('../src/js/toggle-save.js');
const { applyMonitorToggleConfigState } = await import('../src/js/state.js');
const { holodexKeepAliveDue } = await import('../src/js/overview.js');

test('settings edits submit only changed fields and their loaded values', () => {
  const baseline = { interval: 30, enable_danmaku_command: true, api_key: 'old', keywords: ['a'] };
  const edited = { ...baseline, interval: 75 };
  assert.deepEqual(createConfigPatch(edited, baseline), { interval: 75, expected: { interval: 30 } });
  assert.equal(createConfigPatch(baseline, baseline), null);
  assert.throws(() => createConfigPatch(edited, null), /加载配置/);
  assert.deepEqual(createConfigPatch({ keywords: [] }, baseline), { keywords: [], expected: { keywords: ['a'] } });
});

test('biliRoomStats hides missing get_info fields instead of fabricating dashes', () => {
  assert.deepEqual(biliRoomStats({ is_live: true }), { online: null, liveStartTs: null });
  assert.deepEqual(biliRoomStats({ online: 12, live_start_ts: 1_700_000_000 }), {
    online: 12, liveStartTs: 1_700_000_000,
  });
  assert.equal(biliRoomStats({ status: { bilibili: { online: 9 } } }).online, 9);
  assert.equal(biliRoomStats({ online: 4, ffmpeg_running: true }).online, 4);
});

test('bilibili card paints live duration from get_info, not popularity', () => {
  const title = node('bili-title');
  const area = node('bili-area');
  const liveTime = node('bili-live-time');
  const liveTimeLabel = node('bili-live-time-label');
  const liveStats = node('bili-live-stats');
  const panel = node('bili-network-panel');
  node('bili-status');
  node('bili-danmaku-command-toggle');
  const start = Math.floor(Date.now() / 1000) - 90;
  const live = {
    is_live: true,
    title: 'Hello',
    area_name: '虚拟Gamer',
    area_id: 371,
    online: 89012,
    live_start_ts: start,
  };

  // Spare / idle-owner: room is live but this node is not publishing, so
  // the network block (and its 开播) stays off the Bilibili card.
  cards.renderBilibiliCard(live);
  assert.equal(title.textContent, 'Hello');
  assert.equal(area.textContent, '虚拟Gamer (371)');
  assert.equal(panel.classList.contains('hidden'), true);
  assert.equal(liveStats.classList.contains('hidden'), true);

  cards.renderBilibiliCard({ ...live, ffmpeg_running: true });
  assert.equal(panel.classList.contains('hidden'), false);
  assert.equal(liveStats.classList.contains('hidden'), false);
  assert.equal(liveTime.textContent, formatLiveDuration(start));
  assert.equal(liveTimeLabel.textContent, `开播 ${formatLiveClock(start)}`);

  cards.renderBilibiliCard({ ...live, ffmpeg_running: true }, { hideRoomStats: true });
  assert.equal(liveStats.classList.contains('hidden'), true);

  cards.renderBilibiliCard(live, { showNetwork: false });
  assert.equal(liveStats.classList.contains('hidden'), false);
  assert.equal(panel.classList.contains('hidden'), true);

  cards.renderBilibiliCard({ is_live: true, ffmpeg_running: true, title: 'Hello', area_name: '虚拟Gamer', area_id: 371 });
  assert.equal(liveStats.classList.contains('hidden'), true);

  cards.renderBilibiliCard({ is_live: false, title: 'Idle', area_name: '其他单机', area_id: 235 });
  assert.equal(panel.classList.contains('hidden'), true);
});

test('a room lock shows the ban end and blocks start until lock_till', () => {
  const status = node('bili-status');
  const row = node('bili-lock-row');
  row.classList.add('hidden');
  const end = node('bili-lock-end');
  const start = node('startLiveBtn');
  start.title = '开始直播';
  const badge = node('app-live-badge');
  const badgeText = node('app-live-badge-text');
  node('bili-title');
  node('bili-area');
  node('bili-danmaku-command-toggle');
  const panel = node('bili-network-panel');

  const till = 1_790_881_195;
  const now = (till - 140) * 1000;
  assert.equal(activeRoomLock({ room_locked: true, lock_till: till }, now)?.lockTill, till);
  assert.equal(activeRoomLock({ room_locked: true, lock_till: till }, till * 1000), null);
  assert.equal(activeRoomLock({ room_locked: false, lock_till: till }, now), null);
  const date = new Date(till * 1000);
  const pad = value => String(value).padStart(2, '0');
  assert.equal(
    formatLockEnd(till),
    `${date.getFullYear()}-${pad(date.getMonth() + 1)}-${pad(date.getDate())} ${pad(date.getHours())}:${pad(date.getMinutes())}:${pad(date.getSeconds())}`
  );

  const future = Math.floor(Date.now() / 1000) + 3600;
  cards.renderBilibiliCard({
    is_live: true,
    ffmpeg_running: true,
    title: '示例频道 001',
    room_locked: true,
    lock_till: future,
  });
  assert.equal(panel.classList.contains('hidden'), true);
  assert.equal(status.className, 'status-indicator status-locked');
  assert.equal(row.classList.contains('hidden'), false);
  assert.equal(end.textContent, formatLockEnd(future));
  assert.equal(start.disabled, true);
  assert.match(start.title, /解封时间/);
  assert.equal(badge.classList.contains('is-locked'), true);
  assert.equal(badge.classList.contains('is-live'), false);
  assert.equal(badgeText.textContent, '封禁中');

  cards.renderBilibiliCard({ is_live: false, title: 'Idle', room_locked: true, lock_till: 1_700_000_000 });
  assert.equal(status.className, 'status-indicator status-offline');
  assert.equal(row.classList.contains('hidden'), true);
  assert.equal(start.disabled, false);
  assert.equal(start.title, '开始直播');
  assert.equal(badgeText.textContent, '未开播');
});

test('the public Bilibili card shows the lock row and hides live duration', () => {
  const status = node('bili-status');
  const row = node('bili-lock-row');
  row.classList.add('hidden');
  const end = node('bili-lock-end');
  const liveStats = node('bili-live-stats');
  liveStats.classList.remove('hidden');
  const badge = node('app-live-badge');
  const badgeText = node('app-live-badge-text');
  node('bili-live-time');
  node('bili-live-time-label');
  node('bili-title');
  node('bili-area');
  node('bili-danmaku-command-toggle');
  const future = Math.floor(Date.now() / 1000) + 3600;
  const start = Math.floor(Date.now() / 1000) - 90;

  cards.renderBilibiliCard({
    is_live: false,
    title: '示例频道 001 | 示例直播',
    area_name: '其他单机',
    area_id: 235,
    live_start_ts: start,
    room_locked: true,
    lock_till: future,
  }, { readonly: true, showNetwork: false });

  assert.equal(status.className, 'status-indicator status-locked');
  assert.equal(row.classList.contains('hidden'), false);
  assert.equal(end.textContent, formatLockEnd(future));
  assert.equal(liveStats.classList.contains('hidden'), true);
  assert.equal(badge.classList.contains('is-locked'), true);
  assert.equal(badge.classList.contains('is-live'), false);
  assert.equal(badgeText.textContent, '封禁中');

  cards.renderBilibiliCard({ is_live: false, title: 'Idle' }, { readonly: true, showNetwork: false });
});

test('room stays live after handoff, but local network graph disappears', () => {
  const panel = node('bili-network-panel');
  cards.renderBiliNetworkPanel({ is_live: true, ffmpeg_running: true, stream_speed: 1 });
  assert.equal(panel.classList.contains('hidden'), false);
  cards.renderBiliNetworkPanel({ is_live: true, ffmpeg_running: false, stream_speed: 0, stream_quality: '流畅' });
  assert.equal(panel.classList.contains('hidden'), true);
  assert.equal(cards.isBiliNetworkLive(), false);
  assert.equal(cards.getBiliNetworkQuality(), null);
});

test('running stalled publisher stays visible; old quality is not reused', () => {
  const panel = node('bili-network-panel');
  cards.renderBiliNetworkPanel({ ffmpeg_running: true, stream_speed: 0 });
  assert.equal(panel.classList.contains('hidden'), false);
  assert.equal(cards.getBiliNetworkQuality(), '卡顿');
  cards.renderBiliNetworkPanel({ ffmpeg_running: true });
  assert.equal(panel.classList.contains('hidden'), false);
  assert.equal(cards.getBiliNetworkQuality(), null);
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

test('bili network meters stay plain text; time stays plain text', () => {
  const panel = node('bili-network-panel');
  const cacheMeter = node('bili-network-cache-meter');
  const cacheRate = node('bili-network-cache-rate');
  const cacheSpeed = node('bili-network-cache-speed-ratio');
  const cacheTime = node('bili-network-cache-time');
  const rate = node('bili-network-push-rate');
  const speed = node('bili-network-push-speed-ratio');
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
  assert.equal(rate.textContent, '1.50 Mb/s');
  assert.equal(rate.children.length, 0);
  assert.equal(speed.textContent, '1.00x');
  assert.equal(speed.children.length, 0);
  assert.equal(cacheRate.textContent, '800 Kb/s');
  assert.equal(cacheRate.children.length, 0);
  assert.equal(cacheSpeed.textContent, '1.02x');
  assert.equal(time.textContent, '0:10 · 30.0 fps');
  assert.equal(cacheTime.textContent, '0:09');
  assert.equal(time.children.length, 0);
  assert.equal(cacheTime.children.length, 0);
  assert.equal(speed.dataset.tone, 'ok');
  assert.equal(cacheSpeed.dataset.tone, 'ok');

  cards.renderBiliNetworkPanel({ ...live, stream_time_secs: 11, stream_cache_time_secs: 10 });
  assert.equal(time.textContent, '0:11 · 30.0 fps');
  assert.equal(cacheTime.textContent, '0:10');
  assert.equal(rate.textContent, '1.50 Mb/s');
  assert.equal(speed.textContent, '1.00x');

  cards.renderBiliNetworkPanel({
    ...live,
    stream_bitrate_kbps: 2000,
    stream_speed: 0.95,
    stream_time_secs: 11,
    stream_cache_bitrate_kbps: 900,
    stream_cache_speed: 0.93,
    stream_cache_time_secs: 10,
  });
  assert.equal(rate.textContent, '2.00 Mb/s');
  assert.equal(rate.children.length, 0);
  assert.equal(speed.textContent, '0.95x');
  assert.equal(cacheRate.textContent, '900 Kb/s');
  assert.equal(cacheSpeed.textContent, '0.93x');
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

test('YouTube key pool status reads as one line per fact', async () => {
  const { formatKeyPoolSummary, formatKeyState, formatPlaylistPolling } = await import('../src/js/format.js');
  const resetsAt = '2026-09-26T08:00:00Z';
  const at = new Date(resetsAt);
  const clock = `${String(at.getHours()).padStart(2, '0')}:${String(at.getMinutes()).padStart(2, '0')}`;
  const data = {
    configured: true,
    keys: [
      { fingerprint: 'a1b2c3', used: 1234, state: 'usable' },
      { fingerprint: 'd4e5f6', used: 9000, state: 'exhausted' },
      { fingerprint: '0f9e8d', used: 0, state: 'rejected' },
    ],
    budget_per_key: 9000,
    remaining_fraction: 0.29,
    resets_at: resetsAt,
  };
  assert.equal(
    formatKeyPoolSummary(data),
    `今日已用 10,234 / 27,000 单位 · 剩余 29% · 1/3 个 key 可用 · ${clock} 重置`,
  );
  assert.deepEqual(data.keys.map(formatKeyState), [
    'a1b2c3 · 1,234',
    'd4e5f6 · 今日额度已用完',
    '0f9e8d · 被拒绝（检查 key 或 API 是否启用）',
  ]);

  const playlist = { on: true, interval_secs: 134, stretch: 1, rss_down: false, keys_needed: 3 };
  assert.equal(formatPlaylistPolling(playlist), '上传列表轮询：每频道 134s');
  assert.equal(
    formatPlaylistPolling({ ...playlist, interval_secs: 268, stretch: 2, rss_down: true }),
    'RSS 故障，上传列表每频道 268s（配额留给索引，间隔 ×2）',
  );
  assert.equal(
    formatPlaylistPolling({ ...playlist, on: false, interval_secs: 533 }),
    '上传列表轮询：关闭（RSS 正常；3 个可用 key 起常规轮询）',
  );
  assert.equal(
    formatPlaylistPolling({ ...playlist, interval_secs: 79, by_hour: true }),
    '上传列表轮询：每频道 79s（按开播时段）',
  );
  assert.equal(
    formatPlaylistPolling({ ...playlist, interval_secs: 104, stretch: 1.4 }),
    '上传列表轮询：每频道 104s（配额留给索引，间隔 ×1.4）',
  );
  assert.equal(
    formatPlaylistPolling({ ...playlist, interval_secs: null, paused: true }),
    '上传列表轮询：暂停（今日剩余配额留给索引与转播目标）',
  );
  assert.equal(
    formatPlaylistPolling({ ...playlist, on: false, interval_secs: null }),
    '上传列表轮询：关闭（没有可用 key）',
  );
  assert.equal(formatPlaylistPolling(null), '', 'no line before the worker has run');
});

test('go-live hours rotate to local time and read without hovering', async () => {
  const { goliveHourRows, formatGoliveSummary } = await import('../src/js/format.js');
  // UTC 11:00 is the busiest hour; 12:00 UTC is "now".
  const hours = Array.from({ length: 24 }, (_, utc) => ({
    golive_share: utc === 11 ? 0.2 : 0.8 / 23,
    interval_secs: utc === 11 ? 79 : 900,
  }));
  const playlist = { by_hour: true, hours, golives: 443, golives_needed: 50, interval_secs: 900 };
  const now = new Date('2026-09-28T12:30:00Z');
  const rows = goliveHourRows(playlist, 9, now);
  assert.equal(rows.length, 24);
  assert.deepEqual([rows[20].hour, rows[20].share, rows[20].intervalSecs], [20, 0.2, 79], 'UTC 11 is 20:00 at UTC+9');
  assert.equal(rows[21].current, true, 'UTC 12 is 21:00 at UTC+9');
  assert.equal(rows.filter(row => row.current).length, 1);
  assert.equal(rows[20].pollsPerHour, 3600 / 79);
  assert.equal(goliveHourRows(playlist, -5, now)[6].intervalSecs, 79, 'UTC-5 wraps around');
  assert.equal(formatGoliveSummary(playlist, rows), '开播最多 20 点（20%）· 最快 20 点每 79s · 最慢每 900s');

  const flat = { ...playlist, by_hour: false, golives: 12, interval_secs: 238 };
  assert.equal(
    formatGoliveSummary(flat, goliveHourRows(flat, 9, now)),
    '已记录 12 次开播，还需 38 次后按开播时段轮询，目前均匀每频道 238s',
  );
  assert.deepEqual(goliveHourRows({ hours: [] }), [], 'no chart before the worker reports hours');
});

test('key meters and discovery tiles carry state as label, not color alone', async () => {
  const { keyMeterRows, discoveryTiles } = await import('../src/js/format.js');
  const rows = keyMeterRows({
    keys: [
      { fingerprint: 'a1b2c3', used: 4500, state: 'usable' },
      { fingerprint: 'd4e5f6', used: 9000, state: 'exhausted' },
      { fingerprint: '0f9e8d', used: 0, state: 'rejected' },
    ],
    budget_per_key: 9000,
  });
  assert.deepEqual(rows.map(r => [r.fraction, r.value]), [
    [0.5, '4,500 / 9,000'],
    [1, '今日额度已用完'],
    [0, '被拒绝（检查 key 或 API 是否启用）'],
  ]);

  const playlist = { on: true, interval_secs: 268, stretch: 1, rss_down: false, websub_slowed: true };
  const websub = { verified: 37, pending: 0, failed: 0, last_push: null, healthy: false, error: null };
  const tiles = discoveryTiles({ playlist, websub });
  assert.deepEqual(tiles.map(t => [t.name, t.tone, t.label]), [
    ['RSS', 'ok', '正常'],
    ['上传列表', 'ok', '每频道 268s'],
    ['WebSub', 'warn', '37 已验证'],
  ]);
  assert.equal(tiles[1].detail, '上传列表轮询：每频道 268s（WebSub 正常，放慢一倍）');
  assert.equal(tiles[2].detail, 'WebSub 订阅：已验证 37 · 等待 0 · 失败 0 · 尚未收到推送');
  assert.equal(discoveryTiles({ playlist }).length, 2, 'no WebSub tile while it is off');
  const paused = discoveryTiles({ playlist: { ...playlist, interval_secs: null, paused: true } })[1];
  assert.deepEqual([paused.tone, paused.label], ['warn', '暂停']);
});

test('an open Holodex panel renews its lease every 4 min, every minute without SSE', () => {
  const minute = 60 * 1000;
  assert.equal(holodexKeepAliveDue(4 * minute - 1, true), false);
  assert.equal(holodexKeepAliveDue(4 * minute, true), true);
  assert.equal(holodexKeepAliveDue(minute - 1, false), false);
  assert.equal(holodexKeepAliveDue(minute, false), true);
});


test('priority visibility is independent of monitoring and survives config refreshes', async () => {
  const { mergeConfigData, applyDashboardCardVisibility } = await import('../src/js/state.js');
  const card = node('priority-channel-card');
  const twitch = node('twitch-card');
  const niconico = node('niconico-card');
  mergeConfigData({ show_priority_channel: false, priority_channel: { enabled: true, auto_restart: true, channel_name: 'demo' } });
  assert.ok(card.classList.contains('hidden'));
  assert.ok(niconico.classList.contains('hidden'));
  assert.equal(twitch.classList.contains('hidden'), false);
  mergeConfigData({ show_twitch: false, show_niconico: true });
  assert.ok(twitch.classList.contains('hidden'));
  assert.equal(niconico.classList.contains('hidden'), false);
  assert.equal(window.configData.priority_channel.enabled, true);
  mergeConfigData({ priority_channel: { enabled: true } });
  applyDashboardCardVisibility();
  assert.ok(card.classList.contains('hidden'));
  mergeConfigData({ show_priority_channel: true });
  assert.equal(card.classList.contains('hidden'), false);
  assert.deepEqual(window.configData.priority_channel, { enabled: true, auto_restart: true, channel_name: 'demo' });
});

test('RSS disabled, failed and unavailable are distinct discovery states', async () => {
  const { discoveryTiles, formatPlaylistPolling } = await import('../src/js/format.js');
  const playlist = { on: true, interval_secs: 180, rss_enabled: false, rss_down: false };
  assert.equal(discoveryTiles({ playlist })[0].label, '已关闭');
  assert.match(formatPlaylistPolling(playlist), /RSS 已关闭/);
  assert.equal(discoveryTiles({ playlist: { ...playlist, rss_enabled: true, rss_down: true } })[0].label, '故障');
  assert.equal(discoveryTiles({})[0].label, '未运行');
});
