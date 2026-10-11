// notify.js — QQ 通知 panel in 系统设置. Own endpoint (/api/notify), own save
// button; never part of the /api/config patch.

import { getJson, putJson, postJson, deleteJson } from './api.js';
import { showNotification, setButtonLoading } from './dom.js';
import { isViewActive } from './state.js';
import {
  DEFAULTS, WATCH_PLATFORMS, addWatch, bindView, configToForm, errorCopy, filterCandidates,
  formToPatch, formatDuration, formatUtc8, isBadCredentials, removeWatch, sameForm, selectState,
  validateForm, watchKey,
} from './notify-state.js';

const FIELDS = {
  app_id: 'notify-app-id',
  admin_openid: 'notify-admin-openid',
  token_url: 'notify-token-url',
  api_base: 'notify-api-base',
  confirm_secs: 'notify-confirm-secs',
  recovery_hold_secs: 'notify-recovery-secs',
  cooldown_mins: 'notify-cooldown-mins',
  critical_cooldown_mins: 'notify-critical-cooldown-mins',
};
const HELP = {
  confirm_secs: 'notify-confirm-help',
  recovery_hold_secs: 'notify-recovery-help',
  cooldown_mins: 'notify-cooldown-help',
  critical_cooldown_mins: 'notify-critical-cooldown-help',
};

let settings = null;   // GET /api/notify data
let baseline = null;   // form snapshot of `settings`
let status = null;     // GET /api/notify/status data
let statusAt = 0;      // ms when `status` arrived
let loading = false;
let loadError = null;
let busy = false;
let lastAlert = '';
let lastBindState = 'idle';
let bindTimer = null;
let watchDraft = [];    // 关注列表 being edited

const $ = id => document.getElementById(id);

function readForm() {
  const form = { enabled: Boolean($('notify-enabled')?.checked) };
  for (const [key, id] of Object.entries(FIELDS)) form[key] = $(id)?.value ?? '';
  form.commands_enabled = $('notify-commands') ? $('notify-commands').checked : true;
  form.watch_enabled = Boolean($('notify-watch-enabled')?.checked);
  form.watch_channels = JSON.stringify(watchDraft);
  return form;
}

function fillForm(form) {
  $('notify-enabled').checked = form.enabled;
  for (const [key, id] of Object.entries(FIELDS)) $(id).value = form[key];
  if ($('notify-commands')) $('notify-commands').checked = form.commands_enabled;
  if ($('notify-watch-enabled')) $('notify-watch-enabled').checked = form.watch_enabled;
  watchDraft = JSON.parse(form.watch_channels || '[]');
  const advanced = $('notify-advanced');
  if (advanced && (form.token_url || form.api_base)) advanced.open = true;
}

function isDirty() {
  return Boolean(baseline) && (!sameForm(readForm(), baseline) || Boolean($('notify-app-secret')?.value.trim()));
}

function showFieldErrors(errors, warnings) {
  for (const [key, id] of Object.entries(FIELDS)) {
    const input = $(id);
    const message = errors[key] || (key === 'token_url' || key === 'api_base' ? errors.hosts : '');
    input?.setAttribute('aria-invalid', message ? 'true' : 'false');
    const help = HELP[key] && $(HELP[key]);
    if (help) {
      help.textContent = errors[key] || warnings[key] || help.dataset.help;
      help.classList.toggle('is-invalid', Boolean(errors[key] || warnings[key]));
    }
  }
  for (const key of ['app_id', 'admin_openid']) {
    const el = $(`notify-${key.replace('_', '-')}-error`);
    if (!el) continue;
    const text = errors[key] || warnings[key] || '';
    el.textContent = text;
    el.hidden = !text;
  }
  const hosts = $('notify-hosts-error');
  if (hosts) {
    hosts.textContent = errors.hosts || '';
    hosts.hidden = !errors.hosts;
  }
}

function setBadge(el, text, tone) {
  if (!el) return;
  el.className = `cluster-badge${tone ? ` ${tone}` : ''}`;
  el.textContent = text;
}

