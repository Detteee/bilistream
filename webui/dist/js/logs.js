// logs.js — extracted from app.js

import { isDashboardVisible } from './dom.js';
import { isViewActive } from './state.js';
import { getJson } from './api.js';

// Global data
let logLines = [];
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
    if (data.success && data.logs) {
      // Add new logs
      const newLogs = data.logs.split('\n').filter(line => line.trim());
      newLogs.forEach(line => {
        if (!logLines.includes(line)) {
          logLines.push(line);
        }
      });

      // Keep only last maxLogLines
      if (logLines.length > maxLogLines) {
        logLines = logLines.slice(-maxLogLines);
      }

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

  const fragment = document.createDocumentFragment();
  logLines.forEach((line, index) => {
    if (index > 0) {
      fragment.appendChild(document.createTextNode('\n'));
    }

    const lineElement = document.createElement('span');
    lineElement.className = `log-line ${logLineLevel(line)}`.trim();
    lineElement.textContent = line;
    fragment.appendChild(lineElement);
  });
  logOutput.replaceChildren(fragment);
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
