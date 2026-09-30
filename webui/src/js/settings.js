import { initStorageControls, loadManagedStorage } from './storage.js';
// settings.js — extracted from app.js

import { setElementDisplay, appendAntiCollisionRemoveIcon, readIntegerInput, setInputValue, setCheckboxChecked, showNotification } from './dom.js';
import { state, mergeConfigData, updateMonitorToggleStates, applyPriorityAutoRestartToggle, updateDanmakuCommandToggle, applyHolodexMonitorGateToggle, isViewActive } from './state.js';
import { getJson, postJsonApi } from './api.js';
import { loadClusterSettings, getClusterConfigFromForm, getClusterConfigBaseline, acceptClusterConfigBaseline } from './cluster.js';
import { createConfigPatch } from './config-draft.js';
import { saveBooleanToggle } from './toggle-save.js';
import { discoveryTiles, formatGoliveSummary, formatKeyPoolSummary, goliveHourRows, keyMeterRows } from './format.js';

let configBaseline = null;
let keywordsBaseline = null;
let settingsLoadGeneration = 0;
let settingsSaving = false;
let secretRevision = null;
let savedSecretConfig = null;
let nicoSessionChecking = false;
const secretControls = [
  { field: 'holodex_api_key', input: 'config-holodex-key', button: 'config-clear-holodex-key', label: 'Holodex API Key', configured: c => c.holodex_api_key_configured },
  { field: 'youtube_api_key', input: 'config-youtube-api-key', button: 'config-clear-youtube-key', label: 'YouTube Data API Key', configured: c => c.youtube_api_key_configured },
  { field: 'riot_api_key', input: 'config-riot-key', button: 'config-clear-riot-key', label: 'Riot API Key', configured: c => c.riot_api_key_configured },
  { field: 'youtube_proxy', input: 'config-yt-proxy', button: 'config-clear-yt-proxy', label: 'YouTube 代理', configured: c => c.youtube?.proxy_configured },
  { field: 'twitch_proxy', input: 'config-tw-proxy', button: 'config-clear-tw-proxy', label: 'Twitch 代理', configured: c => c.twitch?.proxy_configured },
  { field: 'niconico_proxy', input: 'config-nc-proxy', button: 'config-clear-nc-proxy', label: 'Niconico 代理', configured: c => c.niconico?.proxy_configured },
  { field: 'niconico_user_session', input: 'config-nc-user-session', button: 'config-nc-clear-session', label: 'Niconico 登录信息', configured: c => c.niconico?.user_session_configured || c.niconico?.cookies_file },
];
function renderSecretControls(config = savedSecretConfig) {
  if (!config) return;
  savedSecretConfig = config;
  for (const control of secretControls) {
    const configured = !!control.configured(config);
    document.getElementById(`${control.button}-status`).textContent = configured ? '已保存' : '未保存';
    const button = document.getElementById(control.button);
    button.hidden = !configured;
    button.disabled = settingsSaving || secretRevision === null;
    const input = document.getElementById(control.input);
    const proxy = control.field.endsWith('_proxy');
    const mask = control.field === 'niconico_user_session' ? config.niconico?.user_session_mask : config[`${control.field}_mask`];
    input.classList.toggle('saved-secret', configured && !proxy);
    input.placeholder = proxy ? 'http://127.0.0.1:7890' : configured ? (mask || '已保存') : control.field === 'niconico_user_session' ? '仅填写 user_session 的值' : '填写 API Key';
    if (!proxy) input.title = (configured ? '已保存；输入新值后保存可替换' : '') + (control.field === 'youtube_api_key' ? '（每行一个 Key）' : '');
  }
  updateNicoCheckButton();
}
async function clearSavedSecret(control) {
  if (settingsSaving || !configBaseline || secretRevision === null) return;
  settingsSaving = true;
  ++settingsLoadGeneration;
  renderSecretControls();
  const saveButton = document.getElementById('save-system-config-btn');
  saveButton.disabled = true;
  const input = document.getElementById(control.input);
  const draft = input.value;
  const legacy = control.field === 'niconico_user_session' ? document.getElementById('config-nc-cookies-file') : null;
  const legacyDraft = legacy?.value;
  let cleared = false;
  try {
    const field = `clear_${control.field}`;
    const result = await postJsonApi('/api/config', { [field]: true, expected: { [field]: false }, expected_secret_revision: secretRevision });
    if (!result.success) throw new Error(result.message || '清除失败');
    cleared = true;
    secretRevision = null;
    if (input.value === draft) input.value = '';
    configBaseline[control.field] = '';
    if (legacy) {
      if (legacy.value === legacyDraft) legacy.value = '';
      configBaseline.niconico_cookies_file = '';
    }
    document.getElementById(`${control.button}-status`).textContent = '已清除';
    document.getElementById(control.button).hidden = true;
    const saved = await getJson('/api/config');
    secretRevision = saved.secret_revision ?? null;
    mergeConfigData(saved);
    renderSecretControls(saved);
    if (control.field === 'youtube_api_key') void loadYoutubeKeyStatus();
    if (legacy) void loadNicoSessionStatus();
    showNotification(`${control.label}已清除`, 'success');
  } catch (error) {
    showNotification(cleared ? '已清除，请重新加载状态' : error.message, cleared ? 'success' : 'error');
  } finally {
    settingsSaving = false;
    saveButton.disabled = false;
    if (secretRevision !== null) renderSecretControls();
  }
}

