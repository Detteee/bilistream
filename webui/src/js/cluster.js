// cluster.js — multi-server panel, using the shared authenticated API client.

import { isDashboardVisible, readIntegerInput, setInputValue, setCheckboxChecked, setButtonLoading, showNotification, SVG_NS } from './dom.js';
import { createSelectOption, state, syncMonitorTogglesWithClusterRole } from './state.js';
import { getJson, postJsonApi } from './api.js';
import { eventStreamHealthy } from './events.js';
import { createClusterNetwork, updateClusterNetwork } from './cluster-network.js';
import { selfCheckDisplay, clusterNodeUsable, formatClusterNodeStatus, formatClusterHealthReason, ytIndexChip, ytIndexFollowChip, ytIndexPeerLine } from './cluster-health.js';

const clusterRefreshInterval = 3000;

let clusterRefreshIntervalId = null;

let clusterRefreshInFlight = false;
let clusterRefreshQueued = false;
let clusterRequestGeneration = 0;
let clusterMutationInFlight = false;
let clusterConnected = false;
let confirmedAutoFailover = true;

let lastClusterFetchMs = 0;

let renderedClusterNodes = new Map();
let compactClusterNodes = null;
let pendingClusterFocus = null;

// Which node serves the public status page, so the node list can mark it.
let publicStatusNodeId = '';

let clusterSeenTickerId = null;

let lastClusterLocalStateSignature = null;

let lastClusterStatus = null;
let loadedClusterConfig = {};

function acceptClusterConfigBaseline(cluster) {
  loadedClusterConfig = structuredClone(cluster);
}

function getClusterConfigBaseline() {
  return structuredClone(loadedClusterConfig);
}

function setClusterCardFolded(folded) {
  const card = document.getElementById('cluster-card');
  const btn = document.getElementById('clusterFoldBtn');
  if (!card) return;

  card.classList.toggle('cluster-collapsed', folded);
  if (btn) {
    btn.title = folded ? '展开' : '折叠';
    btn.setAttribute('aria-label', folded ? '展开' : '折叠');
    btn.setAttribute('aria-expanded', String(!folded));
  }
  try {
    localStorage.setItem('clusterCardFolded', folded ? '1' : '0');
  } catch (_) { /* storage is optional */ }
}

function initClusterCardFold() {
  let folded = false;
  try {
    folded = localStorage.getItem('clusterCardFolded') === '1';
  } catch (_) { /* storage is optional */ }
  setClusterCardFolded(folded);
}

function toggleClusterCardFold() {
  const card = document.getElementById('cluster-card');
  setClusterCardFolded(!card?.classList.contains('cluster-collapsed'));
}

function createClusterSvg(viewBox, className) {
  const svg = document.createElementNS(SVG_NS, 'svg');
  svg.classList.add(className);
  svg.setAttribute('viewBox', viewBox);
  svg.setAttribute('aria-hidden', 'true');
  return svg;
}

function appendSvgPath(svg, attributes) {
  const path = document.createElementNS(SVG_NS, 'path');
  Object.entries(attributes).forEach(([name, value]) => {
    path.setAttribute(name, value);
  });
  svg.appendChild(path);
  return path;
}

function appendSvgCircle(svg, attributes) {
  const circle = document.createElementNS(SVG_NS, 'circle');
  Object.entries(attributes).forEach(([name, value]) => {
    circle.setAttribute(name, value);
  });
  svg.appendChild(circle);
  return circle;
}

function appendSvgGroup(svg, attributes = {}) {
  const group = document.createElementNS(SVG_NS, 'g');
  Object.entries(attributes).forEach(([name, value]) => {
    group.setAttribute(name, value);
  });
  svg.appendChild(group);
  return group;
}

function createClusterEnabledIcon() {
  const svg = createClusterSvg('0 0 24 24', 'cluster-inline-icon');
  svg.setAttribute('fill', 'none');
  appendSvgPath(svg, {
    d: 'M20 6 9 17l-5-5',
    stroke: 'currentColor',
    'stroke-width': '2.5',
    'stroke-linecap': 'round',
    'stroke-linejoin': 'round'
  });
  return svg;
}

function createClusterDisabledIcon() {
  const svg = createClusterSvg('0 0 24 24', 'cluster-inline-icon');
  svg.setAttribute('fill', 'none');
  appendSvgCircle(svg, {
    cx: '12',
    cy: '12',
    r: '9',
    stroke: 'currentColor',
    'stroke-width': '2'
  });
  appendSvgPath(svg, {
    d: 'M5.64 5.64l12.72 12.72',
    stroke: 'currentColor',
    'stroke-width': '2',
    'stroke-linecap': 'round'
  });
  return svg;
}

function createClusterSyncErrorIcon() {
  const svg = createClusterSvg('0 0 24 24', 'cluster-inline-icon');
  svg.setAttribute('fill', 'none');
  appendSvgPath(svg, {
    d: 'M15.9375 6.11972C17.7862 7.39969 19 9.55585 19 12C19 15.9274 15.866 19.1111 12 19.1111C11.6411 19.1111 11.2885 19.0837 10.9441 19.0307M13.0149 4.96309C12.6836 4.9142 12.3447 4.88889 12 4.88889C8.13401 4.88889 5 8.07264 5 12C5 14.4071 6.17734 16.5349 7.97895 17.8215M13.0149 4.96309L12.4375 4M13.0149 4.96309L12.4375 5.77778M10.9441 19.0307L11.7866 18.2222M10.9441 19.0307L11.5625 20M12 9V12.5M12 14.5V15',
    stroke: 'currentColor',
    'stroke-linecap': 'round',
    'stroke-linejoin': 'round'
  });
  return svg;
}

function createClusterFaultIcon() {
  const svg = createClusterSvg('0 0 24 24', 'cluster-inline-icon');
  svg.setAttribute('fill', 'none');
  appendSvgPath(svg, {
    d: 'M12 4v9',
    stroke: 'currentColor',
    'stroke-width': '2.4',
    'stroke-linecap': 'round'
  });
  appendSvgPath(svg, {
    d: 'M12 17.5v.5',
    stroke: 'currentColor',
    'stroke-width': '2.6',
    'stroke-linecap': 'round'
  });
  return svg;
}

