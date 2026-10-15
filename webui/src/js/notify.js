// notify.js — QQ 通知 panel in 系统设置. Own endpoint (/api/notify), own save
// button; never part of the /api/config patch.

import { getJson, putJson, postJson, deleteJson } from './api.js';
import { showNotification, setButtonLoading, reconcileChildren } from './dom.js';
import { isViewActive } from './state.js';
import { LIVE_EVENTS_TICK_MS, STATUS_STALE_MS, activePushValueInto, renderLiveEventsInto } from './live-events-view.js';
import {
  WATCH_PLATFORMS, WATCH_MAX, createWatchMigrationAcknowledgements, filterCandidates, groupedWatchSupported,
  toggleWatchCandidate, watchCatalog, watchForSave, watchKey, watchMigrationText, watchRowLive, watchRows, watchSelection,
} from './notify-watch.js';
import {
  DEFAULTS, bindView, configToForm, errorCopy,
  appSecretSet, credentialRows, formToPatch, formatDuration, formatUtc8, hasNodeCredentials, isBadCredentials, localOutcome,
  middleTruncate, openidPending, openidView, readinessRow, writerOpenidSet, isSkipped, liveEventsFieldError, liveEventsGate, pickBudget, skippedRow, pendingLiveNote, liveEventsSupported, withLiveEvents, wakeupBudgetView,
  clearSecretFlow, createSeq, liveRefusal, secretLostRefusal, mergeDraft, refusalSurvivesEdit, unbindFlow, notifyRefusal, openidGate, sameForm, selectState, senderBlockView, senderOutcome, staleLocalFailure, validateForm, nodeSignature,
  credentialsSync, baseGeneration, appIdSet, appIdLine, saveBody, clearSecretBody, clearSecretConfirm, appIdWriteOnly, credentialPending, pendingCredentialBadge, rebindHint, appIdEmpty,
  APPSECRET_HELP_SYNC, APPID_HELP_SYNC, APPID_EMPTY, pendingSettingsForm, LOCAL_SETTINGS_PENDING_TEXT,
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
const watchMigrationAcks = createWatchMigrationAcknowledgements(() => window.localStorage);
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
  const form = readForm();
  const result = validateForm(form, {
    secretConfigured: appSecretSet(settings),
    secretEntered: Boolean($('notify-app-secret')?.value.trim()),
    openidSet: Boolean(settings?.openid_set),
    openidText: typedOpenid(),
    openidPending: openidPending(settings, status),
  });
  // §16.y: under F the AppID is write-only (blank = unchanged), so the
  // 启用前请填写 hint only applies when no AppID exists or is on its way.
  if (appIdSet(settings) || credentialPending(settings, status, 'appid')) delete result.warnings.app_id;
  if (appIdEmpty(settings, $('notify-app-id')?.value, { secretEntered: Boolean($('notify-app-secret')?.value.trim()), enabled: form.enabled, status })) {
    result.errors.app_id = APPID_EMPTY;
  }
  return result;
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
    // F rev 2: 「AppID 不能为空」 is shown in warn, not danger.
    if (text === APPID_EMPTY) el.dataset.tone = 'warn'; else delete el.dataset.tone;
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
  const key = channel.key;
  let item = watchChips.get(key);
  if (!item) {
    item = document.createElement('li');
    item.className = 'notify-watch-item';
    item.dataset.key = key;
    const label = document.createElement('span');
    label.className = 'notify-watch-name';
    const name = document.createElement('span');
    name.className = 'notify-watch-label';
    const badge = document.createElement('span');
    badge.className = 'cluster-badge active notify-watch-live';
    badge.textContent = '直播中';
    label.append(name, ' ', badge);
    const meta = document.createElement('span');
    meta.className = 'notify-watch-meta';
    const remove = document.createElement('button');
    remove.type = 'button';
    remove.className = 'btn-ghost compact-btn notify-watch-remove';
    remove.textContent = '移除';
    remove.addEventListener('click', () => removeFromWatch(key));
    const text = document.createElement('span');
    text.className = 'notify-watch-text';
    text.append(label, meta);
    item.append(text, remove);
    watchChips.set(key, item);
  }
  item.querySelector('.notify-watch-label').textContent = channel.name;
  const platformText = channel.platforms.map(p => WATCH_PLATFORMS[p]).join('、');
  item.querySelector('.notify-watch-meta').textContent = channel.missing
    ? '频道管理中已无此频道，暂不检查；可移除。'
    : channel.unavailable ? '旧关注项未关联频道管理，请先添加对应频道或移除。' : platformText;
  item.dataset.unavailable = String(channel.unavailable);
  const badge = item.querySelector('.notify-watch-live');
  const live = watchRowLive(channel, status);
  badge.hidden = !live && !channel.unavailable;
  badge.classList.toggle('sync-warning', channel.unavailable);
  badge.classList.toggle('active', !channel.unavailable);
  badge.textContent = channel.missing ? '未找到' : channel.unavailable ? '不可用' : '直播中';
  item.querySelector('.notify-watch-remove').setAttribute('aria-label', `从关注列表移除 ${channel.name}${channel.historical ? '（旧关注项）' : ''}`);
  item.querySelector('.notify-watch-remove').disabled = busy || !settings;
  return item;
}

