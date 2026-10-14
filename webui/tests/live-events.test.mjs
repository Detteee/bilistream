import test from 'node:test';
import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import {
  configToForm, formToPatch, formatLocalShort, isSkipped, liveEventsFieldError, liveEventsGate, pickBudget,
  liveEventsSupported, pendingLiveNote, sameForm, selectState, withLiveEvents, senderOutcome, skippedRow, staleLocalFailure, wakeupBudgetView,
} from '../src/js/notify-state.js';
import { createNotifyState } from '../mock/notify-fixtures.mjs';

const LEDGER = ['last_admin_msg_id', 'last_admin_at', 'passive_replies_used'];
const OPENID = 'ADMINOPENID0123456789ABCDEF';
const base = { enabled: true, qq: { app_id: '1020304050' } };

test('switch: present key round-trips on/off; the old "missing reads as on" is gone', () => {
  const on = configToForm({ ...base, live_events_enabled: true });
  assert.equal(on.live_events_enabled, true);
  assert.deepEqual(formToPatch(on, on), {});
  assert.deepEqual(formToPatch({ ...on, live_events_enabled: false }, on), { live_events_enabled: false });
  const off = configToForm({ ...base, live_events_enabled: false });
  assert.equal(off.live_events_enabled, false);
  assert.deepEqual(formToPatch({ ...off, live_events_enabled: true }, off), { live_events_enabled: true });
  assert.ok(!('live_events_enabled' in configToForm(base)), 'missing key is not read as on');
});

test('legacy backend: no key → not supported, never dirty, never sent', () => {
  assert.equal(liveEventsSupported(base), false);
  assert.equal(liveEventsSupported({ ...base, live_events_enabled: false }), true);
  const baseline = configToForm(base);
  // readForm on a legacy backend: the switch (even if checked/unchecked in the DOM) is ignored.
  for (const checked of [true, false]) {
    const form = withLiveEvents({ ...baseline }, liveEventsSupported(base), checked);
    assert.ok(sameForm(form, baseline), 'not dirty');
    assert.deepEqual(formToPatch(form, baseline), {});
    const edited = formToPatch({ ...form, cooldown_mins: '45' }, baseline);
    assert.ok(!('live_events_enabled' in edited), 'PUT never carries the field');
    assert.deepEqual(edited, { cooldown_secs: 2700 });
  }
  // The key comes back on a later load: supported again, value read normally.
  const back = { ...base, live_events_enabled: false };
  const form = withLiveEvents(configToForm(back), liveEventsSupported(back), false);
  assert.equal(form.live_events_enabled, false);
  assert.ok(sameForm(form, configToForm(back)));
});

test('switch follows the writer gate and invalid_field shows inline', () => {
  assert.equal(liveEventsGate(null).enabled, true);
  assert.equal(liveEventsGate({ authority: 'primary' }).enabled, true, 'unknown writer → enabled');
  assert.equal(liveEventsGate({ openid_writer: { node_id: 'jp', is_local: true } }).enabled, true);
  const other = liveEventsGate({ openid_writer: { node_id: 'jp', is_local: false } });
  assert.equal(other.enabled, false);
  assert.match(other.hint, /jp/);
  const none = liveEventsGate({ openid_writer: null });
  assert.equal(none.enabled, false);
  assert.match(none.hint, /^暂无可写节点/);
  assert.ok(liveEventsFieldError({ body: { data: { code: 'invalid_field', field: 'live_events_enabled' } } }));
  assert.equal(liveEventsFieldError({ body: { data: { code: 'invalid_field', field: 'confirm_secs' } } }), '');
});

test('wakeup budget line: ok, warn at reserve, unknown when missing or stale', () => {
  const at = 1_790_000_000;
  const ok = wakeupBudgetView({ wakeup_remaining: 3, wakeup_reserve: 2, wakeup_next_close_at: at });
  assert.equal(ok.main, '唤醒额度 3/4 · 保留 2 用于故障提醒');
  assert.equal(ok.tone, 'ok');
  assert.equal(ok.next, ` · 下一档 ${formatLocalShort(at)} 到期`);
  assert.match(ok.next, /^ · 下一档 \d\d-\d\d \d\d:\d\d 到期$/);
  for (const remaining of [2, 1, 0]) {
    const warn = wakeupBudgetView({ wakeup_remaining: remaining, wakeup_reserve: 2 });
    assert.equal(warn.tone, 'warn');
    assert.equal(warn.main, `唤醒额度 ${remaining}/4 · 保留 2 用于故障提醒，开播/下播只用被动回复`);
  }
  assert.equal(wakeupBudgetView({ wakeup_remaining: 3, wakeup_reserve: 2, wakeup_next_close_at: null }).next, null, 'null hides the segment');
  assert.equal(wakeupBudgetView({ wakeup_remaining: 3, wakeup_reserve: 1 }).main.includes('保留 1'), true, 'reserve is never hard-coded');
  for (const view of [wakeupBudgetView({ wakeup_reserve: 2 }), wakeupBudgetView({ wakeup_remaining: 3 }),
    wakeupBudgetView({ wakeup_remaining: 3, wakeup_reserve: 2, wakeup_next_close_at: at, passive_window_open: true }, { available: false })]) {
    assert.deepEqual(view, { main: '唤醒额度 —', tone: 'muted', next: null, passive: false });
  }
});