function createClusterSwitchIcon() {
  const svg = createClusterSvg('0 0 24 24', 'cluster-btn-icon');
  svg.setAttribute('fill', 'none');
  appendSvgPath(svg, {
    d: 'M18 10L21 7M21 7L18 4M21 7H7M6 14L3 17M3 17L6 20M3 17H17',
    stroke: 'currentColor',
    'stroke-width': '2',
    'stroke-linecap': 'round',
    'stroke-linejoin': 'round'
  });
  return svg;
}

function createClusterRestartIcon() {
  const svg = createClusterSvg('0 0 24 24', 'cluster-btn-icon');
  svg.setAttribute('fill', 'currentColor');
  appendSvgGroup(svg, { 'stroke-width': '0' });
  appendSvgGroup(svg, { 'stroke-linecap': 'round', 'stroke-linejoin': 'round' });
  const group = appendSvgGroup(svg);
  appendSvgPath(group, {
    d: 'M1,12A11,11,0,0,1,17.882,2.7l1.411-1.41A1,1,0,0,1,21,2V6a1,1,0,0,1-1,1H16a1,1,0,0,1-.707-1.707l1.128-1.128A8.994,8.994,0,0,0,3,12a1,1,0,0,1-2,0Zm21-1a1,1,0,0,0-1,1,9.01,9.01,0,0,1-9,9,8.9,8.9,0,0,1-4.42-1.166l1.127-1.127A1,1,0,0,0,8,17H4a1,1,0,0,0-1,1v4a1,1,0,0,0,.617.924A.987.987,0,0,0,4,23a1,1,0,0,0,.707-.293L6.118,21.3A10.891,10.891,0,0,0,12,23,11.013,11.013,0,0,0,23,12,1,1,0,0,0,22,11Z'
  });
  return svg;
}

function createClusterIcon(iconType) {
  switch (iconType) {
    case 'enabled': return createClusterEnabledIcon();
    case 'disabled': return createClusterDisabledIcon();
    case 'sync-error': return createClusterSyncErrorIcon();
    case 'fault': return createClusterFaultIcon();
    case 'switch': return createClusterSwitchIcon();
    case 'restart': return createClusterRestartIcon();
    default: return createClusterSyncErrorIcon();
  }
}

// Membership topology changes only through membership operations. The form
// re-sends the committed topology unchanged; only a never-clustered server
// may edit its local label through an ordinary save.
const TOPOLOGY_FIELDS = ['enabled', 'node_id', 'node_name', 'public_api_url', 'priority', 'peers'];
const LOCAL_INPUTS = ['config-cluster-node-id', 'config-cluster-node-name', 'config-cluster-api-url', 'config-cluster-priority'];
let membershipLifecycle = null;

function localLabelEditable() {
  return membershipLifecycle === 'standalone' && !loadedClusterConfig.enabled;
}

function setLocalNodeInputs(cluster) {
  setInputValue('config-cluster-node-id', cluster.node_id || '');
  setInputValue('config-cluster-node-name', cluster.node_name || cluster.node_id || '');
  setInputValue('config-cluster-api-url', cluster.public_api_url || '');
  setInputValue('config-cluster-priority', Number.isFinite(cluster.priority) ? cluster.priority : 0);
}

function loadClusterSettings(cluster = {}) {
  acceptClusterConfigBaseline(cluster);
  setCheckboxChecked('config-cluster-sync-channels', !!cluster.sync_monitored_channels);
  setCheckboxChecked('config-cluster-auto-failover', cluster.auto_failover !== false);
  updateClusterAutoFailoverToggle(cluster.auto_failover !== false);
  setLocalNodeInputs(cluster);
  setInputValue('config-cluster-heartbeat', cluster.heartbeat_interval_secs || 5);
  setInputValue('config-cluster-failover-timeout', cluster.failover_timeout_secs || 15);
  setInputValue('config-cluster-lease-ttl', cluster.lease_ttl_secs || 20);
  loadPublicStatusSettings(cluster);
  syncLocalNodeInputs();
}

// Called after a membership change committed elsewhere. Ordinary drafts stay.
function rebaseClusterTopology(cluster = {}) {
  for (const field of TOPOLOGY_FIELDS) {
    loadedClusterConfig[field] = structuredClone(cluster[field]);
  }
  loadedClusterConfig.public_status = {
    ...(loadedClusterConfig.public_status || {}),
    node_id: cluster.public_status?.node_id || '',
  };
  setLocalNodeInputs(cluster);
  renderPublicStatusNodeOptions(loadedClusterConfig, loadedClusterConfig.public_status.node_id);
  syncPublicStatusRoleControls();
  syncLocalNodeInputs();
}

function rebaseClusterDraft(draft) {
  const rebased = structuredClone(draft || {});
  for (const field of TOPOLOGY_FIELDS) rebased[field] = structuredClone(loadedClusterConfig[field]);
  if (rebased.public_status) rebased.public_status.node_id = loadedClusterConfig.public_status?.node_id || '';
  return rebased;
}

function setClusterMembershipContext(view) {
  membershipLifecycle = view?.lifecycle || null;
  syncLocalNodeInputs();
  loadPublicStatusHint();
  syncPublicStatusRoleControls();
}

function syncLocalNodeInputs() {
  const locked = !membershipLifecycle || ['managed', 'pairing'].includes(membershipLifecycle);
  for (const id of LOCAL_INPUTS) {
    const input = document.getElementById(id);
    if (input) input.disabled = locked;
  }
  const hint = document.getElementById('cluster-local-hint');
  if (!hint) return;
  hint.textContent = !membershipLifecycle ? ''
    : locked ? '已加入集群：在下方服务器列表中编辑本服务器的名称、地址和优先级。节点 ID 不能更改。'
    : localLabelEditable() ? '新建或加入集群时使用这些信息；单机时名称也用于公开状态页。'
    : '新建或加入集群时使用这些信息；它们不会随「保存设置」保存。';
}

