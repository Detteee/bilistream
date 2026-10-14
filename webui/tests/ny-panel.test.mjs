import test from 'node:test';
import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import {
  configToForm, credentialRows, openidGate, openidPending, openidView, readinessRow, selectState, validateForm, writerOpenidSet,
} from '../src/js/notify-state.js';
import { createNotifyState, NY_MODES } from '../mock/notify-fixtures.mjs';

const OPENID = '0123456789ABCDEF0123456789ABCDEF';
const load = mode => {
  const m = createNotifyState(() => 1_790_000_000);
  assert.ok(m.setMode(mode));
  return { settings: m.route('GET', '/api/notify')[1].data, status: m.route('GET', '/api/notify/status')[1].data };
};

test('the user\'s case: ny pending while jp is bound — no 未绑定, no 请在 jp 上绑定', () => {
  const { settings, status } = load('ny-pending');
  const v = openidView(settings, status);
  assert.equal(v.badge, '已在 jp 绑定 · 待同步');
  assert.equal(v.tone, '', 'neutral, not warn');
  assert.equal(v.pending, true);
  assert.equal(v.hint, 'openid 在发送节点 jp 上绑定，会自动同步到本节点。');
  assert.ok(!v.hint.includes('请在'), 'the sync hint replaces the owner hint');
  assert.match(openidGate(status).hint, /请在 jp/, 'the gate hint still exists, but the panel swaps it while pending');
  const st = selectState({ settings, status });
  assert.ok(!st.missing.includes('管理员 openid'), '还缺少 omits openid while pending');
  const form = configToForm(settings.config);
  assert.equal(validateForm(form, { secretConfigured: true, openidPending: true }).warnings.admin_openid, undefined, 'no red 启用通知前请绑定 line');
  assert.ok(validateForm(form, { secretConfigured: true }).warnings.admin_openid, 'still warns when nothing is pending');
  const ny = credentialRows(status, { writerSet: true }).find(r => r.node_id === 'ny');
  assert.deepEqual(ny.openid, { text: 'openid 待同步', tone: '' });
  assert.deepEqual(readinessRow(status), { count: '2 / 3', gaps: ['ny 缺 openid（待同步）'], warn: false }, 'F rev 3: pending gaps are neutral');
});

test('bound ny: 已绑定, no pending hint, all nodes ready', () => {
  const { settings, status } = load('ny-bound');
  assert.deepEqual(openidView(settings, status), { badge: '已绑定', tone: 'active', pending: false, hint: null });
  assert.deepEqual(readinessRow(status), { count: '3 / 3', gaps: [], warn: false });
});

test('gap text: appsecret, stale, joins and unknown codes', () => {
  assert.deepEqual(readinessRow(load('ny-appsecret').status).gaps, ['ca 缺 AppSecret']);
  assert.deepEqual(readinessRow(load('ny-stale').status).gaps, ['ca 心跳超时']);
  const mixed = { nodes: [
    { node_id: 'jp', missing: [] },
    { node_id: 'ny', missing: ['appid', 'appsecret', 'openid'] },
    { node_id: 'ca', missing: ['openid_pending'] },
    { node_id: 'sg', missing: ['weird', 'other'] },
  ] };
  const r = readinessRow(mixed);
  assert.equal(r.count, '1 / 4');
  assert.deepEqual(r.gaps, ['ny 缺 AppID、缺 AppSecret、缺 openid', 'ca 缺 openid（待同步）', 'sg 配置不完整']);
  assert.equal(r.gaps.join('；'), 'ny 缺 AppID、缺 AppSecret、缺 openid；ca 缺 openid（待同步）；sg 配置不完整');
  assert.equal(readinessRow({ nodes: [{ node_id: 'a' }] }), null, 'no missing arrays → no row');
});

test('old backend / no writer info keeps today\'s 未绑定 behaviour', () => {
  const { settings, status } = load('ny-old');
  assert.equal(writerOpenidSet(settings, status), undefined);
  assert.equal(readinessRow(status), null);
  const unbound = { ...settings, openid_set: false };
  assert.deepEqual(openidView(unbound, status), { badge: '未绑定', tone: 'sync-warning', pending: false, hint: null });
  assert.equal(openidPending(unbound, { writer_openid_set: false }), false);
  // /status wins over GET when both carry the key.
  assert.equal(writerOpenidSet({ writer_openid_set: true }, { writer_openid_set: false }), false);
  assert.equal(writerOpenidSet({ writer_openid_set: true }, {}), true);
});

test('no openid or secret ever reaches the ny view models or mocks', async () => {
  for (const mode of NY_MODES) {
    const { settings, status } = load(mode);
    const leaky = { ...settings, admin_openid: OPENID, openid: OPENID };
    const st = { ...status, nodes: (status.nodes || []).map(n => ({ ...n, openid: OPENID, app_secret: 'S3CRET' })) };
    const out = JSON.stringify([openidView(leaky, st), readinessRow(st), credentialRows(st, { writerSet: true }), selectState({ settings: leaky, status: st })]);
    assert.ok(!out.includes(OPENID) && !out.includes('S3CRET'), mode);
    const body = JSON.stringify([settings, status]);
    assert.ok(!/"admin_openid"|"openid"\s*:|app_secret"/.test(body), `${mode} mock`);
  }
  const src = await readFile(new URL('../src/js/notify.js', import.meta.url), 'utf8');
  assert.match(src, /settings\.openid_set \|\| openidPending\(settings, status\)\) return '';/, 'no 正在所有节点解除绑定 summary while pending');
  assert.match(src, /for \(const id of \['notify-bind-btn', 'notify-bind-any-row'\]\) \{ const el = \$\(id\); if \(el\) el\.hidden = ov\.pending; \}/, '自动获取 hides while pending');
});