function watchOption(channel) {
  const key = watchKey(channel);
  let li = watchOptions.get(key);
  if (!li) {
    li = document.createElement('li');
    li.dataset.key = key;
    const label = document.createElement('label');
    label.className = 'notify-watch-option';
    const input = document.createElement('input');
    input.type = 'checkbox';
    const text = document.createElement('span');
    text.className = 'notify-watch-text';
    const name = document.createElement('span');
    name.className = 'notify-watch-name';
    const meta = document.createElement('span');
    meta.className = 'notify-watch-meta';
    const selected = document.createElement('span');
    selected.className = 'notify-watch-selected';
    text.append(name, meta);
    label.append(input, text, selected);
    input.addEventListener('change', () => {
      const candidate = watchCatalog(settings).find(c => watchKey(c) === key);
      if (!candidate) return;
      const result = toggleWatchCandidate(watchDraft, candidate, input.checked);
      setWatchError(result.error);
      if (!result.error) {
        watchDraft = result.list;
        clearRefusalOnEdit();
      }
      watchSig = ''; // also restore a refused checkbox without replacing it
      render();
    });
    li.append(label);
    watchOptions.set(key, li);
  }
  const selected = watchSelection(channel, watchDraft);
  const label = li.firstChild;
  const input = label.querySelector('input');
  input.checked = selected.checked && !selected.partial;
  input.indeterminate = selected.partial;
  input.disabled = busy || !settings || (watchDraft.length >= WATCH_MAX && !selected.checked);
  label.classList.toggle('is-selected', selected.checked);
  label.classList.toggle('is-disabled', input.disabled);
  label.querySelector('.notify-watch-name').textContent = channel.name;
  label.querySelector('.notify-watch-meta').textContent = channel.platforms.map(p => WATCH_PLATFORMS[p]).join('、');
  label.querySelector('.notify-watch-selected').textContent = selected.partial ? '部分已关注' : selected.checked ? '已关注' : '';
  return li;
}

// After 移除, keep keyboard users in the list: next chip's 移除, else the
// previous one, else the search box.
function removeFromWatch(key) {
  const rows = watchRows(watchDraft, watchCatalog(settings));
  const index = rows.findIndex(channel => channel.key === key);
  const keys = new Set(rows[index]?.members.map(watchKey) || []);
  watchDraft = watchDraft.filter(c => !keys.has(watchKey(c)));
  watchChips.delete(key);
  setWatchError('');
  clearRefusalOnEdit();
  render();
  const remaining = watchRows(watchDraft, watchCatalog(settings));
  const next = remaining[index] || remaining[index - 1];
  const target = next ? watchChips.get(next.key)?.querySelector('.notify-watch-remove') : $('notify-watch-picker-toggle');
  target?.focus();
}

function setWatchError(error) {
  const errEl = $('notify-watch-error');
  if (errEl) {
    errEl.textContent = error;
    errEl.hidden = !error;
  }
}

