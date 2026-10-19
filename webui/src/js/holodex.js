// holodex.js — the Holodex panel, split out of overview.js.
//
// Everything panel-specific lives here: its helpers, and the state they mutate.
// The state is an exported object (`holodexState`) rather than module-level `let`s
// because ESM imported bindings are read-only, so overview.js could not assign to
// them. overview.js imports the declarations back, so its public surface is
// unchanged. What still remains in overview.js is the part that also serves other
// views (the status/warning wiring and the switch flow).

import {
  buildHolodexAddChannelPayload,
  hasConfiguredChannel,
  sameChannelCandidate,
  toAdminChannelCandidate,
  toAdminStreamAction,
} from './admin-stream-actions.js';
import { getJson, managementJsonRequest, managementRequest, postJsonApi } from './api.js';
import { isCropSessionCurrent, loadCapturedCropFrame, openCropConfig } from './crop.js';
import { bindDialog } from './dialog.js';
import {
  createSvgIcon,
  isDashboardVisible,
  isElementHidden,
  parseInteger,
  reconcileChildren,
  setButtonLoading,
  setElementDisplay,
  showNotification,
} from './dom.js';
import { loadChannels } from './manage.js';
import { streamIsOnAir } from './on-air.js';
import { isViewActive, state } from './state.js';
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
  fallbackThumbnailUrl,
  placeholderKind,
  streamStartMs,
  toCardModel,
} from './stream-model.js';







/// The Holodex panel's mutable state, in one place.
///
/// ESM imported bindings are read-only, so the panel's state cannot stay as
/// module-level `let`s in one file and be assigned from another. Both this module
/// and `overview.js` therefore read and write it through this object.
export const holodexState = {
  holodexCurrentSource: 'channels',
  holodexKeyConfigured: false,
  holodexLastFetchMs: 0,
  holodexKeepAliveId: null,
  holodexRefreshGeneration: 0,
  renderedHolodexCards: new Map(),
  holodexScheduleDivider: null,
  holodexOnAir: null,
  lastHolodexLive: [],
  lastHolodexScheduled: [],
  holodexAuthState: { logged_in: false, username: null, expired: false, skip_jwt_verify: false },
  holodexUseFavorites: false,
  holodexUseFavoritesInitialized: false,
};

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

// Renews the server's lease: every 4 min, or every minute while SSE is down
// and can't announce changes.
function holodexKeepAliveDue(sinceLastFetchMs, eventsHealthy) {
  return sinceLastFetchMs >= (eventsHealthy ? holodexKeepAliveMs : holodexKeepAliveWithoutEventsMs);
}

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

function startHolodexDurationTicker() {
  startDurationTicker(() => isHolodexPanelOpen());
}

function createHolodexAddChannelControls(model) {
  const candidate = toAdminChannelCandidate(model);
  if (holodexState.holodexCurrentSource !== 'favorites' || !candidate.name
    || hasConfiguredChannel(candidate, state.channelsData?.channels)) {
    return null;
  }

  if (!candidate.youtubeId && !candidate.twitchId) return null;

  const control = document.createElement('span');
  control.className = 'holodex-channel-add';
  control.dataset.channelName = candidate.name;
  control.dataset.youtubeId = candidate.youtubeId;
  control.dataset.twitchId = candidate.twitchId;

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
    skipToggle.checked = !!holodexState.holodexAuthState.skip_jwt_verify;
  }
  if (favoritesToggle) {
    favoritesToggle.checked = !!holodexState.holodexUseFavorites;
  }

  const loggedIn = holodexState.holodexAuthState.logged_in && !holodexState.holodexAuthState.expired;
  setElementDisplay(authBar, true, 'inline-flex');
  setElementDisplay(loginBtn, false);
  setElementDisplay(heartIcon, loggedIn, 'inline-flex');
  setElementDisplay(favoritesMode, loggedIn, 'inline-flex');
  setElementDisplay(jwtSkip, false);

  if (loggedIn) {
    if (favoritesLabel) {
      favoritesLabel.textContent = holodexState.holodexAuthState.username
        ? `${holodexState.holodexAuthState.username}的收藏`
        : '收藏夹';
    }
    if (favoritesBtn) favoritesBtn.title = '管理 Holodex 登录';
  } else if (holodexState.holodexAuthState.expired) {
    if (favoritesLabel) favoritesLabel.textContent = '⚠️ 重新登录';
    if (favoritesBtn) favoritesBtn.title = 'Holodex 登录已过期';
  } else {
    if (favoritesLabel) favoritesLabel.textContent = '🔗 使用Holodex收藏夹';
    if (favoritesBtn) favoritesBtn.title = '登录后使用 Holodex 收藏夹';
  }

  if (logoutBtn) {
    logoutBtn.classList.toggle('hidden', !holodexState.holodexAuthState.logged_in);
  }
}