function row(label, value, { dot = '', title = '', badge = '' } = {}) {
  const wrap = document.createElement('div');
  wrap.className = 'cluster-node-meta-row';
  const l = document.createElement('span');
  l.className = 'cluster-node-meta-label';
  l.textContent = label;
  const v = document.createElement('span');
  v.className = 'cluster-node-meta-value notify-status-value';
  if (dot) {
    const d = document.createElement('span');
    d.className = `status-indicator ${dot}`;
    d.setAttribute('aria-hidden', 'true');
    v.append(d);
  }
  if (badge) {
    const b = document.createElement('span');
    b.className = `cluster-badge ${badge}`;
    b.textContent = value;
    v.append(b);
  } else {
    v.append(document.createTextNode(value));
  }
  if (title) v.title = title;
  wrap.append(l, v);
  return wrap;
}

function watchChip(channel) {
  const item = document.createElement('li');
  item.className = 'notify-watch-item';
  const label = document.createElement('span');
  label.className = 'notify-watch-name';
  label.textContent = channel.name || channel.channel_id;
  const meta = document.createElement('span');
  meta.className = 'notify-watch-meta notify-mono';
  meta.textContent = `${WATCH_PLATFORMS[channel.platform] || channel.platform} · ${channel.channel_id}`;
  const live = (status?.watch?.live || []).includes(watchKey(channel));
  if (live) {
    const badge = document.createElement('span');
    badge.className = 'cluster-badge active';
    badge.textContent = '直播中';
    label.append(' ', badge);
  }
  const remove = document.createElement('button');
  remove.type = 'button';
  remove.className = 'btn-ghost compact-btn notify-watch-remove';
  remove.textContent = '移除';
  remove.setAttribute('aria-label', `从关注列表移除 ${channel.name || channel.channel_id}`);
  remove.disabled = busy || !settings;
  remove.addEventListener('click', () => {
    watchDraft = removeWatch(watchDraft, watchKey(channel));
    render();
  });
  const text = document.createElement('span');
  text.className = 'notify-watch-text';
  text.append(label, meta);
  item.append(text, remove);
  return item;
}

function addToWatch(channel) {
  const { list, error } = addWatch(watchDraft, channel);
  const errEl = $('notify-watch-error');
  if (errEl) {
    errEl.textContent = error;
    errEl.hidden = !error;
  }
  if (error) return false;
  watchDraft = list;
  render();
  return true;
}

function renderWatch() {
  const listEl = $('notify-watch-list');
  if (!listEl) return;
  listEl.replaceChildren(...watchDraft.map(watchChip));
  const empty = $('notify-watch-empty');
  if (empty) empty.hidden = watchDraft.length > 0;
  const count = $('notify-watch-count');
  if (count) count.textContent = `${watchDraft.length} 个频道`;
  const results = $('notify-watch-results');
  if (results) {
    const matches = filterCandidates(settings?.watch_candidates || [], $('notify-watch-search')?.value, watchDraft);
    results.replaceChildren(...matches.map(channel => {
      const li = document.createElement('li');
      const btn = document.createElement('button');
      btn.type = 'button';
      btn.className = 'notify-watch-option';
      btn.disabled = busy || !settings;
      const name = document.createElement('span');
      name.className = 'notify-watch-name';
      name.textContent = channel.name || channel.channel_id;
      const meta = document.createElement('span');
      meta.className = 'notify-watch-meta notify-mono';
      meta.textContent = `${WATCH_PLATFORMS[channel.platform]} · ${channel.channel_id}`;
      btn.append(name, meta);
      btn.setAttribute('aria-label', `关注 ${channel.name || channel.channel_id}（${WATCH_PLATFORMS[channel.platform]}）`);
      btn.addEventListener('click', () => addToWatch(channel));
      li.append(btn);
      return li;
    }));
    const none = $('notify-watch-no-results');
    if (none) none.hidden = matches.length > 0;
  }
  for (const id of ['notify-watch-search', 'notify-watch-platform', 'notify-watch-id', 'notify-watch-name', 'notify-watch-add']) {
    const el = $(id);
    if (el) el.disabled = busy || !settings;
  }
}

