// overview.js — extracted from app.js

import { isDashboardVisible, parseInteger, showNotification, setButtonLoading } from './dom.js';
import { state, mergeConfigData, updateMonitorToggleStates, applyPriorityAutoRestartToggle, updateDanmakuCommandToggle, applyHolodexMonitorGateToggle, createAreaOption, createSelectOption, normalizeAreaData, getAreaList, getSortedAreas, appendAreaOptions, appendPlatformChannelOptions } from './state.js';
import { getJson, postJsonApi } from './api.js';
import { eventStreamHealthy } from './events.js';

import { toggleDanmakuCommand } from './settings.js';
import {
  cancelNcHlsCacheEdit,
  cancelTwHlsCacheEdit,
  cancelYtHlsCacheEdit,
  hideInfoRowEdit,
  saveNcHlsCacheEdit,
  saveTwHlsCacheEdit,
  saveYtHlsCacheEdit,
  setHlsCacheLatencyInputState,
  showInfoRowEdit,
  toggleNcHlsCacheEdit,
  toggleTwHlsCacheEdit,
  toggleYtHlsCacheEdit,
} from './inline-edit.js';
import { switchToStream } from './switch-flow.js';
import { openCropConfig, clearCropConfig } from './crop.js';
import { bindDialog } from './dialog.js';
import { saveBooleanToggle } from './toggle-save.js';
import { initWarningControls, msSinceLastStatusRefresh, refreshNetworkStatus, refreshStatus, schedulePlatformTitleRowCenters } from './status-refresh.js';
import { QUALITY_MAPPINGS, formatHlsCacheStatus, getQualityDisplayText } from './format.js';

import { setNcChannelDisplay } from './status-cards.js';
import { STREAM_LIST_STATUS_STATE_CLASSES, addHolodexChannelToCache, applyStreamListConfig, applyStreamListSectionVisibility, closeHolodexLoginModal, confirmHolodexAddChannel, createHolodexAddChannelControls, createHolodexChannelAddIcon, createHolodexChannelAddedIndicator, createStreamActionButton, createStreamSvg, createStreamCard, cropAndSwitchToStream, getHolodexChannelPhotoUrl, getHolodexChannelUrl, hideStreamListStatus, streamListKeepAliveCheckMs, streamListKeepAliveDue, streamListState, holodexStreamHasConfiguredChannel, initStreamListFold, initHolodexLoginModalControls, isStreamListPanelOpen, loadHolodexAuthStatus, logoutHolodexJwt, markHolodexChannelAdded, maybeLoadStreamList, openHolodexLogin, openHolodexLoginModal, readHolodexAddChannelData, readStreamActionData, refreshChannelsData, refreshStreamList, renderStreamListCards, saveHolodexApiKey, saveHolodexJwt, setHolodexAddChannelButtonsDisabled, setHolodexAddChannelConfirmVisible, setStreamListCollapsed, setStreamListStatus, startStreamListDurationTicker, stopStreamListDurationTicker, testHolodexApiKey, toggleHolodexFavoritesMode, toggleStreamListFold, toggleHolodexSkipJwtVerify, updateHolodexAuthBar, updateHolodexLoginButton, updateHolodexModalHint } from './stream-list.js';

