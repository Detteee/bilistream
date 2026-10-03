// nodes.js — the server card for the public page.
//
// Mirrors the dashboard cluster panel: a featured card for the node that is
// actually pushing, compact tiles for the rest. No heartbeat ages, no action
// buttons, and no links into a node's WebUI.

import { createClusterNetwork, updateClusterNetwork } from '../../src/js/cluster-network.js';
import { formatClusterNodeStatus } from '../../src/js/cluster-health.js';
import {
  activeRestream,
  clusterIsRestreaming,
  isRestreaming,
} from '../../src/js/on-air.js';

export { activeRestream, clusterIsRestreaming, isRestreaming };

const SVG_NS = 'http://www.w3.org/2000/svg';

let renderedNodes = new Map();
let compactNodes = null;

const STREAM_PLATFORMS = {
  youtube: { symbol: '#i-youtube', label: 'YouTube' },
  twitch: { symbol: '#i-twitch', label: 'Twitch' },
  niconico: { symbol: '#i-niconico', label: 'Niconico' },
};

function hasDetail(node) {
  return node.ffmpeg_running === true;
}

function roleLabel(node) {
  const label = formatClusterNodeStatus(node);
  return label === '活跃' && isRestreaming(node) ? '转播中' : label;
}

function roleClass(node) {
  if (node.role === 'draining') {
    return 'draining';
  }
  if (node.waiting_for_heartbeat) return '';
  if (!node.healthy || node.role === 'unhealthy') return 'fault';
  return node.role === 'active' ? 'active' : '';
}

function createBadge(node) {
  const badge = document.createElement('span');
  badge.className = `cluster-badge ${roleClass(node)}`.trim();
  badge.textContent = roleLabel(node);
  return badge;
}

function createName(node) {
  const name = document.createElement('span');
  name.className = 'cluster-node-name';
  name.textContent = node.name || '-';
  return name;
}

function platformKey(platform) {
  switch ((platform || '').toUpperCase()) {
    case 'YT':
      return 'youtube';
    case 'TW':
      return 'twitch';
    case 'NC':
      return 'niconico';
    default:
      return 'other';
  }
}

function createStreamPlatform(platform) {
  const key = platformKey(platform);
  const known = STREAM_PLATFORMS[key];

  const chip = document.createElement('span');
  chip.className = 'cluster-stream-platform';
  chip.dataset.platform = key;
  chip.title = known ? known.label : platform;

  if (!known) {
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

function createStream(stream) {
  if (!stream) {
    return null;
  }

  const block = document.createElement('div');
  block.className = 'cluster-node-stream';

  const head = document.createElement('div');
  head.className = 'cluster-node-stream-head';

  const platform = (stream.platform || '').trim();
  if (platform) {
    head.appendChild(createStreamPlatform(platform));
  }

  const channel = document.createElement('span');
  channel.className = 'cluster-stream-channel';
  channel.textContent = stream.channel_name || '-';
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

function createFeaturedCard(node) {
  const card = document.createElement('div');
  card.className = 'cluster-node-card cluster-node-card-featured';

  const title = document.createElement('div');
  title.className = 'cluster-node-title';
  title.append(createName(node), createBadge(node));

  const meta = document.createElement('div');
  meta.className = 'cluster-node-meta';

  const stream = createStream(node.stream);
  if (stream) {
    meta.appendChild(stream);
  }
  const network = createClusterNetwork(node);
  if (network) {
    meta.appendChild(network);
  }

  card.append(title, meta);
  return card;
}

function createTile(node) {
  const tile = document.createElement('div');
  tile.className = 'cluster-node-tile';

  const head = document.createElement('div');
  head.className = 'cluster-node-tile-head';
  head.append(createName(node), createBadge(node));
  tile.appendChild(head);
  return tile;
}

function reconcile(parent, desired) {
  const retained = new Set(desired);
  for (const element of [...parent.children]) {
    if (!retained.has(element)) element.remove();
  }
  desired.forEach((element, index) => {
    if (parent.children[index] !== element) parent.insertBefore(element, parent.children[index] || null);
  });
}

export function renderNodes(nodes, message = '暂无节点状态') {
  const list = document.getElementById('cluster-node-list');
  const indicator = document.getElementById('cluster-status-indicator');
  if (!list) return;
  if (!Array.isArray(nodes) || nodes.length === 0) {
    list.replaceChildren(emptyState(message));
    renderedNodes.clear();
    if (indicator) indicator.className = 'status-indicator status-offline';
    return;
  }
  if (indicator) {
    const anyActive = nodes.some(node => node.healthy && node.role === 'active');
    indicator.className = `status-indicator ${anyActive ? 'status-live' : 'status-offline'}`;
  }
  const nextNodes = new Map();
  const occurrences = new Map();
  const detailed = [];
  const compact = [];
  for (const node of nodes) {
    // The public API deliberately has no node IDs. Rust orders by node ID;
    // name plus occurrence retains duplicate display names without exposing it.
    const occurrence = occurrences.get(node.name) || 0;
    occurrences.set(node.name, occurrence + 1);
    const key = JSON.stringify([node.name, occurrence]);
    const signature = JSON.stringify([node.name, node.role, node.healthy,
      node.waiting_for_heartbeat, node.ffmpeg_running, node.stream, roleLabel(node)]);
    const previous = renderedNodes.get(key);
    const element = previous?.signature === signature ? previous.element
      : hasDetail(node) ? createFeaturedCard(node) : createTile(node);
    updateClusterNetwork(element.querySelector('.cluster-node-network'), node);
    nextNodes.set(key, { element, signature });
    (hasDetail(node) ? detailed : compact).push(element);
  }
  const desired = [...detailed];
  if (compact.length) {
    compactNodes ||= document.createElement('div');
    compactNodes.className = 'cluster-node-others';
    reconcile(compactNodes, compact);
    desired.push(compactNodes);
  }
  reconcile(list, desired);
  renderedNodes = nextNodes;
}

function emptyState(message) {
  const empty = document.createElement('div');
  empty.className = 'cluster-empty';
  empty.textContent = message;
  return empty;
}
