// status-refresh.js — the dashboard's polled status: /api/status, the warning
// holds on the platform cards, the priority card, and the Bilibili network
// panel.
//
// One refresh paints everything the top of the dashboard shows, so the fetch,
// its in-flight/queued guard and the DOM it writes live together. `refreshStatus`
// is the entry point every other module calls. The Bilibili network panel is
// invalidated by a status refresh (`networkStatusGeneration`), so a slow network
// answer cannot paint over a newer snapshot.

import { getJson, postJsonApi } from './api.js';
import { isDashboardVisible, setElementText, showNotification } from './dom.js';
import { createPriorityWarningControl } from './priority-warning.js';
import {
  applyDashboardCardVisibility,
  applyMonitorToggleConfigState,
  getAreaName,
  isViewActive,
  state,
} from './state.js';
import {
  isBiliNetworkLive,
  renderBiliNetworkPanel,
  renderStatusCards,
  setPlatformLiveInfoVisibility,
  setStatusCardsMessage,
} from './status-cards.js';

let statusRefreshInFlight = false;
let statusRefreshQueued = false;
/// The Bilibili network panel's own poll. A status refresh bumps the
/// generation, so an answer that left before it cannot paint afterwards.
let networkRefreshInFlight = false;
let networkStatusGeneration = 0;

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

let lastStatusRefreshMs = 0;

/// Milliseconds since the last completed status fetch. The refresh interval
/// uses it to skip a poll while SSE has already delivered a newer snapshot.
export function msSinceLastStatusRefresh() {
  return Date.now() - lastStatusRefreshMs;
}

/// Binds the four warning/cut controls. `overview.js` calls this from
/// `initDashboardControls`, once the DOM exists.
export function initWarningControls() {
  youtubeWarningControl = bindWarningControl('YT', 'yt-warning-row', 'release-yt-warning-btn');
  twitchWarningControl = bindWarningControl('TW', 'tw-warning-row', 'release-tw-warning-btn');
  niconicoWarningControl = bindWarningControl('NC', 'nc-warning-row', 'release-nc-warning-btn');
  priorityWarningControl = bindWarningControl(
    'priority',
    'priority-warning-row',
    'release-priority-warning-btn'
  );
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

export {
  refreshNetworkStatus,
  refreshStatus,
  schedulePlatformTitleRowCenters,
  updatePriorityToggleAvailability,
};
