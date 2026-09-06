// cluster.js — multi-server panel, using the shared authenticated API client.

import { isDashboardVisible, parseInteger, readIntegerInput, setInputValue, setCheckboxChecked, setButtonLoading, showNotification, SVG_NS } from './dom.js';
import { createSelectOption, state, syncMonitorTogglesWithClusterRole } from './state.js';
import { getJson, postJsonApi } from './api.js';
import { eventStreamHealthy } from './events.js';
import { createClusterNetwork, updateClusterNetwork } from './cluster-network.js';
import { selfCheckDisplay, clusterNodeUsable, formatClusterNodeStatus, formatClusterHealthReason } from './cluster-health.js';

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

let currentClusterPeers = [];
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

function loadClusterSettings(cluster = {}) {
  acceptClusterConfigBaseline(cluster);
  setCheckboxChecked('config-cluster-enabled', !!cluster.enabled);
  setCheckboxChecked('config-cluster-sync-channels', !!cluster.sync_monitored_channels);
  setCheckboxChecked('config-cluster-auto-failover', cluster.auto_failover !== false);
  updateClusterAutoFailoverToggle(cluster.auto_failover !== false);
  setInputValue('config-cluster-node-id', cluster.node_id || '');
  setInputValue('config-cluster-node-name', cluster.node_name || cluster.node_id || '');
  setInputValue('config-cluster-api-url', cluster.public_api_url || '');
  setInputValue('config-cluster-priority', Number.isFinite(cluster.priority) ? cluster.priority : 0);
  setInputValue('config-cluster-heartbeat', cluster.heartbeat_interval_secs || 5);
  setInputValue('config-cluster-failover-timeout', cluster.failover_timeout_secs || 15);
  setInputValue('config-cluster-lease-ttl', cluster.lease_ttl_secs || 20);
  currentClusterPeers = Array.isArray(cluster.peers)
    ? cluster.peers.map(peer => ({
      node_id: peer.node_id || '',
      name: peer.name || '',
      api_url: peer.api_url || '',
      priority: Number.isFinite(peer.priority) ? peer.priority : 0
    }))
    : [];
  renderClusterPeerList();
  loadPublicStatusSettings(cluster);
}

function renderClusterPeerList() {
  const container = document.getElementById('cluster-peer-list');
  if (!container) return;

  container.replaceChildren();

  if (!currentClusterPeers.length) {
    const empty = document.createElement('div');
    empty.className = 'cluster-peer-empty';
    empty.textContent = '暂无其他节点';
    container.appendChild(empty);
    return;
  }

  const fragment = document.createDocumentFragment();
  currentClusterPeers.forEach((peer, index) => {
    fragment.appendChild(createClusterPeerCard(peer, index));
  });
  container.appendChild(fragment);
}

function createClusterPeerCard(peer, index) {
  const card = document.createElement('div');
  card.className = 'cluster-peer-card';
  card.dataset.index = String(index);

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
  name.textContent = peer.name || peer.node_id || '未命名节点';

  const nodeId = document.createElement('div');
  nodeId.className = 'cluster-peer-id';
  nodeId.textContent = peer.node_id || '-';

  titleBlock.append(name, nodeId);
  identity.append(dot, titleBlock);

  const actions = document.createElement('div');
  actions.className = 'cluster-peer-header-actions';

  const priority = document.createElement('span');
  priority.className = 'cluster-peer-priority';
  priority.textContent = `优先级 ${clusterPeerPriorityValue(peer)}`;

  const refreshSummary = () => {
    const updatedPeer = currentClusterPeers[index];
    if (!updatedPeer) return;
    name.textContent = updatedPeer.name || updatedPeer.node_id || '未命名节点';
    nodeId.textContent = updatedPeer.node_id || '-';
    priority.textContent = `优先级 ${clusterPeerPriorityValue(updatedPeer)}`;
  };

  const removeButton = document.createElement('button');
  removeButton.className = 'btn-secondary compact-btn icon-btn cluster-action-btn cluster-disable-btn cluster-peer-remove-btn';
  removeButton.type = 'button';
  removeButton.title = '删除节点';
  removeButton.setAttribute('aria-label', '删除节点');
  removeButton.addEventListener('click', () => removeClusterPeer(index));
  appendClusterPeerRemoveIcon(removeButton);

  actions.append(priority, removeButton);
  header.append(identity, actions);

  const fields = document.createElement('div');
  fields.className = 'cluster-peer-fields';
  fields.append(
    createClusterPeerField(index, 'node_id', '节点 ID', peer.node_id, 'text', '节点 ID', '', refreshSummary),
    createClusterPeerField(index, 'name', '节点名称', peer.name, 'text', '名称', '', refreshSummary),
    createClusterPeerField(index, 'api_url', 'API 地址', peer.api_url, 'text', 'API 地址', 'cluster-peer-field-url'),
    createClusterPeerField(index, 'priority', '优先级', clusterPeerPriorityValue(peer), 'number', '优先级', 'cluster-peer-field-priority', refreshSummary)
  );

  card.append(header, fields);
  return card;
}

