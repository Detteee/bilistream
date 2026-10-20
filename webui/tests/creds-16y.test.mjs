import test from 'node:test';
import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import {
  appIdEmpty, appIdSet, baseGeneration, credentialPending, credentialRows, notifyRefusal, onlyPendingCodes,
  openidView, pendingCredentialBadge, readinessRow, rebindHint, selectState,
  APPID_EMPTY, APPSECRET_HELP_SYNC, APPID_HELP_SYNC, OUTCOME_UNKNOWN_TEXT,
} from '../src/js/notify-state.js';
import { createNotifyState, CRED_MODES } from '../mock/notify-fixtures.mjs';

const load = mode => {
  const m = createNotifyState(() => 1_790_000_000);
  assert.ok(m.setMode(mode));
  return { m, settings: m.route('GET', '/api/notify')[1].data, status: m.route('GET', '/api/notify/status')[1].data };
};
const refusal = (status, code) => ({ status, body: { success: false, data: code ? { code } : {} } });

test('base_generation: the generation from GET is always sent', () => {
  const { settings } = load('cred-ok');
  assert.equal(baseGeneration(settings), 7);
  assert.equal(baseGeneration({ ...settings, credentials_generation: 0 }), 0, 'generation 0 is still sent');
});

test('appid_pending / appsecret_pending: neutral 待同步 badge, 待命, no 未配置 / 未就绪', () => {
  for (const [mode, key, gap] of [['cred-appid-pending', 'appid', 'ny 缺 AppID（待同步）'], ['cred-appsecret-pending', 'appsecret', 'ny 缺 AppSecret（待同步）']]) {
    const { settings, status } = load(mode);
    assert.ok(credentialPending(settings, status, key));
    assert.deepEqual(pendingCredentialBadge(settings, status, key), { badge: '已在 jp 保存 · 待同步', tone: '' });
    const st = selectState({ settings, status });
    assert.equal(st.key, 'standby', mode);
    assert.deepEqual(st.missing, []);
    assert.deepEqual(readinessRow(status).gaps, [gap]);
    const ny = credentialRows(status, { writerSet: true }).find(r => r.node_id === 'ny');
    const badge = key === 'appid' ? ny.appid : ny.secret;
    assert.deepEqual(badge, { text: key === 'appid' ? 'AppID 待同步' : 'AppSecret 待同步', tone: '' });
  }
  // No writer name: never null/undefined/blank.
  const { settings, status } = load('cred-appid-pending');
  const anon = { ...status, openid_writer: null, sender: null, authority_node_id: null };
  assert.deepEqual(pendingCredentialBadge(settings, anon, 'appid'), { badge: '已在其他节点保存 · 待同步', tone: '' });
});

test('joining node (rule 9): all three 待同步, normal 待命, not counted ready', () => {
  const { settings, status } = load('cred-joining');
  const st = selectState({ settings, status });
  assert.equal(st.key, 'standby');
  assert.equal(st.badge, '待命');
  assert.deepEqual(st.missing, []);
  assert.equal(openidView(settings, status).pending, true);
  assert.equal(readinessRow(status).count, '2 / 3', 'ny is not counted ready');
  const ny = credentialRows(status, { writerSet: true }).find(r => r.node_id === 'ny');
  assert.equal(ny.appid.text, 'AppID 待同步');
  assert.equal(ny.secret.text, 'AppSecret 待同步');
  assert.equal(ny.openid.text, 'openid 待同步');
  assert.ok(onlyPendingCodes(status.nodes.find(n => n.is_local)));
});

test('ny-pending (§16.x) still shows 待命', () => {
  const { settings, status } = load('cred-ok');
  void settings;
  const m = createNotifyState(() => 1_790_000_000);
  m.setMode('ny-pending');
  const s2 = m.route('GET', '/api/notify')[1].data, st2 = m.route('GET', '/api/notify/status')[1].data;
  const st = selectState({ settings: s2, status: st2 });
  assert.equal(st.key, 'standby');
  assert.equal(st.badge, '待命');
  assert.ok(status.nodes.every(n => n.missing.length === 0));
});

