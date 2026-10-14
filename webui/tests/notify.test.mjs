import test from 'node:test';
import assert from 'node:assert/strict';
import {
  bindView, configToForm, errorCopy, formToPatch, formatCountdown, formatUtc8, selectState, validateForm,
} from '../src/js/notify-state.js';

const full = {
  config: { enabled: true, confirm_secs: 60, recovery_hold_secs: 30, cooldown_secs: 1800, critical_cooldown_secs: 600,
    qq: { app_id: '1234567890', keep_online: true,
      api_base: 'https://api.sgroup.qq.com', token_url: 'https://bots.qq.com/app/getAppAccessToken' } },
  app_secret_configured: true,
  openid_set: true, openid_bound_at: 1_790_000_000, openid_revision: 3,
};

test('state selection follows the design table', () => {
  assert.equal(selectState({ loading: true }).key, 'loading');
  assert.equal(selectState({ loadError: new Error('x') }).key, 'load-error');
  assert.equal(selectState({ settings: { config: { qq: {} }, app_secret_configured: false } }).key, 'off');
  assert.equal(selectState({ settings: { ...full, config: { ...full.config, enabled: false } } }).key, 'off-configured');
  const incomplete = selectState({ settings: { ...full, app_secret_configured: false } });
  assert.equal(incomplete.key, 'incomplete');
  assert.deepEqual(incomplete.missing, ['AppSecret']);
  assert.equal(selectState({ settings: full, status: { authority: 'primary', gateway: { state: 'online' } } }).key, 'sender');
  assert.equal(selectState({ settings: full, status: { authority: 'primary', gateway: { state: 'offline' } } }).key, 'gateway-down');
  assert.equal(selectState({ settings: full, status: { authority: 'none', authority_node_id: 'a' } }).key, 'standby');
  assert.equal(selectState({ settings: full, status: { authority: 'none' } }).key, 'electing');
  assert.equal(selectState({ settings: full, status: { authority: 'no_quorum_fallback' } }).key, 'fallback');
  assert.equal(selectState({ settings: full, status: { authority: 'primary', last_outcome: { ok: false, code: 100016 } } }).key, 'failed');
});

test('form maps minutes to seconds and sends only changed keys', () => {
  const form = configToForm(full.config);
  assert.equal(form.cooldown_mins, '30');
  assert.equal(form.token_url, '');
  assert.deepEqual(formToPatch(form, form), {});
  assert.ok(!('admin_openid' in form), 'the openid is never part of the form (§16)');
  const edited = { ...form, cooldown_mins: '45' };
  assert.deepEqual(formToPatch(edited, form), { cooldown_secs: 2700 });
  const cleared = { ...form, confirm_secs: '' };
  assert.deepEqual(formToPatch(cleared, { ...form, confirm_secs: '90' }), { confirm_secs: 60 });
});

test('validation blocks bad values and warns on odd openids', () => {
  const form = configToForm(full.config);
  assert.deepEqual(validateForm(form).errors, {});
  const bad = validateForm({ ...form, app_id: '12ab', confirm_secs: '601', token_url: 'http://x' });
  assert.equal(bad.errors.app_id, 'AppID 应为数字');
  assert.equal(bad.errors.confirm_secs, '需为 0–600 的整数');
  assert.ok(bad.errors.hosts);
  const odd = validateForm(form, { openidSet: true, openidText: 'ABC123' });
  assert.deepEqual(odd.errors, {});
  assert.ok(odd.warnings.admin_openid);
  const badId = validateForm(form, { openidText: 'ABC 123!' });
  assert.ok(badId.errors.admin_openid);
  assert.ok(!badId.errors.admin_openid.includes('ABC'), 'messages never quote the input');
  assert.ok(validateForm(form, { openidSet: false }).warnings.admin_openid, 'enabled without an openid warns');
  assert.ok(validateForm({ ...form, critical_cooldown_mins: '60' }).warnings.critical_cooldown_mins);
});

test('error codes map to copy', () => {
  assert.equal(errorCopy(100016)[0], '凭据无效');
  assert.equal(errorCopy(40034105)[0], '主动消息不可用');
  assert.equal(errorCopy(null, '')[0], '无法连接 QQ');
  assert.equal(errorCopy(123, '奇怪')[0], '奇怪');
});