function readLocalNodeDescriptor() {
  return {
    node_id: document.getElementById('config-cluster-node-id')?.value.trim() || '',
    name: document.getElementById('config-cluster-node-name')?.value.trim() || '',
    api_url: (document.getElementById('config-cluster-api-url')?.value.trim() || '').replace(/\/+$/, ''),
    priority: readIntegerInput('config-cluster-priority', 0),
  };
}

function getClusterConfigFromForm() {
  const existing = loadedClusterConfig;
  const local = readLocalNodeDescriptor();
  const editable = localLabelEditable();
  return {
    ...existing,
    enabled: !!existing.enabled,
    node_id: editable ? local.node_id : existing.node_id,
    node_name: editable ? (local.name || local.node_id) : existing.node_name,
    public_api_url: editable ? local.api_url : existing.public_api_url,
    priority: editable ? local.priority : existing.priority,
    peers: structuredClone(existing.peers || []),
    heartbeat_interval_secs: readIntegerInput('config-cluster-heartbeat', 5),
    failover_timeout_secs: readIntegerInput('config-cluster-failover-timeout', 15),
    lease_ttl_secs: readIntegerInput('config-cluster-lease-ttl', 20),
    sync_monitored_channels: document.getElementById('config-cluster-sync-channels').checked,
    auto_failover: document.getElementById('config-cluster-auto-failover')?.checked !== false,
    thresholds: existing.thresholds || {
      max_failed_restarts: 3
    },
  };
}

async function refreshClusterStatus() {
  if (clusterRefreshInFlight || clusterMutationInFlight) {
    clusterRefreshQueued = true;
    return;
  }

  clusterRefreshInFlight = true;
  clusterRefreshQueued = false;
  const generation = clusterRequestGeneration;
  lastClusterFetchMs = Date.now();
  try {
    const result = await getJson('/api/cluster/status');
    if (generation !== clusterRequestGeneration) return;
    if (!result.success) {
      renderClusterStatus(null, result.message || '集群状态不可用');
      return;
    }

    renderClusterStatusAndSyncDashboard(result.data, null);
  } catch (error) {
    console.debug('Failed to refresh cluster status:', error);
    if (generation === clusterRequestGeneration) renderClusterStatus(null, '集群状态不可用');
  } finally {
    clusterRefreshInFlight = false;
    if (clusterRefreshQueued && !clusterMutationInFlight) void refreshClusterStatus();
  }
}

function renderClusterStatusAndSyncDashboard(cluster, errorMessage, options = {}) {
  renderClusterStatus(cluster, errorMessage);

  if (!cluster || !cluster.enabled) {
    lastClusterLocalStateSignature = null;
    return;
  }

  const signature = getClusterLocalStateSignature(cluster);
  const changed = lastClusterLocalStateSignature !== null
    && signature !== lastClusterLocalStateSignature;
  lastClusterLocalStateSignature = signature;

  if (options.forceDashboardSync || changed) {
    (state.hooks.refreshStatus?.() || Promise.resolve()).catch((error) => {
      console.debug('Failed to refresh dashboard after cluster state change:', error);
    });
  }
}

function getLocalClusterNode(cluster) {
  const nodes = Array.isArray(cluster?.nodes) ? cluster.nodes : [];
  return nodes.find(node => node.is_local)
    || nodes.find(node => node.node_id && node.node_id === cluster?.local_node_id)
    || null;
}

function getClusterLocalStateSignature(cluster) {
  const localNode = getLocalClusterNode(cluster);
  const toggles = localNode?.monitor_toggles || {};
  return JSON.stringify({
    local_node_id: cluster?.local_node_id || '',
    active_owner: cluster?.active_owner || '',
    config_version: localNode?.config_version || cluster?.config_version || '',
    role: localNode?.role || '',
    draining: !!localNode?.draining,
    network_unstable: !!localNode?.network_unstable,
    ffmpeg_running: !!localNode?.ffmpeg_running,
    enable_danmaku_command: !!toggles.enable_danmaku_command,
    enable_youtube_monitor: !!toggles.enable_youtube_monitor,
    enable_twitch_monitor: !!toggles.enable_twitch_monitor,
    youtube_enable_monitor: !!toggles.youtube_enable_monitor,
    twitch_enable_monitor: !!toggles.twitch_enable_monitor,
    niconico_enable_monitor: !!toggles.niconico_enable_monitor,
    priority_channel_enabled: !!toggles.priority_channel_enabled,
    priority_channel_auto_restart: !!toggles.priority_channel_auto_restart
  });
}

function clusterNodeStructure(node, configVersion, nodes) {
  const follow = node?.yt_index?.state === 'follows' ? ytIndexFollowChip(node, nodes)?.text : '';
  return JSON.stringify([
    node.node_id, node.name, node.api_url, node.is_local, node.role, node.health,
    node.draining, node.network_unstable, node.ffmpeg_running, node.active_stream,
    node.config_version, configVersion, !!node.self_check, publicStatusNodeId,
    node.yt_index, node.websub, follow,
  ]);
}

function reconcileClusterChildren(parent, desired) {
  const retained = new Set(desired);
  for (const element of [...parent.children]) {
    if (!retained.has(element)) element.remove();
  }
  desired.forEach((element, index) => {
    if (parent.children[index] !== element) parent.insertBefore(element, parent.children[index] || null);
  });
}

function updateClusterNodeMetrics(element, node) {
  const seen = element.querySelector('.cluster-seen-value');
  if (seen) {
    seen.dataset.lastSeen = node.last_seen || 0;
    const value = formatClusterSeen(node.last_seen);
    if (seen.textContent !== value) seen.textContent = value;
  }
  const label = element.querySelector('.cluster-self-check');
  if (label) {
    const display = selfCheckDisplay(node.self_check, node.health?.stale === true);
    label.dataset.state = display.state;
    if (label.textContent !== display.label) label.textContent = display.label;
    label.title = display.title;
  }
  updateClusterNetwork(element.querySelector('.cluster-node-network'), node);
}

