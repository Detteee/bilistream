// nodes.js — the server card for the public page.
//
// Mirrors the dashboard cluster panel: a featured card for the node that is
// actually pushing, compact tiles for the rest. No heartbeat ages, no action
// buttons, and no links into a node's WebUI.

import { formatBytes, formatFps, formatNetworkRate, formatSpeedRatio } from '/shared/js/format.js?v=7';

const SVG_NS = 'http://www.w3.org/2000/svg';

const ROLE_LABELS = {
  active: '活跃',
  restreaming: '转播中',
  standby: '备用',
  draining: '维护中',
  unhealthy: '异常',
};

const STREAM_PLATFORMS = {
  youtube: { symbol: '#i-youtube', label: 'YouTube' },
  twitch: { symbol: '#i-twitch', label: 'Twitch' },
  niconico: { symbol: '#i-niconico', label: 'Niconico' },
};

function hasPositive(value) {
  return Number.isFinite(value) && value > 0;
}

function hasRtmpTx(network) {
  return !!network && (hasPositive(network.stream_bitrate_kbps) || hasPositive(network.stream_speed));
}

function hasHlsCache(network) {
  return !!network && network.hls_cache_active && (
    hasPositive(network.stream_cache_bitrate_kbps)
    || hasPositive(network.stream_cache_speed)
    || hasPositive(network.stream_cache_total_bytes)
  );
}

/// Owner that is actually pushing. An idle active node is 活跃, not 转播中.
function isRestreaming(node) {
  return !!node.ffmpeg_running && hasRtmpTx(node.network);
}

/// Same condition as the 转播中 badge: an active node with ffmpeg on the wire.
export function clusterIsRestreaming(nodes) {
  return Array.isArray(nodes) && nodes.some((node) => node.role === 'active' && isRestreaming(node));
}

function hasDetail(node) {
  return !!node.stream || isRestreaming(node) || hasHlsCache(node.network);
}

function roleLabel(node) {
  // role is set before healthy is cleared for drain/fault, so check it first
  // or a 维护中 node would read as 异常.
  if (node.role === 'draining') {
    return ROLE_LABELS.draining;
  }
  if (!node.healthy) {
    return ROLE_LABELS.unhealthy;
  }
  if (node.role === 'active' && isRestreaming(node)) {
    return ROLE_LABELS.restreaming;
  }
  return ROLE_LABELS[node.role] || '未知';
}

function roleClass(node) {
  if (node.role === 'draining') {
    return 'draining';
  }
  if (!node.healthy) {
    return 'unhealthy';
  }
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

function speedTone(speed) {
  if (!Number.isFinite(speed) || speed <= 0) {
    return '';
  }
  if (speed > 0.97) {
    return 'ok';
  }
  if (speed > 0.94) {
    return 'warn';
  }
  return 'danger';
}

function createNetworkMeter(label, speed, tone, value, detailGroups) {
  const meter = document.createElement('div');
  meter.className = 'bili-network-meter';

  const meterLabel = document.createElement('div');
  meterLabel.className = 'bili-network-meter-label';
  const labelSpan = document.createElement('span');
  labelSpan.textContent = label;
  meterLabel.appendChild(labelSpan);

  const meterValue = document.createElement('div');
  meterValue.className = 'bili-network-meter-value';
  const valueSpan = document.createElement('span');
  valueSpan.textContent = value;
  meterValue.appendChild(valueSpan);

  if (speed) {
    const speedSpan = document.createElement('span');
    speedSpan.className = 'bili-network-meter-speed';
    if (tone) {
      speedSpan.dataset.tone = tone;
    }
    speedSpan.textContent = speed;
    meterValue.appendChild(speedSpan);
  }

  meter.append(meterLabel, meterValue);
  for (const group of detailGroups) {
    const parts = group.filter(Boolean);
    if (parts.length === 0) {
      continue;
    }

    const detail = document.createElement('div');
    detail.className = 'bili-network-total';
    parts.forEach((text, index) => {
      if (index > 0) {
        const sep = document.createElement('span');
        sep.className = 'bili-network-total-sep';
        sep.textContent = '·';
        detail.appendChild(sep);
      }
      const part = document.createElement('span');
      part.textContent = text;
      detail.appendChild(part);
    });
    meter.appendChild(detail);
  }

  return meter;
}

/// Same column layout as the dashboard. Never the bar graph: that needs
/// per-sample history and a fast poll, which a public page should not do.
function createNetwork(node) {
  const network = node.network || {};
  const pushing = isRestreaming(node);
  const cache = hasHlsCache(network);
  if (!pushing && !cache) {
    return null;
  }

  const panel = document.createElement('div');
  panel.className = 'cluster-node-network';

  const meters = document.createElement('div');
  meters.className = 'bili-network-meters';

  if (pushing) {
    meters.appendChild(createNetworkMeter(
      'RTMP TX',
      formatSpeedRatio(network.stream_speed),
      speedTone(network.stream_speed),
      formatNetworkRate(network.stream_bitrate_kbps),
      [
        [`累计 ${formatBytes(network.stream_total_bytes)}`, `${formatFps(network.stream_fps)} fps`],
      ],
    ));
  }

  if (cache) {
    meters.appendChild(createNetworkMeter(
      'HLS Cache',
      '',
      '',
      formatNetworkRate(network.stream_cache_bitrate_kbps),
      [[`累计 ${formatBytes(network.stream_cache_total_bytes)}`]],
    ));
  }

  panel.appendChild(meters);
  return panel;
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
  const network = createNetwork(node);
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

export function renderNodes(nodes) {
  const list = document.getElementById('cluster-node-list');
  const indicator = document.getElementById('cluster-status-indicator');
  if (!list) {
    return;
  }

  if (!Array.isArray(nodes) || nodes.length === 0) {
    list.replaceChildren(emptyState('暂无节点状态'));
    if (indicator) {
      indicator.className = 'status-indicator status-offline';
    }
    return;
  }

  if (indicator) {
    const anyActive = nodes.some((node) => node.healthy && node.role === 'active');
    indicator.className = `status-indicator ${anyActive ? 'status-live' : 'status-offline'}`;
  }

  const detailed = [];
  const compact = [];
  for (const node of nodes) {
    (hasDetail(node) ? detailed : compact).push(node);
  }

  const fragment = document.createDocumentFragment();
  for (const node of detailed) {
    fragment.appendChild(createFeaturedCard(node));
  }
  if (compact.length > 0) {
    const others = document.createElement('div');
    others.className = 'cluster-node-others';
    for (const node of compact) {
      others.appendChild(createTile(node));
    }
    fragment.appendChild(others);
  }
  list.replaceChildren(fragment);
}

function emptyState(message) {
  const empty = document.createElement('div');
  empty.className = 'cluster-empty';
  empty.textContent = message;
  return empty;
}