test('openid_rebind: 需重新绑定 warn, hint names the sender, counts as 本节点未就绪, bind controls shown', () => {
  const { settings, status } = load('cred-rebind');
  const v = openidView(settings, status);
  assert.equal(v.badge, '需重新绑定');
  assert.equal(v.tone, 'sync-warning');
  assert.equal(v.pending, false, 'not pending: bind controls are shown');
  assert.equal(v.hint, 'AppID 已更换（当前 …4050），原管理员 openid 已失效。请在发送节点 jp 上重新「自动获取」。');
  assert.equal(rebindHint({}), 'AppID 已更换，原管理员 openid 已失效。请在发送节点上重新「自动获取」。');
  const st = selectState({ settings, status });
  assert.equal(st.key, 'node-unready');
  assert.equal(st.badge, '本节点未就绪');
  assert.deepEqual(st.lacks, ['管理员 openid（需重新绑定）']);
  assert.ok(!st.missing.includes('管理员 openid'), 'not 未配置');
  assert.ok(readinessRow(status).gaps.includes('ny 需重新绑定 openid'));
  assert.equal(readinessRow(status).warn, true);
  assert.deepEqual(credentialRows(status).find(r => r.node_id === 'ny').openid, { text: 'openid 需重新绑定', tone: 'sync-warning' });
});

test('C1: while pending, notify_capable:false counts only when a non-pending code is present', () => {
  const { settings, status } = load('cred-appsecret-pending');
  const withCode = code => ({ ...status, nodes: status.nodes.map(n => n.is_local ? { ...n, missing: ['appsecret_pending', code] } : n) });
  const st = selectState({ settings, status: withCode('stale') });
  assert.equal(st.key, 'node-unready', 'a non-pending code with notify_capable:false → 未就绪');
  // Only pending codes (capable:false is expected while syncing): still 待命.
  assert.equal(selectState({ settings, status }).key, 'standby');
  // openid_set false is never checked while the openid is pending (no 未就绪 regression).
  const ny = load('ny-pending');
  assert.equal(selectState(ny).key, 'standby');
});

test('N3: 凭据同步 marks a silent node 离线', () => {
  const { status } = load('cred-offline');
  const rows = credentialRows(status, { writerSet: true });
  assert.equal(rows.find(r => r.node_id === 'ca').offline, true);
  assert.equal(rows.find(r => r.node_id === 'jp').offline, false);
  assert.equal(credentialRows({ nodes: [{ node_id: 'x', appsecret_set: true, openid_set: true, fresh: false }] })[0].offline, true);
});

test('409 credentials_conflict and 400 base_generation_required: same copy and 刷新; 503 holder down in warn', () => {
  for (const [code, status] of [['credentials_conflict', 409], ['base_generation_required', 400]]) {
    const r = notifyRefusal(refusal(status, code), null, { route: 'config' });
    assert.equal(r.kind, 'credentials_conflict');
    assert.equal(r.banner.lead, '配置已在其他节点更新，请刷新后重试。');
    assert.equal(r.banner.tone, 'warn');
    assert.deepEqual(r.banner.action, { label: '刷新', target: 'refresh' });
  }
  // Forward-fix copy (spec rev 4): the coded 503 with outcome not_sent names
  // the node (发送节点 when it isn't in the node list). Rev 9 L1: an uncoded
  // 503 takes the unknown ending with 刷新 instead of claiming nothing was
  // saved. Unknown outcomes are covered in notify-forward-codes.
  const coded = notifyRefusal({ status: 503, body: { success: false, data: { code: 'credentials_holder_unreachable', outcome: 'not_sent' } } }, null, { route: 'config' });
  assert.equal(coded.kind, 'holder_down');
  assert.equal(coded.banner.lead, '发送节点暂时无法连接，未保存任何更改。');
  assert.equal(coded.banner.text, '请稍后重试。');
  assert.equal(coded.banner.tone, 'warn');
  assert.ok(!coded.banner.action, 'no retry button, no auto retry');
  const uncoded = notifyRefusal(refusal(503, null), null, { route: 'config' });
  assert.equal(uncoded.kind, 'holder_unknown');
  assert.equal(uncoded.outcome, 'unknown');
  assert.equal(uncoded.banner.lead, '发送节点暂时无法连接，保存结果未知。');
  assert.equal(uncoded.banner.text, OUTCOME_UNKNOWN_TEXT);
  assert.equal(uncoded.banner.tone, 'warn');
  assert.deepEqual(uncoded.banner.action, { label: '刷新', target: 'refresh' });
});

