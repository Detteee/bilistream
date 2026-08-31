// status-cards.js — renderers for the Bilibili / YouTube / Twitch status cards.
// status cards. Shared by the admin dashboard and the public status page, so
// the two never drift. Pure painting: no fetching, no config writes.
//
// Every lookup is null-safe on purpose — the public page ships a trimmed
// markup (no quality / crop / HLS cache rows, no network graph) and simply
// omits the elements it does not want.

import { setElementDisplay, setElementText } from './dom.js';
import {
  asBitrateHistory,
  formatAreaText,
  formatFps,
  formatHlsCacheStatus,
  formatNetworkRate,
  formatSpeedRatio,
  formatStreamTime,
  getQualityDisplayText,
} from './format.js';

const biliNetworkHistoryLimit = 60;

let lastBiliNetworkLive = false;
let lastBiliNetworkQuality = null;

/// True while the Bilibili room is known to be live, so callers can skip
/// polling the network endpoint when there is nothing to draw.
export function isBiliNetworkLive() {
  return lastBiliNetworkLive;
}

export function getBiliNetworkQuality() {
  return lastBiliNetworkQuality;
}

function setStatusIndicator(id, stateClass) {
  const indicator = document.getElementById(id);
  if (indicator) {
    indicator.className = `status-indicator ${stateClass}`;
  }
}

// Mirrors the Bilibili room state into the top bar so the current state
// is readable from every view. Absent on the public page.
export function updateAppLiveBadge(isLive) {
  const badge = document.getElementById('app-live-badge');
  const text = document.getElementById('app-live-badge-text');
  if (!badge) return;

  badge.classList.toggle('is-live', !!isLive);
  if (text) {
    text.textContent = isLive ? '直播中' : '未开播';
  }
}

export function setPlatformLiveInfoVisibility(platform, isLive) {
  const rowIds = platform === 'youtube'
    ? ['yt-title-row', 'yt-topic-row']
    : platform === 'twitch'
      ? ['tw-title-row', 'tw-game-row']
      : platform === 'priority'
        ? ['priority-title-row']
        : [];
  for (const id of rowIds) {
    const row = document.getElementById(id);
    if (row) {
      row.style.display = isLive ? '' : 'none';
    }
  }
}


function applyBiliStreamQualityColor(element, quality) {
  element.classList.toggle('bili-network-quality-smooth', quality === '流畅');
  element.classList.toggle('bili-network-quality-unstable', quality === '波动');
  element.classList.toggle('bili-network-quality-stalled', quality === '卡顿');
}

function sliceNetworkHistory(series, width) {
  const start = Math.max(0, series.length - width);
  return series.slice(start);
}

function setToggleChecked(id, checked) {
  const toggle = document.getElementById(id);
  if (toggle && typeof checked === 'boolean') {
    toggle.checked = checked;
  }
}

function createBiliNetworkBar(type, heightPercent) {
  const bar = document.createElement('span');
  bar.className = `bili-network-bar ${type} active`;
  bar.style.height = `${heightPercent}%`;
  return bar;
}

