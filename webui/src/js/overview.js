// overview.js — extracted from app.js

import { isDashboardVisible, isElementHidden, setElementDisplay, reconcileChildren, createSvgIcon, parseInteger, setElementText, showNotification, setButtonLoading } from './dom.js';
import { state, applyDashboardCardVisibility, mergeConfigData, updateMonitorToggleStates, applyMonitorToggleConfigState, applyPriorityAutoRestartToggle, updateDanmakuCommandToggle, applyHolodexMonitorGateToggle, isViewActive, createAreaOption, createSelectOption, normalizeAreaData, getAreaList, getSortedAreas, appendAreaOptions, appendPlatformChannelOptions, getAreaName } from './state.js';
import { managementRequest, managementJsonRequest, getJson, postJsonApi } from './api.js';
import { eventStreamHealthy } from './events.js';
import { loadChannels } from './manage.js';
import { toggleDanmakuCommand } from './settings.js';
import { openCropConfig, clearCropConfig, loadCapturedCropFrame, isCropSessionCurrent } from './crop.js';
import { bindDialog, bindListboxKeyboard } from './dialog.js';
import { saveBooleanToggle } from './toggle-save.js';
import { createPriorityWarningControl } from './priority-warning.js';
import {
  QUALITY_MAPPINGS,
  formatHlsCacheStatus,
  getQualityDisplayText,
} from './format.js';
import { fallbackThumbnailUrl, placeholderKind, streamStartMs, toCardModel } from './stream-model.js';
import {
  buildHolodexAddChannelPayload,
  createHolodexSwitchPayload,
  hasConfiguredChannel,
  sameChannelCandidate,
  toAdminChannelCandidate,
  toAdminStreamAction,
} from './admin-stream-actions.js';
import { streamIsOnAir } from './on-air.js';
import {
  createScheduleDivider,
  createWatchLink,
  renderCard,
  startDurationTicker,
  stopDurationTicker,
  updateViewerCount,
  watchUrl,
} from './stream-card.js';
import {
  isBiliNetworkLive,
  renderBiliNetworkPanel,
  renderStatusCards,
  setNcChannelDisplay,
  setPlatformLiveInfoVisibility,
  setStatusCardsMessage,
} from './status-cards.js';
import {
  HOLODEX_STATUS_STATE_CLASSES,
  addHolodexChannelToCache,
  closeHolodexLoginModal,
  confirmHolodexAddChannel,
  createHolodexAddChannelControls,
  createHolodexChannelAddIcon,
  createHolodexChannelAddedIndicator,
  createHolodexStreamActionButton,
  createHolodexStreamSvg,
  createStreamCard,
  cropAndSwitchToHolodexStream,
  getHolodexChannelPhotoUrl,
  getHolodexChannelUrl,
  hideHolodexStatus,
  holodexKeepAliveCheckMs,
  holodexKeepAliveDue,
  holodexKeepAliveMs,
  holodexKeepAliveWithoutEventsMs,
  holodexState,
  holodexStreamHasConfiguredChannel,
  isHolodexPanelOpen,
  loadHolodexAuthStatus,
  markHolodexChannelAdded,
  openHolodexLogin,
  readHolodexAddChannelData,
  readHolodexStreamActionData,
  refreshHolodexChannelsData,
  saveHolodexApiKey,
  setHolodexAddChannelButtonsDisabled,
  setHolodexAddChannelConfirmVisible,
  setHolodexCollapsed,
  setHolodexStatus,
  startHolodexDurationTicker,
  stopHolodexDurationTicker,
  testHolodexApiKey,
  updateHolodexAuthBar,
  updateHolodexLoginButton,
  updateHolodexModalHint,
} from './holodex.js';