test('passive window hint shows only when true and never warns', () => {
  assert.equal(wakeupBudgetView({ wakeup_remaining: 3, wakeup_reserve: 2, passive_window_open: true }).passive, true);
  const lowOpen = wakeupBudgetView({ wakeup_remaining: 1, wakeup_reserve: 2, passive_window_open: true });
  assert.equal(lowOpen.passive, true);
  assert.equal(lowOpen.tone, 'warn', 'the hint is independent of the budget tone');
  assert.equal(wakeupBudgetView({ wakeup_remaining: 3, wakeup_reserve: 2, passive_window_open: false }).passive, false);
  assert.equal(wakeupBudgetView({ wakeup_remaining: 3, wakeup_reserve: 2 }).passive, false);
  assert.equal(wakeupBudgetView({ wakeup_remaining: 3, wakeup_reserve: 2, passive_window_open: 'true' }).passive, false);
});

test('skipped_quota is held, not failed', () => {
  const full = { config: { ...base }, appsecret_set: true, openid_set: true };
  const st = { node_id: 'ny', authority: 'none', authority_node_id: 'jp', sender: { node: 'jp', last_send: { ok: false, status: 'skipped_quota', at: 1_790_000_000 } },
    local_last_send: { ok: false, status: 'skipped_quota', at: 1_790_000_000, node_id: 'ny' } };
  const out = senderOutcome(st);
  assert.ok(isSkipped(out));
  assert.notEqual(selectState({ settings: full, status: st }).key, 'failed', 'badge does not turn red');
  assert.equal(staleLocalFailure(st), null, 'no 本节点上次 failure row');
  assert.match(skippedRow(out, 'ny'), /^已跳过：额度不足 · \d\d-\d\d \d\d:\d\d（来自 jp）$/);
  assert.equal(skippedRow({ ok: true, at: 1 }, 'jp'), null);
});

test('ledger fields and the openid never reach the panel', async () => {
  const leaky = { wakeup_remaining: 3, wakeup_reserve: 2, wakeup_next_close_at: 1_790_000_000, passive_window_open: true,
    last_admin_msg_id: 'MSGID-LEAK', last_admin_at: 1_789_999_000, passive_replies_used: 3, admin_openid: OPENID, openid: OPENID };
  const picked = pickBudget({ quota: leaky }, leaky);
  assert.deepEqual(Object.keys(picked).sort(), ['passive_window_open', 'wakeup_next_close_at', 'wakeup_remaining', 'wakeup_reserve']);
  const st = { sender: { node: 'jp', last_send: { ok: false, status: 'skipped_quota', at: 1, ...leaky } } };
  const rendered = JSON.stringify([wakeupBudgetView(picked), skippedRow(senderOutcome(st), 'ny'), pendingLiveNote({ ...leaky, pending_live_events: ['live_end'] }, 'jp', 'ny'), senderOutcome(st), liveEventsGate(st)]);
  for (const bad of [OPENID, 'MSGID-LEAK', '1789999000', ...LEDGER]) assert.ok(!rendered.includes(bad), `${bad} leaked`);
  // Source and markup: the panel code never names the ledger fields, and the
  // §17 elements carry no openid.
  const src = await readFile(new URL('../src/js/notify.js', import.meta.url), 'utf8') + await readFile(new URL('../src/js/notify-state.js', import.meta.url), 'utf8');
  const code = src.replace(/\/\/.*$/gm, '');
  for (const key of LEDGER) assert.ok(!code.includes(key), `${key} referenced in panel code`);
  const html = await readFile(new URL('../src/index.html', import.meta.url), 'utf8');
  const group = html.slice(html.indexOf('id="notify-types"'), html.indexOf('notify-commands-section'));
  assert.ok(group.includes('id="notify-live-events"') && group.includes('id="notify-wakeup-budget"') && group.includes('id="notify-passive-hint"'));
  assert.ok(!/openid/i.test(group), 'no openid in the 通知类型 group');
  assert.ok(!/style=|innerHTML/.test(group));
});

