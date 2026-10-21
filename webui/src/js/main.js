// main.js — view router, login gate, SSE wakeup, and timers.

import { isDashboardVisible, showNotification } from './dom.js';
import { initTooltips } from './tooltip.js';
import { state, isViewActive, invalidateManagedData } from './state.js';
import { ensureWebUiAccess, getJson, postJson } from './api.js';
import { formatNetworkRate, formatSpeedRatio, formatFps } from './format.js';
import { createAnalyticsLoader } from './analytics-loader.js';
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
import { initClusterMembership, refreshMembership } from './cluster-membership.js';
import { initNotifySettings, loadNotifySettings, refreshNotifyStatus } from './notify.js';
import {
  applyStreamListConfig,
  initDashboardControls,
  initStreamListFold,
  initHolodexLoginModalControls,
  initFaceAuthModalControls,
  initStatusRefresh,
  maybeLoadStreamList,
  startStreamListDurationTicker,
  stopStreamListDurationTicker,
  loadChannelData,
  setOnAir,
} from './overview.js';
import {
  refreshNetworkStatus,
  refreshStatus,
  updatePriorityToggleAvailability,
} from './status-refresh.js';
import { initAreaModalControls, switchToStream } from './switch-flow.js';

const VIEW_IDS = ['overview', 'manage', 'settings', 'logs', 'analytics', 'notify'];
const viewsLoaded = new Set();

// 统计 is a lazy chunk (W14 P1). It must not import stateful app modules, so
// the few it needs are injected here.
const analytics = createAnalyticsLoader({
  importer: () => import('./analytics.js'),
  getView: () => document.getElementById('view-analytics'),
  deps: { getJson, postJson, showNotification, isDashboardVisible, formatNetworkRate, formatSpeedRatio, formatFps },
  reload: () => window.location.reload(),
  isActive: () => isViewActive('analytics'),
});

function loadViewData(name) {
  switch (name) {
    case 'overview':
      maybeLoadStreamList();
      refreshNetworkStatus();
      break;
    case 'manage':
      loadManagementListsOnce();
      break;
    case 'settings':
      loadSystemConfig();
      break;
    case 'notify':
      // QQ 通知 has its own endpoint and save contract, so it owns its own
      // view and its own first-load fetch instead of riding on 系统设置.
      loadNotifySettings();
      break;
    case 'logs':
      refreshLogs();
      break;
    case 'analytics':
      analytics.mount();
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

  if (name !== 'analytics') analytics.unmount();
  document.getElementById(`tab-${name}`)?.scrollIntoView?.({ block: 'nearest', inline: 'nearest' });
  markNavOverflow();

  if (!viewsLoaded.has(name)) {
    viewsLoaded.add(name);
    loadViewData(name);
  } else if (options.reload || name === 'overview' || name === 'analytics') {
    // 统计 refetches on every activation, like 日志.
    // The 直播与预告 panel catches up every time the overview comes back.
    loadViewData(name);
  }
  if (name === 'overview') startStreamListDurationTicker();
  else stopStreamListDurationTicker();
}

// R4: mark which edges of the scrolling tab row are clipped (edge fade in CSS).
function markNavOverflow() {
  const nav = document.querySelector('.tabbar__inner');
  if (!nav) return;
  const max = nav.scrollWidth - nav.clientWidth - 1;
  const x = nav.scrollLeft;
  nav.dataset.overflow = max <= 0 ? '' : x <= 0 ? 'end' : x >= max ? 'start' : 'both';
}

function initViewRouter() {
  const tabs = Array.from(document.querySelectorAll('.tab[data-view]'));
  const nav = document.querySelector('.tabbar__inner');
  nav?.addEventListener('scroll', markNavOverflow, { passive: true });
  window.addEventListener('resize', markNavOverflow);

  tabs.forEach((tab, index) => {
    if (tab.dataset.view === 'analytics') {
      // P4: start fetching the chunk on intent; import() is memoised.
      for (const type of ['pointerenter', 'focus', 'touchstart']) {
        tab.addEventListener(type, analytics.prefetch, { once: true, passive: true });
      }
    }
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

// Reloads the config and re-applies the 直播与预告 panel's key layout, which
// also refetches an open panel.
function reloadConfigAndStreamList() {
  reloadServerConfig().then(config => {
    if (config) applyStreamListConfig(config);
    else maybeLoadStreamList();
  });
}

function bindEventStream() {
  configureEventStream({
    onStatus: () => {
      refreshStatus();
      if (isViewActive('notify')) refreshNotifyStatus();
    },
    onConfig: () => {
      invalidateManagedData();
      reloadConfigAndStreamList();
      refreshStatus();
    },
    onStreamList: maybeLoadStreamList,
    onHistory: () => {
      if (isViewActive('analytics')) analytics.refreshIfVisible();
    },
    onCluster: () => {
      refreshClusterStatus();
      if (isViewActive('settings')) refreshMembership();
    },
    onRefresh: () => {
      invalidateManagedData();
      reloadConfigAndStreamList();
      refreshStatus();
      refreshClusterStatus();
    },
  });
}

function boot() {
  state.hooks.switchToStream = switchToStream;
  state.hooks.refreshStatus = refreshStatus;
  state.hooks.reloadServerConfig = reloadServerConfig;
  state.hooks.refreshClusterStatus = refreshClusterStatus;
  state.hooks.updatePriorityToggleAvailability = updatePriorityToggleAvailability;
  state.hooks.setOnAir = setOnAir;

  bindEventStream();
  startLogRefresh();
  initStreamListFold();
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
  initClusterMembership();
  initNotifySettings();
  initTooltips();

  document.addEventListener('visibilitychange', () => {
    if (isDashboardVisible()) {
      startStreamListDurationTicker();
      if (isViewActive('logs')) {
        refreshLogs();
      }
      if (isViewActive('analytics')) analytics.refreshIfVisible();
      refreshStatus();
      maybeLoadStreamList();
    } else {
      stopStreamListDurationTicker();
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
