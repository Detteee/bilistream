// main.js — polling and wiring for the public status page.
//
// Read-only throughout: the page issues GETs and nothing else. Polling is
// paced for a page that may be open in many tabs for hours, and pauses while
// the tab is hidden.

import { renderStatusCards, renderNiconicoCard, setStatusCardsMessage } from '/shared/js/status-cards.js';
import { clusterIsRestreaming, renderNodes } from './nodes.js';
import { createJsonPoller } from './request.js';
import { bindDialog, bindListboxKeyboard } from '/shared/js/dialog.js';
import {
  closeAreaModal,
  closeCommandModal,
  confirmArea,
  copyCommand,
  renderStreams,
  setAreas,
  setDanmakuEnabled,
  setStatus,
  setStatusFreshness,
  setStreamsFreshness,
  startDurationTicker,
  stopDurationTicker,
} from './streams.js';

/// The status snapshot lives 5s on the server; polling much faster only costs
/// 304s. Streams turn over on the server's own 30s timer.
const STATUS_POLL_MS = 10_000;
const STREAMS_POLL_MS = 30_000;
const AREAS_POLL_MS = 60_000;

let statusTimer = null;
let streamsTimer = null;
let areasTimer = null;
const getJson = createJsonPoller();
let statusCardsSignature = null;
let nodesSignature = null;

function setSyncBanner(message = '') {
  const banner = document.getElementById('public-sync-banner');
  if (!banner) return;
  banner.textContent = message;
  banner.classList.toggle('hidden', !message);
}

async function refreshStatus() {
  try {
    const status = await getJson('/api/public/status');
    if (!status) {
      throw new Error('缺少状态数据');
    }
    // Public cards show confirmed settings as text instead of editable switches.
    const nextCards = JSON.stringify([
      status.bilibili, status.youtube, status.twitch, status.niconico, status.priority_channel,
    ]);
    if (nextCards !== statusCardsSignature) {
      renderStatusCards(status, { readonly: true, showNetwork: false });
      statusCardsSignature = nextCards;
    } else if (status.niconico?.scheduled_start) {
      // Relative scheduled labels change even when the payload/ETag does not.
      renderNiconicoCard(status.niconico, { readonly: true });
    }
    const nextNodes = JSON.stringify(status.nodes);
    if (nextNodes !== nodesSignature) {
      renderNodes(status.nodes);
      nodesSignature = nextNodes;
    }
    setDanmakuEnabled(
      status.bilibili?.enable_danmaku_command,
      clusterIsRestreaming(status.nodes),
    );
    setStatusFreshness(status.in_sync !== false);
    setSyncBanner(status.in_sync === false ? '正在与直播节点同步，点播暂不可用。' : '');
  } catch (error) {
    console.debug('status refresh failed', error);
    statusCardsSignature = null;
    nodesSignature = null;
    setStatusFreshness(false);
    setStatusCardsMessage('连接中断');
    renderNodes(null, '连接中断，节点状态未知');
    setSyncBanner('连接中断，状态无法更新；恢复连接后自动重试。');
  }
}

async function refreshStreams() {
  try {
    const streams = await getJson('/api/public/streams');
    if (!streams) {
      throw new Error('缺少直播列表');
    }
    renderStreams(streams);
  } catch (error) {
    console.debug('streams refresh failed', error);
    setStreamsFreshness(false);
    setStatus(null, '直播列表暂时无法更新，显示上次结果；点播已暂停');
  }
}

async function loadAreas() {
  try {
    setAreas(await getJson('/api/public/areas'));
  } catch (error) {
    console.debug('areas load failed', error);
    setAreas(null);
  }
}

function startPolling() {
  stopPolling();
  if (document.visibilityState !== 'visible') {
    return;
  }
  statusTimer = setInterval(refreshStatus, STATUS_POLL_MS);
  streamsTimer = setInterval(refreshStreams, STREAMS_POLL_MS);
  areasTimer = setInterval(loadAreas, AREAS_POLL_MS);
}

function stopPolling() {
  if (statusTimer) {
    clearInterval(statusTimer);
    statusTimer = null;
  }
  if (streamsTimer) {
    clearInterval(streamsTimer);
    streamsTimer = null;
  }
  if (areasTimer) {
    clearInterval(areasTimer);
    areasTimer = null;
  }
}

function initTheme() {
  const toggle = document.getElementById('themeToggle');
  const icon = toggle?.querySelector('use');
  const apply = (light) => {
    document.documentElement.classList.toggle('light-theme', light);
    icon?.setAttribute('href', light ? '#i-moon' : '#i-sun');
  };

  apply(document.documentElement.classList.contains('light-theme'));
  toggle?.addEventListener('click', () => {
    const light = !document.documentElement.classList.contains('light-theme');
    apply(light);
    try {
      localStorage.setItem('theme', light ? 'light' : 'dark');
    } catch (error) {
      /* storage unavailable */
    }
  });
}

function initModals() {
  document.getElementById('area-modal-cancel')?.addEventListener('click', closeAreaModal);
  document.getElementById('area-modal-confirm')?.addEventListener('click', confirmArea);
  document.getElementById('command-close')?.addEventListener('click', closeCommandModal);
  document.getElementById('command-copy')?.addEventListener('click', () => copyCommand('command-text'));
  document
    .getElementById('command-short-copy')
    ?.addEventListener('click', () => copyCommand('command-short-text'));

  bindDialog('area-modal', closeAreaModal);
  bindDialog('command-modal', closeCommandModal);
  bindListboxKeyboard(document.getElementById('area-modal-list'));
}

function init() {
  initTheme();
  initModals();

  loadAreas();
  refreshStatus();
  refreshStreams();
  startPolling();

  // A page left open in a background tab should not keep polling for hours.
  document.addEventListener('visibilitychange', () => {
    if (document.visibilityState === 'visible') {
      startDurationTicker();
      refreshStatus();
      refreshStreams();
      loadAreas();
      startPolling();
    } else {
      stopPolling();
      stopDurationTicker();
    }
  });
}

if (document.readyState === 'loading') {
  document.addEventListener('DOMContentLoaded', init);
} else {
  init();
}
