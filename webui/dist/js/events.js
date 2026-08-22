// events.js — SSE is a wakeup: refetch the matching REST snapshot.

import { isDashboardVisible } from './dom.js';
import { eventStreamUrl, isWebUiAccessReady } from './api.js';

let dashboardEventSource = null;
const eventHandlers = {
  onStatus: null,
  onCluster: null,
  onConfig: null,
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

function initEventStream() {
  if (!isWebUiAccessReady() || !window.EventSource || dashboardEventSource) {
    return;
  }

  dashboardEventSource = new EventSource(eventStreamUrl());
  dashboardEventSource.addEventListener('status', () => {
    runHandler(eventHandlers.onStatus);
  });
  dashboardEventSource.addEventListener('cluster', () => {
    runHandler(eventHandlers.onCluster);
  });
  dashboardEventSource.addEventListener('config', () => {
    runHandler(eventHandlers.onConfig);
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