function updateHolodexModalHint() {
  const hint = document.getElementById('holodex-modal-auth-hint');
  if (!hint) return;

  if (holodexState.holodexAuthState.logged_in && !holodexState.holodexAuthState.expired) {
    hint.classList.add('hidden');
    hint.classList.remove('warning');
    return;
  }

  hint.classList.remove('hidden');

  if (holodexState.holodexAuthState.expired) {
    hint.textContent = '⚠️ JWT 已过期，请重新登录 Holodex 并保存新的 token';
    hint.classList.add('warning');
  } else {
    hint.textContent = '未登录 — 当前使用 channels.json 频道列表 (YouTube + Twitch 外部配信)';
    hint.classList.remove('warning');
  }
}

function markHolodexChannelAdded(channelData) {
  document.querySelectorAll('.holodex-channel-add').forEach(control => {
    const data = readHolodexAddChannelData(control);
    if (sameChannelCandidate(channelData, data)) {
      control.classList.add('holodex-channel-add-added');
      control.replaceChildren(createHolodexChannelAddedIndicator());
    }
  });
}

function setHolodexCollapsed(collapsed) {
  const section = document.getElementById('holodex-section');
  const button = document.getElementById('holodexFoldBtn');
  if (!section) return;

  section.classList.toggle('is-collapsed', collapsed);
  if (collapsed) stopHolodexDurationTicker();
  else startHolodexDurationTicker();
  if (button) {
    const label = collapsed ? '展开' : '折叠';
    button.setAttribute('aria-expanded', collapsed ? 'false' : 'true');
    button.setAttribute('aria-label', label);
    button.title = label;
  }
}

