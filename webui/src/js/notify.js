// notify.js — QQ 通知 panel in 系统设置. Own endpoint (/api/notify), own save
// button; never part of the /api/config patch.

import { getJson, putJson, postJson, deleteJson } from './api.js';
import { showNotification, setButtonLoading, reconcileChildren } from './dom.js';
import { isViewActive } from './state.js';
import { LIVE_EVENTS_TICK_MS, renderLiveEventsInto } from './live-events-view.js';
import {
  DEFAULTS, WATCH_PLATFORMS, addWatch, bindView, configToForm, errorCopy, filterCandidates,
  appSecretSet, credentialRows, formToPatch, formatDuration, formatUtc8, hasNodeCredentials, isBadCredentials, localOutcome,
  middleTruncate, removeWatch, openidPending, openidView, readinessRow, writerOpenidSet, isSkipped, liveEventsFieldError, liveEventsGate, pickBudget, skippedRow, pendingLiveNote, liveEventsSupported, withLiveEvents, wakeupBudgetView,
  createSeq, liveRefusal, mergeDraft, refusalSurvivesEdit, unbindFlow, notifyRefusal, openidGate, sameForm, selectState, senderOutcome, staleLocalFailure, validateForm, watchKey,
} from './notify-state.js';

const FIELDS = {
  app_id: 'notify-app-id',
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
let statusSeq = 0;     // R1: request sequence of the fetch that produced `status`
const statusReq = createSeq();   // /api/notify/status requests
const settingsReq = createSeq(); // L1: GET /api/notify requests
let loading = false;
let loadError = null;
let busy = false;
let lastAlert = '';
let lastBindState = 'idle';
let bindTimer = null;
let watchDraft = [];    // 关注列表 being edited
let watchSig = '';      // inputs of the last renderWatch(); skip identical re-renders
let statusSig = '';     // same for renderStatus()
let liveEventsError = '';  // inline invalid_field copy for the 开播/下播 switch
const watchChips = new Map();    // watchKey → <li>, reused so focus survives
const watchOptions = new Map();  // watchKey → <li> in the picker
// Unbind progress (§16.9): node ids that still had the openid when the user
// confirmed, and when each one reported it cleared.
let pendingUnbind = new Set();
let refusal = null;   // last coded refusal from a notify write (banner + recovery)
let refusalSrc = null; // { error, route, seq, creds } so the banner follows fresh status (B2, R1, L3)
const clearedAt = new Map();

const $ = id => document.getElementById(id);

function readForm() {
  const form = { enabled: Boolean($('notify-enabled')?.checked) };
  for (const [key, id] of Object.entries(FIELDS)) form[key] = $(id)?.value ?? '';
  form.commands_enabled = $('notify-commands') ? $('notify-commands').checked : true;
form.watch_enabled = Boolean($('notify-watch-enabled')?.checked);
  form.watch_channels = JSON.stringify(watchDraft);
  return withLiveEvents(form, liveEventsSupported(settings?.config), $('notify-live-events')?.checked);
}

function fillForm(form) {
  $('notify-enabled').checked = form.enabled;
  for (const [key, id] of Object.entries(FIELDS)) $(id).value = form[key];
  if ($('notify-commands')) $('notify-commands').checked = form.commands_enabled;
  if ($('notify-live-events') && 'live_events_enabled' in form) $('notify-live-events').checked = form.live_events_enabled;
  if ($('notify-watch-enabled')) $('notify-watch-enabled').checked = form.watch_enabled;
  watchDraft = JSON.parse(form.watch_channels || '[]');
  watchChips.clear();
  watchOptions.clear();
  watchSig = '';
  const advanced = $('notify-advanced');
  if (advanced && (form.token_url || form.api_base)) advanced.open = true;
}

function typedOpenid() {
  return $('notify-admin-openid')?.value.trim() || '';
}

function isDirty() {
  return Boolean(baseline) && (!sameForm(readForm(), baseline)
    || Boolean($('notify-app-secret')?.value.trim()) || Boolean(typedOpenid()));
}

function validateNow() {
  return validateForm(readForm(), {
    secretConfigured: appSecretSet(settings),
    secretEntered: Boolean($('notify-app-secret')?.value.trim()),
    openidSet: Boolean(settings?.openid_set),
    openidText: typedOpenid(),
    openidPending: openidPending(settings, status),
  });
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
  // Never echo the typed openid: messages are fixed strings (§16.4).
  $('notify-admin-openid')?.setAttribute('aria-invalid', errors.admin_openid ? 'true' : 'false');
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

// B3: only touch the DOM on a real change, so role="status" badges are not
// re-announced on every keystroke.
function setBadge(el, text, tone) {
  if (!el) return;
  const cls = `cluster-badge${tone ? ` ${tone}` : ''}`;
  if (el.className !== cls) el.className = cls;
  if (el.textContent !== text) el.textContent = text;
}

function clearRefusal() { refusal = null; refusalSrc = null; }
function readCreds() {
  return { app_id: $('notify-app-id')?.value || '', app_secret: $('notify-app-secret')?.value || '' };
}
// L3: edits clear the banner, except a credential hint while AppID/AppSecret are unchanged.
function clearRefusalOnEdit() {
  if (!refusal) return;
  if (refusalSrc && refusalSurvivesEdit(refusal.kind, refusalSrc.creds, readCreds())) return;
  clearRefusal();
}

function row(label, value, { dot = '', title = '', badge = '', valueClass = '', copy = '' } = {}) {
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
  } else if (copy) {
    const btn = document.createElement('button');
    btn.type = 'button';
    btn.className = `notify-copy ${valueClass}`.trim();
    btn.textContent = value;
    btn.setAttribute('aria-label', `${label} ${copy}，点击复制`);
    btn.addEventListener('click', () => copyText(copy, `已复制 ${label}`));
    v.append(btn);
  } else {
    if (valueClass) v.classList.add(...valueClass.split(' '));
    v.append(document.createTextNode(value));
  }
  if (title) v.title = title;
  wrap.append(l, v);
  return wrap;
}

function watchChip(channel) {
  const key = watchKey(channel);
  let item = watchChips.get(key);
  if (!item) {
    item = document.createElement('li');
    item.className = 'notify-watch-item';
    item.dataset.key = key;
    const label = document.createElement('span');
    label.className = 'notify-watch-name';
    const name = document.createElement('span');
    name.textContent = channel.name || channel.channel_id;
    const badge = document.createElement('span');
    badge.className = 'cluster-badge active notify-watch-live';
    badge.textContent = '直播中';
    label.append(name, ' ', badge);
    const meta = document.createElement('span');
    meta.className = 'notify-watch-meta notify-mono';
    meta.textContent = `${WATCH_PLATFORMS[channel.platform] || channel.platform} · ${channel.channel_id}`;
    const remove = document.createElement('button');
    remove.type = 'button';
    remove.className = 'btn-ghost compact-btn notify-watch-remove';
    remove.textContent = '移除';
    remove.setAttribute('aria-label', `从关注列表移除 ${channel.name || channel.channel_id}`);
    remove.addEventListener('click', () => removeFromWatch(key));
    const text = document.createElement('span');
    text.className = 'notify-watch-text';
    text.append(label, meta);
    item.append(text, remove);
    watchChips.set(key, item);
  }
  const live = (status?.watch?.live || []).includes(key);
  const badge = item.querySelector('.notify-watch-live');
  badge.hidden = !live;
  item.querySelector('.notify-watch-remove').disabled = busy || !settings;
  return item;
}

function watchOption(channel) {
  const key = watchKey(channel);
  let li = watchOptions.get(key);
  if (!li) {
    li = document.createElement('li');
    const btn = document.createElement('button');
    btn.type = 'button';
    btn.className = 'notify-watch-option';
    const name = document.createElement('span');
    name.className = 'notify-watch-name';
    name.textContent = channel.name || channel.channel_id;
    const meta = document.createElement('span');
    meta.className = 'notify-watch-meta notify-mono';
    meta.textContent = `${WATCH_PLATFORMS[channel.platform]} · ${channel.channel_id}`;
    btn.append(name, meta);
    btn.setAttribute('aria-label', `关注 ${channel.name || channel.channel_id}（${WATCH_PLATFORMS[channel.platform]}）`);
    btn.addEventListener('click', () => {
      if (addToWatch(channel)) $('notify-watch-search')?.focus();
    });
    li.append(btn);
    watchOptions.set(key, li);
  }
  li.firstChild.disabled = busy || !settings;
  return li;
}

// After 移除, keep keyboard users in the list: next chip's 移除, else the
// previous one, else the search box.
function removeFromWatch(key) {
  const index = watchDraft.findIndex(channel => watchKey(channel) === key);
  watchDraft = removeWatch(watchDraft, key);
  watchChips.delete(key);
  render();
  const next = watchDraft[index] || watchDraft[index - 1];
  const target = next ? watchChips.get(watchKey(next))?.querySelector('.notify-watch-remove') : $('notify-watch-search');
  target?.focus();
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
  const disabled = busy || !settings;
  for (const id of ['notify-watch-search', 'notify-watch-platform', 'notify-watch-id', 'notify-watch-name', 'notify-watch-add']) {
    const el = $(id);
    if (el) el.disabled = disabled;
  }
  // Only rebuild when something the list shows changed (F1): typing in other
  // fields and status polls must not move focus.
  const query = $('notify-watch-search')?.value || '';
  const sig = JSON.stringify([watchDraft, settings?.watch_candidates || [], query, status?.watch?.live || [], disabled]);
  if (sig === watchSig) return;
  watchSig = sig;
  reconcileChildren(listEl, watchDraft.map(watchChip));
  const empty = $('notify-watch-empty');
  if (empty) empty.hidden = watchDraft.length > 0;
  const count = $('notify-watch-count');
  if (count) count.textContent = `${watchDraft.length} 个频道`;
  const results = $('notify-watch-results');
  if (results) {
    const matches = filterCandidates(settings?.watch_candidates || [], query, watchDraft);
    reconcileChildren(results, matches.map(watchOption));
    const none = $('notify-watch-no-results');
    if (none) none.hidden = matches.length > 0;
  }
}

// §17.3: budget line, next-close segment and passive hint. Only text nodes
// and attributes change, and only when they differ (no flicker, B3).
function setText(el, text) { if (el && el.textContent !== text) el.textContent = text; }
function setHidden(el, hidden) { if (el && el.hidden !== hidden) el.hidden = hidden; }
function renderLiveEvents() {
  renderLiveEventsInto(document, {
    group: $('notify-types'),
    line: $('notify-wakeup-budget'), main: $('notify-wakeup-main'), next: $('notify-wakeup-next'),
    passive: $('notify-passive-hint'), error: $('notify-live-events-error'), owner: $('notify-live-events-owner'),
    input: $('notify-live-events'),
  }, { status, settings, statusAt, now: Date.now(), error: liveEventsError, supported: liveEventsSupported(settings?.config) });
}

function renderStatus() {
  const box = $('notify-status');
  if (!box) return;
  const rows = [];
  const s = status || {};
  const node = s.node_id || '本节点';
  const now = Date.now() / 1000;
  if (s.authority === 'primary') rows.push(row('发送节点', `${node}（本节点）`));
  else if (s.authority === 'delegate') rows.push(row('发送节点', `${node}（代发：活跃节点通知设置不完整）`));
  else if (s.authority === 'no_quorum_fallback') rows.push(row('发送节点', `${node}（无多数派代发）`));
  else if (s.authority_node_id) rows.push(row('发送节点', s.authority_node_id));
  else rows.push(row('发送节点', '暂无（等待选举）'));

  const gw = s.gateway || {};
  const enabled = Boolean(settings?.config?.enabled);
  const keepOnline = settings?.config?.qq?.keep_online !== false;
  if (gw.state === 'failed') rows.push(row('QQ 网关', `已停止 · ${gw.reason || '机器人不可用'}`, { dot: 'notify-dot-danger', title: '修改凭据并保存后重试' }));
  else if (gw.state === 'online') rows.push(row('QQ 网关', `已连接 · ${formatDuration(now - gw.since)}`, { dot: 'status-live' }));
  else if (gw.state === 'connecting' || gw.state === 'offline') rows.push(row('QQ 网关', '未连接 · 正在重连', { dot: 'status-scheduled', title: gw.reason || '' }));
  else if (s.authority !== 'primary' && s.authority_node_id) rows.push(row('QQ 网关', '由发送节点保持'));
  else if (!enabled) rows.push(row('QQ 网关', '未启用'));
  else if (!keepOnline) rows.push(row('QQ 网关', '已关闭（未保持在线）'));
  else rows.push(row('QQ 网关', '未连接', { dot: 'status-offline' }));

  const out = senderOutcome(s);
  const modeText = { passive: '被动回复', active: '主动消息', wakeup: '召回消息' };
  rows.push(row('送达方式', out?.ok ? (modeText[out.mode] || '—') : '—'));
  if (!out) {
    const senderNode = s.sender?.node;
    const isSender = s.authority === 'primary';
    rows.push(row('上次发送', isSender || !s.sender ? '尚未发送' : senderNode ? `暂无记录（发送节点 ${senderNode}）` : '—'));
  }
  else if (isSkipped(out)) {
    // §17.4: held, not failed — warn tone, no 错误 / Trace ID rows.
    rows.push(row('上次发送', skippedRow(out, s.node_id), { valueClass: 'is-skipped' }));
  }
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
    if (out.trace_id) rows.push(traceRow(out.trace_id));
  }
  const stale = staleLocalFailure(s);
  if (stale) {
    const [short] = errorCopy(stale.code, stale.reason);
    rows.push(row('本节点上次', `失败 · ${formatUtc8(stale.at)} · ${short}${stale.code ? ` ${stale.code}` : ''}`));
    if (stale.trace_id && stale.trace_id !== out?.trace_id) rows.push(traceRow(stale.trace_id));
  }
  // Addendum D: pending notices from their own field, fresh /status first.
  const pendingSrc = s && 'pending_live_events' in s ? s : settings;
  const pending = pendingLiveNote(pendingSrc, s.sender?.node || '', s.node_id || '');
  if (pending && liveEventsSupported(settings?.config)) rows.push(row('待补发', pending));
  rows.push(row('未送达', `${Number(s.undelivered) || 0} 条`));
  const readiness = readinessRow(s);
  if (readiness) rows.push(readinessRowEl(readiness));
  if (hasNodeCredentials(s)) rows.push(credentialRow(s));
  // Addendum C supersedes the older notify_capable summary.
  else if (!readiness && Array.isArray(s.nodes) && s.nodes.length) {
    const capable = s.nodes.filter(n => n.notify_capable);
    const missing = s.nodes.filter(n => !n.notify_capable).map(n => n.node_id);
    const text = `${capable.length} / ${s.nodes.length}${missing.length ? ` · ${missing.join('、')} 未配置完整` : ''}`;
    rows.push(row('可发送节点', text, { badge: missing.length ? 'sync-warning' : '' }));
  }
  // Rebuild only when the text changed; the gateway uptime ticks per minute.
  const sig = rows.map(r => r.textContent).join('\n');
  if (sig === statusSig) return;
  statusSig = sig;
  box.replaceChildren(...rows);
}

