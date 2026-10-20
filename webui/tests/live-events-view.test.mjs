import test from 'node:test';
import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { budgetSegments, renderLiveEventsInto, setLiveEventsError, STATUS_STALE_MS, LIVE_EVENTS_TICK_MS } from '../src/js/live-events-view.js';
import { createNotifyState } from '../mock/notify-fixtures.mjs';
import { wakeupBudgetView } from '../src/js/notify-state.js';

// Minimal fake DOM: enough for textContent, hidden, dataset, attributes, children.
class El {
  constructor(tag = 'span') { this.tag = tag; this.dataset = {}; this.attrs = {}; this.hidden = false; this.children = []; this._text = ''; this.className = ''; this.disabled = false; }
  get textContent() { return this.children.length ? this.children.map(c => c.textContent).join('') : this._text; }
  set textContent(v) { this.children = []; this._text = String(v); }
  replaceChildren(...kids) { this.children = kids; this._text = ''; }
  getAttribute(k) { return k in this.attrs ? this.attrs[k] : null; }
  setAttribute(k, v) { this.attrs[k] = String(v); }
  removeAttribute(k) { delete this.attrs[k]; }
}
const doc = { createElement: t => new El(t), createTextNode: t => ({ textContent: t, text: true }) };
function els() {
  const input = new El('input');
  input.setAttribute('aria-describedby', 'notify-live-events-help notify-wakeup-budget');
  return { group: new El('div'), line: new El('p'), main: new El(), next: new El(), passive: new El(), error: new El(), owner: new El(), input };
}

test('B1: a blocked /status falls back to — after 90 s, driven by failed polls and the tick', async () => {
  const mock = createNotifyState(() => 1_790_000_000);
  mock.setMode('ok');
  const e = els();
  let clock = 1_000_000;           // controlled ms clock
  let status = null, statusAt = 0, settings = mock.route('GET', '/api/notify')[1].data;
  let blocked = false;
  const fetchStatus = async () => { if (blocked) throw new Error('blocked'); return mock.route('GET', '/api/notify/status')[1].data; };
  const draw = () => renderLiveEventsInto(doc, e, { status, settings, statusAt, now: clock });
  // Mirrors refreshNotifyStatus: success stores the status, failure still redraws.
  const poll = async () => { try { status = await fetchStatus(); statusAt = clock; } catch {} draw(); };
  await poll();
  assert.equal(e.main.textContent, '唤醒额度 3/4 · 保留 2 用于故障提醒');
  assert.equal(e.next.hidden, false);
  assert.equal(e.passive.hidden, false);
  blocked = true;
  for (const [dt, dashed] of [[30_000, false], [60_000, false], [89_999, false], [STATUS_STALE_MS, true], [130_000, true]]) {
    clock = 1_000_000 + dt;
    if (dt % 30_000 === 0) await poll(); else draw();   // failed poll or the timer tick
    assert.equal(e.main.textContent === '唤醒额度 —', dashed, `t+${dt}`);
    assert.equal(e.next.hidden, dashed);
    assert.equal(e.passive.hidden, dashed);
    assert.equal(e.line.dataset.tone, dashed ? 'muted' : 'ok');
  }
  assert.ok(LIVE_EVENTS_TICK_MS < STATUS_STALE_MS);
  blocked = false;
  await poll();
  assert.equal(e.main.textContent, '唤醒额度 3/4 · 保留 2 用于故障提醒', 'recovers on the next good poll');
  const src = await readFile(new URL('../src/js/notify.js', import.meta.url), 'utf8');
  // These three assert the wiring still exists in source. They fail on a rename
  // and cannot catch a behaviour break; the behavioural checks above carry that
  // weight.
  assert.match(src, /catch \(error\) \{[^}]*renderLiveEvents\(\);/, 'poll catch still redraws');
  assert.match(src, /liveTick = setInterval\(liveTickFn, LIVE_EVENTS_TICK_MS\)/, 'live tick still wired to the shared interval constant');
  assert.match(src, /function liveTickFn\(\) \{ if \(liveTickActive\(\)\) \{ renderStatus\(\); renderLiveEvents\(\); \} \}/, 'tick still gated on liveTickActive()');
});

