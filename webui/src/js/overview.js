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
import { MISSING, QUALITY_MAPPINGS, formatHlsCacheStatus, getQualityDisplayText } from './format.js';

import { setNcChannelDisplay } from './status-cards.js';
import { PLATFORM_EDIT_ROWS, channelEditPayload, fieldEditIds } from './platform-edit-model.js';
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
  bindFieldEditRows();
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
// Platform row editors: channel, area and quality, one row per platform.
// The three platforms differ only by DOM prefix, platform key and how a value is
// read back, so the flow — open the row, fill the options, preselect the current
// value, save, echo — is written once and parameterised.

function fieldEditDom(row) {
  const ids = fieldEditIds(row);
  return {
    span: document.getElementById(ids.span),
    container: document.getElementById(ids.container),
    select: document.getElementById(ids.select)
  };
}
function closeFieldEdit(row) {
  const dom = fieldEditDom(row);
  hideInfoRowEdit(dom.span, dom.container);
}
async function commitFieldEdit(row, payload, { failText, okText, echo }) {
  const data = await postChannelUpdate({ platform: row.platform, ...payload });
  if (!data.success) {
    showNotification(data.message || failText, 'error');
    return;
  }

  showNotification(data.message || okText, 'success');
  closeFieldEdit(row);
  echo?.();
  refreshStatus();
}
function fillChannelSelect(select, platform) {
  select.replaceChildren(createSelectOption('', '选择频道…'));
  appendPlatformChannelOptions(select, platform);
}
function fillAreaSelect(select) {
  select.replaceChildren(createAreaOption('', '选择分区…'));
  if (state.areasData && state.areasData.areas) {
    appendAreaOptions(select, state.areasData.areas);
  }
}
function preselectNcChannel(select, span) {
  const currentId = (span?.dataset.channelId || '').trim();
  const currentName = (span?.textContent || '').trim();
  for (const option of select.options) {
    if (!option.value) continue;
    try {
      const info = JSON.parse(option.value);
      if (
        (currentId && currentId !== MISSING && info.id === currentId)
        || (currentName && currentName !== MISSING && info.name === currentName)
      ) {
        select.value = option.value;
        break;
      }
    } catch {
      // Skip malformed option values from older cached channel lists.
    }
  }

  if (select.options.length <= 1) {
    showNotification('没有可选择的 Niconico 频道。请先在频道配置中填写 Niconico 频道ID 和转播标题。', 'error');
  }
}
function openChannelRow(row) {
  const dom = fieldEditDom(row);
  showInfoRowEdit(dom.span, dom.container);

  if (row.platform === 'niconico') {
    // Niconico keeps its own list rather than the shared channel cache, so read
    // it fresh — a channel added in another tab should show up here.
    getJson('/api/channels')
      .then((data) => {
        state.channelsData = data;
        fillChannelSelect(dom.select, row.platform);
        preselectNcChannel(dom.select, dom.span);
      })
      .catch((error) => {
        showNotification('加载频道列表失败：' + error.message, 'error');
      });
  } else if (state.channelsData) {
    fillChannelSelect(dom.select, row.platform);
  } else {
    loadChannelData().then(() => fillChannelSelect(dom.select, row.platform));
  }

  dom.select?.focus();
}
async function saveChannelRow(row) {
  const dom = fieldEditDom(row);
  const selectedValue = dom.select?.value || '';

  if (!selectedValue) {
    showNotification('请选择频道', 'error');
    return;
  }

  try {
    const payload = channelEditPayload(selectedValue);
    await commitFieldEdit(
      row,
      payload,
      {
        failText: `${row.label}频道更新失败`,
        okText: `${row.label}频道已更新`,
        echo: () => {
          if (row.platform === 'niconico') {
            setNcChannelDisplay(payload.channel_name, payload.channel_id);
          }
        }
      }
    );
  } catch (error) {
    showNotification('更新失败：' + error.message, 'error');
  }
}
function openAreaRow(row) {
  const dom = fieldEditDom(row);
  showInfoRowEdit(dom.span, dom.container);

  if (state.areasData) {
    fillAreaSelect(dom.select);
  } else {
    loadChannelData().then(() => fillAreaSelect(dom.select));
  }

  dom.select?.focus();
}
async function saveAreaRow(row) {
  const dom = fieldEditDom(row);
  const areaId = parseInteger(dom.select?.value, 0);

  if (!areaId) {
    showNotification('请选择分区', 'error');
    return;
  }

  try {
    await commitFieldEdit(
      row,
      { area_id: areaId },
      { failText: `${row.label}配置分区更新失败`, okText: `${row.label}配置分区已更新` }
    );
  } catch (error) {
    showNotification('更新失败：' + error.message, 'error');
  }
}
function openQualityRow(row) {
  const dom = fieldEditDom(row);
  showInfoRowEdit(dom.span, dom.container);
  populateQualityOptions(dom.select, row.platform);

  // The row shows a display label; the select wants the technical value.
  const currentDisplayText = dom.span?.textContent;
  if (dom.select && currentDisplayText && currentDisplayText !== MISSING) {
    dom.select.value = getTechnicalQualityValue(currentDisplayText, row.platform);
  }

  dom.select?.focus();
}
async function saveQualityRow(row) {
  const dom = fieldEditDom(row);
  const quality = dom.select?.value || '';

  if (!quality) {
    showNotification('请选择画质', 'error');
    return;
  }

  try {
    await commitFieldEdit(
      row,
      { quality },
      {
        failText: `${row.label}画质更新失败`,
        okText: `${row.label}画质已更新`,
        echo: () => {
          if (dom.span) {
            dom.span.textContent = getQualityDisplayText(quality, row.platform);
          }
        }
      }
    );
  } catch (error) {
    showNotification('更新失败：' + error.message, 'error');
  }
}
const FIELD_EDIT_OPENERS = { channel: openChannelRow, area: openAreaRow, quality: openQualityRow };
const FIELD_EDIT_SAVERS = { channel: saveChannelRow, area: saveAreaRow, quality: saveQualityRow };
function bindFieldEditRows() {
  for (const row of PLATFORM_EDIT_ROWS) {
    const ids = fieldEditIds(row);
    document
      .getElementById(ids.editButton)
      ?.addEventListener('click', () => FIELD_EDIT_OPENERS[row.kind](row));
    document
      .getElementById(ids.saveButton)
      ?.addEventListener('click', () => FIELD_EDIT_SAVERS[row.kind](row));
    document
      .getElementById(ids.cancelButton)
      ?.addEventListener('click', () => closeFieldEdit(row));
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