function renderBiliNetworkGraph(showCache, pushHistory, cacheHistory) {
  const graph = document.getElementById('bili-network-graph');
  if (!graph) {
    return;
  }

  // Cache on: mirrored halves. Cache off: full-height single-sided push bars.
  graph.classList.toggle('single-sided', !showCache);

  const activeSeries = showCache
    ? cacheHistory.concat(pushHistory)
    : pushHistory;
  const maxRate = Math.max(1, ...activeSeries);
  setElementText('bili-network-scale', `Scale ${formatNetworkRate(maxRate)}`);
  const graphWidth = window.matchMedia('(max-width: 520px)').matches ? 32 : biliNetworkHistoryLimit;
  const pushSeries = sliceNetworkHistory(pushHistory, graphWidth);
  const cacheSeries = sliceNetworkHistory(cacheHistory, graphWidth);
  const heightScale = showCache ? 50 : 100;
  const fragment = document.createDocumentFragment();

  for (let i = 0; i < graphWidth; i += 1) {
    const pushValue = pushSeries[i - (graphWidth - pushSeries.length)] || 0;
    const cacheValue = cacheSeries[i - (graphWidth - cacheSeries.length)] || 0;
    const cacheHeight = showCache ? Math.max(2, Math.round((cacheValue / maxRate) * heightScale)) : 0;
    const pushHeight = Math.max(2, Math.round((pushValue / maxRate) * heightScale));

    const column = document.createElement('span');
    column.className = 'bili-network-column';
    if (showCache) {
      column.appendChild(createBiliNetworkBar('cache', cacheHeight));
    }
    column.appendChild(createBiliNetworkBar('push', pushHeight));
    fragment.appendChild(column);
  }

  graph.replaceChildren(fragment);
}

function applySpeedTone(element, speed) {
  if (!element) {
    return;
  }
  if (!Number.isFinite(speed) || speed <= 0) {
    delete element.dataset.tone;
    return;
  }
  element.dataset.tone = speed > 0.97 ? 'ok' : speed > 0.94 ? 'warn' : 'danger';
}

function meterDetail(timeSecs, fps) {
  const time = formatStreamTime(timeSecs);
  if (!Number.isFinite(fps) || fps < 0) {
    return time;
  }
  return `${time} · ${formatFps(fps)} fps`;
}

function updateBiliNetworkMeter(kind, metrics) {
  setElementText(`bili-network-${kind}-rate`, formatNetworkRate(metrics.bitrateKbps));
  const speedEl = document.getElementById(`bili-network-${kind}-speed-ratio`);
  if (speedEl) {
    speedEl.textContent = formatSpeedRatio(metrics.speed);
    applySpeedTone(speedEl, metrics.speed);
  }
  setElementText(`bili-network-${kind}-time`, metrics.detail);
}

/// Paints the network meters and the bar graph.
export function renderBiliNetworkPanel(bili) {
  const panel = document.getElementById('bili-network-panel');
  if (!panel) {
    return;
  }

  lastBiliNetworkLive = typeof bili.is_live === 'boolean' ? bili.is_live : lastBiliNetworkLive;
  lastBiliNetworkQuality = bili.stream_quality || lastBiliNetworkQuality;
  const hasPush = Number.isFinite(bili.stream_bitrate_kbps)
    || Number.isFinite(bili.stream_speed)
    || Number.isFinite(bili.stream_fps)
    || Number.isFinite(bili.stream_time_secs)
    || Number.isFinite(bili.stream_frame);
  const hasCache = bili.hls_cache_active && (
    Number.isFinite(bili.stream_cache_bitrate_kbps)
    || Number.isFinite(bili.stream_cache_speed)
    || Number.isFinite(bili.stream_cache_time_secs)
  );
  if (!lastBiliNetworkLive || (!hasPush && !hasCache && !lastBiliNetworkQuality)) {
    panel.classList.add('hidden');
    return;
  }

  panel.classList.remove('hidden');

  const quality = document.getElementById('bili-network-quality');
  if (quality) {
    quality.textContent = lastBiliNetworkQuality || 'Live';
    applyBiliStreamQualityColor(quality, lastBiliNetworkQuality);
  }

  updateBiliNetworkMeter('push', {
    bitrateKbps: bili.stream_bitrate_kbps,
    speed: bili.stream_speed,
    detail: meterDetail(bili.stream_time_secs, bili.stream_fps)
  });

  const cacheMeter = document.getElementById('bili-network-cache-meter');
  setElementDisplay(cacheMeter, hasCache, '');
  if (hasCache) {
    updateBiliNetworkMeter('cache', {
      bitrateKbps: bili.stream_cache_bitrate_kbps,
      speed: bili.stream_cache_speed,
      detail: formatStreamTime(bili.stream_cache_time_secs)
    });
  }

  renderBiliNetworkGraph(
    hasCache,
    asBitrateHistory(bili.stream_bitrate_history),
    hasCache ? asBitrateHistory(bili.stream_cache_bitrate_history) : []
  );
}