// §16.x 可发送节点: count, then one nowrap span per node gap joined by ；.
function readinessRowEl(r) {
  const wrap = row('可发送节点', '');
  wrap.classList.add('notify-readiness-row');
  const v = wrap.lastChild;
  v.replaceChildren();
  const count = document.createElement('span');
  count.textContent = r.count;
  v.append(count);
  if (r.gaps.length) {
    v.classList.add('notify-gaps');
    v.dataset.tone = 'warn';
    v.append(document.createTextNode(' · '));
    r.gaps.forEach((gap, i) => {
      if (i) v.append(document.createTextNode('；'));
      const span = document.createElement('span');
      span.className = 'notify-gap';
      span.textContent = gap;
      v.append(span);
    });
  }
  return wrap;
}

function credentialRow(s) {
  const wrap = document.createElement('div');
  wrap.className = 'cluster-node-meta-row notify-cred-row';
  const label = document.createElement('span');
  label.className = 'cluster-node-meta-label';
  label.textContent = '凭据同步';
  const list = document.createElement('ul');
  list.className = 'cluster-node-meta-value notify-cred-list';
  const rowsData = credentialRows(s, { pending: pendingUnbind, localUnbound: settings && !settings.openid_set && !openidPending(settings, s), cleared: clearedAt, writerSet: writerOpenidSet(settings, s) });
  for (const r of rowsData) {
    const li = document.createElement('li');
    const name = document.createElement('span');
    name.className = 'notify-cred-node';
    name.textContent = r.local ? `${r.node_id}（本节点）` : r.node_id;
    li.append(name);
    for (const b of [r.secret, r.openid]) {
      if (!b) continue;
      const badge = document.createElement('span');
      badge.className = `cluster-badge${b.tone ? ` ${b.tone}` : ''}`;
      badge.textContent = b.tone === 'sync-warning' && b.text.endsWith('未同步') ? b.text.replace('未同步', '⚠ 未同步') : b.text;
      li.append(badge);
    }
    list.append(li);
  }
  wrap.append(label, list);
  return wrap;
}