function initAntiCollisionControls() {
  document
    .getElementById('config-anti-collision-checkbox')
    ?.addEventListener('change', toggleAntiCollisionList);
  document
    .getElementById('anti-collision-add-btn')
    ?.addEventListener('click', addAntiCollisionEntry);
}
// Key spend moves with every YouTube call; keep it current while settings
// are on screen instead of only on load.
const KEY_STATUS_REFRESH_MS = 30_000;
function startKeyStatusRefresh() {
  setInterval(() => {
    if (document.visibilityState === 'visible' && isViewActive('settings')) {
      loadYoutubeKeyStatus();
      loadNicoSessionStatus();
    }
  }, KEY_STATUS_REFRESH_MS);
}
function initSystemSettingsActions() {
  initStorageControls();
  for (const control of secretControls) document.getElementById(control.button)?.addEventListener('click', () => clearSavedSecret(control));
  startKeyStatusRefresh();
  document.getElementById('check-nico-session')?.addEventListener('click', checkNicoSession);
  document.getElementById('config-nc-user-session')?.addEventListener('input', updateNicoCheckButton);
  document
    .getElementById('save-system-config-btn')
    ?.addEventListener('click', saveSystemConfig);
  document
    .getElementById('reload-system-config-btn')
    ?.addEventListener('click', loadSystemConfig);
  document
    .getElementById('config-lol-monitor-checkbox')
    ?.addEventListener('change', toggleConfigRiotApiKey);
}
function initFooterUpdateControls() {
  document
    .getElementById('check-updates-btn')
    ?.addEventListener('click', checkForUpdates);
  document
    .getElementById('auto-update-btn')
    ?.addEventListener('click', autoInstallUpdate);
}
function initThemeControls() {
  document
    .getElementById('theme-toggle')
    ?.addEventListener('click', toggleTheme);
}
// Re-apply server config to the config-driven controls without touching
// refresh timers (used when the server signals a config change).
// Resolves to the reloaded config, or null when it could not be loaded.
async function reloadServerConfig() {
  try {
    const config = await getJson('/api/config');
    mergeConfigData(config);
    updateMonitorToggleStates(config);
    updateDanmakuCommandToggle(config.bilibili?.enable_danmaku_command !== false);
    applyPriorityAutoRestartToggle(config);
    return config;
  } catch (error) {
    console.debug('Failed to reload config:', error);
    return null;
  }
}
function toggleConfigRiotApiKey() {
  const checkbox = document.getElementById('config-lol-monitor-checkbox');
  const riotGroup = document.getElementById('config-riot-api-group');
  const intervalGroup = document.getElementById('config-lol-interval-group');
  if (!checkbox || !riotGroup || !intervalGroup) return;

  setElementDisplay(riotGroup, checkbox.checked, 'grid');
  setElementDisplay(intervalGroup, checkbox.checked, 'grid');
}
function toggleAntiCollisionList() {
  const checkbox = document.getElementById('config-anti-collision-checkbox');
  const section = document.getElementById('anti-collision-section');

  if (!checkbox || !section) return;
  section.classList.toggle('hidden', !checkbox.checked);
}
async function loadSystemConfig() {
  void loadManagedStorage();
  if (settingsSaving) return;
  const generation = ++settingsLoadGeneration;
  configBaseline = null;
  try {
    const config = await getJson('/api/config');
    if (generation !== settingsLoadGeneration) return;
    mergeConfigData(config);

    // Load basic settings
    setInputValue('config-interval', config.interval ?? 30);
    setCheckboxChecked('config-show-twitch-checkbox', config.show_twitch !== false);
    setCheckboxChecked('config-show-niconico-checkbox', config.show_niconico === true);
    setCheckboxChecked('config-show-priority-checkbox', config.show_priority_channel === true);
    setCheckboxChecked('config-youtube-rss-checkbox', config.youtube_rss_enabled !== false);
    setCheckboxChecked('config-auto-cover-checkbox', config.auto_cover || false);
    setCheckboxChecked('config-danmaku-command-checkbox', config.bilibili?.enable_danmaku_command !== false);
    applyHolodexMonitorGateToggle(config.holodex_monitor_gate !== false);
    setCheckboxChecked('config-anti-collision-checkbox', config.enable_anti_collision || false);
    toggleAntiCollisionList(); // Show/hide anti-collision section based on checkbox

    // Load API keys
    secretRevision = config.secret_revision ?? null;
    renderSecretControls(config);
    setInputValue('config-holodex-key', config.holodex_api_key || '');
    setInputValue('config-youtube-api-key', config.youtube_api_key || '');
    setInputValue('config-websub-callback-url', config.youtube_websub_callback_url || '');
    setInputValue('config-websub-port', config.youtube_websub_port ?? 3151);
    setInputValue('config-riot-key', config.riot_api_key || '');

    // Load LoL monitor settings
    const lolMonitorEnabled = config.enable_lol_monitor || false;
    setCheckboxChecked('config-lol-monitor-checkbox', lolMonitorEnabled);
    setInputValue('config-lol-interval', config.lol_monitor_interval ?? 1);
    toggleConfigRiotApiKey(); // Show/hide riot API fields based on checkbox

    // Load Twitch settings
    setInputValue('config-tw-region', config.twitch?.proxy_region ?? 'asl');

    // Load YouTube cookies settings
    setInputValue('config-yt-cookies-browser', (config.youtube && config.youtube.cookies_from_browser) || '');
    setInputValue('config-yt-cookies-file', (config.youtube && config.youtube.cookies_file) || '');
    setInputValue('config-yt-deno-path', (config.youtube && config.youtube.deno_path) || '');

    // Load proxy settings
    setInputValue('config-yt-proxy', config.youtube?.proxy || '');
    setInputValue('config-tw-proxy', config.twitch?.proxy || '');
    setInputValue('config-nc-user-session', '');
    setCheckboxChecked('config-nc-session-check', config.niconico?.session_check_enabled !== false);
    loadNicoSessionStatus();
    setInputValue('config-nc-cookies-file', (config.niconico && config.niconico.cookies_file) || '');
    setInputValue('config-nc-proxy', config.niconico?.proxy || '');
    updateNicoCheckButton();

    loadClusterSettings(config.cluster || {});

    // Load anti-collision list
    window.currentAntiCollisionList = config.anti_collision_list || {};
    loadAntiCollisionList(window.currentAntiCollisionList);

    configBaseline = structuredClone(getCurrentConfig());
    loadYoutubeKeyStatus();

    // Load banned keywords
    await loadBannedKeywords(generation);
    if (generation !== settingsLoadGeneration) return;

    // Load monitor toggle states from the config payload already fetched above.
    updateMonitorToggleStates(config);

  } catch (error) {
    console.error('Failed to load system config:', error);
    showNotification('加载配置失败', 'error');
  }
}
function renderNicoSessionStatus(status) {
  const output = document.getElementById('niconico-session-status');
  if (!output) return;
  const time = status?.checked_at ? ` · ${new Date(status.checked_at).toLocaleString()}` : '';
  output.textContent = (status?.message || '尚未检测') + time;
  output.dataset.state = status?.state || 'unchecked';
}
async function loadNicoSessionStatus() {
  try {
    const result = await getJson('/api/niconico/session');
    if (result.success && !nicoSessionChecking) renderNicoSessionStatus(result.data);
  } catch (_) { if (!nicoSessionChecking) renderNicoSessionStatus({ state: 'unavailable', message: '暂时无法读取可用性检测状态' }); }
}
function updateNicoCheckButton() {
  const button = document.getElementById('check-nico-session');
  const input = document.getElementById('config-nc-user-session');
  if (!button || !input) return;
  button.textContent = nicoSessionChecking ? '检测中…' : input.value.trim() ? '保存并检测' : '检测可用性';
  button.disabled = nicoSessionChecking || settingsSaving;
}
async function checkNicoSession() {
  if (nicoSessionChecking || settingsSaving) return;
  const input = document.getElementById('config-nc-user-session');
  const draft = input.value.trim();
  nicoSessionChecking = true;
  updateNicoCheckButton();
  renderNicoSessionStatus({ state: 'checking', message: draft ? '正在保存登录信息…' : '正在检测可用性…' });
  try {
    if (draft) {
      if (!configBaseline || secretRevision === null) throw new Error('请先重新加载凭据状态');
      settingsSaving = true;
      ++settingsLoadGeneration;
      document.getElementById('save-system-config-btn').disabled = true;
      renderSecretControls();
      try {
        const saved = await postJsonApi('/api/config', { niconico_user_session: draft, expected: { niconico_user_session: '' }, expected_secret_revision: secretRevision });
        if (!saved.success) throw new Error(saved.message || '登录信息保存失败');
        secretRevision = null;
        if (input.value.trim() === draft) input.value = '';
        configBaseline.niconico_user_session = '';
        const config = await getJson('/api/config');
        secretRevision = config.secret_revision ?? null;
        mergeConfigData(config);
        renderSecretControls(config);
      } finally {
        settingsSaving = false;
        document.getElementById('save-system-config-btn').disabled = false;
        renderSecretControls();
      }
    }
    renderNicoSessionStatus({ state: 'checking', message: '正在检测可用性…' });
    const result = await postJsonApi('/api/niconico/session/check', {});
    if (!result.success) throw new Error(result.message || '无法检测可用性');
    renderNicoSessionStatus(result.data);
    showNotification(result.data?.message || '检测完成', result.data?.state === 'valid' ? 'success' : 'error');
  } catch (error) {
    renderNicoSessionStatus({ state: 'unavailable', message: error.message });
    showNotification(error.message, 'error');
  } finally {
    nicoSessionChecking = false;
    updateNicoCheckButton();
  }
}