// rev 2: handoff — every writer-name source null/missing/blank.
import { liveEventsGate, noWriterReason, writerName, pendingLiveNote } from '../src/js/notify-state.js';

const BAD = [/null/, /undefined/, /已在  /, /在\s+(上|绑定)/, /已在 ·/, /由\s+发送/, /请在\s+上/, /\s{2,}/];
function panelTexts(settings, status) {
  const v = openidView(settings, status);
  const gate = openidGate(status);
  const r = readinessRow(status);
  return [v.badge, v.hint, gate.hint, liveEventsGate(status).hint, noWriterReason(status),
    ...(r ? [r.count, ...r.gaps] : []),
    ...credentialRows(status, { writerSet: writerOpenidSet(settings, status) }).flatMap(c => [c.node_id, c.secret?.text, c.openid?.text]),
    ...(selectState({ settings, status }).missing || []),
    pendingLiveNote({ pending_live_events: ['live_end'] }, status?.sender?.node || '', status?.node_id || ''),
  ].filter(t => t != null).map(String);
}

test('rev 2: ny-handoff (all writer sources null) falls back to 发送节点', () => {
  const { settings, status } = load('ny-handoff');
  assert.equal(status.openid_writer, null);
  assert.equal(status.sender, null);
  assert.equal(status.authority_node_id, null);
  assert.equal(writerName(status), '');
  const v = openidView(settings, status);
  assert.equal(v.badge, '已在发送节点绑定 · 待同步');
  assert.equal(v.hint, 'openid 在发送节点上绑定，会自动同步到本节点。');
});

test('rev 2: every ny state and blank writers render without null/undefined/empty name slots', () => {
  const cases = NY_MODES.map(mode => [mode, load(mode)]);
  const pend = load('ny-pending');
  for (const blank of ['', '   ', null, undefined]) {
    const status = { ...pend.status, openid_writer: { node_id: blank, is_local: false }, sender: { node: blank, last_send: null }, authority_node_id: blank };
    cases.push([`blank writer ${JSON.stringify(blank)}`, { settings: pend.settings, status }]);
  }
  cases.push(['writer empty only', { settings: pend.settings, status: { ...pend.status, openid_writer: { node_id: '', is_local: false } } }]);
  for (const [name, { settings, status }] of cases) {
    for (const text of panelTexts(settings, status)) {
      for (const bad of BAD) assert.ok(!bad.test(text), `${name}: ${JSON.stringify(text)} matches ${bad}`);
    }
  }
  // An empty openid_writer alone still finds the next source.
  assert.equal(writerName({ openid_writer: { node_id: '' }, sender: { node: 'jp' } }), 'jp');
  assert.equal(writerName({ openid_writer: { node_id: '  ' }, authority_node_id: ' ca ' }), 'ca');
  assert.equal(openidGate({ openid_writer: { node_id: '   ', is_local: false } }).hint.startsWith('暂无可写节点'), true);
});

// rev 3 (§16.x B1): a pending openid is not missing.
test('rev 3: pending openid alone → 待命, no 未就绪 / 缺少 openid / 自动获取 banner', () => {
  const { settings, status } = load('ny-pending');
  const st = selectState({ settings: { ...settings, config: { ...settings.config, enabled: true } }, status });
  assert.equal(st.key, 'standby');
  assert.equal(st.badge, '待命');
  assert.ok(!st.lacks);
  const text = JSON.stringify(st);
  for (const bad of ['本节点未就绪', '管理员 openid', '自动获取']) assert.ok(!text.includes(bad), bad);
});

test('rev 3: pending + missing AppSecret → 未就绪 naming only AppSecret', () => {
  const { settings, status } = load('ny-pending-appsecret');
  const st = selectState({ settings: { ...settings, appsecret_set: true }, status });   // GET says set elsewhere; node row decides
  assert.equal(st.key, 'node-unready');
  assert.deepEqual(st.lacks, ['AppSecret']);
  const appid = { ...status, nodes: status.nodes.map(n => n.is_local ? { ...n, appsecret_set: true, appid_set: false, missing: ['appid', 'openid_pending'] } : n) };
  assert.deepEqual(selectState({ settings: { ...settings, appsecret_set: true }, status: appid }).lacks, ['AppID']);
});

test('rev 3: a plain missing openid (not pending) still counts as before', () => {
  const { settings, status } = load('ny-pending');
  const plain = { ...status, writer_openid_set: false };
  // GET says unbound and nothing is pending → 还缺少 管理员 openid (state 2, unchanged).
  const st = selectState({ settings: { ...settings, writer_openid_set: false }, status: plain });
  assert.equal(st.key, 'incomplete');
  assert.ok(st.missing.includes('管理员 openid'));
  // GET says bound but this node's row lacks it, not pending → 本节点未就绪 with openid (2n, unchanged).
  const rowOnly = selectState({ settings: { ...settings, openid_set: true, writer_openid_set: false }, status: plain });
  assert.equal(rowOnly.key, 'node-unready');
  assert.equal(rowOnly.badge, '本节点未就绪');
  assert.deepEqual(rowOnly.lacks, ['管理员 openid']);
});