function traceRow(id) {
  return row('Trace ID', middleTruncate(id, 16), { title: id, valueClass: 'notify-trace', copy: id });
}

async function copyText(text, done) {
  try {
    await navigator.clipboard.writeText(text);
    showNotification(done, 'success');
  } catch {
    showNotification('复制失败，请手动选择', 'error');
  }
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
    const temp = view.state === 'waiting' && settings?.config?.qq?.keep_online === false ? '将临时连接 QQ 网关，绑定完成后断开。' : '';
    $('notify-bind-text').textContent = temp ? `${view.text}${temp}` : view.text;
    $('notify-bind-cancel').hidden = !view.cancel;
  }
  const waiting = view.state === 'waiting';
  const btn = $('notify-bind-btn');
  if (btn && !btn.classList.contains('is-loading')) {
    btn.textContent = waiting ? '重新生成' : '自动获取';
    const gate = openidGate(status);
    const temp = settings?.config?.qq?.keep_online === false ? '将临时连接 QQ 网关，绑定完成后断开。' : '';
    btn.disabled = busy || !gate.enabled || !settings?.config?.qq?.app_id || !appSecretSet(settings);
    btn.title = !gate.enabled ? gate.hint : btn.disabled ? '请先保存 AppID 和 AppSecret' : `生成配对码，在 QQ 向机器人发送后自动完成绑定${temp ? `。${temp}` : ''}`;
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
  let summary = {
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
    'node-unready': `本节点缺少 ${(st.lacks || []).join(' / ')}，接管发送后将无法送达通知。`,
    'no-sender': '没有配置完整的节点，通知将记为未送达。',
  }[st.key];
  const unbindText = unbindSummary();
  if (unbindText) summary = unbindText;
  const summaryEl = $('notify-summary');
  if (summaryEl.textContent !== (summary || '')) summaryEl.textContent = summary || '';

  let alert = null;   // { lead, text, tone, action }
  if (st.key === 'incomplete') alert = { lead: '本节点无法发送通知。', text: '填写缺少的项目并保存。', tone: 'warn' };
  else if (st.key === 'gateway-failed') {
    alert = { lead: `QQ 网关已停止（${status?.gateway?.reason || '机器人不可用'}），不会自动重连。`, text: '请在 QQ 开放平台检查机器人状态，修改凭据并保存后重试。', tone: 'danger', action: true };
  } else if (st.key === 'gateway-down') alert = { lead: 'QQ 网关未连接，正在重连。', text: '断开期间只能用主动消息或召回额度发送，可能送达失败。', tone: 'warn' };
  else if (st.key === 'node-unready') {
    const lacks = st.lacks || [];
    const fix = lacks.includes('AppSecret') ? '请在本节点填写 AppSecret。'
      : lacks.includes('AppID') ? '请在本节点填写 AppID。'
      : 'openid 正在同步；如长时间未同步，请重新保存或「自动获取」。';
    alert = { lead: '本节点未配置完整。', text: `如果它成为发送节点，通知会记为未送达。${fix}`, tone: 'warn' };
  } else if (st.key === 'no-sender') {
    const n = Number(status?.undelivered) || 0;
    alert = { lead: '所有节点都缺少凭据，通知无法发送。', text: `已记为未送达 ${n} 条。请在至少一个节点填写 AppSecret 并绑定管理员 openid。`, tone: n > 0 ? 'danger' : 'warn' };
  } else if (st.key === 'failed') {
    const out = senderOutcome(status) || {};
    const bad = isBadCredentials(status);
    const code = bad && !out.code ? (localOutcome(status)?.code || 100016) : out.code;
    const [short, hint] = errorCopy(code, out.reason || status?.last_reason?.reason);
    alert = { lead: `${short}${code ? `（${code}）` : ''}。`, text: `通知无法送达，转播不受影响。${hint}`, tone: 'danger', action: bad };
  }
  if (refusalSrc) {
    refusal = liveRefusal(refusalSrc.error, refusalSrc.route, status, statusSeq > refusalSrc.seq);
    if (!refusal) refusalSrc = null;
  }
  if (refusal) alert = refusal.banner;
  renderAlert(alert);

  const secretSet = appSecretSet(settings);
  setBadge($('notify-app-secret-state'), settings ? (secretSet ? '已设置' : '未设置') : '读取中', secretSet ? 'active' : '');
  $('notify-clear-app-secret').hidden = !secretSet;
  const secretInput = $('notify-app-secret');
  secretInput.placeholder = secretSet ? '已设置（留空则不变）' : '填写 AppSecret';
  secretInput.title = secretSet ? '已保存；输入新值后保存可替换' : '';

  // 管理员 openid: write-only (§16.2). The value never comes from the API.
  const openidSet = Boolean(settings?.openid_set);
  const ov = openidView(settings, status);
  setBadge($('notify-openid-state'), ov.badge, ov.tone);
  $('notify-clear-openid').hidden = !openidSet;
  const openidInput = $('notify-admin-openid');
  openidInput.placeholder = openidSet ? '已绑定（留空则不变）' : '点「自动获取」或填写 openid';
  openidInput.title = openidSet ? '已保存；输入新值后保存可替换' : '';
  const gate = openidGate(status);
  const ownerHint = $('notify-openid-owner');
  // §16.x: while pending, the sync hint replaces 「请在 X 上绑定或解绑…」, and
  // 自动获取 (with its options) is hidden; nothing needs doing on this node.
  const ownerText = ov.pending ? ov.hint : gate.hint;
  if (ownerHint) {
    if (ownerHint.textContent !== ownerText) ownerHint.textContent = ownerText;
    ownerHint.hidden = !ownerText;
    ownerHint.dataset.tone = ov.pending ? 'info' : 'warn';
  }
  for (const id of ['notify-bind-btn', 'notify-bind-any-row']) { const el = $(id); if (el) el.hidden = ov.pending; }
  const meta = $('notify-openid-meta');
  const metaText = typedOpenid() && openidSet ? '保存后替换当前绑定'
    : openidSet && settings?.openid_bound_at ? `绑定于 ${formatUtc8(settings.openid_bound_at)}` : '';
  meta.textContent = metaText;
  meta.hidden = !metaText;

  const disabled = !settings || busy;
  for (const id of ['notify-enabled', 'notify-app-secret', 'notify-admin-openid', 'notify-clear-openid', 'notify-commands', 'notify-live-events', 'notify-watch-enabled', 'notify-bind-any', ...Object.values(FIELDS)]) {
    const el = $(id);
    if (el) el.disabled = disabled;
  }
  for (const id of ['notify-admin-openid', 'notify-clear-openid']) {
    const el = $(id);
    if (!gate.enabled) el.disabled = true;
    el.title = gate.enabled ? (id === 'notify-admin-openid' ? openidInput.title : '') : gate.hint;
  }
  const dirty = isDirty();
  $('notify-dirty').hidden = !dirty;
  const save = $('notify-save-btn');
  if (!save.classList.contains('is-loading')) save.disabled = disabled || !dirty;
  const test = $('notify-test-btn');
  const canTest = settings?.config?.qq?.app_id && secretSet && openidSet;
  if (!test.classList.contains('is-loading')) {
    test.disabled = disabled || !canTest;
    test.title = canTest ? '向管理员发送一条测试私信，会占用共享发送额度' : '请先保存 AppID、AppSecret 和管理员 openid';
  }
  renderStatus();
  renderLiveEvents();
  renderBind();
  renderWatch();
}