// Key pool state under the YouTube key field; hidden without a key.
async function loadYoutubeKeyStatus() {
  const block = document.getElementById('youtube-key-status');
  if (!block) return;
  let data = null;
  try {
    const result = await getJson('/api/youtube/keys');
    if (result.success) data = result.data;
  } catch (error) {
    console.debug('Failed to load YouTube key status:', error);
  }
  if (!data?.configured) {
    block.hidden = true;
    block.replaceChildren();
    renderHoursChart(null);
    return;
  }
  const meters = el('div', 'key-meters');
  meters.append(...keyMeterRows(data).map(renderKeyMeter));
  block.replaceChildren(
    renderPoolHeadline(data),
    meters,
    renderDiscoveryTiles(discoveryTiles(data)),
  );
  block.hidden = false;
  renderHoursChart(data.playlist);
}
// Two small charts on one local-hour axis: the roster's go-live share, and
// the uploads-playlist polls per channel the budget gives each hour.
function renderHoursChart(playlist) {
  const figure = document.getElementById('youtube-hours-chart');
  if (!figure) return;
  const rows = goliveHourRows(playlist);
  if (!rows.length) {
    figure.hidden = true;
    return;
  }
  const tip = el('div', 'hours-chart-tip');
  tip.hidden = true;
  const maxShare = Math.max(...rows.map(row => row.share));
  const maxPolls = Math.max(...rows.map(row => row.pollsPerHour));
  const plots = figure.querySelector('.hours-chart-plots');
  plots.replaceChildren(
    hoursPlot(rows, '开播占比', maxShare ? `${Math.round(maxShare * 100)}%` : '', row => (maxShare ? row.share / maxShare : 0), tip, figure),
    hoursPlot(rows, '每频道每小时轮询', maxPolls ? `${Math.round(maxPolls)} 次` : '', row => (maxPolls ? row.pollsPerHour / maxPolls : 0), tip, figure),
    hoursAxis(rows),
    tip,
  );
  figure.querySelector('.hours-chart-summary').textContent = formatGoliveSummary(playlist, rows);
  const details = figure.querySelector('.hours-chart-table');
  details.querySelector('table')?.remove();
  details.append(hoursTable(rows));
  figure.hidden = false;
}
function hourTipText(row) {
  const share = `${Math.round(row.share * 100)}%`;
  const pace = row.intervalSecs ? `每 ${row.intervalSecs}s` : '暂停';
  return [`${row.hour}:00`, `开播 ${share}`, `轮询 ${pace}`];
}
function hoursPlot(rows, title, max, height, tip, figure) {
  const plot = el('div', 'hours-chart-plot');
  const head = el('div', 'hours-chart-plot-head');
  head.append(el('span', null, title), el('span', null, max));
  const bars = el('div', 'hours-chart-bars');
  for (const row of rows) {
    const col = el('div', `hours-chart-col${row.current ? ' is-current' : ''}`);
    col.tabIndex = 0;
    col.setAttribute('aria-label', hourTipText(row).join('，'));
    const bar = el('div', 'hours-chart-bar');
    bar.style.height = `${Math.max(0, Math.min(1, height(row))) * 100}%`;
    col.append(bar);
    const show = () => showHourTip(tip, figure, col, row);
    col.addEventListener('pointerenter', show);
    col.addEventListener('focus', show);
    col.addEventListener('pointerleave', () => { tip.hidden = true; });
    col.addEventListener('blur', () => { tip.hidden = true; });
    bars.append(col);
  }
  plot.append(head, bars);
  return plot;
}
function showHourTip(tip, figure, col, row) {
  const [hour, share, pace] = hourTipText(row);
  const value = el('strong', null, pace);
  tip.replaceChildren(document.createTextNode(`${hour} · ${share} · `), value);
  const box = figure.getBoundingClientRect();
  const at = col.getBoundingClientRect();
  tip.style.left = `${Math.min(Math.max(at.left - box.left + at.width / 2, 60), box.width - 60)}px`;
  tip.style.top = `${at.top - box.top - 26}px`;
  tip.hidden = false;
}
function hoursAxis(rows) {
  const axis = el('div', 'hours-chart-axis');
  for (const row of rows) {
    const label = row.current ? '现在' : row.hour % 6 === 0 ? String(row.hour) : '';
    axis.append(el('span', row.current ? 'is-current' : null, label));
  }
  return axis;
}
function hoursTable(rows) {
  const table = el('table');
  const head = el('tr');
  for (const name of ['时', '开播占比', '轮询间隔']) head.append(el('th', null, name));
  table.append(head);
  for (const row of rows) {
    const tr = el('tr');
    tr.append(
      el('td', null, `${row.hour}:00`),
      el('td', null, `${Math.round(row.share * 100)}%`),
      el('td', null, row.intervalSecs ? `${row.intervalSecs}s` : '—'),
    );
    table.append(tr);
  }
  return table;
}
function el(tag, className, text) {
  const node = document.createElement(tag);
  if (className) node.className = className;
  if (text != null) node.textContent = text;
  return node;
}
function renderPoolHeadline(data) {
  const left = Math.round((data.remaining_fraction || 0) * 100);
  const row = el('div', 'key-pool-headline');
  row.append(el('span', 'key-pool-left', `${left}%`), el('span', 'key-pool-caption', formatKeyPoolSummary(data)));
  return row;
}
// One bar per key: fill = share of today's budget used.
function renderKeyMeter(meter) {
  const row = el('div', `key-meter key-meter--${meter.state}`);
  row.title = `${meter.label}: ${meter.value}`;
  const track = el('div', 'key-meter-track');
  track.setAttribute('role', 'meter');
  track.setAttribute('aria-label', `${meter.label} 今日用量`);
  track.setAttribute('aria-valuemin', '0');
  track.setAttribute('aria-valuemax', '100');
  track.setAttribute('aria-valuenow', String(Math.round(meter.fraction * 100)));
  const fill = el('div', 'key-meter-fill');
  fill.style.width = `${meter.fraction * 100}%`;
  track.append(fill);
  row.append(el('span', 'key-meter-label', meter.label), track, el('span', 'key-meter-value', meter.value));
  return row;
}
function renderDiscoveryTiles(tiles) {
  const row = el('div', 'discovery-tiles');
  for (const tile of tiles) {
    const card = el('div', `discovery-tile discovery-tile--${tile.tone}`);
    card.title = tile.detail || '';
    const head = el('div', 'discovery-tile-name');
    head.append(el('span', 'discovery-tile-dot'), document.createTextNode(tile.name));
    card.append(head, el('div', 'discovery-tile-label', tile.label));
    row.append(card);
  }
  return row;
}
async function loadMonitorToggleStates(config = window.configData) {
  try {
    if (typeof config.youtube?.enable_monitor !== 'boolean' || typeof config.twitch?.enable_monitor !== 'boolean' || typeof config.niconico?.enable_monitor !== 'boolean') {
      config = mergeConfigData(await getJson('/api/config'));
    }

    updateMonitorToggleStates(config);
  } catch (error) {
    console.error('Failed to load monitor toggle states:', error);
  }
}
async function loadBannedKeywords(generation = settingsLoadGeneration) {
  keywordsBaseline = null;
  try {
    const data = await getJson('/api/banned-keywords');
    if (generation !== settingsLoadGeneration) return;

    setInputValue('streaming-banned-keywords', (data.streaming_banned_keywords || []).join('\n'));
    setInputValue('danmaku-banned-keywords', (data.danmaku_banned_keywords || []).join('\n'));
    keywordsBaseline = readBannedKeywords();
  } catch (error) {
    console.error('Failed to load banned keywords:', error);
    showNotification('禁用关键词加载失败；本次不会保存关键词，请重新加载后编辑', 'error');
  }
}
// Danmaku Command Toggle Functions
async function loadDanmakuCommandState(config = window.configData) {
  try {
    if (!config.bilibili || typeof config.bilibili.enable_danmaku_command !== 'boolean') {
      config = mergeConfigData(await getJson('/api/config'));
    }

    updateDanmakuCommandToggle(config.bilibili?.enable_danmaku_command !== false);
  } catch (error) {
    console.error('Failed to load danmaku command state:', error);
  }
}
async function toggleDanmakuCommand() {
  return saveBooleanToggle(
    'bili-danmaku-command-toggle',
    window.configData.bilibili?.enable_danmaku_command !== false,
    enabled => postJsonApi('/api/config', { enable_danmaku_command: enabled }),
    enabled => mergeConfigData({ bilibili: { enable_danmaku_command: enabled } })
  );
}
function loadAntiCollisionList(list) {
  const container = document.getElementById('anti-collision-list');
  if (!container) return;

  container.replaceChildren();
  const entries = Object.entries(list || {});
  if (entries.length === 0) {
    const empty = document.createElement('div');
    empty.className = 'anti-collision-empty';
    empty.textContent = '暂无防撞车名单';
    container.appendChild(empty);
    return;
  }

  const table = document.createElement('table');
  table.className = 'anti-collision-table';

  const thead = document.createElement('thead');
  const headerRow = document.createElement('tr');
  headerRow.append(
    createTextCell('th', '用户名'),
    createTextCell('th', '房间号'),
    createTextCell('th', '操作', 'anti-collision-action-cell')
  );
  thead.appendChild(headerRow);

  const tbody = document.createElement('tbody');
  for (const [username, roomId] of entries) {
    tbody.appendChild(createAntiCollisionRow(username, roomId));
  }

  table.append(thead, tbody);
  container.appendChild(table);
}
function createAntiCollisionRow(username, roomId) {
  const row = document.createElement('tr');
  row.append(
    createTextCell('td', username, 'anti-collision-username'),
    createTextCell('td', String(roomId), 'anti-collision-room'),
    createAntiCollisionActionCell(username)
  );
  return row;
}
function createAntiCollisionActionCell(username) {
  const cell = document.createElement('td');
  cell.className = 'anti-collision-action-cell';

  const button = document.createElement('button');
  button.className = 'btn-secondary compact-btn icon-btn cluster-action-btn anti-collision-remove-btn';
  button.type = 'button';
  button.title = '删除';
  button.setAttribute('aria-label', '删除');
  button.addEventListener('click', () => removeAntiCollisionEntry(username));
  appendAntiCollisionRemoveIcon(button);

  cell.appendChild(button);
  return cell;
}
function createTextCell(tagName, text, className = '') {
  const cell = document.createElement(tagName);
  if (className) {
    cell.className = className;
  }
  cell.textContent = text;
  return cell;
}
function readAntiCollisionEntryForm() {
  return {
    username: document.getElementById('anti-collision-username').value.trim(),
    roomId: readIntegerInput('anti-collision-roomid', NaN)
  };
}
function clearAntiCollisionEntryForm() {
  setInputValue('anti-collision-username', '');
  setInputValue('anti-collision-roomid', '');
}
function addAntiCollisionEntry() {
  const { username, roomId } = readAntiCollisionEntryForm();

  if (!username || !Number.isFinite(roomId) || roomId <= 0) {
    showNotification('请填写用户名和有效的房间号', 'error');
    return;
  }

  // Add to global anti-collision list
  if (!window.currentAntiCollisionList) {
    window.currentAntiCollisionList = {};
  }
  window.currentAntiCollisionList[username] = roomId;

  loadAntiCollisionList(window.currentAntiCollisionList);

  clearAntiCollisionEntryForm();
  showNotification('已添加到防撞车名单', 'success');
}
function removeAntiCollisionEntry(username) {
  if (window.currentAntiCollisionList && window.currentAntiCollisionList[username]) {
    delete window.currentAntiCollisionList[username];
    loadAntiCollisionList(window.currentAntiCollisionList);
    showNotification('已从防撞车名单移除', 'success');
  }
}
function getCurrentConfig() {
  return {
    interval: readIntegerInput('config-interval', 30),
    show_twitch: document.getElementById('config-show-twitch-checkbox').checked,
    show_niconico: document.getElementById('config-show-niconico-checkbox').checked,
    show_priority_channel: document.getElementById('config-show-priority-checkbox').checked,
    youtube_rss_enabled: document.getElementById('config-youtube-rss-checkbox').checked,
    auto_cover: document.getElementById('config-auto-cover-checkbox').checked,
    enable_danmaku_command: document.getElementById('config-danmaku-command-checkbox').checked,
    holodex_monitor_gate: !document.getElementById('holodex-monitor-gate-toggle')?.checked,
    enable_anti_collision: document.getElementById('config-anti-collision-checkbox').checked,
    holodex_api_key: document.getElementById('config-holodex-key').value.trim(),
    youtube_api_key: document.getElementById('config-youtube-api-key').value.trim(),
    youtube_websub_callback_url: document.getElementById('config-websub-callback-url').value.trim(),
    youtube_websub_port: readIntegerInput('config-websub-port', 3151),
    riot_api_key: document.getElementById('config-riot-key').value.trim(),
    enable_lol_monitor: document.getElementById('config-lol-monitor-checkbox').checked,
    lol_monitor_interval: readIntegerInput('config-lol-interval', 1),
    youtube_proxy: document.getElementById('config-yt-proxy').value.trim(),
    twitch_proxy: document.getElementById('config-tw-proxy').value.trim(),
    twitch_proxy_region: document.getElementById('config-tw-region').value,
    anti_collision_list: window.currentAntiCollisionList || {},
    youtube_cookies_from_browser: document.getElementById('config-yt-cookies-browser').value.trim(),
    youtube_cookies_file: document.getElementById('config-yt-cookies-file').value.trim(),
    youtube_deno_path: document.getElementById('config-yt-deno-path').value.trim(),
    niconico_user_session: document.getElementById('config-nc-user-session').value.trim(),
    niconico_session_check_enabled: document.getElementById('config-nc-session-check').checked,
    niconico_cookies_file: document.getElementById('config-nc-cookies-file').value.trim(),
    niconico_proxy: document.getElementById('config-nc-proxy').value.trim(),
    cluster: getClusterConfigFromForm()
  };
}
async function saveSystemConfig() {
  if (settingsSaving) return;
  settingsSaving = true;
  renderSecretControls();
  const button = document.getElementById('save-system-config-btn');
  if (button) button.disabled = true;
  try {
    const config = structuredClone(getCurrentConfig());
    if (config.cluster.enabled && (!config.cluster.node_id || !config.cluster.public_api_url)) {
      throw new Error('启用多服务器时必须填写本节点 ID 和 API 地址');
    }
    const patch = createConfigPatch(config, configBaseline);
    if (patch) {
      if (secretRevision === null && secretControls.some(control => control.field in patch)) throw new Error('请重新加载凭据状态后再保存');
      // Proxy addresses are visible; an unchanged masked password is retained
      // explicitly rather than submitted as a credential.
      // expected_secret_revision protects these edits against concurrent changes.
      for (const control of secretControls) {
        if (!control.field.endsWith('_proxy') || !(control.field in patch)) continue;
        const mask = patch[control.field].match(/:([•]+)@/);
        const saved = savedSecretConfig?.[control.field.slice(0, -6)]?.proxy || '';
        if (mask) {
          if (mask[0] !== saved.match(/:([•]+)@/)?.[0]) throw new Error('请完整填写新的代理密码，或保留原来的圆点');
          patch[control.field] = patch[control.field].replace(/:([•]+)@/, ':@');
          patch[`${control.field}_keep_password`] = true;
        }
        patch.expected[control.field] = '';
      }
      if (patch.cluster) patch.expected.cluster = getClusterConfigBaseline();
      patch.expected_secret_revision = secretRevision;
      const result = await postJsonApi('/api/config', patch);
      if (!result.success) throw new Error(result.message || '未知错误');
      if (patch.cluster) acceptClusterConfigBaseline(config.cluster);
      // Preserve edits made during the save: advance only to what was sent.
      configBaseline = structuredClone(config);
      for (const control of secretControls) {
        const input = document.getElementById(control.input);
        if (!control.field.endsWith('_proxy') && control.field in patch && input.value.trim() === config[control.field]) {
          input.value = '';
          configBaseline[control.field] = '';
        }
      }
      const saved = await reloadServerConfig();
      secretRevision = saved?.secret_revision ?? null;
      if (saved) {
        for (const control of secretControls.filter(control => control.field.endsWith('_proxy'))) {
          if (!(control.field in patch)) continue;
          const value = saved[control.field.slice(0, -6)]?.proxy || '';
          const input = document.getElementById(control.input);
          if (input.value.trim() === config[control.field]) input.value = value;
          configBaseline[control.field] = value;
        }
        renderSecretControls(saved);
      }
      loadNicoSessionStatus();
      if (['youtube_api_key', 'youtube_rss_enabled', 'youtube_websub_callback_url', 'youtube_websub_port'].some(key => key in patch)) {
        loadYoutubeKeyStatus();
      }
      if ('holodex_monitor_gate' in patch) {
        state.hooks.refreshStatus?.();
      }
    }
    try {
      await saveBannedKeywords();
      showNotification(keywordsBaseline ? '配置已保存' : '系统配置已保存；关键词未加载，未保存关键词', 'success');
    } catch (keywordError) {
      showNotification(`系统配置已保存，但禁用关键词保存失败: ${keywordError.message}`, 'error');
    }
  } catch (error) {
    console.error('Failed to save system config:', error);
    showNotification(`配置保存失败: ${error.message}`, 'error');
  } finally {
    settingsSaving = false;
    renderSecretControls();
    if (button) button.disabled = false;
  }
}
async function saveBannedKeywords() {
  if (!keywordsBaseline) return;
  const keywords = readBannedKeywords();
  const patch = createConfigPatch(keywords, keywordsBaseline);
  if (!patch) return;
  const result = await postJsonApi('/api/banned-keywords', patch);
  if (!result.success) {
    throw new Error(result.message || '未知错误');
  }
  keywordsBaseline = keywords;
}
function readBannedKeywords() {
  return {
    streaming_banned_keywords: readBannedKeywordLines('streaming-banned-keywords'),
    danmaku_banned_keywords: readBannedKeywordLines('danmaku-banned-keywords')
  };
}
function readBannedKeywordLines(elementId) {
  const value = document.getElementById(elementId)?.value || '';
  return value
    .split('\n')
    .map(keyword => keyword.trim())
    .filter(Boolean);
}
function toggleLolMonitorInputs() {
  const areaId = document.getElementById('area-select').value;
  const lolMonitorGroup = document.getElementById('lol-monitor-group');
  const riotApiKeyGroup = document.getElementById('riot-api-key-group');
  const enableCheckbox = document.getElementById('enable-lol-monitor-inline');
  if (!lolMonitorGroup || !riotApiKeyGroup || !enableCheckbox) return;

  const isLolArea = areaId === '86';
  setElementDisplay(lolMonitorGroup, isLolArea);

  if (isLolArea) {
    // Load current enable_lol_monitor state
    if (window.configData) {
      enableCheckbox.checked = window.configData.enable_lol_monitor || false;
    }
  }

  setElementDisplay(riotApiKeyGroup, isLolArea && enableCheckbox.checked);
}
function toggleRiotApiKeyInputInline() {
  const enableCheckbox = document.getElementById('enable-lol-monitor-inline');
  const riotApiKeyGroup = document.getElementById('riot-api-key-group');
  if (!enableCheckbox || !riotApiKeyGroup) return;

  setElementDisplay(riotApiKeyGroup, enableCheckbox.checked);
}
// Check for updates function
let CURRENT_VERSION = null;
// Will be fetched from API
let IS_TAURI = false;
// Will be fetched from API
const GITHUB_REPO = 'Detteee/bilistream';
let latestUpdateInfo = null;
function getUpdateInfo() {
  return getJson('/api/update/check');
}
// Fetch current version from API
async function loadVersion() {
  try {
    const data = await getJson('/api/version');
    if (data.success && data.data) {
      CURRENT_VERSION = data.data.version;
      IS_TAURI = data.data.is_tauri === true;
      document.getElementById('version-display').textContent = `Bilistream v${CURRENT_VERSION}${IS_TAURI ? ' (Desktop)' : ''}`;
    }
  } catch (error) {
    console.error('Failed to load version:', error);
    document.getElementById('version-display').textContent = 'Bilistream';
  }
}
async function checkForUpdates() {
  try {
    showNotification('正在检查更新...', 'success');

    // Use backend API to check for updates
    const data = await getUpdateInfo();

    if (!data.success) {
      throw new Error(data.message || '检查更新失败');
    }

    const updateInfo = data.data;
    latestUpdateInfo = updateInfo;

    // Compare versions
    if (updateInfo.has_update) {
      renderUpdateNotification(updateInfo, { includeBuildType: true });
      showNotification(`发现新版本 v${updateInfo.latest_version}！`, 'success');
    } else {
      showNotification('已是最新版本！', 'success');
      hideUpdateNotification();
    }
  } catch (error) {
    console.error('Failed to check for updates:', error);
    showNotification('检查更新失败: ' + error.message, 'error');
  }
}
function renderUpdateNotification(updateInfo, options = {}) {
  const updateNotification = document.getElementById('update-notification');
  const updateMessage = document.getElementById('update-message');
  const updateLink = document.getElementById('update-link');
  const autoUpdateBtn = document.getElementById('auto-update-btn');
  const updateProgress = document.getElementById('update-progress');
  if (!updateNotification || !updateMessage || !updateLink || !autoUpdateBtn) {
    return;
  }

  updateMessage.textContent = formatUpdateMessage(updateInfo, options);
  updateProgress?.classList.add('hidden');
  if (updateProgress) {
    updateProgress.textContent = '';
  }
  autoUpdateBtn.disabled = false;
  autoUpdateBtn.textContent = '🚀 自动更新';
  if (updateInfo.download_url) {
    updateLink.href = updateInfo.download_url;
    autoUpdateBtn.classList.remove('hidden');
  } else {
    updateLink.href = `https://github.com/${GITHUB_REPO}/releases/latest`;
    autoUpdateBtn.classList.add('hidden');
  }
  updateNotification.classList.remove('hidden');
}
function hideUpdateNotification() {
  document.getElementById('update-notification')?.classList.add('hidden');
}
function formatUpdateMessage(updateInfo, options = {}) {
  let message = `最新版本 v${updateInfo.latest_version} 已发布！当前版本：v${updateInfo.current_version}`;
  if (updateInfo.asset_name) {
    const sizeMB = (updateInfo.asset_size / 1024 / 1024).toFixed(1);
    const buildSuffix = options.includeBuildType
      ? ` — ${IS_TAURI ? '桌面版 (Tauri)' : '标准版'}`
      : '';
    message += `\n文件: ${updateInfo.asset_name} (${sizeMB} MB)${buildSuffix}`;
  }
  return message;
}
async function autoInstallUpdate() {
  if (!latestUpdateInfo || !latestUpdateInfo.download_url) {
    showNotification('无法获取下载地址', 'error');
    return;
  }

  try {
    const autoUpdateBtn = document.getElementById('auto-update-btn');
    const updateProgress = document.getElementById('update-progress');

    autoUpdateBtn.disabled = true;
    autoUpdateBtn.textContent = '⏳ 下载中...';
    updateProgress.classList.remove('hidden');
    updateProgress.textContent = '正在下载更新，请稍候...';

    showNotification('开始下载更新...', 'success');

    const data = await postJsonApi('/api/update/download', { download_url: latestUpdateInfo.download_url });

    if (data.success) {
      updateProgress.textContent = '更新任务已开始，正在等待完成…';
      const deadline = Date.now() + 360000;
      while (Date.now() < deadline) {
        await new Promise(resolve => setTimeout(resolve, 2000));
        let status, version;
        try {
          [status, version] = await Promise.all([
            getJson('/api/update/status').catch(() => null),
            getJson('/api/version')
          ]);
        } catch (_) {
          updateProgress.textContent = '正在等待程序重启…';
          continue;
        }
        if (status?.phase === 'failed') throw new Error(status.message);
        if (status?.message) updateProgress.textContent = status.message;
        // A release can be refreshed without a version bump. The old process
        // still reports that version while downloading/installing/restarting.
        if (status?.phase === 'idle'
          && version?.data?.version === latestUpdateInfo.latest_version) {
          location.reload();
          return;
        }
      }
      throw new Error('未确认升级完成，请查看日志后重试');
    } else {
      throw new Error(data.message || '下载失败');
    }
  } catch (error) {
    console.error('Failed to download update:', error);
    showNotification('自动更新失败: ' + error.message, 'error');

    const autoUpdateBtn = document.getElementById('auto-update-btn');
    const updateProgress = document.getElementById('update-progress');
    autoUpdateBtn.disabled = false;
    autoUpdateBtn.textContent = '🚀 自动更新';
    updateProgress.textContent = '❌ 更新失败，请尝试手动下载';
  }
}
function compareVersions(v1, v2) {
  const parts1 = v1.split('.').map(Number);
  const parts2 = v2.split('.').map(Number);

  for (let i = 0; i < Math.max(parts1.length, parts2.length); i++) {
    const part1 = parts1[i] || 0;
    const part2 = parts2[i] || 0;

    if (part1 > part2) return 1;
    if (part1 < part2) return -1;
  }

  return 0;
}
// Auto-check for updates on page load (only on main page)
async function autoCheckUpdates() {
  const mainPage = document.getElementById('main-page');
  if (mainPage && !mainPage.classList.contains('hidden')) {
    // Check for updates silently (without notification)
    try {
      const data = await getUpdateInfo();
      if (data.success && data.data && data.data.has_update) {
        const updateInfo = data.data;
        latestUpdateInfo = updateInfo;
        renderUpdateNotification(updateInfo);
      }
    } catch (error) {
      console.debug('Auto-update check failed (silent):', error);
    }
  }
}
// Theme toggle function
// The theme class lives on <html> so the inline head script can apply it
// before first paint; this only keeps the button icon in sync.
function applyThemeIcon(isLight) {
  const button = document.getElementById('theme-toggle');
  const icon = document.querySelector('#theme-toggle-icon use');
  if (icon) {
    icon.setAttribute('href', isLight ? '#i-sun' : '#i-moon');
  }
  if (button) {
    const label = isLight ? '切换到暗色主题' : '切换到亮色主题';
    button.title = label;
    button.setAttribute('aria-label', label);
  }
}
function toggleTheme() {
  const isLight = document.documentElement.classList.toggle('light-theme');
  applyThemeIcon(isLight);
  try {
    localStorage.setItem('theme', isLight ? 'light' : 'dark');
  } catch (error) {
    // Preference simply will not persist when storage is blocked.
  }
}
// Load saved theme preference
function loadTheme() {
  let savedTheme = null;
  try {
    savedTheme = localStorage.getItem('theme');
  } catch (error) {
    // Ignore unavailable storage and keep the default dark theme.
  }

  const isLight = savedTheme === 'light';
  document.documentElement.classList.toggle('light-theme', isLight);
  applyThemeIcon(isLight);
}