test('bind view shows code, countdown, success and expiry', () => {
  assert.equal(bindView({ state: 'idle' }).hidden, true);
  const waiting = bindView({ state: 'waiting', code: '482913', mode: 'code', remaining_secs: 300 }, 61);
  assert.equal(waiting.code, '482913');
  assert.equal(waiting.badge, '剩余 3:59');
  assert.equal(waiting.text, '在 QQ 向机器人发送此码');
  assert.equal(waiting.cancel, true);
  assert.equal(bindView({ state: 'waiting', code: '1', remaining_secs: 10 }, 11).state, 'expired');
  const bound = bindView({ state: 'bound', via: 'message', at: 1760166000 });
  assert.equal(bound.badge, '绑定成功');
  assert.equal(bindView({ state: 'expired', at: 1 }).badge, '已过期');
  assert.equal(formatCountdown(65), '1:05');
  assert.equal(formatUtc8(1760166000), '10-11 15:00');
});

import {
  addWatch, filterCandidates, removeWatch, watchEntryError, watchKey,
} from '../src/js/notify-state.js';

test('watch list: identity, validation, add/remove and patch', () => {
  assert.equal(watchKey({ platform: 'twitch', channel_id: 'SomeLogin' }), 'twitch:somelogin');
  let list = [];
  ({ list } = addWatch(list, { platform: 'youtube', channel_id: 'UCabc', name: '主频道' }));
  assert.equal(list.length, 1);
  assert.equal(addWatch(list, { platform: 'youtube', channel_id: 'UCabc' }).error, '已在关注列表中');
  assert.match(watchEntryError({ platform: 'youtube', channel_id: 'bad id' }), /只能包含/);
  assert.equal(watchEntryError({ platform: 'bilibili', channel_id: 'x' }), '请选择平台');
  assert.match(watchEntryError({ platform: 'twitch', channel_id: 'x', name: 'n'.repeat(41) }), /40/);
  const candidates = [
    { platform: 'youtube', channel_id: 'UCabc', name: '主频道' },
    { platform: 'twitch', channel_id: 'other', name: '副频道' },
  ];
  assert.deepEqual(filterCandidates(candidates, '', list).map(c => c.channel_id), ['other']);
  assert.deepEqual(filterCandidates(candidates, 'twitch', []).map(c => c.channel_id), ['other']);
  assert.deepEqual(filterCandidates(candidates, '主', []).map(c => c.channel_id), ['UCabc']);
  list = removeWatch(list, 'youtube:UCabc');
  assert.equal(list.length, 0);

  const base = configToForm({ watch: { enabled: false, channels: [] } });
  assert.equal(base.commands_enabled, true);
  const next = { ...base, watch_enabled: true, watch_channels: JSON.stringify([candidates[1]]) };
  const patch = formToPatch(next, base);
  assert.deepEqual(patch.watch, { enabled: true, channels: [candidates[1]] });
  assert.equal(patch.commands_enabled, undefined);
  assert.deepEqual(formToPatch({ ...base, commands_enabled: false }, base), { commands_enabled: false });
});

test('delegate and stopped gateway states', () => {
  const settings = { app_secret_configured: true, openid_set: true, config: { enabled: true, qq: { app_id: '1' } } };
  assert.equal(selectState({ settings, status: { authority: 'delegate' } }).key, 'delegate');
  assert.equal(selectState({ settings, status: { authority: 'primary', gateway: { state: 'failed' } } }).key, 'gateway-failed');
});

// Batch 2: the badge follows the sender node's last send (sender.last_send).
import {
  localOutcome as b2localOutcome, middleTruncate as b2middleTruncate, selectState as b2selectState,
  senderOutcome as b2senderOutcome, staleLocalFailure as b2staleLocalFailure,
} from '../src/js/notify-state.js';

const b2settings = { config: { enabled: true, qq: { app_id: '1' } }, app_secret_configured: true, openid_set: true };