let statusRefreshInterval = 60000; // Default 60 seconds
const networkRefreshInterval = 1000;
let statusIntervalId = null;
let networkRefreshIntervalId = null;
let statusRefreshInFlight = false;
let statusRefreshQueued = false;
let networkRefreshInFlight = false;
let networkStatusGeneration = 0;
let faceAuthUrl = null;
const monitorToggleSaveDebounceMs = 160;
let youtubeWarningControl = null;
let twitchWarningControl = null;
let niconicoWarningControl = null;
let priorityWarningControl = null;
function bindWarningControl(platform, rowId, buttonId) {
  return createPriorityWarningControl({
    platform,
    row: document.getElementById(rowId),
    button: document.getElementById(buttonId),
    post: postJsonApi,
    refresh: refreshStatus,
    notify: showNotification,
  });
}
function renderPlatformWarningHolds(status) {
  youtubeWarningControl?.render(status?.youtube);
  twitchWarningControl?.render(status?.twitch);
  niconicoWarningControl?.render(status?.niconico);
  priorityWarningControl?.render(status?.priority_channel);
}
function initDashboardControls() {
  youtubeWarningControl = bindWarningControl('YT', 'yt-warning-row', 'release-yt-warning-btn');
  twitchWarningControl = bindWarningControl('TW', 'tw-warning-row', 'release-tw-warning-btn');
  niconicoWarningControl = bindWarningControl('NC', 'nc-warning-row', 'release-nc-warning-btn');
  priorityWarningControl = bindWarningControl(
    'priority',
    'priority-warning-row',
    'release-priority-warning-btn'
  );
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
    .getElementById('niconico-monitor-toggle')
    ?.addEventListener('change', toggleNiconicoMonitor);
  document
    .getElementById('refreshNiconicoBtn')
    ?.addEventListener('click', refreshNiconicoStatus);
  document
    .getElementById('refreshHolodexBtn')
    ?.addEventListener('click', () => refreshHolodexStreams({ force: true }));
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
  document
    .getElementById('nc-channel-edit-btn')
    ?.addEventListener('click', toggleNcChannelEdit);
  document
    .getElementById('nc-channel-save-btn')
    ?.addEventListener('click', saveNcChannelEdit);
  document
    .getElementById('nc-channel-cancel-btn')
    ?.addEventListener('click', cancelNcChannelEdit);
  document
    .getElementById('nc-area-edit-btn')
    ?.addEventListener('click', toggleNcAreaEdit);
  document
    .getElementById('nc-area-save-btn')
    ?.addEventListener('click', saveNcAreaEdit);
  document
    .getElementById('nc-area-cancel-btn')
    ?.addEventListener('click', cancelNcAreaEdit);
  document
    .getElementById('nc-quality-edit-btn')
    ?.addEventListener('click', toggleNcQualityEdit);
  document
    .getElementById('nc-quality-save-btn')
    ?.addEventListener('click', saveNcQualityEdit);
  document
    .getElementById('nc-quality-cancel-btn')
    ?.addEventListener('click', cancelNcQualityEdit);
  document
    .getElementById('nc-crop-edit-btn')
    ?.addEventListener('click', () => openCropConfig('niconico'));
  document
    .getElementById('nc-crop-clear-btn')
    ?.addEventListener('click', () => clearCropConfig('niconico'));
  document
    .getElementById('nc-hls-cache-edit-btn')
    ?.addEventListener('click', toggleNcHlsCacheEdit);
  document
    .getElementById('nc-hls-cache-save-btn')
    ?.addEventListener('click', saveNcHlsCacheEdit);
  document
    .getElementById('nc-hls-cache-cancel-btn')
    ?.addEventListener('click', cancelNcHlsCacheEdit);
  document
    .getElementById('nc-hls-cache-enabled')
    ?.addEventListener('change', event => setHlsCacheLatencyInputState('nc', event.currentTarget.checked));
  document
    .getElementById('priority-toggle')
    ?.addEventListener('change', togglePriorityChannel);
  document
    .getElementById('refreshPriorityBtn')
    ?.addEventListener('click', refreshPriorityStatus);
  document
    .getElementById('priority-channel-edit-btn')
    ?.addEventListener('click', togglePriorityChannelEdit);
  document
    .getElementById('priority-channel-save-btn')
    ?.addEventListener('click', savePriorityChannelEdit);
  document
    .getElementById('priority-channel-cancel-btn')
    ?.addEventListener('click', cancelPriorityChannelEdit);
  document
    .getElementById('priority-area-edit-btn')
    ?.addEventListener('click', togglePriorityAreaEdit);
  document
    .getElementById('priority-area-save-btn')
    ?.addEventListener('click', savePriorityAreaEdit);
  document
    .getElementById('priority-area-cancel-btn')
    ?.addEventListener('click', cancelPriorityAreaEdit);
  document
    .getElementById('priority-auto-restart-toggle')
    ?.addEventListener('change', togglePriorityAutoRestart);
}
function initHolodexLoginModalControls() {
  bindDialog('holodex-login-modal', closeHolodexLoginModal);
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
}
function initFaceAuthModalControls() {
  bindDialog('face-auth-modal', closeFaceAuthModal);
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

    applyHolodexMonitorGateToggle(config.holodex_monitor_gate !== false);
    applyHolodexConfig(config);

    applyPriorityAutoRestartToggle(config);

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

  if (holodexState.holodexKeepAliveId) {
    clearInterval(holodexState.holodexKeepAliveId);
  }
  holodexState.holodexKeepAliveId = setInterval(() => {
    if (holodexKeepAliveDue(Date.now() - holodexState.holodexLastFetchMs, eventStreamHealthy())) {
      maybeLoadHolodexStreams();
    }
  }, holodexKeepAliveCheckMs);

  // Initial refresh
  refreshStatus();
  refreshNetworkStatus();

  let titleRowCenterTimer;
  window.addEventListener('resize', () => {
    clearTimeout(titleRowCenterTimer);
    titleRowCenterTimer = setTimeout(schedulePlatformTitleRowCenters, 100);
  });

}
// The list shows with either key: Holodex rows, or what YouTube discovery
// finds. Favorites and login need the Holodex key.
function applyHolodexSectionVisibility(holodexKey, youtubeKey) {
  const listAvailable = holodexKey || youtubeKey;
  setElementDisplay(document.getElementById('holodex-section'), true);
  setElementDisplay(document.getElementById('holodex-init-hint'), false);
  setElementDisplay(document.getElementById('holodex-api-config'), !listAvailable);
  setElementDisplay(document.getElementById('holodex-streams-section'), listAvailable);
  setElementDisplay(document.getElementById('holodex-toolbar'), holodexKey, 'flex');
  // Auth status owns the login button. A later config reload must not show it
  // again while the favorites bar is already in use.
  updateHolodexAuthBar();
  maybeLoadHolodexStreams();
}
// Applies the panel's key-driven layout from a loaded config. The auth status
// may renew the JWT upstream, so it loads only when the Holodex key appears.
function applyHolodexConfig(config) {
  const holodexKey = !!config.holodex_api_key_configured || !!config.holodex_api_key?.trim();
  const youtubeKey = !!config.youtube_api_key_configured || !!config.youtube_api_key?.trim();
  const holodexKeyAdded = holodexKey && !holodexState.holodexKeyConfigured;
  holodexState.holodexKeyConfigured = holodexKey;
  if (!holodexKey) {
    holodexState.holodexUseFavorites = false;
    holodexState.holodexUseFavoritesInitialized = false;
  }
  applyHolodexSectionVisibility(holodexKey, youtubeKey);
  if (holodexKeyAdded) {
    // The first fetch went out in channels mode; follow a login to favorites.
    const usedFavorites = holodexState.holodexUseFavorites;
    loadHolodexAuthStatus().then(() => {
      if (holodexState.holodexUseFavorites !== usedFavorites) maybeLoadHolodexStreams();
    });
  }
}
// Every trigger (SSE, opening the panel, the tab coming back, the keep-alive)
// refetches only while the panel is open; a closed one catches up on opening.
function maybeLoadHolodexStreams() {
  if (isHolodexPanelOpen()) {
    refreshHolodexStreams();
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
    // Opening the panel catches up on what changed while it was closed.
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
function setOnAir(next) {
  const normalized = next && (next.platform || next.channel_name || next.title)
    ? {
      platform: next.platform || '',
      channel_name: next.channel_name || '',
      title: next.title || '',
    }
    : null;
  if (JSON.stringify(holodexState.holodexOnAir) === JSON.stringify(normalized)) return;
  holodexState.holodexOnAir = normalized;
  const container = document.getElementById('holodex-streams');
  if (container && (holodexState.lastHolodexLive.length || holodexState.lastHolodexScheduled.length)) {
    renderHolodexCards(container, holodexState.lastHolodexLive, holodexState.lastHolodexScheduled);
  }
}

function renderHolodexCards(container, live, scheduled) {
  holodexState.lastHolodexLive = live;
  holodexState.lastHolodexScheduled = scheduled;
  const next = new Map();
  const occurrences = new Map();
  const elements = [];
  const append = (stream) => {
    const baseKey = `${stream.id}:${stream.link}`;
    const occurrence = occurrences.get(baseKey) || 0;
    occurrences.set(baseKey, occurrence + 1);
    const key = `${baseKey}:${occurrence}`;
    const { liveViewers, ...content } = stream;
    const configured = holodexState.holodexCurrentSource === 'favorites' && holodexStreamHasConfiguredChannel(stream);
    const onAirFlag = streamIsOnAir(stream, holodexState.holodexOnAir) ? '1' : '0';
    const signature = JSON.stringify([content, holodexState.holodexCurrentSource, configured, onAirFlag]);
    const previous = holodexState.renderedHolodexCards.get(key);
    const element = previous?.signature === signature ? previous.element : createStreamCard(stream);
    updateViewerCount(element, liveViewers);
    next.set(key, { signature, element });
    elements.push(element);
  };
  live.forEach(stream => append(stream));
  if (live.length && scheduled.length) {
    holodexState.holodexScheduleDivider ??= createScheduleDivider();
    elements.push(holodexState.holodexScheduleDivider);
  }
  scheduled.forEach(stream => append(stream));
  reconcileChildren(container, elements);
  holodexState.renderedHolodexCards = next;
}

// `force`: the refresh button; the server asks Holodex now instead of
// answering from its list.
async function refreshHolodexStreams({ force = false } = {}) {
  const generation = ++holodexState.holodexRefreshGeneration;
  holodexState.holodexLastFetchMs = Date.now();
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

  // Refreshing an existing list should not insert a banner above the cards.
  streamsDiv.setAttribute('aria-busy', 'true');
  if (!streamsDiv.childElementCount) {
    setHolodexStatus(statusDiv, '⏳ 加载中...', 'holodex-status-loading');
  }

  try {
    const favoritesParam = holodexState.holodexUseFavorites ? 'true' : 'false';
    const forceParam = force ? '&force=true' : '';
    const data = await getJson(`/api/holodex/streams?favorites=${favoritesParam}${forceParam}`);
    if (generation !== holodexState.holodexRefreshGeneration) return;

    if (!data.success) {
      setHolodexStatus(statusDiv, `⚠️ ${data.message}`, 'holodex-status-warning');
      return;
    }

    const streams = (data.data || []).map(toCardModel);
    const isFavorites = data.source === 'favorites';
    holodexState.holodexCurrentSource = data.source || (isFavorites ? 'favorites' : 'channels');

    if (isFavorites) {
      try {
        await refreshHolodexChannelsData();
        if (generation !== holodexState.holodexRefreshGeneration) return;
      } catch (error) {
        if (generation !== holodexState.holodexRefreshGeneration) return;
        console.error('Failed to load channels.json for Holodex add controls:', error);
        holodexState.holodexCurrentSource = 'channels';
        showNotification('加载 channels.json 失败，无法显示添加按钮', 'error');
      }
    }

    // Separate live and scheduled streams
    const liveStreams = streams.filter(s => s.status === 'live');
    const scheduledStreams = streams.filter(s => s.status !== 'live');
    if (generation !== holodexState.holodexRefreshGeneration) return;
    stopHolodexDurationTicker();

    if (streams.length === 0) {
      streamsDiv.replaceChildren();
      holodexState.renderedHolodexCards.clear();
      holodexState.lastHolodexLive = [];
      holodexState.lastHolodexScheduled = [];
      const emptyMessage = isFavorites
        ? '✅ 收藏夹 - 当前无直播或预告'
        : '当前无直播或预告';
      setHolodexStatus(statusDiv, emptyMessage, 'holodex-status-success');
      return;
    }

    // Sort scheduled streams by time - nearest first
    scheduledStreams.sort((a, b) => {
      const timeA = streamStartMs(a, false) ?? Infinity;
      const timeB = streamStartMs(b, false) ?? Infinity;
      return timeA - timeB;
    });

    hideHolodexStatus(statusDiv);

    renderHolodexCards(streamsDiv, liveStreams, scheduledStreams);
    startHolodexDurationTicker();

  } catch (error) {
    if (generation !== holodexState.holodexRefreshGeneration) return;
    setHolodexStatus(statusDiv, `❌ 请求失败: ${error.message}`, 'holodex-status-error');
  } finally {
    // Stop spinning animation when complete
    if (generation === holodexState.holodexRefreshGeneration) {
      setButtonLoading(button, icon, false);
      streamsDiv.setAttribute('aria-busy', 'false');
    }
  }
}
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
  bindDialog('area-modal', closeAreaModal);
  bindListboxKeyboard(document.getElementById('modal-area-list'));
  document
    .getElementById('confirm-area-selection-btn')
    ?.addEventListener('click', confirmAreaSelection);
  document
    .getElementById('cancel-area-selection-btn')
    ?.addEventListener('click', closeAreaModal);
}
function showFaceAuthModal(qrUrl, qrImage) {
  faceAuthUrl = qrUrl;
  const modal = document.getElementById('face-auth-modal');
  const container = document.getElementById('face-auth-qr-container');
  if (!modal || !container) return;

  // The backend renders the QR code locally; verification URLs stay private.
  container.replaceChildren();

  if (!qrImage?.startsWith('data:image/svg+xml;base64,')) {
    container.appendChild(createFaceAuthQrFallback(qrUrl));
    modal.classList.add('active');
    return;
  }

  const qrImg = document.createElement('img');
  qrImg.className = 'face-auth-qr-image';
  qrImg.src = qrImage;
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
async function performSwitch(channelId, areaId, title, topicId, status, platform, twitchChannelId, externalLink) {
  try {
    const payload = createHolodexSwitchPayload({
      channelId,
      title,
      topicId,
      status,
      platform,
      twitchId: twitchChannelId,
      externalLink
    }, areaId);
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

async function togglePriorityChannel() {
  return saveBooleanToggle(
    'priority-toggle', !!window.configData.priority_channel?.enabled,
    enabled => postJsonApi('/api/priority-channel', { enabled }),
    enabled => mergeConfigData({ priority_channel: { enabled } })
  );
}

async function togglePriorityAutoRestart() {
  return saveBooleanToggle(
    'priority-auto-restart-toggle', !!window.configData.priority_channel?.auto_restart,
    auto_restart => postJsonApi('/api/priority-channel', { auto_restart }),
    auto_restart => mergeConfigData({ priority_channel: { auto_restart } })
  );
}

let priorityChannelEditOriginal = null;
let priorityAreaEditOriginal = null;
async function togglePriorityChannelEdit() {
  priorityChannelEditOriginal = window.configData.priority_channel?.channel_name ?? null;
  const channelSpan = document.getElementById('priority-channel-name');
  const editContainer = document.getElementById('priority-channel-edit-container');
  const editSelect = document.getElementById('priority-channel-edit-select');

  showInfoRowEdit(channelSpan, editContainer);

  await loadChannelsForPriorityInline();
  editSelect.focus();
}

async function loadChannelsForPriorityInline() {
  try {
    const data = await getJson('/api/channels');
    const select = document.getElementById('priority-channel-edit-select');

    select.replaceChildren(createSelectOption('', '选择频道...'));

    if (data.channels) {
      data.channels.forEach(channel => {
        select.appendChild(createSelectOption(channel.name, channel.name));
      });

      const currentChannel = document.getElementById('priority-channel-name').textContent;
      if (currentChannel && currentChannel !== '未配置') {
        select.value = currentChannel;
      }
    }
  } catch (error) {
    console.error('Failed to load channels:', error);
  }
}

function cancelPriorityChannelEdit() {
  priorityChannelEditOriginal = null;
  const channelSpan = document.getElementById('priority-channel-name');
  const editContainer = document.getElementById('priority-channel-edit-container');

  hideInfoRowEdit(channelSpan, editContainer);
}

async function savePriorityChannelEdit() {
  const editSelect = document.getElementById('priority-channel-edit-select');
  const newChannel = editSelect.value;

  if (!newChannel || priorityChannelEditOriginal === null) {
    showNotification('请选择一个频道', 'error');
    return;
  }

  try {
    const priorityConfig = {
      channel_name: newChannel,
      expected: { channel_name: priorityChannelEditOriginal },
    };

    const result = await postJsonApi('/api/priority-channel', priorityConfig);
    if (result.success) {
      showNotification('频道配置已保存', 'success');
      cancelPriorityChannelEdit();
      await refreshStatus();
    } else {
      showNotification(result.message || '保存失败', 'error');
    }
  } catch (error) {
    console.error('Failed to save priority channel:', error);
    showNotification('保存失败: ' + error.message, 'error');
  }
}

async function togglePriorityAreaEdit() {
  priorityAreaEditOriginal = window.configData.priority_channel?.default_area ?? null;
  const areaSpan = document.getElementById('priority-default-area');
  const editContainer = document.getElementById('priority-area-edit-container');
  const editSelect = document.getElementById('priority-area-edit-select');

  showInfoRowEdit(areaSpan, editContainer);

  await loadAreasForPriorityInline();
  editSelect.focus();
}

async function loadAreasForPriorityInline() {
  try {
    const data = await getJson('/api/areas');
    const select = document.getElementById('priority-area-edit-select');

    select.replaceChildren(createAreaOption('', '选择分区...'));

    if (data.areas) {
      getSortedAreas(data.areas).forEach(area => {
        select.appendChild(createAreaOption(area.id, area.name));
      });

      const currentAreaText = document.getElementById('priority-default-area').textContent;
      if (currentAreaText && currentAreaText !== '-') {
        const match = currentAreaText.match(/\((\d+)\)/);
        if (match) {
          select.value = match[1];
        }
      }
    }
  } catch (error) {
    console.error('Failed to load areas:', error);
  }
}

function cancelPriorityAreaEdit() {
  priorityAreaEditOriginal = null;
  const areaSpan = document.getElementById('priority-default-area');
  const editContainer = document.getElementById('priority-area-edit-container');

  hideInfoRowEdit(areaSpan, editContainer);
}

async function savePriorityAreaEdit() {
  const editSelect = document.getElementById('priority-area-edit-select');
  const newAreaId = parseInteger(editSelect.value, 0);

  if (!newAreaId || priorityAreaEditOriginal === null) {
    showNotification('请选择一个分区', 'error');
    return;
  }

  try {
    const priorityConfig = {
      default_area: newAreaId,
      expected: { default_area: priorityAreaEditOriginal },
    };

    const result = await postJsonApi('/api/priority-channel', priorityConfig);
    if (result.success) {
      showNotification('默认分区已保存', 'success');
      cancelPriorityAreaEdit();
      await refreshStatus();
    } else {
      showNotification(result.message || '保存失败', 'error');
    }
  } catch (error) {
    console.error('Failed to save priority area:', error);
    showNotification('保存失败: ' + error.message, 'error');
  }
}

async function toggleHolodexFavoritesMode() {
  const toggle = document.getElementById('holodex-use-favorites-toggle');
  if (!holodexState.holodexAuthState.logged_in || holodexState.holodexAuthState.expired) {
    if (toggle) toggle.checked = false;
    openHolodexLoginModal();
    return;
  }

  holodexState.holodexUseFavorites = toggle ? toggle.checked : !holodexState.holodexUseFavorites;
  holodexState.holodexUseFavoritesInitialized = true;
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
async function toggleHolodexSkipJwtVerify() {
  const toggle = document.getElementById('holodex-skip-jwt-verify');
  if (!toggle) return;

  try {
    const data = await postJsonApi('/api/config', { holodex_skip_jwt_verify: toggle.checked });
    if (data.success) {
      holodexState.holodexAuthState.skip_jwt_verify = toggle.checked;
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
      holodexState.holodexUseFavorites = true;
      holodexState.holodexUseFavoritesInitialized = true;
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
      holodexState.holodexUseFavorites = false;
      holodexState.holodexUseFavoritesInitialized = false;
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
function toggleNcHlsCacheEdit() {
  toggleHlsCacheEdit('nc', 'niconico');
}
function cancelNcHlsCacheEdit() {
  cancelHlsCacheEdit('nc');
}
function saveNcHlsCacheEdit() {
  return saveHlsCacheEdit('nc', 'niconico', 'Niconico');
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
function platformMonitorEnabledFromConfig(platform) {
  const enabled = window.configData[platform]?.enable_monitor;
  if (platform === 'niconico') {
    return enabled === true;
  }
  return enabled !== false;
}
function getMonitorToggleSaveState(platform, toggleId) {
  let toggleState = state.monitorToggleSaveState.get(toggleId);
  if (!toggleState) {
    const confirmed = platformMonitorEnabledFromConfig(platform);
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
function toggleNiconicoMonitor() {
  return togglePlatformMonitor('niconico', 'niconico-monitor-toggle', '/api/toggle-niconico-monitor');
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
function refreshNiconicoStatus() {
  return refreshPlatformStatus('Niconico', '/api/refresh/niconico', 'refreshNiconicoBtn', 'refreshNiconicoIcon');
}

function refreshPriorityStatus() {
  return refreshDashboardStatusEndpoint({
    endpoint: '/api/refresh/priority-channel',
    buttonId: 'refreshPriorityBtn',
    iconId: 'refreshPriorityIcon',
    successMessage: '优先频道状态已刷新',
    failureMessage: '刷新优先频道状态失败',
    defaultError: 'Failed to refresh priority channel status',
    logMessage: 'Error refreshing priority channel status:'
  });
}

function setPriorityStatusFields(priority, liveStatus, title, defaultArea) {
  const status = document.getElementById('priority-status');
  if (status) {
    status.className = `status-indicator ${priority?.is_live && priority.enabled ? 'status-live' : 'status-offline'}`;
  }
  setPlatformLiveInfoVisibility('priority', !!priority?.is_live && priority.enabled);
  setElementText('priority-channel-name', priority?.channel_name || '未配置');
  setElementText('priority-live-status', liveStatus);
  setElementText('priority-title', title);
  setElementText('priority-default-area', defaultArea);
  updatePriorityToggleAvailability();
}

function updatePriorityToggleAvailability() {
  const toggle = document.getElementById('priority-toggle');
  if (!toggle) return;

  const blocked = !state.localNodeCanEnableMonitorToggles && !toggle.checked;
  toggle.disabled = blocked;

  const label = toggle.closest('.toggle-label');
  if (label) {
    label.title = blocked ? '只有活跃节点可启用优先频道' : '启用优先频道监控';
  }
}

function renderPriorityChannelStatus(priority) {
  priorityWarningControl?.render(priority);
  const priorityCard = document.getElementById('priority-channel-card');
  const priorityToggle = document.getElementById('priority-toggle');
  const priorityAutoRestartToggle = document.getElementById('priority-auto-restart-toggle');
  if (!priorityCard || !priorityToggle) {
    return;
  }

  applyDashboardCardVisibility();

  if (!priority) {
    applyMonitorToggleConfigState(priorityToggle, 'priority-toggle', false);
    if (priorityAutoRestartToggle) {
      applyMonitorToggleConfigState(priorityAutoRestartToggle, 'priority-auto-restart-toggle', false);
    }
    setPriorityStatusFields(null, '未启用', '-', '-');
    return;
  }

  applyMonitorToggleConfigState(priorityToggle, 'priority-toggle', !!priority.enabled);
  if (priorityAutoRestartToggle) {
    applyMonitorToggleConfigState(priorityAutoRestartToggle, 'priority-auto-restart-toggle', !!priority.auto_restart);
  }
  window.configData.priority_channel = {
    ...(window.configData.priority_channel || {}),
    enabled: priority.enabled,
    auto_restart: priority.auto_restart || false,
    channel_name: priority.channel_name || '',
    default_area: priority.default_area || 235
  };

  const areaName = priority.default_area_name || getAreaName(priority.default_area || 235);
  const defaultArea = priority.default_area
    ? (String(areaName) === String(priority.default_area) ? `分区 ${priority.default_area}` : `${areaName} (${priority.default_area})`)
    : '-';
  if (priority.enabled) {
    setPriorityStatusFields(
      priority,
      priority.is_live ? '直播中' : '未直播',
      priority.title || '-',
      defaultArea
    );
    return;
  }

  setPriorityStatusFields(priority, '未启用', '-', defaultArea);
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
  syncInfoRowLabelCenter(
    document.getElementById('priority-title-row'),
    document.getElementById('priority-title')
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
  if (!isDashboardVisible() || !isViewActive('overview')
    || networkRefreshInFlight || statusRefreshInFlight || !isBiliNetworkLive()) {
    return;
  }

  networkRefreshInFlight = true;
  const generation = networkStatusGeneration;
  try {
    const result = await getJson('/api/network-status');
    if (result.success && result.data && generation === networkStatusGeneration
      && isDashboardVisible() && isViewActive('overview')) {
      renderBiliNetworkPanel(result.data);
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
  networkStatusGeneration += 1;
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
      renderPlatformWarningHolds(data.data);
      renderPriorityChannelStatus(data.data.priority_channel);
    } else {
      console.error('Invalid API response:', data);
      showNotification('获取状态失败：响应格式错误', 'error');
      setStatusCardsMessage('数据错误');
    }
  } catch (error) {
    console.error('Failed to refresh status:', error);

    setStatusCardsMessage('连接中断，等待重连');
    renderPlatformWarningHolds(null);
    setPriorityStatusFields(null, '连接中断', '-', '-');
    setElementText('priority-channel-name', '连接中断');

    // Avoid repeated notifications while the connection is unavailable.
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
      showFaceAuthModal(data.data.qr_url, data.data.qr_image);
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
async function toggleNcChannelEdit() {
  const channelSpan = document.getElementById('nc-channel-name');
  const editContainer = document.getElementById('nc-channel-edit-container');
  const editSelect = document.getElementById('nc-channel-edit-select');

  showInfoRowEdit(channelSpan, editContainer);

  try {
    state.channelsData = await getJson('/api/channels');
  } catch (error) {
    showNotification('加载频道列表失败: ' + error.message, 'error');
    return;
  }

  populateNcChannelEditSelect();
  editSelect.focus();
}
function populateNcChannelEditSelect() {
  const editSelect = document.getElementById('nc-channel-edit-select');
  editSelect.replaceChildren(createSelectOption('', '选择频道...'));
  appendPlatformChannelOptions(editSelect, 'niconico');

  const channelSpan = document.getElementById('nc-channel-name');
  const currentId = (channelSpan?.dataset.channelId || '').trim();
  const currentName = (channelSpan?.textContent || '').trim();
  for (const option of editSelect.options) {
    if (!option.value) continue;
    try {
      const info = JSON.parse(option.value);
      if (
        (currentId && currentId !== '-' && info.id === currentId)
        || (currentName && currentName !== '-' && info.name === currentName)
      ) {
        editSelect.value = option.value;
        break;
      }
    } catch (_error) {
      // Skip malformed option values from older cached channel lists.
    }
  }

  if (editSelect.options.length <= 1) {
    showNotification('没有可选择的 Niconico 频道。请先在频道配置中填写 Niconico 频道ID 和转播标题。', 'error');
  }
}
function cancelNcChannelEdit() {
  const channelSpan = document.getElementById('nc-channel-name');
  const editContainer = document.getElementById('nc-channel-edit-container');

  hideInfoRowEdit(channelSpan, editContainer);
}
async function saveNcChannelEdit() {
  const editSelect = document.getElementById('nc-channel-edit-select');
  const selectedValue = editSelect.value;

  if (!selectedValue) {
    showNotification('请选择频道', 'error');
    return;
  }

  try {
    const channelInfo = JSON.parse(selectedValue);
    const data = await postChannelUpdate({
      platform: 'niconico',
      channel_id: channelInfo.id,
      channel_name: channelInfo.name
    });

    if (!data.success) {
      showNotification(data.message || 'Niconico频道更新失败', 'error');
      return;
    }

    showNotification(data.message || 'Niconico频道已更新', 'success');
    cancelNcChannelEdit();
    setNcChannelDisplay(channelInfo.name, channelInfo.id);
    refreshStatus();
  } catch (error) {
    showNotification('更新失败: ' + error.message, 'error');
  }
}
function toggleNcQualityEdit() {
  const qualitySpan = document.getElementById('nc-quality');
  const editContainer = document.getElementById('nc-quality-edit-container');
  const editSelect = document.getElementById('nc-quality-edit-select');

  showInfoRowEdit(qualitySpan, editContainer);
  populateQualityOptions(editSelect, 'niconico');

  const currentDisplayText = qualitySpan.textContent;
  if (currentDisplayText && currentDisplayText !== '-') {
    editSelect.value = getTechnicalQualityValue(currentDisplayText, 'niconico');
  }

  editSelect.focus();
}
function cancelNcQualityEdit() {
  hideInfoRowEdit(
    document.getElementById('nc-quality'),
    document.getElementById('nc-quality-edit-container')
  );
}
async function saveNcQualityEdit() {
  const quality = document.getElementById('nc-quality-edit-select').value;

  if (!quality) {
    showNotification('请选择画质', 'error');
    return;
  }

  try {
    const data = await postChannelUpdate({
      platform: 'niconico',
      quality
    });

    if (!data.success) {
      showNotification(data.message || 'Niconico画质更新失败', 'error');
      return;
    }

    showNotification(data.message || 'Niconico画质已更新', 'success');
    cancelNcQualityEdit();
    document.getElementById('nc-quality').textContent = getQualityDisplayText(quality, 'niconico');
    refreshStatus();
  } catch (error) {
    showNotification('更新失败: ' + error.message, 'error');
  }
}
function toggleNcAreaEdit() {
  const areaSpan = document.getElementById('nc-area');
  const editContainer = document.getElementById('nc-area-edit-container');
  const editSelect = document.getElementById('nc-area-edit-select');

  showInfoRowEdit(areaSpan, editContainer);

  if (!state.areasData) {
    loadChannelData().then(() => {
      populateNcAreaEditSelect();
    });
  } else {
    populateNcAreaEditSelect();
  }

  editSelect.focus();
}
function populateNcAreaEditSelect() {
  const editSelect = document.getElementById('nc-area-edit-select');
  editSelect.replaceChildren(createAreaOption('', '选择分区...'));

  if (state.areasData && state.areasData.areas) {
    appendAreaOptions(editSelect, state.areasData.areas);
  }
}
function cancelNcAreaEdit() {
  hideInfoRowEdit(
    document.getElementById('nc-area'),
    document.getElementById('nc-area-edit-container')
  );
}
async function saveNcAreaEdit() {
  const areaId = parseInteger(document.getElementById('nc-area-edit-select').value, 0);

  if (!areaId) {
    showNotification('请选择分区', 'error');
    return;
  }

  try {
    const data = await postChannelUpdate({
      platform: 'niconico',
      area_id: areaId
    });

    if (!data.success) {
      showNotification(data.message || 'Niconico配置分区更新失败', 'error');
      return;
    }

    showNotification(data.message || 'Niconico配置分区已更新', 'success');
    cancelNcAreaEdit();
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
  const generation = state.managedDataGeneration;
  // Start continuous spinning animation
  const icon = document.getElementById('loadChannelIcon');
  setButtonLoading(null, icon, true);

  try {
    const [channelsResult, areasResult] = await Promise.all([
      getJson('/api/channels'),
      getJson('/api/areas')
    ]);
    if (generation !== state.managedDataGeneration) return loadChannelData();
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
    },
    niconico: {
      '最佳质量': 'best',
      '1080p60': '1080p60',
      '720p60': '720p60',
      '450p': '450p',
      '288p': '288p'
    }
  };

  return reverseMappings[platform]?.[displayText] || displayText;
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

  // Add options for the specified platform; the labels are the shared table so
  // the picker and the channel card cannot drift apart.
  const platformQualities = QUALITY_MAPPINGS[platform];
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
  applyHolodexConfig,
  isHolodexPanelOpen,
  maybeLoadHolodexStreams,
  holodexKeepAliveDue,
  setHolodexCollapsed,
  toggleHolodexFold,
  initHolodexFold,
  setHolodexStatus,
  hideHolodexStatus,
  refreshHolodexStreams,
  createHolodexStreamSvg,
  stopHolodexDurationTicker,
  startHolodexDurationTicker,
  setOnAir,
  getHolodexChannelUrl,
  getHolodexChannelPhotoUrl,
  refreshHolodexChannelsData,
  holodexStreamHasConfiguredChannel,
  createHolodexChannelAddIcon,
  createHolodexAddChannelControls,
  createHolodexStreamActionButton,
  readHolodexStreamActionData,
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
  updatePriorityToggleAvailability,
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
  monitorToggleSaveDebounceMs,
  lastStatusRefreshMs,
  HOLODEX_STATUS_STATE_CLASSES,
  pendingSwitchData,
};
