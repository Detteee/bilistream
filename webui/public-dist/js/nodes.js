// nodes.js — the server card for the public page.
//
// Status only: whether each node is up and streaming, plus throughput meters.
// No heartbeat ages and no links into a node's WebUI — a viewer has no use for
// either, and the second is not theirs to open.

import { formatBytes, formatFps, formatFrameCount, formatNetworkRate, formatSpeedRatio } from '/shared/js/format.js';

const ROLE_LABELS = {
  active: '转播中',
  standby: '备用',
  draining: '维护中',
  unhealthy: '异常',
};

function roleLabel(node) {
  if (!node.healthy) {
    return ROLE_LABELS.unhealthy;
  }
  return ROLE_LABELS[node.role] || '未知';
}

function roleClass(node) {
  if (!node.healthy) {
    return 'unhealthy';
  }
  return node.role === 'active' ? 'active' : '';
}

function createMeter(label, metrics) {
  const meter = document.createElement('div');
  meter.className = 'bili-network-meter';

  const labelRow = document.createElement('div');
  labelRow.className = 'bili-network-meter-label';
  const name = document.createElement('span');
  name.textContent = label;
  const ratio = document.createElement('span');
  ratio.textContent = formatSpeedRatio(metrics.speed);
  labelRow.append(name, ratio);

  const value = document.createElement('div');
  value.className = 'bili-network-meter-value';
  value.textContent = formatNetworkRate(metrics.bitrateKbps);

  const total = document.createElement('div');
  total.className = 'bili-network-total';
  total.textContent = `Total ${formatBytes(metrics.totalBytes)}`;

  meter.append(labelRow, value, total);

  if (metrics.extra) {
    const extra = document.createElement('div');
    extra.className = 'bili-network-total';
    extra.textContent = metrics.extra;
    meter.appendChild(extra);
  }

  return meter;
}

/// Meters, never the bar graph: that needs per-sample history and a fast poll,
/// which is exactly what a public page should not be doing.
function createNetwork(network) {
  if (!network) {
    return null;
  }
  const hasPush = Number.isFinite(network.stream_bitrate_kbps) || Number.isFinite(network.stream_speed);
  const hasCache = network.hls_cache_active
    && (Number.isFinite(network.stream_cache_bitrate_kbps) || Number.isFinite(network.stream_cache_speed));
  if (!hasPush && !hasCache) {
    return null;
  }

  const panel = document.createElement('div');
  panel.className = 'bili-network-meters';

  if (hasCache) {
    panel.appendChild(createMeter('Cache RX', {
      bitrateKbps: network.stream_cache_bitrate_kbps,
      speed: network.stream_cache_speed,
      totalBytes: network.stream_cache_total_bytes,
    }));
  }
  if (hasPush) {
    panel.appendChild(createMeter('RTMP TX', {
      bitrateKbps: network.stream_bitrate_kbps,
      speed: network.stream_speed,
      totalBytes: network.stream_total_bytes,
      extra: `FPS ${formatFps(network.stream_fps)} / Frame ${formatFrameCount(network.stream_frame)}`,
    }));
  }

  return panel;
}

function createNodeTile(node) {
  const tile = document.createElement('div');
  tile.className = 'cluster-node-tile';

  const head = document.createElement('div');
  head.className = 'cluster-node-tile-head';

  const name = document.createElement('span');
  name.className = 'cluster-node-name';
  name.textContent = node.name || '-';

  const badge = document.createElement('span');
  badge.className = `cluster-badge ${roleClass(node)}`.trim();
  badge.textContent = roleLabel(node);

  head.append(name, badge);
  tile.appendChild(head);

  if (node.ffmpeg_running) {
    const streaming = document.createElement('span');
    streaming.className = 'cluster-node-tile-seen';
    streaming.textContent = '推流进行中';
    tile.appendChild(streaming);
  }

  const network = createNetwork(node.network);
  if (network) {
    tile.appendChild(network);
  }

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

  const fragment = document.createDocumentFragment();
  for (const node of nodes) {
    fragment.appendChild(createNodeTile(node));
  }
  list.replaceChildren(fragment);
}

function emptyState(message) {
  const empty = document.createElement('div');
  empty.className = 'cluster-empty';
  empty.textContent = message;
  return empty;
}