test('senderOutcome prefers sender.last_send and maps via to mode', () => {
  const status = { sender: { node: 'tokyo-1', last_send: { at: 200, ok: true, code: null, via: 'passive' } }, last_outcome: { ok: false, at: 100, code: 22009, node_id: 'osaka-2' } };
  assert.deepEqual(b2senderOutcome(status), { ok: true, at: 200, mode: 'passive', code: null, reason: null, trace_id: null, node_id: 'tokyo-1' });
  assert.equal(b2senderOutcome({ sender: { node: 'tokyo-1', last_send: null } }), null);
  assert.equal(b2senderOutcome({ last_outcome: { ok: true, at: 1 } }).at, 1, 'older backends fall back to the local outcome');
  assert.equal(b2localOutcome({ local_last_send: { at: 5 }, last_outcome: { at: 1 } }).at, 5);
});

test('a stale local failure on a standby is a detail row, not a red badge', () => {
  const status = {
    authority: 'none', authority_node_id: 'tokyo-1', node_id: 'osaka-2',
    sender: { node: 'tokyo-1', last_send: { at: 200, ok: true, via: 'active' } },
    last_outcome: { ok: false, at: 100, code: 22009, node_id: 'osaka-2', trace_id: 't' },
  };
  assert.equal(b2selectState({ settings: b2settings, status }).key, 'standby');
  assert.equal(b2staleLocalFailure(status).code, 22009);
  const failing = { ...status, sender: { node: 'tokyo-1', last_send: { at: 300, ok: false, code: 40034105 } } };
  assert.equal(b2selectState({ settings: b2settings, status: failing }).key, 'failed');
  assert.equal(b2staleLocalFailure({ last_outcome: { ok: false, at: 1 } }), null, 'no duplicate row when the local outcome is the one shown');
});

test('middleTruncate keeps both ends of long ids', () => {
  assert.equal(b2middleTruncate('short'), 'short');
  const out = b2middleTruncate('0123456789abcdefghijklmnopqrstuvwxyz', 16);
  assert.equal(out.length, 16);
  assert.ok(out.startsWith('01234567') && out.endsWith('tuvwxyz') && out.includes('…'));
});

// §16: openid write-only, per-node credentials (1b) and the 1a fallback.
import { credentialRows, hasNodeCredentials } from '../src/js/notify-state.js';

test('missing openid comes from openid_set, never a value', () => {
  const st = selectState({ settings: { ...full, openid_set: false } });
  assert.equal(st.key, 'incomplete');
  assert.deepEqual(st.missing, ['管理员 openid']);
});

test('hasNodeCredentials needs the 1b boolean fields, not just a nodes array', () => {
  assert.equal(hasNodeCredentials({}), false);
  assert.equal(hasNodeCredentials({ nodes: [] }), false);
  assert.equal(hasNodeCredentials({ nodes: [{ node_id: 'a', notify_capable: true }] }), false, "today's nodes[] is 1a");
  assert.equal(hasNodeCredentials({ nodes: [{ node_id: 'a', appsecret_set: true, openid_set: false }] }), true);
});

const n = (node_id, appsecret_set, openid_set, extra = {}) => ({ node_id, appsecret_set, openid_set, notify_capable: true, fresh: true, ...extra });

test('2n and 2z only with 1b data; 2z wins; a QQ failure still wins', () => {
  const standby = { authority: 'none', authority_node_id: 'a' };
  const unready = { ...standby, nodes: [n('a', true, true), n('b', true, false, { is_local: true })] };
  const st = selectState({ settings: full, status: unready });
  assert.equal(st.key, 'node-unready');
  assert.deepEqual(st.lacks, ['管理员 openid']);
  assert.equal(selectState({ settings: full, status: { ...standby, nodes: [{ node_id: 'b', is_local: true, notify_capable: false }] } }).key, 'standby', '1a: no 2n');
  const none = { ...standby, nodes: [n('a', false, true), n('b', true, false, { is_local: true })] };
  assert.equal(selectState({ settings: full, status: none }).key, 'no-sender');
  assert.equal(selectState({ settings: full, status: { ...none, last_outcome: { ok: false, code: 100016 } } }).key, 'failed');
});

