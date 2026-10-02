// Managed membership: create/join and add/remove/edit operations. Every
// operation is immediate and independent of Save Settings drafts. Passwords
// are sent once and never stored; operation IDs survive a reload.

import { getJson, postJsonApi } from './api.js';
import { showNotification } from './dom.js';
import { state } from './state.js';
import {
  committedPublicNodeId,
  getLastClusterStatus,
  publicStatusSelection,
  readLocalNodeDescriptor,
  rebaseClusterTopology,
  setClusterMembershipContext,
  syncPublicStatusRoleControls,
} from './cluster.js';
import {
  LIFECYCLE_LABELS,
  describeOperation,
  lifecycleSummary,
  loadTrackedOperations,
  missingOperationIsPending,
  newOperationBlock,
  newOperationId,
  operationCleanupPending,
  operationUnsettled,
  pollDelay,
  leaveBlocked,
  removalConsequences,
  saveTrackedOperations,
  setupAvailability,
  submitFailureIsDefinite,
} from './cluster-membership-state.js';

const element = id => document.getElementById(id);
const operationPath = id => `/api/cluster/membership/operations/${encodeURIComponent(id)}`;

let view = null;
let viewKey = null;
let membersKey = null;
let refreshing = null;
let submitting = false;
let pendingSetup = null;
let tracked = [];
// Live progress for tracked and server-reported operations, keyed by ID.
const runs = new Map();
const notices = [];
const openEditors = new Set();
let removing = null;

function storage() {
  try { return window.localStorage; } catch { return null; }
}

function persist() {
  tracked = saveTrackedOperations(storage(), tracked);
}

function run(id) {
  if (!runs.has(id)) runs.set(id, { status: null, attempt: 0, misses: 0, timer: null, active: false, unknown: false, error: '' });
  return runs.get(id);
}

async function refreshMembership() {
  if (refreshing) return refreshing;
  refreshing = (async () => {
    try {
      const result = await getJson('/api/cluster/membership');
      if (!result?.data) return;
      view = result.data;
      setClusterMembershipContext(view);
      const key = JSON.stringify([view.lifecycle, view.revision, view.digest, view.public_member_id, view.local_revision]);
      if (viewKey !== null && key !== viewKey) await rebaseFromServer();
      viewKey = key;
      const op = view.operation;
      if (op?.operation_id && (operationUnsettled(op) || operationCleanupPending(op))) {
        const progress = run(op.operation_id);
        progress.status = op;
        if (!progress.timer && !progress.active) schedule(op.operation_id, pollDelay(progress.attempt));
      }
      render();
    } catch (error) {
      console.debug('Failed to load cluster membership:', error);
      const summary = element('cluster-membership-summary');
      if (summary && !view) summary.textContent = '无法读取多服务器状态';
    } finally {
      refreshing = null;
    }
  })();
  return refreshing;
}

async function rebaseFromServer() {
  const config = await state.hooks.reloadServerConfig?.();
  if (config?.cluster) rebaseClusterTopology(config.cluster);
}

function schedule(id, delay) {
  const progress = run(id);
  clearTimeout(progress.timer);
  progress.timer = setTimeout(() => { progress.timer = null; void poll(id); }, delay);
}

function trackedEntry(id) {
  return tracked.find(entry => entry.operation_id === id);
}

async function poll(id) {
  if (!runs.has(id)) return;
  const entry = trackedEntry(id) || { operation_id: id };
  const progress = run(id);
  if (progress.active) return;
  progress.active = true;
  try {
    const result = await getJson(operationPath(id));
    progress.misses = 0;
    progress.error = '';
    if (result?.data) progress.status = result.data;
  } catch (error) {
    if (error.status === 404) {
      progress.misses += 1;
      if (entry.self_removal) await refreshMembership();
      if (!missingOperationIsPending(entry, view, progress.misses) && view?.lifecycle !== 'left') progress.unknown = true;
    } else {
      progress.error = error.message || '暂时无法查询';
    }
  } finally {
    progress.active = false;
  }
  if (!runs.has(id)) return;
  if (entry.self_removal && view?.lifecycle === 'left') {
    return settle(id, { ...(progress.status || { kind: 'remove' }), phase: 'completed', terminal: true, pending_node_ids: [], message: '本服务器已离开集群' });
  }
  if (progress.status && !operationUnsettled(progress.status)) {
    if (!operationCleanupPending(progress.status)) return settle(id, progress.status);
    // Retained membership is complete. Continue observing departed cleanup,
    // without keeping an unresolved submission that blocks unrelated edits.
    tracked = tracked.filter(item => item.operation_id !== id);
    persist();
    await refreshMembership();
    state.hooks.refreshClusterStatus?.();
  }
  if (!progress.unknown) schedule(id, pollDelay(progress.attempt++));
  render();
}

