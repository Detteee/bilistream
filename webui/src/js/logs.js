// logs.js — extracted from app.js

import { isDashboardVisible, reconcileChildren } from './dom.js';
import { isViewActive } from './state.js';
import { getJson } from './api.js';

// Global data
let logLines = [];
let logLineSet = new Set();
const renderedLines = new Map();
let maxLogLines = 500;
let logRefreshIntervalId = null;
let logRefreshInFlight = false;
let logGeneration = 0;
function initLogControls() {
  document
    .getElementById('clear-logs-btn')
    ?.addEventListener('click', clearLogs);
  document
    .getElementById('refresh-logs-btn')
    ?.addEventListener('click', refreshLogs);
}
function startLogRefresh() {
  if (logRefreshIntervalId) {
    clearInterval(logRefreshIntervalId);
  }

  logRefreshIntervalId = setInterval(() => {
    if (isDashboardVisible() && isViewActive('logs')) {
      refreshLogs();
    }
  }, 5000);
}
function clearLogs() {
  logGeneration += 1;
  logLines = [];
  logLineSet.clear();
  renderedLines.clear();
  const logOutput = document.getElementById('log-output');
  if (logOutput) {
    logOutput.replaceChildren(document.createTextNode('日志已清空'));
  }
}
async function refreshLogs() {
  if (logRefreshInFlight) return;
  logRefreshInFlight = true;
  const generation = logGeneration;
  try {
    const data = await getJson('/api/logs');
    if (generation !== logGeneration) return;
    if (data.success && typeof data.logs === 'string') {
      // The API returns the complete rolling snapshot, not a delta. Re-appending
      // lines trimmed on the previous refresh would rotate old entries to the end.
      logLines = [...new Set(data.logs.split('\n').filter(line => line.trim()))]
        .slice(-maxLogLines);
      logLineSet = new Set(logLines);

      renderLogs();

      // Auto scroll if enabled
      const logScroll = document.getElementById('log-scroll');
      if (document.getElementById('auto-scroll-checkbox')?.checked && logScroll) {
        logScroll.scrollTop = logScroll.scrollHeight;
      }
    }
  } catch (error) {
    // Silently fail - logs are optional
    console.debug('Failed to fetch logs:', error);
  } finally {
    logRefreshInFlight = false;
  }
}
function renderLogs() {
  const logOutput = document.getElementById('log-output');
  if (!logOutput) return;

  if (!renderedLines.size) logOutput.replaceChildren();
  for (const line of renderedLines.keys()) {
    if (!logLineSet.has(line)) renderedLines.delete(line);
  }
  const nodes = [];
  logLines.forEach((line, index) => {
    let lineElement = renderedLines.get(line);
    if (!lineElement) {
      lineElement = document.createElement('span');
      lineElement.className = `log-line ${logLineLevel(line)}`.trim();
      renderedLines.set(line, lineElement);
    }
    const text = line + (index + 1 < logLines.length ? '\n' : '');
    if (lineElement.textContent !== text) lineElement.textContent = text;
    nodes.push(lineElement);
  });
  reconcileChildren(logOutput, nodes);
}
function logLineLevel(line) {
  if (line.includes('ERROR') || line.includes('❌')) {
    return 'error';
  }
  if (line.includes('WARN') || line.includes('⚠️')) {
    return 'warn';
  }
  if (line.includes('INFO') || line.includes('✅') || line.includes('🚀')) {
    return 'info';
  }
  if (line.includes('DEBUG') || line.includes('🔄')) {
    return 'debug';
  }
  return '';
}

export {
  initLogControls,
  startLogRefresh,
  clearLogs,
  refreshLogs,
  renderLogs,
  logLineLevel,
  logLines,
  maxLogLines,
  logRefreshIntervalId,
};
