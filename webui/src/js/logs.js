// logs.js — extracted from app.js

import { isDashboardVisible, reconcileChildren } from './dom.js';
import { isViewActive } from './state.js';
import { getJson } from './api.js';

// Global data
let logLines = [];
let renderedLines = new Map();
let maxLogLines = 500;
let logRefreshIntervalId = null;
let logRefreshInFlight = false;
let logGeneration = 0;
let logFilter = '';

const LEVELS = /^(ERROR|WARN|INFO|DEBUG|TRACE)\b/;
const TIMESTAMP = /^(\d{4}-\d{2}-\d{2}[ T])?(\d{2}:\d{2}:\d{2})(\.\d+)?(Z|[+-]\d{2}:?\d{2})?\s+/;
const PLATFORM = /^(?:YT|TW|NC)$/;

function initLogControls() {
  document
    .getElementById('clear-logs-btn')
    ?.addEventListener('click', clearLogs);
  document
    .getElementById('refresh-logs-btn')
    ?.addEventListener('click', refreshLogs);
  document
    .getElementById('copy-logs-btn')
    ?.addEventListener('click', copyLogs);
  document
    .getElementById('log-filter-input')
    ?.addEventListener('input', event => {
      logFilter = event.target.value.trim();
      renderLogs();
    });
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
  renderedLines.clear();
  renderLogs(true);
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
/** Lines matching the current filter; render and copy share this list. */
function visibleLogLines() {
  if (!logFilter) return logLines;
  const needle = logFilter.toLowerCase();
  return logLines.filter(line => line.toLowerCase().includes(needle));
}
function renderLogs(cleared = false) {
  const logOutput = document.getElementById('log-output');
  if (!logOutput) return;

  // The snapshot markup may leave whitespace text nodes inside the tbody; a
  // table body should hold rows only, and they would join every copied text.
  for (const child of [...logOutput.childNodes]) {
    if (child.nodeType !== Node.ELEMENT_NODE) child.remove();
  }
  const next = new Map();
  const nodes = [];
  for (const line of visibleLogLines()) {
    let row = renderedLines.get(line);
    if (!row) row = buildLogRow(line);
    next.set(line, row);
    nodes.push(row);
  }
  renderedLines = next;

  if (!nodes.length) {
    const empty = document.createElement('tr');
    empty.className = 'log-row log-row--empty';
    const cell = document.createElement('td');
    cell.colSpan = 6;
    cell.textContent = cleared
      ? '日志已清空'
      : logFilter
        ? '没有匹配的日志'
        : '等待日志…';
    empty.append(cell);
    nodes.push(empty);
  }
  reconcileChildren(logOutput, nodes);
}
function buildLogRow(line) {
  const parsed = parseLogLine(line);
  const row = document.createElement('tr');
  row.className = `log-row ${parsed.level}`.trim();
  row.title = `${parsed.fullTime || '未标注时间'} · 点击复制整行`;
  row.addEventListener('click', () => copyText(line, row));

  const cells = [
    ['log-cell log-cell--time', parsed.time, parsed.fullTime],
    ['log-cell log-cell--level', parsed.level ? parsed.level.toUpperCase() : '', ''],
    ['log-cell log-cell--component', parsed.component, ''],
    ['log-cell log-cell--node', parsed.node, ''],
    ['log-cell log-cell--platform', parsed.platform, ''],
  ];
  for (const [className, text, title] of cells) {
    const cell = document.createElement('td');
    cell.className = className;
    cell.textContent = text;
    if (title) cell.title = title;
    row.append(cell);
  }
  // Brackets are column separators on the wire; in a table they only add noise,
  // so the tag becomes a styled chip and the message keeps its own text.
  const message = document.createElement('td');
  message.className = 'log-cell log-cell--message';
  if (parsed.label) {
    const tag = document.createElement('span');
    tag.className = 'log-tag';
    tag.textContent = parsed.label;
    message.append(tag, ' ');
  }
  message.append(document.createTextNode(parsed.message));
  row.append(message);
  return row;
}
/**
 * Split one record into the columns the table shows. Bracket tags carry the
 * record's identity: `[node]`, `[platform]`, then an optional `[标签]` such as
 * `[弹幕 转播]`. Anything unrecognised stays in `message` verbatim, so a
 * UI-only or future-format line still renders as its full text.
 */
function parseLogLine(line) {
  const raw = String(line ?? '');
  let rest = raw.trimStart();
  let time = '';
  let fullTime = '';
  let level = '';
  let component = '';
  const stamp = rest.match(TIMESTAMP);
  if (stamp) {
    // The column shows the clock only; the untouched stamp stays in the tooltip.
    fullTime = stamp[0].trim();
    time = stamp[2];
    rest = rest.slice(stamp[0].length);
  }
  const severity = rest.match(LEVELS);
  if (severity) {
    level = severity[1].toLowerCase();
    rest = rest.slice(severity[0].length).trimStart();
  }
  const head = rest.match(/^\[([^\]]{1,64})\]\s?/);
  if (head) {
    component = head[1];
    rest = rest.slice(head[0].length);
  }
  let node = '';
  let platform = '';
  let label = '';
  for (let i = 0; i < 3; i += 1) {
    const tag = rest.match(/^\[([^\]]{1,64})\]\s*/);
    if (!tag) break;
    const value = tag[1];
    const isPlatform = PLATFORM.test(value);
    if (!node && isPlatform) {
      platform = value;
    } else if (!node) {
      node = value;
    } else if (!platform && isPlatform) {
      platform = value;
    } else if (!label) {
      label = value;
    } else {
      break;
    }
    rest = rest.slice(tag[0].length);
  }
  // A record that parsed nothing (a UI-only line) keeps its whole text.
  const parsedSomething = time || level || component || node || platform || label;
  return {
    time,
    fullTime,
    level,
    component,
    node,
    platform,
    label,
    message: rest.trim() || (parsedSomething ? '' : raw.trim()),
  };
}
function logLineLevel(line) {
  // Tracing's level is authoritative; titles may contain ERROR or warning emoji.
  // Accept date+offset, older time-only records and explicit bare-level UI lines.
  const parsed = parseLogLine(line);
  if (parsed.level) return parsed.level;

  // Older explicit UI-only messages sometimes begin with an emoji instead.
  const text = parsed.message.trimStart();
  if (text.startsWith('❌')) {
    return 'error';
  }
  if (text.startsWith('⚠')) {
    return 'warn';
  }
  if (text.startsWith('✅') || text.startsWith('🚀')) {
    return 'info';
  }
  if (text.startsWith('🔄')) {
    return 'debug';
  }
  return '';
}
async function copyLogs() {
  await copyText(visibleLogLines().join('\n'), document.getElementById('copy-logs-btn'));
}
/** Copies the untouched record, never the shortened table cells. */
async function copyText(text, feedback) {
  if (!text) return;
  try {
    await navigator.clipboard.writeText(text);
    flash(feedback, '已复制');
  } catch (error) {
    console.debug('Clipboard unavailable:', error);
  }
}
function flash(element, label) {
  if (!element) return;
  if (element.classList?.contains('log-row')) {
    element.classList.add('log-row--copied');
    setTimeout(() => element.classList.remove('log-row--copied'), 600);
    return;
  }
  const previous = element.textContent;
  element.textContent = label;
  element.disabled = true;
  setTimeout(() => {
    element.textContent = previous;
    element.disabled = false;
  }, 900);
}

export {
  initLogControls,
  startLogRefresh,
  clearLogs,
  refreshLogs,
  renderLogs,
  parseLogLine,
  logLineLevel,
  logLines,
  maxLogLines,
  logRefreshIntervalId,
};