function renderClusterStatus(cluster, errorMessage) {
  const indicator = document.getElementById('cluster-status-indicator');
  const nodeList = document.getElementById('cluster-node-list');
  const focused = document.activeElement;
  const focusNode = focused?.closest('[data-node-id]')?.dataset.nodeId;
  const action = focused?.dataset.clusterAction || null;

  // Recorded before the no-op early return below, so the gate still
  // tracks ownership changes on renders that draw nothing new.
  clusterConnected = !!cluster;
  if (cluster) lastClusterStatus = cluster;
  const canEnable = !!cluster && (!cluster.enabled
    || (!!cluster.active_owner && cluster.active_owner === cluster.local_node_id));
  const roleChanged = state.localNodeCanEnableMonitorToggles !== canEnable;
  state.localNodeCanEnableMonitorToggles = canEnable;
  if (roleChanged) {
    syncMonitorTogglesWithClusterRole();
  }
  state.hooks.updatePriorityToggleAvailability?.();

  if (!indicator || !nodeList) {
    return;
  }

  if (!cluster || !cluster.enabled) {
    indicator.className = 'status-indicator status-offline';
    nodeList.replaceChildren(createClusterEmptyState(errorMessage || '在 系统设置 → 多服务器节点 中新建或加入集群后显示所有节点'));
    updateClusterAutoFailoverToggle(window.configData?.cluster?.auto_failover !== false);
    renderedClusterNodes.clear();
    syncClusterActionAvailability();
    return;
  }

  indicator.className = `status-indicator ${cluster.active_owner ? 'status-live' : 'status-offline'}`;
  updateClusterAutoFailoverToggle(cluster.auto_failover !== false);
  publicStatusNodeId = (cluster.public_status?.node_id || '').trim();

  const nodes = Array.isArray(cluster.nodes) ? cluster.nodes : [];
  if (nodes.length === 0) {
    nodeList.replaceChildren(createClusterEmptyState('暂无节点心跳'));
    renderedClusterNodes.clear();
    syncClusterActionAvailability();
    return;
  }

  const nextNodes = new Map();
  const detailed = [];
  const compact = [];
  nodes.forEach(node => {
    const signature = clusterNodeStructure(node, cluster.config_version, nodes);
    const previous = renderedClusterNodes.get(node.node_id);
    const hasDetail = clusterNodeHasDetail(node);
    const element = previous?.signature === signature ? previous.element
      : hasDetail ? createClusterNodeCard(node, cluster.config_version, nodes)
        : createClusterNodeTile(node, cluster.config_version);
    element.dataset.nodeId = node.node_id;
    updateClusterNodeMetrics(element, node);
    nextNodes.set(node.node_id, { signature, element });
    (hasDetail ? detailed : compact).push(element);
  });

  const desired = [...detailed];
  if (compact.length > 0) {
    compactClusterNodes ||= document.createElement('div');
    compactClusterNodes.className = 'cluster-node-others';
    reconcileClusterChildren(compactClusterNodes, compact);
    desired.push(compactClusterNodes);
  }
  reconcileClusterChildren(nodeList, desired);
  renderedClusterNodes = nextNodes;
  syncClusterActionAvailability();
  if (action && !focused.isConnected) {
    pendingClusterFocus = { nodeId: focusNode, action };
    restoreClusterActionFocus();
  }
}

function updateClusterSeenTexts(cluster) {
  const byNode = new Map();
  (cluster?.nodes || []).forEach(node => {
    if (node.node_id) {
      byNode.set(node.node_id, node.last_seen || 0);
    }
  });

  document.querySelectorAll('.cluster-seen-value').forEach(el => {
    if (byNode.has(el.dataset.nodeId)) {
      el.dataset.lastSeen = byNode.get(el.dataset.nodeId);
    }
    const lastSeen = Number(el.dataset.lastSeen) || 0;
    el.textContent = formatClusterSeen(lastSeen > 0 ? lastSeen : null);
  });
}

function startClusterSeenTicker() {
  if (clusterSeenTickerId) {
    return;
  }
  clusterSeenTickerId = setInterval(() => {
    if (isDashboardVisible()) {
      updateClusterSeenTexts(null);
    }
  }, 1000);
}

function createClusterEmptyState(message) {
  const empty = document.createElement('div');
  empty.className = 'cluster-empty';
  empty.textContent = message;
  return empty;
}

function clusterNodeHasDetail(node) {
  return !node.is_local && node.ffmpeg_running === true && node.health?.stale !== true;
}

function createClusterNodeBadge(node, clusterConfigVersion) {
  const role = node.role || 'unknown';
  const configState = formatNodeConfigSync(node, clusterConfigVersion);
  const badge = document.createElement('span');
  badge.className = `cluster-badge ${clusterNodeBadgeClass(node, role, configState)}`.trim();
  badge.appendChild(createClusterIcon(clusterStatusIcon(node, role, configState)));
  badge.appendChild(document.createTextNode(formatClusterNodeStatus(node)));
  if (!clusterNodeUsable(node)) {
    const reason = formatClusterHealthReason(node.health?.reason || '-');
    badge.title = node.draining && node.health?.reason !== 'draining'
      ? `${formatClusterHealthReason('draining')}；${reason}` : reason;
  }
  return badge;
}

function createClusterSeenValue(node) {
  const seen = document.createElement('span');
  seen.className = 'cluster-node-meta-value cluster-seen-value';
  seen.textContent = formatClusterSeen(node.last_seen);
  seen.dataset.nodeId = node.node_id || '';
  seen.dataset.lastSeen = node.last_seen || 0;
  return seen;
}

function createClusterHeartbeat(node) {
  const wrap = document.createElement('span');
  wrap.className = 'cluster-heartbeat';
  const label = document.createElement('span');
  label.className = 'cluster-heartbeat-label';
  label.textContent = '心跳';
  const seen = createClusterSeenValue(node);
  seen.className = 'cluster-seen-value';
  wrap.append(label, seen);
  return wrap;
}

