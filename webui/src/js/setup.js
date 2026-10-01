import { bindYoutubeResolver } from './channel-resolver.js';
// setup.js — extracted from app.js

import { loadAreaCatalog, fillAreaCatalog } from './area-catalog.js';
import { readIntegerInput, showNotification } from './dom.js';
import { getAreaList, appendAreaOptions, createPlatformChannelOption, createSelectOption } from './state.js';
import { ensureWebUiAccess, getWebUiAuth, getJson, postJsonApi } from './api.js';

let setupAreaChoices = [];
let officialAreasLoaded = false;
let setupChannels = [];
let favoriteChannels = [];
const selectedFavorites = new Map();
let favoritesGeneration = 0;
const setupPlatforms = { yt: 'youtube', tw: 'twitch', nc: 'niconico' };

function renderSetupPassword(auth = getWebUiAuth()) {
  const checkbox = document.getElementById('setup-panel-password-enabled');
  const input = document.getElementById('setup-panel-password');
  const canCreate = !!auth?.can_create_password && !auth.required;
  document.getElementById('setup-security-title').textContent = auth?.required ? '面板密码' : '面板密码（可选）';
  document.getElementById('setup-panel-password-status').textContent = !auth ? '正在读取密码状态…'
    : auth.required ? '面板密码已设置，完成后可在系统设置 → 安全中更改。'
    : canCreate ? '仅在本机使用可跳过；开放远程访问前请先设置密码。'
    : '当前连接不能设置密码，请在服务器本机直接打开面板。';
  document.getElementById('setup-panel-password-choice').classList.toggle('hidden', !canCreate);
  if (!canCreate) { checkbox.checked = false; input.value = ''; }
  document.getElementById('setup-panel-password-group').classList.toggle('hidden', !canCreate || !checkbox.checked);
}

function updateSetupTarget(platform) {
  const value = document.getElementById(`setup-${platform}-channel-select`).value;
  document.getElementById(`setup-${platform}-fields`).classList.toggle('hidden', !value);
  document.getElementById(`setup-${platform}-identity`).classList.toggle('hidden', value !== 'manual');
  if (value && value !== 'manual') {
    const channel = JSON.parse(value);
    document.getElementById(`setup-${platform}-name`).value = channel.name;
    document.getElementById(`setup-${platform}-id`).value = channel.id;
  }
}

function renderSetupChannelOptions() {
  for (const [platform, provider] of Object.entries(setupPlatforms)) {
    const select = document.getElementById(`setup-${platform}-channel-select`);
    const previous = select.value;
    const seen = new Set();
    const options = [createSelectOption('', '不转播（关闭监控）'), createSelectOption('manual', '手动添加频道...')];
    const channels = platform === 'yt'
      ? [...setupChannels, ...[...selectedFavorites.values()].map(channel => ({ name: channel.name, platforms: { youtube: channel.id } }))]
      : setupChannels;
    for (const channel of channels) {
      const id = channel.platforms?.[provider];
      if (!id || seen.has(id)) continue;
      seen.add(id);
      options.push(createPlatformChannelOption(channel, provider));
    }
    select.replaceChildren(...options);
    select.value = options.some(option => option.value === previous) ? previous : '';
    updateSetupTarget(platform);
  }
}

function filteredFavorites() {
  const query = document.getElementById('setup-favorites-search').value.trim().toLocaleLowerCase();
  return favoriteChannels.filter(channel => `${channel.name} ${channel.id}`.toLocaleLowerCase().includes(query));
}

function renderSetupFavorites() {
  const list = document.getElementById('setup-favorites-list');
  const channels = filteredFavorites();
  list.replaceChildren();
  for (const channel of channels) {
    const label = document.createElement('label');
    label.className = 'setup-favorite-option';
    const checkbox = document.createElement('input');
    checkbox.type = 'checkbox';
    checkbox.value = channel.id;
    checkbox.checked = selectedFavorites.has(channel.id);
    checkbox.addEventListener('change', () => {
      if (checkbox.checked) selectedFavorites.set(channel.id, channel);
      else selectedFavorites.delete(channel.id);
      updateFavoritesCount();
      renderSetupChannelOptions();
    });
    const text = document.createElement('span');
    const name = document.createElement('span');
    name.textContent = channel.name || channel.id;
    const id = document.createElement('small');
    id.textContent = channel.id;
    text.append(name, id);
    label.append(checkbox, text);
    list.append(label);
  }
  if (!channels.length) list.textContent = '没有匹配的频道，试试其他名称。';
  updateFavoritesCount();
}

