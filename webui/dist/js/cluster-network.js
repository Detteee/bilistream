// Network detail shared by admin and public node cards. A node's publisher
// lifecycle owns visibility; old rate samples cannot keep an idle card alive.
import {
  biliRoomStats,
  formatCount,
  formatFps,
  formatLiveClock,
  formatLiveDuration,
  formatNetworkRate,
  formatSpeedRatio,
  formatStreamTime,
} from './format.js';
import { mountNetworkHistory, paintNetworkGraph } from './status-cards.js';

const panels = new WeakMap();

function text(element, value) {
  if (element && element.textContent !== value) element.textContent = value;
}

function history(series) {
  return Array.isArray(series) ? series.slice(-60).map(value => Number.isFinite(value) && value > 0 ? value : 0) : [];
}

function liveStat(label, value) {
  const item = document.createElement('span');
  item.className = 'bili-live-stat';
  const valueEl = document.createElement('span');
  valueEl.className = 'bili-live-stat-value';
  valueEl.textContent = value;
  const labelEl = document.createElement('span');
  labelEl.className = 'bili-live-stat-label';
  labelEl.textContent = label;
  item.append(valueEl, labelEl);
  return item;
}

function paintClusterLiveStats(panel, node) {
  const { online, liveStartTs } = biliRoomStats(node);
  let stats = panel.querySelector(':scope > .cluster-live-stats');
  if (online == null && liveStartTs == null) {
    stats?.remove();
    return;
  }
  if (!stats) {
    stats = document.createElement('div');
    stats.className = 'bili-live-stats cluster-live-stats';
    stats.setAttribute('aria-label', '人气与开播时长');
  }
  if (panel.firstChild !== stats) {
    panel.insertBefore(stats, panel.firstChild);
  }
  const parts = [];
  if (online != null) {
    parts.push(liveStat('人气', formatCount(online)));
  }
  if (liveStartTs != null) {
    const clock = formatLiveClock(liveStartTs);
    parts.push(liveStat(clock ? `开播 ${clock}` : '开播', formatLiveDuration(liveStartTs)));
  }
  stats.replaceChildren(...parts);
}

function createMeter(label, leg) {
  const meter = document.createElement('div');
  meter.className = 'bili-network-meter';
  meter.dataset.leg = leg;
  const title = document.createElement('div');
  title.className = 'bili-network-meter-label';
  const name = document.createElement('span');
  name.textContent = label;
  const value = document.createElement('div');
  value.className = 'bili-network-meter-value';
  const rate = document.createElement('span');
  const speed = document.createElement('span');
  speed.className = 'bili-network-meter-speed';
  if (leg === 'rx') {
    title.append(name, speed);
    value.append(rate);
  } else {
    title.append(name);
    value.append(rate, speed);
  }
  const detail = document.createElement('div');
  detail.className = 'bili-network-total';
  meter.append(title, value, detail);
  return { meter, rate, speed, detail };
}

function updateMeter(parts, network, cache) {
  if (!parts) return;
  const rate = cache ? network.stream_cache_bitrate_kbps : network.stream_bitrate_kbps;
  const speed = cache ? network.stream_cache_speed : network.stream_speed;
  const seconds = cache ? network.stream_cache_time_secs : network.stream_time_secs;
  text(parts.rate, rate === 0 ? '0 Kb/s' : formatNetworkRate(rate));
  text(parts.speed, speed === 0 ? '0.00x' : formatSpeedRatio(speed));
  parts.speed.dataset.tone = Number.isFinite(speed)
    ? speed > 0.97 ? 'ok' : speed > 0.94 ? 'warn' : 'danger'
    : '';
  const fps = !cache && Number.isFinite(network.stream_fps) && network.stream_fps >= 0
    ? ` · ${formatFps(network.stream_fps)} fps` : '';
  text(parts.detail, `${formatStreamTime(seconds)}${fps}`);
}

export function createClusterNetwork(node) {
  if (node.ffmpeg_running !== true || node.health?.stale === true) return null;
  const panel = document.createElement('div');
  panel.className = 'cluster-node-network';
  updateClusterNetwork(panel, node);
  return panel;
}

export function updateClusterNetwork(panel, node) {
  if (!panel) return;
  if (node.ffmpeg_running !== true || node.health?.stale === true) {
    panel.remove();
    return;
  }
  const network = node.network || {};
  const showCache = !!network.hls_cache_active;
  const pushHistory = history(network.stream_bitrate_history);
  const cacheHistory = showCache ? history(network.stream_cache_bitrate_history) : [];
  const shape = `${showCache}:${pushHistory.some(value => value > 0)}:${cacheHistory.some(value => value > 0)}`;
  let parts = panels.get(panel);
  if (!parts || parts.shape !== shape) {
    const meters = document.createElement('div');
    meters.className = 'bili-network-meters';
    const push = createMeter('RTMP TX', 'tx');
    const cache = showCache ? createMeter('HLS Cache', 'rx') : null;
    meters.appendChild(push.meter);
    if (cache) meters.appendChild(cache.meter);
    panel.replaceChildren(meters);
    mountNetworkHistory(panel, { stream_bitrate_history: pushHistory, stream_cache_bitrate_history: cacheHistory }, {
      showCache, pushMeter: push.meter, cacheMeter: cache?.meter,
    });
    parts = { shape, push, cache, historySignature: null };
    panels.set(panel, parts);
  }
  updateMeter(parts.push, network, false);
  updateMeter(parts.cache, network, true);
  paintClusterLiveStats(panel, node);

  const historySignature = JSON.stringify([pushHistory, cacheHistory]);
  if (parts.historySignature === historySignature) return;
  parts.historySignature = historySignature;
  const maxRate = Math.max(1, ...pushHistory, ...cacheHistory);
  const mirror = panel.querySelector('.cluster-network-mirror');
  if (mirror) {
    paintNetworkGraph(mirror.querySelector('.bili-network-graph'), {
      showCache, pushHistory, cacheHistory, maxRate,
      scaleEl: mirror.querySelector('.bili-network-scale'),
      windowEl: mirror.querySelector('.bili-network-graph-axis > span'),
    });
  }
  for (const [series, meter] of [['push', parts.push], ['cache', parts.cache]]) {
    paintNetworkGraph(meter?.meter.closest('.cluster-network-row')?.querySelector('.cluster-network-spark'), {
      series, pushHistory, cacheHistory, maxRate,
    });
  }
}