function createClusterSelfCheck(node) {
  if (!node.self_check) return null;
  const display = selfCheckDisplay(node.self_check, node.health?.stale === true);
  const label = document.createElement('span');
  label.className = 'cluster-self-check';
  label.dataset.state = display.state;
  label.textContent = display.label;
  label.title = display.title;
  return label;
}

function createClusterNodeActions(node) {
  const usable = clusterNodeUsable(node);
  const isRecoverableFault = !!node.draining || !!node.network_unstable
    || (node.health?.healthy === false && node.health?.reason !== 'waiting_for_heartbeat');
  const showEnableButton = isRecoverableFault;

  const actions = document.createElement('div');
  actions.className = 'cluster-node-actions';

  const toggleButton = createClusterIconButton(
    showEnableButton ? '恢复节点（退出维护并重新检测）' : '进入维护（暂停接管）',
    showEnableButton ? 'cluster-enable-btn' : 'cluster-disable-btn',
    () => setClusterDrainForNode(node.node_id || '', !showEnableButton)
  );
  toggleButton.appendChild(createClusterIcon(showEnableButton ? 'enabled' : 'disabled'));
  toggleButton.dataset.clusterAction = 'maintenance';

  const activateButton = createClusterIconButton('切换到此节点（手动设为活跃）', 'cluster-switch-btn', () => {
    setClusterNodeActive(node.node_id || '');
  });
  activateButton.disabled = !usable;
  activateButton.dataset.unavailable = String(!usable);
  activateButton.dataset.clusterAction = 'activate';
  activateButton.appendChild(createClusterIcon('switch'));

  const restartButton = createClusterIconButton('重启节点程序', 'cluster-restart-btn', () => {
    restartClusterNode(node.node_id || '');
  });
  restartButton.appendChild(createClusterIcon('restart'));
  restartButton.dataset.clusterAction = 'restart';

  actions.append(toggleButton, activateButton, restartButton);
  return actions;
}

function createClusterNodeTile(node, clusterConfigVersion) {
  const tile = document.createElement('div');
  tile.className = 'cluster-node-tile';

  const head = document.createElement('div');
  head.className = 'cluster-node-tile-head';
  head.append(createClusterNodeName(node), createClusterNodeBadge(node, clusterConfigVersion));
  const tileChip = createPublicStatusChip(node);
  if (tileChip) {
    head.appendChild(tileChip);
  }
  const tileIndexChip = createYtIndexChip(node);
  if (tileIndexChip) head.appendChild(tileIndexChip);

  const seen = createClusterSeenValue(node);
  seen.classList.add('cluster-node-tile-seen');

  tile.append(head, seen);
  const selfCheck = createClusterSelfCheck(node);
  if (selfCheck) tile.appendChild(selfCheck);
  tile.appendChild(createClusterNodeActions(node));
  return tile;
}

function createClusterNodeCard(node, clusterConfigVersion, nodes) {
  const stream = node.active_stream;

  const card = document.createElement('div');
  card.className = 'cluster-node-card cluster-node-card-featured';

  const title = document.createElement('div');
  title.className = 'cluster-node-title';

  const identity = document.createElement('div');
  identity.className = 'cluster-node-identity';
  identity.append(createClusterNodeName(node), createClusterNodeBadge(node, clusterConfigVersion));
  const titleChip = createPublicStatusChip(node);
  if (titleChip) {
    identity.appendChild(titleChip);
  }
  const indexChip = createYtIndexChip(node) || createYtFollowChip(node, nodes);
  if (indexChip) identity.appendChild(indexChip);
  const selfCheck = createClusterSelfCheck(node);
  if (selfCheck) identity.appendChild(selfCheck);
  title.appendChild(identity);
  // Title-row caption: putting this in meta sat it in the network column.
  const peerLine = ytIndexPeerLine(node);
  if (peerLine) {
    const line = document.createElement('small');
    line.className = 'cluster-node-yt-index';
    line.textContent = peerLine;
    line.title = peerLine;
    title.appendChild(line);
  }
  title.appendChild(createClusterHeartbeat(node));

  const meta = document.createElement('div');
  meta.className = 'cluster-node-meta';

  if (stream) {
    meta.appendChild(createClusterNodeStream(stream));
  }
  const networkPanel = createClusterNetwork(node);
  if (networkPanel) {
    meta.appendChild(networkPanel);
  }

  card.append(title, meta, createClusterNodeActions(node));
  return card;
}

// The page does not follow the active node, so which node serves it is worth
// showing next to the name rather than leaving it buried in settings.
function createPublicStatusChip(node) {
  if (!publicStatusNodeId || (node?.node_id || '').trim() !== publicStatusNodeId) {
    return null;
  }
  const chip = document.createElement('span');
  chip.className = 'cluster-badge cluster-badge-public-status';
  chip.textContent = '状态页';
  chip.title = '此节点提供公开状态页';
  return chip;
}

// Whether this node answers YouTube for the cluster (or should and cannot).
function createYtIndexChip(node) {
  const model = ytIndexChip(node);
  if (!model) return null;
  return ytIndexBadge(model);
}

function createYtFollowChip(node, nodes) {
  const model = ytIndexFollowChip(node, nodes);
  if (!model) return null;
  return ytIndexBadge(model);
}

function ytIndexBadge(model) {
  const chip = document.createElement('span');
  chip.className = `cluster-badge cluster-badge-yt-index${model.warn ? ' is-warn' : ''}`;
  chip.textContent = model.text;
  chip.title = model.title;
  if (model.websub) {
    const dot = document.createElement('span');
    dot.className = `cluster-yt-index-websub is-${model.websub}`;
    chip.appendChild(dot);
  }
  return chip;
}

function createClusterNodeName(node) {
  const label = node.name || node.node_id || '-';
  const webuiUrl = clusterNodeWebuiUrl(node);
  if (!node.is_local && webuiUrl) {
    const link = document.createElement('a');
    link.className = 'cluster-node-name cluster-node-name-link';
    link.href = webuiUrl;
    link.target = '_blank';
    link.rel = 'noopener noreferrer';
    link.title = '打开节点 WebUI';
    link.textContent = label;
    return link;
  }

  const name = document.createElement('span');
  name.className = 'cluster-node-name';
  name.textContent = label;
  return name;
}