function updateHolodexLoginButton() {
  updateHolodexAuthBar();
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

function createStreamCard(model) {
  const onAirNow = streamIsOnAir(model, holodexState.holodexOnAir);
  const kind = model.isPlaceholder ? placeholderKind(model) : '';
  const href = watchUrl(model) || (model.id ? `https://www.youtube.com/watch?v=${model.id}` : '');
  const channelHref = getHolodexChannelUrl(model.channelId);
  const streamActionData = toAdminStreamAction(model);

  const secondary = document.createElement('div');
  secondary.className = 'holodex-stream-secondary';
  if (href) {
    secondary.appendChild(createWatchLink(href));
  }
  if (model.isLive && kind !== 'niconico') {
    secondary.prepend(createHolodexStreamActionButton(
      'holodex-stream-btn-crop crop-switch-button',
      streamActionData,
      createHolodexStreamSvg([
        { d: 'M6.13 1L6 16a2 2 0 0 0 2 2h15' },
        { d: 'M1 6.13L16 6a2 2 0 0 1 2 2v15' }
      ])
    ));
  }

  const card = renderCard(model, {
    actions: [
      secondary,
      createHolodexStreamActionButton(
        'holodex-stream-btn-switch switch-button',
        streamActionData,
        createHolodexStreamSvg([
          { d: 'M22 12c0 6-4.39 10-9.806 10C7.792 22 4.24 19.665 3 16m-1-4C2 6 6.39 2 11.807 2C16.208 2 19.758 4.335 21 8' },
          { d: 'm7 17l-4-1l-1 4M17 7l4 1l1-4' }
        ])
      ),
    ],
    onAir: onAirNow,
    href,
    thumbnail: model.thumbnail || fallbackThumbnailUrl(model),
    channelHref,
    avatarHref: model.channelId ? channelHref : '',
    avatarPhoto: model.channelId ? getHolodexChannelPhotoUrl(model) : '',
    areaHint: model.suggestedAreaId && model.suggestedAreaName
      ? `🎯 建议分区: ${model.suggestedAreaName}`
      : '',
    channelExtra: createHolodexAddChannelControls(model),
  });
  updateViewerCount(card, model.liveViewers);
  return card;
}
// Store pending switch data

async function loadHolodexAuthStatus() {
  try {
    const data = await getJson('/api/holodex/auth/status');
    if (!data.success) {
      holodexState.holodexAuthState = { logged_in: false, username: null, expired: false, skip_jwt_verify: false };
      holodexState.holodexUseFavorites = false;
      holodexState.holodexUseFavoritesInitialized = false;
      updateHolodexLoginButton();
      updateHolodexModalHint();
      return;
    }
    const info = data.data || {};
    holodexState.holodexAuthState = {
      logged_in: !!info.logged_in,
      username: info.username || null,
      expired: !!info.expired,
      skip_jwt_verify: !!info.skip_jwt_verify
    };
    if (holodexState.holodexAuthState.logged_in && !holodexState.holodexAuthState.expired) {
      if (!holodexState.holodexUseFavoritesInitialized) {
        holodexState.holodexUseFavorites = true;
        holodexState.holodexUseFavoritesInitialized = true;
      }
    } else {
      holodexState.holodexUseFavorites = false;
      holodexState.holodexUseFavoritesInitialized = false;
    }
  } catch (error) {
    holodexState.holodexAuthState = { logged_in: false, username: null, expired: false, skip_jwt_verify: false };
    holodexState.holodexUseFavorites = false;
    holodexState.holodexUseFavoritesInitialized = false;
  }
  updateHolodexLoginButton();
  updateHolodexModalHint();
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

function openHolodexLoginModal() {
  const modal = document.getElementById('holodex-login-modal');
  if (!modal) return;
  updateHolodexModalHint();
  modal.classList.add('active');
  loadHolodexAuthStatus();
}

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

// Every trigger (SSE, opening the panel, the tab coming back, the keep-alive)
// refetches only while the panel is open; a closed one catches up on opening.
function maybeLoadHolodexStreams() {
  if (isHolodexPanelOpen()) {
    refreshHolodexStreams();
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

export {
  HOLODEX_STATUS_STATE_CLASSES,
  addHolodexChannelToCache,
  applyHolodexConfig,
  applyHolodexSectionVisibility,
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
  holodexStreamHasConfiguredChannel,
  initHolodexFold,
  initHolodexLoginModalControls,
  isHolodexPanelOpen,
  loadHolodexAuthStatus,
  logoutHolodexJwt,
  markHolodexChannelAdded,
  maybeLoadHolodexStreams,
  openHolodexLogin,
  openHolodexLoginModal,
  readHolodexAddChannelData,
  readHolodexStreamActionData,
  refreshHolodexChannelsData,
  refreshHolodexStreams,
  renderHolodexCards,
  saveHolodexApiKey,
  saveHolodexJwt,
  setHolodexAddChannelButtonsDisabled,
  setHolodexAddChannelConfirmVisible,
  setHolodexCollapsed,
  setHolodexStatus,
  startHolodexDurationTicker,
  stopHolodexDurationTicker,
  testHolodexApiKey,
  toggleHolodexFavoritesMode,
  toggleHolodexFold,
  toggleHolodexSkipJwtVerify,
  updateHolodexAuthBar,
  updateHolodexLoginButton,
  updateHolodexModalHint,
};