function renderAlert(alert) {
  const el = $('notify-alert');
  const key = alert ? JSON.stringify(alert) : '';
  if (key === lastAlert) return;
  lastAlert = key;
  el.hidden = !alert;
  if (!alert) { el.replaceChildren(); return; }
  el.dataset.tone = alert.tone;
  const lead = document.createElement('strong');
  lead.textContent = alert.lead;
  const parts = [lead, document.createTextNode(alert.text)];
  if (alert.action) {
    const btn = document.createElement('button');
    btn.type = 'button';
    btn.className = 'btn-ghost compact-btn notify-alert-action';
    const action = alert.action === true ? { label: '重新填写', target: 'notify-app-secret' } : alert.action;
    btn.textContent = action.label;
    btn.addEventListener('click', () => {
      const input = actionTarget(action.target);
      input?.scrollIntoView({ block: 'center' });
      input?.focus();
    });
    parts.push(btn);
  }
  el.replaceChildren(...parts);
}

function actionTarget(target) {
  if (target === 'credentials') return !$('notify-app-id').value.trim() ? $('notify-app-id') : $('notify-app-secret');
  return $(target);
}

function setResult(text) {
  const el = $('notify-action-result');
  if (el) el.textContent = text;
}

export async function loadNotifySettings() {
  if (!$('notify-settings')) return;
  const token = settingsReq.next();
  const stToken = statusReq.next();
  loading = true;
  render();
  try {
    const [config, st] = await Promise.all([
      getJson('/api/notify'),
      getJson('/api/notify/status').catch(() => null),
    ]);
    // L1: a newer load has started; this response is stale.
    if (!settingsReq.isCurrent(token)) return;
    applySettings(config.data, { force: !isDirty() });
    setStatus(st?.data, stToken);
    loadError = null;
  } catch (error) {
    if (settingsReq.isCurrent(token)) loadError = error;
  } finally {
    if (settingsReq.isCurrent(token)) loading = false;
    render();
  }
}