test('mock serves the rev 2 fields, never the ledger or openid', () => {
  const notify = createNotifyState(() => 1_790_000_000);
  for (const mode of ['ok', 'low', 'pending-after-alert', 'unknown', 'legacy']) {
    assert.ok(notify.setMode(mode));
    const body = JSON.stringify([notify.route('GET', '/api/notify'), notify.route('GET', '/api/notify/status')]);
    for (const key of [...LEDGER, 'admin_openid', '"openid"']) assert.ok(!body.includes(key), `${mode}: ${key}`);
  }
  notify.setMode('low');
  const [, st] = notify.route('GET', '/api/notify/status');
  assert.equal(st.data.sender.last_send.status, 'skipped_quota');
  assert.equal(wakeupBudgetView(pickBudget(st.data)).tone, 'warn');
  notify.setMode('ok');
  const [, ok] = notify.route('GET', '/api/notify/status');
  assert.equal(wakeupBudgetView(pickBudget(ok.data)).passive, true);
  const [code, saved] = notify.route('PUT', '/api/notify', { config: { live_events_enabled: false }, expected_revision: 1 });
  assert.equal(code, 200);
  assert.equal(saved.data.config.live_events_enabled, false);
  const [bad, refused] = notify.route('PUT', '/api/notify', { config: { live_events_enabled: 'no' } });
  assert.equal(bad, 400);
  assert.ok(liveEventsFieldError({ body: refused }));
  assert.equal(notify.setMode('nope'), false);
});

test('待补发 follows pending_live_events alone (Addendum D)', () => {
  const note = '通知待补发，管理员下次给机器人发消息时一并送达。';
  assert.equal(pendingLiveNote({ pending_live_events: ['live_end'] }, 'jp', 'jp'), `下播${note}`);
  assert.equal(pendingLiveNote({ pending_live_events: ['live_end', 'live_start'] }, 'jp', 'jp'), `开播/下播${note}`);
  assert.equal(pendingLiveNote({ pending_live_events: ['live_start'] }, 'jp', 'ny'), `开播${note}（来自 jp）`);
  // skipped_quota with nothing pending → no note; missing field → nothing.
  const skippedOnly = { sender: { node: 'jp', last_send: { ok: false, status: 'skipped_quota', at: 1 } }, pending_live_events: [] };
  assert.equal(pendingLiveNote(skippedOnly, 'jp', 'jp'), '');
  assert.equal(pendingLiveNote({ sender: skippedOnly.sender }, 'jp', 'jp'), '');
  assert.equal(pendingLiveNote(null), '');
  assert.equal(pendingLiveNote({ pending_live_events: ['bogus'] }), '');
  // A later successful send does not clear it.
  const afterAlert = { sender: { node: 'jp', last_send: { ok: true, via: 'wakeup', at: 2 } }, pending_live_events: ['live_end'] };
  assert.equal(pendingLiveNote(afterAlert, 'jp', 'jp'), `下播${note}`);
});

test('the old 召回额度 row is gone; the budget line is the only wakeup count', async () => {
  const src = (await readFile(new URL('../src/js/notify.js', import.meta.url), 'utf8')).replace(/\/\/.*$/gm, '');
  assert.ok(!src.includes("row('召回额度'"));
  assert.ok(!src.includes('wakeups_left'));
});

test('mock pending-after-alert: wakeup alert succeeded, 下播 still pending', () => {
  const notify = createNotifyState(() => 1_790_000_000);
  assert.ok(notify.setMode('pending-after-alert'));
  const [, st] = notify.route('GET', '/api/notify/status');
  const [, cfg] = notify.route('GET', '/api/notify');
  assert.deepEqual(st.data.sender.last_send.ok, true);
  assert.equal(st.data.sender.last_send.via, 'wakeup');
  assert.deepEqual(cfg.data.pending_live_events, ['live_end']);
  assert.ok(pendingLiveNote(st.data, 'jp', 'jp').startsWith('下播通知待补发'));
  notify.setMode('ok');
  assert.deepEqual(notify.route('GET', '/api/notify')[1].data.pending_live_events, []);
});

test('mock legacy: no live_events_enabled and none of the new fields', () => {
  const notify = createNotifyState(() => 1_790_000_000);
  assert.ok(notify.setMode('legacy'));
  const cfg = notify.route('GET', '/api/notify')[1].data;
  const st = notify.route('GET', '/api/notify/status')[1].data;
  assert.equal(liveEventsSupported(cfg.config), false);
  for (const key of ['live_events_enabled', 'pending_live_events', 'wakeup_remaining', 'wakeup_reserve', 'wakeup_next_close_at', 'passive_window_open']) {
    assert.ok(!JSON.stringify([cfg, st]).includes(key), key);
  }
  notify.setMode('ok');
  assert.equal(liveEventsSupported(notify.route('GET', '/api/notify')[1].data.config), true);
});
