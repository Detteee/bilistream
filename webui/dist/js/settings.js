// settings.js — extracted from app.js

import { setElementDisplay, appendAntiCollisionRemoveIcon, readIntegerInput, setInputValue, setCheckboxChecked, showNotification } from './dom.js';
import { mergeConfigData, updateMonitorToggleStates, updateDanmakuCommandToggle } from './state.js';
import { getJson, postJsonApi } from './api.js';
import { createConfigPatch } from './config-draft.js';
import { saveBooleanToggle } from './toggle-save.js';

let configBaseline = null;
let keywordsBaseline = null;
let settingsLoadGeneration = 0;
let settingsSaving = false;

function initAntiCollisionControls() {
  document
    .getElementById('config-anti-collision-checkbox')
    ?.addEventListener('change', toggleAntiCollisionList);
  document
    .getElementById('anti-collision-add-btn')
    ?.addEventListener('click', addAntiCollisionEntry);
}
function initSystemSettingsActions() {
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
async function reloadServerConfig() {
  try {
    const config = await getJson('/api/config');
    mergeConfigData(config);
    updateMonitorToggleStates(config);
    updateDanmakuCommandToggle(config.bilibili?.enable_danmaku_command !== false);
  } catch (error) {
    console.debug('Failed to reload config:', error);
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
  if (settingsSaving) return;
  const generation = ++settingsLoadGeneration;
  configBaseline = null;
  try {
    const config = await getJson('/api/config');
    if (generation !== settingsLoadGeneration) return;
    mergeConfigData(config);

    // Load basic settings
    setInputValue('config-interval', config.interval ?? 30);
    setCheckboxChecked('config-auto-cover-checkbox', config.auto_cover || false);
    setCheckboxChecked('config-danmaku-command-checkbox', config.bilibili?.enable_danmaku_command !== false);
    setCheckboxChecked('config-anti-collision-checkbox', config.enable_anti_collision || false);
    toggleAntiCollisionList(); // Show/hide anti-collision section based on checkbox

    // Load API keys
    setInputValue('config-holodex-key', config.holodex_api_key || '');
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
    setInputValue('config-yt-proxy', (config.youtube && config.youtube.proxy) || '');
    setInputValue('config-tw-proxy', (config.twitch && config.twitch.proxy) || '');

    // Load anti-collision list
    window.currentAntiCollisionList = config.anti_collision_list || {};
    loadAntiCollisionList(window.currentAntiCollisionList);

    configBaseline = structuredClone(getCurrentConfig());

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
async function loadMonitorToggleStates(config = window.configData) {
  try {
    if (typeof config.youtube?.enable_monitor !== 'boolean' || typeof config.twitch?.enable_monitor !== 'boolean') {
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
    auto_cover: document.getElementById('config-auto-cover-checkbox').checked,
    enable_danmaku_command: document.getElementById('config-danmaku-command-checkbox').checked,
    enable_anti_collision: document.getElementById('config-anti-collision-checkbox').checked,
    holodex_api_key: document.getElementById('config-holodex-key').value.trim(),
    riot_api_key: document.getElementById('config-riot-key').value.trim(),
    enable_lol_monitor: document.getElementById('config-lol-monitor-checkbox').checked,
    lol_monitor_interval: readIntegerInput('config-lol-interval', 1),
    youtube_proxy: document.getElementById('config-yt-proxy').value.trim(),
    twitch_proxy: document.getElementById('config-tw-proxy').value.trim(),
    twitch_proxy_region: document.getElementById('config-tw-region').value,
    anti_collision_list: window.currentAntiCollisionList || {},
    youtube_cookies_from_browser: document.getElementById('config-yt-cookies-browser').value.trim(),
    youtube_cookies_file: document.getElementById('config-yt-cookies-file').value.trim(),
    youtube_deno_path: document.getElementById('config-yt-deno-path').value.trim()
  };
}
async function saveSystemConfig() {
  if (settingsSaving) return;
  settingsSaving = true;
  const button = document.getElementById('save-system-config-btn');
  if (button) button.disabled = true;
  try {
    const config = structuredClone(getCurrentConfig());
    const patch = createConfigPatch(config, configBaseline);
    if (patch) {
      const result = await postJsonApi('/api/config', patch);
      if (!result.success) throw new Error(result.message || '未知错误');
      // Preserve edits made during the save: advance only to what was sent.
      configBaseline = structuredClone(config);
      await reloadServerConfig();
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
  document.dispatchEvent(new Event('areas-json-changed'));
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
      updateProgress.textContent = '✅ 更新下载完成！程序将自动重启...';
      showNotification('更新安装中，程序即将重启...', 'success');

      // Wait a bit then reload the page (program will restart)
      setTimeout(() => {
        location.reload();
      }, 5000);
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