test('empty AppID (rev 3): only when no AppID is saved; whitespace = blank when one is', () => {
  const { settings } = load('cred-ok');
  assert.equal(appIdEmpty(settings, '   ', { secretEntered: true, enabled: true }), false, 'appid_set: whitespace is blank, no error');
  assert.equal(appIdEmpty(settings, '', { enabled: true }), false);
  assert.equal(appIdEmpty(settings, '1234567'), false);
  const none = { ...settings, appid_set: false, config: { ...settings.config, qq: { ...settings.config.qq, appid_set: false, app_id_hint: null } } };
  assert.equal(appIdEmpty(none, '', { secretEntered: true }), true);
  assert.equal(appIdEmpty(none, '   ', { enabled: true }), true, 'whitespace counts as blank');
  assert.equal(appIdEmpty(none, '   '), false, 'same save conditions as before');
  assert.equal(appIdEmpty(none, '7654321', { enabled: true }), false);
  const pend = load('cred-appid-pending');
  assert.equal(appIdEmpty(pend.settings, '', { enabled: true, status: pend.status }), false, 'pending is not missing');
  assert.equal(APPID_EMPTY, 'AppID 不能为空');
  // Whitespace with an AppID saved: the PUT leaves app_id out.
  const base = { ...configToForm(settings.config), app_id: '' };
  const patch = formToPatch({ ...base, app_id: '   ' }, base);
  assert.ok(!patch.qq || !('app_id' in patch.qq));
});

test('mock F save: base_generation required, stale → 409, codes for each refusal, all-or-nothing', () => {
  const { m, settings } = load('cred-ok');
  assert.equal(m.route('PUT', '/api/notify', { expected_revision: settings.revision, config: {} })[1].data.code, 'base_generation_required');
  assert.equal(m.route('PUT', '/api/notify', { expected_revision: settings.revision, base_generation: 3, config: {} })[0], 409);
  const ok = m.route('PUT', '/api/notify', { expected_revision: settings.revision, base_generation: 7, app_secret: 'x'.repeat(32), config: {} });
  assert.equal(ok[0], 200);
  assert.equal(ok[1].data.credentials_generation, 8);
  assert.deepEqual(m.lastPut().keys, ['app_secret', 'base_generation', 'config', 'expected_revision']);
  for (const [mode, code, status] of [['cred-conflict', 'credentials_conflict', 409], ['cred-basegen', 'base_generation_required', 400], ['cred-holder-down', 'credentials_holder_unreachable', 503]]) {
    const x = load(mode);
    const before = JSON.stringify(x.m.route('GET', '/api/notify')[1].data);
    const res = x.m.route('PUT', '/api/notify', { expected_revision: x.settings.revision, base_generation: 7, config: { enabled: false } });
    assert.equal(res[0], status);
    assert.equal(res[1].data.code, code);
    assert.equal(JSON.stringify(x.m.route('GET', '/api/notify')[1].data), before, `${mode}: nothing applied`);
  }
});