// R1: only take a status newer than the one we have, and remember its sequence.
function setStatus(data, token) {
  if (!data || token < statusSeq) return false;
  status = data;
  statusAt = Date.now();
  statusSeq = token;
  return true;
}

function applySettings(data, { force }) {
  const previous = baseline;
  settings = data;
  baseline = configToForm(data.config);
  if (force || !previous) fillForm(baseline);
  else {
    // Keep the draft but take server-side changes (e.g. a bound openid).
    const keys = [...Object.keys(FIELDS), 'watch_channels'];
    const draft = mergeDraft(previous, baseline, readForm(), keys);
    for (const key of Object.keys(FIELDS)) if ($(FIELDS[key]).value !== draft[key]) $(FIELDS[key]).value = draft[key];
    if (draft.watch_channels !== JSON.stringify(watchDraft)) watchDraft = JSON.parse(draft.watch_channels);
  }
}

export async function refreshNotifyStatus() {
  if (!$('notify-settings') || !settings) return;
  try {
    const stToken = statusReq.next();
    const st = await getJson('/api/notify/status');
    if (!setStatus(st.data, stToken)) return;
    updateUnbindProgress();
    const bindState = status?.bind?.state || 'idle';
    if (bindState === 'bound' && lastBindState === 'waiting') {
      showNotification('已获取管理员 openid', 'success');
      clearRefusal();
      const token = settingsReq.next();
      const config = await getJson('/api/notify');
      if (settingsReq.isCurrent(token)) applySettings(config.data, { force: false });
    }
    lastBindState = bindState;
  } catch (error) {
    // Keep the last status; the next poll retries. Review B1: still redraw
    // the budget line so it falls back to — once the status is 90 s old.
    renderLiveEvents();
    return;
  }
  render();
}