test('B2: the invalid_field error is described, marks the switch invalid, and clears', async () => {
  const e = els();
  setLiveEventsError(e, '开播/下播开关的值无效，请刷新后重试。');
  assert.equal(e.error.hidden, false);
  assert.equal(e.input.getAttribute('aria-describedby'), 'notify-live-events-help notify-wakeup-budget notify-live-events-error');
  assert.equal(e.input.getAttribute('aria-invalid'), 'true');
  setLiveEventsError(e, '开播/下播开关的值无效，请刷新后重试。');
  assert.equal(e.input.getAttribute('aria-describedby').split(' ').filter(x => x === 'notify-live-events-error').length, 1, 'no duplicates');
  setLiveEventsError(e, '');
  assert.equal(e.error.hidden, true);
  assert.equal(e.input.getAttribute('aria-describedby'), 'notify-live-events-help notify-wakeup-budget');
  assert.equal(e.input.getAttribute('aria-invalid'), null);
  const html = await readFile(new URL('../src/index.html', import.meta.url), 'utf8');
  assert.match(html, /id="notify-live-events-error" role="alert"/);
});

test('B3: budget phrases are separate nowrap spans; breaks only at · and ，', async () => {
  const low = budgetSegments(wakeupBudgetView({ wakeup_remaining: 2, wakeup_reserve: 2 }));
  assert.deepEqual(low, [{ text: '唤醒额度 2/4' }, { sep: ' · ' }, { text: '保留 2 用于故障提醒' }, { sep: '，' }, { text: '开播/下播只用被动回复', suffix: true }]);
  assert.deepEqual(budgetSegments(wakeupBudgetView({}, { available: false })), [{ text: '唤醒额度 —' }]);
  const e = els();
  renderLiveEventsInto(doc, e, { status: { quota: { wakeup_remaining: 1, wakeup_reserve: 2, wakeup_next_close_at: 1_790_000_000 } }, statusAt: 0, now: 0 });
  const spans = [...e.main.children, ...e.next.children].filter(c => !c.text);
  assert.ok(spans.every(s => s.className.includes('notify-budget-seg')));
  assert.ok(e.main.children.find(c => !c.text && c.className.includes('notify-budget-suffix')));
  assert.match(e.next.textContent, /^ · 下一档 \d\d-\d\d \d\d:\d\d 到期$/);
  const css = await readFile(new URL('../src/styles.css', import.meta.url), 'utf8');
  assert.match(css, /\.notify-budget-seg \{ white-space: nowrap; \}/);
  assert.ok(!/notify-budget \{[^}]*max-width/.test(css), '60ch cap dropped');
});

test('B4: the toggle row checkbox has a focus-visible outline', async () => {
  const css = await readFile(new URL('../src/styles.css', import.meta.url), 'utf8');
  assert.match(css, /\.notify-toggle-row input:focus-visible \{\s*outline: 2px solid var\(--brand\);/);
});

test('the 通知类型 group renders the meters', () => {
  const e = els();
  const st = { quota: { wakeup_remaining: 3, wakeup_reserve: 2, passive_window_open: true } };
  const v = renderLiveEventsInto(doc, e, { status: st, statusAt: 0, now: 0 });
  assert.equal(e.group.hidden, false);
  assert.equal(v.tone, 'ok');
});

test('markup ships hidden and no capability gate remains', async () => {
  const html = await readFile(new URL('../src/index.html', import.meta.url), 'utf8');
  assert.match(html, /id="notify-types" hidden>/);
});

test('no live-event capability check remains in the source', async () => {
  for (const file of ['notify.js', 'notify-state.js', 'live-events-view.js']) {
    const src = await readFile(new URL(`../src/js/${file}`, import.meta.url), 'utf8');
    assert.doesNotMatch(src, /liveEventsSupported|supported: liveEventsSupported/, file);
  }
});

test('the 通知类型 group, hidden in the markup, is shown after a render', async () => {
  const e = els();
  e.group.hidden = true;
  const mock = createNotifyState(() => 1_790_000_000);
  mock.setMode('ok');
  const settings = mock.route('GET', '/api/notify')[1].data;
  const status = mock.route('GET', '/api/notify/status')[1].data;
  renderLiveEventsInto(doc, e, { status, settings, statusAt: 1_000_000, now: 1_000_000 });
  assert.equal(e.group.hidden, false);
  const html = await readFile(new URL('../src/index.html', import.meta.url), 'utf8');
  assert.match(html, /id="notify-types" hidden/, 'markup still starts hidden, so this render is what reveals it');
});