function renderStatus() {
  const box = $('notify-status');
  if (!box) return;
  const rows = [];
  const s = status || {};
  const node = s.node_id || '本节点';
  const now = Date.now() / 1000;
  if (s.authority === 'primary') rows.push(row('发送节点', `${node}（本节点）`));
  else if (s.authority === 'delegate') rows.push(row('发送节点', `${node}（代发：活跃节点未配置完整）`));
  else if (s.authority === 'no_quorum_fallback') rows.push(row('发送节点', `${node}（无多数派代发）`));
  else if (s.authority_node_id) rows.push(row('发送节点', s.authority_node_id));
  else rows.push(row('发送节点', '暂无（等待选举）'));

  const gw = s.gateway || {};
  const keepOnline = settings?.config?.qq?.keep_online !== false;
  if (gw.state === 'failed') rows.push(row('QQ 网关', `已停止 · ${gw.reason || '机器人不可用'}`, { dot: 'notify-dot-danger', title: '修改凭据并保存后重试' }));
  else if (gw.state === 'online') rows.push(row('QQ 网关', `已连接 · ${formatDuration(now - gw.since)}`, { dot: 'status-live' }));
  else if (gw.state === 'connecting' || gw.state === 'offline') rows.push(row('QQ 网关', '未连接 · 正在重连', { dot: 'status-scheduled', title: gw.reason || '' }));
  else if (s.authority !== 'primary' && s.authority_node_id) rows.push(row('QQ 网关', '由发送节点保持'));
  else rows.push(row('QQ 网关', keepOnline ? '未连接' : '已关闭（未保持在线）', { dot: 'status-offline' }));

  const out = s.last_outcome;
  const modeText = { passive: '被动回复', active: '主动消息', wakeup: '召回消息' };
  rows.push(row('送达方式', out?.ok ? (modeText[out.mode] || '—') : '—'));
  if (!out) rows.push(row('上次发送', '尚未发送'));
  else {
    const from = out.node_id && out.node_id !== s.node_id ? `（来自 ${out.node_id}）` : '';
    const test = out.test ? '（测试）' : '';
    rows.push(row('上次发送', `${out.ok ? '成功' : '失败'} · ${formatUtc8(out.at)}${test}${from}`, {
      dot: out.ok ? 'status-live' : 'notify-dot-danger',
    }));
    if (!out.ok) {
      const [short] = errorCopy(out.code, out.reason);
      rows.push(row('错误', `${short}${out.code ? ` ${out.code}` : ''}`));
    }
    if (out.trace_id) rows.push(row('Trace ID', out.trace_id, { title: out.trace_id }));
  }
  rows.push(row('未送达', `${Number(s.undelivered) || 0} 条`));
  const quota = s.quota || {};
  rows.push(row('召回额度', quota.wakeups_left == null ? '未知（等待管理员消息）' : `剩余 ${quota.wakeups_left} / 4`));
  if (Array.isArray(s.nodes) && s.nodes.length) {
    const capable = s.nodes.filter(n => n.notify_capable);
    const missing = s.nodes.filter(n => !n.notify_capable).map(n => n.node_id);
    const text = `${capable.length} / ${s.nodes.length}${missing.length ? ` · ${missing.join('、')} 未配置完整` : ''}`;
    rows.push(row('可发送节点', text, { badge: missing.length ? 'sync-warning' : '' }));
  }
  box.replaceChildren(...rows);
}

function renderBind() {
  const box = $('notify-bind');
  if (!box) return;
  const view = bindView(status?.bind, (Date.now() - statusAt) / 1000);
  box.hidden = view.hidden;
  if (!view.hidden) {
    box.dataset.state = view.state;
    $('notify-bind-code').textContent = view.code;
    setBadge($('notify-bind-badge'), view.badge, view.tone);
    $('notify-bind-text').textContent = view.text;
    $('notify-bind-cancel').hidden = !view.cancel;
  }
  const waiting = view.state === 'waiting';
  const btn = $('notify-bind-btn');
  if (btn && !btn.classList.contains('is-loading')) {
    btn.textContent = waiting ? '重新生成' : '自动获取';
    btn.disabled = busy || !settings?.config?.qq?.app_id || !settings?.app_secret_configured;
    btn.title = btn.disabled ? '请先保存 AppID 和 AppSecret' : '生成配对码，在 QQ 向机器人发送后自动填入 openid';
  }
  if (waiting && !bindTimer) {
    bindTimer = setInterval(() => {
      renderBind();
      // Poll the status while waiting; SSE `status` also wakes us.
      if (Math.round((Date.now() - statusAt) / 1000) >= 3) refreshNotifyStatus();
    }, 1000);
  } else if (!waiting && bindTimer) {
    clearInterval(bindTimer);
    bindTimer = null;
  }
}