test('credentialRows shows booleans only and the unbind progress', () => {
  assert.deepEqual(credentialRows({ nodes: [{ node_id: 'a' }] }), [], '1a: row hidden');
  const status = { nodes: [n('a', true, true, { is_local: true }), n('b', false, false), n('c', true, true, { fresh: false })] };
  const rows = credentialRows(status);
  assert.deepEqual(rows.map(r => [r.secret.text, r.openid.text]), [
    ['AppSecret 已同步', 'openid 已同步'], ['AppSecret 未设置', 'openid 未同步'], ['AppSecret 已同步', 'openid 已同步'],
  ]);
  const unbinding = { nodes: [n('a', true, false, { is_local: true }), n('b', true, true), n('c', true, true, { fresh: false })] };
  const pend = credentialRows(unbinding, { pending: new Set(['b', 'c']), localUnbound: true });
  assert.deepEqual(pend.map(r => r.openid?.text ?? null), [null, '解绑中…', '等待节点上线后清除']);
  const done = credentialRows({ nodes: [n('a', true, false, { is_local: true }), n('b', true, false)] }, { localUnbound: true, cleared: new Map([['b', 1000]]), now: 5000 });
  assert.deepEqual(done.map(r => r.openid?.text ?? null), [null, '已清除']);
  assert.doesNotMatch(JSON.stringify(rows), /[0-9A-F]{16,}/, 'no id-like values');
});

import { appSecretSet, isHandoff, notifyRefusal, openidGate } from '../src/js/notify-state.js';

test('openidGate: local, remote, null with reasons, and absent', () => {
  assert.deepEqual(openidGate({ openid_writer: { node_id: 'a', is_local: true } }), { enabled: true, hint: '' });
  const remote = openidGate({ openid_writer: { node_id: 'tokyo-1', is_local: false } });
  assert.equal(remote.enabled, false);
  assert.match(remote.hint, /^请在 tokyo-1 上绑定或解绑。/);
  assert.match(openidGate({ openid_writer: null, authority: 'none', nodes: [{}, {}] }).hint, /集群暂无多数派/);
  assert.match(openidGate({ openid_writer: null, authority: 'no_quorum_fallback' }).hint, /无多数派代发/);
  assert.match(openidGate({ openid_writer: null, enabled: false }).hint, /请先启用通知/);
  assert.match(openidGate({ openid_writer: null }).hint, /^暂无可写节点。请稍后重试。$/);
  assert.deepEqual(openidGate({ authority: 'none' }), { enabled: true, hint: '' }, 'absent key = unknown, enabled');
  assert.deepEqual(openidGate(null), { enabled: true, hint: '' });
  const handoff = { openid_writer: { node_id: 'b', is_local: true }, gateway_owner: { node_id: 'a', is_local: false } };
  assert.ok(isHandoff(handoff));
  assert.deepEqual(openidGate(handoff), { enabled: true, hint: '发送节点切换中，绑定可能暂时失败。' }, 'handoff hints, never disables');
  assert.equal(isHandoff({ openid_writer: { node_id: 'b', is_local: true }, gateway_owner: null }), false);
  assert.deepEqual(openidGate({ openid_writer: { node_id: 'solo', is_local: true }, gateway_owner: null }), { enabled: true, hint: '' }, 'single node, cluster off');
});

test('notifyRefusal branches on data.code for every refusal', () => {
  const err = data => ({ status: 409, body: { success: false, message: 'x', data } });
  assert.equal(notifyRefusal(err({ code: 'not_owner', owner: 'tokyo-1' })).banner.text, '请在 tokyo-1 上绑定或解绑。');
  assert.equal(notifyRefusal(err({ code: 'not_owner' })).banner.text, '请在当前发送节点上绑定或解绑。');
  assert.equal(notifyRefusal(err({ code: 'revision_conflict', current_revision: 4 })).kind, 'revision');
  const handoff = { openid_writer: { node_id: 'b', is_local: true }, gateway_owner: { node_id: 'a', is_local: false } };
  assert.equal(notifyRefusal(err({ code: 'no_writer' }), handoff).banner.text, '发送节点切换中，请稍后重试。');
  assert.equal(notifyRefusal(err({ code: 'revision_conflict' }), null, { route: 'config' }).banner.lead, '设置已在其他地方更新，已刷新，请检查后重新保存。');
  assert.deepEqual(notifyRefusal(err({ code: 'already_bound' })).banner.action, { label: '去解除绑定', target: 'notify-clear-openid' });
  assert.equal(notifyRefusal(err({ code: 'already_bound' })).banner.lead, '已绑定，请先解除绑定再重新绑定。');
  assert.deepEqual(notifyRefusal(err({ code: 'credentials_missing' })).banner.action, { label: '去填写', target: 'credentials' });
  assert.equal(notifyRefusal(err({ code: 'bad_credentials' })).banner.action.label, '重新填写');
  assert.equal(notifyRefusal(err({ code: 'mystery' })), null);
  assert.equal(notifyRefusal(err(null)), null);
  assert.equal(notifyRefusal({ status: 409, body: { success: false, message: 'x' } }), null, 'missing data');
  assert.equal(notifyRefusal(new Error('network')), null);
});