function renderWatch() {
  const listEl = $('notify-watch-list');
  if (!listEl) return;
  const disabled = busy || !settings;
  for (const id of ['notify-watch-search', 'notify-watch-search-clear']) {
    const el = $(id);
    if (el) el.disabled = disabled;
  }
  $('notify-watch-picker-toggle')?.setAttribute('aria-disabled', String(disabled));
  // Only rebuild when something the list shows changed (F1): typing in other
  // fields and status polls must not move focus.
  const query = $('notify-watch-search')?.value || '';
  const catalog = watchCatalog(settings);
  const migration = watchMigrationText(settings);
  const migrationDismissed = watchMigrationAcks.isDismissed(status?.node_id, settings?.config?.watch?.migration_notice);
  const sig = JSON.stringify([watchDraft, catalog, query, status?.watch, disabled, migration, migrationDismissed, groupedWatchSupported(settings)]);
  if (sig === watchSig) return;
  watchSig = sig;
  const rows = watchRows(watchDraft, catalog);
  reconcileChildren(listEl, rows.map(watchChip));
  const rowKeys = new Set(rows.map(c => c.key));
  for (const key of watchChips.keys()) if (!rowKeys.has(key)) watchChips.delete(key);
  const catalogKeys = new Set(catalog.map(watchKey));
  for (const key of watchOptions.keys()) if (!catalogKeys.has(key)) watchOptions.delete(key);
  const empty = $('notify-watch-empty');
  if (empty) empty.hidden = rows.length > 0;
  const count = $('notify-watch-count');
  setBadge(count, `已关注 ${rows.length} / ${WATCH_MAX}`, watchDraft.length >= WATCH_MAX ? 'sync-warning' : '');
  $('notify-watch-help').textContent = groupedWatchSupported(settings)
    ? '按频道关注，自动涵盖已关联的平台。同一场直播只通知一次，不占用召回额度。'
    : '按频道名称选择关注项。当前节点暂不支持跨平台合并通知。';
  $('notify-watch-search-clear').hidden = !query;
  $('notify-watch-migration-text').textContent = migration;
  $('notify-watch-migration').hidden = !migration || migrationDismissed;
  const results = $('notify-watch-results');
  if (results) {
    const matches = filterCandidates(catalog, query);
    reconcileChildren(results, matches.map(watchOption));
    const selected = catalog.filter(c => watchSelection(c, watchDraft).checked).length;
    const countText = query.trim() ? `找到 ${matches.length} 个频道` : `共 ${catalog.length} 个频道 · 已选 ${selected} 个`;
    const resultsCount = $('notify-watch-results-count');
    if (resultsCount.textContent !== countText) resultsCount.textContent = countText;
    const none = $('notify-watch-no-results');
    if (none) {
      none.hidden = matches.length > 0;
      none.textContent = !settings ? '正在读取频道列表…' : !catalog.length
        ? '还没有可关注的频道，请先在频道管理中添加频道。'
        : '没有匹配的频道，可清空搜索，或先在频道管理中添加频道。';
    }
  }
}

// §17.3: budget line, next-close segment and passive hint. Only text nodes
// and attributes change, and only when they differ (no flicker, B3).
function renderLiveEvents() {
  renderLiveEventsInto(document, {
    group: $('notify-types'),
    line: $('notify-wakeup-budget'), main: $('notify-wakeup-main'), next: $('notify-wakeup-next'),
    passive: $('notify-passive-hint'), error: $('notify-live-events-error'), owner: $('notify-live-events-owner'),
    input: $('notify-live-events'), help: $('notify-live-events-help'),
  }, { status, settings, statusAt, now: Date.now(), error: liveEventsError, supported: liveEventsSupported(settings?.config), disabled: !settings || busy });
}