function render() {
  const panel = $('notify-settings');
  if (!panel) return;
  const st = selectState({ loading, loadError, settings, status });
  panel.dataset.state = st.key;
  setBadge($('notify-state-badge'), st.badge, st.tone);
  const node = status?.node_id || '本节点';
  const summary = {
    loading: '正在读取通知设置…',
    'load-error': `无法读取通知设置：${loadError?.message || ''}`,
    off: '未启用。填写机器人凭据并开启后，集群节点故障时会私信管理员。',
    'off-configured': '通知已关闭，凭据已保存。',
    incomplete: `还缺少：${(st.missing || []).join('、')}。`,
    sender: `本节点 ${node} 是当前发送节点，QQ 网关已连接。`,
    'gateway-down': `本节点 ${node} 是当前发送节点，但 QQ 网关未连接。`,
    standby: `当前由 ${status?.authority_node_id} 发送通知；本节点 ${node} 待命，接管后自动发送。`,
    delegate: `活跃节点缺少通知设置，本节点 ${node} 代为发送（消息带「代发」标记）。`,
    'gateway-failed': `QQ 网关已停止：${status?.gateway?.reason || ''}`,
    fallback: `集群暂无多数派，本节点 ${node} 临时代发（消息带「无多数派」标记）。`,
    electing: '暂无发送节点，通知会在选出后补发。',
    failed: status?.authority === 'primary' ? `本节点 ${node} 是当前发送节点。` : `本节点 ${node}。`,
  }[st.key];
  $('notify-summary').textContent = summary || '';

  let alert = '';
  let tone = 'warn';
  if (st.key === 'incomplete') alert = '本节点无法发送通知。填写缺少的项目并保存。';
  else if (st.key === 'gateway-failed') {
    tone = 'danger';
    alert = `QQ 网关已停止（${status?.gateway?.reason || '机器人不可用'}），不会自动重连。请在 QQ 开放平台检查机器人状态，修改凭据并保存后重试。`;
  } else if (st.key === 'gateway-down') alert = 'QQ 网关未连接，正在重连。断开期间只能用主动消息或召回额度发送，可能送达失败。';
  else if (st.key === 'failed') {
    tone = 'danger';
    const out = status?.last_outcome || {};
    const code = isBadCredentials(status) && !out.code ? 100016 : out.code;
    const [short, hint] = errorCopy(code, out.reason || status?.last_reason?.reason);
    alert = `${short}${code ? `（${code}）` : ''}。通知无法送达，转播不受影响。${hint}`;
  }
  const alertEl = $('notify-alert');
  if (alert !== lastAlert) {
    alertEl.textContent = alert;
    alertEl.dataset.tone = tone;
    alertEl.hidden = !alert;
    lastAlert = alert;
  }

  const secretSet = Boolean(settings?.app_secret_configured);
  setBadge($('notify-app-secret-state'), settings ? (secretSet ? '已设置' : '未设置') : '读取中', secretSet ? 'active' : '');
  $('notify-clear-app-secret').hidden = !secretSet;
  const secretInput = $('notify-app-secret');
  secretInput.placeholder = secretSet ? '已设置（留空则不变）' : '填写 AppSecret';
  secretInput.title = secretSet ? '已保存；输入新值后保存可替换' : '';

  const disabled = !settings || busy;
  for (const id of ['notify-enabled', 'notify-app-secret', 'notify-commands', 'notify-watch-enabled', ...Object.values(FIELDS)]) {
    const el = $(id);
    if (el) el.disabled = disabled;
  }
  const dirty = isDirty();
  $('notify-dirty').hidden = !dirty;
  const save = $('notify-save-btn');
  if (!save.classList.contains('is-loading')) save.disabled = disabled || !dirty;
  const test = $('notify-test-btn');
  const canTest = settings?.config?.qq?.app_id && secretSet && settings?.config?.qq?.admin_openid;
  if (!test.classList.contains('is-loading')) {
    test.disabled = disabled || !canTest;
    test.title = canTest ? '向管理员发送一条测试私信，会占用共享发送额度' : '请先保存 AppID、AppSecret 和管理员 openid';
  }
  renderStatus();
  renderBind();
  renderWatch();
}

