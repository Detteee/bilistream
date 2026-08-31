// status-cards.js — renderers for the Bilibili / YouTube / Twitch / Niconico
// status cards. Shared by the admin dashboard and the public status page, so
// the two never drift. Pure painting: no fetching, no config writes.
//
// Every lookup is null-safe on purpose — the public page ships a trimmed
// markup (no quality / crop / HLS cache rows, no platform-card graph) and
// simply omits the elements it does not want. Node cards still paint the
// shared 60s history plot from the last heartbeat.

import { setElementDisplay, setElementText } from './dom.js';
import {
  asBitrateHistory,
  formatAreaText,
  formatFps,
  formatHlsCacheStatus,
  formatNetworkRate,
  formatScheduledStart,
  formatSpeedRatio,
  formatStreamTime,
  getQualityDisplayText,
} from './format.js';

const biliNetworkHistoryLimit = 60;
// One column per second, matching ffmpeg NETWORK_HISTORY_SAMPLE_MS.

let lastBiliNetworkLive = false;
let lastBiliNetworkQuality = null;

/// True while this node runs a publisher, including a stalled publisher.
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
      : platform === 'niconico'
        ? ['nc-title-row', 'nc-live-id-row']
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

/// The dashboard sets the monitor switches from config, with its own debounced
/// save, so the renderer must not fight it there. A read-only page has no
/// config to read, so it takes the value out of the status payload instead.
function monitorToggleValue(enabled, options) {
  return options.readonly ? enabled : undefined;
}

// The public page renders every switch as a locked, greyed control rather than
// hiding it, so viewers can still see what is turned on.
function applyToggle(id, checked, readonly) {
  const toggle = document.getElementById(id);
  if (!toggle) {
    return;
  }
  if (typeof checked === 'boolean' && (readonly || toggle.dataset.saving !== 'true')) {
    toggle.checked = checked;
  }
  if (readonly) {
    toggle.disabled = true;
    toggle.setAttribute('aria-disabled', 'true');
    toggle.closest('.toggle-switch')?.classList.add('is-locked');
  }
}

function networkBarHeight(value, maxRate, heightScale) {
  if (!(value > 0) || !(maxRate > 0)) {
    return 0;
  }
  return Math.max(2, Math.round((value / maxRate) * heightScale));
}

function setNodeText(target, value) {
  if (!target) {
    return;
  }
  if (typeof target === 'string') {
    setElementText(target, value);
    return;
  }
  target.textContent = value;
}

function setNetworkBarHeight(bar, heightPercent) {
  const visible = heightPercent > 0;
  bar.style.height = visible ? `${heightPercent}%` : '0';
  bar.classList.toggle('active', visible);
}

function createBiliNetworkBar(type, heightPercent) {
  const bar = document.createElement('span');
  bar.className = `bili-network-bar ${type}`;
  setNetworkBarHeight(bar, heightPercent);
  return bar;
}

function createNetworkColumn(showCache) {
  const column = document.createElement('span');
  column.className = 'bili-network-column';
  if (showCache) {
    column.appendChild(createBiliNetworkBar('cache', 0));
  }
  column.appendChild(createBiliNetworkBar('push', 0));
  return column;
}

function paintNetworkColumn(column, showCache, cacheValue, pushValue, maxRate, heightScale) {
  const bars = column.children;
  if (showCache) {
    setNetworkBarHeight(bars[0], networkBarHeight(cacheValue, maxRate, heightScale));
    setNetworkBarHeight(bars[1], networkBarHeight(pushValue, maxRate, heightScale));
    return;
  }
  setNetworkBarHeight(bars[0], networkBarHeight(pushValue, maxRate, heightScale));
}