function settle(id, status) {
  if (!runs.has(id) && !trackedEntry(id)) return;
  const entry = trackedEntry(id) || {};
  const progress = runs.get(id);
  clearTimeout(progress?.timer);
  runs.delete(id);
  tracked = tracked.filter(item => item.operation_id !== id);
  persist();
  const summary = describeOperation(status, entry);
  notices.unshift({ id, ...summary });
  notices.length = 1;
  showNotification(`${summary.title}：${summary.phase}${summary.message ? `（${summary.message}）` : ''}`, summary.success ? 'success' : 'error');
  render();
  void refreshMembership();
  state.hooks.refreshClusterStatus?.();
}

function track(entry) {
  tracked = [...tracked.filter(item => item.operation_id !== entry.operation_id), entry];
  persist();
}

// The ID is recorded before the request so a reload can ask about it.
async function submitOperation(kind, body, { label = '', selfRemoval = false, clear = [] } = {}) {
  const block = newOperationBlock(view, tracked);
  if (submitting || block) {
    if (block) showNotification(block, 'error');
    return false;
  }
  const id = newOperationId();
  track({ operation_id: id, kind, label, self_removal: selfRemoval, created_at: Date.now() });
  submitting = true;
  render();
  const progress = run(id);
  try {
    const result = await postJsonApi('/api/cluster/membership/operations', {
      operation_id: id, expected_revision: view.revision, kind, ...body,
    }, { replayAfterLogin: false });
    if (result.data) progress.status = result.data;
    if (result.message) showNotification(result.message, 'success');
    schedule(id, 500);
    return true;
  } catch (error) {
    await recoverAfterFailure(id, error, kind);
    return false;
  } finally {
    for (const input of clear) if (input) input.value = '';
    submitting = false;
    render();
  }
}

async function recoverAfterFailure(id, error, kind) {
  const progress = run(id);
  let known = null;
  let lookupFailed = false;
  try {
    known = (await getJson(operationPath(id)))?.data || null;
  } catch (lookup) {
    lookupFailed = lookup.status !== 404;
  }
  if (known) {
    progress.status = known;
    showError(error.message || '操作未完成', kind);
    schedule(id, 500);
    return;
  }
  if (submitFailureIsDefinite(error) && !lookupFailed) {
    tracked = tracked.filter(item => item.operation_id !== id);
    persist();
    runs.delete(id);
    showError(error.message || '操作被拒绝', kind);
    if (error.status === 409) void refreshMembership();
    return;
  }
  progress.error = '无法确认操作是否已被服务器接收，请查询同一操作的状态，不要重新提交。';
  schedule(id, pollDelay(0));
}

function showError(message, kind = 'add') {
  const output = element('cluster-add-error');
  if (output && kind === 'add') output.textContent = message;
  showNotification(message, 'error');
}

async function retryOperation(id) {
  const progress = run(id);
  progress.unknown = false;
  progress.misses = 0;
  try {
    const result = await postJsonApi(`${operationPath(id)}/retry`, {}, { replayAfterLogin: false });
    if (result.data) progress.status = result.data;
  } catch (error) {
    if (error.status !== 404) progress.error = error.message || '重试失败';
  }
  schedule(id, 500);
  render();
}

function dismissOperation(id) {
  clearTimeout(runs.get(id)?.timer);
  runs.delete(id);
  tracked = tracked.filter(item => item.operation_id !== id);
  persist();
  render();
}

