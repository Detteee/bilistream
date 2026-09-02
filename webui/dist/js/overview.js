// overview.js — extracted from app.js

import { isDashboardVisible, isElementHidden, setElementDisplay, createSvgIcon, parseInteger, setElementText, showNotification, setButtonLoading } from './dom.js';
import { state, mergeConfigData, updateMonitorToggleStates, updateDanmakuCommandToggle, isViewActive, createAreaOption, createSelectOption, normalizeAreaData, getAreaList, getSortedAreas, appendAreaOptions, appendPlatformChannelOptions } from './state.js';
import { managementRequest, managementJsonRequest, getJson, postJsonApi } from './api.js';
import { eventStreamHealthy } from './events.js';
import { loadChannels } from './manage.js';
import { toggleDanmakuCommand } from './settings.js';
import { openCropConfig, clearCropConfig, loadCapturedCropFrame } from './crop.js';
import {
  formatHlsCacheStatus,
  formatScheduledStart as formatHolodexScheduledStart,
  getQualityDisplayText,
} from './format.js';
import {
  getBiliNetworkQuality,
  isBiliNetworkLive,
  renderBiliNetworkPanel,
  renderStatusCards,
  setPlatformLiveInfoVisibility,
  setStatusCardsMessage,
} from './status-cards.js';