/// Paints a 60s × 1 Hz RX/TX plot. History is sampled on the streaming node;
/// cluster and public pages redraw it at their own poll, not at 1 Hz.
export function paintNetworkGraph(graph, options = {}) {
  if (!graph) {
    return;
  }

  const showCache = !!options.showCache;
  const pushHistory = asBitrateHistory(options.pushHistory);
  const cacheHistory = showCache ? asBitrateHistory(options.cacheHistory) : [];

  // Cache on: mirrored halves. Cache off: full-height single-sided push bars.
  graph.classList.toggle('single-sided', !showCache);

  const activeSeries = showCache
    ? cacheHistory.concat(pushHistory)
    : pushHistory;
  const maxRate = Math.max(1, ...activeSeries);
  setNodeText(options.scaleEl, formatNetworkRate(maxRate));
  // One column per second for the whole 60s window. Halving that on a
  // 520px viewport made a full-width plot draw a handful of fat bars.
  const graphWidth = biliNetworkHistoryLimit;
  setNodeText(options.windowEl, `−${graphWidth}s`);
  const pushSeries = sliceNetworkHistory(pushHistory, graphWidth);
  const cacheSeries = sliceNetworkHistory(cacheHistory, graphWidth);
  const heightScale = showCache ? 50 : 100;

  const needsRebuild = graph.childElementCount !== graphWidth
    || graph.dataset.cache !== String(!!showCache);
  if (needsRebuild) {
    graph.dataset.cache = String(!!showCache);
    const fragment = document.createDocumentFragment();
    for (let i = 0; i < graphWidth; i += 1) {
      fragment.appendChild(createNetworkColumn(showCache));
    }
    graph.replaceChildren(fragment);
  }

  for (let i = 0; i < graphWidth; i += 1) {
    const pushValue = pushSeries[i - (graphWidth - pushSeries.length)] || 0;
    const cacheValue = cacheSeries[i - (graphWidth - cacheSeries.length)] || 0;
    paintNetworkColumn(
      graph.children[i],
      showCache,
      cacheValue,
      pushValue,
      maxRate,
      heightScale
    );
  }
}

function renderBiliNetworkGraph(showCache, pushHistory, cacheHistory) {
  paintNetworkGraph(document.getElementById('bili-network-graph'), {
    showCache,
    pushHistory,
    cacheHistory,
    scaleEl: 'bili-network-scale',
    windowEl: 'bili-network-window',
  });
}

function historyHasSignal(series) {
  return series.some((value) => value > 0);
}