test('appsecret_set wins over the deprecated name', () => {
  assert.equal(appSecretSet({ appsecret_set: false, app_secret_configured: true }), false);
  assert.equal(appSecretSet({ app_secret_configured: true }), true);
  assert.equal(appSecretSet(null), false);
});

import { noCapableNode } from '../src/js/notify-state.js';

test('status.warning wins over the per-node fallback', () => {
  const ready = [{ node_id: 'a', is_local: true, appsecret_set: true, openid_set: true }];
  const empty = [{ node_id: 'a', is_local: true, appsecret_set: false, openid_set: false }];
  assert.equal(noCapableNode({ warning: 'no_capable_node', nodes: ready }), true);
  assert.equal(noCapableNode({ warning: null, nodes: empty }), false);
  assert.equal(noCapableNode({ nodes: empty }), true, 'absent field falls back to nodes');
  assert.equal(noCapableNode({ nodes: ready }), false);
  assert.equal(noCapableNode({}), false);
  assert.equal(selectState({ settings: full, status: { warning: 'no_capable_node' } }).key, 'no-sender');
});

test('keep_online off: gateway_owner null between bind windows is not a handoff', () => {
  const st = { openid_writer: { node_id: 'a', is_local: true }, gateway_owner: null };
  assert.equal(isHandoff(st), false);
  assert.deepEqual(openidGate(st), { enabled: true, hint: '' });
  const remote = { openid_writer: { node_id: 'a', is_local: false }, gateway_owner: null };
  assert.equal(isHandoff(remote), false);
  assert.doesNotMatch(openidGate(remote).hint, /切换中/);
  assert.equal(isHandoff({ openid_writer: { node_id: 'a', is_local: true }, gateway_owner: { node_id: 'a', is_local: true } }), false);
});

import { liveRefusal, mergeOpenidFields } from '../src/js/notify-state.js';

test('B1: unbind keeps AppID and 启用', () => {
  const before = { config: { enabled: true, qq: { app_id: '1029384' } }, appsecret_set: true, openid_set: true, openid_bound_at: 1, openid_revision: 4, revision: 9 };
  const after = mergeOpenidFields(before, { openid_set: false, openid_bound_at: null, openid_revision: 5 });
  assert.equal(after.config.qq.app_id, '1029384');
  assert.equal(after.config.enabled, true);
  assert.equal(after.appsecret_set, true);
  assert.equal(after.revision, 9);
  assert.deepEqual([after.openid_set, after.openid_bound_at, after.openid_revision], [false, null, 5]);
  assert.equal(mergeOpenidFields(before, undefined).openid_set, false, 'empty body still marks unbound');
  assert.equal(mergeOpenidFields(before, { config: {} }).config.qq.app_id, '1029384', 'ignores non-openid keys');
});

test('B2: writer refusals follow fresh status and drop when controls re-enable', () => {
  const err = data => ({ status: 409, body: { success: false, message: 'x', data } });
  const remote = { openid_writer: { node_id: 'b', is_local: false } };
  const local = { openid_writer: { node_id: 'a', is_local: true } };
  assert.equal(liveRefusal(err({ code: 'not_owner', owner: 'b' }), 'openid', remote).kind, 'not_owner');
  assert.equal(liveRefusal(err({ code: 'not_owner', owner: 'b' }), 'openid', local), null);
  assert.equal(liveRefusal(err({ code: 'no_writer' }), 'openid', local), null);
  const quorum = { openid_writer: null, authority: 'none', nodes: [{}, {}] };
  assert.match(liveRefusal(err({ code: 'no_writer' }), 'openid', quorum).banner.text, /集群暂无多数派/);
  assert.equal(liveRefusal(err({ code: 'no_writer' }), 'openid', {}).kind, 'no_writer', 'unknown writer keeps the banner');
  assert.equal(liveRefusal(err({ code: 'bad_credentials' }), 'openid', local).kind, 'bad_credentials');
});
