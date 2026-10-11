import test from 'node:test';
import assert from 'node:assert/strict';
import {
  bindView, configToForm, errorCopy, formToPatch, formatCountdown, formatUtc8, selectState, validateForm,
} from '../src/js/notify-state.js';

const full = {
  config: { enabled: true, confirm_secs: 60, recovery_hold_secs: 30, cooldown_secs: 1800, critical_cooldown_secs: 600,
    qq: { app_id: '1234567890', admin_openid: 'E4F0A1B2C3D4E5F60718293A4B5C6D7E', keep_online: true,
      api_base: 'https://api.sgroup.qq.com', token_url: 'https://bots.qq.com/app/getAppAccessToken' } },
  app_secret_configured: true,
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
  const edited = { ...form, cooldown_mins: '45', admin_openid: ' ABC ' };
  assert.deepEqual(formToPatch(edited, form), { cooldown_secs: 2700, qq: { admin_openid: 'ABC' } });
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
  const odd = validateForm({ ...form, admin_openid: 'ABC123' });
  assert.deepEqual(odd.errors, {});
  assert.ok(odd.warnings.admin_openid);
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
  const settings = { app_secret_configured: true, config: { enabled: true, qq: { app_id: '1', admin_openid: 'A' } } };
  assert.equal(selectState({ settings, status: { authority: 'delegate' } }).key, 'delegate');
  assert.equal(selectState({ settings, status: { authority: 'primary', gateway: { state: 'failed' } } }).key, 'gateway-failed');
});