function clusterNodeWebuiUrl(node) {
  const rawUrl = (node?.api_url || '').trim().replace(/\/+$/, '');
  if (!rawUrl) return '';

  try {
    const url = new URL(rawUrl);
    return (url.protocol === 'http:' || url.protocol === 'https:') ? url.href : '';
  } catch (_) {
    return '';
  }
}

function createClusterNodeStream(stream) {
  const block = document.createElement('div');
  block.className = 'cluster-node-stream';

  const head = document.createElement('div');
  head.className = 'cluster-node-stream-head';

  const platform = (stream.platform || '').trim();
  if (platform) {
    head.appendChild(createClusterStreamPlatform(platform));
  }

  const channel = document.createElement('span');
  channel.className = 'cluster-stream-channel';
  channel.textContent = stream.channel_name || stream.channel_id || '-';
  channel.title = channel.textContent;
  head.appendChild(channel);

  block.appendChild(head);

  const streamTitle = (stream.title || '').trim();
  if (streamTitle) {
    const titleLine = document.createElement('div');
    titleLine.className = 'cluster-stream-title';
    titleLine.textContent = streamTitle;
    titleLine.title = streamTitle;
    block.appendChild(titleLine);
  }

  return block;
}

const CLUSTER_STREAM_PLATFORMS = {
  youtube: { symbol: '#i-youtube', label: 'YouTube' },
  twitch: { symbol: '#i-twitch', label: 'Twitch' },
  niconico: { symbol: '#i-niconico', label: 'Niconico' }
}

function createClusterStreamPlatform(platform) {
  const key = clusterStreamPlatformKey(platform);
  const known = CLUSTER_STREAM_PLATFORMS[key];

  const chip = document.createElement('span');
  chip.className = 'cluster-stream-platform';
  chip.dataset.platform = key;
  chip.title = known ? known.label : platform;

  if (!known) {
    // Unknown platform: fall back to the code rather than a blank chip.
    chip.textContent = platform;
    return chip;
  }

  const svg = document.createElementNS(SVG_NS, 'svg');
  svg.setAttribute('viewBox', '0 0 24 24');
  svg.setAttribute('role', 'img');
  svg.setAttribute('aria-label', known.label);
  const use = document.createElementNS(SVG_NS, 'use');
  use.setAttribute('href', known.symbol);
  svg.appendChild(use);
  chip.appendChild(svg);
  return chip;
}

function clusterStreamPlatformKey(platform) {
  switch (platform.toUpperCase()) {
    case 'YT': return 'youtube';
    case 'TW': return 'twitch';
    case 'NC': return 'niconico';
    default: return 'other';
  }
}

function createClusterIconButton(label, extraClass, onClick) {
  const button = document.createElement('button');
  button.className = `btn-secondary compact-btn icon-btn cluster-action-btn ${extraClass}`;
  button.type = 'button';
  button.title = label;
  button.setAttribute('aria-label', label);
  button.addEventListener('click', onClick);
  return button;
}

function renderClusterConfigSync(cluster) {
  const version = cluster?.config_version;
  const nodes = Array.isArray(cluster?.nodes) ? cluster.nodes : [];
  if (!version || nodes.length === 0) {
    return '-';
  }

  const knownNodes = nodes.filter(node => node.config_version);
  const outOfSync = knownNodes.filter(node => node.config_version !== version);
  if (outOfSync.length === 0) {
    return renderClusterSyncLabel('已同步');
  }

  return renderClusterSyncLabel(`${outOfSync.length} 个节点未同步`, false);
}

function formatNodeConfigSync(node, clusterConfigVersion) {
  if (!node.config_version || !clusterConfigVersion) {
    return '-';
  }
  return node.config_version === clusterConfigVersion ? '已同步' : '未同步';
}

function renderClusterSyncLabel(label, synced = label === '已同步') {
  const wrapper = document.createElement('span');
  wrapper.className = 'cluster-label-with-icon';
  wrapper.append(createClusterIcon(synced ? 'enabled' : 'sync-error'), document.createTextNode(label));
  return wrapper;
}

function clusterNodeBadgeClass(node, role, configState = '-') {
  if (node.draining || role === 'draining') return 'draining';
  if (node.health?.reason === 'waiting_for_heartbeat') {
    return '';
  }
  if (!clusterNodeUsable(node) || role === 'unhealthy') {
    return 'fault';
  }
  if (configState === '未同步') {
    return 'sync-warning';
  }
  return role === 'active' ? 'active' : '';
}

function clusterStatusIcon(node, role, configState = '-') {
  if (node.draining || role === 'draining') return 'disabled';
  if (node.health?.reason === 'waiting_for_heartbeat') {
    return 'sync-error';
  }
  if (!clusterNodeUsable(node) || role === 'unhealthy') {
    return 'fault';
  }
  if (configState === '未同步') {
    return 'sync-error';
  }
  return 'enabled';
}

function formatClusterSeen(lastSeen) {
  if (!lastSeen) {
    return '无心跳';
  }
  const secondsAgo = Math.max(0, Math.floor(Date.now() / 1000) - lastSeen);
  if (secondsAgo < 2) return '刚刚';
  if (secondsAgo < 60) return `${secondsAgo}s 前`;
  if (secondsAgo < 3600) return `${Math.floor(secondsAgo / 60)}m 前`;
  const hours = Math.floor(secondsAgo / 3600);
  const minutes = Math.floor((secondsAgo % 3600) / 60);
  return minutes > 0 ? `${hours}h ${minutes}m 前` : `${hours}h 前`;
}

function syncClusterActionAvailability() {
  for (const button of document.querySelectorAll('#cluster-node-list button, #clusterAutoFailoverToggle, #clusterSyncBtn, #public-status-save-btn')) {
    button.disabled = clusterMutationInFlight || !clusterConnected || button.dataset.unavailable === 'true';
  }
}

