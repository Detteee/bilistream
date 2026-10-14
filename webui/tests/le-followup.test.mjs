import test from 'node:test';
import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { activePushView, formatLocalShort, pickBudget, pushLimited, wakeupBudgetView, LIVE_EVENTS_HINT, LIVE_EVENTS_HINT_PUSH } from '../src/js/notify-state.js';
import { activePushValueInto, budgetSegments, renderLiveEventsInto } from '../src/js/live-events-view.js';
import { createNotifyState, PUSH_MODES } from '../mock/notify-fixtures.mjs';

class El {
  constructor() { this.dataset = {}; this.attrs = {}; this.hidden = false; this.children = []; this._text = ''; this.className = ''; this.disabled = false; }
  get textContent() { return this.children.length ? this.children.map(c => c.textContent).join('') : this._text; }
  set textContent(v) { this.children = []; this._text = String(v); }
  replaceChildren(...k) { this.children = k; this._text = ''; }
  append(...k) { this.children.push(...k); }
  getAttribute(k) { return k in this.attrs ? this.attrs[k] : null; }
  setAttribute(k, v) { this.attrs[k] = String(v); }
  removeAttribute(k) { delete this.attrs[k]; }
}
const doc = { createElement: () => new El(), createTextNode: t => ({ textContent: t, text: true }) };
const els = () => ({ group: new El(), line: new El(), main: new El(), next: new El(), passive: new El(), error: new El(), owner: new El(), input: new El(), help: new El() });
const T = '2026-10-14T08:40:00Z';
const local = formatLocalShort(Date.parse(T) / 1000);
const B = { wakeup_remaining: 2, wakeup_reserve: 2, wakeup_next_close_at: 1_790_000_000, passive_window_open: true };

test('§17.8 主动推送 row: ok / no_permission / rate_limited / null / absent / stale', () => {
  assert.deepEqual(activePushView({ active_push: { last_result: 'ok', last_at: T, refused_since: null, month_sent: 37 } }), { segments: ['可用', local, '本月 37 条'], tone: '' });
  assert.deepEqual(activePushView({ active_push: { last_result: 'no_permission', last_at: T, refused_since: T, month_sent: 3 } }), { segments: ['受限（无主动消息权限）', `自 ${local}`], tone: 'warn' });
  assert.deepEqual(activePushView({ active_push: { last_result: 'rate_limited', last_at: T, refused_since: T, month_sent: 998 } }), { segments: ['受限（频率限制）', `自 ${local}`, '本月 998 条'], tone: 'warn' });
  assert.deepEqual(activePushView({ active_push: { last_result: null, last_at: null, refused_since: null, month_sent: 0 } }), { segments: ['—'], tone: '' });
  assert.deepEqual(activePushView({ active_push: null }), { segments: ['—'], tone: '' });
  assert.equal(activePushView({}), null, 'missing field → no row');
  assert.equal(activePushView(null), null);
  assert.deepEqual(activePushView({ active_push: { last_result: 'rate_limited', refused_since: T } }, { available: false }), { segments: ['—'], tone: '' }, '90 s stale → —');
  const v = activePushValueInto(doc, { active_push: { last_result: 'rate_limited', refused_since: T, month_sent: 5 } });
  assert.equal(v.textContent, `受限（频率限制） · 自 ${local} · 本月 5 条`);
  assert.equal(v.dataset.tone, 'warn');
  assert.ok(v.children.filter(c => !c.text).every(c => c.className === 'notify-push-seg'), 'one nowrap span per segment');
  assert.equal(activePushValueInto(doc, {}), null);
});