async function runSetup(create) {
  const availability = setupAvailability(view);
  if (submitting || !(create ? availability.create : availability.join)) return;
  const descriptor = readLocalNodeDescriptor();
  if (!descriptor.node_id || !descriptor.api_url) {
    showNotification('请先填写本节点 ID 和 API 地址', 'error');
    element(descriptor.node_id ? 'config-cluster-api-url' : 'config-cluster-node-id')?.focus();
    return;
  }
  const question = create
    ? '新建集群后，本服务器成为唯一成员；之后可在此添加其他服务器。继续吗？'
    : '准备加入会停止本服务器的转播，直到被其他服务器添加完成。继续吗？';
  if (!confirm(question)) return;
  const payload = { expected_local_revision: view.local_revision, name: descriptor.name || descriptor.node_id, node_id: descriptor.node_id, api_url: descriptor.api_url, priority: descriptor.priority };
  // An uncertain reply is resumed with the same ID; the server treats it as one request.
  const signature = JSON.stringify([create, payload]);
  if (pendingSetup?.signature !== signature) pendingSetup = { signature, id: newOperationId() };
  submitting = true;
  render();
  try {
    const result = await postJsonApi(create ? '/api/cluster/create' : '/api/cluster/prepare-join', { operation_id: pendingSetup.id, ...payload }, { replayAfterLogin: false });
    pendingSetup = null;
    showNotification(result.message || (create ? '已创建集群' : '已准备加入'), 'success');
  } catch (error) {
    if (submitFailureIsDefinite(error)) pendingSetup = null;
    showNotification(error.message || '操作失败', 'error');
  } finally {
    submitting = false;
    await refreshMembership();
    render();
  }
}

function addServer() {
  const url = element('cluster-add-url');
  const password = element('cluster-add-password');
  const target = (url?.value || '').trim().replace(/\/+$/, '');
  element('cluster-add-error').textContent = '';
  if (!target) { showError('请填写目标服务器地址'); url?.focus(); return; }
  if (!password?.value) { showError('请填写目标服务器的面板密码'); password?.focus(); return; }
  const secret = password.value;
  password.value = '';
  void submitOperation('add', { target_url: target, target_password: secret }, { label: target, clear: [password] })
    .then(started => { if (started && url) url.value = ''; });
}

function memberById(id) {
  return view?.members?.find(member => member.member_id === id);
}

async function leaveCluster() {
  const blocked = leaveBlocked(view, tracked);
  if (submitting || blocked === null || blocked) return;
  if (!confirm('向其他服务器确认本节点是否已被移除？只有每台都确认本节点已不在集群中，本服务器才会关闭全部监控并退出。仍被承认时请改用「移除」。网络不通不会当作已退出。')) return;
  submitting = true;
  const output = element('cluster-leave-error');
  if (output) output.textContent = '';
  render();
  try {
    const result = await postJsonApi('/api/cluster/membership/leave', {}, { replayAfterLogin: false });
    if (result?.success === false) throw new Error(result.message || '退出失败');
    showNotification(result?.message || '本服务器已离开集群', 'success');
    await refreshMembership();
  } catch (error) {
    const message = error.message || '退出失败';
    if (output) output.textContent = message;
    showNotification(message, 'error');
  } finally {
    submitting = false;
    render();
  }
}

function changePublicNode() {
  const nodeId = publicStatusSelection();
  if (nodeId === committedPublicNodeId()) return;
  const member = view?.members?.find(item => item.node_id === nodeId);
  if (nodeId && !member) { showNotification('所选服务器不在当前成员列表中', 'error'); return; }
  const label = member ? (member.name || member.node_id) : '关闭状态页';
  if (!confirm(`将公开状态页和共享 YouTube 索引改到「${label}」？期间所有服务器会暂停转播。`)) return;
  void submitOperation('set_public_node', { public_member_id: member ? member.member_id : null }, { label });
}

// Rendering ------------------------------------------------------------------

function render() {
  if (!element('cluster-membership')) return;
  const lifecycle = view?.lifecycle || '';
  const badge = element('cluster-membership-badge');
  badge.dataset.lifecycle = lifecycle;
  badge.textContent = LIFECYCLE_LABELS[lifecycle] || '读取中';
  element('cluster-membership-summary').textContent = lifecycleSummary(view);

  const availability = setupAvailability(view);
  const setupVisible = !!view && (availability.create || availability.join || !!availability.reason);
  element('cluster-membership-setup').classList.toggle('hidden', !setupVisible);
  element('cluster-membership-setup-hint').classList.toggle('hidden', !setupVisible);
  element('cluster-membership-password-hint').classList.toggle('hidden', !availability.reason);
  const create = element('cluster-create-btn');
  const join = element('cluster-prepare-join-btn');
  create.disabled = submitting || !availability.create;
  create.textContent = lifecycle === 'join_ready' ? '改为新建集群' : '新建集群';
  join.disabled = submitting || !availability.join;
  join.classList.toggle('hidden', lifecycle === 'join_ready');

  const managed = lifecycle === 'managed';
  const block = newOperationBlock(view, tracked);
  const leaveReason = leaveBlocked(view, tracked);
  const leaveWrap = element('cluster-membership-leave');
  const leaveButton = element('cluster-leave-btn');
  leaveWrap?.classList.toggle('hidden', leaveReason === null);
  element('cluster-leave-hint')?.classList.toggle('hidden', leaveReason === null);
  if (leaveButton) {
    leaveButton.disabled = submitting || !!leaveReason;
    leaveButton.title = leaveReason || '';
  }
  element('cluster-member-add').classList.toggle('hidden', !managed);
  element('cluster-add-btn').disabled = submitting || !!block;
  element('cluster-add-btn').title = block || '';
  const roleButton = element('public-status-role-btn');
  if (roleButton) {
    syncPublicStatusRoleControls();
    roleButton.disabled = submitting || !!block || roleButton.dataset.unchanged === 'true';
    roleButton.title = block || '';
  }

  renderOperations();
  renderMembers(block);
}

