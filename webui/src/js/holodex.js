// holodex.js — the Holodex panel's pure helpers, split out of overview.js.
//
// Only declarations that do not depend on the rest of overview.js live here. The
// panel's mutable state (its cache, keep-alive timer, on-air snapshot, favourites
// flag) stays in overview.js, because an imported binding cannot be assigned to.
// overview.js imports these back, so its public surface is unchanged.

import { hasConfiguredChannel, toAdminChannelCandidate } from './admin-stream-actions.js';
import { managementRequest, postJsonApi } from './api.js';
import { isCropSessionCurrent, loadCapturedCropFrame, openCropConfig } from './crop.js';
import {
  createSvgIcon,
  isDashboardVisible,
  isElementHidden,
  parseInteger,
  showNotification,
} from './dom.js';
import { isViewActive, state } from './state.js';
import { stopDurationTicker } from './stream-card.js';

// The server keeps a list for 10 min after the last request; SSE announces
// its changes, so an open panel only has to renew that lease.
const holodexKeepAliveMs = 4 * 60 * 1000;
const holodexKeepAliveWithoutEventsMs = 60 * 1000;
const holodexKeepAliveCheckMs = 60 * 1000;

// The overview is the visible view of a visible dashboard, and the panel is
// open with its list shown.
function isHolodexPanelOpen() {
  return isDashboardVisible()
    && isViewActive('overview')
    && !document.getElementById('holodex-section')?.classList.contains('is-collapsed')
    && !isElementHidden(document.getElementById('holodex-streams-section'));
}

const HOLODEX_STATUS_STATE_CLASSES = [
  'holodex-status-loading',
  'holodex-status-warning',
  'holodex-status-success',
  'holodex-status-error'
];

function stopHolodexDurationTicker() {
  stopDurationTicker();
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
function getHolodexChannelUrl(channelId) {
  if (!channelId) return '';
  return `https://holodex.net/channel/${encodeURIComponent(channelId)}`;
}
function getHolodexChannelPhotoUrl(model) {
  if (model.channelPhoto) {
    return model.channelPhoto;
  }
  if (!model.channelId) return '';
  return `https://holodex.net/statics/channelImg/${encodeURIComponent(model.channelId)}/50.png`;
}
async function refreshHolodexChannelsData() {
  const result = await managementRequest('/api/manage/channels');
  if (!result.success || !result.data) {
    throw new Error(result.message || 'Failed to load channels');
  }
  state.channelsData = result.data;
  return state.channelsData;
}
function holodexStreamHasConfiguredChannel(model) {
  return hasConfiguredChannel(toAdminChannelCandidate(model), state.channelsData?.channels);
}
function createHolodexChannelAddIcon(pathData, title) {
  const button = document.createElement('button');
  button.type = 'button';
  button.className = 'holodex-channel-add-btn';
  button.title = title;
  button.appendChild(createSvgIcon('0 0 24 24', pathData));
  return button;
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
  const crop = button.classList.contains('crop-switch-button');
  label.textContent = '切换';
  button.title = crop ? '裁剪切换' : '切换转播';
  button.setAttribute('aria-label', button.title);
  button.append(icon);
  if (!crop) button.append(label);
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

async function cropAndSwitchToHolodexStream(channelId, suggestedAreaId, title, topicId, status, platform = 'youtube', twitchChannelId = '', externalLink = '') {
  // First, open crop modal and auto-capture
  const session = openCropConfig(platform, { autoCapture: false });

  showNotification('正在捕获直播帧...', 'info');

  try {
    const captureUrl = platform === 'twitch'
      ? `/api/crop/capture/twitch?channel_id=${encodeURIComponent(twitchChannelId)}`
      : `/api/crop/capture/youtube?channel_id=${encodeURIComponent(channelId)}`;
    const result = await postJsonApi(captureUrl, undefined, { signal: session.signal });
    if (!isCropSessionCurrent(session)) return;

    if (result.success && result.message) {
      loadCapturedCropFrame(result.message, () => {
        showNotification('直播帧已捕获，请选择裁剪区域后点击"应用裁剪"，然后会自动切换频道', 'success');
        session.pendingSwitch = {
          channelId,
          suggestedAreaId,
          title,
          topicId,
          status,
          platform,
          twitchChannelId,
          externalLink
        };
      }, session);
    } else {
      showNotification(result.message || '捕获失败，请确保直播正在进行', 'error');
    }
  } catch (error) {
    if (!isCropSessionCurrent(session)) return;
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

function closeHolodexLoginModal() {
  const modal = document.getElementById('holodex-login-modal');
  modal?.classList.remove('active');
}

function openHolodexLogin() {
  window.open('https://holodex.net/login', '_blank', 'noopener,noreferrer');
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

function readHolodexAddChannelData(control) {
  return {
    name: control.dataset.channelName || '',
    youtubeId: control.dataset.youtubeId || '',
    twitchId: control.dataset.twitchId || ''
  };
}
function addHolodexChannelToCache(payload) {
  if (!state.channelsData || !Array.isArray(state.channelsData.channels)) {
    state.channelsData = { channels: [] };
  }

  const exists = hasConfiguredChannel({
    name: payload.name,
    youtubeId: payload.platforms.youtube,
    twitchId: payload.platforms.twitch,
  }, state.channelsData.channels);

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

function setHolodexAddChannelConfirmVisible(control, visible) {
  control.classList.toggle('holodex-channel-add-active', visible);
}
function setHolodexAddChannelButtonsDisabled(control, disabled) {
  control.querySelectorAll('button').forEach(button => {
    button.disabled = disabled;
  });
}

export {
  holodexKeepAliveMs,
  holodexKeepAliveWithoutEventsMs,
  holodexKeepAliveCheckMs,
  isHolodexPanelOpen,
  HOLODEX_STATUS_STATE_CLASSES,
  stopHolodexDurationTicker,
  createHolodexStreamSvg,
  getHolodexChannelUrl,
  getHolodexChannelPhotoUrl,
  refreshHolodexChannelsData,
  holodexStreamHasConfiguredChannel,
  createHolodexChannelAddIcon,
  createHolodexStreamActionButton,
  readHolodexStreamActionData,
  cropAndSwitchToHolodexStream,
  testHolodexApiKey,
  closeHolodexLoginModal,
  openHolodexLogin,
  saveHolodexApiKey,
  readHolodexAddChannelData,
  addHolodexChannelToCache,
  createHolodexChannelAddedIndicator,
  setHolodexAddChannelConfirmVisible,
  setHolodexAddChannelButtonsDisabled,
};