function createClusterPeerField(index, field, labelText, value, type, placeholder, extraClass = '', afterChange = null) {
  const label = document.createElement('label');
  label.className = `cluster-peer-field${extraClass ? ` ${extraClass}` : ''}`;

  const labelSpan = document.createElement('span');
  labelSpan.textContent = labelText;

  const input = document.createElement('input');
  input.type = type;
  input.value = value ?? '';
  input.placeholder = placeholder;
  input.addEventListener('input', () => {
    updateClusterPeer(index, field, input.value);
    if (afterChange) {
      afterChange();
    }
  });

  label.append(labelSpan, input);
  return label;
}

function clusterPeerPriorityValue(peer) {
  return Number.isFinite(peer.priority) ? peer.priority : 0;
}

function appendClusterPeerRemoveIcon(button) {
  const svgNamespace = 'http://www.w3.org/2000/svg';
  const svg = document.createElementNS(svgNamespace, 'svg');
  svg.classList.add('cluster-btn-icon');
  svg.setAttribute('viewBox', '0 0 24 24');
  svg.setAttribute('fill', 'none');
  svg.setAttribute('stroke', 'currentColor');
  svg.setAttribute('stroke-width', '2');
  svg.setAttribute('stroke-linecap', 'round');
  svg.setAttribute('stroke-linejoin', 'round');
  svg.setAttribute('aria-hidden', 'true');

  const polyline = document.createElementNS(svgNamespace, 'polyline');
  polyline.setAttribute('points', '3,6 5,6 21,6');

  const path = document.createElementNS(svgNamespace, 'path');
  path.setAttribute('d', 'M19,6v14a2,2 0 0,1 -2,2H7a2,2 0 0,1 -2,-2V6m3,0V4a2,2 0 0,1 2,-2h4a2,2 0 0,1 2,2v2');

  svg.append(polyline, path);
  button.appendChild(svg);
}

function updateClusterPeer(index, field, value) {
  if (!currentClusterPeers[index]) return;
  currentClusterPeers[index][field] = field === 'priority' ? parseInteger(value, 0) : value.trim();
}

function addClusterPeer() {
  const nodeIdInput = document.getElementById('cluster-peer-node-id');
  const nameInput = document.getElementById('cluster-peer-name');
  const apiUrlInput = document.getElementById('cluster-peer-api-url');
  const priorityInput = document.getElementById('cluster-peer-priority');
  const nodeId = nodeIdInput.value.trim();
  const apiUrl = apiUrlInput.value.trim();

  if (!nodeId || !apiUrl) {
    showNotification('请填写节点 ID 和 API 地址', 'error');
    return;
  }

  const localNodeId = document.getElementById('config-cluster-node-id').value.trim();
  if (nodeId === localNodeId || currentClusterPeers.some(peer => peer.node_id === nodeId)) {
    showNotification('节点 ID 已存在', 'error');
    return;
  }

  currentClusterPeers.push({
    node_id: nodeId,
    name: nameInput.value.trim() || nodeId,
    api_url: apiUrl.replace(/\/+$/, ''),
    priority: parseInteger(priorityInput.value, 0)
  });
  nodeIdInput.value = '';
  nameInput.value = '';
  apiUrlInput.value = '';
  priorityInput.value = '';
  renderClusterPeerList();
}

