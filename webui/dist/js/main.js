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
import {
  initDashboardControls,
  initHolodexFold,
  initHolodexLoginModalControls,
  initFaceAuthModalControls,
  initAreaModalControls,
  initStatusRefresh,
  maybeLoadHolodexStreams,
  loadChannelData,
  refreshStatus,
  switchToHolodexStream,
} from './overview.js';

const VIEW_IDS = ['overview', 'manage', 'settings', 'logs'];
const viewsLoaded = new Set();

function loadViewData(name) {
  switch (name) {
    case 'overview':
      maybeLoadHolodexStreams();
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
  } else if (options.reload) {
    loadViewData(name);
  }
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

function bindEventStream() {
  configureEventStream({
    onStatus: refreshStatus,
    onConfig: () => {
      invalidateManagedData();
      reloadServerConfig();
      refreshStatus();
    },
    onCluster: () => {
      state.hooks.refreshClusterStatus?.();
    },
    onRefresh: () => {
      invalidateManagedData();
      reloadServerConfig();
      refreshStatus();
      state.hooks.refreshClusterStatus?.();
    },
  });
}

function boot() {
  state.hooks.switchToHolodexStream = switchToHolodexStream;
  state.hooks.refreshStatus = refreshStatus;
  state.hooks.reloadServerConfig = reloadServerConfig;

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

  document.addEventListener('visibilitychange', () => {
    if (isDashboardVisible()) {
      if (isViewActive('logs')) {
        refreshLogs();
      }
      refreshStatus();
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