async function save() {
  const form = readForm();
  const secret = $('notify-app-secret').value.trim();
  const openid = typedOpenid();
  const { errors, warnings } = validateNow();
  showFieldErrors(errors, warnings);
  if (Object.keys(errors).length) {
    const first = Object.keys(errors)[0];
    (first === 'admin_openid' ? $('notify-admin-openid') : $(FIELDS[first] || FIELDS.token_url))?.focus();
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
  if (openid) {
    body.admin_openid = openid;
    body.expected_openid_revision = settings.openid_revision;
  }
  busy = true;
  const btn = $('notify-save-btn');
  btn.textContent = '正在保存…';
  btn.setAttribute('aria-busy', 'true');
  setButtonLoading(btn, null, true);
  try {
    const result = await putJson('/api/notify', body);
    clearRefusal();
    liveEventsError = '';
    $('notify-app-secret').value = '';
    applySettings(result.data, { force: true });
    if (openid) { pendingUnbind = new Set(); clearedAt.clear(); }
    showNotification(openid ? '管理员 openid 已绑定' : '通知设置已保存', 'success');
    setResult('');
  } catch (error) {
    handleRefusal(error, openid ? 'openid' : 'config');
    liveEventsError = liveEventsFieldError(error);
    if (liveEventsError) $('notify-live-events')?.focus();
    showNotification(`通知设置保存失败: ${refusal ? refusal.banner.lead : error.message}`, 'error');
  } finally {
    // §16.4: the typed openid never outlives the request, success or not.
    $('notify-admin-openid').value = '';
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
    clearRefusal();
    applySettings(result.data, { force: !isDirty() });
    showNotification('AppSecret 已清除', 'success');
  } catch (error) {
    handleRefusal(error, 'config');
    showNotification(`通知设置保存失败: ${refusal ? refusal.banner.lead : error.message}`, 'error');
  } finally {
    busy = false;
    render();
    $('notify-app-secret').focus();
  }
}

// Coded refusal → banner, refresh, recovery focus (§16.11). The typed openid
// is cleared in every case; other dirty fields stay.
// Recovery buttons move focus only; nothing is unbound or saved by them.
// loadNotifySettings keeps dirty form edits (force only when clean).
function handleRefusal(error, route = 'openid') {
  refusal = notifyRefusal(error, status, { route });
  // R1: statuses from requests started up to now are not fresh for this refusal.
  // L3: snapshot the credentials so a credential hint survives unrelated edits.
  refusalSrc = refusal ? { error, route, seq: statusReq.last, creds: readCreds() } : null;
  if (!refusal) return;
  $('notify-admin-openid').value = '';
  if (refusal.kind === 'revision') loadNotifySettings();
  else refreshNotifyStatus();
}

// Unbind on all nodes (§16.9). The route is the contract's DELETE; the body
// carries the revision so a concurrent bind wins over a stale unbind.
const UNBIND = { method: deleteJson, path: '/api/notify/openid' };

async function unbindOpenid() {
  if (!confirm('确定要在所有节点上解除管理员 openid 绑定吗？解除后所有节点都无法发送通知，需要重新「自动获取」或填写。离线的节点会在上线后清除。')) return;
  const clustered = hasNodeCredentials(status);
  const before = clustered ? status.nodes.filter(n => n.openid_set && !n.is_local).map(n => n.node_id) : [];
  busy = true;
  render();
  try {
    const { merged } = await unbindFlow({
      settings,
      remove: body => UNBIND.method(UNBIND.path, body),
      reload: loadNotifySettings,
    });
    clearRefusal();
    settings = merged;
    pendingUnbind = new Set(before);
    showNotification(before.length ? '已在本节点解除绑定，正在同步到其他节点…' : '管理员 openid 已解除绑定', 'success');
  } catch (error) {
    handleRefusal(error);
    showNotification(`解除绑定失败: ${refusal ? refusal.banner.lead : error.message}`, 'error');
  } finally {
    busy = false;
    render();
    refreshNotifyStatus();
  }
}

// Track unbind progress from fresh status; returns the summary override.
function updateUnbindProgress() {
  if (!pendingUnbind.size || !hasNodeCredentials(status)) return;
  for (const n of status.nodes) {
    if (pendingUnbind.has(n.node_id) && !n.openid_set) {
      pendingUnbind.delete(n.node_id);
      clearedAt.set(n.node_id, Date.now());
      if (!pendingUnbind.size) unbindDone = true;
    }
  }
}
let unbindDone = false;

function unbindSummary() {
  // §16.x: a pending node is waiting for a bind, not watching an unbind.
  if (!hasNodeCredentials(status) || !settings || settings.openid_set || openidPending(settings, status)) return '';
  const holders = status.nodes.filter(n => n.openid_set && !n.is_local);
  if (!holders.length) {
    if (unbindDone) { unbindDone = false; return '已在所有节点解除绑定'; }
    return '';
  }
  const offline = holders.filter(n => n.fresh === false).length;
  if (offline === holders.length) return `${offline} 个节点离线，上线后会自动清除`;
  const total = status.nodes.length;
  return `正在所有节点解除绑定（${total - holders.length}/${total}）`;
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
    const replace = Boolean(settings?.openid_set);
    if (replace && !confirm('已绑定管理员 openid。重新绑定成功后会替换它，确定吗？')) return;
    const result = await postJson('/api/notify/bind', { mode, replace });
    clearRefusal(); // R2: a new pairing code supersedes an earlier refusal
    status = { ...(status || {}), bind: result.data };
    statusAt = Date.now();
    lastBindState = 'waiting';
    setResult('');
  } catch (error) {
    handleRefusal(error);
    setResult(refusal ? refusal.banner.lead : error.message);
    showNotification(refusal ? refusal.banner.lead : error.message, 'warning');
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
    clearRefusalOnEdit();
    render();
  });
  panel.addEventListener('change', event => {
    if (event.target.id === 'notify-live-events') liveEventsError = '';
    clearRefusalOnEdit();
    render();
  });
  for (const id of Object.values(FIELDS)) {
    $(id)?.addEventListener('blur', () => {
      const { errors, warnings } = validateNow();
      showFieldErrors(errors, warnings);
    });
  }
  $('notify-admin-openid')?.addEventListener('blur', () => {
    const { errors, warnings } = validateNow();
    showFieldErrors(errors, warnings);
  });
  $('notify-clear-openid')?.addEventListener('click', unbindOpenid);
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
  // Review B1: the 90 s stale rule needs a clock, not only successful polls.
  setInterval(() => { if (settings) renderLiveEvents(); }, LIVE_EVENTS_TICK_MS);
  // Same visibility gate as the other 系统设置 timers.
  setInterval(() => {
    if (isViewActive('settings') && !document.hidden) refreshNotifyStatus();
  }, 30_000);
}

export { DEFAULTS };