test('leak audit: F responses carry flags only, never a credential value', () => {
  for (const mode of CRED_MODES) {
    const { settings, status } = load(mode);
    const text = JSON.stringify([settings, status]);
    assert.ok(!text.includes('1020304050'), `${mode}: AppID value`);
    assert.ok(!/app_secret"|admin_openid|"openid"\s*:/.test(text), `${mode}: secret/openid keys`);
    assert.ok(!('app_id' in settings.config.qq), `${mode}: config.qq.app_id`);
    assert.equal(typeof settings.appid_set, 'boolean');
    assert.ok(appIdSet(settings) === settings.appid_set);
  }
});

test('panel wiring: base_generation on every save, AppSecret cleared after every request, AppID write-only, copy', async () => {
  const js = await readFile(new URL('../src/js/notify.js', import.meta.url), 'utf8');
  const html = await readFile(new URL('../src/index.html', import.meta.url), 'utf8');
  assert.match(js, /const patch = formToPatch\(form, baseline\);/);
  assert.match(js, /const body = saveBody\(settings, patch, \{ secret, openid \}\);/);
  assert.match(js, /finally \{[\s\S]{0,200}\$\('notify-app-secret'\)\.value = '';/);
  assert.match(js, /baseline\.app_id = '';/);
  assert.match(js, /action\.target === 'refresh'\) \{ clearRefusal\(\); loadNotifySettings\(\); return; \}/);
  assert.ok(!/innerHTML|\.style\./.test(js));
  assert.match(html, /aria-describedby="notify-app-id-current notify-app-id-error notify-app-id-help"/);
  assert.match(html, /id="notify-app-id-state" role="status" hidden/);
  assert.equal(APPSECRET_HELP_SYNC, 'AppSecret 加密保存，不会显示。保存后会同步到所有节点。');
  assert.equal(APPID_HELP_SYNC, '保存后会同步到所有节点；更换 AppID 需要重新绑定管理员 openid。');
  const css = await readFile(new URL('../src/styles.css', import.meta.url), 'utf8');
  assert.match(css, /\.settings-help-text\.is-invalid\[data-tone="warn"\] \{ color: var\(--warn\); \}/);
  assert.match(css, /li\.is-offline \.notify-cred-node \{ color: var\(--text-mute\); \}/);
});

// ---- F rev 3 (rules 16, 17) ----
import { appIdHint, appIdLine, configToForm, formToPatch } from '../src/js/notify-state.js';

test('F rev 3: appid_set from config.qq and app_id_hint last 4 only', () => {
  const { settings } = load('cred-ok');
  assert.equal(settings.config.qq.appid_set, true);
  assert.equal(settings.config.qq.app_id_hint, '4050');
  assert.equal(appIdHint(settings), '4050');
  const q = qq => ({ config: { qq } });
  assert.equal(appIdHint(q({ app_id_hint: null, appid_set: false })), null);
  assert.equal(appIdSet(q({ app_id_hint: null, appid_set: false })), false);
  assert.equal(appIdHint(q({ app_id_hint: '123456789' })), '6789', 'sliced defensively');
  assert.equal(appIdHint(q({ app_id_hint: 42 })), null, 'non-string → null');
  assert.equal(appIdHint(q({ app_id: '1020304050' })), null, 'app_id is never read');
  assert.equal(appIdSet(q({ app_id: '1020304050' })), false, 'app_id is never read');
  assert.equal(appIdHint(load('cred-appid-pending').settings), null);
});

test('F rev 4: AppID line uses appid_set even when a short AppID has no hint', () => {
  const q = qq => ({ config: { qq } });
  assert.deepEqual(appIdLine(q({ appid_set: true, app_id_hint: '4050' })), { text: '…4050', mono: true });
  const short = appIdLine(q({ appid_set: true, app_id_hint: null }));
  assert.deepEqual(short, { text: '已设置', mono: false });
  assert.doesNotMatch(short.text, /\d/, 'no digits are guessed for a short AppID');
  assert.deepEqual(appIdLine(q({ appid_set: false, app_id_hint: null })), { text: '未设置', mono: false });
  assert.deepEqual(appIdLine(q({ appid_set: false, app_id_hint: '4050' })), { text: '未设置', mono: false }, 'the flag wins over a stale hint');
  assert.deepEqual(appIdLine(q({ appid_set: true, app_id_hint: '123456789' })), { text: '…6789', mono: true });
});

test('F rev 3: rebind hint shows …XXXX when known, current text otherwise', () => {
  const { settings, status } = load('cred-rebind');
  assert.equal(rebindHint(status, settings), 'AppID 已更换（当前 …4050），原管理员 openid 已失效。请在发送节点 jp 上重新「自动获取」。');
  assert.equal(rebindHint(status, null), 'AppID 已更换，原管理员 openid 已失效。请在发送节点 jp 上重新「自动获取」。');
  assert.equal(rebindHint({}, { config: { qq: { app_id_hint: null } } }), 'AppID 已更换，原管理员 openid 已失效。请在发送节点上重新「自动获取」。');
});

test('F rev 3: a blank AppID field omits app_id from the PUT; "" is never sent', () => {
  const { settings } = load('cred-ok');
  const base = { ...configToForm(settings.config), app_id: '' };
  assert.ok(!('qq' in formToPatch({ ...base }, base)) || !('app_id' in formToPatch({ ...base }, base).qq));
  assert.equal(formToPatch({ ...base, app_id: '7654321' }, base).qq.app_id, '7654321');
  // Older backend: the field is still write-only, blank stays omitted.
  const old = load('ok').settings;
  const ob = { ...configToForm(old.config), app_id: '' };
  assert.ok(!formToPatch({ ...ob, enabled: false }, ob).qq);
});

test('F rev 3: 可发送节点 gaps of only *_pending codes are neutral; others warn', () => {
  assert.equal(readinessRow(load('cred-joining').status).warn, false);
  assert.equal(readinessRow(load('cred-appid-pending').status).warn, false);
  assert.equal(readinessRow(load('cred-rebind').status).warn, true);
  assert.equal(readinessRow(load('cred-offline').status).warn, true);
});

test('F rev 3: every node openid_rebind → 本节点未就绪 + rebind, never 无法发送', () => {
  const { settings, status } = load('cred-rebind');
  assert.ok(status.nodes.every(n => n.missing.includes('openid_rebind')));
  const st = selectState({ settings, status });
  assert.equal(st.key, 'node-unready');
  assert.notEqual(st.key, 'no-sender');
  assert.deepEqual(st.lacks, ['管理员 openid（需重新绑定）']);
});

test('F rev 3 rule 17: credentials_holder_moved → same conflict text and 刷新', () => {
  const r = notifyRefusal({ status: 409, body: { data: { code: 'credentials_holder_moved' } } }, null, { route: 'config' });
  assert.equal(r.banner.lead, '配置已在其他节点更新，请刷新后重试。');
  assert.deepEqual(r.banner.action, { label: '刷新', target: 'refresh' });
});

test('F rev 3: 当前 AppID markup and class-based monospace (no inline style)', async () => {
  const html = await readFile(new URL('../src/index.html', import.meta.url), 'utf8');
  const js = await readFile(new URL('../src/js/notify.js', import.meta.url), 'utf8');
  const css = await readFile(new URL('../src/styles.css', import.meta.url), 'utf8');
  assert.match(html, /id="notify-app-id-current" hidden/);
  assert.match(js, /tail\.textContent = line\.text;/);
  assert.match(css, /\.notify-appid-tail \{ font-family: var\(--font-mono\); \}/);
  assert.ok(!/\.style\.|innerHTML/.test(js));
});

test('F rev 3: the 可发送节点 row only takes the warn tone when readinessRow says warn', async () => {
  const js = await readFile(new URL('../src/js/notify.js', import.meta.url), 'utf8');
  const css = await readFile(new URL('../src/styles.css', import.meta.url), 'utf8');
  assert.match(js, /if \(r\.warn\) v\.dataset\.tone = 'warn';/);
  assert.match(css, /\.notify-gaps\[data-tone="warn"\] \{ color: var\(--warn\); \}/);
  assert.ok(!/\.notify-settings \.notify-gaps \{ color: var\(--warn\); \}/.test(css));
});

test('rev 3: the 当前 AppID line is hidden while the node has appid_pending', async () => {
  const js = await readFile(new URL('../src/js/notify.js', import.meta.url), 'utf8');
  assert.match(js, /current\.hidden = credentialPending\(settings, status, 'appid'\);/);
  const p = load('cred-appid-pending');
  assert.ok(credentialPending(p.settings, p.status, 'appid'));
  const ok = load('cred-ok');
  assert.ok(!credentialPending(ok.settings, ok.status, 'appid'));
});

import { saveBody, clearSecretBody, pendingSettingsForm } from '../src/js/notify-state.js';

test('clear AppSecret PUT always carries base_generation and the same refusal path', async () => {
  const f = load('cred-ok').settings;
  assert.deepEqual(clearSecretBody(f), { clear_app_secret: true, expected_secret_revision: f.secret_revision, base_generation: 7 });
  assert.equal(clearSecretBody(load('ok').settings).base_generation, 7);
  const js = await readFile(new URL('../src/js/notify.js', import.meta.url), 'utf8');
  const clear = js.slice(js.indexOf('async function clearSecret'), js.indexOf('function handleRefusal'));
  assert.match(clear, /putJson\('\/api\/notify', clearSecretBody\(settings\)\)/);
  assert.match(clear, /handleRefusal\(error, 'config'\)/, 'same handling as saves (409 / 400 / 503)');
  for (const [code, status] of [['credentials_conflict', 409], ['base_generation_required', 400], ['credentials_holder_unreachable', 503]]) {
    assert.ok(notifyRefusal({ status, body: { data: { code } } }, null, { route: 'config' }), code);
  }
});

test('AppID + AppSecret change: exactly one PUT with config.qq.app_id, top-level app_secret and base_generation', async () => {
  const { m, settings } = load('cred-ok');
  const base = { ...configToForm(settings.config), app_id: '' };
  const body = saveBody(settings, formToPatch({ ...base, app_id: '7654321' }, base), { secret: 'S'.repeat(32) });
  assert.equal(body.config.qq.app_id, '7654321');
  assert.equal(body.app_secret, 'S'.repeat(32));
  assert.equal(body.base_generation, 7);
  assert.ok(!('app_secret' in body.config) && !('base_generation' in body.config));
  const js = await readFile(new URL('../src/js/notify.js', import.meta.url), 'utf8');
  const save = js.slice(js.indexOf('async function save()'), js.indexOf('async function clearSecret'));
  assert.equal((save.match(/putJson\(/g) || []).length, 1, 'one PUT per save');
  const res = m.route('PUT', '/api/notify', body);
  assert.equal(res[0], 200);
  assert.equal(m.lastPut().app_id_in_patch, true);
});

test('AppSecret-only save: top-level base_generation, no config.qq', () => {
  const { settings } = load('cred-ok');
  const base = { ...configToForm(settings.config), app_id: '' };
  const body = saveBody(settings, formToPatch({ ...base }, base), { secret: 'S'.repeat(32) });
  assert.equal(body.base_generation, 7);
  assert.equal(body.app_secret, 'S'.repeat(32));
  assert.ok(!('qq' in body.config));
});

test('holder committed but local settings pending: keep preference drafts and retry at the committed generation', () => {
  const { settings } = load('cred-ok');
  const before = configToForm(settings.config);
  const submitted = { ...before, app_id: '7654321', enabled: false, token_url: 'https://synthetic.example/token', confirm_secs: '90' };
  const committed = { ...settings, credentials_generation: 8, local_settings_pending: true, config: { ...settings.config, recovery_hold_secs: 45 } };
  const draft = pendingSettingsForm(committed, before, submitted);
  assert.equal(draft.app_id, '', 'the committed AppID is not resubmitted');
  assert.equal(draft.enabled, false);
  assert.equal(draft.token_url, submitted.token_url);
  assert.equal(draft.confirm_secs, '90');
  assert.equal(draft.recovery_hold_secs, '45', 'an unedited preference takes the current server value');
  const retry = saveBody(committed, formToPatch(draft, configToForm(committed.config)));
  assert.equal(retry.base_generation, 8);
  assert.deepEqual(retry.config, { enabled: false, confirm_secs: 90, qq: { token_url: submitted.token_url } });
  assert.ok(!('app_secret' in retry));
  assert.equal(pendingSettingsForm(settings, before, submitted), null, 'ordinary success takes the full returned settings');
});

import { clearSecretConfirm } from '../src/js/notify-state.js';

test('B1: every gap code is its own nowrap span, joined by 、 text', async () => {
  const r = readinessRow(load('cred-joining').status);
  assert.deepEqual(r.segments, [{ node: 'ny', parts: ['缺 AppID（待同步）', '缺 AppSecret（待同步）', '缺 openid（待同步）'] }]);
  assert.deepEqual(r.gaps, ['ny 缺 AppID（待同步）、缺 AppSecret（待同步）、缺 openid（待同步）'], 'text unchanged');
  const rb = readinessRow(load('cred-rebind').status);
  assert.equal(rb.segments.length, 3);
  assert.ok(rb.segments.every(g => g.parts.length === 1));
  const js = await readFile(new URL('../src/js/notify.js', import.meta.url), 'utf8');
  assert.match(js, /g\.parts\.forEach\(\(part, j\) => \{\s*if \(j\) v\.append\(document\.createTextNode\('、'\)\);\s*const span = document\.createElement\('span'\);\s*span\.className = 'notify-gap';/);
  assert.ok(!/span\.textContent = gap;/.test(js), 'no single span per node');
});

test('N1: 清除 confirm copy names the synced set', () => {
  assert.equal(clearSecretConfirm(), '将清除所有节点的 AppSecret，QQ 通知将停止，直到重新填写。');
});

test('N2: the AppID is write-only; the set flag is the only state', () => {
  assert.equal(appIdSet(load('cred-ok').settings), true);
  assert.equal(appIdSet(load('cred-appid-pending').settings), false);
});

test('L1: refused save/清除 with a typed AppSecret adds the second line and focus; none typed leaves banner as is', async () => {
  const m = await import('../src/js/notify-state.js');
  // Rev 9 L2: without the not_sent line the cleared AppSecret gets a muted note
  // under the refresh hint and focus stays on the banner.
  for (const [code, status] of [['credentials_conflict', 409], ['base_generation_required', 400], ['credentials_holder_unreachable', 503]]) {
    const missing = m.notifyRefusal({ status, body: { data: { code } } }, null, { route: 'config' });
    const typed = m.secretLostRefusal(missing, true);
    assert.equal(typed.banner.note, m.SECRET_CLEARED_NOTE, `${code} without outcome`);
    assert.equal(typed.banner.noteTone, 'muted', `${code}: muted`);
    assert.equal(typed.banner.lead, missing.banner.lead);
    assert.ok(!typed.focus, `${code}: banner keeps focus`);
    assert.equal(m.secretLostRefusal(missing, false), missing, `${code}: nothing typed`);
  }
  const uncoded = m.notifyRefusal({ status: 503, body: {} }, null, { route: 'config' });
  assert.equal(m.secretLostRefusal(uncoded, true).banner.note, m.SECRET_CLEARED_NOTE, 'uncoded 503 carries no outcome');
  for (const [code, status] of [['credentials_conflict', 409], ['base_generation_required', 400], ['credentials_holder_unreachable', 503]]) {
    const error = { status, body: { data: { code, outcome: 'not_sent' } } };
    const base = m.notifyRefusal(error, null, { route: 'config' });
    const typed = m.secretLostRefusal(base, true);
    assert.equal(typed.banner.note, 'AppSecret 未保存，请重新填写。');
    assert.equal(typed.banner.tone, base.banner.tone);
    assert.equal(typed.banner.lead, base.banner.lead);
    assert.equal(typed.focus, 'notify-app-secret');
    const none = m.secretLostRefusal(base, false);
    assert.equal(none, base);
    assert.ok(!none.banner.note && !none.focus);
  }
  // 解除绑定 never carries an AppSecret, so its refusals stay unchanged.
  const notOwner = m.notifyRefusal({ status: 409, body: { data: { code: 'not_owner' } } });
  assert.equal(m.secretLostRefusal(notOwner, true, 'openid'), notOwner);
});

test('L3/N2/rev 9 L1: an uncoded 503 is unknown on the settings route only; N1: one copy of each string', async () => {
  const m = await import('../src/js/notify-state.js');
  const r = m.notifyRefusal({ status: 503, body: {} }, null, { route: 'config' });
  assert.equal(r.kind, 'holder_unknown');
  assert.equal(r.banner.lead, '发送节点暂时无法连接，保存结果未知。');
  assert.equal(r.banner.text, m.OUTCOME_UNKNOWN_TEXT);
  assert.deepEqual(r.banner.action, { label: '刷新', target: 'refresh' });
  assert.equal(m.notifyRefusal({ status: 503, body: {} }, null, { route: 'openid' }), null);
  // Rev 4: a coded 503 maps on every route; a missing outcome is unknown.
  assert.equal(m.notifyRefusal({ status: 503, body: { data: { code: 'credentials_holder_unreachable' } } }, null, { route: 'openid' }).kind, 'holder_unknown');
  assert.equal(m.notifyRefusal({ status: 503, body: { data: { code: 'credentials_holder_unreachable', outcome: 'not_sent' } } }, null, { route: 'openid' }).kind, 'holder_down');
  const fs = await import('node:fs');
  const src = fs.readFileSync(new URL('../src/js/notify-state.js', import.meta.url), 'utf8');
  assert.equal(src.split('配置已在其他节点更新，请刷新后重试').length - 1, 1);
  assert.equal(src.split(m.SECRET_CLEARED_NOTE).length - 1, 1);
  assert.equal(src.split('暂时无法连接').length - 1, 1, 'one copy of the unreachable phrase');
});

// Minimal focus model: focus() on a disabled element is a no-op, like the browser.
function focusDoc() {
  const doc = { activeElement: null };
  const el = name => ({ name, value: '', disabled: false, focus() { if (!this.disabled) doc.activeElement = this; } });
  return { doc, secret: el('notify-app-secret'), button: el('notify-clear-secret') };
}

test('清除 success: activeElement is the AppSecret field after the finally render (spec 2931bccf)', async () => {
  const m = await import('../src/js/notify-state.js');
  const { doc, secret, button } = focusDoc();
  secret.value = 'synthetic-unsaved-secret';
  button.focus();
  let busy = false;
  await m.clearSecretFlow({
    setBusy: v => { busy = v; },
    render: () => { secret.disabled = busy; button.disabled = busy; },
    request: async () => ({ data: {} }),
    onSuccess: () => { assert.ok(secret.disabled, 'still busy inside onSuccess'); },
    onError: () => assert.fail('no error'),
    secretInput: () => secret,
  });
  assert.equal(doc.activeElement, secret);
  assert.equal(secret.disabled, false);
  assert.equal(secret.value, '', 'a completed clear discards any typed replacement secret');
});

test('清除 refused with nothing typed: focus does not move', async () => {
  const m = await import('../src/js/notify-state.js');
  const { doc, secret, button } = focusDoc();
  button.focus();
  let busy = false;
  await m.clearSecretFlow({
    setBusy: v => { busy = v; },
    render: () => { secret.disabled = busy; button.disabled = busy; },
    request: async () => { throw { status: 409, body: { data: { code: 'credentials_conflict' } } }; },
    onSuccess: () => assert.fail('no success'),
    onError: () => {},
    secretInput: () => secret,
  });
  assert.equal(doc.activeElement, button);
});

test('清除 transport failure: typed AppSecret is cleared after the error handler reads it', async () => {
  const m = await import('../src/js/notify-state.js');
  const { doc, secret, button } = focusDoc();
  secret.value = 'synthetic-unsaved-secret';
  button.focus();
  let busy = false;
  await m.clearSecretFlow({
    setBusy: v => { busy = v; },
    render: () => { secret.disabled = busy; button.disabled = busy; },
    request: async () => { throw new Error('synthetic transport error'); },
    onSuccess: () => assert.fail('no success'),
    onError: () => assert.equal(secret.value, 'synthetic-unsaved-secret', 'refusal handling can inspect whether a secret was typed'),
    secretInput: () => secret,
  });
  assert.equal(secret.value, '');
  assert.equal(secret.disabled, false);
  assert.equal(doc.activeElement, button);
});