function renderStatus() {
  const box = $('notify-status');
  if (!box) return;
  const rows = [];
  const s = status || {};
  const node = s.node_id || '本节点';
  const now = Date.now() / 1000;
  if (s.authority === 'primary' || s.authority === 'delegate') rows.push(row('发送节点', `${node}（本节点）`));
  else if (s.authority === 'no_quorum_fallback') rows.push(row('发送节点', `${node}（无多数派）`));
  else if (s.authority_node_id) rows.push(row('发送节点', s.authority_node_id));
  else rows.push(row('发送节点', '暂无（等待选举）'));

  const gw = s.gateway || {};
  const enabled = Boolean(settings?.config?.enabled);
  const keepOnline = settings?.config?.qq?.keep_online !== false;
  if (gw.state === 'failed') rows.push(row('QQ 网关', `已停止 · ${gw.reason || '机器人不可用'}`, { dot: 'notify-dot-danger', title: '修改凭据并保存后重试' }));
  else if (gw.state === 'online') rows.push(row('QQ 网关', `已连接 · ${formatDuration(now - gw.since)}`, { dot: 'status-live' }));
  else if (gw.state === 'connecting' || gw.state === 'offline') rows.push(row('QQ 网关', '未连接 · 正在重连', { dot: 'status-scheduled', title: gw.reason || '' }));
  else if (s.authority !== 'primary' && s.authority !== 'delegate' && s.authority_node_id) rows.push(row('QQ 网关', '由发送节点保持'));
  else if (!enabled) rows.push(row('QQ 网关', '未启用'));
  else if (!keepOnline) rows.push(row('QQ 网关', '已关闭（未保持在线）'));
  else rows.push(row('QQ 网关', '未连接', { dot: 'status-offline' }));

  const out = senderOutcome(s);
  const modeText = { passive: '被动回复', active: '主动消息', wakeup: '召回消息' };
  rows.push(row('送达方式', out?.ok ? (modeText[out.mode] || '—') : '—'));
  // §17.8: 主动推送 after 送达方式; no row when the backend has no active_push.
  const push = activePushValueInto(document, s, { available: Boolean(status) && Date.now() - statusAt < STATUS_STALE_MS });
  if (push) {
    const r = row('主动推送', '');
    r.lastChild.replaceWith(push);
    rows.push(r);
  }
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
  // Rebuild only when the rendered rows change (text, tone, class or title);
  // the 10 s tick re-runs this but leaves the DOM alone when nothing moved.
  const sig = rows.map(nodeSignature).join('\n');
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
    // F rev 3: gaps made only of *_pending codes are neutral 待同步.
    if (r.warn) v.dataset.tone = 'warn';
    v.append(document.createTextNode(' · '));
    // B1: one nowrap span per code (the first carries the node name), joined
    // by 、 text so a multi-code gap wraps instead of widening the panel.
    r.segments.forEach((g, i) => {
      if (i) v.append(document.createTextNode('；'));
      g.parts.forEach((part, j) => {
        if (j) v.append(document.createTextNode('、'));
        const span = document.createElement('span');
        span.className = 'notify-gap';
        span.textContent = j ? part : `${g.node} ${part}`;
        v.append(span);
      });
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
    // N3: a silent node is muted and marked 离线 (§16.6).
    if (r.offline) {
      li.classList.add('is-offline');
      const off = document.createElement('span');
      off.className = 'cluster-badge notify-cred-offline';
      off.textContent = '离线';
      li.append(off);
    }
    for (const b of [r.appid, r.secret, r.openid]) {
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
    btn.disabled = busy || !gate.enabled || !appIdSet(settings) || !appSecretSet(settings);
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
  const senderBlock = senderBlockView(status);
  let summary = {
    loading: '正在读取通知设置…',
    'load-error': `无法读取通知设置：${loadError?.message || ''}`,
    off: '未启用。填写机器人凭据并开启后，集群节点故障时会私信管理员。',
    'off-configured': '通知已关闭，凭据已保存。',
    incomplete: `还缺少：${(st.missing || []).join('、')}。`,
    sender: `本节点 ${node} 是当前发送节点，QQ 网关已连接。`,
    'gateway-down': `本节点 ${node} 是当前发送节点，但 QQ 网关未连接。`,
    standby: `当前由 ${status?.authority_node_id} 发送通知；本节点 ${node} 待命，接管后自动发送。`,
    'gateway-failed': `QQ 网关已停止：${status?.gateway?.reason || ''}`,
    fallback: `集群暂无多数派，本节点 ${node} 发送本节点的异常通知，消息会说明「无多数派」。`,
    electing: senderBlock ? `${senderBlock.reason}，暂时没有发送节点。` : '暂无发送节点，通知会在选出后补发。',
    failed: ['primary', 'delegate'].includes(status?.authority) ? `本节点 ${node} 是当前发送节点。` : `本节点 ${node}。`,
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
    const fix = lacks.includes('管理员 openid（需重新绑定）') ? rebindHint(status, settings)
      : lacks.includes('AppSecret') ? '请在本节点填写 AppSecret。'
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
  if (settings?.local_settings_pending === true) alert = { lead: LOCAL_SETTINGS_PENDING_TEXT, text: '', tone: 'warn' };
  if (refusalSrc) {
    refusal = secretLostRefusal(liveRefusal(refusalSrc.error, refusalSrc.route, status, statusSeq > refusalSrc.seq), refusalSrc.secretTyped);
    if (!refusal) refusalSrc = null;
  }
  if (refusal) alert = refusal.banner;
  renderAlert(alert);

  const secretSet = appSecretSet(settings);
  const sync = credentialsSync(settings);
  // §16.y: a synced value on its way is a neutral 待同步 badge, never 未设置.
  const secretPending = pendingCredentialBadge(settings, status, 'appsecret');
  if (secretPending) setBadge($('notify-app-secret-state'), secretPending.badge, secretPending.tone);
  else setBadge($('notify-app-secret-state'), settings ? (secretSet ? '已设置' : '未设置') : '读取中', secretSet ? 'active' : '');
  const appIdState = $('notify-app-id-state');
  if (appIdState) {
    const pend = pendingCredentialBadge(settings, status, 'appid');
    if (sync) setBadge(appIdState, pend ? pend.badge : appIdSet(settings) ? '已设置' : '未设置', pend ? pend.tone : appIdSet(settings) ? 'active' : '');
    appIdState.hidden = !sync;
  }
  const appIdInput = $('notify-app-id');
  const writeOnly = appIdWriteOnly(settings);
  const appIdPlaceholder = !writeOnly ? 'QQ 开放平台的机器人 AppID' : appIdSet(settings) ? '已设置（留空则不变）' : '填写 AppID';
  // F rev 4: no hint may mean a saved short AppID, so read the set flag too.
  const current = $('notify-app-id-current');
  if (current && settings) {
    const line = appIdLine(settings);
    const key = line.text;
    if (current.dataset.key !== key || !current.childNodes.length) {
      current.dataset.key = key;
      const tail = document.createElement('span');
      tail.className = line.mono ? 'notify-appid-tail' : '';
      tail.textContent = line.text;
      current.replaceChildren(document.createTextNode('当前 AppID：'), tail);
    }
    // §16.y rev 3: no hint line at all while this node's AppID is syncing.
    // N2: an older backend shows the AppID in the input itself.
    current.hidden = !writeOnly || credentialPending(settings, status, 'appid');
  }
  if (appIdInput.placeholder !== appIdPlaceholder) appIdInput.placeholder = appIdPlaceholder;
  const appIdHelp = $('notify-app-id-help');
  if (appIdHelp) { appIdHelp.hidden = !sync; appIdHelp.textContent = sync ? APPID_HELP_SYNC : ''; }
  const secretHelp = $('notify-app-secret-help');
  if (secretHelp) {
    secretHelp.dataset.help ??= secretHelp.textContent;
    const text = sync ? APPSECRET_HELP_SYNC : secretHelp.dataset.help;
    if (secretHelp.textContent !== text) secretHelp.textContent = text;
  }
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
  // §16.y: rebind shows its own warn hint; the bind controls stay visible
  // and the writer gate still decides whether they are enabled here.
  const ownerText = ov.pending || ov.rebind ? ov.hint : gate.hint;
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
  const canTest = appIdSet(settings) && secretSet && openidSet;
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
  if (alert.note) {
    const note = document.createElement('div');
    note.className = 'notify-alert-note';
    note.textContent = alert.note;
    parts.push(note);
  }
  if (alert.action) {
    const btn = document.createElement('button');
    btn.type = 'button';
    btn.className = 'btn-ghost compact-btn notify-alert-action';
    const action = alert.action === true ? { label: '重新填写', target: 'notify-app-secret' } : alert.action;
    btn.textContent = action.label;
    btn.addEventListener('click', () => {
      // §16.y 刷新: reload settings (typed fields are kept), never re-save.
      if (action.target === 'refresh') { clearRefusal(); loadNotifySettings(); return; }
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
  if (target === 'watch-remove') return $('notify-watch-list')?.querySelector('.notify-watch-remove') || $('notify-watch-picker-toggle');
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

function applySettings(data, { force, keepCatalog = false }) {
  // A GET that captured settings before a successful save must not restore the
  // old watch list or migration notice. Newer reads (and legacy replies without
  // a revision) remain eligible; request sequencing still orders equal versions.
  if (Number.isSafeInteger(data.revision) && Number.isSafeInteger(settings?.revision)
    && data.revision < settings.revision) return;
  const previous = baseline;
  // Older PUT responses omit the read-only picker catalog. A mutation keeps
  // the loaded catalog; a fresh GET can still change the reported capability.
  if (keepCatalog) {
    for (const key of ['configured_channels', 'watch_candidates']) {
      if (!(key in data) && Array.isArray(settings?.[key])) data = { ...data, [key]: settings[key] };
    }
  }
  settings = data;
  baseline = configToForm(data.config);
  // F rev 3: the AppID is write-only; its value never goes in the input.
  // N2: only when the backend reports appid_set; older backends unchanged.
  if (appIdWriteOnly(data)) baseline.app_id = '';
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
    // the budget line (and 主动推送) so they fall back to — once the status is 90 s old.
    renderStatus();
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
  // One PUT for the whole save (F rev 2: all-or-nothing), base_generation
  // on every save whenever the backend reports a generation.
  const patch = formToPatch(form, baseline);
  if (patch.watch) {
    const encoded = watchForSave(watchDraft, settings);
    if (encoded.error) {
      setWatchError(encoded.error);
      $('notify-watch-picker-toggle')?.focus();
      return;
    }
    patch.watch.channels = encoded.list;
  }
  const body = saveBody(settings, patch, { secret, openid });
  busy = true;
  render();
  const btn = $('notify-save-btn');
  btn.textContent = '正在保存…';
  btn.setAttribute('aria-busy', 'true');
  setButtonLoading(btn, null, true);
  try {
    const result = await putJson('/api/notify', body);
    clearRefusal();
    liveEventsError = '';
    setWatchError('');
    $('notify-app-secret').value = '';
    const pendingDraft = pendingSettingsForm(result.data, baseline, form);
    applySettings(result.data, { force: true, keepCatalog: true });
    if (pendingDraft) fillForm(pendingDraft);
    if (openid) { pendingUnbind = new Set(); clearedAt.clear(); }
    showNotification(pendingDraft ? LOCAL_SETTINGS_PENDING_TEXT : openid ? '管理员 openid 已绑定' : '通知设置已保存', pendingDraft ? 'warning' : 'success');
    setResult('');
  } catch (error) {
    // This is the settings PUT even when it also carries an openid. Only
    // the dedicated bind route keeps legacy uncoded-503 behavior.
    handleRefusal(error, 'config');
    liveEventsError = liveEventsFieldError(error);
    if (liveEventsError) $('notify-live-events')?.focus();
    showNotification(`通知设置保存失败: ${refusal ? refusal.banner.lead : error.message}`, 'error');
  } finally {
    // §16.4: the typed openid never outlives the request, success or not.
    $('notify-admin-openid').value = '';
    // §16.y: nor the typed AppSecret (cleared after every request).
    $('notify-app-secret').value = '';
    busy = false;
    btn.textContent = '保存通知设置';
    btn.removeAttribute('aria-busy');
    setButtonLoading(btn, null, false);
    render();
    refreshNotifyStatus();
  }
}

async function clearSecret() {
  if (!confirm(clearSecretConfirm(settings))) return;
  await clearSecretFlow({
    setBusy: value => { busy = value; },
    render,
    request: () => putJson('/api/notify', clearSecretBody(settings)),
    onSuccess: result => {
      clearRefusal();
      applySettings(result.data, { force: !isDirty(), keepCatalog: true });
      showNotification('AppSecret 已清除', 'success');
    },
    onError: error => {
      handleRefusal(error, 'config');
      showNotification(`通知设置保存失败: ${refusal ? refusal.banner.lead : error.message}`, 'error');
    },
    secretInput: () => $('notify-app-secret'),
  });
}

// Coded refusal → banner, refresh, recovery focus (§16.11). The typed openid
// is cleared in every case; other dirty fields stay.
// Recovery buttons move focus only; nothing is unbound or saved by them.
// loadNotifySettings keeps dirty form edits (force only when clean).
function handleRefusal(error, route = 'openid') {
  const secretTyped = Boolean(($('notify-app-secret')?.value || '').trim());
  refusal = secretLostRefusal(notifyRefusal(error, status, { route }), secretTyped);
  // R1: statuses from requests started up to now are not fresh for this refusal.
  // L3: snapshot the credentials so a credential hint survives unrelated edits.
  refusalSrc = refusal ? { error, route, seq: statusReq.last, creds: readCreds(), secretTyped } : null;
  if (!refusal) return;
  $('notify-admin-openid').value = '';
  // L1: the AppSecret field is cleared either way; focus moves only when one was typed.
  $('notify-app-secret').value = '';
  if (refusal.focus) queueMicrotask(() => actionTarget(refusal?.focus || '')?.focus());
  if (refusal.kind === 'revision' || error?.body?.data?.code === 'watch_unknown_channel') loadNotifySettings();
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

let liveTick = null;
let liveVisibilityHooked = false;
export function liveTickActive() {
  return Boolean(settings) && isViewActive('settings') && !document.hidden;
}
function liveTickFn() { if (liveTickActive()) { renderStatus(); renderLiveEvents(); } }

export function initNotifySettings() {
  const panel = $('notify-settings');
  if (!panel) return;
  panel.addEventListener('input', event => {
    if (event.target.id === 'notify-bind-any' || event.target.closest('.notify-watch-picker')) return;
    clearRefusalOnEdit();
    render();
  });
  panel.addEventListener('change', event => {
    if (event.target.closest('.notify-watch-picker')) return;
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
  $('notify-watch-search-clear')?.addEventListener('click', () => {
    $('notify-watch-search').value = '';
    renderWatch();
    $('notify-watch-search').focus();
  });
  $('notify-watch-picker-toggle')?.addEventListener('click', event => {
    if (busy || !settings) event.preventDefault();
  });
  $('notify-watch-picker')?.addEventListener('keydown', event => {
    if (event.key !== 'Escape' || !$('notify-watch-picker').open) return;
    event.preventDefault();
    event.stopPropagation();
    $('notify-watch-picker').open = false;
    $('notify-watch-picker-toggle').focus();
  });
  $('notify-watch-search')?.addEventListener('keydown', event => {
    if (event.key !== 'ArrowDown') return;
    const first = $('notify-watch-results').querySelector('input:not(:disabled)');
    if (first) { event.preventDefault(); first.focus(); }
  });
  $('notify-watch-migration-dismiss')?.addEventListener('click', () => {
    watchMigrationAcks.dismiss(status?.node_id, settings?.config?.watch?.migration_notice);
    renderWatch();
    $('notify-watch-picker-toggle').focus();
  });
  $('notify-refresh-btn')?.addEventListener('click', async () => {
    const icon = $('notify-refresh-icon');
    icon?.classList.add('is-spinning');
    await (settings ? refreshNotifyStatus() : loadNotifySettings());
    icon?.classList.remove('is-spinning');
  });
  // Review B1: the 90 s stale rule needs a clock, not only successful polls.
  // Clerk L1: one handle, cleared on re-init, and paused (skipped) while the
  // tab is hidden or 系统设置 is not the active view.
  if (liveTick) clearInterval(liveTick);
  liveTick = setInterval(liveTickFn, LIVE_EVENTS_TICK_MS);
  if (!liveVisibilityHooked) {
    liveVisibilityHooked = true;
    document.addEventListener('visibilitychange', liveTickFn);
  }
  // Same visibility gate as the other 系统设置 timers.
  setInterval(() => {
    if (isViewActive('settings') && !document.hidden) refreshNotifyStatus();
  }, 30_000);
}

export { DEFAULTS };