function operationEntries() {
  const ids = new Set(tracked.map(entry => entry.operation_id));
  for (const [id, progress] of runs) if (progress.status && (operationUnsettled(progress.status) || operationCleanupPending(progress.status))) ids.add(id);
  return [...ids];
}

function renderOperations() {
  const list = element('cluster-operation-list');
  const cards = [];
  for (const id of operationEntries()) {
    const entry = trackedEntry(id) || { operation_id: id };
    const progress = run(id);
    const info = describeOperation(progress.status, entry);
    const card = document.createElement('div');
    card.className = 'cluster-operation';
    card.dataset.operationId = id;
    card.dataset.phase = progress.unknown ? 'unknown' : progress.status?.phase || 'submitting';
    const title = document.createElement('strong');
    title.textContent = info.title;
    const phase = document.createElement('span');
    phase.className = 'cluster-operation-phase';
    phase.textContent = progress.unknown ? '服务器尚未记录此操作' : submitting && !progress.status ? '正在提交' : info.phase;
    card.append(title, phase);
    for (const text of [info.attention, info.cleanup, info.message, progress.error]) {
      if (!text) continue;
      const line = document.createElement('p');
      line.className = 'settings-help-text';
      line.textContent = text;
      card.appendChild(line);
    }
    if (info.coordinator) {
      const diag = document.createElement('small');
      diag.className = 'settings-help-text cluster-operation-diagnostic';
      diag.textContent = `${info.coordinator} · 操作 ${id.slice(0, 8)}`;
      card.appendChild(diag);
    }
    const actions = document.createElement('div');
    actions.className = 'session-actions cluster-operation-actions';
    if (progress.unknown) {
      actions.append(
        actionButton('重新查询', 'btn-secondary', () => { progress.unknown = false; progress.misses = 0; void poll(id); }),
        actionButton('放弃此记录', 'btn-ghost', () => dismissOperation(id)),
      );
    } else if ((info.retryable && (progress.status?.phase === 'needs_attention' || operationCleanupPending(progress.status))) || progress.error) {
      actions.appendChild(actionButton('重试', 'btn-secondary', () => retryOperation(id)));
    }
    if (actions.children.length) card.appendChild(actions);
    cards.push(card);
  }
  for (const notice of notices) {
    if (cards.some(card => card.dataset.operationId === notice.id)) continue;
    const card = document.createElement('div');
    card.className = 'cluster-operation';
    card.dataset.phase = notice.success ? 'completed' : 'aborted';
    const title = document.createElement('strong');
    title.textContent = notice.title;
    const phase = document.createElement('span');
    phase.className = 'cluster-operation-phase';
    phase.textContent = notice.phase;
    card.append(title, phase);
    if (notice.message) {
      const line = document.createElement('p');
      line.className = 'settings-help-text';
      line.textContent = notice.message;
      card.appendChild(line);
    }
    const actions = document.createElement('div');
    actions.className = 'session-actions cluster-operation-actions';
    actions.appendChild(actionButton('关闭', 'btn-ghost', () => { notices.splice(notices.indexOf(notice), 1); render(); }));
    card.appendChild(actions);
    cards.push(card);
  }
  list.replaceChildren(...cards);
}

function actionButton(text, className, onClick) {
  const button = document.createElement('button');
  button.type = 'button';
  button.className = `${className} compact-btn`;
  button.textContent = text;
  button.addEventListener('click', onClick);
  return button;
}