test('§17.8 budget line: 备用： prefix, warn and passive hint only while push is not ok', () => {
  const ok = wakeupBudgetView(B, { push: { last_result: 'ok' } });
  assert.equal(ok.main, '备用：唤醒额度 2/4 · 保留 2 用于故障提醒');
  assert.equal(ok.tone, 'ok', 'neutral at N ≤ reserve while push works');
  assert.equal(ok.passive, false, 'no passive hint while push works');
  for (const r of ['no_permission', 'rate_limited']) {
    const v = wakeupBudgetView(B, { push: { last_result: r } });
    assert.equal(v.tone, 'warn');
    assert.equal(v.main, '备用：唤醒额度 2/4 · 保留 2 用于故障提醒，开播/下播只用被动回复');
    assert.equal(v.passive, true);
    assert.ok(pushLimited({ last_result: r }));
  }
  const unknown = wakeupBudgetView(B, { push: { last_result: null } });
  assert.equal(unknown.tone, 'warn', 'null is not ok');
  assert.equal(unknown.passive, false, 'null is not limited');
  assert.equal(wakeupBudgetView({ ...B, wakeup_remaining: 3 }, { push: { last_result: 'rate_limited' } }).tone, 'ok');
  assert.equal(wakeupBudgetView({ ...B, passive_window_open: false }, { push: { last_result: 'rate_limited' } }).passive, false);
  // Without active_push the §17 rules are unchanged.
  const legacy = wakeupBudgetView(B);
  assert.equal(legacy.main, '唤醒额度 2/4 · 保留 2 用于故障提醒，开播/下播只用被动回复');
  assert.equal(legacy.passive, true);
  assert.deepEqual(budgetSegments(ok)[0], { text: '备用：唤醒额度 2/4' });
  assert.equal(wakeupBudgetView(B, { push: { last_result: 'ok' }, available: false }).main, '备用：唤醒额度 —');
});

test('§17.8 switch hint follows active_push presence', () => {
  const e = els();
  renderLiveEventsInto(doc, e, { status: { quota: B, active_push: { last_result: 'ok' } }, statusAt: 0, now: 0 });
  assert.equal(e.help.textContent, LIVE_EVENTS_HINT_PUSH);
  assert.equal(LIVE_EVENTS_HINT_PUSH, '优先主动推送；被限制时改用回复窗口或唤醒额度，故障提醒优先');
  renderLiveEventsInto(doc, e, { status: { quota: B }, statusAt: 0, now: 0 });
  assert.equal(e.help.textContent, LIVE_EVENTS_HINT);
});

test('Clerk L2: a redraw re-enables the switch as well as disabling it', () => {
  const e = els();
  const remote = { quota: B, openid_writer: { node_id: 'jp', is_local: false } };
  const local = { quota: B, openid_writer: { node_id: 'ny', is_local: true } };
  renderLiveEventsInto(doc, e, { status: remote, statusAt: 0, now: 0 });
  assert.equal(e.input.disabled, true);
  renderLiveEventsInto(doc, e, { status: local, statusAt: 0, now: 0 });
  assert.equal(e.input.disabled, false, 'writer is local again → enabled by the tick path alone');
  renderLiveEventsInto(doc, e, { status: local, statusAt: 0, now: 0, disabled: true });
  assert.equal(e.input.disabled, true, 'panel lock (busy/loading) still wins');
  renderLiveEventsInto(doc, e, { status: null, statusAt: 0, now: 0 });
  assert.equal(e.input.disabled, false, 'unknown writer → enabled');
});

test('Clerk L1 / N1: one timer handle, cleared on re-init, view/visibility gated; no dead helpers', async () => {
  const src = await readFile(new URL('../src/js/notify.js', import.meta.url), 'utf8');
  assert.match(src, /if \(liveTick\) clearInterval\(liveTick\);\n\s+liveTick = setInterval\(liveTickFn, LIVE_EVENTS_TICK_MS\);/);
  assert.equal(src.match(/setInterval\(liveTickFn/g).length, 1);
  assert.match(src, /return Boolean\(settings\) && isViewActive\('settings'\) && !document\.hidden;/);
  assert.match(src, /if \(!liveVisibilityHooked\)/, 'visibility listener added once');
  assert.ok(!/function setText|function setHidden/.test(src), 'unused helpers removed');
  assert.match(src, /\n  form\.watch_enabled = Boolean/, 'readForm indentation fixed');
});

test('mock active_push states', () => {
  const m = createNotifyState(() => 1_790_000_000);
  const results = {};
  for (const mode of PUSH_MODES) {
    assert.ok(m.setMode(mode));
    const st = m.route('GET', '/api/notify/status')[1].data;
    results[mode] = st.active_push.last_result;
    assert.ok(!/last_admin|passive_replies|openid"/.test(JSON.stringify(st)));
    assert.ok(activePushView(st));
    assert.ok(pickBudget(st).passive_window_open);
  }
  assert.deepEqual(results, { 'push-ok': 'ok', 'push-no-permission': 'no_permission', 'push-rate-limited': 'rate_limited', 'push-null': null });
  m.setMode('ok');
  assert.ok(!('active_push' in m.route('GET', '/api/notify/status')[1].data), 'other modes: older backend, no row');
});