function removeClusterPeer(index) {
  currentClusterPeers.splice(index, 1);
  renderClusterPeerList();
}

function getClusterConfigFromForm() {
  const existing = loadedClusterConfig;
  const nodeId = document.getElementById('config-cluster-node-id').value.trim();
  const peers = currentClusterPeers
    .map(peer => ({
      node_id: (peer.node_id || '').trim(),
      name: (peer.name || '').trim(),
      api_url: (peer.api_url || '').trim().replace(/\/+$/, ''),
      priority: parseInteger(peer.priority, 0)
    }))
    .filter(peer => peer.node_id && peer.api_url && peer.node_id !== nodeId);

  return {
    ...existing,
    enabled: document.getElementById('config-cluster-enabled').checked,
    node_id: nodeId,
    node_name: document.getElementById('config-cluster-node-name').value.trim() || nodeId,
    public_api_url: document.getElementById('config-cluster-api-url').value.trim().replace(/\/+$/, ''),
    priority: readIntegerInput('config-cluster-priority', 0),
    heartbeat_interval_secs: readIntegerInput('config-cluster-heartbeat', 5),
    failover_timeout_secs: readIntegerInput('config-cluster-failover-timeout', 15),
    lease_ttl_secs: readIntegerInput('config-cluster-lease-ttl', 20),
    sync_monitored_channels: document.getElementById('config-cluster-sync-channels').checked,
    auto_failover: document.getElementById('config-cluster-auto-failover')?.checked !== false,
    thresholds: existing.thresholds || {
      max_failed_restarts: 3
    },
    peers
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

function clusterNodeStructure(node, configVersion) {
  return JSON.stringify([
    node.node_id, node.name, node.api_url, node.is_local, node.role, node.health,
    node.draining, node.network_unstable, node.ffmpeg_running, node.active_stream,
    node.config_version, configVersion, !!node.self_check, publicStatusNodeId,
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
    nodeList.replaceChildren(createClusterEmptyState(errorMessage || '在配置中启用 cluster 后显示所有节点'));
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
    const signature = clusterNodeStructure(node, cluster.config_version);
    const previous = renderedClusterNodes.get(node.node_id);
    const hasDetail = clusterNodeHasDetail(node);
    const element = previous?.signature === signature ? previous.element
      : hasDetail ? createClusterNodeCard(node, cluster.config_version)
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

  const seen = createClusterSeenValue(node);
  seen.classList.add('cluster-node-tile-seen');

  tile.append(head, seen);
  const selfCheck = createClusterSelfCheck(node);
  if (selfCheck) tile.appendChild(selfCheck);
  tile.appendChild(createClusterNodeActions(node));
  return tile;
}

function createClusterNodeCard(node, clusterConfigVersion) {
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
  const selfCheck = createClusterSelfCheck(node);
  if (selfCheck) identity.appendChild(selfCheck);
  title.append(identity, createClusterHeartbeat(node));

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
    holodex_refresh_secs: readIntegerInput('config-public-status-refresh', 30),
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
  for (const peer of Array.isArray(cluster.peers) ? cluster.peers : []) {
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
      ? `${node.id} (未知节点)`
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
  setInputValue('config-public-status-refresh', publicStatus.holodex_refresh_secs || 30);
  setInputValue('config-public-status-url', publicStatus.public_url || '');
  updatePublicStatusUrlHint();
}

async function savePublicStatusSettings() {
  if (clusterMutationInFlight) return;
  const button = document.getElementById('public-status-save-btn');
  setButtonLoading(button, null, true);
  try {
    const config = publicStatusFromForm();
    await runClusterMutation(() => postJsonApi('/api/cluster/public-status', { config }), '保存公开状态页配置');
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
    .getElementById('cluster-peer-add-btn')
    ?.addEventListener('click', addClusterPeer);
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

export {
  loadClusterSettings,
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
