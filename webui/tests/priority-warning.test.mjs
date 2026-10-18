import test from 'node:test';
import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { createPriorityWarningControl } from '../src/js/priority-warning.js';

function fixture(post) {
  const calls = [];
  const row = { style: {}, classList: { toggle() {} } };
  const button = { disabled: false, addEventListener(event, fn) { this[event] = fn; } };
  let status = { channel_name: '示例频道 001', warning_held: true, auto_restart: false };
  let control;
  control = createPriorityWarningControl({ row, button,
    post: async (path, body) => { calls.push([path, body]); return post?.() ?? { success: true }; },
    refresh: async () => control.render(status),
    notify: (message, type) => calls.push([type, message]),
  });
  return { calls, row, button, control, status: value => { status = value; } };
}

test('hold release is hidden for absent and old status, regardless of the restart toggle', () => {
  const { control, row, button } = fixture();
  assert.equal(row.style.display, 'none');
  for (const status of [null, { channel_name: '示例频道 001', auto_restart: true }, { warning_held: false }]) {
    control.render(status);
    assert.equal(row.style.display, 'none');
    assert.equal(button.disabled, true);
  }
  control.render({ channel_name: '示例频道 001', warning_held: true, auto_restart: false });
  assert.equal(row.style.display, 'flex');
  assert.equal(button.disabled, false);
});

test('one click releases the rendered channel only and reconciles fresh status', async () => {
  let resolve;
  const pending = new Promise(done => { resolve = done; });
  const f = fixture(() => pending);
  const status = { channel_name: '示例频道 001', warning_held: true, auto_restart: false };
  f.control.render(status);
  const first = f.button.click();
  await f.button.click();
  assert.equal(f.button.disabled, true);
  assert.deepEqual(f.calls, [['/api/priority-channel/release-warning', {
    channel_name: '示例频道 001',
    platform: 'priority',
  }]]);
  f.status({ ...status, warning_held: false });
  resolve({ success: true });
  await first;
  assert.equal(f.row.style.display, 'none');
  assert.equal(f.button.disabled, true);
  assert.deepEqual(status, { channel_name: '示例频道 001', warning_held: true, auto_restart: false });
});

test('a refused release preserves the server hold and remains retryable', async () => {
  const f = fixture(() => ({ success: false, message: '优先频道已变更，请刷新后重试' }));
  f.control.render({ channel_name: '示例频道 001', warning_held: true });
  await f.control.release();
  assert.equal(f.row.style.display, 'flex');
  assert.equal(f.button.disabled, false);
  assert.deepEqual(f.calls[1], ['error', '优先频道已变更，请刷新后重试']);
});

test('a twitch card posts that platform for the held channel', async () => {
  const calls = [];
  const row = { style: {}, classList: { toggle() {} } };
  const button = { disabled: false, addEventListener(event, fn) { this[event] = fn; } };
  const control = createPriorityWarningControl({
    platform: 'TW',
    row,
    button,
    post: async (path, body) => { calls.push([path, body]); return { success: true }; },
    refresh: async () => {},
    notify: () => {},
  });
  control.render({ channel_name: '示例频道 002', warning_held: true });
  await control.release();
  assert.deepEqual(calls, [['/api/priority-channel/release-warning', {
    channel_name: '示例频道 002',
    platform: 'TW',
  }]]);
});

test('each YouTube, Twitch, Niconico and priority card has a hidden 解除临时跳过 row', async () => {
  const html = await readFile(new URL('../src/index.html', import.meta.url), 'utf8');
  const css = await readFile(new URL('../src/styles.css', import.meta.url), 'utf8');
  assert.match(css, /\.warning-hold-row /, 'the hold row style is shared, not one id');
  for (const [platform, row, button] of [
    ['youtube', 'yt-warning-row', 'release-yt-warning-btn'],
    ['twitch', 'tw-warning-row', 'release-tw-warning-btn'],
    ['niconico', 'nc-warning-row', 'release-nc-warning-btn'],
    ['priority', 'priority-warning-row', 'release-priority-warning-btn'],
  ]) {
    const article = html.match(
      new RegExp(`<article class="card[^"]*" data-platform="${platform}"[\\s\\S]*?</article>`)
    )?.[0];
    assert.ok(article, `${platform} card is present`);
    assert.match(article, new RegExp(`id="${row}"`), `${platform} hosts its own hold row`);
    assert.match(article, new RegExp(`id="${button}"`), `${platform} hosts its own release button`);
    assert.match(article, /解除临时跳过/);
  }
});