function updateFavoritesCount() {
  document.getElementById('setup-favorites-count').textContent = `已选 ${selectedFavorites.size} / ${favoriteChannels.length} 个频道`;
}

function resetSetupFavorites() {
  favoritesGeneration += 1;
  favoriteChannels = [];
  selectedFavorites.clear();
  document.getElementById('setup-favorites-choices').classList.add('hidden');
  document.getElementById('setup-favorites-status').textContent = '登录信息已更改，请重新加载收藏。';
  renderSetupChannelOptions();
}

async function loadSetupFavorites() {
  const button = document.getElementById('setup-load-favorites');
  const status = document.getElementById('setup-favorites-status');
  const generation = ++favoritesGeneration;
  const apiKey = document.getElementById('setup-holodex').value.trim();
  const jwt = document.getElementById('setup-holodex-jwt').value.trim().replace(/^Bearer\s+/i, '');
  if (!apiKey || !jwt) {
    status.textContent = '请填写 Holodex API Key 和 JWT，或跳过收藏导入。';
    return;
  }
  button.disabled = true;
  status.textContent = '正在加载收藏频道...';
  try {
    const result = await postJsonApi('/api/setup/holodex-favorites', { api_key: apiKey, jwt }, { timeoutMs: 25000 });
    if (generation !== favoritesGeneration) return;
    if (!result.success || !Array.isArray(result.data)) throw new Error(result.message || '收藏加载失败');
    favoriteChannels = result.data;
    const ids = new Set(favoriteChannels.map(channel => channel.id));
    for (const id of selectedFavorites.keys()) if (!ids.has(id)) selectedFavorites.delete(id);
    document.getElementById('setup-favorites-choices').classList.toggle('hidden', !favoriteChannels.length);
    status.textContent = favoriteChannels.length
      ? `已加载 ${favoriteChannels.length} 个频道，请勾选。`
      : '收藏夹中没有可导入的频道，可以跳过或手动添加。';
    renderSetupFavorites();
    renderSetupChannelOptions();
  } catch (error) {
    if (generation === favoritesGeneration) status.textContent = `${error.message}；可重试或跳过。`;
  } finally {
    button.disabled = false;
  }
}
async function loadOfficialSetupAreas() {
  const button = document.getElementById('setup-load-areas');
  button.disabled = true;
  try {
    const official = await loadAreaCatalog();
    const combined = new Map(setupAreaChoices.map(area => [Number(area.id), area]));
    for (const area of official) if (!combined.has(area.id)) combined.set(area.id, area);
    setupAreaChoices = [...combined.values()]; officialAreasLoaded = true;
    for (const platform of ['yt', 'tw', 'nc']) fillAreaCatalog(document.getElementById(`setup-${platform}-area`), setupAreaChoices);
    document.getElementById('setup-area-hint').textContent = '已加载，完成设置时保存所选分区。';
  } catch (error) {
    document.getElementById('setup-area-hint').textContent = `官方分区暂不可用：${error.message}。仍可选择本地分区或稍后重试。`;
  } finally { button.disabled = false; }
}
function initSetupControls() {
  document.addEventListener('webui-auth-changed', event => renderSetupPassword(event.detail));
  document.getElementById('setup-panel-password')?.addEventListener('input', () => {
    document.getElementById('setup-panel-password-error').textContent = '';
  });
  document.getElementById('setup-panel-password-enabled')?.addEventListener('change', () => {
    if (!document.getElementById('setup-panel-password-enabled').checked) document.getElementById('setup-panel-password').value = '';
    document.getElementById('setup-panel-password-error').textContent = '';
    renderSetupPassword();
  });
  renderSetupPassword();
  bindYoutubeResolver('setup-yt-id', 'setup-resolve-youtube');
  document.getElementById('setup-load-areas')?.addEventListener('click', loadOfficialSetupAreas);
  document.getElementById('setup-nc-channel-select')?.addEventListener('change', () => updateSetupTarget('nc'));
  document.getElementById('setup-load-favorites')?.addEventListener('click', loadSetupFavorites);
  document.getElementById('setup-favorites-search')?.addEventListener('input', renderSetupFavorites);
  for (const id of ['setup-holodex', 'setup-holodex-jwt']) document.getElementById(id)?.addEventListener('input', resetSetupFavorites);
  document.getElementById('setup-favorites-select')?.addEventListener('click', () => {
    for (const channel of filteredFavorites()) selectedFavorites.set(channel.id, channel);
    renderSetupFavorites(); renderSetupChannelOptions();
  });
  document.getElementById('setup-favorites-clear')?.addEventListener('click', () => {
    selectedFavorites.clear(); renderSetupFavorites(); renderSetupChannelOptions();
  });
  document
    .getElementById('show-qr-btn')
    ?.addEventListener('click', showQrCode);
  document
    .getElementById('check-login-status-btn')
    ?.addEventListener('click', checkLoginStatus);
  document
    .getElementById('setup-step-1-next-btn')
    ?.addEventListener('click', () => goToStep(2));
  document
    .getElementById('setup-step-2-prev-btn')
    ?.addEventListener('click', () => goToStep(1));
  document
    .getElementById('setup-step-2-next-btn')
    ?.addEventListener('click', () => goToStep(3));
  document
    .getElementById('setup-step-3-prev-btn')
    ?.addEventListener('click', () => goToStep(2));
  document
    .getElementById('setup-save-btn')
    ?.addEventListener('click', saveSetupConfig);
  document
    .getElementById('setup-yt-channel-select')
    ?.addEventListener('change', updateSetupYouTubeChannel);
  document
    .getElementById('setup-tw-channel-select')
    ?.addEventListener('change', updateSetupTwitchChannel);
  document
    .getElementById('setup-lol-monitor')
    ?.addEventListener('change', toggleRiotApiKey);
}
// Setup wizard functions
let currentStep = 1;
function goToStep(step) {
  // Hide all steps
  for (let i = 1; i <= 3; i++) {
    document.getElementById(`setup-step-${i}`)?.classList.add('hidden');
    document.getElementById(`step-dot-${i}`)?.classList.remove('active');
  }

  // Show target step
  document.getElementById(`setup-step-${step}`)?.classList.remove('hidden');
  document.getElementById(`step-dot-${step}`)?.classList.add('active');
  currentStep = step;

  // Reload channels and areas when entering step 3
  if (step === 3) {
    loadAreasForSetup();
    loadChannelsForSetup();
  }
}
function toggleRiotApiKey() {
  const checkbox = document.getElementById('setup-lol-monitor');
  const group = document.getElementById('riot-api-group');
  if (!checkbox || !group) return;

  group.classList.toggle('hidden', !checkbox.checked);
}
function setSetupLoginStatus(loggedIn) {
  const statusDiv = document.getElementById('login-status');
  const statusText = document.getElementById('login-status-text');
  if (!statusDiv || !statusText) return;

  statusDiv.classList.toggle('setup-login-status-success', loggedIn);
  statusDiv.classList.toggle('setup-login-status-error', !loggedIn);
  statusText.textContent = loggedIn
    ? '✅ 已登录 Bilibili'
    : '❌ 未登录，请点击下方按钮登录';
}
async function checkLoginStatus() {
  try {
    const data = await getJson('/api/setup/login-status');
    setSetupLoginStatus(data.logged_in);
  } catch (error) {
    console.error('Failed to check login status:', error);
    showNotification('检查登录状态失败', 'error');
  }
}
let loginPollInterval = null;
let currentAuthCode = null;
let qrGeneration = 0;
let loginPollInFlight = false;
function setSetupQrStatus(message, isError = false) {
  const qrStatus = document.getElementById('qr-status');
  if (!qrStatus) return;

  qrStatus.textContent = message;
  qrStatus.classList.toggle('setup-qr-status-error', isError);
}
async function showQrCode() {
  const generation = ++qrGeneration;
  clearInterval(loginPollInterval);
  currentAuthCode = null;
  try {
    // Get QR code from API
    const data = await getJson('/api/setup/qrcode');
    if (generation !== qrGeneration) return;

    if (!data.success || !data.data) {
      showNotification(data.message || '获取二维码失败', 'error');
      return;
    }

    const { qr_image, auth_code } = data.data;
    if (!qr_image?.startsWith('data:image/svg+xml;base64,')) {
      throw new Error('服务器未提供二维码，请更新服务端后重试');
    }
    currentAuthCode = auth_code;

    // Rendered by the backend without a third-party QR service.
    const qrContainer = document.getElementById('qr-code-display');
    qrContainer.replaceChildren();

    const qrImg = document.createElement('img');
    qrImg.src = qr_image;
    qrImg.alt = 'Bilibili 登录二维码';
    qrImg.className = 'setup-qr-image';
    qrContainer.appendChild(qrImg);

    // Show QR code container
    document.getElementById('qr-code-container')?.classList.remove('hidden');
    document.getElementById('show-qr-btn').textContent = '🔄 刷新二维码';
    setSetupQrStatus('等待扫码...');

    // Start polling for login status
    startLoginPolling();

    showNotification('请使用 Bilibili APP 扫码登录', 'success');
  } catch (error) {
    if (generation !== qrGeneration) return;
    console.error('Failed to get QR code:', error);
    showNotification('获取二维码失败: ' + error.message, 'error');
  }
}
function startLoginPolling() {
  // Clear existing interval
  if (loginPollInterval) {
    clearInterval(loginPollInterval);
  }

  // Poll every 2 seconds
  loginPollInterval = setInterval(async () => {
    if (!currentAuthCode || loginPollInFlight) return;
    const authCode = currentAuthCode;
    const generation = qrGeneration;
    loginPollInFlight = true;

    try {
      const data = await postJsonApi('/api/setup/poll-login', { auth_code: authCode }, { timeoutMs: 10000 });
      if (generation !== qrGeneration || authCode !== currentAuthCode) return;

      if (data.success && data.data) {
        const { status, message } = data.data;
        setSetupQrStatus(message);

        if (status === 'success') {
          clearInterval(loginPollInterval);
          loginPollInterval = null;
          showNotification('登录成功！', 'success');
          document.getElementById('qr-code-container')?.classList.add('hidden');
          await checkLoginStatus();
        } else if (status === 'expired') {
          clearInterval(loginPollInterval);
          loginPollInterval = null;
          showNotification('二维码已过期，请重新获取', 'error');
          setSetupQrStatus('二维码已过期', true);
        }
      }
    } catch (error) {
      console.error('Poll login failed:', error);
    } finally {
      loginPollInFlight = false;
    }
  }, 2000);
}
async function triggerBiliLogin() {
  showNotification('正在启动登录流程，请在终端查看二维码...', 'success');

  try {
    const data = await postJsonApi('/api/setup/login');

    if (data.success) {
      showNotification('登录成功！', 'success');
      await checkLoginStatus();
    } else {
      showNotification(data.message || '登录失败', 'error');
    }
  } catch (error) {
    console.error('Login failed:', error);
    showNotification('登录失败: ' + error.message, 'error');
  }
}
let setupSaving = false;
let setupNeedsRecheck = false;
async function reconcileSetupSave() {
  await ensureWebUiAccess({ strict: true });
  const status = await getJson('/api/setup-status');
  if (status.storage_error) throw new Error(status.storage_error);
  if (typeof status.needs_setup !== 'boolean') throw new Error('无法确认保存结果');
  setupNeedsRecheck = false;
  if (!status.needs_setup) {
    location.reload();
    return true;
  }
  renderSetupPassword();
  document.getElementById('setup-panel-password-error').textContent = '尚未完成设置，请确认填写内容后再次点击完成设置。';
  return false;
}
async function saveSetupConfig() {
  if (setupSaving) return;
  const passwordError = document.getElementById('setup-panel-password-error');
  passwordError.textContent = '';
  // A failed connection may hide a successful commit. Reconcile before another write.
  if (setupNeedsRecheck) {
    setupSaving = true;
    document.getElementById('setup-save-btn').disabled = true;
    try { await reconcileSetupSave(); }
    catch { passwordError.textContent = '无法确认保存结果，请恢复连接后重新检查。'; }
    finally {
      setupSaving = false;
      document.getElementById('setup-save-btn').disabled = false;
      document.getElementById('setup-save-btn').textContent = setupNeedsRecheck ? '重新检查保存结果' : '完成设置';
    }
    return;
  }
  const password = document.getElementById('setup-panel-password');
  const createPassword = document.getElementById('setup-panel-password-enabled').checked;
  if (createPassword && !password.value.trim()) {
    passwordError.textContent = '请填写面板密码，或关闭「设置面板密码」以跳过。';
    password.focus();
    return;
  }
  // Validate required fields
  const room = readIntegerInput('setup-room', 0);
  if (room <= 0) {
    showNotification('请输入有效的直播间号', 'error');
    goToStep(2);
    return;
  }

  // Collect all configuration
  const config = {
    room,
    interval: readIntegerInput('setup-interval', 60) || 60,
    auto_cover: document.getElementById('setup-auto-cover').checked,
    enable_danmaku_command: document.getElementById('setup-danmaku-command').checked,
    anti_collision: document.getElementById('setup-anti-collision').checked,
    youtube_enable_monitor: !!document.getElementById('setup-yt-channel-select').value,
    twitch_enable_monitor: !!document.getElementById('setup-tw-channel-select').value,
    niconico_enable_monitor: !!document.getElementById('setup-nc-channel-select').value,
    selected_youtube_channels: [...selectedFavorites.values()].map(({ id, name }) => ({ id, name })),

    // YouTube
    youtube_channel_name: document.getElementById('setup-yt-name').value || null,
    youtube_channel_id: document.getElementById('setup-yt-id').value || null,
    youtube_area_v2: readIntegerInput('setup-yt-area', 0) || null,
    youtube_quality: document.getElementById('setup-yt-quality').value || null,
    youtube_proxy: document.getElementById('setup-yt-proxy').value || null,

    // Twitch
    twitch_channel_name: document.getElementById('setup-tw-name').value || null,
    twitch_channel_id: document.getElementById('setup-tw-id').value || null,
    twitch_area_v2: readIntegerInput('setup-tw-area', 0) || null,
    twitch_proxy_region: document.getElementById('setup-tw-region').value || null,
    twitch_quality: document.getElementById('setup-tw-quality').value || null,
    twitch_proxy: document.getElementById('setup-tw-proxy').value || null,

    niconico_channel_name: document.getElementById('setup-nc-name').value.trim() || null,
    niconico_channel_id: document.getElementById('setup-nc-id').value.trim() || null,
    niconico_area_v2: readIntegerInput('setup-nc-area', 235),
    selected_areas: setupAreaChoices.filter(area => ['yt', 'tw', 'nc'].some(platform => document.getElementById(`setup-${platform}-channel-select`).value && readIntegerInput(`setup-${platform}-area`, 235) === Number(area.id))),

    // Advanced
    holodex_api_key: document.getElementById('setup-holodex').value.trim() || null,
    holodex_jwt: (() => {
      const jwt = document.getElementById('setup-holodex-jwt').value.trim().replace(/^BEARER\s+/i, '');
      return jwt || null;
    })(),
    riot_api_key: document.getElementById('setup-riot').value || null,
    enable_lol_monitor: document.getElementById('setup-lol-monitor').checked
  };
  if (createPassword) config.panel_password = password.value;

  setupSaving = true;
  const saveButton = document.getElementById('setup-save-btn');
  saveButton.disabled = true;
  let saved = false;
  try {
    const data = await postJsonApi('/api/setup/save-config', config, { replayAfterLogin: false });

    if (data.success) {
      saved = true;
      password.value = '';
      showNotification('配置保存成功！正在加载控制面板...', 'success');
      setTimeout(() => {
        location.reload();
      }, 1500);
    } else {
      showNotification(data.message || '保存配置失败', 'error');
    }
  } catch (error) {
    showNotification('保存配置失败: ' + error.message, 'error');
    if (!error.status || error.status === 401 || error.status === 409 || error.status >= 500) {
      password.value = '';
      setupNeedsRecheck = true;
      try { saved = await reconcileSetupSave(); }
      catch { passwordError.textContent = '无法确认保存结果，请恢复连接后重新检查。'; }
    }
  } finally {
    if (!saved) { setupSaving = false; saveButton.disabled = false; }
    saveButton.textContent = setupNeedsRecheck ? '重新检查保存结果' : '完成设置';
  }
}
// Setup check functions
function setSetupPageVisible(visible) {
  document.getElementById('setup-page')?.classList.toggle('active', visible);
  document.getElementById('main-page')?.classList.toggle('hidden', visible);
  document.documentElement.classList.add('app-ready');
}
async function checkSetupStatus() {
  try {
    const data = await getJson('/api/setup-status');

    if (data.storage_error) { showNotification(data.storage_error, 'error'); }
    if (data.needs_setup) {
      setSetupPageVisible(true);

      // Load areas for dropdowns
      await loadAreasForSetup();

      // Load channels for dropdowns
      await loadChannelsForSetup();

      // Check login status
      await checkLoginStatus();
    } else {
      setSetupPageVisible(false);
    }

    return data.needs_setup;
  } catch (error) {
    console.error('Failed to check setup status:', error);
    // On error, show main page
    setSetupPageVisible(false);
    return false;
  }
}
async function loadAreasForSetup() {
  try {
    const areasList = getAreaList(await getJson('/api/areas'));
    if (officialAreasLoaded) return;
    setupAreaChoices = areasList;

    if (areasList.length > 0) {
      const ytAreaSelect = document.getElementById('setup-yt-area');
      const twAreaSelect = document.getElementById('setup-tw-area');

      [ytAreaSelect, twAreaSelect, document.getElementById('setup-nc-area')].forEach(select => {
        if (!select) return;
        const previous = select.value;
        select.replaceChildren();
        appendAreaOptions(select, areasList, true);
        select.value = areasList.some(area => String(area.id) === previous) ? previous : '235';
      });
    }
  } catch (error) {
    console.error('Failed to load areas:', error);
  }
}
async function loadChannelsForSetup() {
  try {
    const data = await getJson('/api/channels');
    setupChannels = Array.isArray(data?.channels) ? data.channels : [];
    renderSetupChannelOptions();
  } catch (error) {
    showNotification('无法加载已有频道，仍可手动添加', 'error');
  }
}
function updateSetupYouTubeChannel() { updateSetupTarget('yt'); }
function updateSetupTwitchChannel() { updateSetupTarget('tw'); }
async function checkSetupAndRefresh() {
  const needsSetup = await checkSetupStatus();
  if (!needsSetup) {
    showNotification('设置完成！正在加载控制面板...', 'success');
    setTimeout(() => {
      location.reload();
    }, 1000);
  } else {
    showNotification('请先完成设置步骤', 'error');
  }
}

export {
  initSetupControls,
  goToStep,
  toggleRiotApiKey,
  setSetupLoginStatus,
  checkLoginStatus,
  setSetupQrStatus,
  showQrCode,
  startLoginPolling,
  triggerBiliLogin,
  saveSetupConfig,
  setSetupPageVisible,
  checkSetupStatus,
  loadAreasForSetup,
  loadChannelsForSetup,
  updateSetupYouTubeChannel,
  updateSetupTwitchChannel,
  checkSetupAndRefresh,
  currentStep,
  loginPollInterval,
  currentAuthCode,
};