function memberContext() {
  return {
    memberCount: view?.members?.length || 0,
    localMemberId: view?.local?.member_id || '',
    publicMemberId: view?.public_member_id || '',
    activeNodeId: getLastClusterStatus()?.active_owner || '',
  };
}

// Member cards keep their inline drafts; only a membership change redraws them.
function renderMembers(block) {
  const list = element('cluster-member-list');
  const members = view?.lifecycle === 'managed' && Array.isArray(view.members) ? view.members : [];
  const context = memberContext();
  const key = JSON.stringify([members, context]);
  if (key !== membersKey) {
    membersKey = key;
    for (const id of [...openEditors]) if (!memberById(id)) openEditors.delete(id);
    if (removing && !memberById(removing)) removing = null;
    list.replaceChildren(...members.map(member => memberCard(member, context)));
  }
  for (const button of list.querySelectorAll('button[data-membership-submit]')) {
    button.disabled = submitting || !!block;
    button.title = block || '';
  }
}

function memberCard(member, context) {
  const card = document.createElement('div');
  card.className = 'cluster-peer-card cluster-member-card';
  card.dataset.memberId = member.member_id;

  const header = document.createElement('div');
  header.className = 'cluster-peer-card-header';
  const identity = document.createElement('div');
  identity.className = 'cluster-peer-identity';
  const dot = document.createElement('span');
  dot.className = 'cluster-peer-node-dot';
  dot.setAttribute('aria-hidden', 'true');
  const titleBlock = document.createElement('div');
  titleBlock.className = 'cluster-peer-title-block';
  const name = document.createElement('div');
  name.className = 'cluster-peer-name';
  name.textContent = member.name || member.node_id;
  const nodeId = document.createElement('div');
  nodeId.className = 'cluster-peer-id';
  nodeId.textContent = `${member.node_id} · ${member.fingerprint}`;
  nodeId.title = `节点 ID 与身份指纹，不能修改`;
  titleBlock.append(name, nodeId);
  identity.append(dot, titleBlock);

  const tags = document.createElement('div');
  tags.className = 'cluster-peer-header-actions cluster-member-tags';
  const tag = (text, extra = '') => {
    const span = document.createElement('span');
    span.className = `cluster-peer-priority ${extra}`.trim();
    span.textContent = text;
    tags.appendChild(span);
  };
  if (member.member_id === context.localMemberId) tag('本机');
  if (member.node_id === context.activeNodeId) tag('活跃', 'is-active');
  if (member.member_id === context.publicMemberId) tag('状态页 · 索引');
  tag(`优先级 ${member.priority}`);
  header.append(identity, tags);

  const address = document.createElement('div');
  address.className = 'cluster-member-address';
  address.textContent = member.api_url;

  const actions = document.createElement('div');
  actions.className = 'session-actions cluster-member-actions';
  const edit = actionButton(openEditors.has(member.member_id) ? '收起编辑' : '编辑', 'btn-secondary', () => {
    if (openEditors.has(member.member_id)) openEditors.delete(member.member_id); else openEditors.add(member.member_id);
    membersKey = null; render();
  });
  const remove = actionButton('移除', 'btn-secondary cluster-member-remove', () => {
    removing = removing === member.member_id ? null : member.member_id;
    membersKey = null; render();
  });
  actions.append(edit, remove);

  card.append(header, address, actions);
  if (openEditors.has(member.member_id)) card.appendChild(memberEditor(member));
  if (removing === member.member_id) card.appendChild(removalConfirm(member, context));
  return card;
}

function field(labelText, input) {
  const label = document.createElement('label');
  label.className = 'cluster-peer-field';
  const span = document.createElement('span');
  span.textContent = labelText;
  label.append(span, input);
  return label;
}

