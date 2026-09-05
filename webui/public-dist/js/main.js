// main.js — polling and wiring for the public status page.
//
// Read-only throughout: the page issues GETs and nothing else. Polling is
// paced for a page that may be open in many tabs for hours, and pauses while
// the tab is hidden.

import { renderStatusCards, setStatusCardsMessage } from '/shared/js/status-cards.js?v=10';
import { clusterIsRestreaming, renderNodes } from './nodes.js?v=13';
import { createJsonPoller } from './request.js?v=1';
import {
  closeAreaModal,
  closeCommandModal,
  confirmArea,
  copyCommand,
  renderStreams,
  setAreas,
  setDanmakuEnabled,
  setStatus,
  startDurationTicker,
  stopDurationTicker,
} from './streams.js?v=17';

/// The status snapshot lives 5s on the server; polling much faster only costs
/// 304s. Streams turn over on the server's own 30s timer.
const STATUS_POLL_MS = 10_000;
const STREAMS_POLL_MS = 30_000;

let statusTimer = null;
let streamsTimer = null;
const getJson = createJsonPoller();
let statusCardsSignature = null;
let nodesSignature = null;

function setSyncBanner(inSync) {
  document.getElementById('public-sync-banner')?.classList.toggle('hidden', inSync);
}

async function refreshStatus() {
  try {
    const status = await getJson('/api/public/status');
    if (!status) {
      return;
    }
    // readonly keeps every switch visible but locked, so viewers can see what
    // is on without being offered a control that is not theirs.
    const nextCards = JSON.stringify([
      status.bilibili, status.youtube, status.twitch, status.niconico, status.priority_channel,
    ]);
    if (nextCards !== statusCardsSignature) {
      renderStatusCards(status, { readonly: true, showNetwork: false });
      statusCardsSignature = nextCards;
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
    setSyncBanner(status.in_sync !== false);
  } catch (error) {
    console.debug('status refresh failed', error);
    statusCardsSignature = null;
    setStatusCardsMessage('连接中断');
  }
}

async function refreshStreams() {
  try {
    const streams = await getJson('/api/public/streams');
    if (!streams) {
      return;
    }
    renderStreams(streams);
  } catch (error) {
    console.debug('streams refresh failed', error);
    setStatus(null, '暂时无法读取直播列表');
  }
}

async function loadAreas() {
  try {
    setAreas(await getJson('/api/public/areas'));
  } catch (error) {
    console.debug('areas load failed', error);
  }
}

function startPolling() {
  stopPolling();
  if (document.visibilityState !== 'visible') {
    return;
  }
  statusTimer = setInterval(refreshStatus, STATUS_POLL_MS);
  streamsTimer = setInterval(refreshStreams, STREAMS_POLL_MS);
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

  document.addEventListener('keydown', (event) => {
    if (event.key === 'Escape') {
      closeAreaModal();
      closeCommandModal();
    }
  });
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
