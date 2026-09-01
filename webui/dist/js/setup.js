// setup.js — extracted from app.js

import { readIntegerInput, showNotification } from './dom.js';
import { getAreaList, appendAreaOptions, createPlatformChannelOption, createSelectOption } from './state.js';
import { getJson, postJsonApi } from './api.js';

function initSetupControls() {
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
function setSetupQrStatus(message, isError = false) {
  const qrStatus = document.getElementById('qr-status');
  if (!qrStatus) return;

  qrStatus.textContent = message;
  qrStatus.classList.toggle('setup-qr-status-error', isError);
}
async function showQrCode() {
  try {
    // Get QR code from API
    const data = await getJson('/api/setup/qrcode');

    if (!data.success || !data.data) {
      showNotification(data.message || '获取二维码失败', 'error');
      return;
    }

    const { qr_url, auth_code } = data.data;
    currentAuthCode = auth_code;

    // Generate QR code using external API
    const qrContainer = document.getElementById('qr-code-display');
    qrContainer.replaceChildren();

    // Create QR code using QR Server API
    const qrImg = document.createElement('img');
    qrImg.src = `https://api.qrserver.com/v1/create-qr-code/?size=200x200&data=${encodeURIComponent(qr_url)}`;
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
    if (!currentAuthCode) return;

    try {
      const data = await postJsonApi('/api/setup/poll-login', { auth_code: currentAuthCode });

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
async function saveSetupConfig() {
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

    // Advanced
    holodex_api_key: document.getElementById('setup-holodex').value.trim() || null,
    holodex_jwt: (() => {
      const jwt = document.getElementById('setup-holodex-jwt').value.trim().replace(/^BEARER\s+/i, '');
      return jwt || null;
    })(),
    riot_api_key: document.getElementById('setup-riot').value || null,
    enable_lol_monitor: document.getElementById('setup-lol-monitor').checked
  };

  try {
    const data = await postJsonApi('/api/setup/save-config', config);

    if (data.success) {
      showNotification('配置保存成功！正在加载控制面板...', 'success');
      setTimeout(() => {
        location.reload();
      }, 1500);
    } else {
      showNotification(data.message || '保存配置失败', 'error');
    }
  } catch (error) {
    console.error('Failed to save config:', error);
    showNotification('保存配置失败: ' + error.message, 'error');
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

    if (areasList.length > 0) {
      const ytAreaSelect = document.getElementById('setup-yt-area');
      const twAreaSelect = document.getElementById('setup-tw-area');

      [ytAreaSelect, twAreaSelect].forEach(select => {
        if (!select) return;
        select.replaceChildren();
        appendAreaOptions(select, areasList, true);
        if (areasList.some(area => Number(area.id) === 235)) {
          select.value = '235';
        }
      });
    }
  } catch (error) {
    console.error('Failed to load areas:', error);
  }
}
async function loadChannelsForSetup() {
  try {
    const channelsData = await getJson('/api/channels');

    if (channelsData && channelsData.channels) {
      const ytChannelSelect = document.getElementById('setup-yt-channel-select');
      const twChannelSelect = document.getElementById('setup-tw-channel-select');
      if (!ytChannelSelect || !twChannelSelect) return;

      // Populate YouTube channels
      ytChannelSelect.replaceChildren(createSelectOption('', '从 channels.json 选择或手动输入...'));
      channelsData.channels.forEach(channel => {
        if (channel.platforms && channel.platforms.youtube) {
          ytChannelSelect.appendChild(createPlatformChannelOption(channel, 'youtube'));
        }
      });

      // Populate Twitch channels
      twChannelSelect.replaceChildren(createSelectOption('', '从 channels.json 选择或手动输入...'));
      channelsData.channels.forEach(channel => {
        if (channel.platforms && channel.platforms.twitch) {
          twChannelSelect.appendChild(createPlatformChannelOption(channel, 'twitch'));
        }
      });
    } else {
      console.warn('No channels data found or invalid format');
    }
  } catch (error) {
    console.error('Failed to load channels:', error);
  }
}
function updateSetupYouTubeChannel() {
  const select = document.getElementById('setup-yt-channel-select');
  const selectedValue = select.value;

  if (!selectedValue) {
    return;
  }

  try {
    const channelInfo = JSON.parse(selectedValue);
    document.getElementById('setup-yt-id').value = channelInfo.id;
    document.getElementById('setup-yt-name').value = channelInfo.name;
  } catch (error) {
    console.error('Failed to parse channel info:', error);
  }
}
function updateSetupTwitchChannel() {
  const select = document.getElementById('setup-tw-channel-select');
  const selectedValue = select.value;

  if (!selectedValue) {
    return;
  }

  try {
    const channelInfo = JSON.parse(selectedValue);
    document.getElementById('setup-tw-id').value = channelInfo.id;
    document.getElementById('setup-tw-name').value = channelInfo.name;
  } catch (error) {
    console.error('Failed to parse channel info:', error);
  }
}
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