/// Graph + axis for a node card. Skips the plot when this snapshot has no
/// samples yet, so a brand-new restream is meters-only until the first window.
export function appendNetworkHistoryPlot(parent, network, options = {}) {
  const showCache = !!options.showCache;
  const pushHistory = asBitrateHistory(network?.stream_bitrate_history);
  const cacheHistory = showCache ? asBitrateHistory(network?.stream_cache_bitrate_history) : [];
  if (!historyHasSignal(pushHistory) && !historyHasSignal(cacheHistory)) {
    return false;
  }

  const graph = document.createElement('div');
  graph.className = 'bili-network-graph';

  const axis = document.createElement('div');
  axis.className = 'bili-network-graph-axis';
  axis.setAttribute('aria-hidden', 'true');

  const windowEl = document.createElement('span');
  const scaleEl = document.createElement('span');
  scaleEl.className = 'bili-network-scale';
  const nowEl = document.createElement('span');
  nowEl.textContent = 'now';
  axis.append(windowEl, scaleEl, nowEl);

  parent.append(graph, axis);
  paintNetworkGraph(graph, {
    showCache,
    pushHistory,
    cacheHistory,
    scaleEl,
    windowEl,
  });
  return true;
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

/// Paints the platform-card meters, and the bar graph when `options.showGraph`
/// is set. The public Bilibili card still omits that graph; node cards paint
/// the same 60s window from the heartbeat snapshot instead.
export function renderBiliNetworkPanel(bili, options = {}) {
  const { showGraph = true } = options;
  const panel = document.getElementById('bili-network-panel');
  if (!panel) {
    return;
  }

  lastBiliNetworkLive = bili.ffmpeg_running === true;
  lastBiliNetworkQuality = lastBiliNetworkLive
    ? (Number.isFinite(bili.stream_speed)
      ? (bili.stream_speed > 0.97 ? '流畅' : bili.stream_speed > 0.94 ? '波动' : '卡顿')
      : bili.stream_quality || null)
    : null;
  const hasCache = !!bili.hls_cache_active;
  if (!lastBiliNetworkLive) {
    panel.classList.add('hidden');
    document.getElementById('bili-network-graph')?.replaceChildren();
    return;
  }

  panel.classList.remove('hidden');
  panel.classList.toggle('single-sided', !hasCache);

  const quality = document.getElementById('bili-network-quality');
  if (quality) {
    quality.textContent = lastBiliNetworkQuality || '等待推流数据';
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

  if (showGraph) {
    renderBiliNetworkGraph(
      hasCache,
      asBitrateHistory(bili.stream_bitrate_history),
      hasCache ? asBitrateHistory(bili.stream_cache_bitrate_history) : []
    );
  }
}

export function renderBilibiliCard(bili, options = {}) {
  const { readonly = false, showNetwork = true } = options;

  setStatusIndicator('bili-status', bili.is_live ? 'status-live' : 'status-offline');
  updateAppLiveBadge(bili.is_live);
  setElementText('bili-title', bili.title || '-');
  setElementText('bili-area', formatAreaText(bili.area_name, bili.area_id));

  if (showNetwork) {
    renderBiliNetworkPanel(bili, options);
  } else {
    lastBiliNetworkLive = typeof bili.is_live === 'boolean' ? bili.is_live : lastBiliNetworkLive;
  }

  applyToggle('bili-danmaku-command-toggle', bili.enable_danmaku_command, readonly);
}

export function renderYouTubeCard(yt, options = {}) {
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
  applyToggle('youtube-monitor-toggle', monitorToggleValue(yt.enable_monitor, options), options.readonly);
}

export function renderTwitchCard(tw, options = {}) {
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
  setElementText('tw-game', tw.game || tw.topic || '-');
  setElementText('tw-area', formatAreaText(tw.area_name, tw.area_id));
  setElementText('tw-quality', tw.quality ? getQualityDisplayText(tw.quality, 'twitch') : '-');
  setElementText('tw-crop-status', tw.crop_enabled ? '开启' : '关闭');
  setElementText('tw-hls-cache-status', formatHlsCacheStatus(tw.ffmpeg_cache_enabled, tw.ffmpeg_cache_latency_secs));
  applyToggle('twitch-monitor-toggle', monitorToggleValue(tw.enable_monitor, options), options.readonly);
}

/// The channel id rides along on the name element's dataset so the card has
/// one row instead of two.
export function setNcChannelDisplay(name, channelId) {
  const channelSpan = document.getElementById('nc-channel-name');
  if (!channelSpan) {
    return;
  }
  channelSpan.textContent = name || '-';
  if (channelId) {
    channelSpan.dataset.channelId = channelId;
  } else {
    delete channelSpan.dataset.channelId;
  }
}

export function renderNiconicoCard(nc, options = {}) {
  const scheduledRow = document.getElementById('nc-scheduled-row');

  if (!nc) {
    setStatusIndicator('nc-status', 'status-offline');
    setPlatformLiveInfoVisibility('niconico', false);
    if (scheduledRow) {
      scheduledRow.style.display = 'none';
    }
    setNcChannelDisplay('-', '');
    setElementText('nc-live-id', '-');
    setElementText('nc-title', '-');
    setElementText('nc-scheduled', '-');
    setElementText('nc-area', '-');
    setElementText('nc-quality', '-');
    setElementText('nc-crop-status', '关闭');
    setElementText('nc-hls-cache-status', '关闭');
    return;
  }

  // An upcoming 放送予定 is worth showing even though it is not on air yet.
  const ncScheduled = !nc.is_live && !!(nc.scheduled_start || (nc.title && nc.live_id));
  setStatusIndicator(
    'nc-status',
    nc.is_live ? 'status-live' : ncScheduled ? 'status-scheduled' : 'status-offline'
  );
  setPlatformLiveInfoVisibility('niconico', nc.is_live || ncScheduled);
  if (scheduledRow) {
    scheduledRow.style.display = nc.scheduled_start && !nc.is_live ? '' : 'none';
  }
  setNcChannelDisplay(nc.channel_name, nc.channel_id);
  setElementText('nc-live-id', nc.live_id || '-');
  setElementText('nc-title', nc.title || '-');
  setElementText('nc-scheduled', nc.scheduled_start ? formatScheduledStart(nc.scheduled_start) : '-');
  setElementText('nc-area', formatAreaText(nc.area_name, nc.area_id));
  setElementText('nc-quality', nc.quality ? getQualityDisplayText(nc.quality, 'niconico') : '-');
  setElementText('nc-crop-status', nc.crop_enabled ? '开启' : '关闭');
  setElementText('nc-hls-cache-status', formatHlsCacheStatus(nc.ffmpeg_cache_enabled, nc.ffmpeg_cache_latency_secs));
  applyToggle('niconico-monitor-toggle', monitorToggleValue(nc.enable_monitor, options), options.readonly);
}

/// Paints every platform card from one `/api/status` style payload.
export function renderStatusCards(status, options = {}) {
  renderBilibiliCard(status.bilibili || {}, options);
  renderYouTubeCard(status.youtube, options);
  renderTwitchCard(status.twitch, options);
  renderNiconicoCard(status.niconico, options);
}

/// Replaces the channel/title fields with a short message when the status
/// payload could not be read at all.
export function setStatusCardsMessage(message) {
  for (const id of ['bili-status', 'yt-status', 'tw-status']) {
    setStatusIndicator(id, 'status-offline');
  }
  updateAppLiveBadge(false);
  setElementText('app-live-badge-text', '连接中断');
  renderBiliNetworkPanel({ ffmpeg_running: false });
  setElementText('bili-title', message);
  setElementText('yt-channel-name', message);
  setElementText('tw-channel-name', message);
  setElementText('nc-channel-name', message);
}
