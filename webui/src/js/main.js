// main.js — view router, login gate, SSE wakeup, and timers.

import { isDashboardVisible } from './dom.js';
import { state, isViewActive, invalidateManagedData } from './state.js';
import { ensureWebUiAccess } from './api.js';
import { configureEventStream, initEventStream } from './events.js';
import { startLogRefresh, initLogControls, refreshLogs } from './logs.js';
import { initManagementControls, loadManagementListsOnce } from './manage.js';
import {
  initAntiCollisionControls,
  initSystemSettingsActions,
  initFooterUpdateControls,
  initThemeControls,
  loadSystemConfig,
  loadVersion,
  autoCheckUpdates,
  loadTheme,
  reloadServerConfig,
} from './settings.js';
import { checkSetupStatus, initSetupControls } from './setup.js';
import { initCropModalControls } from './crop.js';
import { initClusterControls, startClusterRefresh, refreshClusterStatus } from './cluster.js';
import {
  applyHolodexConfig,
  initDashboardControls,
  initHolodexFold,
  initHolodexLoginModalControls,
  initFaceAuthModalControls,
  initAreaModalControls,
  initStatusRefresh,
  maybeLoadHolodexStreams,
  startHolodexDurationTicker,
  stopHolodexDurationTicker,
  loadChannelData,
  refreshStatus,
  refreshNetworkStatus,
  switchToHolodexStream,
  updatePriorityToggleAvailability,
} from './overview.js';

const VIEW_IDS = ['overview', 'manage', 'settings', 'logs'];
const viewsLoaded = new Set();

function loadViewData(name) {
  switch (name) {
    case 'overview':
      maybeLoadHolodexStreams();
      refreshNetworkStatus();
      break;
    case 'manage':
      loadManagementListsOnce();
      break;
    case 'settings':
      loadSystemConfig();
      break;
    case 'logs':
      refreshLogs();
      break;
  }
}

function activateView(name, options = {}) {
  if (!VIEW_IDS.includes(name)) {
    name = 'overview';
  }

  state.activeView = name;

  for (const id of VIEW_IDS) {
    const panel = document.getElementById(`view-${id}`);
    const tab = document.getElementById(`tab-${id}`);
    const selected = id === name;

    panel?.classList.toggle('is-active', selected);
    if (tab) {
      tab.classList.toggle('is-active', selected);
      tab.setAttribute('aria-selected', selected ? 'true' : 'false');
    }
  }

  try {
    localStorage.setItem('activeView', name);
  } catch (error) {
    // Storage can be unavailable in private windows; navigation still works.
  }

  if (!viewsLoaded.has(name)) {
    viewsLoaded.add(name);
    loadViewData(name);
  } else if (options.reload || name === 'overview') {
    // The Holodex panel catches up every time the overview comes back.
    loadViewData(name);
  }
  if (name === 'overview') startHolodexDurationTicker();
  else stopHolodexDurationTicker();
}

function initViewRouter() {
  const tabs = Array.from(document.querySelectorAll('.tab[data-view]'));

  tabs.forEach((tab, index) => {
    tab.addEventListener('click', () => activateView(tab.dataset.view));
    tab.addEventListener('keydown', event => {
      const offset = event.key === 'ArrowRight' ? 1 : event.key === 'ArrowLeft' ? -1 : 0;
      if (!offset) return;

      event.preventDefault();
      const next = tabs[(index + offset + tabs.length) % tabs.length];
      next.focus();
      activateView(next.dataset.view);
    });
  });

  let saved = null;
  try {
    saved = localStorage.getItem('activeView');
  } catch (error) {
    // Ignore unavailable storage and fall back to the default view.
  }

  activateView(saved || 'overview');
}

// Reloads the config and re-applies the Holodex panel's key layout, which
// also refetches an open panel.
function reloadConfigAndHolodexPanel() {
  reloadServerConfig().then(config => {
    if (config) applyHolodexConfig(config);
    else maybeLoadHolodexStreams();
  });
}

function bindEventStream() {
  configureEventStream({
    onStatus: refreshStatus,
    onConfig: () => {
      invalidateManagedData();
      reloadConfigAndHolodexPanel();
      refreshStatus();
    },
    onHolodex: maybeLoadHolodexStreams,
    onCluster: () => {
      refreshClusterStatus();
    },
    onRefresh: () => {
      invalidateManagedData();
      reloadConfigAndHolodexPanel();
      refreshStatus();
      refreshClusterStatus();
    },
  });
}

function boot() {
  state.hooks.switchToHolodexStream = switchToHolodexStream;
  state.hooks.refreshStatus = refreshStatus;
  state.hooks.reloadServerConfig = reloadServerConfig;
  state.hooks.refreshClusterStatus = refreshClusterStatus;
  state.hooks.updatePriorityToggleAvailability = updatePriorityToggleAvailability;

  bindEventStream();
  startLogRefresh();
  initHolodexFold();
  initViewRouter();
  initDashboardControls();
  initAntiCollisionControls();
  initSystemSettingsActions();
  initLogControls();
  initFooterUpdateControls();
  initThemeControls();
  initSetupControls();
  initCropModalControls();
  initAreaModalControls();
  initManagementControls();
  initHolodexLoginModalControls();
  initFaceAuthModalControls();
  initClusterControls();

  document.addEventListener('visibilitychange', () => {
    if (isDashboardVisible()) {
      startHolodexDurationTicker();
      if (isViewActive('logs')) {
        refreshLogs();
      }
      refreshStatus();
      maybeLoadHolodexStreams();
    } else {
      stopHolodexDurationTicker();
    }
  });

  loadTheme();
  ensureWebUiAccess().then(() => {
    initEventStream();
    return checkSetupStatus();
  }).then(needsSetup => {
    if (!needsSetup) {
      loadVersion();
      initStatusRefresh();
      startClusterRefresh();
      loadChannelData();
      setTimeout(autoCheckUpdates, 2000);
    }
  });
}

boot();

export {
  loadViewData,
  activateView,
  initViewRouter,
  boot,
};