function setResult(text) {
  const el = $('notify-action-result');
  if (el) el.textContent = text;
}

export async function loadNotifySettings() {
  if (!$('notify-settings')) return;
  loading = true;
  render();
  try {
    const [config, st] = await Promise.all([
      getJson('/api/notify'),
      getJson('/api/notify/status').catch(() => null),
    ]);
    applySettings(config.data, { force: !isDirty() });
    if (st?.data) { status = st.data; statusAt = Date.now(); }
    loadError = null;
  } catch (error) {
    loadError = error;
  } finally {
    loading = false;
    render();
  }
}

function applySettings(data, { force }) {
  const previous = baseline;
  settings = data;
  baseline = configToForm(data.config);
  if (force || !previous) fillForm(baseline);
  else {
    // Keep the draft but take server-side changes (e.g. a bound openid).
    for (const key of Object.keys(FIELDS)) {
      if (previous[key] !== baseline[key] && $(FIELDS[key]).value === previous[key]) $(FIELDS[key]).value = baseline[key];
    }
    if (previous.watch_channels !== baseline.watch_channels && JSON.stringify(watchDraft) === previous.watch_channels) {
      watchDraft = JSON.parse(baseline.watch_channels);
    }
  }
}

export async function refreshNotifyStatus() {
  if (!$('notify-settings') || !settings) return;
  try {
    const st = await getJson('/api/notify/status');
    status = st.data;
    statusAt = Date.now();
    const bindState = status?.bind?.state || 'idle';
    if (bindState === 'bound' && lastBindState === 'waiting') {
      showNotification('已获取管理员 openid', 'success');
      const config = await getJson('/api/notify');
      applySettings(config.data, { force: false });
    }
    lastBindState = bindState;
  } catch (error) {
    // Keep the last status; the next poll retries.
  }
  render();
}

async function save() {
  const form = readForm();
  const secret = $('notify-app-secret').value.trim();
  const { errors, warnings } = validateForm(form, {
    secretConfigured: settings?.app_secret_configured,
    secretEntered: Boolean(secret),
  });
  showFieldErrors(errors, warnings);
  if (Object.keys(errors).length) {
    const first = Object.keys(errors)[0];
    $(FIELDS[first] || FIELDS.token_url)?.focus();
    showNotification('通知设置保存失败: 请检查标红的项目', 'error');
    return;
  }
  if (!form.enabled && baseline?.enabled && status?.authority === 'primary'
    && !confirm('关闭后集群节点故障将不再发送 QQ 私信，确定吗？')) return;
  const body = { config: formToPatch(form, baseline), expected_revision: settings.revision };
  if (secret) {
    body.app_secret = secret;
    body.expected_secret_revision = settings.secret_revision;
  }
  busy = true;
  const btn = $('notify-save-btn');
  btn.textContent = '正在保存…';
  btn.setAttribute('aria-busy', 'true');
  setButtonLoading(btn, null, true);
  try {
    const result = await putJson('/api/notify', body);
    $('notify-app-secret').value = '';
    applySettings(result.data, { force: true });
    showNotification('通知设置已保存', 'success');
    setResult('');
  } catch (error) {
    const message = error.status === 409 ? '配置已被其他操作修改，请重新加载后再保存' : error.message;
    showNotification(`通知设置保存失败: ${message}`, 'error');
  } finally {
    busy = false;
    btn.textContent = '保存通知设置';
    btn.removeAttribute('aria-busy');
    setButtonLoading(btn, null, false);
    render();
    refreshNotifyStatus();
  }
}

async function clearSecret() {
  if (!confirm('确定要清除本节点保存的 AppSecret 吗？清除后本节点无法发送通知；如果它是发送节点，通知将无法送达，直到重新填写。')) return;
  busy = true;
  render();
  try {
    const result = await putJson('/api/notify', { clear_app_secret: true, expected_secret_revision: settings.secret_revision });
    applySettings(result.data, { force: !isDirty() });
    showNotification('AppSecret 已清除', 'success');
  } catch (error) {
    showNotification(`通知设置保存失败: ${error.message}`, 'error');
  } finally {
    busy = false;
    render();
    $('notify-app-secret').focus();
  }
}