function restoreClusterActionFocus() {
  if (!pendingClusterFocus || clusterMutationInFlight) return;
  const { nodeId, action } = pendingClusterFocus;
  pendingClusterFocus = null;
  if (document.activeElement !== document.body) return;
  const replacement = [...(renderedClusterNodes.get(nodeId)?.element.querySelectorAll('button') || [])]
    .find(button => button.dataset.clusterAction === action && !button.disabled);
  replacement?.focus();
}

// Cluster actions share one writer because recovery, ownership, and config
// propagation affect the same snapshot. Reads begun before a write cannot
// replace its acknowledged result; an SSE refresh during it is replayed.
async function runClusterMutation(action, context, onSuccess) {
  if (clusterMutationInFlight) return false;
  const focused = document.activeElement;
  if (focused?.dataset.clusterAction) {
    pendingClusterFocus = {
      nodeId: focused.closest('[data-node-id]')?.dataset.nodeId,
      action: focused.dataset.clusterAction,
    };
  }
  clusterMutationInFlight = true;
  clusterRequestGeneration += 1;
  syncClusterActionAvailability();
  try {
    const result = await action();
    if (result.success) onSuccess?.(result);
    if (result.data) renderClusterStatusAndSyncDashboard(result.data, null, { forceDashboardSync: true });
    showNotification(result.message || `${context}${result.success ? '成功' : '失败'}`, result.success ? 'success' : 'error');
    return !!result.success;
  } catch (error) {
    showNotification(`${context}失败: ${error.message}`, 'error');
    return false;
  } finally {
    clusterMutationInFlight = false;
    updateClusterAutoFailoverToggle(confirmedAutoFailover);
    syncClusterActionAvailability();
    restoreClusterActionFocus();
    void refreshClusterStatus();
  }
}

async function setClusterDrainForNode(nodeId, draining) {
  return runClusterMutation(
    () => postJsonApi('/api/cluster/drain', { node_id: nodeId, draining }),
    draining ? '设置节点维护' : '恢复节点',
  );
}

async function setClusterNodeActive(nodeId) {
  if (!nodeId) return;
  return runClusterMutation(async () => {
    const recovered = await postJsonApi('/api/cluster/drain', { node_id: nodeId, draining: false });
    if (!recovered.success) return recovered;
    if (recovered.data) renderClusterStatusAndSyncDashboard(recovered.data, null);
    return postJsonApi('/api/cluster/failover', { target_node_id: nodeId });
  }, '切换活跃节点');
}

async function restartClusterNode(nodeId) {
  if (!nodeId || clusterMutationInFlight || !confirm(`确定要重启节点 ${nodeId} 的 bilistream 程序吗？`)) return;
  const succeeded = await runClusterMutation(
    () => postJsonApi('/api/cluster/restart-node', { node_id: nodeId }), '重启节点',
  );
  if (succeeded) setTimeout(refreshClusterStatus, 3000);
}

function updateClusterAutoFailoverToggle(enabled) {
  confirmedAutoFailover = !!enabled;
  const toggle = document.getElementById('clusterAutoFailoverToggle');
  if (toggle && !clusterMutationInFlight) toggle.checked = confirmedAutoFailover;
}

async function setClusterAutoFailover(enabled) {
  if (clusterMutationInFlight) return;
  return runClusterMutation(
    () => postJsonApi('/api/cluster/auto-failover', { enabled: !!enabled }),
    '设置自动故障转移',
    () => {
      confirmedAutoFailover = !!enabled;
      if (window.configData?.cluster) window.configData.cluster.auto_failover = !!enabled;
      // The settings form keeps its loaded draft. An unrelated save must not
      // re-submit this dashboard change as a stale whole-cluster edit.
    },
  );
}

async function pushClusterConfig() {
  return runClusterMutation(() => postJsonApi('/api/cluster/push-config'), '同步频道配置');
}

// Public status page --------------------------------------------------------
// One node serves it, picked for bandwidth rather than for holding the stream,
// so the choice is cluster-wide config rather than a local flag.

// %查询 answers the page as 「详情：<url>」, and Bilibili caps a live danmaku at
// 30 characters, so the prefix comes out of the same budget.
const DANMAKU_ADVERT_PREFIX = '详情：';
const PUBLIC_URL_MAX_CHARS = 30 - [...DANMAKU_ADVERT_PREFIX].length;
const PUBLIC_URL_HINT = `收到 %查询 弹幕时播报「${DANMAKU_ADVERT_PREFIX}地址」，留空则不播报。`;

function publicStatusFromForm() {
  return {
    node_id: document.getElementById('config-public-status-node')?.value.trim() || '',
    bind: document.getElementById('config-public-status-bind')?.value.trim() || '127.0.0.1',
    port: readIntegerInput('config-public-status-port', 23234),
    public_url: document.getElementById('config-public-status-url')?.value.trim() || '',
  };
}

function renderPublicStatusNodeOptions(cluster = {}, selected = '') {
  const select = document.getElementById('config-public-status-node');
  if (!select) return;

  const nodes = [];
  const localId = (cluster.node_id || '').trim();
  if (localId) {
    nodes.push({ id: localId, name: (cluster.node_name || localId).trim() });
  }
  for (const peer of cluster.enabled && Array.isArray(cluster.peers) ? cluster.peers : []) {
    const id = (peer.node_id || '').trim();
    if (id && !nodes.some(node => node.id === id)) {
      nodes.push({ id, name: (peer.name || id).trim() });
    }
  }
  // Keep a node that is configured but no longer listed, so opening the panel
  // does not silently reassign the page.
  if (selected && !nodes.some(node => node.id === selected)) {
    nodes.push({ id: selected, name: selected, unknown: true });
  }

  const options = [createSelectOption('', '不启用')];
  for (const node of nodes) {
    const label = node.unknown
      ? `${node.id} (未连接节点)`
      : node.id === localId ? `本机${cluster.enabled ? ` (${node.name})` : ''}`
        : node.name === node.id ? node.id : `${node.name} (${node.id})`;
    options.push(createSelectOption(node.id, label));
  }
  select.replaceChildren(...options);
  select.value = selected;
}