let statusRefreshInterval = 60000; // Default 60 seconds
const networkRefreshInterval = 1000;
let statusIntervalId = null;
let networkRefreshIntervalId = null;
let statusRefreshInFlight = false;
let statusRefreshQueued = false;
let networkRefreshInFlight = false;
let faceAuthUrl = null;
let holodexCurrentSource = 'channels';
let holodexStreamsRequested = false;
const monitorToggleSaveDebounceMs = 160;
function initDashboardControls() {
  document
    .getElementById('startLiveBtn')
    ?.addEventListener('click', startStream);
  document
    .getElementById('stopLiveBtn')
    ?.addEventListener('click', stopStream);
  document
    .getElementById('restartStreamBtn')
    ?.addEventListener('click', restartStream);
  document
    .getElementById('refreshBilibiliBtn')
    ?.addEventListener('click', refreshBilibiliStatus);
  document
    .getElementById('youtube-monitor-toggle')
    ?.addEventListener('change', toggleYouTubeMonitor);
  document
    .getElementById('refreshYouTubeBtn')
    ?.addEventListener('click', refreshYouTubeStatus);
  document
    .getElementById('twitch-monitor-toggle')
    ?.addEventListener('change', toggleTwitchMonitor);
  document
    .getElementById('refreshTwitchBtn')
    ?.addEventListener('click', refreshTwitchStatus);
  document
    .getElementById('refreshHolodexBtn')
    ?.addEventListener('click', refreshHolodexStreams);
  document.addEventListener('areas-json-changed', () => {
    if (holodexStreamsRequested) {
      refreshHolodexStreams();
    }
  });
  document
    .getElementById('bili-title-edit-btn')
    ?.addEventListener('click', toggleTitleEdit);
  document
    .getElementById('bili-title-save-btn')
    ?.addEventListener('click', saveTitleEdit);
  document
    .getElementById('bili-title-cancel-btn')
    ?.addEventListener('click', cancelTitleEdit);
  document
    .getElementById('title-edit-input')
    ?.addEventListener('keydown', handleTitleEditKeydown);
  document
    .getElementById('bili-area-edit-btn')
    ?.addEventListener('click', toggleAreaEdit);
  document
    .getElementById('bili-area-save-btn')
    ?.addEventListener('click', saveAreaEdit);
  document
    .getElementById('bili-area-cancel-btn')
    ?.addEventListener('click', cancelAreaEdit);
  document
    .getElementById('bili-danmaku-command-toggle')
    ?.addEventListener('change', toggleDanmakuCommand);
  document
    .getElementById('yt-channel-edit-btn')
    ?.addEventListener('click', toggleYtChannelEdit);
  document
    .getElementById('yt-channel-save-btn')
    ?.addEventListener('click', saveYtChannelEdit);
  document
    .getElementById('yt-channel-cancel-btn')
    ?.addEventListener('click', cancelYtChannelEdit);
  document
    .getElementById('yt-area-edit-btn')
    ?.addEventListener('click', toggleYtAreaEdit);
  document
    .getElementById('yt-area-save-btn')
    ?.addEventListener('click', saveYtAreaEdit);
  document
    .getElementById('yt-area-cancel-btn')
    ?.addEventListener('click', cancelYtAreaEdit);
  document
    .getElementById('yt-quality-edit-btn')
    ?.addEventListener('click', toggleYtQualityEdit);
  document
    .getElementById('yt-quality-save-btn')
    ?.addEventListener('click', saveYtQualityEdit);
  document
    .getElementById('yt-quality-cancel-btn')
    ?.addEventListener('click', cancelYtQualityEdit);
  document
    .getElementById('yt-crop-edit-btn')
    ?.addEventListener('click', () => openCropConfig('youtube'));
  document
    .getElementById('yt-crop-clear-btn')
    ?.addEventListener('click', () => clearCropConfig('youtube'));
  document
    .getElementById('yt-hls-cache-edit-btn')
    ?.addEventListener('click', toggleYtHlsCacheEdit);
  document
    .getElementById('yt-hls-cache-save-btn')
    ?.addEventListener('click', saveYtHlsCacheEdit);
  document
    .getElementById('yt-hls-cache-cancel-btn')
    ?.addEventListener('click', cancelYtHlsCacheEdit);
  document
    .getElementById('yt-hls-cache-enabled')
    ?.addEventListener('change', event => setHlsCacheLatencyInputState('yt', event.currentTarget.checked));
  document
    .getElementById('tw-channel-edit-btn')
    ?.addEventListener('click', toggleTwChannelEdit);
  document
    .getElementById('tw-channel-save-btn')
    ?.addEventListener('click', saveTwChannelEdit);
  document
    .getElementById('tw-channel-cancel-btn')
    ?.addEventListener('click', cancelTwChannelEdit);
  document
    .getElementById('tw-area-edit-btn')
    ?.addEventListener('click', toggleTwAreaEdit);
  document
    .getElementById('tw-area-save-btn')
    ?.addEventListener('click', saveTwAreaEdit);
  document
    .getElementById('tw-area-cancel-btn')
    ?.addEventListener('click', cancelTwAreaEdit);
  document
    .getElementById('tw-quality-edit-btn')
    ?.addEventListener('click', toggleTwQualityEdit);
  document
    .getElementById('tw-quality-save-btn')
    ?.addEventListener('click', saveTwQualityEdit);
  document
    .getElementById('tw-quality-cancel-btn')
    ?.addEventListener('click', cancelTwQualityEdit);
  document
    .getElementById('tw-crop-edit-btn')
    ?.addEventListener('click', () => openCropConfig('twitch'));
  document
    .getElementById('tw-crop-clear-btn')
    ?.addEventListener('click', () => clearCropConfig('twitch'));
  document
    .getElementById('tw-hls-cache-edit-btn')
    ?.addEventListener('click', toggleTwHlsCacheEdit);
  document
    .getElementById('tw-hls-cache-save-btn')
    ?.addEventListener('click', saveTwHlsCacheEdit);
  document
    .getElementById('tw-hls-cache-cancel-btn')
    ?.addEventListener('click', cancelTwHlsCacheEdit);
  document
    .getElementById('tw-hls-cache-enabled')
    ?.addEventListener('change', event => setHlsCacheLatencyInputState('tw', event.currentTarget.checked));
}
function initHolodexLoginModalControls() {
  document
    .getElementById('holodex-save-api-key-btn')
    ?.addEventListener('click', saveHolodexApiKey);
  document
    .getElementById('holodex-test-api-key-btn')
    ?.addEventListener('click', testHolodexApiKey);

  const modal = document.getElementById('holodex-login-modal');
  modal?.addEventListener('click', event => {
    if (event.target === modal) {
      closeHolodexLoginModal();
    }
  });

  document.addEventListener('keydown', event => {
    if (event.key === 'Escape' && modal?.classList.contains('active')) {
      closeHolodexLoginModal();
    }
  });

  document
    .getElementById('holodex-login-close-btn')
    ?.addEventListener('click', closeHolodexLoginModal);
  document
    .getElementById('holodex-cancel-login-btn')
    ?.addEventListener('click', closeHolodexLoginModal);
  document
    .getElementById('holodex-open-login-btn')
    ?.addEventListener('click', openHolodexLogin);
  document
    .getElementById('holodex-save-jwt-btn')
    ?.addEventListener('click', saveHolodexJwt);
  document
    .getElementById('holodex-logout-btn')
    ?.addEventListener('click', logoutHolodexJwt);
  document
    .getElementById('holodex-favorites-btn')
    ?.addEventListener('click', openHolodexLoginModal);
  document
    .getElementById('holodex-login-btn')
    ?.addEventListener('click', openHolodexLoginModal);
  document
    .getElementById('holodex-use-favorites-toggle')
    ?.addEventListener('change', toggleHolodexFavoritesMode);
  document
    .getElementById('holodex-skip-jwt-verify')
    ?.addEventListener('change', toggleHolodexSkipJwtVerify);
  document
    .getElementById('holodex-monitor-gate-toggle')
    ?.addEventListener('change', toggleHolodexMonitorGate);
}
function initFaceAuthModalControls() {
  const modal = document.getElementById('face-auth-modal');
  modal?.addEventListener('click', event => {
    if (event.target === modal) {
      closeFaceAuthModal();
    }
  });

  document.addEventListener('keydown', event => {
    if (event.key === 'Escape' && modal?.classList.contains('active')) {
      closeFaceAuthModal();
    }
  });

  document
    .getElementById('face-auth-retry-btn')
    ?.addEventListener('click', retryStartStream);
  document
    .getElementById('face-auth-close-btn')
    ?.addEventListener('click', closeFaceAuthModal);
}
let lastStatusRefreshMs = 0;
// Load config and set up status refresh interval
async function initStatusRefresh() {
  try {
    const config = await getJson('/api/config');

    // Store config globally and apply config-driven controls once.
    mergeConfigData(config);
    updateMonitorToggleStates(config);
    updateDanmakuCommandToggle(config.bilibili?.enable_danmaku_command !== false);

    const holodexApiKeyConfigured = !!config.holodex_api_key?.trim();
    applyHolodexMonitorGateToggle(config.holodex_monitor_gate !== false);
    applyHolodexSectionVisibility(holodexApiKeyConfigured);
    if (holodexApiKeyConfigured) {
      loadHolodexAuthStatus();
    }

    if (config.interval) {
      statusRefreshInterval = config.interval * 1000; // Convert to milliseconds
    }
  } catch (error) {
    console.warn('Failed to load config, using default interval:', error);
  }

  // Start status refresh with configured interval
  if (statusIntervalId) {
    clearInterval(statusIntervalId);
  }
  statusIntervalId = setInterval(() => {
    if (!isDashboardVisible()) {
      return;
    }
    if (eventStreamHealthy()
      && Date.now() - lastStatusRefreshMs < statusRefreshInterval * 5) {
      return;
    }
    refreshStatus();
  }, statusRefreshInterval);

  if (networkRefreshIntervalId) {
    clearInterval(networkRefreshIntervalId);
  }
  networkRefreshIntervalId = setInterval(() => {
    if (isDashboardVisible()) {
      refreshNetworkStatus();
    }
  }, networkRefreshInterval);

  // Initial refresh
  refreshStatus();
  refreshNetworkStatus();

  let titleRowCenterTimer;
  window.addEventListener('resize', () => {
    clearTimeout(titleRowCenterTimer);
    titleRowCenterTimer = setTimeout(schedulePlatformTitleRowCenters, 100);
  });

}
function applyHolodexSectionVisibility(apiKeyConfigured) {
  setElementDisplay(document.getElementById('holodex-section'), true);
  setElementDisplay(document.getElementById('holodex-init-hint'), false);
  setElementDisplay(document.getElementById('holodex-api-config'), !apiKeyConfigured);
  setElementDisplay(document.getElementById('holodex-streams-section'), apiKeyConfigured);
  setElementDisplay(document.getElementById('holodex-login-btn'), apiKeyConfigured, 'inline-flex');
  maybeLoadHolodexStreams();
}
function applyHolodexMonitorGateToggle(enabled) {
  const toggle = document.getElementById('holodex-monitor-gate-toggle');
  if (toggle) {
    toggle.checked = !!enabled;
  }
}
// The stream list is only worth fetching once the API key is known to be
// configured, the overview is the visible view, and the panel is open.
function maybeLoadHolodexStreams() {
  if (holodexStreamsRequested || !isViewActive('overview')) {
    return;
  }

  const section = document.getElementById('holodex-section');
  if (section?.classList.contains('is-collapsed')) {
    return;
  }

  const streams = document.getElementById('holodex-streams-section');
  if (!streams || getComputedStyle(streams).display === 'none') {
    return;
  }

  holodexStreamsRequested = true;
  refreshHolodexStreams();
}
function setHolodexCollapsed(collapsed) {
  const section = document.getElementById('holodex-section');
  const button = document.getElementById('holodexFoldBtn');
  if (!section) return;

  section.classList.toggle('is-collapsed', collapsed);
  if (button) {
    const label = collapsed ? '展开' : '折叠';
    button.setAttribute('aria-expanded', collapsed ? 'false' : 'true');
    button.setAttribute('aria-label', label);
    button.title = label;
  }
}
function toggleHolodexFold() {
  const section = document.getElementById('holodex-section');
  if (!section) return;

  const collapsed = !section.classList.contains('is-collapsed');
  setHolodexCollapsed(collapsed);
  try {
    localStorage.setItem('holodexCollapsed', collapsed ? 'true' : 'false');
  } catch (error) {
    // The fold still works; it just will not be remembered.
  }

  if (!collapsed) {
    // Opening the panel is what triggers the first fetch.
    maybeLoadHolodexStreams();
  }
}
function initHolodexFold() {
  let collapsed = false;
  try {
    collapsed = localStorage.getItem('holodexCollapsed') === 'true';
  } catch (error) {
    // Ignore unavailable storage and start expanded.
  }
  setHolodexCollapsed(collapsed);

  document
    .getElementById('holodexFoldBtn')
    ?.addEventListener('click', toggleHolodexFold);

  const heading = document.getElementById('holodex-heading');
  heading?.addEventListener('click', toggleHolodexFold);
}
const HOLODEX_STATUS_STATE_CLASSES = [
  'holodex-status-loading',
  'holodex-status-warning',
  'holodex-status-success',
  'holodex-status-error'
];
function setHolodexStatus(statusDiv, message, stateClass) {
  statusDiv.classList.remove(...HOLODEX_STATUS_STATE_CLASSES);
  statusDiv.classList.add(stateClass);
  statusDiv.style.display = 'block';
  statusDiv.textContent = message;
}
function hideHolodexStatus(statusDiv) {
  statusDiv.classList.remove(...HOLODEX_STATUS_STATE_CLASSES);
  statusDiv.style.display = 'none';
  statusDiv.textContent = '';
}
function createHolodexScheduleDivider() {
  const divider = document.createElement('div');
  divider.className = 'holodex-schedule-divider';

  const label = document.createElement('span');
  label.className = 'holodex-schedule-divider-label';
  label.textContent = '预告';

  divider.appendChild(label);
  return divider;
}
async function refreshHolodexStreams() {
  // Start continuous spinning animation
  const button = document.getElementById('refreshHolodexBtn');
  const icon = document.getElementById('refreshHolodexIcon');
  setButtonLoading(button, icon, true);

  const statusDiv = document.getElementById('holodex-status');
  const streamsDiv = document.getElementById('holodex-streams');
  if (!statusDiv || !streamsDiv) {
    setButtonLoading(button, icon, false);
    return;
  }

  stopHolodexDurationTicker();
  setHolodexStatus(statusDiv, '⏳ 加载中...', 'holodex-status-loading');
  streamsDiv.replaceChildren();

  try {
    const favoritesParam = holodexUseFavorites ? 'true' : 'false';
    const data = await getJson(`/api/holodex/streams?favorites=${favoritesParam}`);

    if (!data.success) {
      setHolodexStatus(statusDiv, `⚠️ ${data.message}`, 'holodex-status-warning');
      return;
    }

    const streams = data.data || [];
    const isFavorites = data.source === 'favorites';
    holodexCurrentSource = data.source || (isFavorites ? 'favorites' : 'channels');

    if (isFavorites) {
      try {
        await refreshHolodexChannelsData();
      } catch (error) {
        console.error('Failed to load channels.json for Holodex add controls:', error);
        holodexCurrentSource = 'channels';
        showNotification('加载 channels.json 失败，无法显示添加按钮', 'error');
      }
    }

    // Separate live and scheduled streams
    const liveStreams = streams.filter(s => s.status === 'live');
    const scheduledStreams = streams.filter(s => s.status !== 'live');

    if (streams.length === 0) {
      const emptyMessage = isFavorites
        ? '✅ 收藏夹 - 当前无直播或预告'
        : '当前无直播或预告';
      setHolodexStatus(statusDiv, emptyMessage, 'holodex-status-success');
      return;
    }

    // Sort scheduled streams by time - nearest first
    scheduledStreams.sort((a, b) => {
      const timeA = getHolodexStreamStartMs(a, false) ?? Infinity;
      const timeB = getHolodexStreamStartMs(b, false) ?? Infinity;
      return timeA - timeB;
    });

    hideHolodexStatus(statusDiv);

    // Render live streams first
    liveStreams.forEach(stream => {
      streamsDiv.appendChild(createStreamCard(stream, true));
    });

    // Add divider if both live and scheduled exist
    if (liveStreams.length > 0 && scheduledStreams.length > 0) {
      streamsDiv.appendChild(createHolodexScheduleDivider());
    }

    // Render scheduled streams (now sorted by time)
    scheduledStreams.forEach(stream => {
      streamsDiv.appendChild(createStreamCard(stream, false));
    });

    startHolodexDurationTicker();

  } catch (error) {
    setHolodexStatus(statusDiv, `❌ 请求失败: ${error.message}`, 'holodex-status-error');
  } finally {
    // Stop spinning animation when complete
    setButtonLoading(button, icon, false);
  }
}
let holodexDurationIntervalId = null;
function getHolodexPlaceholderKind(stream) {
  const link = (stream.external_link || '').toLowerCase();
  if (link.includes('twitch.tv')) {
    return 'twitch';
  }
  return 'radio';
}
function createHolodexStreamSvg(pathData) {
  const svg = createSvgIcon('0 0 24 24', pathData);
  svg.setAttribute('width', '14');
  svg.setAttribute('height', '14');
  svg.setAttribute('fill', 'none');
  svg.setAttribute('stroke', 'currentColor');
  svg.setAttribute('stroke-width', '2');
  svg.setAttribute('stroke-linecap', 'round');
  svg.setAttribute('stroke-linejoin', 'round');
  return svg;
}
function createHolodexWatchIcon() {
  const svg = createSvgIcon(
    '0 0 24 24',
    'M7 4.5a1 1 0 0 1 1.53-.85l11 7.5a1 1 0 0 1 0 1.7l-11 7.5A1 1 0 0 1 7 19.5v-15Z',
  );
  svg.setAttribute('width', '15');
  svg.setAttribute('height', '15');
  svg.setAttribute('fill', 'currentColor');
  return svg;
}
function createHolodexPlaceholderIcon(kind) {
  const iconClass = kind === 'twitch'
    ? 'holodex-duration-twitch-icon'
    : 'holodex-duration-radio-icon';
  const wrapper = document.createElement('span');
  wrapper.className = iconClass;
  wrapper.appendChild(kind === 'twitch'
    ? createSvgIcon('0 0 24 24', 'M11.64 5.93H13.07V10.21H11.64M15.57 5.93H17V10.21H15.57M7 2L3.43 5.57V18.43H7.71V22L11.29 18.43H14.14L20.57 12V2M19.14 11.29L16.29 14.14H13.43L10.93 16.64V14.14H7.71V3.43H19.14Z')
    : createSvgIcon('0 0 24 24', 'M12 10C10.9 10 10 10.9 10 12S10.9 14 12 14 14 13.1 14 12 13.1 10 12 10M18 12C18 8.7 15.3 6 12 6S6 8.7 6 12C6 14.2 7.2 16.1 9 17.2L10 15.5C8.8 14.8 8 13.5 8 12.1C8 9.9 9.8 8.1 12 8.1S16 9.9 16 12.1C16 13.6 15.2 14.9 14 15.5L15 17.2C16.8 16.2 18 14.2 18 12M12 2C6.5 2 2 6.5 2 12C2 15.7 4 18.9 7 20.6L8 18.9C5.6 17.5 4 14.9 4 12C4 7.6 7.6 4 12 4S20 7.6 20 12C20 15 18.4 17.5 16 18.9L17 20.6C20 18.9 22 15.7 22 12C22 6.5 17.5 2 12 2Z')
  );
  return wrapper;
}
function createHolodexPlaceholderDurationOverlay(stream, isLive) {
  const kind = getHolodexPlaceholderKind(stream);
  const durationClass = kind === 'twitch'
    ? 'holodex-stream-duration holodex-stream-duration-twitch'
    : 'holodex-stream-duration holodex-stream-duration-radio';
  const hoverText = kind === 'twitch' ? '外部配信' : '外部直播';
  const startMs = getHolodexStreamStartMs(stream, isLive);
  const duration = document.createElement('div');
  duration.className = durationClass;
  let hasDurationText = false;

  if (isLive && startMs) {
    duration.dataset.tick = 'live';
    duration.dataset.startMs = String(startMs);

    const text = document.createElement('span');
    text.className = 'holodex-duration-text';
    text.textContent = formatHolodexDuration(Date.now() - startMs);
    duration.appendChild(text);
    hasDurationText = true;
  }

  if (!hasDurationText && stream.start_scheduled) {
    const start = new Date(stream.start_scheduled);
    const clock = Number.isNaN(start.getTime())
      ? '预告'
      : `${String(start.getHours()).padStart(2, '0')}:${String(start.getMinutes()).padStart(2, '0')}`;
    const text = document.createElement('span');
    text.className = 'holodex-duration-text';
    text.textContent = clock;
    duration.appendChild(text);
  }

  const hover = document.createElement('span');
  hover.className = 'holodex-duration-hover';
  hover.textContent = hoverText;
  duration.append(hover, createHolodexPlaceholderIcon(kind));
  return duration;
}
function createHolodexDurationOverlay(stream, isLive, isPlaceholder) {
  if (isPlaceholder) {
    return createHolodexPlaceholderDurationOverlay(stream, isLive);
  }

  const startMs = getHolodexStreamStartMs(stream, isLive);
  if (isLive && startMs) {
    const duration = document.createElement('div');
    duration.className = 'holodex-stream-duration holodex-stream-duration-live';
    duration.dataset.tick = 'live';
    duration.dataset.startMs = String(startMs);

    const text = document.createElement('span');
    text.className = 'holodex-duration-text';
    text.textContent = formatHolodexDuration(Date.now() - startMs);
    duration.appendChild(text);
    return duration;
  }

  return null;
}
function escapeHolodexHtml(text) {
  return String(text ?? '')
    .replace(/&/g, '&amp;')
    .replace(/</g, '&lt;')
    .replace(/>/g, '&gt;')
    .replace(/"/g, '&quot;');
}
function formatHolodexDuration(ms) {
  const totalSec = Math.max(0, Math.floor(ms / 1000));
  const h = Math.floor(totalSec / 3600);
  const m = Math.floor((totalSec % 3600) / 60);
  const s = totalSec % 60;
  if (h > 0) {
    return `${h}:${String(m).padStart(2, '0')}:${String(s).padStart(2, '0')}`;
  }
  return `${m}:${String(s).padStart(2, '0')}`;
}
function getHolodexStreamStartMs(stream, isLive) {
  const raw = isLive
    ? (stream.start_actual || stream.available_at || stream.published_at || stream.start_scheduled)
    : stream.start_scheduled;
  if (!raw) return null;
  const ms = new Date(raw).getTime();
  return Number.isNaN(ms) ? null : ms;
}
function stopHolodexDurationTicker() {
  if (holodexDurationIntervalId) {
    clearInterval(holodexDurationIntervalId);
    holodexDurationIntervalId = null;
  }
}
function updateHolodexDurations() {
  const now = Date.now();
  document.querySelectorAll('.holodex-stream-duration[data-tick="live"]').forEach((el) => {
    const startMs = Number(el.dataset.startMs);
    if (!startMs) return;
    const textEl = el.querySelector('.holodex-duration-text');
    if (textEl) {
      textEl.textContent = formatHolodexDuration(now - startMs);
    }
  });
}
function startHolodexDurationTicker() {
  stopHolodexDurationTicker();
  updateHolodexDurations();
  if (document.querySelector('.holodex-stream-duration[data-tick="live"]')) {
    holodexDurationIntervalId = setInterval(updateHolodexDurations, 1000);
  }
}
function getHolodexChannelUrl(channelId) {
  if (!channelId) return '';
  return `https://holodex.net/channel/${encodeURIComponent(channelId)}`;
}
function getHolodexChannelPhotoUrl(stream) {
  if (stream.channel_photo) {
    return stream.channel_photo;
  }
  if (!stream.channel_id) return '';
  return `https://holodex.net/statics/channelImg/${encodeURIComponent(stream.channel_id)}/50.png`;
}
function createHolodexAvatarBlock(stream) {
  if (!stream.channel_id) return null;
  const holodexUrl = getHolodexChannelUrl(stream.channel_id);
  const photoUrl = getHolodexChannelPhotoUrl(stream);

  const avatar = document.createElement('a');
  avatar.className = 'holodex-stream-avatar';
  avatar.href = holodexUrl;
  avatar.target = '_blank';
  avatar.rel = 'noopener noreferrer';
  avatar.title = stream.channel_name || 'channel';

  const image = document.createElement('img');
  image.src = photoUrl;
  image.alt = '';
  image.loading = 'lazy';
  avatar.appendChild(image);
  return avatar;
}
async function refreshHolodexChannelsData() {
  const result = await managementRequest('/api/manage/channels');
  if (!result.success || !result.data) {
    throw new Error(result.message || 'Failed to load channels');
  }
  state.channelsData = result.data;
  return state.channelsData;
}
function normalizeHolodexChannelValue(value) {
  return String(value || '').trim().toLowerCase();
}
function holodexStreamHasConfiguredChannel(stream) {
  if (!state.channelsData || !Array.isArray(state.channelsData.channels)) return false;

  const streamName = normalizeHolodexChannelValue(stream.channel_name);
  const youtubeId = normalizeHolodexChannelValue(stream.channel_id);
  const twitchId = normalizeHolodexChannelValue(parseTwitchLoginFromLink(stream.external_link));

  return state.channelsData.channels.some(channel => {
    const platforms = channel.platforms || {};
    return (streamName && normalizeHolodexChannelValue(channel.name) === streamName)
      || (youtubeId && normalizeHolodexChannelValue(platforms.youtube) === youtubeId)
      || (twitchId && normalizeHolodexChannelValue(platforms.twitch) === twitchId);
  });
}
function createHolodexChannelAddIcon(pathData, title) {
  const button = document.createElement('button');
  button.type = 'button';
  button.className = 'holodex-channel-add-btn';
  button.title = title;
  button.appendChild(createSvgIcon('0 0 24 24', pathData));
  return button;
}
function createHolodexAddChannelControls(stream) {
  if (holodexCurrentSource !== 'favorites' || !stream.channel_name || holodexStreamHasConfiguredChannel(stream)) {
    return null;
  }

  const youtubeId = stream.channel_id || '';
  const twitchId = parseTwitchLoginFromLink(stream.external_link);
  if (!youtubeId && !twitchId) return null;

  const control = document.createElement('span');
  control.className = 'holodex-channel-add';
  control.dataset.channelName = stream.channel_name;
  control.dataset.youtubeId = youtubeId;
  control.dataset.twitchId = twitchId;

  const startButton = document.createElement('button');
  startButton.type = 'button';
  startButton.className = 'holodex-channel-add-btn holodex-add-channel-start';
  startButton.title = '添加到 channels.json';
  startButton.setAttribute('aria-label', '添加到 channels.json');
  startButton.textContent = '➕';

  const actions = document.createElement('span');
  actions.className = 'holodex-channel-add-actions';
  actions.setAttribute('aria-label', '确认添加频道');

  const confirmButton = createHolodexChannelAddIcon('M20 6 9 17l-5-5', '确认添加');
  confirmButton.classList.add('holodex-add-channel-confirm');

  const rejectButton = createHolodexChannelAddIcon('M18 6 6 18M6 6l12 12', '取消');
  rejectButton.classList.add('holodex-add-channel-reject');

  actions.append(confirmButton, rejectButton);
  control.append(startButton, actions);
  return control;
}
function createHolodexChannelBlock(stream) {
  if (!stream.channel_name) return null;
  const holodexUrl = getHolodexChannelUrl(stream.channel_id);
  const row = document.createElement('div');
  row.className = 'holodex-stream-channel-row';

  const channel = document.createElement(holodexUrl ? 'a' : 'p');
  channel.className = 'holodex-stream-channel';
  channel.textContent = stream.channel_name;
  if (holodexUrl) {
    channel.href = holodexUrl;
    channel.target = '_blank';
    channel.rel = 'noopener noreferrer';
  }
  row.appendChild(channel);

  const addControls = createHolodexAddChannelControls(stream);
  if (addControls) {
    row.appendChild(addControls);
  }
  return row;
}
function createHolodexStreamActionButton(extraClasses, streamActionData, icon) {
  const button = document.createElement('button');
  button.type = 'button';
  button.className = `holodex-stream-btn ${extraClasses}`;
  button.dataset.platform = streamActionData.platform;
  button.dataset.channelId = streamActionData.channelId;
  button.dataset.twitchId = streamActionData.twitchId;
  button.dataset.externalLink = streamActionData.externalLink;
  button.dataset.suggestedAreaId = streamActionData.suggestedAreaId;
  button.dataset.title = streamActionData.title;
  button.dataset.topicId = streamActionData.topicId;
  button.dataset.status = streamActionData.status;

  const label = document.createElement('span');
  label.textContent = '切换';
  button.append(icon, label);
  return button;
}
function readHolodexStreamActionData(button) {
  return {
    channelId: button.dataset.channelId || '',
    suggestedAreaId: button.dataset.suggestedAreaId ? parseInteger(button.dataset.suggestedAreaId, 0) || null : null,
    title: button.dataset.title || '',
    topicId: button.dataset.topicId || '',
    status: button.dataset.status || '',
    platform: button.dataset.platform || 'youtube',
    twitchChannelId: button.dataset.twitchId || '',
    externalLink: button.dataset.externalLink || ''
  };
}
function parseTwitchLoginFromLink(link) {
  if (!link) return '';
  const prefixes = [
    'https://www.twitch.tv/',
    'https://twitch.tv/',
    'http://www.twitch.tv/',
    'http://twitch.tv/'
  ];
  for (const prefix of prefixes) {
    if (link.startsWith(prefix)) {
      const rest = link.slice(prefix.length).split(/[/?#]/)[0].trim();
      if (rest) return rest;
    }
  }
  return '';
}
function createStreamCard(stream, isLive) {
  const streamCard = document.createElement('div');
  streamCard.className = 'holodex-stream-card';

  const isPlaceholder = stream.is_placeholder || stream.stream_type === 'placeholder';
  const placeholderKind = isPlaceholder ? getHolodexPlaceholderKind(stream) : '';
  const platform = isPlaceholder ? (placeholderKind === 'twitch' ? 'twitch' : 'external') : 'youtube';
  const twitchChannelId = isPlaceholder ? parseTwitchLoginFromLink(stream.external_link) : '';
  const watchUrl = stream.external_link || `https://www.youtube.com/watch?v=${stream.id}`;
  const thumbUrl = stream.thumbnail || (isPlaceholder
    ? (placeholderKind === 'twitch' && twitchChannelId
      ? `https://static-cdn.jtvnw.net/previews-ttv/live_user_${twitchChannelId}-640x360.jpg`
      : '')
    : `https://i.ytimg.com/vi/${stream.id}/sddefault.jpg`);

  const viewers = stream.live_viewers ? `${stream.live_viewers.toLocaleString()} 观看` : '';

  const areaInfo = document.createElement('p');
  areaInfo.className = 'holodex-stream-area-hint';
  if (stream.suggested_area_id && stream.suggested_area_name) {
    areaInfo.textContent = `🎯 建议分区: ${stream.suggested_area_name}`;
  }

  const statusMeta = document.createElement('div');
  statusMeta.className = 'holodex-stream-meta';
  if (!isLive) {
    const scheduleText = stream.start_scheduled
      ? formatHolodexScheduledStart(stream.start_scheduled)
      : '预告';
    const scheduled = document.createElement('span');
    scheduled.className = 'holodex-stream-scheduled';
    scheduled.textContent = scheduleText;
    statusMeta.appendChild(scheduled);
  } else if (viewers) {
    const liveLabel = document.createElement('span');
    liveLabel.className = 'holodex-stream-live-label';
    liveLabel.textContent = '直播中';
    const viewerText = document.createElement('span');
    viewerText.textContent = `• ${viewers}`;
    statusMeta.append(liveLabel, viewerText);
  } else {
    const liveLabel = document.createElement('span');
    liveLabel.className = 'holodex-stream-live-label';
    liveLabel.textContent = '直播中';
    statusMeta.appendChild(liveLabel);
  }

  const streamActionData = {
    platform,
    channelId: stream.channel_id || '',
    twitchId: twitchChannelId,
    externalLink: stream.external_link || '',
    suggestedAreaId: stream.suggested_area_id || '',
    title: stream.title || '',
    topicId: stream.topic_id || '',
    status: stream.status || ''
  };

  const thumb = document.createElement('div');
  thumb.className = 'holodex-stream-thumb';
  const thumbLink = document.createElement('a');
  thumbLink.className = 'holodex-stream-thumb-link';
  thumbLink.href = watchUrl;
  thumbLink.target = '_blank';
  thumbLink.rel = 'noopener noreferrer';
  if (thumbUrl) {
    const image = document.createElement('img');
    image.src = thumbUrl;
    image.alt = '';
    thumbLink.appendChild(image);
  } else {
    const placeholder = document.createElement('div');
    placeholder.className = 'holodex-stream-thumb-placeholder';
    thumbLink.appendChild(placeholder);
  }

  const thumbTop = document.createElement('div');
  thumbTop.className = 'holodex-stream-thumb-top';
  if (stream.topic_id) {
    const topic = document.createElement('span');
    topic.className = 'holodex-stream-topic';
    topic.textContent = stream.topic_id;
    thumbTop.appendChild(topic);
  }

  thumb.append(thumbLink, thumbTop);
  const durationBlock = createHolodexDurationOverlay(stream, isLive, isPlaceholder);
  if (durationBlock) {
    const thumbBottom = document.createElement('div');
    thumbBottom.className = 'holodex-stream-thumb-bottom';
    thumbBottom.appendChild(durationBlock);
    thumb.appendChild(thumbBottom);
  }

  const body = document.createElement('div');
  body.className = 'holodex-stream-body';
  const contentRow = document.createElement('div');
  contentRow.className = 'holodex-stream-content-row';
  const avatar = createHolodexAvatarBlock(stream);
  if (avatar) {
    contentRow.appendChild(avatar);
  }

  const lines = document.createElement('div');
  lines.className = 'holodex-stream-lines';
  const title = document.createElement('h4');
  title.className = 'holodex-stream-title';
  title.textContent = stream.title || '';
  lines.appendChild(title);

  const channelBlock = createHolodexChannelBlock(stream);
  if (channelBlock) {
    lines.appendChild(channelBlock);
  }
  lines.appendChild(statusMeta);
  if (areaInfo.textContent) {
    lines.appendChild(areaInfo);
  }
  contentRow.appendChild(lines);

  const actions = document.createElement('div');
  actions.className = 'holodex-stream-actions';
  const watchLink = document.createElement('a');
  watchLink.className = 'holodex-stream-watch';
  watchLink.href = watchUrl;
  watchLink.target = '_blank';
  watchLink.rel = 'noopener noreferrer';
  const watchLabel = document.createElement('span');
  watchLabel.textContent = '观看';
  watchLink.append(createHolodexWatchIcon(), watchLabel);
  actions.appendChild(watchLink);

  if (isLive) {
    actions.appendChild(createHolodexStreamActionButton(
      'holodex-stream-btn-crop crop-switch-button',
      streamActionData,
      createHolodexStreamSvg([
        { d: 'M6.13 1L6 16a2 2 0 0 0 2 2h15' },
        { d: 'M1 6.13L16 6a2 2 0 0 1 2 2v15' }
      ])
    ));
  }
  actions.appendChild(createHolodexStreamActionButton(
    'holodex-stream-btn-switch switch-button',
    streamActionData,
    createHolodexStreamSvg([
      { d: 'M22 12c0 6-4.39 10-9.806 10C7.792 22 4.24 19.665 3 16m-1-4C2 6 6.39 2 11.807 2C16.208 2 19.758 4.335 21 8' },
      { d: 'm7 17l-4-1l-1 4M17 7l4 1l1-4' }
    ])
  ));

  body.append(contentRow, actions);
  streamCard.append(thumb, body);

  return streamCard;
}
// Store pending switch data
let pendingSwitchData = null;
let selectedAreaId = null;
async function switchToHolodexStream(channelId, suggestedAreaId, title, topicId, status, platform, twitchChannelId, externalLink) {
  if (!suggestedAreaId) {
    // Show modal for area selection
    pendingSwitchData = { channelId, areaId: null, title, topicId, status, platform, twitchChannelId, externalLink };
    await showAreaModal();
  } else {
    // Direct switch with suggested area
    await performSwitch(channelId, suggestedAreaId, title, topicId, status, platform, twitchChannelId, externalLink);
  }
}
async function showAreaModal() {
  const modal = document.getElementById('area-modal');
  const list = document.getElementById('modal-area-list');
  const confirm = document.getElementById('confirm-area-selection-btn');
  if (!modal || !list) return;

  // Load areas if not already loaded
  if (!state.areasData) {
    try {
      state.areasData = normalizeAreaData(await getJson('/api/areas'));
    } catch (error) {
      showNotification('加载分区列表失败', 'error');
      return;
    }
  }

  selectedAreaId = null;
  if (confirm) {
    confirm.disabled = true;
  }

  const areas = getSortedAreas(getAreaList());
  if (areas.length === 0) {
    const empty = document.createElement('p');
    empty.className = 'area-option-empty';
    empty.textContent = '没有可用分区';
    list.replaceChildren(empty);
  } else {
    list.replaceChildren(...areas.map((area) => createAreaPickerOption(area)));
  }

  modal.classList.remove('hidden');
  list.querySelector('.area-option')?.focus();
}

function createAreaPickerOption(area) {
  const button = document.createElement('button');
  button.type = 'button';
  button.className = 'area-option';
  button.setAttribute('role', 'option');
  button.setAttribute('aria-selected', 'false');
  button.dataset.id = String(area.id);
  button.textContent = `${area.name} (${area.id})`;
  button.addEventListener('click', () => {
    if (selectedAreaId === area.id) {
      confirmAreaSelection();
      return;
    }
    selectAreaOption(area.id);
  });
  return button;
}

function selectAreaOption(id) {
  selectedAreaId = id;
  const confirm = document.getElementById('confirm-area-selection-btn');
  if (confirm) {
    confirm.disabled = false;
  }
  document.querySelectorAll('#modal-area-list .area-option').forEach((button) => {
    const on = Number(button.dataset.id) === id;
    button.classList.toggle('is-selected', on);
    button.setAttribute('aria-selected', on ? 'true' : 'false');
  });
}

function closeAreaModal() {
  document.getElementById('area-modal')?.classList.add('hidden');
  pendingSwitchData = null;
  selectedAreaId = null;
}

function initAreaModalControls() {
  document
    .getElementById('confirm-area-selection-btn')
    ?.addEventListener('click', confirmAreaSelection);
  document
    .getElementById('cancel-area-selection-btn')
    ?.addEventListener('click', closeAreaModal);
}
function showFaceAuthModal(qrUrl) {
  faceAuthUrl = qrUrl;
  const modal = document.getElementById('face-auth-modal');
  const container = document.getElementById('face-auth-qr-container');
  if (!modal || !container) return;

  // Generate QR code using QR Server API, with a clickable fallback if it fails.
  container.replaceChildren();

  const qrImg = document.createElement('img');
  qrImg.className = 'face-auth-qr-image';
  qrImg.src = `https://api.qrserver.com/v1/create-qr-code/?size=256x256&data=${encodeURIComponent(qrUrl)}`;
  qrImg.alt = 'Bilibili 人脸验证二维码';
  qrImg.addEventListener('error', () => {
    container.replaceChildren(createFaceAuthQrFallback(qrUrl));
  }, { once: true });

  container.appendChild(qrImg);
  modal.classList.add('active');
}
function createFaceAuthQrFallback(qrUrl) {
  const fallback = document.createElement('div');
  fallback.className = 'face-auth-fallback';

  const message = document.createElement('p');
  message.textContent = '无法生成二维码，请点击下方链接：';

  const link = document.createElement('a');
  link.href = qrUrl;
  link.target = '_blank';
  link.rel = 'noopener noreferrer';
  link.textContent = qrUrl;

  fallback.append(message, link);
  return fallback;
}
function closeFaceAuthModal() {
  const modal = document.getElementById('face-auth-modal');
  if (modal) {
    modal.classList.remove('active');
  }
  faceAuthUrl = null;
}
function retryStartStream() {
  closeFaceAuthModal();
  startStream();
}
async function confirmAreaSelection() {
  const areaId = parseInteger(selectedAreaId, 0);

  if (!areaId) {
    showNotification('请选择分区', 'error');
    return;
  }

  if (!pendingSwitchData) {
    console.error('No pending switch data');
    closeAreaModal();
    return;
  }

  const channelId = pendingSwitchData.channelId;
  const { title, topicId, status, platform, twitchChannelId, externalLink } = pendingSwitchData;
  closeAreaModal();
  await performSwitch(channelId, areaId, title, topicId, status, platform, twitchChannelId, externalLink);
}
function createHolodexSwitchPayload(channelId, areaId, title, topicId, status, platform, twitchChannelId, externalLink) {
  return {
    channel_id: channelId,
    area_id: areaId,
    title: title || null,
    topic_id: topicId || null,
    status: status || null,
    platform: platform || 'youtube',
    twitch_channel_id: twitchChannelId || null,
    external_link: externalLink || null
  };
}
async function performSwitch(channelId, areaId, title, topicId, status, platform, twitchChannelId, externalLink) {
  try {
    const payload = createHolodexSwitchPayload(
      channelId,
      areaId,
      title,
      topicId,
      status,
      platform,
      twitchChannelId,
      externalLink
    );
    const result = await postJsonApi('/api/holodex/switch', payload);
    if (result.success) {
      showNotification(result.message || '频道切换成功', 'success');
      // Refresh status after switching
      setTimeout(refreshStatus, 1000);
    } else {
      showNotification(result.message || '频道切换失败', 'error');
    }
  } catch (error) {
    console.error('Switch error:', error);
    showNotification('频道切换失败: ' + error.message, 'error');
  }
}
async function cropAndSwitchToHolodexStream(channelId, suggestedAreaId, title, topicId, status, platform = 'youtube', twitchChannelId = '', externalLink = '') {
  // First, open crop modal and auto-capture
  document.getElementById('cropModal').classList.add('active');
  document.getElementById('cropPlatform').value = platform;

  showNotification('正在捕获直播帧...', 'info');

  try {
    const captureUrl = platform === 'twitch'
      ? `/api/crop/capture/twitch?channel_id=${encodeURIComponent(twitchChannelId)}`
      : `/api/crop/capture/youtube?channel_id=${encodeURIComponent(channelId)}`;
    const result = await postJsonApi(captureUrl);

    if (result.success && result.message) {
      loadCapturedCropFrame(result.message, () => {
        showNotification('直播帧已捕获，请选择裁剪区域后点击"应用裁剪"，然后会自动切换频道', 'success');
        window.pendingHolodexSwitch = {
          channelId,
          suggestedAreaId,
          title,
          topicId,
          status,
          platform,
          twitchChannelId,
          externalLink
        };
      });
    } else {
      showNotification(result.message || '捕获失败，请确保直播正在进行', 'error');
    }
  } catch (error) {
    showNotification('捕获失败: ' + error.message, 'error');
  }
}
// Holodex API Key Management Functions
async function testHolodexApiKey() {
  const apiKeyInput = document.getElementById('holodex-api-input');
  const apiKey = apiKeyInput.value.trim();
  if (!apiKey) {
    showNotification('请输入 API Key', 'error');
    return;
  }
  try {
    const response = await fetch('https://holodex.net/api/v2/live?limit=1', {
      headers: { 'X-APIKEY': apiKey }
    });
    if (response.ok) {
      showNotification('Holodex API 连接成功', 'success');
    } else {
      const text = await response.text();
      showNotification('连接失败: ' + text, 'error');
    }
  } catch (error) {
    showNotification('连接失败: ' + error.message, 'error');
  }
}
let holodexAuthState = { logged_in: false, username: null, expired: false, skip_jwt_verify: false };
let holodexUseFavorites = false;
let holodexUseFavoritesInitialized = false;
function updateHolodexAuthBar() {
  const authBar = document.getElementById('holodex-auth-bar');
  const loginBtn = document.getElementById('holodex-login-btn');
  const favoritesLabel = document.getElementById('holodex-favorites-label');
  const favoritesBtn = document.getElementById('holodex-favorites-btn');
  const heartIcon = document.querySelector('#holodex-favorites-btn .holodex-heart-icon');
  const favoritesMode = document.querySelector('.holodex-favorites-mode');
  const favoritesToggle = document.getElementById('holodex-use-favorites-toggle');
  const jwtSkip = document.querySelector('.holodex-jwt-skip');
  const skipToggle = document.getElementById('holodex-skip-jwt-verify');
  const logoutBtn = document.getElementById('holodex-logout-btn');

  if (skipToggle) {
    skipToggle.checked = !!holodexAuthState.skip_jwt_verify;
  }
  if (favoritesToggle) {
    favoritesToggle.checked = !!holodexUseFavorites;
  }

  const loggedIn = holodexAuthState.logged_in && !holodexAuthState.expired;
  setElementDisplay(authBar, true, 'inline-flex');
  setElementDisplay(loginBtn, false);
  setElementDisplay(heartIcon, loggedIn, 'inline-flex');
  setElementDisplay(favoritesMode, loggedIn, 'inline-flex');
  setElementDisplay(jwtSkip, false);

  if (loggedIn) {
    if (favoritesLabel) {
      favoritesLabel.textContent = holodexAuthState.username
        ? `${holodexAuthState.username}的收藏`
        : '收藏夹';
    }
    if (favoritesBtn) favoritesBtn.title = '管理 Holodex 登录';
  } else if (holodexAuthState.expired) {
    if (favoritesLabel) favoritesLabel.textContent = '⚠️ 重新登录';
    if (favoritesBtn) favoritesBtn.title = 'Holodex 登录已过期';
  } else {
    if (favoritesLabel) favoritesLabel.textContent = '🔗 使用Holodex收藏夹';
    if (favoritesBtn) favoritesBtn.title = '登录后使用 Holodex 收藏夹';
  }

  if (logoutBtn) {
    logoutBtn.classList.toggle('hidden', !holodexAuthState.logged_in);
  }
}
async function toggleHolodexFavoritesMode() {
  const toggle = document.getElementById('holodex-use-favorites-toggle');
  if (!holodexAuthState.logged_in || holodexAuthState.expired) {
    if (toggle) toggle.checked = false;
    openHolodexLoginModal();
    return;
  }

  holodexUseFavorites = toggle ? toggle.checked : !holodexUseFavorites;
  holodexUseFavoritesInitialized = true;
  updateHolodexAuthBar();
  await refreshHolodexStreams();
}
function openHolodexLoginModal() {
  const modal = document.getElementById('holodex-login-modal');
  if (!modal) return;
  updateHolodexModalHint();
  modal.classList.add('active');
  loadHolodexAuthStatus();
}
function closeHolodexLoginModal() {
  const modal = document.getElementById('holodex-login-modal');
  modal?.classList.remove('active');
}
function updateHolodexLoginButton() {
  updateHolodexAuthBar();
}
function updateHolodexModalHint() {
  const hint = document.getElementById('holodex-modal-auth-hint');
  if (!hint) return;

  if (holodexAuthState.logged_in && !holodexAuthState.expired) {
    hint.classList.add('hidden');
    hint.classList.remove('warning');
    return;
  }

  hint.classList.remove('hidden');

  if (holodexAuthState.expired) {
    hint.textContent = '⚠️ JWT 已过期，请重新登录 Holodex 并保存新的 token';
    hint.classList.add('warning');
  } else {
    hint.textContent = '未登录 — 当前使用 channels.json 频道列表 (YouTube + Twitch 外部配信)';
    hint.classList.remove('warning');
  }
}
async function toggleHolodexSkipJwtVerify() {
  const toggle = document.getElementById('holodex-skip-jwt-verify');
  if (!toggle) return;

  try {
    const data = await postJsonApi('/api/config', { holodex_skip_jwt_verify: toggle.checked });
    if (data.success) {
      holodexAuthState.skip_jwt_verify = toggle.checked;
      showNotification(toggle.checked ? '已跳过 JWT 校验' : '已启用 JWT 校验', 'success');
      await loadHolodexAuthStatus();
      await refreshHolodexStreams();
    } else {
      toggle.checked = !toggle.checked;
      showNotification('更新失败: ' + (data.message || '未知错误'), 'error');
    }
  } catch (error) {
    toggle.checked = !toggle.checked;
    showNotification('更新失败: ' + error.message, 'error');
  }
}
async function toggleHolodexMonitorGate() {
  const toggle = document.getElementById('holodex-monitor-gate-toggle');
  if (!toggle) return;

  try {
    const data = await postJsonApi('/api/config', { holodex_monitor_gate: toggle.checked });
    if (data.success) {
      showNotification(
        toggle.checked ? '已启用 Holodex 状态查询' : '已改用 yt-dlp 查询 YouTube',
        'success'
      );
      refreshYouTubeStatus();
    } else {
      toggle.checked = !toggle.checked;
      showNotification('更新失败: ' + (data.message || '未知错误'), 'error');
    }
  } catch (error) {
    toggle.checked = !toggle.checked;
    showNotification('更新失败: ' + error.message, 'error');
  }
}
function openHolodexLogin() {
  window.open('https://holodex.net/login', '_blank', 'noopener,noreferrer');
}
async function loadHolodexAuthStatus() {
  try {
    const data = await getJson('/api/holodex/auth/status');
    if (!data.success) {
      holodexAuthState = { logged_in: false, username: null, expired: false, skip_jwt_verify: false };
      holodexUseFavorites = false;
      holodexUseFavoritesInitialized = false;
      updateHolodexLoginButton();
      updateHolodexModalHint();
      return;
    }
    const info = data.data || {};
    holodexAuthState = {
      logged_in: !!info.logged_in,
      username: info.username || null,
      expired: !!info.expired,
      skip_jwt_verify: !!info.skip_jwt_verify
    };
    if (holodexAuthState.logged_in && !holodexAuthState.expired) {
      if (!holodexUseFavoritesInitialized) {
        holodexUseFavorites = true;
        holodexUseFavoritesInitialized = true;
      }
    } else {
      holodexUseFavorites = false;
      holodexUseFavoritesInitialized = false;
    }
  } catch (error) {
    holodexAuthState = { logged_in: false, username: null, expired: false, skip_jwt_verify: false };
    holodexUseFavorites = false;
    holodexUseFavoritesInitialized = false;
  }
  updateHolodexLoginButton();
  updateHolodexModalHint();
}
async function saveHolodexJwt() {
  const jwtInput = document.getElementById('holodex-jwt-input');
  const jwt = jwtInput.value.trim().replace(/^BEARER\s+/i, '');
  if (!jwt) {
    showNotification('请输入 JWT', 'error');
    return;
  }
  try {
    const data = await postJsonApi('/api/config', { holodex_jwt: jwt });
    if (data.success) {
      showNotification('Holodex 登录已保存', 'success');
      jwtInput.value = '';
      holodexUseFavorites = true;
      holodexUseFavoritesInitialized = true;
      closeHolodexLoginModal();
      await loadHolodexAuthStatus();
      await refreshHolodexStreams();
    } else {
      showNotification('保存失败: ' + (data.message || '未知错误'), 'error');
    }
  } catch (error) {
    showNotification('保存失败: ' + error.message, 'error');
  }
}
async function logoutHolodexJwt() {
  try {
    const data = await postJsonApi('/api/config', { holodex_jwt: '' });
    if (data.success) {
      showNotification('已退出 Holodex 登录', 'success');
      holodexUseFavorites = false;
      holodexUseFavoritesInitialized = false;
      document.getElementById('holodex-jwt-input').value = '';
      closeHolodexLoginModal();
      await loadHolodexAuthStatus();
      await refreshHolodexStreams();
    } else {
      showNotification('退出失败: ' + (data.message || '未知错误'), 'error');
    }
  } catch (error) {
    showNotification('退出失败: ' + error.message, 'error');
  }
}
async function saveHolodexApiKey() {
  const apiKeyInput = document.getElementById('holodex-api-input');
  const apiKey = apiKeyInput.value.trim();

  if (!apiKey) {
    showNotification('请输入 API Key', 'error');
    return;
  }

  try {
    // Save the API key to config
    const data = await postJsonApi('/api/config', { holodex_api_key: apiKey });
    if (data.success) {
      showNotification('Holodex API Key 保存成功！', 'success');
      // Refresh the page to update the UI
      setTimeout(() => window.location.reload(), 1500);
    } else {
      showNotification('保存失败: ' + (data.message || '未知错误'), 'error');
    }
  } catch (error) {
    showNotification('保存失败: ' + error.message, 'error');
    console.error('Save API key error:', error);
  }
}
function setHlsCacheLatencyInputState(platform, enabled) {
  const latencyGroup = document.getElementById(`${platform}-hls-cache-latency-group`);
  const latencyInput = document.getElementById(`${platform}-hls-cache-latency`);
  if (latencyGroup) {
    if (latencyGroup.classList.contains('hls-cache-latency-group')) {
      latencyGroup.classList.toggle('hidden', !enabled);
      latencyGroup.style.display = '';
    } else {
      latencyGroup.style.display = enabled ? 'flex' : 'none';
    }
  }
  if (latencyInput) {
    latencyInput.disabled = !enabled;
  }
}
function hlsCacheDom(prefix) {
  return {
    status: document.getElementById(`${prefix}-hls-cache-status`),
    editor: document.getElementById(`${prefix}-hls-cache-edit-container`),
    enabled: document.getElementById(`${prefix}-hls-cache-enabled`),
    latency: document.getElementById(`${prefix}-hls-cache-latency`)
  };
}
function setHlsCacheEditorValues(prefix, cache = {}) {
  const fields = hlsCacheDom(prefix);
  const enabled = !!cache.enabled;
  if (fields.enabled) {
    fields.enabled.checked = enabled;
  }
  if (fields.latency) {
    fields.latency.value = cache.latency_secs || 8;
  }
  setHlsCacheLatencyInputState(prefix, enabled);
}
function readHlsCacheEditorValues(prefix) {
  const fields = hlsCacheDom(prefix);
  return {
    enabled: !!fields.enabled?.checked,
    latencySecs: Math.min(60, Math.max(1, parseInteger(fields.latency?.value, 8)))
  };
}
async function toggleHlsCacheEdit(prefix, apiPlatform) {
  const fields = hlsCacheDom(prefix);
  showInfoRowEdit(fields.status, fields.editor);

  try {
    const result = await getJson(`/api/ffmpeg-cache/${apiPlatform}`);
    setHlsCacheEditorValues(prefix, result.data || {});
  } catch {
    setHlsCacheEditorValues(prefix, { enabled: false, latency_secs: 8 });
  }
}
function cancelHlsCacheEdit(prefix) {
  const fields = hlsCacheDom(prefix);
  hideInfoRowEdit(fields.status, fields.editor);
}
async function saveHlsCacheEdit(prefix, apiPlatform, label) {
  const { enabled, latencySecs } = readHlsCacheEditorValues(prefix);

  try {
    const result = await postJsonApi('/api/ffmpeg-cache/update', {
      platform: apiPlatform,
      enabled,
      latency_secs: latencySecs
    });
    if (!result.success) {
      showNotification(result.message || 'HLS 缓存配置保存失败', 'error');
      return;
    }

    const fields = hlsCacheDom(prefix);
    if (fields.status) {
      fields.status.textContent = formatHlsCacheStatus(enabled, latencySecs);
    }
    cancelHlsCacheEdit(prefix);
    showNotification(`${label} HLS 缓存已更新`, 'success');
  } catch (error) {
    showNotification('HLS 缓存配置保存失败: ' + error.message, 'error');
  }
}
function toggleYtHlsCacheEdit() {
  toggleHlsCacheEdit('yt', 'youtube');
}
function cancelYtHlsCacheEdit() {
  cancelHlsCacheEdit('yt');
}
function saveYtHlsCacheEdit() {
  return saveHlsCacheEdit('yt', 'youtube', 'YouTube');
}
function toggleTwHlsCacheEdit() {
  toggleHlsCacheEdit('tw', 'twitch');
}
function cancelTwHlsCacheEdit() {
  cancelHlsCacheEdit('tw');
}
function saveTwHlsCacheEdit() {
  return saveHlsCacheEdit('tw', 'twitch', 'Twitch');
}
function readHolodexAddChannelData(control) {
  return {
    name: control.dataset.channelName || '',
    youtubeId: control.dataset.youtubeId || '',
    twitchId: control.dataset.twitchId || ''
  };
}
function buildHolodexAddChannelPayload(channelData) {
  const platforms = {};
  if (channelData.youtubeId) platforms.youtube = channelData.youtubeId;
  if (channelData.twitchId) platforms.twitch = channelData.twitchId;

  return {
    name: channelData.name,
    aliases: [],
    platforms,
    riot_puuid: null
  };
}
function addHolodexChannelToCache(payload) {
  if (!state.channelsData || !Array.isArray(state.channelsData.channels)) {
    state.channelsData = { channels: [] };
  }

  const exists = state.channelsData.channels.some(channel => {
    const platforms = channel.platforms || {};
    return normalizeHolodexChannelValue(channel.name) === normalizeHolodexChannelValue(payload.name)
      || (payload.platforms.youtube && normalizeHolodexChannelValue(platforms.youtube) === normalizeHolodexChannelValue(payload.platforms.youtube))
      || (payload.platforms.twitch && normalizeHolodexChannelValue(platforms.twitch) === normalizeHolodexChannelValue(payload.platforms.twitch));
  });

  if (!exists) {
    state.channelsData.channels.push(payload);
  }
}
function createHolodexChannelAddedIndicator() {
  const indicator = document.createElement('span');
  indicator.className = 'holodex-channel-added-icon';
  indicator.title = '已添加到 channels.json';
  indicator.appendChild(createSvgIcon('0 0 24 24', 'M20 6 9 17l-5-5'));
  return indicator;
}
function markHolodexChannelAdded(channelData) {
  document.querySelectorAll('.holodex-channel-add').forEach(control => {
    const data = readHolodexAddChannelData(control);
    const sameChannel = normalizeHolodexChannelValue(data.name) === normalizeHolodexChannelValue(channelData.name)
      || (channelData.youtubeId && normalizeHolodexChannelValue(data.youtubeId) === normalizeHolodexChannelValue(channelData.youtubeId))
      || (channelData.twitchId && normalizeHolodexChannelValue(data.twitchId) === normalizeHolodexChannelValue(channelData.twitchId));

    if (sameChannel) {
      control.classList.add('holodex-channel-add-added');
      control.replaceChildren(createHolodexChannelAddedIndicator());
    }
  });
}
function setHolodexAddChannelConfirmVisible(control, visible) {
  control.classList.toggle('holodex-channel-add-active', visible);
}
function setHolodexAddChannelButtonsDisabled(control, disabled) {
  control.querySelectorAll('button').forEach(button => {
    button.disabled = disabled;
  });
}
async function confirmHolodexAddChannel(button) {
  const control = button.closest('.holodex-channel-add');
  if (!control) return;

  const channelData = readHolodexAddChannelData(control);
  const payload = buildHolodexAddChannelPayload(channelData);
  if (!payload.name || Object.keys(payload.platforms).length === 0) {
    showNotification('频道信息不完整，无法添加', 'error');
    return;
  }

  setHolodexAddChannelButtonsDisabled(control, true);

  try {
    const result = await managementJsonRequest('/api/manage/channels', 'POST', payload);
    const alreadyExists = /already exists/i.test(result.message || '');

    if (result.success || alreadyExists) {
      addHolodexChannelToCache(payload);
      markHolodexChannelAdded(channelData);
      showNotification(
        alreadyExists ? 'channels.json 已有该频道' : (result.message || '已添加到 channels.json'),
        'success'
      );

      const channelsContent = document.getElementById('channels-content');
      if (channelsContent && !isElementHidden(channelsContent)) {
        loadChannels();
      }
    } else {
      showNotification('添加失败: ' + (result.message || 'Unknown error'), 'error');
      setHolodexAddChannelButtonsDisabled(control, false);
    }
  } catch (error) {
    showNotification('添加失败: ' + error.message, 'error');
    setHolodexAddChannelButtonsDisabled(control, false);
  }
}
async function refreshDashboardStatusEndpoint(options) {
  const btn = document.getElementById(options.buttonId);
  const icon = document.getElementById(options.iconId);
  setButtonLoading(btn, icon, true);

  try {
    const result = await getJson(options.endpoint);
    if (!result.success) {
      throw new Error(result.message || options.defaultError);
    }
    await refreshStatus();
    showNotification(options.successMessage, 'success');
  } catch (error) {
    console.error(options.logMessage, error);
    showNotification(`${options.failureMessage}: ${error.message || options.defaultError}`, 'error');
  } finally {
    setButtonLoading(btn, icon, false);
  }
}
function refreshBilibiliStatus() {
  return refreshDashboardStatusEndpoint({
    endpoint: '/api/status',
    buttonId: 'refreshBilibiliBtn',
    iconId: 'refreshBilibiliIcon',
    successMessage: 'Bilibili status refreshed',
    failureMessage: '刷新失败',
    defaultError: '服务器未响应',
    logMessage: 'Error refreshing Bilibili status:'
  });
}
function togglePlatformMonitor(platform, toggleId, endpoint) {
  const toggle = document.getElementById(toggleId);
  if (!toggle) {
    return;
  }

  const state = getMonitorToggleSaveState(platform, toggleId);
  state.desired = toggle.checked;

  if (state.timer) {
    clearTimeout(state.timer);
  }
  state.timer = setTimeout(() => {
    state.timer = null;
    flushPlatformMonitorToggle(platform, toggleId, endpoint);
  }, monitorToggleSaveDebounceMs);
}
function getMonitorToggleSaveState(platform, toggleId) {
  let toggleState = state.monitorToggleSaveState.get(toggleId);
  if (!toggleState) {
    const confirmed = window.configData[platform]?.enable_monitor !== false;
    toggleState = {
      confirmed,
      desired: confirmed,
      inFlight: false,
      timer: null
    };
    state.monitorToggleSaveState.set(toggleId, toggleState);
  }
  return toggleState;
}
async function flushPlatformMonitorToggle(platform, toggleId, endpoint) {
  const state = getMonitorToggleSaveState(platform, toggleId);
  if (state.inFlight) {
    return;
  }

  const toggle = document.getElementById(toggleId);
  const enabled = state.desired;
  state.inFlight = true;
  try {
    const result = await postJsonApi(endpoint, { enabled });
    if (result.success) {
      state.confirmed = enabled;
      window.configData[platform] = {
        ...(window.configData[platform] || {}),
        enable_monitor: enabled
      };
      if (state.desired === enabled) {
        showNotification(result.message, 'success');
        refreshStatus().catch((error) => {
          console.debug('Failed to refresh status after monitor toggle:', error);
        });
      }
    } else {
      if (state.desired === enabled) {
        state.desired = state.confirmed;
        if (toggle) {
          toggle.checked = state.confirmed;
        }
        showNotification(result.message || '保存失败', 'error');
      }
    }
  } catch (error) {
    console.error(`Failed to toggle ${platform} monitor:`, error);
    if (state.desired === enabled) {
      state.desired = state.confirmed;
      if (toggle) {
        toggle.checked = state.confirmed;
      }
      showNotification('保存失败: ' + error.message, 'error');
    }
  } finally {
    state.inFlight = false;
    if (state.desired !== state.confirmed) {
      flushPlatformMonitorToggle(platform, toggleId, endpoint);
    }
  }
}
function toggleYouTubeMonitor() {
  return togglePlatformMonitor('youtube', 'youtube-monitor-toggle', '/api/toggle-youtube-monitor');
}
function toggleTwitchMonitor() {
  return togglePlatformMonitor('twitch', 'twitch-monitor-toggle', '/api/toggle-twitch-monitor');
}
async function refreshPlatformStatus(label, endpoint, buttonId, iconId) {
  const btn = document.getElementById(buttonId);
  const icon = document.getElementById(iconId);

  setButtonLoading(btn, icon, true);

  try {
    const data = await getJson(endpoint);
    if (data.success) {
      showNotification(`${label} status refreshed`, 'success');
      await refreshStatus();
    } else {
      showNotification(data.message || `Failed to refresh ${label} status`, 'error');
    }
  } catch (error) {
    console.error(`Error refreshing ${label} status:`, error);
    showNotification(`Failed to refresh ${label} status: ${error.message}`, 'error');
  } finally {
    setButtonLoading(btn, icon, false);
  }
}
function refreshYouTubeStatus() {
  return refreshPlatformStatus('YouTube', '/api/refresh/youtube', 'refreshYouTubeBtn', 'refreshYouTubeIcon');
}
function refreshTwitchStatus() {
  return refreshPlatformStatus('Twitch', '/api/refresh/twitch', 'refreshTwitchBtn', 'refreshTwitchIcon');
}
function syncInfoRowLabelCenter(rowElement, valueElement) {
  if (!rowElement || !valueElement) {
    return;
  }

  const lineHeight = parseFloat(getComputedStyle(valueElement).lineHeight);
  const isMultiLine = Number.isFinite(lineHeight)
    && lineHeight > 0
    && valueElement.offsetHeight > lineHeight * 1.5;
  rowElement.classList.toggle('info-row-label-center', isMultiLine);
}
function syncPlatformTitleRowCenters() {
  syncInfoRowLabelCenter(
    document.getElementById('yt-title-row'),
    document.getElementById('yt-title')
  );
  syncInfoRowLabelCenter(
    document.getElementById('tw-title-row'),
    document.getElementById('tw-title')
  );
}
// Mirrors the Bilibili room state into the top bar so the current state
// is readable from every view.
function schedulePlatformTitleRowCenters() {
  requestAnimationFrame(() => {
    requestAnimationFrame(syncPlatformTitleRowCenters);
  });
}
async function refreshNetworkStatus() {
  if (networkRefreshInFlight || !isBiliNetworkLive()) {
    return;
  }

  networkRefreshInFlight = true;
  try {
    const result = await getJson('/api/network-status');
    if (result.success && result.data) {
      renderBiliNetworkPanel({
        ...result.data,
        is_live: isBiliNetworkLive(),
        stream_quality: getBiliNetworkQuality(),
      });
    }
  } catch (error) {
    console.debug('Failed to refresh network status:', error);
  } finally {
    networkRefreshInFlight = false;
  }
}
async function refreshStatus() {
  if (statusRefreshInFlight) {
    statusRefreshQueued = true;
    return;
  }

  statusRefreshInFlight = true;
  statusRefreshQueued = false;
  lastStatusRefreshMs = Date.now();

  try {
    const data = await getJson('/api/status');

    if (!data.success) {
      // API returned an error
      throw new Error(data.message || '服务器返回错误');
    }

    if (data.success && data.data) {
      renderStatusCards(data.data);
    } else {
      console.error('Invalid API response:', data);
      showNotification('获取状态失败：响应格式错误', 'error');
      setStatusCardsMessage('数据错误');
    }
  } catch (error) {
    console.error('Failed to refresh status:', error);

    // Suppress network errors (when server is down)
    if (error.message && error.message.includes('NetworkError')) {
      return;
    }

    // Show more helpful error messages for other errors
    let errorMsg = error.message;
    if (errorMsg.includes('权限错误') || errorMsg.includes('Permission denied')) {
      errorMsg = '配置文件权限错误！请在可执行文件所在目录运行程序。';
    } else if (errorMsg.includes('不存在') || errorMsg.includes('No such file')) {
      errorMsg = '配置文件不存在！请在 Web UI 中完成设置。';
    } else if (errorMsg.includes('Failed to fetch')) {
      // Also suppress generic fetch failures
      return;
    }

    showNotification(errorMsg, 'error');

    // Show connection error state
    setStatusCardsMessage('配置错误');
  } finally {
    statusRefreshInFlight = false;
    schedulePlatformTitleRowCenters();
    if (statusRefreshQueued) {
      refreshStatus();
    }
  }
}
async function startStream() {
  try {
    const data = await postJsonApi('/api/start', {});

    // Check if face verification is required
    if (data.data && data.data.requires_face_auth) {
      showFaceAuthModal(data.data.qr_url);
      showNotification(data.message || '需要人脸验证', 'error');
      return;
    }

    // Check if the operation was successful
    if (data.success) {
      showNotification(data.message || '直播已开始', 'success');
    } else {
      showNotification(data.message || '开播失败', 'error');
    }
    setTimeout(refreshStatus, 2000);
  } catch (error) {
    console.error('Start stream error:', error);
    showNotification('操作失败: ' + error.message, 'error');
  }
}
async function stopStream() {
  try {
    const data = await postJsonApi('/api/stop');
    showNotification(data.message || (data.success ? '直播已停止' : '停播失败'), data.success ? 'success' : 'error');
    setTimeout(refreshStatus, 2000);
  } catch (error) {
    showNotification('操作失败: ' + error.message, 'error');
  }
}
async function restartStream() {
  if (!confirm('确定要停止当前流并重新加载配置吗？')) {
    return;
  }

  const btn = document.getElementById('restartStreamBtn');
  const icon = document.getElementById('restartStreamIcon');

  // Disable button and start spinning animation
  setButtonLoading(btn, icon, true);

  try {
    const data = await postJsonApi('/api/restart');
    showNotification(data.message || (data.success ? '已重启流' : '重启失败'), data.success ? 'success' : 'error');
    setTimeout(refreshStatus, 2000);
  } catch (error) {
    showNotification('操作失败: ' + error.message, 'error');
  } finally {
    // Re-enable button and stop spinning after a delay
    setTimeout(() => {
      setButtonLoading(btn, icon, false);
    }, 2000);
  }
}
function postTitleUpdate(title) {
  return postJsonApi('/api/title', { title });
}
function postChannelUpdate(payload) {
  return postJsonApi('/api/channel', payload);
}
async function changeTitle() {
  const title = document.getElementById('title-input').value.trim();
  if (!title) {
    showNotification('请输入直播标题', 'error');
    return;
  }

  try {
    const data = await postTitleUpdate(title);
    if (data.success) {
      showNotification(data.message || '直播标题已更新', 'success');
      document.getElementById('title-input').value = '';
      setTimeout(refreshStatus, 2000);
    } else {
      showNotification(data.message || '更新失败', 'error');
    }
  } catch (error) {
    showNotification('更新失败: ' + error.message, 'error');
  }
}
// Inline row edit helpers
function showInfoRowEdit(valueEl, editContainer) {
  const valueContainer = valueEl?.parentElement;
  if (!valueContainer || !editContainer) {
    return;
  }

  valueContainer.classList.add('hidden');
  valueContainer.style.display = '';
  editContainer.classList.remove('hidden');
  editContainer.style.display = '';
}
function hideInfoRowEdit(valueEl, editContainer) {
  const valueContainer = valueEl?.parentElement;
  if (!valueContainer || !editContainer) {
    return;
  }

  editContainer.classList.add('hidden');
  editContainer.style.display = '';
  valueContainer.classList.remove('hidden');
  valueContainer.style.display = '';
}
function handleTitleEditKeydown(event) {
  if (event.key === 'Enter') {
    saveTitleEdit();
  } else if (event.key === 'Escape') {
    cancelTitleEdit();
  }
}
// Inline title editing functions
function toggleTitleEdit() {
  const titleSpan = document.getElementById('bili-title');
  const editContainer = document.getElementById('title-edit-container');
  const editInput = document.getElementById('title-edit-input');

  showInfoRowEdit(titleSpan, editContainer);

  // Set current title as input value
  editInput.value = titleSpan.textContent === '-' ? '' : titleSpan.textContent;
  editInput.focus();
  editInput.select();
}
function cancelTitleEdit() {
  const titleSpan = document.getElementById('bili-title');
  const editContainer = document.getElementById('title-edit-container');

  hideInfoRowEdit(titleSpan, editContainer);
}
// YouTube channel editing functions
function toggleYtChannelEdit() {
  const channelSpan = document.getElementById('yt-channel-name');
  const editContainer = document.getElementById('yt-channel-edit-container');
  const editSelect = document.getElementById('yt-channel-edit-select');

  showInfoRowEdit(channelSpan, editContainer);

  // Load channels if not already loaded
  if (!state.channelsData) {
    loadChannelData().then(() => {
      populateYtChannelEditSelect();
    });
  } else {
    populateYtChannelEditSelect();
  }

  editSelect.focus();
}
function populateYtChannelEditSelect() {
  const editSelect = document.getElementById('yt-channel-edit-select');
  editSelect.replaceChildren(createSelectOption('', '选择频道...'));
  appendPlatformChannelOptions(editSelect, 'youtube');
}
function cancelYtChannelEdit() {
  const channelSpan = document.getElementById('yt-channel-name');
  const editContainer = document.getElementById('yt-channel-edit-container');

  hideInfoRowEdit(channelSpan, editContainer);
}
async function saveYtChannelEdit() {
  const editSelect = document.getElementById('yt-channel-edit-select');
  const selectedValue = editSelect.value;

  if (!selectedValue) {
    showNotification('请选择频道', 'error');
    return;
  }

  try {
    const channelInfo = JSON.parse(selectedValue);
    const data = await postChannelUpdate({
      platform: 'youtube',
      channel_id: channelInfo.id,
      channel_name: channelInfo.name
    });

    if (!data.success) {
      showNotification(data.message || 'YouTube频道更新失败', 'error');
      return;
    }

    showNotification(data.message || 'YouTube频道已更新', 'success');
    cancelYtChannelEdit();
    // Refresh status immediately to show updated info
    refreshStatus();
  } catch (error) {
    showNotification('更新失败: ' + error.message, 'error');
  }
}
// YouTube area editing functions
function toggleYtAreaEdit() {
  const areaSpan = document.getElementById('yt-area');
  const editContainer = document.getElementById('yt-area-edit-container');
  const editSelect = document.getElementById('yt-area-edit-select');

  showInfoRowEdit(areaSpan, editContainer);

  // Load areas if not already loaded
  if (!state.areasData) {
    loadChannelData().then(() => {
      populateYtAreaEditSelect();
    });
  } else {
    populateYtAreaEditSelect();
  }

  editSelect.focus();
}
function populateYtAreaEditSelect() {
  const editSelect = document.getElementById('yt-area-edit-select');
  editSelect.replaceChildren(createAreaOption('', '选择分区...'));

  if (state.areasData && state.areasData.areas) {
    appendAreaOptions(editSelect, state.areasData.areas);
  }
}
function cancelYtAreaEdit() {
  const areaSpan = document.getElementById('yt-area');
  const editContainer = document.getElementById('yt-area-edit-container');

  hideInfoRowEdit(areaSpan, editContainer);
}
async function saveYtAreaEdit() {
  const editSelect = document.getElementById('yt-area-edit-select');
  const areaId = parseInteger(editSelect.value, 0);

  if (!areaId) {
    showNotification('请选择分区', 'error');
    return;
  }

  try {
    const data = await postChannelUpdate({
      platform: 'youtube',
      area_id: areaId
    });

    if (!data.success) {
      showNotification(data.message || 'YouTube配置分区更新失败', 'error');
      return;
    }

    showNotification(data.message || 'YouTube配置分区已更新', 'success');
    cancelYtAreaEdit();
    // Refresh status immediately to show updated info
    refreshStatus();
  } catch (error) {
    showNotification('更新失败: ' + error.message, 'error');
  }
}
// YouTube quality editing functions
function toggleYtQualityEdit() {
  const qualitySpan = document.getElementById('yt-quality');
  const editContainer = document.getElementById('yt-quality-edit-container');
  const editSelect = document.getElementById('yt-quality-edit-select');

  showInfoRowEdit(qualitySpan, editContainer);

  // Populate quality options for YouTube
  populateQualityOptions(editSelect, 'youtube');

  // Set current quality as selected - convert display text back to technical value
  const currentDisplayText = qualitySpan.textContent;
  if (currentDisplayText && currentDisplayText !== '-') {
    const technicalValue = getTechnicalQualityValue(currentDisplayText, 'youtube');
    editSelect.value = technicalValue;
  }

  editSelect.focus();
}
function cancelYtQualityEdit() {
  const qualitySpan = document.getElementById('yt-quality');
  const editContainer = document.getElementById('yt-quality-edit-container');

  hideInfoRowEdit(qualitySpan, editContainer);
}
async function saveYtQualityEdit() {
  const editSelect = document.getElementById('yt-quality-edit-select');
  const quality = editSelect.value;

  if (!quality) {
    showNotification('请选择画质', 'error');
    return;
  }

  try {
    const data = await postChannelUpdate({
      platform: 'youtube',
      quality // Send technical value to server
    });

    if (!data.success) {
      showNotification(data.message || 'YouTube画质更新失败', 'error');
      return;
    }

    showNotification(data.message || 'YouTube画质已更新', 'success');
    cancelYtQualityEdit();
    // Update the display immediately with display text
    document.getElementById('yt-quality').textContent = getQualityDisplayText(quality, 'youtube');
    // Refresh status to get updated info
    refreshStatus();
  } catch (error) {
    showNotification('更新失败: ' + error.message, 'error');
  }
}
// Twitch quality editing functions
function toggleTwQualityEdit() {
  const qualitySpan = document.getElementById('tw-quality');
  const editContainer = document.getElementById('tw-quality-edit-container');
  const editSelect = document.getElementById('tw-quality-edit-select');

  showInfoRowEdit(qualitySpan, editContainer);

  // Populate quality options for Twitch
  populateQualityOptions(editSelect, 'twitch');

  // Set current quality as selected - convert display text back to technical value
  const currentDisplayText = qualitySpan.textContent;
  if (currentDisplayText && currentDisplayText !== '-') {
    const technicalValue = getTechnicalQualityValue(currentDisplayText, 'twitch');
    editSelect.value = technicalValue;
  }

  editSelect.focus();
}
function cancelTwQualityEdit() {
  const qualitySpan = document.getElementById('tw-quality');
  const editContainer = document.getElementById('tw-quality-edit-container');

  hideInfoRowEdit(qualitySpan, editContainer);
}
async function saveTwQualityEdit() {
  const editSelect = document.getElementById('tw-quality-edit-select');
  const quality = editSelect.value;

  if (!quality) {
    showNotification('请选择画质', 'error');
    return;
  }

  try {
    const data = await postChannelUpdate({
      platform: 'twitch',
      quality // Send technical value to server
    });

    if (!data.success) {
      showNotification(data.message || 'Twitch画质更新失败', 'error');
      return;
    }

    showNotification(data.message || 'Twitch画质已更新', 'success');
    cancelTwQualityEdit();
    // Update the display immediately with display text
    document.getElementById('tw-quality').textContent = getQualityDisplayText(quality, 'twitch');
    // Refresh status to get updated info
    refreshStatus();
  } catch (error) {
    showNotification('更新失败: ' + error.message, 'error');
  }
}
// Twitch channel editing functions
function toggleTwChannelEdit() {
  const channelSpan = document.getElementById('tw-channel-name');
  const editContainer = document.getElementById('tw-channel-edit-container');
  const editSelect = document.getElementById('tw-channel-edit-select');

  showInfoRowEdit(channelSpan, editContainer);

  // Load channels if not already loaded
  if (!state.channelsData) {
    loadChannelData().then(() => {
      populateTwChannelEditSelect();
    });
  } else {
    populateTwChannelEditSelect();
  }

  editSelect.focus();
}
function populateTwChannelEditSelect() {
  const editSelect = document.getElementById('tw-channel-edit-select');
  editSelect.replaceChildren(createSelectOption('', '选择频道...'));
  appendPlatformChannelOptions(editSelect, 'twitch');
}
function cancelTwChannelEdit() {
  const channelSpan = document.getElementById('tw-channel-name');
  const editContainer = document.getElementById('tw-channel-edit-container');

  hideInfoRowEdit(channelSpan, editContainer);
}
async function saveTwChannelEdit() {
  const editSelect = document.getElementById('tw-channel-edit-select');
  const selectedValue = editSelect.value;

  if (!selectedValue) {
    showNotification('请选择频道', 'error');
    return;
  }

  try {
    const channelInfo = JSON.parse(selectedValue);
    const data = await postChannelUpdate({
      platform: 'twitch',
      channel_id: channelInfo.id,
      channel_name: channelInfo.name
    });

    if (!data.success) {
      showNotification(data.message || 'Twitch频道更新失败', 'error');
      return;
    }

    showNotification(data.message || 'Twitch频道已更新', 'success');
    cancelTwChannelEdit();
    // Refresh status immediately to show updated info
    refreshStatus();
  } catch (error) {
    showNotification('更新失败: ' + error.message, 'error');
  }
}
// Twitch area editing functions
function toggleTwAreaEdit() {
  const areaSpan = document.getElementById('tw-area');
  const editContainer = document.getElementById('tw-area-edit-container');
  const editSelect = document.getElementById('tw-area-edit-select');

  showInfoRowEdit(areaSpan, editContainer);

  // Load areas if not already loaded
  if (!state.areasData) {
    loadChannelData().then(() => {
      populateTwAreaEditSelect();
    });
  } else {
    populateTwAreaEditSelect();
  }

  editSelect.focus();
}
function populateTwAreaEditSelect() {
  const editSelect = document.getElementById('tw-area-edit-select');
  editSelect.replaceChildren(createAreaOption('', '选择分区...'));

  if (state.areasData && state.areasData.areas) {
    appendAreaOptions(editSelect, state.areasData.areas);
  }
}
function cancelTwAreaEdit() {
  const areaSpan = document.getElementById('tw-area');
  const editContainer = document.getElementById('tw-area-edit-container');

  hideInfoRowEdit(areaSpan, editContainer);
}
async function saveTwAreaEdit() {
  const editSelect = document.getElementById('tw-area-edit-select');
  const areaId = parseInteger(editSelect.value, 0);

  if (!areaId) {
    showNotification('请选择分区', 'error');
    return;
  }

  try {
    const data = await postChannelUpdate({
      platform: 'twitch',
      area_id: areaId
    });

    if (!data.success) {
      showNotification(data.message || 'Twitch配置分区更新失败', 'error');
      return;
    }

    showNotification(data.message || 'Twitch配置分区已更新', 'success');
    cancelTwAreaEdit();
    // Refresh status immediately to show updated info
    refreshStatus();
  } catch (error) {
    showNotification('更新失败: ' + error.message, 'error');
  }
}
// Area editing functions
function toggleAreaEdit() {
  const areaSpan = document.getElementById('bili-area');
  const editContainer = document.getElementById('area-edit-container');
  const editSelect = document.getElementById('area-edit-select');

  showInfoRowEdit(areaSpan, editContainer);

  // Load areas if not already loaded
  if (!state.areasData) {
    loadChannelData().then(() => {
      populateAreaEditSelect();
    });
  } else {
    populateAreaEditSelect();
  }

  editSelect.focus();
}
function populateAreaEditSelect() {
  const editSelect = document.getElementById('area-edit-select');
  editSelect.replaceChildren(createAreaOption('', '选择分区...'));
  appendAreaOptions(editSelect, getAreaList());
}
function cancelAreaEdit() {
  const areaSpan = document.getElementById('bili-area');
  const editContainer = document.getElementById('area-edit-container');

  hideInfoRowEdit(areaSpan, editContainer);
}
async function saveAreaEdit() {
  const editSelect = document.getElementById('area-edit-select');
  const areaId = parseInteger(editSelect.value, 0);

  if (!areaId) {
    showNotification('请选择分区', 'error');
    return;
  }

  try {
    const data = await postJsonApi('/api/area', { area_id: areaId });
    if (!data.success) {
      showNotification(data.message || '分区更新失败', 'error');
      return;
    }

    showNotification(data.message || '分区已更新', 'success');
    cancelAreaEdit();
    setTimeout(refreshStatus, 2000);
  } catch (error) {
    showNotification('更新失败: ' + error.message, 'error');
  }
}
async function saveTitleEdit() {
  const editInput = document.getElementById('title-edit-input');
  const title = editInput.value.trim();

  if (!title) {
    showNotification('请输入直播标题', 'error');
    return;
  }

  try {
    const data = await postTitleUpdate(title);
    if (data.success) {
      showNotification(data.message || '直播标题已更新', 'success');
      cancelTitleEdit();
      setTimeout(refreshStatus, 2000);
    } else {
      showNotification(data.message || '更新失败', 'error');
    }
  } catch (error) {
    showNotification('更新失败: ' + error.message, 'error');
  }
}
// Channel management functions
async function loadChannelData() {
  // Start continuous spinning animation
  const icon = document.getElementById('loadChannelIcon');
  setButtonLoading(null, icon, true);

  try {
    const [channelsResult, areasResult] = await Promise.all([
      getJson('/api/channels'),
      getJson('/api/areas')
    ]);
    state.channelsData = channelsResult;
    state.areasData = normalizeAreaData(areasResult);

    // Populate the removed legacy channel-management area select if present.
    const areaSelect = document.getElementById('area-select');
    if (areaSelect) {
      areaSelect.replaceChildren(createAreaOption('', '不修改分区'));
    }

    const areasList = getAreaList();

    if (areasList.length > 0) {
      if (areaSelect) {
        // Populate channel management area select
        appendAreaOptions(areaSelect, areasList, true);
      }

    } else {
      console.error('No areas data found:', state.areasData);
      showNotification('未找到分区数据', 'error');
    }

    // Update the removed legacy channel list if the controls still exist.
    if (document.getElementById('platform-select') && document.getElementById('channel-select')) {
      updateChannelList();
    }
  } catch (error) {
    console.error('Failed to load channel data:', error);
    showNotification('加载频道数据失败: ' + error.message, 'error');
  } finally {
    // Stop spinning animation when complete
    setButtonLoading(null, icon, false);
  }
}
function updateChannelList() {
  const platformSelect = document.getElementById('platform-select');
  const channelSelect = document.getElementById('channel-select');
  if (!platformSelect || !channelSelect) return;

  const platform = platformSelect.value;

  channelSelect.replaceChildren(createSelectOption('', '从 channels.json 选择或手动输入...'));
  appendPlatformChannelOptions(channelSelect, platform);

  // Don't clear inputs - allow manual entry to persist
  // Update quality options based on platform
  updateQualityOptions();
}
function updateQualityOptions() {
  const platformSelect = document.getElementById('platform-select');
  const qualitySelect = document.getElementById('quality-select');
  if (!platformSelect || !qualitySelect) return;

  const platform = platformSelect.value;

  // Store current value
  const currentValue = qualitySelect.value;

  // Clear all options except the first "不修改画质" option
  const firstOption = qualitySelect.querySelector('option[value=""]');
  if (firstOption) {
    qualitySelect.replaceChildren(firstOption);
  } else {
    qualitySelect.replaceChildren();
  }

  // Add platform-specific quality options if platform is selected
  if (platform) {
    populateQualityOptions(qualitySelect, platform, false); // false = don't add empty option
  }

  // Try to restore previous value if it's still valid
  if (currentValue && qualitySelect.querySelector(`option[value="${currentValue}"]`)) {
    qualitySelect.value = currentValue;
  }
}
function updateChannelInfo() {
  const channelSelect = document.getElementById('channel-select');
  const selectedValue = channelSelect.value;

  if (!selectedValue) {
    document.getElementById('channel-id-input').value = '';
    document.getElementById('channel-name-input').value = '';
    return;
  }

  try {
    const channelInfo = JSON.parse(selectedValue);
    document.getElementById('channel-id-input').value = channelInfo.id;
    document.getElementById('channel-name-input').value = channelInfo.name;
  } catch (error) {
    console.error('Failed to parse channel info:', error);
  }
}
// Quality mapping functions for display vs technical values
function getTechnicalQualityValue(displayText, platform = 'youtube') {
  const reverseMappings = {
    youtube: {
      '最佳质量': 'best',
      '超清 (1080p)': 'best[height<=1080]',
      '高清 (720p)': 'best[height<=720]',
      '标清 (480p)': 'best[height<=480]',
      '流畅 (360p)': 'best[height<=360]',
      '最低质量': 'worst'
    },
    twitch: {
      '原画质量': 'best',
      '高质量 (720p)': 'high',
      '中等质量 (540p)': 'medium',
      '低质量 (360p)': 'low',
      '仅音频': 'audio_only',
      '最低质量': 'worst'
    }
  };

  return reverseMappings[platform][displayText] || displayText;
}
// Unified function to populate quality options for any select element
function populateQualityOptions(selectElement, platform, includeEmpty = true) {
  if (!selectElement) return;

  // Clear existing options
  selectElement.replaceChildren();

  // Add empty option if requested
  if (includeEmpty) {
    selectElement.appendChild(createSelectOption('', '选择画质...'));
  }

  // Get quality mappings for the platform
  const qualityMappings = {
    youtube: {
      'best': '最佳质量',
      'best[height<=1080]': '超清 (1080p)',
      'best[height<=720]': '高清 (720p)',
      'best[height<=480]': '标清 (480p)',
      'best[height<=360]': '流畅 (360p)',
      'worst': '最低质量'
    },
    twitch: {
      'best': '原画质量',
      'high': '高质量 (720p)',
      'medium': '中等质量 (540p)',
      'low': '低质量 (360p)',
      'audio_only': '仅音频',
      'worst': '最低质量'
    }
  };

  // Add options for the specified platform
  const platformQualities = qualityMappings[platform];
  if (platformQualities) {
    Object.entries(platformQualities).forEach(([value, text]) => {
      const option = document.createElement('option');
      option.value = value;
      option.textContent = text;
      selectElement.appendChild(option);
    });
  }
}
async function applyChannelChange() {
  const platform = document.getElementById('platform-select').value;
  const channelId = document.getElementById('channel-id-input').value.trim();
  const channelName = document.getElementById('channel-name-input').value.trim();
  const areaId = document.getElementById('area-select').value;
  const quality = document.getElementById('quality-select').value;
  const riotApiKey = document.getElementById('riot-api-key-input').value.trim();
  const enableLolMonitor = document.getElementById('enable-lol-monitor-inline').checked;

  if (!channelId || !channelName) {
    showNotification('请输入频道ID和频道名称', 'error');
    return;
  }

  try {
    // First, update channel info
    const channelPayload = {
      platform: platform,
      channel_id: channelId,
      channel_name: channelName
    };

    if (areaId) {
      channelPayload.area_id = parseInteger(areaId, 0);

      // Include Riot API Key if area is 86 and key is provided
      if (areaId === '86' && riotApiKey) {
        channelPayload.riot_api_key = riotApiKey;
      }
    }

    if (quality) {
      channelPayload.quality = quality;
    }

    const channelData = await postJsonApi('/api/channel', channelPayload);
    if (!channelData.success) {
      showNotification(channelData.message || '频道更新失败', 'error');
      return;
    }

    // If area is 86, also update enable_lol_monitor setting
    if (areaId === '86') {
      const configPayload = {
        enable_lol_monitor: enableLolMonitor
      };

      // Include Riot API Key in config update if provided
      if (riotApiKey) {
        configPayload.riot_api_key = riotApiKey;
      }

      const configData = await postJsonApi('/api/config', configPayload);
      if (!configData.success) {
        showNotification(configData.message || 'LoL 监控配置更新失败', 'error');
        return;
      }

      // Update global config
      window.configData.enable_lol_monitor = enableLolMonitor;
      if (riotApiKey) {
        window.configData.riot_api_key = riotApiKey;
      }
    }

    showNotification(channelData.message || '频道已更新', 'success');

    // Clear Riot API Key input after successful update
    if (riotApiKey) {
      document.getElementById('riot-api-key-input').value = '';
    }

    setTimeout(refreshStatus, 2000);
  } catch (error) {
    showNotification('更新失败: ' + error.message, 'error');
  }
}
// Add event listener for switch buttons using data attributes
document.addEventListener('click', function (e) {
  const addStart = e.target.closest('.holodex-add-channel-start');
  if (addStart) {
    const control = addStart.closest('.holodex-channel-add');
    if (control) setHolodexAddChannelConfirmVisible(control, true);
    return;
  }

  const addReject = e.target.closest('.holodex-add-channel-reject');
  if (addReject) {
    const control = addReject.closest('.holodex-channel-add');
    if (control) setHolodexAddChannelConfirmVisible(control, false);
    return;
  }

  const addConfirm = e.target.closest('.holodex-add-channel-confirm');
  if (addConfirm) {
    confirmHolodexAddChannel(addConfirm);
    return;
  }

  const switchButton = e.target.closest('.switch-button');
  if (switchButton) {
    const action = readHolodexStreamActionData(switchButton);
    switchToHolodexStream(
      action.channelId,
      action.suggestedAreaId,
      action.title,
      action.topicId,
      action.status,
      action.platform,
      action.twitchChannelId,
      action.externalLink
    );
    return;
  }

  const cropSwitchButton = e.target.closest('.crop-switch-button');
  if (cropSwitchButton) {
    const action = readHolodexStreamActionData(cropSwitchButton);
    cropAndSwitchToHolodexStream(
      action.channelId,
      action.suggestedAreaId,
      action.title,
      action.topicId,
      action.status,
      action.platform,
      action.twitchChannelId,
      action.externalLink
    );
    return;
  }
});
// YouTube cookies editing functions
function toggleYtCookiesEdit() {
  const statusSpan = document.getElementById('yt-cookies-status');
  const editContainer = document.getElementById('yt-cookies-edit-container');

  showInfoRowEdit(statusSpan, editContainer);
}
function cancelYtCookiesEdit() {
  const statusSpan = document.getElementById('yt-cookies-status');
  const editContainer = document.getElementById('yt-cookies-edit-container');

  hideInfoRowEdit(statusSpan, editContainer);
}
async function saveYtCookiesEdit() {
  const browserSelect = document.getElementById('yt-cookies-browser-select');
  const fileInput = document.getElementById('yt-cookies-file-input');

  const cookiesFromBrowser = browserSelect.value;
  const cookiesFile = fileInput.value.trim();

  try {
    const data = await postChannelUpdate({
      platform: 'youtube',
      cookies_from_browser: cookiesFromBrowser,
      cookies_file: cookiesFile
    });
    if (!data.success) {
      showNotification(data.message || 'YouTube Cookies 更新失败', 'error');
      return;
    }

    showNotification(data.message || 'YouTube Cookies 已更新', 'success');

    // Update status display
    updateYtCookiesStatus(cookiesFromBrowser, cookiesFile);

    cancelYtCookiesEdit();
    refreshStatus();
  } catch (error) {
    showNotification('更新失败: ' + error.message, 'error');
  }
}
function updateYtCookiesStatus(browser, file) {
  const statusSpan = document.getElementById('yt-cookies-status');
  if (browser) {
    statusSpan.textContent = `浏览器: ${browser}`;
  } else if (file) {
    statusSpan.textContent = `文件: ${file.split('/').pop()}`;
  } else {
    statusSpan.textContent = '未配置';
  }
}

export {
  initDashboardControls,
  initHolodexLoginModalControls,
  initFaceAuthModalControls,
  initStatusRefresh,
  applyHolodexSectionVisibility,
  maybeLoadHolodexStreams,
  setHolodexCollapsed,
  toggleHolodexFold,
  initHolodexFold,
  setHolodexStatus,
  hideHolodexStatus,
  createHolodexScheduleDivider,
  refreshHolodexStreams,
  getHolodexPlaceholderKind,
  createHolodexStreamSvg,
  createHolodexWatchIcon,
  createHolodexPlaceholderIcon,
  createHolodexPlaceholderDurationOverlay,
  createHolodexDurationOverlay,
  escapeHolodexHtml,
  formatHolodexDuration,
  getHolodexStreamStartMs,
  stopHolodexDurationTicker,
  updateHolodexDurations,
  startHolodexDurationTicker,
  getHolodexChannelUrl,
  getHolodexChannelPhotoUrl,
  createHolodexAvatarBlock,
  refreshHolodexChannelsData,
  normalizeHolodexChannelValue,
  holodexStreamHasConfiguredChannel,
  createHolodexChannelAddIcon,
  createHolodexAddChannelControls,
  createHolodexChannelBlock,
  createHolodexStreamActionButton,
  readHolodexStreamActionData,
  parseTwitchLoginFromLink,
  createStreamCard,
  switchToHolodexStream,
  showAreaModal,
  closeAreaModal,
  initAreaModalControls,
  showFaceAuthModal,
  createFaceAuthQrFallback,
  closeFaceAuthModal,
  retryStartStream,
  confirmAreaSelection,
  createHolodexSwitchPayload,
  performSwitch,
  cropAndSwitchToHolodexStream,
  testHolodexApiKey,
  updateHolodexAuthBar,
  toggleHolodexFavoritesMode,
  openHolodexLoginModal,
  closeHolodexLoginModal,
  updateHolodexLoginButton,
  updateHolodexModalHint,
  toggleHolodexSkipJwtVerify,
  openHolodexLogin,
  loadHolodexAuthStatus,
  saveHolodexJwt,
  logoutHolodexJwt,
  saveHolodexApiKey,
  setHlsCacheLatencyInputState,
  hlsCacheDom,
  setHlsCacheEditorValues,
  readHlsCacheEditorValues,
  toggleHlsCacheEdit,
  cancelHlsCacheEdit,
  saveHlsCacheEdit,
  toggleYtHlsCacheEdit,
  cancelYtHlsCacheEdit,
  saveYtHlsCacheEdit,
  toggleTwHlsCacheEdit,
  cancelTwHlsCacheEdit,
  saveTwHlsCacheEdit,
  readHolodexAddChannelData,
  buildHolodexAddChannelPayload,
  addHolodexChannelToCache,
  createHolodexChannelAddedIndicator,
  markHolodexChannelAdded,
  setHolodexAddChannelConfirmVisible,
  setHolodexAddChannelButtonsDisabled,
  confirmHolodexAddChannel,
  refreshDashboardStatusEndpoint,
  refreshBilibiliStatus,
  togglePlatformMonitor,
  getMonitorToggleSaveState,
  flushPlatformMonitorToggle,
  toggleYouTubeMonitor,
  toggleTwitchMonitor,
  refreshPlatformStatus,
  refreshYouTubeStatus,
  refreshTwitchStatus,
  syncInfoRowLabelCenter,
  syncPlatformTitleRowCenters,
  schedulePlatformTitleRowCenters,
  refreshNetworkStatus,
  refreshStatus,
  startStream,
  stopStream,
  restartStream,
  postTitleUpdate,
  postChannelUpdate,
  changeTitle,
  showInfoRowEdit,
  hideInfoRowEdit,
  handleTitleEditKeydown,
  toggleTitleEdit,
  cancelTitleEdit,
  toggleYtChannelEdit,
  populateYtChannelEditSelect,
  cancelYtChannelEdit,
  saveYtChannelEdit,
  toggleYtAreaEdit,
  populateYtAreaEditSelect,
  cancelYtAreaEdit,
  saveYtAreaEdit,
  toggleYtQualityEdit,
  cancelYtQualityEdit,
  saveYtQualityEdit,
  toggleTwQualityEdit,
  cancelTwQualityEdit,
  saveTwQualityEdit,
  toggleTwChannelEdit,
  populateTwChannelEditSelect,
  cancelTwChannelEdit,
  saveTwChannelEdit,
  toggleTwAreaEdit,
  populateTwAreaEditSelect,
  cancelTwAreaEdit,
  saveTwAreaEdit,
  toggleAreaEdit,
  populateAreaEditSelect,
  cancelAreaEdit,
  saveAreaEdit,
  saveTitleEdit,
  loadChannelData,
  updateChannelList,
  updateQualityOptions,
  updateChannelInfo,
  getTechnicalQualityValue,
  populateQualityOptions,
  applyChannelChange,
  toggleYtCookiesEdit,
  cancelYtCookiesEdit,
  saveYtCookiesEdit,
  updateYtCookiesStatus,
  statusRefreshInterval,
  networkRefreshInterval,
  statusIntervalId,
  networkRefreshIntervalId,
  statusRefreshInFlight,
  statusRefreshQueued,
  networkRefreshInFlight,
  faceAuthUrl,
  holodexCurrentSource,
  holodexStreamsRequested,
  monitorToggleSaveDebounceMs,
  lastStatusRefreshMs,
  HOLODEX_STATUS_STATE_CLASSES,
  holodexDurationIntervalId,
  pendingSwitchData,
  holodexAuthState,
  holodexUseFavorites,
  holodexUseFavoritesInitialized,
};