export function renderBilibiliCard(bili) {
  setStatusIndicator('bili-status', bili.is_live ? 'status-live' : 'status-offline');
  updateAppLiveBadge(bili.is_live);
  setElementText('bili-title', bili.title || '-');
  setElementText('bili-area', formatAreaText(bili.area_name, bili.area_id));

  renderBiliNetworkPanel(bili);

  setToggleChecked('bili-danmaku-command-toggle', bili.enable_danmaku_command);
}

export function renderYouTubeCard(yt) {
  if (!yt) {
    setStatusIndicator('yt-status', 'status-offline');
    setPlatformLiveInfoVisibility('youtube', false);
    setElementText('yt-channel-name', '-');
    setElementText('yt-title', '-');
    setElementText('yt-topic', '-');
    setElementText('yt-area', '-');
    setElementText('yt-quality', '-');
    setElementText('yt-crop-status', '关闭');
    setElementText('yt-hls-cache-status', '关闭');
    return;
  }

  setStatusIndicator('yt-status', yt.is_live ? 'status-live' : 'status-offline');
  setPlatformLiveInfoVisibility('youtube', yt.is_live);
  setElementText('yt-channel-name', yt.channel_name || '-');
  setElementText('yt-title', yt.title || '-');
  setElementText('yt-topic', yt.topic || '-');
  setElementText('yt-area', formatAreaText(yt.area_name, yt.area_id));
  setElementText('yt-quality', yt.quality ? getQualityDisplayText(yt.quality, 'youtube') : '-');
  setElementText('yt-crop-status', yt.crop_enabled ? '开启' : '关闭');
  setElementText('yt-hls-cache-status', formatHlsCacheStatus(yt.ffmpeg_cache_enabled, yt.ffmpeg_cache_latency_secs));
}

export function renderTwitchCard(tw) {
  if (!tw) {
    setStatusIndicator('tw-status', 'status-offline');
    setPlatformLiveInfoVisibility('twitch', false);
    setElementText('tw-channel-name', '-');
    setElementText('tw-title', '-');
    setElementText('tw-game', '-');
    setElementText('tw-area', '-');
    setElementText('tw-quality', '-');
    setElementText('tw-crop-status', '关闭');
    setElementText('tw-hls-cache-status', '关闭');
    return;
  }

  setStatusIndicator('tw-status', tw.is_live ? 'status-live' : 'status-offline');
  setPlatformLiveInfoVisibility('twitch', tw.is_live);
  setElementText('tw-channel-name', tw.channel_name || '-');
  setElementText('tw-title', tw.title || '-');
  setElementText('tw-game', tw.game || '-');
  setElementText('tw-area', formatAreaText(tw.area_name, tw.area_id));
  setElementText('tw-quality', tw.quality ? getQualityDisplayText(tw.quality, 'twitch') : '-');
  setElementText('tw-crop-status', tw.crop_enabled ? '开启' : '关闭');
  setElementText('tw-hls-cache-status', formatHlsCacheStatus(tw.ffmpeg_cache_enabled, tw.ffmpeg_cache_latency_secs));
}

/// Paints every platform card from one `/api/status` style payload.
export function renderStatusCards(status) {
  renderBilibiliCard(status.bilibili || {});
  renderYouTubeCard(status.youtube);
  renderTwitchCard(status.twitch);
}

/// Replaces the channel/title fields with a short message when the status
/// payload could not be read at all.
export function setStatusCardsMessage(message) {
  setElementText('bili-title', message);
  setElementText('yt-channel-name', message);
  setElementText('tw-channel-name', message);
}