function updatePublicStatusUrlHint() {
  const input = document.getElementById('config-public-status-url');
  const hint = document.getElementById('public-status-url-hint');
  if (!input || !hint) return;

  const length = [...input.value.trim()].length;
  const tooLong = length > PUBLIC_URL_MAX_CHARS;
  hint.classList.toggle('is-invalid', tooLong);
  hint.textContent = tooLong
    ? `公开地址 ${length} 字，超过 ${PUBLIC_URL_MAX_CHARS} 字，%查询 不会播报；请用更短的域名。`
    : PUBLIC_URL_HINT;
}

function loadPublicStatusSettings(cluster = {}) {
  const publicStatus = cluster.public_status || {};
  renderPublicStatusNodeOptions(cluster, (publicStatus.node_id || '').trim());
  setInputValue('config-public-status-bind', publicStatus.bind || '127.0.0.1');
  setInputValue('config-public-status-port', publicStatus.port || 23234);
  setInputValue('config-public-status-url', publicStatus.public_url || '');
  loadPublicStatusHint(cluster);
  updatePublicStatusUrlHint();
  syncPublicStatusRoleControls();
}

function committedPublicNodeId() {
  return (loadedClusterConfig.public_status?.node_id || '').trim();
}

// Standalone keeps the old single-save flow. A managed cluster changes the
// serving node through a membership operation; port and URL stay ordinary.
function loadPublicStatusHint(cluster = loadedClusterConfig) {
  const managed = membershipLifecycle === 'managed';
  const held = !!membershipLifecycle && !['managed', 'standalone'].includes(membershipLifecycle);
  const hint = document.getElementById('public-status-node-hint');
  if (hint) hint.textContent = managed
    ? '所选服务器提供公开页；有 YouTube key 时也提供共享索引。更改运行位置是一次成员操作，期间会暂停转播。'
    : held ? '本服务器未加入集群，暂不能更改运行位置。'
    : cluster.enabled ? '所选节点提供公开页；有 YouTube key 时也提供共享索引。'
    : '本机即可运行，无需开启多服务器。';
  const button = document.getElementById('public-status-save-btn');
  if (button) button.textContent = managed ? '保存并同步' : '保存公开页设置';
}

function syncPublicStatusRoleControls() {
  const managed = membershipLifecycle === 'managed';
  const held = !!membershipLifecycle && !['managed', 'standalone'].includes(membershipLifecycle);
  const select = document.getElementById('config-public-status-node');
  const roleButton = document.getElementById('public-status-role-btn');
  if (select) select.disabled = held;
  if (roleButton) {
    roleButton.classList.toggle('hidden', !managed);
    roleButton.dataset.unchanged = String(!select || select.value === committedPublicNodeId());
  }
}

function publicStatusSelection() {
  return document.getElementById('config-public-status-node')?.value.trim() || '';
}

async function savePublicStatusSettings() {
  if (clusterMutationInFlight) return;
  const button = document.getElementById('public-status-save-btn');
  const config = publicStatusFromForm();
  if (membershipLifecycle && membershipLifecycle !== 'standalone') {
    if (config.node_id !== committedPublicNodeId()) {
      showNotification('运行位置需通过「更改运行位置」提交；本次只保存端口、地址等设置', 'error');
    }
    config.node_id = committedPublicNodeId();
  }
  setButtonLoading(button, null, true);
  try {
    const saved = await runClusterMutation(() => postJsonApi('/api/cluster/public-status', { config }), '保存公开状态页配置');
    if (saved) {
      const server = await state.hooks.reloadServerConfig?.();
      if (server?.cluster?.public_status) loadedClusterConfig.public_status = structuredClone(server.cluster.public_status);
    }
  } finally {
    setButtonLoading(button, null, false);
    syncClusterActionAvailability();
  }
}

function initClusterControls() {
  document
    .getElementById('clusterAutoFailoverToggle')
    ?.addEventListener('change', event => setClusterAutoFailover(event.currentTarget.checked));
  document
    .getElementById('clusterFoldBtn')
    ?.addEventListener('click', toggleClusterCardFold);
  document
    .getElementById('clusterRefreshBtn')
    ?.addEventListener('click', refreshClusterStatus);
  document
    .getElementById('clusterSyncBtn')
    ?.addEventListener('click', pushClusterConfig);
  document
    .getElementById('config-public-status-node')
    ?.addEventListener('change', syncPublicStatusRoleControls);
  document
    .getElementById('public-status-save-btn')
    ?.addEventListener('click', savePublicStatusSettings);
  document
    .getElementById('config-public-status-url')
    ?.addEventListener('input', updatePublicStatusUrlHint);
  initClusterCardFold();
}

function startClusterRefresh() {
  if (clusterRefreshIntervalId) {
    clearInterval(clusterRefreshIntervalId);
  }
  clusterRefreshIntervalId = setInterval(() => {
    if (!isDashboardVisible()) {
      return;
    }
    if (eventStreamHealthy() && Date.now() - lastClusterFetchMs < 15000) {
      return;
    }
    refreshClusterStatus();
  }, clusterRefreshInterval);
  startClusterSeenTicker();
  refreshClusterStatus();
}

function getLastClusterStatus() {
  return lastClusterStatus;
}

export {
  loadClusterSettings,
  rebaseClusterTopology,
  rebaseClusterDraft,
  setClusterMembershipContext,
  readLocalNodeDescriptor,
  publicStatusSelection,
  committedPublicNodeId,
  syncPublicStatusRoleControls,
  getLastClusterStatus,
  loadPublicStatusSettings,
  savePublicStatusSettings,
  updatePublicStatusUrlHint,
  getClusterConfigFromForm,
  getClusterConfigBaseline,
  acceptClusterConfigBaseline,
  refreshClusterStatus,
  initClusterControls,
  startClusterRefresh,
};