function memberEditor(member) {
  const form = document.createElement('div');
  form.className = 'cluster-member-editor';
  const fields = document.createElement('div');
  fields.className = 'cluster-peer-fields';
  const name = Object.assign(document.createElement('input'), { type: 'text', value: member.name || '' });
  name.dataset.field = 'name';
  const url = Object.assign(document.createElement('input'), { type: 'text', inputMode: 'url', value: member.api_url || '', spellcheck: false });
  url.dataset.field = 'api_url';
  const priority = Object.assign(document.createElement('input'), { type: 'number', value: String(member.priority ?? 0) });
  priority.dataset.field = 'priority';
  const urlField = field('API 地址', url);
  urlField.classList.add('cluster-peer-field-url');
  const priorityField = field('优先级', priority);
  priorityField.classList.add('cluster-peer-field-priority');
  fields.append(field('名称', name), priorityField, urlField);
  const save = actionButton('保存服务器信息', 'btn-primary', () => {
    const body = {
      target_member_id: member.member_id,
      name: name.value.trim() || member.node_id,
      api_url: url.value.trim().replace(/\/+$/, ''),
      priority: Number.parseInt(priority.value, 10) || 0,
    };
    if (!body.api_url) { showNotification('请填写 API 地址', 'error'); return; }
    if (body.name === member.name && body.api_url === member.api_url && body.priority === member.priority) {
      showNotification('服务器信息没有变化', 'error');
      return;
    }
    void submitOperation('update_node', body, { label: member.name || member.node_id }).then(started => {
      if (started) { openEditors.delete(member.member_id); membersKey = null; render(); }
    });
  });
  save.dataset.membershipSubmit = 'update';
  const note = document.createElement('small');
  note.className = 'settings-help-text';
  note.textContent = '节点 ID 和身份不能修改；需要更换时请移除后重新加入。保存期间所有服务器暂停转播。';
  form.append(fields, note, save);
  return form;
}

function removalConfirm(member, context) {
  const box = document.createElement('div');
  box.className = 'cluster-member-remove-confirm';
  box.setAttribute('role', 'group');
  box.setAttribute('aria-label', `确认移除 ${member.name || member.node_id}`);
  const { lines, needsReplacement } = removalConsequences(member, context);
  const list = document.createElement('ul');
  for (const text of lines) {
    const item = document.createElement('li');
    item.textContent = text;
    list.appendChild(item);
  }
  box.appendChild(list);
  let replacement = null;
  if (needsReplacement) {
    replacement = document.createElement('select');
    replacement.className = 'cluster-member-replacement';
    const placeholder = new Option('请选择接替的服务器', '');
    placeholder.disabled = true;
    replacement.append(placeholder, new Option('关闭公开状态页', 'none'));
    for (const other of view.members.filter(item => item.member_id !== member.member_id)) {
      replacement.append(new Option(other.name || other.node_id, other.member_id));
    }
    replacement.value = '';
    box.appendChild(field('状态页接替服务器', replacement));
  }
  const confirmButton = actionButton(`确认移除 ${member.name || member.node_id}`, 'btn-primary cluster-member-remove-confirm-btn', () => {
    const body = { target_member_id: member.member_id };
    if (needsReplacement) {
      if (!replacement.value) { showNotification('请选择接替状态页的服务器，或选择关闭状态页', 'error'); replacement.focus(); return; }
      body.replacement_public_member_id = replacement.value === 'none' ? null : replacement.value;
    }
    const selfRemoval = member.member_id === context.localMemberId;
    void submitOperation('remove', body, { label: member.name || member.node_id, selfRemoval }).then(started => {
      if (started) { removing = null; membersKey = null; render(); }
    });
  });
  confirmButton.dataset.membershipSubmit = 'remove';
  const cancel = actionButton('取消', 'btn-ghost', () => { removing = null; membersKey = null; render(); });
  const actions = document.createElement('div');
  actions.className = 'session-actions cluster-operation-actions';
  actions.append(confirmButton, cancel);
  box.appendChild(actions);
  return box;
}

function resumeTrackedOperations() {
  tracked = loadTrackedOperations(storage());
  for (const entry of tracked) schedule(entry.operation_id, 0);
}

function initClusterMembership() {
  element('cluster-create-btn')?.addEventListener('click', () => runSetup(true));
  element('cluster-prepare-join-btn')?.addEventListener('click', () => runSetup(false));
  element('cluster-leave-btn')?.addEventListener('click', () => { void leaveCluster(); });
  element('cluster-add-btn')?.addEventListener('click', addServer);
  element('public-status-role-btn')?.addEventListener('click', changePublicNode);
  element('config-public-status-node')?.addEventListener('change', () => render());
  element('cluster-membership-security-link')?.addEventListener('click', event => {
    event.preventDefault();
    const target = element('panel-security-title');
    target?.scrollIntoView({ block: 'center' });
    element('panel-password-new')?.focus({ preventScroll: true });
  });
  document.addEventListener('webui-auth-changed', () => { if (view) void refreshMembership(); });
  resumeTrackedOperations();
}

export { initClusterMembership, refreshMembership, memberById };