export {
  initAntiCollisionControls,
  initSystemSettingsActions,
  initFooterUpdateControls,
  initThemeControls,
  reloadServerConfig,
  toggleConfigRiotApiKey,
  toggleAntiCollisionList,
  loadSystemConfig,
  loadYoutubeKeyStatus,
  loadMonitorToggleStates,
  loadBannedKeywords,
  loadDanmakuCommandState,
  toggleDanmakuCommand,
  loadAntiCollisionList,
  createAntiCollisionRow,
  createAntiCollisionActionCell,
  createTextCell,
  readAntiCollisionEntryForm,
  clearAntiCollisionEntryForm,
  addAntiCollisionEntry,
  removeAntiCollisionEntry,
  getCurrentConfig,
  saveSystemConfig,
  saveBannedKeywords,
  readBannedKeywordLines,
  toggleLolMonitorInputs,
  toggleRiotApiKeyInputInline,
  getUpdateInfo,
  loadVersion,
  checkForUpdates,
  renderUpdateNotification,
  hideUpdateNotification,
  formatUpdateMessage,
  autoInstallUpdate,
  compareVersions,
  autoCheckUpdates,
  applyThemeIcon,
  toggleTheme,
  loadTheme,
  CURRENT_VERSION,
  IS_TAURI,
  GITHUB_REPO,
  latestUpdateInfo,
};
