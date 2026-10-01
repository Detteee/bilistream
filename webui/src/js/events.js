// events.js — SSE is a wakeup: refetch the matching REST snapshot.

import { isDashboardVisible } from './dom.js';
import { ensureWebUiAccess, eventStreamUrl, isWebUiAccessReady, settleWebUiAuthMutations } from './api.js';

let dashboardEventSource = null;
let checkingAccess = false;
const eventHandlers = {
  onStatus: null,
  onCluster: null,
  onConfig: null,
  onHolodex: null,
  onRefresh: null,
};

function configureEventStream(handlers = {}) {
  Object.assign(eventHandlers, handlers);
}

function eventStreamHealthy() {
  return !!dashboardEventSource
    && dashboardEventSource.readyState === EventSource.OPEN;
}

function runHandler(handler) {
  if (isDashboardVisible() && typeof handler === 'function') {
    handler();
  }
}

async function recheckStreamAccess() {
  if (checkingAccess) return;
  checkingAccess = true;
  try {
    // The server closes streams when credentials change. Wait for a wizard
    // response carrying its new session cookie before checking access.
    await settleWebUiAuthMutations();
    await ensureWebUiAccess({ strict: true });
    if (dashboardEventSource?.readyState === EventSource.CLOSED) {
      dashboardEventSource.close();
      dashboardEventSource = null;
      initEventStream();
    }
  } catch {
    // A 401 permanently closes EventSource. If the auth check also lost its
    // connection, retry it; ordinary connecting streams use native backoff.
    if (dashboardEventSource?.readyState === EventSource.CLOSED) setTimeout(recheckStreamAccess, 3000);
  } finally { checkingAccess = false; }
}

function initEventStream() {
  if (!isWebUiAccessReady() || !window.EventSource || dashboardEventSource) {
    return;
  }

  dashboardEventSource = new EventSource(eventStreamUrl());
  dashboardEventSource.addEventListener('error', recheckStreamAccess);
  dashboardEventSource.addEventListener('status', () => {
    runHandler(eventHandlers.onStatus);
  });
  dashboardEventSource.addEventListener('cluster', () => {
    runHandler(eventHandlers.onCluster);
  });
  dashboardEventSource.addEventListener('config', () => {
    runHandler(eventHandlers.onConfig);
  });
  dashboardEventSource.addEventListener('holodex', () => {
    runHandler(eventHandlers.onHolodex);
  });
  dashboardEventSource.addEventListener('refresh', () => {
    runHandler(eventHandlers.onRefresh);
  });
}

export {
  configureEventStream,
  eventStreamHealthy,
  initEventStream,
};