async function sendTest() {
  const btn = $('notify-test-btn');
  btn.textContent = '正在发送…';
  btn.setAttribute('aria-busy', 'true');
  setButtonLoading(btn, null, true);
  if (isDirty()) setResult('测试使用已保存的设置');
  try {
    const result = await postJson('/api/notify/test', {});
    const report = result.data || {};
    const by = report.node_id ? `由 ${report.node_id} 发送` : '';
    if (report.ok) {
      const text = `测试消息已发送 · ${formatUtc8(Date.now() / 1000)}（UTC+8）${by ? ` · ${by}` : ''}`;
      setResult(text);
      showNotification(text, 'success');
    } else {
      const [short] = errorCopy(report.code, report.reason);
      const text = `测试消息发送失败：${short}${report.code ? `（${report.code}）` : ''}${by ? ` · ${by}` : ''}`;
      setResult(text);
      showNotification(text, 'error');
    }
  } catch (error) {
    setResult(`测试消息发送失败：${error.message}`);
    showNotification(`测试消息发送失败：${error.message}`, 'error');
  } finally {
    btn.textContent = '发送测试消息';
    btn.removeAttribute('aria-busy');
    setButtonLoading(btn, null, false);
    refreshNotifyStatus();
  }
}

async function startBind() {
  const btn = $('notify-bind-btn');
  setButtonLoading(btn, null, true);
  btn.textContent = '正在生成…';
  try {
    const mode = $('notify-bind-any')?.checked ? 'any' : 'code';
    const replace = Boolean(settings?.config?.qq?.admin_openid);
    if (replace && !confirm('已绑定管理员 openid。重新绑定成功后会替换它，确定吗？')) return;
    const result = await postJson('/api/notify/bind', { mode, replace });
    status = { ...(status || {}), bind: result.data };
    statusAt = Date.now();
    lastBindState = 'waiting';
    setResult('');
  } catch (error) {
    // 409 names the node that owns the QQ gateway.
    setResult(error.message);
    showNotification(error.message, 'warning');
  } finally {
    setButtonLoading(btn, null, false);
    btn.classList.remove('is-loading');
    render();
  }
}

async function cancelBind() {
  try {
    await deleteJson('/api/notify/bind', {});
  } catch (error) {
    showNotification(error.message, 'error');
  }
  refreshNotifyStatus();
}

export function initNotifySettings() {
  const panel = $('notify-settings');
  if (!panel) return;
  panel.addEventListener('input', event => {
    if (['notify-bind-any', 'notify-watch-search', 'notify-watch-id', 'notify-watch-name'].includes(event.target.id)) return;
    render();
  });
  panel.addEventListener('change', () => render());
  for (const id of Object.values(FIELDS)) {
    $(id)?.addEventListener('blur', () => {
      const { errors, warnings } = validateForm(readForm(), {
        secretConfigured: settings?.app_secret_configured,
        secretEntered: Boolean($('notify-app-secret').value.trim()),
      });
      showFieldErrors(errors, warnings);
    });
  }
  $('notify-save-btn')?.addEventListener('click', save);
  $('notify-test-btn')?.addEventListener('click', sendTest);
  $('notify-clear-app-secret')?.addEventListener('click', clearSecret);
  $('notify-bind-btn')?.addEventListener('click', startBind);
  $('notify-bind-cancel')?.addEventListener('click', cancelBind);
  $('notify-watch-search')?.addEventListener('input', () => renderWatch());
  $('notify-watch-add')?.addEventListener('click', () => {
    const ok = addToWatch({
      platform: $('notify-watch-platform').value,
      channel_id: $('notify-watch-id').value,
      name: $('notify-watch-name').value,
    });
    if (ok) {
      $('notify-watch-id').value = '';
      $('notify-watch-name').value = '';
    }
  });
  $('notify-refresh-btn')?.addEventListener('click', async () => {
    const icon = $('notify-refresh-icon');
    icon?.classList.add('is-spinning');
    await (settings ? refreshNotifyStatus() : loadNotifySettings());
    icon?.classList.remove('is-spinning');
  });
  // Same visibility gate as the other 系统设置 timers.
  setInterval(() => {
    if (isViewActive('settings') && !document.hidden) refreshNotifyStatus();
  }, 30_000);
}

export { DEFAULTS };