let statusRefreshInterval = 60000; // Default 60 seconds
const networkRefreshInterval = 1000;
let statusIntervalId = null;
let networkRefreshIntervalId = null;
let faceAuthUrl = null;
const monitorToggleSaveDebounceMs = 160;
function initDashboardControls() {
  initWarningControls();
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
    .getElementById('refreshStreamsBtn')
    ?.addEventListener('click', () => refreshStreamList({ force: true }));
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
// Load config and set up status refresh interval
async function initStatusRefresh() {
  try {
    const config = await getJson('/api/config');

    // Store config globally and apply config-driven controls once.
    mergeConfigData(config);
    updateMonitorToggleStates(config);
    updateDanmakuCommandToggle(config.bilibili?.enable_danmaku_command !== false);

    applyHolodexMonitorGateToggle(config.holodex_monitor_gate !== false);
    applyStreamListConfig(config);

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
      && msSinceLastStatusRefresh() < statusRefreshInterval * 5) {
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

  if (streamListState.keepAliveId) {
    clearInterval(streamListState.keepAliveId);
  }
  streamListState.keepAliveId = setInterval(() => {
    if (streamListKeepAliveDue(Date.now() - streamListState.lastFetchMs, eventStreamHealthy())) {
      maybeLoadStreamList();
    }
  }, streamListKeepAliveCheckMs);

  // Initial refresh
  refreshStatus();
  refreshNetworkStatus();

  let titleRowCenterTimer;
  window.addEventListener('resize', () => {
    clearTimeout(titleRowCenterTimer);
    titleRowCenterTimer = setTimeout(schedulePlatformTitleRowCenters, 100);
  });

}
function setOnAir(next) {
  const normalized = next && (next.platform || next.channel_name || next.title)
    ? {
      platform: next.platform || '',
      channel_name: next.channel_name || '',
      title: next.title || '',
    }
    : null;
  if (JSON.stringify(streamListState.onAir) === JSON.stringify(normalized)) return;
  streamListState.onAir = normalized;
  const container = document.getElementById('streams-list');
  if (container && (streamListState.lastLive.length || streamListState.lastScheduled.length)) {
    renderStreamListCards(container, streamListState.lastLive, streamListState.lastScheduled);
  }
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

    select.replaceChildren(createSelectOption('', '选择频道…'));

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
    showNotification('保存失败：' + error.message, 'error');
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

    select.replaceChildren(createAreaOption('', '选择分区…'));

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
    showNotification('保存失败：' + error.message, 'error');
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
    successMessage: 'B站状态已刷新',
    failureMessage: '刷新失败',
    defaultError: '服务器未响应',
    logMessage: '刷新 B站状态出错：'
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
      showNotification('保存失败：' + error.message, 'error');
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
      showNotification(`${label} 状态已刷新`, 'success');
      await refreshStatus();
    } else {
      showNotification(data.message || `刷新 ${label} 状态失败`, 'error');
    }
  } catch (error) {
    console.error(`Error refreshing ${label} status:`, error);
    showNotification(`刷新 ${label} 状态失败：${error.message}`, 'error');
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
    failureMessage: '刷新失败',
    defaultError: '服务器未响应',
    logMessage: '刷新优先频道状态出错：'
  });
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
    showNotification('操作失败：' + error.message, 'error');
  }
}
async function stopStream() {
  try {
    const data = await postJsonApi('/api/stop');
    showNotification(data.message || (data.success ? '直播已停止' : '停播失败'), data.success ? 'success' : 'error');
    setTimeout(refreshStatus, 2000);
  } catch (error) {
    showNotification('操作失败：' + error.message, 'error');
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
    showNotification('操作失败：' + error.message, 'error');
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
    showNotification('更新失败：' + error.message, 'error');
  }
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
  editSelect.replaceChildren(createSelectOption('', '选择频道…'));
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
    showNotification('更新失败：' + error.message, 'error');
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
  editSelect.replaceChildren(createAreaOption('', '选择分区…'));

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
    showNotification('更新失败：' + error.message, 'error');
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
    showNotification('更新失败：' + error.message, 'error');
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
    showNotification('更新失败：' + error.message, 'error');
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
  editSelect.replaceChildren(createSelectOption('', '选择频道…'));
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
    showNotification('更新失败：' + error.message, 'error');
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
  editSelect.replaceChildren(createAreaOption('', '选择分区…'));

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
    showNotification('更新失败：' + error.message, 'error');
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
    showNotification('加载频道列表失败：' + error.message, 'error');
    return;
  }

  populateNcChannelEditSelect();
  editSelect.focus();
}
function populateNcChannelEditSelect() {
  const editSelect = document.getElementById('nc-channel-edit-select');
  editSelect.replaceChildren(createSelectOption('', '选择频道…'));
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
    showNotification('更新失败：' + error.message, 'error');
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
    showNotification('更新失败：' + error.message, 'error');
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
  editSelect.replaceChildren(createAreaOption('', '选择分区…'));

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
    showNotification('更新失败：' + error.message, 'error');
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
  editSelect.replaceChildren(createAreaOption('', '选择分区…'));
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
    showNotification('更新失败：' + error.message, 'error');
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
    showNotification('更新失败：' + error.message, 'error');
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

    if (getAreaList().length === 0) {
      console.error('No areas data found:', state.areasData);
      showNotification('未找到分区数据', 'error');
    }
  } catch (error) {
    console.error('Failed to load channel data:', error);
    showNotification('加载频道数据失败：' + error.message, 'error');
  } finally {
    // Stop spinning animation when complete
    setButtonLoading(null, icon, false);
  }
}
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
    selectElement.appendChild(createSelectOption('', '选择画质…'));
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
    const action = readStreamActionData(switchButton);
    switchToStream(
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
    const action = readStreamActionData(cropSwitchButton);
    cropAndSwitchToStream(
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
    showNotification('更新失败：' + error.message, 'error');
  }
}
function updateYtCookiesStatus(browser, file) {
  const statusSpan = document.getElementById('yt-cookies-status');
  if (browser) {
    statusSpan.textContent = `浏览器：${browser}`;
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
  applyStreamListSectionVisibility,
  applyStreamListConfig,
  isStreamListPanelOpen,
  maybeLoadStreamList,
  streamListKeepAliveDue,
  setStreamListCollapsed,
  toggleStreamListFold,
  initStreamListFold,
  setStreamListStatus,
  hideStreamListStatus,
  refreshStreamList,
  createStreamSvg,
  stopStreamListDurationTicker,
  startStreamListDurationTicker,
  setOnAir,
  getHolodexChannelUrl,
  getHolodexChannelPhotoUrl,
  refreshChannelsData,
  holodexStreamHasConfiguredChannel,
  createHolodexChannelAddIcon,
  createHolodexAddChannelControls,
  createStreamActionButton,
  readStreamActionData,
  createStreamCard,
  showFaceAuthModal,
  createFaceAuthQrFallback,
  closeFaceAuthModal,
  retryStartStream,
  cropAndSwitchToStream,
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
  startStream,
  stopStream,
  restartStream,
  postTitleUpdate,
  postChannelUpdate,
  changeTitle,
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
  getTechnicalQualityValue,
  populateQualityOptions,
  toggleYtCookiesEdit,
  cancelYtCookiesEdit,
  saveYtCookiesEdit,
  updateYtCookiesStatus,
  statusRefreshInterval,
  networkRefreshInterval,
  statusIntervalId,
  networkRefreshIntervalId,
  faceAuthUrl,
  monitorToggleSaveDebounceMs,
  STREAM_LIST_STATUS_STATE_CLASSES,
};
