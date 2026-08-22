      // Global data
      let channelsData = null;
      let areasData = null;
      let logLines = [];
      let maxLogLines = 500;
      let statusRefreshInterval = 60000; // Default 60 seconds
      const networkRefreshInterval = 1000;
      let statusIntervalId = null;
      let networkRefreshIntervalId = null;
      let logRefreshIntervalId = null;
      let statusRefreshInFlight = false;
      let statusRefreshQueued = false;
      let networkRefreshInFlight = false;
      let lastBiliNetworkLive = false;
      let lastBiliNetworkQuality = null;
      const biliNetworkHistory = { cache: [], push: [] };
      const biliNetworkHistoryLimit = 48;
      let faceAuthUrl = null;
      let holodexCurrentSource = 'channels';
      let holodexStreamsRequested = false;
      const monitorToggleSaveDebounceMs = 160;
      const monitorToggleSaveState = new Map();

      let webUiAccessReady = false;
      let webUiLoginPromise = null;

      function eventStreamUrl() {
        return '/api/events';
      }

      function unauthorizedApiError() {
        return '需要访问密码才能打开控制面板';
      }

      function ensureWebUiLoginStyles() {
        if (document.getElementById('webui-login-style')) {
          return;
        }
        const style = document.createElement('style');
        style.id = 'webui-login-style';
        style.textContent = `
          #webui-login-gate {
            position: fixed;
            inset: 0;
            z-index: 400;
            display: flex;
            align-items: center;
            justify-content: center;
            padding: 24px;
            background: rgba(4, 4, 10, 0.72);
            backdrop-filter: blur(6px);
            -webkit-backdrop-filter: blur(6px);
          }
          #webui-login-gate.hidden { display: none !important; }
          #webui-login-gate .webui-login-dialog {
            width: min(420px, 100%);
            padding: 28px;
            border: 1px solid var(--line, #2a2a38);
            border-radius: 16px;
            background: var(--surface-1, #16161f);
            color: var(--text, #f3f3f7);
            box-shadow: 0 18px 48px rgba(0, 0, 0, 0.35);
          }
          #webui-login-gate h2 {
            margin: 0 0 8px;
            font-size: 1.25rem;
          }
          #webui-login-gate p {
            margin: 0 0 16px;
            color: var(--text-mute, #9a9aab);
            font-size: 0.95rem;
          }
          #webui-login-gate label {
            display: block;
            margin-bottom: 6px;
            font-size: 0.9rem;
          }
          #webui-login-password {
            width: 100%;
            margin-bottom: 12px;
            padding: 10px 12px;
            border: 1px solid var(--line, #2a2a38);
            border-radius: 10px;
            background: var(--surface-2, #1e1e2a);
            color: inherit;
          }
          #webui-login-error {
            min-height: 1.2em;
            margin: 0 0 12px;
            color: #f07178;
            font-size: 0.9rem;
          }
          #webui-login-submit {
            width: 100%;
          }
        `;
        document.head.appendChild(style);
      }

      function showWebUiLoginGate(onSuccess) {
        ensureWebUiLoginStyles();
        let gate = document.getElementById('webui-login-gate');
        if (!gate) {
          gate = document.createElement('div');
          gate.id = 'webui-login-gate';
          gate.setAttribute('role', 'dialog');
          gate.setAttribute('aria-modal', 'true');
          gate.setAttribute('aria-labelledby', 'webui-login-title');

          const dialog = document.createElement('form');
          dialog.className = 'webui-login-dialog';
          dialog.id = 'webui-login-form';

          const title = document.createElement('h2');
          title.id = 'webui-login-title';
          title.textContent = '访问密码';

          const hint = document.createElement('p');
          hint.textContent = '请输入访问密码。';

          const label = document.createElement('label');
          label.setAttribute('for', 'webui-login-password');
          label.textContent = '密码';

          const input = document.createElement('input');
          input.id = 'webui-login-password';
          input.name = 'password';
          input.type = 'password';
          input.autocomplete = 'current-password';
          input.required = true;

          const error = document.createElement('p');
          error.id = 'webui-login-error';

          const submit = document.createElement('button');
          submit.id = 'webui-login-submit';
          submit.type = 'submit';
          submit.className = 'btn-primary';
          submit.textContent = '进入';

          dialog.append(title, hint, label, input, error, submit);
          gate.appendChild(dialog);
          document.body.appendChild(gate);

          dialog.addEventListener('submit', async (event) => {
            event.preventDefault();
            error.textContent = '';
            submit.disabled = true;
            try {
              const response = await fetch('/api/login', {
                method: 'POST',
                headers: { 'Content-Type': 'application/json' },
                body: JSON.stringify({ password: input.value })
              });
              const body = await response.json().catch(() => null);
              if (!response.ok || body?.success === false) {
                error.textContent = body?.message || '密码错误';
                input.focus();
                input.select();
                return;
              }
              input.value = '';
              gate.classList.add('hidden');
              dialog._webUiLoginSuccess?.();
            } catch (loginError) {
              error.textContent = loginError.message || '登录失败';
            } finally {
              submit.disabled = false;
            }
          });
        }

        gate.classList.remove('hidden');
        const form = document.getElementById('webui-login-form');
        if (form) {
          form._webUiLoginSuccess = onSuccess;
        }
        const error = document.getElementById('webui-login-error');
        if (error) {
          error.textContent = '';
        }
        document.getElementById('webui-login-password')?.focus();
      }

      function promptWebUiLogin() {
        if (webUiLoginPromise) {
          return webUiLoginPromise;
        }
        webUiLoginPromise = new Promise((resolve) => {
          showWebUiLoginGate(() => resolve());
        }).finally(() => {
          webUiLoginPromise = null;
        });
        return webUiLoginPromise;
      }

      async function ensureWebUiAccess() {
        try {
          const response = await fetch('/api/auth');
          const auth = await response.json();
          if (auth.required && !auth.authenticated) {
            await promptWebUiLogin();
          }
        } catch (error) {
          // Old servers without /api/auth should still load the panel.
        }
        webUiAccessReady = true;
        initEventStream();
      }

      async function fetchWithWebUiAuth(path, options = {}) {
        const response = await fetch(path, options);
        if (response.status !== 401) {
          return response;
        }
        await promptWebUiLogin();
        return fetch(path, options);
      }

      // Global config data for access across functions
      window.configData = {
        enable_lol_monitor: false,
        riot_api_key: '',
        holodex_api_key: '',
        bilibili: {},
        youtube: {},
        twitch: {}
      };

      function mergeConfigData(config) {
        window.configData = {
          ...window.configData,
          ...config,
          enable_lol_monitor: config.enable_lol_monitor || false,
          riot_api_key: config.riot_api_key || '',
          holodex_api_key: config.holodex_api_key || '',
          bilibili: config.bilibili || window.configData.bilibili || {},
          youtube: config.youtube || window.configData.youtube || {},
          twitch: config.twitch || window.configData.twitch || {}
        };
        return window.configData;
      }

      function updateMonitorToggleStates(config = window.configData) {
        const youtubeToggle = document.getElementById('youtube-monitor-toggle');
        const twitchToggle = document.getElementById('twitch-monitor-toggle');

        if (youtubeToggle) {
          applyMonitorToggleConfigState(youtubeToggle, 'youtube-monitor-toggle', config.youtube?.enable_monitor !== false);
        }
        if (twitchToggle) {
          applyMonitorToggleConfigState(twitchToggle, 'twitch-monitor-toggle', config.twitch?.enable_monitor !== false);
        }
      }

      function applyMonitorToggleConfigState(toggle, toggleId, enabled) {
        const state = monitorToggleSaveState.get(toggleId);
        if (state?.timer || state?.inFlight) {
          return;
        }

        toggle.checked = enabled;
        if (state) {
          state.confirmed = enabled;
          state.desired = enabled;
        }
      }

      function updateDanmakuCommandToggle(enabled) {
        const toggle = document.getElementById('bili-danmaku-command-toggle');
        if (toggle && typeof enabled === 'boolean') {
          toggle.checked = enabled;
        }
      }

      function isDashboardVisible() {
        const mainPage = document.getElementById('main-page');
        return document.visibilityState === 'visible' && mainPage && !mainPage.classList.contains('hidden');
      }

      function initDashboardControls() {
        document
          .getElementById('startLiveBtn')
          ?.addEventListener('click', startStream);
        document
          .getElementById('stopLiveBtn')
          ?.addEventListener('click', stopStream);
        document
          .getElementById('restartStreamBtn')
          ?.addEventListener('click', restartStream);
        document
          .getElementById('refreshBilibiliBtn')
          ?.addEventListener('click', refreshBilibiliStatus);
        document
          .getElementById('youtube-monitor-toggle')
          ?.addEventListener('change', toggleYouTubeMonitor);
        document
          .getElementById('refreshYouTubeBtn')
          ?.addEventListener('click', refreshYouTubeStatus);
        document
          .getElementById('twitch-monitor-toggle')
          ?.addEventListener('change', toggleTwitchMonitor);
        document
          .getElementById('refreshTwitchBtn')
          ?.addEventListener('click', refreshTwitchStatus);
        document
          .getElementById('refreshHolodexBtn')
          ?.addEventListener('click', refreshHolodexStreams);
        document
          .getElementById('bili-title-edit-btn')
          ?.addEventListener('click', toggleTitleEdit);
        document
          .getElementById('bili-title-save-btn')
          ?.addEventListener('click', saveTitleEdit);
        document
          .getElementById('bili-title-cancel-btn')
          ?.addEventListener('click', cancelTitleEdit);
        document
          .getElementById('title-edit-input')
          ?.addEventListener('keydown', handleTitleEditKeydown);
        document
          .getElementById('bili-area-edit-btn')
          ?.addEventListener('click', toggleAreaEdit);
        document
          .getElementById('bili-area-save-btn')
          ?.addEventListener('click', saveAreaEdit);
        document
          .getElementById('bili-area-cancel-btn')
          ?.addEventListener('click', cancelAreaEdit);
        document
          .getElementById('bili-danmaku-command-toggle')
          ?.addEventListener('change', toggleDanmakuCommand);
        document
          .getElementById('yt-channel-edit-btn')
          ?.addEventListener('click', toggleYtChannelEdit);
        document
          .getElementById('yt-channel-save-btn')
          ?.addEventListener('click', saveYtChannelEdit);
        document
          .getElementById('yt-channel-cancel-btn')
          ?.addEventListener('click', cancelYtChannelEdit);
        document
          .getElementById('yt-area-edit-btn')
          ?.addEventListener('click', toggleYtAreaEdit);
        document
          .getElementById('yt-area-save-btn')
          ?.addEventListener('click', saveYtAreaEdit);
        document
          .getElementById('yt-area-cancel-btn')
          ?.addEventListener('click', cancelYtAreaEdit);
        document
          .getElementById('yt-quality-edit-btn')
          ?.addEventListener('click', toggleYtQualityEdit);
        document
          .getElementById('yt-quality-save-btn')
          ?.addEventListener('click', saveYtQualityEdit);
        document
          .getElementById('yt-quality-cancel-btn')
          ?.addEventListener('click', cancelYtQualityEdit);
        document
          .getElementById('yt-crop-edit-btn')
          ?.addEventListener('click', () => openCropConfig('youtube'));
        document
          .getElementById('yt-crop-clear-btn')
          ?.addEventListener('click', () => clearCropConfig('youtube'));
        document
          .getElementById('yt-hls-cache-edit-btn')
          ?.addEventListener('click', toggleYtHlsCacheEdit);
        document
          .getElementById('yt-hls-cache-save-btn')
          ?.addEventListener('click', saveYtHlsCacheEdit);
        document
          .getElementById('yt-hls-cache-cancel-btn')
          ?.addEventListener('click', cancelYtHlsCacheEdit);
        document
          .getElementById('yt-hls-cache-enabled')
          ?.addEventListener('change', event => setHlsCacheLatencyInputState('yt', event.currentTarget.checked));
        document
          .getElementById('tw-channel-edit-btn')
          ?.addEventListener('click', toggleTwChannelEdit);
        document
          .getElementById('tw-channel-save-btn')
          ?.addEventListener('click', saveTwChannelEdit);
        document
          .getElementById('tw-channel-cancel-btn')
          ?.addEventListener('click', cancelTwChannelEdit);
        document
          .getElementById('tw-area-edit-btn')
          ?.addEventListener('click', toggleTwAreaEdit);
        document
          .getElementById('tw-area-save-btn')
          ?.addEventListener('click', saveTwAreaEdit);
        document
          .getElementById('tw-area-cancel-btn')
          ?.addEventListener('click', cancelTwAreaEdit);
        document
          .getElementById('tw-quality-edit-btn')
          ?.addEventListener('click', toggleTwQualityEdit);
        document
          .getElementById('tw-quality-save-btn')
          ?.addEventListener('click', saveTwQualityEdit);
        document
          .getElementById('tw-quality-cancel-btn')
          ?.addEventListener('click', cancelTwQualityEdit);
        document
          .getElementById('tw-crop-edit-btn')
          ?.addEventListener('click', () => openCropConfig('twitch'));
        document
          .getElementById('tw-crop-clear-btn')
          ?.addEventListener('click', () => clearCropConfig('twitch'));
        document
          .getElementById('tw-hls-cache-edit-btn')
          ?.addEventListener('click', toggleTwHlsCacheEdit);
        document
          .getElementById('tw-hls-cache-save-btn')
          ?.addEventListener('click', saveTwHlsCacheEdit);
        document
          .getElementById('tw-hls-cache-cancel-btn')
          ?.addEventListener('click', cancelTwHlsCacheEdit);
        document
          .getElementById('tw-hls-cache-enabled')
          ?.addEventListener('change', event => setHlsCacheLatencyInputState('tw', event.currentTarget.checked));
      }

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

      function initLogControls() {
        document
          .getElementById('clear-logs-btn')
          ?.addEventListener('click', clearLogs);
        document
          .getElementById('refresh-logs-btn')
          ?.addEventListener('click', refreshLogs);
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

      function initCropModalControls() {
        document
          .getElementById('cropPlatform')
          ?.addEventListener('change', loadCurrentCropSettings);
        document
          .getElementById('cropImageUpload')
          ?.addEventListener('change', loadCropImage);
        document
          .querySelectorAll('[data-crop-ratio]')
          .forEach(button => {
            button.addEventListener('click', () => selectAspectRatio(button.dataset.cropRatio));
          });
        document
          .getElementById('crop-disable-btn')
          ?.addEventListener('click', disableCrop);
        document
          .getElementById('crop-apply-restart-btn')
          ?.addEventListener('click', applyCropAndRestart);
        document
          .getElementById('crop-apply-btn')
          ?.addEventListener('click', applyCrop);
        document
          .getElementById('crop-close-btn')
          ?.addEventListener('click', closeCropModal);
      }

      function initAreaModalControls() {
        document
          .getElementById('confirm-area-selection-btn')
          ?.addEventListener('click', confirmAreaSelection);
        document
          .getElementById('cancel-area-selection-btn')
          ?.addEventListener('click', closeAreaModal);
      }

      function isElementHidden(element) {
        return !element || getComputedStyle(element).display === 'none';
      }

      let editingAreaId = null;
      let isEditingChannel = false;

      function submitAreaForm() {
        if (editingAreaId !== null) {
          return updateArea(editingAreaId);
        }
        return addArea();
      }

      function submitChannelForm() {
        if (isEditingChannel) {
          return updateChannel();
        }
        return addChannel();
      }

      function initManagementControls() {
        document
          .getElementById('area-submit-btn')
          ?.addEventListener('click', submitAreaForm);
        document
          .getElementById('area-clear-btn')
          ?.addEventListener('click', clearAreaForm);
        document
          .getElementById('channel-submit-btn')
          ?.addEventListener('click', submitChannelForm);
        document
          .getElementById('channel-clear-btn')
          ?.addEventListener('click', clearChannelForm);
        document
          .getElementById('refreshAreasBtn')
          ?.addEventListener('click', refreshAreas);
        document
          .getElementById('refreshChannelsBtn')
          ?.addEventListener('click', refreshChannels);
      }

      function initHolodexLoginModalControls() {
        document
          .getElementById('holodex-save-api-key-btn')
          ?.addEventListener('click', saveHolodexApiKey);
        document
          .getElementById('holodex-test-api-key-btn')
          ?.addEventListener('click', testHolodexApiKey);

        const modal = document.getElementById('holodex-login-modal');
        modal?.addEventListener('click', event => {
          if (event.target === modal) {
            closeHolodexLoginModal();
          }
        });

        document.addEventListener('keydown', event => {
          if (event.key === 'Escape' && modal?.classList.contains('active')) {
            closeHolodexLoginModal();
          }
        });

        document
          .getElementById('holodex-login-close-btn')
          ?.addEventListener('click', closeHolodexLoginModal);
        document
          .getElementById('holodex-cancel-login-btn')
          ?.addEventListener('click', closeHolodexLoginModal);
        document
          .getElementById('holodex-open-login-btn')
          ?.addEventListener('click', openHolodexLogin);
        document
          .getElementById('holodex-save-jwt-btn')
          ?.addEventListener('click', saveHolodexJwt);
        document
          .getElementById('holodex-logout-btn')
          ?.addEventListener('click', logoutHolodexJwt);
        document
          .getElementById('holodex-favorites-btn')
          ?.addEventListener('click', openHolodexLoginModal);
        document
          .getElementById('holodex-login-btn')
          ?.addEventListener('click', openHolodexLoginModal);
        document
          .getElementById('holodex-use-favorites-toggle')
          ?.addEventListener('change', toggleHolodexFavoritesMode);
        document
          .getElementById('holodex-skip-jwt-verify')
          ?.addEventListener('change', toggleHolodexSkipJwtVerify);
      }

      function initFaceAuthModalControls() {
        const modal = document.getElementById('face-auth-modal');
        modal?.addEventListener('click', event => {
          if (event.target === modal) {
            closeFaceAuthModal();
          }
        });

        document.addEventListener('keydown', event => {
          if (event.key === 'Escape' && modal?.classList.contains('active')) {
            closeFaceAuthModal();
          }
        });

        document
          .getElementById('face-auth-retry-btn')
          ?.addEventListener('click', retryStartStream);
        document
          .getElementById('face-auth-close-btn')
          ?.addEventListener('click', closeFaceAuthModal);
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

      // Server-sent events: the backend pushes named events whenever state
      // changes, so the dashboard updates immediately instead of waiting for
      // the next poll. Polling stays active as a fallback.
      let dashboardEventSource = null;
      let lastStatusRefreshMs = 0;

      // While the event stream is healthy the server pushes changes, so the
      // status poll only acts as a slow safety net.
      function eventStreamHealthy() {
        return !!dashboardEventSource
          && dashboardEventSource.readyState === EventSource.OPEN;
      }

      function initEventStream() {
        if (!webUiAccessReady || !window.EventSource || dashboardEventSource) {
          return;
        }

        dashboardEventSource = new EventSource(eventStreamUrl());
        dashboardEventSource.addEventListener('status', () => {
          if (isDashboardVisible()) {
            refreshStatus();
          }
        });
        dashboardEventSource.addEventListener('config', () => {
          if (isDashboardVisible()) {
            reloadServerConfig();
            refreshStatus();
          }
        });
        dashboardEventSource.addEventListener('refresh', () => {
          if (isDashboardVisible()) {
            reloadServerConfig();
            refreshStatus();
          }
        });
      }

      // Top-level navigation. Each view owns its data, so switching to one
      // loads whatever it needs the first time it is shown.
      const VIEW_IDS = ['overview', 'manage', 'settings', 'logs'];
      const viewsLoaded = new Set();
      let activeView = 'overview';

      function isViewActive(name) {
        return activeView === name;
      }

      function loadViewData(name) {
        switch (name) {
          case 'overview':
            maybeLoadHolodexStreams();
            break;
          case 'manage':
            loadManagementListsOnce();
            break;
          case 'settings':
            loadSystemConfig();
            break;
          case 'logs':
            refreshLogs();
            break;
        }
      }

      function activateView(name, options = {}) {
        if (!VIEW_IDS.includes(name)) {
          name = 'overview';
        }

        activeView = name;

        for (const id of VIEW_IDS) {
          const panel = document.getElementById(`view-${id}`);
          const tab = document.getElementById(`tab-${id}`);
          const selected = id === name;

          panel?.classList.toggle('is-active', selected);
          if (tab) {
            tab.classList.toggle('is-active', selected);
            tab.setAttribute('aria-selected', selected ? 'true' : 'false');
          }
        }

        try {
          localStorage.setItem('activeView', name);
        } catch (error) {
          // Storage can be unavailable in private windows; navigation still works.
        }

        if (!viewsLoaded.has(name)) {
          viewsLoaded.add(name);
          loadViewData(name);
        } else if (options.reload) {
          loadViewData(name);
        }
      }

      function initViewRouter() {
        const tabs = Array.from(document.querySelectorAll('.tab[data-view]'));

        tabs.forEach((tab, index) => {
          tab.addEventListener('click', () => activateView(tab.dataset.view));
          tab.addEventListener('keydown', event => {
            const offset = event.key === 'ArrowRight' ? 1 : event.key === 'ArrowLeft' ? -1 : 0;
            if (!offset) return;

            event.preventDefault();
            const next = tabs[(index + offset + tabs.length) % tabs.length];
            next.focus();
            activateView(next.dataset.view);
          });
        });

        let saved = null;
        try {
          saved = localStorage.getItem('activeView');
        } catch (error) {
          // Ignore unavailable storage and fall back to the default view.
        }

        activateView(saved || 'overview');
      }

      // Refresh logs only while the dashboard is visible.
      startLogRefresh();
      initEventStream();
      initHolodexFold();
      initViewRouter();
      initDashboardControls();
      initAntiCollisionControls();
      initSystemSettingsActions();
      initLogControls();
      initFooterUpdateControls();
      initThemeControls();
      initSetupControls();
      initCropModalControls();
      initAreaModalControls();
      initManagementControls();
      initHolodexLoginModalControls();
      initFaceAuthModalControls();

      document.addEventListener('visibilitychange', () => {
        if (isDashboardVisible()) {
          if (isViewActive('logs')) {
            refreshLogs();
          }
          refreshStatus();
        }
      });

      // Load config and set up status refresh interval
      async function initStatusRefresh() {
        try {
          const config = await getJson('/api/config');

          // Store config globally and apply config-driven controls once.
          mergeConfigData(config);
          updateMonitorToggleStates(config);
          updateDanmakuCommandToggle(config.bilibili?.enable_danmaku_command !== false);

          const holodexApiKeyConfigured = !!config.holodex_api_key?.trim();
          applyHolodexSectionVisibility(holodexApiKeyConfigured);
          if (holodexApiKeyConfigured) {
            loadHolodexAuthStatus();
          }

          if (config.interval) {
            statusRefreshInterval = config.interval * 1000; // Convert to milliseconds
          }
        } catch (error) {
          console.warn('Failed to load config, using default interval:', error);
        }

        // Start status refresh with configured interval
        if (statusIntervalId) {
          clearInterval(statusIntervalId);
        }
        statusIntervalId = setInterval(() => {
          if (!isDashboardVisible()) {
            return;
          }
          if (eventStreamHealthy()
            && Date.now() - lastStatusRefreshMs < statusRefreshInterval * 5) {
            return;
          }
          refreshStatus();
        }, statusRefreshInterval);

        if (networkRefreshIntervalId) {
          clearInterval(networkRefreshIntervalId);
        }
        networkRefreshIntervalId = setInterval(() => {
          if (isDashboardVisible()) {
            refreshNetworkStatus();
          }
        }, networkRefreshInterval);

        // Initial refresh
        refreshStatus();
        refreshNetworkStatus();

        let titleRowCenterTimer;
        window.addEventListener('resize', () => {
          clearTimeout(titleRowCenterTimer);
          titleRowCenterTimer = setTimeout(schedulePlatformTitleRowCenters, 100);
        });

      }

      function setElementDisplay(element, visible, display = 'block') {
        if (element) {
          element.style.display = visible ? display : 'none';
        }
      }

      function applyHolodexSectionVisibility(apiKeyConfigured) {
        setElementDisplay(document.getElementById('holodex-section'), true);
        setElementDisplay(document.getElementById('holodex-init-hint'), false);
        setElementDisplay(document.getElementById('holodex-api-config'), !apiKeyConfigured);
        setElementDisplay(document.getElementById('holodex-streams-section'), apiKeyConfigured);
        setElementDisplay(document.getElementById('holodex-login-btn'), apiKeyConfigured, 'inline-flex');
        maybeLoadHolodexStreams();
      }

      // The stream list is only worth fetching once the API key is known to be
      // configured, the overview is the visible view, and the panel is open.
      function maybeLoadHolodexStreams() {
        if (holodexStreamsRequested || !isViewActive('overview')) {
          return;
        }

        const section = document.getElementById('holodex-section');
        if (section?.classList.contains('is-collapsed')) {
          return;
        }

        const streams = document.getElementById('holodex-streams-section');
        if (!streams || getComputedStyle(streams).display === 'none') {
          return;
        }

        holodexStreamsRequested = true;
        refreshHolodexStreams();
      }

      function setHolodexCollapsed(collapsed) {
        const section = document.getElementById('holodex-section');
        const button = document.getElementById('holodexFoldBtn');
        if (!section) return;

        section.classList.toggle('is-collapsed', collapsed);
        if (button) {
          const label = collapsed ? '展开' : '折叠';
          button.setAttribute('aria-expanded', collapsed ? 'false' : 'true');
          button.setAttribute('aria-label', label);
          button.title = label;
        }
      }

      function toggleHolodexFold() {
        const section = document.getElementById('holodex-section');
        if (!section) return;

        const collapsed = !section.classList.contains('is-collapsed');
        setHolodexCollapsed(collapsed);
        try {
          localStorage.setItem('holodexCollapsed', collapsed ? 'true' : 'false');
        } catch (error) {
          // The fold still works; it just will not be remembered.
        }

        if (!collapsed) {
          // Opening the panel is what triggers the first fetch.
          maybeLoadHolodexStreams();
        }
      }

      function initHolodexFold() {
        let collapsed = false;
        try {
          collapsed = localStorage.getItem('holodexCollapsed') === 'true';
        } catch (error) {
          // Ignore unavailable storage and start expanded.
        }
        setHolodexCollapsed(collapsed);

        document
          .getElementById('holodexFoldBtn')
          ?.addEventListener('click', toggleHolodexFold);

        const heading = document.getElementById('holodex-heading');
        heading?.addEventListener('click', toggleHolodexFold);
      }

      const HOLODEX_STATUS_STATE_CLASSES = [
        'holodex-status-loading',
        'holodex-status-warning',
        'holodex-status-success',
        'holodex-status-error'
      ];

      function setHolodexStatus(statusDiv, message, stateClass) {
        statusDiv.classList.remove(...HOLODEX_STATUS_STATE_CLASSES);
        statusDiv.classList.add(stateClass);
        statusDiv.style.display = 'block';
        statusDiv.textContent = message;
      }

      function hideHolodexStatus(statusDiv) {
        statusDiv.classList.remove(...HOLODEX_STATUS_STATE_CLASSES);
        statusDiv.style.display = 'none';
        statusDiv.textContent = '';
      }

      function createHolodexScheduleDivider() {
        const divider = document.createElement('div');
        divider.className = 'holodex-schedule-divider';

        const label = document.createElement('span');
        label.className = 'holodex-schedule-divider-label';
        label.textContent = '预告';

        divider.appendChild(label);
        return divider;
      }

      async function refreshHolodexStreams() {
        // Start continuous spinning animation
        const button = document.getElementById('refreshHolodexBtn');
        const icon = document.getElementById('refreshHolodexIcon');
        setButtonLoading(button, icon, true);

        const statusDiv = document.getElementById('holodex-status');
        const streamsDiv = document.getElementById('holodex-streams');
        if (!statusDiv || !streamsDiv) {
          setButtonLoading(button, icon, false);
          return;
        }

        stopHolodexDurationTicker();
        setHolodexStatus(statusDiv, '⏳ 加载中...', 'holodex-status-loading');
        streamsDiv.replaceChildren();

        try {
          const favoritesParam = holodexUseFavorites ? 'true' : 'false';
          const data = await getJson(`/api/holodex/streams?favorites=${favoritesParam}`);

          if (!data.success) {
            setHolodexStatus(statusDiv, `⚠️ ${data.message}`, 'holodex-status-warning');
            return;
          }

          const streams = data.data || [];
          const isFavorites = data.source === 'favorites';
          holodexCurrentSource = data.source || (isFavorites ? 'favorites' : 'channels');

          if (isFavorites) {
            try {
              await refreshHolodexChannelsData();
            } catch (error) {
              console.error('Failed to load channels.json for Holodex add controls:', error);
              holodexCurrentSource = 'channels';
              showNotification('加载 channels.json 失败，无法显示添加按钮', 'error');
            }
          }

          // Separate live and scheduled streams
          const liveStreams = streams.filter(s => s.status === 'live');
          const scheduledStreams = streams.filter(s => s.status !== 'live');

          if (streams.length === 0) {
            const emptyMessage = isFavorites
              ? '✅ 收藏夹 - 当前无直播或预告'
              : '当前无直播或预告';
            setHolodexStatus(statusDiv, emptyMessage, 'holodex-status-success');
            return;
          }

          // Sort scheduled streams by time - nearest first
          scheduledStreams.sort((a, b) => {
            const timeA = getHolodexStreamStartMs(a, false) ?? Infinity;
            const timeB = getHolodexStreamStartMs(b, false) ?? Infinity;
            return timeA - timeB;
          });

          hideHolodexStatus(statusDiv);

          // Render live streams first
          liveStreams.forEach(stream => {
            streamsDiv.appendChild(createStreamCard(stream, true));
          });

          // Add divider if both live and scheduled exist
          if (liveStreams.length > 0 && scheduledStreams.length > 0) {
            streamsDiv.appendChild(createHolodexScheduleDivider());
          }

          // Render scheduled streams (now sorted by time)
          scheduledStreams.forEach(stream => {
            streamsDiv.appendChild(createStreamCard(stream, false));
          });

          startHolodexDurationTicker();

        } catch (error) {
          setHolodexStatus(statusDiv, `❌ 请求失败: ${error.message}`, 'holodex-status-error');
        } finally {
          // Stop spinning animation when complete
          setButtonLoading(button, icon, false);
        }
      }

      let holodexDurationIntervalId = null;
      const SVG_NS = 'http://www.w3.org/2000/svg';

      function getHolodexPlaceholderKind(stream) {
        const link = (stream.external_link || '').toLowerCase();
        if (link.includes('twitch.tv')) {
          return 'twitch';
        }
        return 'radio';
      }

      function createSvgIcon(viewBox, pathData, className = '') {
        const svg = document.createElementNS(SVG_NS, 'svg');
        svg.setAttribute('viewBox', viewBox);
        svg.setAttribute('aria-hidden', 'true');
        if (className) {
          svg.classList.add(className);
        }

        const paths = Array.isArray(pathData) ? pathData : [pathData];
        paths.forEach(pathDefinition => {
          const path = document.createElementNS(SVG_NS, 'path');
          if (typeof pathDefinition === 'string') {
            path.setAttribute('d', pathDefinition);
          } else {
            Object.entries(pathDefinition).forEach(([name, value]) => {
              path.setAttribute(name, value);
            });
          }
          svg.appendChild(path);
        });
        return svg;
      }

      function createHolodexStreamSvg(pathData) {
        const svg = createSvgIcon('0 0 24 24', pathData);
        svg.setAttribute('width', '14');
        svg.setAttribute('height', '14');
        svg.setAttribute('fill', 'none');
        svg.setAttribute('stroke', 'currentColor');
        svg.setAttribute('stroke-width', '2');
        svg.setAttribute('stroke-linecap', 'round');
        svg.setAttribute('stroke-linejoin', 'round');
        return svg;
      }

      function createHolodexPlaceholderIcon(kind) {
        const iconClass = kind === 'twitch'
          ? 'holodex-duration-twitch-icon'
          : 'holodex-duration-radio-icon';
        const wrapper = document.createElement('span');
        wrapper.className = iconClass;
        wrapper.appendChild(kind === 'twitch'
          ? createSvgIcon('0 0 24 24', 'M11.64 5.93H13.07V10.21H11.64M15.57 5.93H17V10.21H15.57M7 2L3.43 5.57V18.43H7.71V22L11.29 18.43H14.14L20.57 12V2M19.14 11.29L16.29 14.14H13.43L10.93 16.64V14.14H7.71V3.43H19.14Z')
          : createSvgIcon('0 0 24 24', 'M12 10C10.9 10 10 10.9 10 12S10.9 14 12 14 14 13.1 14 12 13.1 10 12 10M18 12C18 8.7 15.3 6 12 6S6 8.7 6 12C6 14.2 7.2 16.1 9 17.2L10 15.5C8.8 14.8 8 13.5 8 12.1C8 9.9 9.8 8.1 12 8.1S16 9.9 16 12.1C16 13.6 15.2 14.9 14 15.5L15 17.2C16.8 16.2 18 14.2 18 12M12 2C6.5 2 2 6.5 2 12C2 15.7 4 18.9 7 20.6L8 18.9C5.6 17.5 4 14.9 4 12C4 7.6 7.6 4 12 4S20 7.6 20 12C20 15 18.4 17.5 16 18.9L17 20.6C20 18.9 22 15.7 22 12C22 6.5 17.5 2 12 2Z')
        );
        return wrapper;
      }

      function createHolodexPlaceholderDurationOverlay(stream, isLive) {
        const kind = getHolodexPlaceholderKind(stream);
        const durationClass = kind === 'twitch'
          ? 'holodex-stream-duration holodex-stream-duration-twitch'
          : 'holodex-stream-duration holodex-stream-duration-radio';
        const hoverText = kind === 'twitch' ? '外部配信' : '外部直播';
        const startMs = getHolodexStreamStartMs(stream, isLive);
        const duration = document.createElement('div');
        duration.className = durationClass;
        let hasDurationText = false;

        if (isLive && startMs) {
          duration.dataset.tick = 'live';
          duration.dataset.startMs = String(startMs);

          const text = document.createElement('span');
          text.className = 'holodex-duration-text';
          text.textContent = formatHolodexDuration(Date.now() - startMs);
          duration.appendChild(text);
          hasDurationText = true;
        }

        if (!hasDurationText && stream.start_scheduled) {
          const start = new Date(stream.start_scheduled);
          const clock = Number.isNaN(start.getTime())
            ? '预告'
            : `${String(start.getHours()).padStart(2, '0')}:${String(start.getMinutes()).padStart(2, '0')}`;
          const text = document.createElement('span');
          text.className = 'holodex-duration-text';
          text.textContent = clock;
          duration.appendChild(text);
        }

        const hover = document.createElement('span');
        hover.className = 'holodex-duration-hover';
        hover.textContent = hoverText;
        duration.append(hover, createHolodexPlaceholderIcon(kind));
        return duration;
      }

      function createHolodexDurationOverlay(stream, isLive, isPlaceholder) {
        if (isPlaceholder) {
          return createHolodexPlaceholderDurationOverlay(stream, isLive);
        }

        const startMs = getHolodexStreamStartMs(stream, isLive);
        if (isLive && startMs) {
          const duration = document.createElement('div');
          duration.className = 'holodex-stream-duration holodex-stream-duration-live';
          duration.dataset.tick = 'live';
          duration.dataset.startMs = String(startMs);

          const text = document.createElement('span');
          text.className = 'holodex-duration-text';
          text.textContent = formatHolodexDuration(Date.now() - startMs);
          duration.appendChild(text);
          return duration;
        }

        return null;
      }

      function escapeHolodexHtml(text) {
        return String(text ?? '')
          .replace(/&/g, '&amp;')
          .replace(/</g, '&lt;')
          .replace(/>/g, '&gt;')
          .replace(/"/g, '&quot;');
      }

      function formatHolodexScheduledStart(startScheduled) {
        const start = new Date(startScheduled);
        if (Number.isNaN(start.getTime())) {
          return '预告';
        }

        const now = Date.now();
        const diffMs = start.getTime() - now;
        const clock = `${String(start.getHours()).padStart(2, '0')}:${String(start.getMinutes()).padStart(2, '0')}`;

        if (diffMs <= 0) {
          return `即将开播 (${clock})`;
        }

        const diffMinutes = diffMs / (1000 * 60);
        if (diffMinutes < 60) {
          const minutes = Math.max(1, Math.ceil(diffMinutes));
          return `将在 ${minutes} 分钟内开播 (${clock})`;
        }

        const diffHours = diffMs / (1000 * 60 * 60);
        if (diffHours < 24) {
          const hours = Math.ceil(diffHours);
          return `将在 ${hours} 小时内开播 (${clock})`;
        }

        const y = start.getFullYear();
        const m = start.getMonth() + 1;
        const d = start.getDate();
        return `将在 ${y}/${m}/${d}开播 (${clock})`;
      }

      function formatHolodexDuration(ms) {
        const totalSec = Math.max(0, Math.floor(ms / 1000));
        const h = Math.floor(totalSec / 3600);
        const m = Math.floor((totalSec % 3600) / 60);
        const s = totalSec % 60;
        if (h > 0) {
          return `${h}:${String(m).padStart(2, '0')}:${String(s).padStart(2, '0')}`;
        }
        return `${m}:${String(s).padStart(2, '0')}`;
      }

      function getHolodexStreamStartMs(stream, isLive) {
        const raw = isLive
          ? (stream.start_actual || stream.available_at || stream.published_at || stream.start_scheduled)
          : stream.start_scheduled;
        if (!raw) return null;
        const ms = new Date(raw).getTime();
        return Number.isNaN(ms) ? null : ms;
      }

      function stopHolodexDurationTicker() {
        if (holodexDurationIntervalId) {
          clearInterval(holodexDurationIntervalId);
          holodexDurationIntervalId = null;
        }
      }

      function updateHolodexDurations() {
        const now = Date.now();
        document.querySelectorAll('.holodex-stream-duration[data-tick="live"]').forEach((el) => {
          const startMs = Number(el.dataset.startMs);
          if (!startMs) return;
          const textEl = el.querySelector('.holodex-duration-text');
          if (textEl) {
            textEl.textContent = formatHolodexDuration(now - startMs);
          }
        });
      }

      function startHolodexDurationTicker() {
        stopHolodexDurationTicker();
        updateHolodexDurations();
        if (document.querySelector('.holodex-stream-duration[data-tick="live"]')) {
          holodexDurationIntervalId = setInterval(updateHolodexDurations, 1000);
        }
      }

      function getHolodexChannelUrl(channelId) {
        if (!channelId) return '';
        return `https://holodex.net/channel/${encodeURIComponent(channelId)}`;
      }

      function getHolodexChannelPhotoUrl(stream) {
        if (stream.channel_photo) {
          return stream.channel_photo;
        }
        if (!stream.channel_id) return '';
        return `https://holodex.net/statics/channelImg/${encodeURIComponent(stream.channel_id)}/50.png`;
      }

      function createHolodexAvatarBlock(stream) {
        if (!stream.channel_id) return null;
        const holodexUrl = getHolodexChannelUrl(stream.channel_id);
        const photoUrl = getHolodexChannelPhotoUrl(stream);

        const avatar = document.createElement('a');
        avatar.className = 'holodex-stream-avatar';
        avatar.href = holodexUrl;
        avatar.target = '_blank';
        avatar.rel = 'noopener noreferrer';
        avatar.title = stream.channel_name || 'channel';

        const image = document.createElement('img');
        image.src = photoUrl;
        image.alt = '';
        image.loading = 'lazy';
        avatar.appendChild(image);
        return avatar;
      }

      async function refreshHolodexChannelsData() {
        const result = await managementRequest('/api/manage/channels');
        if (!result.success || !result.data) {
          throw new Error(result.message || 'Failed to load channels');
        }
        channelsData = result.data;
        return channelsData;
      }

      function normalizeHolodexChannelValue(value) {
        return String(value || '').trim().toLowerCase();
      }

      function holodexStreamHasConfiguredChannel(stream) {
        if (!channelsData || !Array.isArray(channelsData.channels)) return false;

        const streamName = normalizeHolodexChannelValue(stream.channel_name);
        const youtubeId = normalizeHolodexChannelValue(stream.channel_id);
        const twitchId = normalizeHolodexChannelValue(parseTwitchLoginFromLink(stream.external_link));

        return channelsData.channels.some(channel => {
          const platforms = channel.platforms || {};
          return (streamName && normalizeHolodexChannelValue(channel.name) === streamName)
            || (youtubeId && normalizeHolodexChannelValue(platforms.youtube) === youtubeId)
            || (twitchId && normalizeHolodexChannelValue(platforms.twitch) === twitchId);
        });
      }

      function createHolodexChannelAddIcon(pathData, title) {
        const button = document.createElement('button');
        button.type = 'button';
        button.className = 'holodex-channel-add-btn';
        button.title = title;
        button.appendChild(createSvgIcon('0 0 24 24', pathData));
        return button;
      }

      function createHolodexAddChannelControls(stream) {
        if (holodexCurrentSource !== 'favorites' || !stream.channel_name || holodexStreamHasConfiguredChannel(stream)) {
          return null;
        }

        const youtubeId = stream.channel_id || '';
        const twitchId = parseTwitchLoginFromLink(stream.external_link);
        if (!youtubeId && !twitchId) return null;

        const control = document.createElement('span');
        control.className = 'holodex-channel-add';
        control.dataset.channelName = stream.channel_name;
        control.dataset.youtubeId = youtubeId;
        control.dataset.twitchId = twitchId;

        const startButton = document.createElement('button');
        startButton.type = 'button';
        startButton.className = 'holodex-channel-add-btn holodex-add-channel-start';
        startButton.title = '添加到 channels.json';
        startButton.setAttribute('aria-label', '添加到 channels.json');
        startButton.textContent = '➕';

        const actions = document.createElement('span');
        actions.className = 'holodex-channel-add-actions';
        actions.setAttribute('aria-label', '确认添加频道');

        const confirmButton = createHolodexChannelAddIcon('M20 6 9 17l-5-5', '确认添加');
        confirmButton.classList.add('holodex-add-channel-confirm');

        const rejectButton = createHolodexChannelAddIcon('M18 6 6 18M6 6l12 12', '取消');
        rejectButton.classList.add('holodex-add-channel-reject');

        actions.append(confirmButton, rejectButton);
        control.append(startButton, actions);
        return control;
      }

      function createHolodexChannelBlock(stream) {
        if (!stream.channel_name) return null;
        const holodexUrl = getHolodexChannelUrl(stream.channel_id);
        const row = document.createElement('div');
        row.className = 'holodex-stream-channel-row';

        const channel = document.createElement(holodexUrl ? 'a' : 'p');
        channel.className = 'holodex-stream-channel';
        channel.textContent = stream.channel_name;
        if (holodexUrl) {
          channel.href = holodexUrl;
          channel.target = '_blank';
          channel.rel = 'noopener noreferrer';
        }
        row.appendChild(channel);

        const addControls = createHolodexAddChannelControls(stream);
        if (addControls) {
          row.appendChild(addControls);
        }
        return row;
      }

      function createHolodexStreamActionButton(extraClasses, streamActionData, icon) {
        const button = document.createElement('button');
        button.type = 'button';
        button.className = `holodex-stream-btn ${extraClasses}`;
        button.dataset.platform = streamActionData.platform;
        button.dataset.channelId = streamActionData.channelId;
        button.dataset.twitchId = streamActionData.twitchId;
        button.dataset.externalLink = streamActionData.externalLink;
        button.dataset.suggestedAreaId = streamActionData.suggestedAreaId;
        button.dataset.title = streamActionData.title;
        button.dataset.topicId = streamActionData.topicId;
        button.dataset.status = streamActionData.status;

        const label = document.createElement('span');
        label.textContent = '切换';
        button.append(icon, label);
        return button;
      }

      function readHolodexStreamActionData(button) {
        return {
          channelId: button.dataset.channelId || '',
          suggestedAreaId: button.dataset.suggestedAreaId ? parseInteger(button.dataset.suggestedAreaId, 0) || null : null,
          title: button.dataset.title || '',
          topicId: button.dataset.topicId || '',
          status: button.dataset.status || '',
          platform: button.dataset.platform || 'youtube',
          twitchChannelId: button.dataset.twitchId || '',
          externalLink: button.dataset.externalLink || ''
        };
      }

      function parseTwitchLoginFromLink(link) {
        if (!link) return '';
        const prefixes = [
          'https://www.twitch.tv/',
          'https://twitch.tv/',
          'http://www.twitch.tv/',
          'http://twitch.tv/'
        ];
        for (const prefix of prefixes) {
          if (link.startsWith(prefix)) {
            const rest = link.slice(prefix.length).split(/[/?#]/)[0].trim();
            if (rest) return rest;
          }
        }
        return '';
      }

      function createStreamCard(stream, isLive) {
        const streamCard = document.createElement('div');
        streamCard.className = 'holodex-stream-card';

        const isPlaceholder = stream.is_placeholder || stream.stream_type === 'placeholder';
        const placeholderKind = isPlaceholder ? getHolodexPlaceholderKind(stream) : '';
        const platform = isPlaceholder ? (placeholderKind === 'twitch' ? 'twitch' : 'external') : 'youtube';
        const twitchChannelId = isPlaceholder ? parseTwitchLoginFromLink(stream.external_link) : '';
        const watchUrl = stream.external_link || `https://www.youtube.com/watch?v=${stream.id}`;
        const thumbUrl = stream.thumbnail || (isPlaceholder
          ? (placeholderKind === 'twitch' && twitchChannelId
            ? `https://static-cdn.jtvnw.net/previews-ttv/live_user_${twitchChannelId}-640x360.jpg`
            : '')
          : `https://i.ytimg.com/vi/${stream.id}/sddefault.jpg`);

        const viewers = stream.live_viewers ? `${stream.live_viewers.toLocaleString()} 观看` : '';

        const areaInfo = document.createElement('p');
        areaInfo.className = 'holodex-stream-area-hint';
        if (stream.suggested_area_id && stream.suggested_area_name) {
          areaInfo.textContent = `🎯 建议分区: ${stream.suggested_area_name}`;
        }

        const statusMeta = document.createElement('div');
        statusMeta.className = 'holodex-stream-meta';
        if (!isLive) {
          const scheduleText = stream.start_scheduled
            ? formatHolodexScheduledStart(stream.start_scheduled)
            : '预告';
          const scheduled = document.createElement('span');
          scheduled.className = 'holodex-stream-scheduled';
          scheduled.textContent = scheduleText;
          statusMeta.appendChild(scheduled);
        } else if (viewers) {
          const liveLabel = document.createElement('span');
          liveLabel.className = 'holodex-stream-live-label';
          liveLabel.textContent = '直播中';
          const viewerText = document.createElement('span');
          viewerText.textContent = `• ${viewers}`;
          statusMeta.append(liveLabel, viewerText);
        } else {
          const liveLabel = document.createElement('span');
          liveLabel.className = 'holodex-stream-live-label';
          liveLabel.textContent = '直播中';
          statusMeta.appendChild(liveLabel);
        }

        const streamActionData = {
          platform,
          channelId: stream.channel_id || '',
          twitchId: twitchChannelId,
          externalLink: stream.external_link || '',
          suggestedAreaId: stream.suggested_area_id || '',
          title: stream.title || '',
          topicId: stream.topic_id || '',
          status: stream.status || ''
        };

        const thumb = document.createElement('div');
        thumb.className = 'holodex-stream-thumb';
        const thumbLink = document.createElement('a');
        thumbLink.className = 'holodex-stream-thumb-link';
        thumbLink.href = watchUrl;
        thumbLink.target = '_blank';
        thumbLink.rel = 'noopener noreferrer';
        if (thumbUrl) {
          const image = document.createElement('img');
          image.src = thumbUrl;
          image.alt = '';
          thumbLink.appendChild(image);
        } else {
          const placeholder = document.createElement('div');
          placeholder.className = 'holodex-stream-thumb-placeholder';
          thumbLink.appendChild(placeholder);
        }

        const thumbTop = document.createElement('div');
        thumbTop.className = 'holodex-stream-thumb-top';
        if (stream.topic_id) {
          const topic = document.createElement('span');
          topic.className = 'holodex-stream-topic';
          topic.textContent = stream.topic_id;
          thumbTop.appendChild(topic);
        }

        thumb.append(thumbLink, thumbTop);
        const durationBlock = createHolodexDurationOverlay(stream, isLive, isPlaceholder);
        if (durationBlock) {
          const thumbBottom = document.createElement('div');
          thumbBottom.className = 'holodex-stream-thumb-bottom';
          thumbBottom.appendChild(durationBlock);
          thumb.appendChild(thumbBottom);
        }

        const body = document.createElement('div');
        body.className = 'holodex-stream-body';
        const contentRow = document.createElement('div');
        contentRow.className = 'holodex-stream-content-row';
        const avatar = createHolodexAvatarBlock(stream);
        if (avatar) {
          contentRow.appendChild(avatar);
        }

        const lines = document.createElement('div');
        lines.className = 'holodex-stream-lines';
        const title = document.createElement('h4');
        title.className = 'holodex-stream-title';
        title.textContent = stream.title || '';
        lines.appendChild(title);

        const channelBlock = createHolodexChannelBlock(stream);
        if (channelBlock) {
          lines.appendChild(channelBlock);
        }
        lines.appendChild(statusMeta);
        if (areaInfo.textContent) {
          lines.appendChild(areaInfo);
        }
        contentRow.appendChild(lines);

        const actions = document.createElement('div');
        actions.className = 'holodex-stream-actions';
        const watchLink = document.createElement('a');
        watchLink.className = 'holodex-stream-watch';
        watchLink.href = watchUrl;
        watchLink.target = '_blank';
        watchLink.rel = 'noopener noreferrer';
        watchLink.textContent = '📺 观看';
        actions.appendChild(watchLink);

        if (isLive) {
          actions.appendChild(createHolodexStreamActionButton(
            'holodex-stream-btn-crop crop-switch-button',
            streamActionData,
            createHolodexStreamSvg([
              { d: 'M6.13 1L6 16a2 2 0 0 0 2 2h15' },
              { d: 'M1 6.13L16 6a2 2 0 0 1 2 2v15' }
            ])
          ));
        }
        actions.appendChild(createHolodexStreamActionButton(
          'holodex-stream-btn-switch switch-button',
          streamActionData,
          createHolodexStreamSvg([
            { d: 'M22 12c0 6-4.39 10-9.806 10C7.792 22 4.24 19.665 3 16m-1-4C2 6 6.39 2 11.807 2C16.208 2 19.758 4.335 21 8' },
            { d: 'm7 17l-4-1l-1 4M17 7l4 1l1-4' }
          ])
        ));

        body.append(contentRow, actions);
        streamCard.append(thumb, body);

        return streamCard;
      }

      // Store pending switch data
      let pendingSwitchData = null;

      async function switchToHolodexStream(channelId, suggestedAreaId, title, topicId, status, platform, twitchChannelId, externalLink) {
        if (!suggestedAreaId) {
          // Show modal for area selection
          pendingSwitchData = { channelId, areaId: null, title, topicId, status, platform, twitchChannelId, externalLink };
          await showAreaModal();
        } else {
          // Direct switch with suggested area
          await performSwitch(channelId, suggestedAreaId, title, topicId, status, platform, twitchChannelId, externalLink);
        }
      }

      async function showAreaModal() {
        const modal = document.getElementById('area-modal');
        const select = document.getElementById('modal-area-select');
        if (!modal || !select) return;

        // Load areas if not already loaded
        if (!areasData) {
          try {
            areasData = normalizeAreaData(await getJson('/api/areas'));
          } catch (error) {
            showNotification('加载分区列表失败', 'error');
            return;
          }
        }

        // Populate select
        select.replaceChildren(createAreaOption('', '选择分区...'));
        appendAreaOptions(select, getAreaList(), true);

        modal.classList.remove('hidden');
      }

      function closeAreaModal() {
        document.getElementById('area-modal')?.classList.add('hidden');
        pendingSwitchData = null;
      }

      function createAreaOption(value, label) {
        const option = document.createElement('option');
        option.value = value;
        option.textContent = label;
        return option;
      }

      function normalizeAreaData(data) {
        return Array.isArray(data) ? { areas: data } : (data || { areas: [] });
      }

      function getAreaList(data = areasData) {
        return normalizeAreaData(data).areas || [];
      }

      function getSortedAreas(areas) {
        return [...areas].sort((a, b) => {
          if (a.id === 235) return -1;
          if (b.id === 235) return 1;
          return 0;
        });
      }

      function appendAreaOptions(select, areas, includeId = false) {
        getSortedAreas(areas).forEach(area => {
          const label = includeId ? `${area.name} (${area.id})` : area.name;
          select.appendChild(createAreaOption(area.id, label));
        });
      }

      function createPlatformChannelOption(channel, platform) {
        const platforms = channel.platforms || {};
        return createSelectOption(
          JSON.stringify({
            id: platforms[platform],
            name: channel.name
          }),
          channel.name
        );
      }

      function appendPlatformChannelOptions(select, platform) {
        if (!channelsData || !Array.isArray(channelsData.channels)) return;

        channelsData.channels.forEach(channel => {
          const platforms = channel.platforms || {};
          if (platforms[platform]) {
            select.appendChild(createPlatformChannelOption(channel, platform));
          }
        });
      }

      function showFaceAuthModal(qrUrl) {
        faceAuthUrl = qrUrl;
        const modal = document.getElementById('face-auth-modal');
        const container = document.getElementById('face-auth-qr-container');
        if (!modal || !container) return;

        // Generate QR code using QR Server API, with a clickable fallback if it fails.
        container.replaceChildren();

        const qrImg = document.createElement('img');
        qrImg.className = 'face-auth-qr-image';
        qrImg.src = `https://api.qrserver.com/v1/create-qr-code/?size=256x256&data=${encodeURIComponent(qrUrl)}`;
        qrImg.alt = 'Bilibili 人脸验证二维码';
        qrImg.addEventListener('error', () => {
          container.replaceChildren(createFaceAuthQrFallback(qrUrl));
        }, { once: true });

        container.appendChild(qrImg);
        modal.classList.add('active');
      }

      function createFaceAuthQrFallback(qrUrl) {
        const fallback = document.createElement('div');
        fallback.className = 'face-auth-fallback';

        const message = document.createElement('p');
        message.textContent = '无法生成二维码，请点击下方链接：';

        const link = document.createElement('a');
        link.href = qrUrl;
        link.target = '_blank';
        link.rel = 'noopener noreferrer';
        link.textContent = qrUrl;

        fallback.append(message, link);
        return fallback;
      }

      function closeFaceAuthModal() {
        const modal = document.getElementById('face-auth-modal');
        if (modal) {
          modal.classList.remove('active');
        }
        faceAuthUrl = null;
      }

      function retryStartStream() {
        closeFaceAuthModal();
        startStream();
      }

      async function confirmAreaSelection() {
        const select = document.getElementById('modal-area-select');
        const areaId = parseInteger(select.value, 0);

        if (!areaId) {
          showNotification('请选择分区', 'error');
          return;
        }

        if (!pendingSwitchData) {
          console.error('No pending switch data');
          closeAreaModal();
          return;
        }

        const channelId = pendingSwitchData.channelId;
        const { title, topicId, status, platform, twitchChannelId, externalLink } = pendingSwitchData;
        closeAreaModal();
        await performSwitch(channelId, areaId, title, topicId, status, platform, twitchChannelId, externalLink);
      }

      function createHolodexSwitchPayload(channelId, areaId, title, topicId, status, platform, twitchChannelId, externalLink) {
        return {
          channel_id: channelId,
          area_id: areaId,
          title: title || null,
          topic_id: topicId || null,
          status: status || null,
          platform: platform || 'youtube',
          twitch_channel_id: twitchChannelId || null,
          external_link: externalLink || null
        };
      }

      async function performSwitch(channelId, areaId, title, topicId, status, platform, twitchChannelId, externalLink) {
        try {
          const payload = createHolodexSwitchPayload(
            channelId,
            areaId,
            title,
            topicId,
            status,
            platform,
            twitchChannelId,
            externalLink
          );
          const result = await postJsonApi('/api/holodex/switch', payload);
          if (result.success) {
            showNotification(result.message || '频道切换成功', 'success');
            // Refresh status after switching
            setTimeout(refreshStatus, 1000);
          } else {
            showNotification(result.message || '频道切换失败', 'error');
          }
        } catch (error) {
          console.error('Switch error:', error);
          showNotification('频道切换失败: ' + error.message, 'error');
        }
      }

      async function cropAndSwitchToHolodexStream(channelId, suggestedAreaId, title, topicId, status, platform = 'youtube', twitchChannelId = '', externalLink = '') {
        // First, open crop modal and auto-capture
        document.getElementById('cropModal').classList.add('active');
        document.getElementById('cropPlatform').value = platform;

        showNotification('正在捕获直播帧...', 'info');

        try {
          const captureUrl = platform === 'twitch'
            ? `/api/crop/capture/twitch?channel_id=${encodeURIComponent(twitchChannelId)}`
            : `/api/crop/capture/youtube?channel_id=${encodeURIComponent(channelId)}`;
          const result = await postJsonApi(captureUrl);

          if (result.success && result.message) {
            // Load the captured image
            cropImage = new Image();
            cropImage.onload = function () {
              cropCanvas = document.getElementById('cropCanvas');
              cropCtx = cropCanvas.getContext('2d');

              // Show container FIRST so canvas has dimensions
              showCropCanvasContainer();

              // Set canvas size to image size
              cropCanvas.width = cropImage.width;
              cropCanvas.height = cropImage.height;

              // Fill with white background first
              cropCtx.fillStyle = 'white';
              cropCtx.fillRect(0, 0, cropCanvas.width, cropCanvas.height);

              // Draw image
              cropCtx.drawImage(cropImage, 0, 0);

              // Setup canvas interaction
              setupCanvasInteraction();

              showNotification('直播帧已捕获，请选择裁剪区域后点击"应用裁剪"，然后会自动切换频道', 'success');

              // Store the switch data for later use
              window.pendingHolodexSwitch = {
                channelId,
                suggestedAreaId,
                title,
                topicId,
                status,
                platform,
                twitchChannelId,
                externalLink
              };
            };
            cropImage.onerror = function (e) {
              console.error('Image load error:', e);
              showNotification('图片加载失败', 'error');
            };
            cropImage.src = result.message;
          } else {
            showNotification(result.message || '捕获失败，请确保直播正在进行', 'error');
          }
        } catch (error) {
          showNotification('捕获失败: ' + error.message, 'error');
        }
      }

      // Helper function to get area name by ID
      function getAreaName(areaId) {
        if (!areasData || !areasData.areas) return areaId.toString();
        const area = areasData.areas.find(a => a.id === areaId);
        return area ? area.name : areaId.toString();
      }

      // Holodex API Key Management Functions
      async function testHolodexApiKey() {
        const apiKeyInput = document.getElementById('holodex-api-input');
        const apiKey = apiKeyInput.value.trim();
        if (!apiKey) {
          showNotification('请输入 API Key', 'error');
          return;
        }
        try {
          const response = await fetch('https://holodex.net/api/v2/live?limit=1', {
            headers: { 'X-APIKEY': apiKey }
          });
          if (response.ok) {
            showNotification('Holodex API 连接成功', 'success');
          } else {
            const text = await response.text();
            showNotification('连接失败: ' + text, 'error');
          }
        } catch (error) {
          showNotification('连接失败: ' + error.message, 'error');
        }
      }

      let holodexAuthState = { logged_in: false, username: null, expired: false, skip_jwt_verify: false };
      let holodexUseFavorites = false;
      let holodexUseFavoritesInitialized = false;

      function updateHolodexAuthBar() {
        const authBar = document.getElementById('holodex-auth-bar');
        const loginBtn = document.getElementById('holodex-login-btn');
        const favoritesLabel = document.getElementById('holodex-favorites-label');
        const favoritesBtn = document.getElementById('holodex-favorites-btn');
        const heartIcon = document.querySelector('#holodex-favorites-btn .holodex-heart-icon');
        const favoritesMode = document.querySelector('.holodex-favorites-mode');
        const favoritesToggle = document.getElementById('holodex-use-favorites-toggle');
        const jwtSkip = document.querySelector('.holodex-jwt-skip');
        const skipToggle = document.getElementById('holodex-skip-jwt-verify');
        const logoutBtn = document.getElementById('holodex-logout-btn');

        if (skipToggle) {
          skipToggle.checked = !!holodexAuthState.skip_jwt_verify;
        }
        if (favoritesToggle) {
          favoritesToggle.checked = !!holodexUseFavorites;
        }

        const loggedIn = holodexAuthState.logged_in && !holodexAuthState.expired;
        setElementDisplay(authBar, true, 'inline-flex');
        setElementDisplay(loginBtn, false);
        setElementDisplay(heartIcon, loggedIn, 'inline-flex');
        setElementDisplay(favoritesMode, loggedIn, 'inline-flex');
        setElementDisplay(jwtSkip, false);

        if (loggedIn) {
          if (favoritesLabel) {
            favoritesLabel.textContent = holodexAuthState.username
              ? `${holodexAuthState.username}的收藏`
              : '收藏夹';
          }
          if (favoritesBtn) favoritesBtn.title = '管理 Holodex 登录';
        } else if (holodexAuthState.expired) {
          if (favoritesLabel) favoritesLabel.textContent = '⚠️ 重新登录';
          if (favoritesBtn) favoritesBtn.title = 'Holodex 登录已过期';
        } else {
          if (favoritesLabel) favoritesLabel.textContent = '🔗 使用Holodex收藏夹';
          if (favoritesBtn) favoritesBtn.title = '登录后使用 Holodex 收藏夹';
        }

        if (logoutBtn) {
          logoutBtn.classList.toggle('hidden', !holodexAuthState.logged_in);
        }
      }

      async function toggleHolodexFavoritesMode() {
        const toggle = document.getElementById('holodex-use-favorites-toggle');
        if (!holodexAuthState.logged_in || holodexAuthState.expired) {
          if (toggle) toggle.checked = false;
          openHolodexLoginModal();
          return;
        }

        holodexUseFavorites = toggle ? toggle.checked : !holodexUseFavorites;
        holodexUseFavoritesInitialized = true;
        updateHolodexAuthBar();
        await refreshHolodexStreams();
      }

      function openHolodexLoginModal() {
        const modal = document.getElementById('holodex-login-modal');
        if (!modal) return;
        updateHolodexModalHint();
        modal.classList.add('active');
        loadHolodexAuthStatus();
      }

      function closeHolodexLoginModal() {
        const modal = document.getElementById('holodex-login-modal');
        modal?.classList.remove('active');
      }

      function updateHolodexLoginButton() {
        updateHolodexAuthBar();
      }

      function updateHolodexModalHint() {
        const hint = document.getElementById('holodex-modal-auth-hint');
        if (!hint) return;

        if (holodexAuthState.logged_in && !holodexAuthState.expired) {
          hint.classList.add('hidden');
          hint.classList.remove('warning');
          return;
        }

        hint.classList.remove('hidden');

        if (holodexAuthState.expired) {
          hint.textContent = '⚠️ JWT 已过期，请重新登录 Holodex 并保存新的 token';
          hint.classList.add('warning');
        } else {
          hint.textContent = '未登录 — 当前使用 channels.json 频道列表 (YouTube + Twitch 外部配信)';
          hint.classList.remove('warning');
        }
      }

      async function toggleHolodexSkipJwtVerify() {
        const toggle = document.getElementById('holodex-skip-jwt-verify');
        if (!toggle) return;

        try {
          const data = await postJsonApi('/api/config', { holodex_skip_jwt_verify: toggle.checked });
          if (data.success) {
            holodexAuthState.skip_jwt_verify = toggle.checked;
            showNotification(toggle.checked ? '已跳过 JWT 校验' : '已启用 JWT 校验', 'success');
            await loadHolodexAuthStatus();
            await refreshHolodexStreams();
          } else {
            toggle.checked = !toggle.checked;
            showNotification('更新失败: ' + (data.message || '未知错误'), 'error');
          }
        } catch (error) {
          toggle.checked = !toggle.checked;
          showNotification('更新失败: ' + error.message, 'error');
        }
      }

      function openHolodexLogin() {
        window.open('https://holodex.net/login', '_blank', 'noopener,noreferrer');
      }

      async function loadHolodexAuthStatus() {
        try {
          const data = await getJson('/api/holodex/auth/status');
          if (!data.success) {
            holodexAuthState = { logged_in: false, username: null, expired: false, skip_jwt_verify: false };
            holodexUseFavorites = false;
            holodexUseFavoritesInitialized = false;
            updateHolodexLoginButton();
            updateHolodexModalHint();
            return;
          }
          const info = data.data || {};
          holodexAuthState = {
            logged_in: !!info.logged_in,
            username: info.username || null,
            expired: !!info.expired,
            skip_jwt_verify: !!info.skip_jwt_verify
          };
          if (holodexAuthState.logged_in && !holodexAuthState.expired) {
            if (!holodexUseFavoritesInitialized) {
              holodexUseFavorites = true;
              holodexUseFavoritesInitialized = true;
            }
          } else {
            holodexUseFavorites = false;
            holodexUseFavoritesInitialized = false;
          }
        } catch (error) {
          holodexAuthState = { logged_in: false, username: null, expired: false, skip_jwt_verify: false };
          holodexUseFavorites = false;
          holodexUseFavoritesInitialized = false;
        }
        updateHolodexLoginButton();
        updateHolodexModalHint();
      }

      async function saveHolodexJwt() {
        const jwtInput = document.getElementById('holodex-jwt-input');
        const jwt = jwtInput.value.trim().replace(/^BEARER\s+/i, '');
        if (!jwt) {
          showNotification('请输入 JWT', 'error');
          return;
        }
        try {
          const data = await postJsonApi('/api/config', { holodex_jwt: jwt });
          if (data.success) {
            showNotification('Holodex 登录已保存', 'success');
            jwtInput.value = '';
            holodexUseFavorites = true;
            holodexUseFavoritesInitialized = true;
            closeHolodexLoginModal();
            await loadHolodexAuthStatus();
            await refreshHolodexStreams();
          } else {
            showNotification('保存失败: ' + (data.message || '未知错误'), 'error');
          }
        } catch (error) {
          showNotification('保存失败: ' + error.message, 'error');
        }
      }

      async function logoutHolodexJwt() {
        try {
          const data = await postJsonApi('/api/config', { holodex_jwt: '' });
          if (data.success) {
            showNotification('已退出 Holodex 登录', 'success');
            holodexUseFavorites = false;
            holodexUseFavoritesInitialized = false;
            document.getElementById('holodex-jwt-input').value = '';
            closeHolodexLoginModal();
            await loadHolodexAuthStatus();
            await refreshHolodexStreams();
          } else {
            showNotification('退出失败: ' + (data.message || '未知错误'), 'error');
          }
        } catch (error) {
          showNotification('退出失败: ' + error.message, 'error');
        }
      }

      async function saveHolodexApiKey() {
        const apiKeyInput = document.getElementById('holodex-api-input');
        const apiKey = apiKeyInput.value.trim();

        if (!apiKey) {
          showNotification('请输入 API Key', 'error');
          return;
        }

        try {
          // Save the API key to config
          const data = await postJsonApi('/api/config', { holodex_api_key: apiKey });
          if (data.success) {
            showNotification('Holodex API Key 保存成功！', 'success');
            // Refresh the page to update the UI
            setTimeout(() => window.location.reload(), 1500);
          } else {
            showNotification('保存失败: ' + (data.message || '未知错误'), 'error');
          }
        } catch (error) {
          showNotification('保存失败: ' + error.message, 'error');
          console.error('Save API key error:', error);
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

      function formatHlsCacheStatus(enabled, latencySecs) {
        return enabled ? `${latencySecs || 8}秒` : '关闭';
      }

      function setHlsCacheLatencyInputState(platform, enabled) {
        const latencyGroup = document.getElementById(`${platform}-hls-cache-latency-group`);
        const latencyInput = document.getElementById(`${platform}-hls-cache-latency`);
        if (latencyGroup) {
          if (latencyGroup.classList.contains('hls-cache-latency-group')) {
            latencyGroup.classList.toggle('hidden', !enabled);
            latencyGroup.style.display = '';
          } else {
            latencyGroup.style.display = enabled ? 'flex' : 'none';
          }
        }
        if (latencyInput) {
          latencyInput.disabled = !enabled;
        }
      }

      function hlsCacheDom(prefix) {
        return {
          status: document.getElementById(`${prefix}-hls-cache-status`),
          editor: document.getElementById(`${prefix}-hls-cache-edit-container`),
          enabled: document.getElementById(`${prefix}-hls-cache-enabled`),
          latency: document.getElementById(`${prefix}-hls-cache-latency`)
        };
      }

      function setHlsCacheEditorValues(prefix, cache = {}) {
        const fields = hlsCacheDom(prefix);
        const enabled = !!cache.enabled;
        if (fields.enabled) {
          fields.enabled.checked = enabled;
        }
        if (fields.latency) {
          fields.latency.value = cache.latency_secs || 8;
        }
        setHlsCacheLatencyInputState(prefix, enabled);
      }

      function readHlsCacheEditorValues(prefix) {
        const fields = hlsCacheDom(prefix);
        return {
          enabled: !!fields.enabled?.checked,
          latencySecs: Math.min(60, Math.max(1, parseInteger(fields.latency?.value, 8)))
        };
      }

      async function toggleHlsCacheEdit(prefix, apiPlatform) {
        const fields = hlsCacheDom(prefix);
        showInfoRowEdit(fields.status, fields.editor);

        try {
          const result = await getJson(`/api/ffmpeg-cache/${apiPlatform}`);
          setHlsCacheEditorValues(prefix, result.data || {});
        } catch {
          setHlsCacheEditorValues(prefix, { enabled: false, latency_secs: 8 });
        }
      }

      function cancelHlsCacheEdit(prefix) {
        const fields = hlsCacheDom(prefix);
        hideInfoRowEdit(fields.status, fields.editor);
      }

      async function saveHlsCacheEdit(prefix, apiPlatform, label) {
        const { enabled, latencySecs } = readHlsCacheEditorValues(prefix);

        try {
          const result = await postJsonApi('/api/ffmpeg-cache/update', {
            platform: apiPlatform,
            enabled,
            latency_secs: latencySecs
          });
          if (!result.success) {
            showNotification(result.message || 'HLS 缓存配置保存失败', 'error');
            return;
          }

          const fields = hlsCacheDom(prefix);
          if (fields.status) {
            fields.status.textContent = formatHlsCacheStatus(enabled, latencySecs);
          }
          cancelHlsCacheEdit(prefix);
          showNotification(`${label} HLS 缓存已更新`, 'success');
        } catch (error) {
          showNotification('HLS 缓存配置保存失败: ' + error.message, 'error');
        }
      }

      function toggleYtHlsCacheEdit() {
        toggleHlsCacheEdit('yt', 'youtube');
      }

      function cancelYtHlsCacheEdit() {
        cancelHlsCacheEdit('yt');
      }

      function saveYtHlsCacheEdit() {
        return saveHlsCacheEdit('yt', 'youtube', 'YouTube');
      }

      function toggleTwHlsCacheEdit() {
        toggleHlsCacheEdit('tw', 'twitch');
      }

      function cancelTwHlsCacheEdit() {
        cancelHlsCacheEdit('tw');
      }

      function saveTwHlsCacheEdit() {
        return saveHlsCacheEdit('tw', 'twitch', 'Twitch');
      }

      async function loadSystemConfig() {
        try {
          const config = await getJson('/api/config');
          mergeConfigData(config);

          // Load basic settings
          setInputValue('config-interval', config.interval || 30);
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
          setInputValue('config-lol-interval', config.lol_monitor_interval || 1);
          toggleConfigRiotApiKey(); // Show/hide riot API fields based on checkbox

          // Load Twitch settings
          setInputValue('config-tw-region', (config.twitch && config.twitch.proxy_region) || 'asl');

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

          // Load banned keywords
          await loadBannedKeywords();

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

      async function loadBannedKeywords() {
        try {
          const data = await getJson('/api/banned-keywords');

          setInputValue('streaming-banned-keywords', (data.streaming_banned_keywords || []).join('\n'));
          setInputValue('danmaku-banned-keywords', (data.danmaku_banned_keywords || []).join('\n'));
        } catch (error) {
          console.error('Failed to load banned keywords:', error);
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
        const toggle = document.getElementById('bili-danmaku-command-toggle');
        if (!toggle) return;
        const enabled = toggle.checked;

        try {
          const result = await postJsonApi('/api/config', {
            enable_danmaku_command: enabled
          });
          if (result.success) {
            window.configData.bilibili = {
              ...(window.configData.bilibili || {}),
              enable_danmaku_command: enabled
            };
            showNotification(enabled ? '弹幕指令已启用' : '弹幕指令已禁用', 'success');
          } else {
            // Revert toggle if save failed
            toggle.checked = !enabled;
            showNotification(result.message || '保存失败', 'error');
          }
        } catch (error) {
          console.error('Failed to toggle danmaku command:', error);
          // Revert toggle if save failed
          toggle.checked = !enabled;
          showNotification('保存失败: ' + error.message, 'error');
        }
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

      function appendAntiCollisionRemoveIcon(button) {
        const svgNamespace = 'http://www.w3.org/2000/svg';
        const svg = document.createElementNS(svgNamespace, 'svg');
        svg.classList.add('cluster-btn-icon');
        svg.setAttribute('viewBox', '0 0 24 24');
        svg.setAttribute('fill', 'none');
        svg.setAttribute('stroke', 'currentColor');
        svg.setAttribute('stroke-width', '2');
        svg.setAttribute('stroke-linecap', 'round');
        svg.setAttribute('stroke-linejoin', 'round');
        svg.setAttribute('aria-hidden', 'true');

        const polyline = document.createElementNS(svgNamespace, 'polyline');
        polyline.setAttribute('points', '3,6 5,6 21,6');

        const path = document.createElementNS(svgNamespace, 'path');
        path.setAttribute('d', 'm19,6v14a2,2 0 0,1 -2,2H7a2,2 0 0,1 -2,-2V6m3,0V4a2,2 0 0,1 2,-2h4a2,2 0 0,1 2,2v2');

        const leftLine = document.createElementNS(svgNamespace, 'line');
        leftLine.setAttribute('x1', '10');
        leftLine.setAttribute('y1', '11');
        leftLine.setAttribute('x2', '10');
        leftLine.setAttribute('y2', '17');

        const rightLine = document.createElementNS(svgNamespace, 'line');
        rightLine.setAttribute('x1', '14');
        rightLine.setAttribute('y1', '11');
        rightLine.setAttribute('x2', '14');
        rightLine.setAttribute('y2', '17');

        svg.append(polyline, path, leftLine, rightLine);
        button.appendChild(svg);
      }

      function appendEditIcon(button) {
        const svgNamespace = 'http://www.w3.org/2000/svg';
        const svg = document.createElementNS(svgNamespace, 'svg');
        svg.classList.add('cluster-btn-icon');
        svg.setAttribute('viewBox', '0 0 24 24');
        svg.setAttribute('fill', 'none');
        svg.setAttribute('stroke', 'currentColor');
        svg.setAttribute('stroke-width', '2');
        svg.setAttribute('stroke-linecap', 'round');
        svg.setAttribute('stroke-linejoin', 'round');
        svg.setAttribute('aria-hidden', 'true');

        const bottomLine = document.createElementNS(svgNamespace, 'path');
        bottomLine.setAttribute('d', 'M12 20h9');

        const pencil = document.createElementNS(svgNamespace, 'path');
        pencil.setAttribute('d', 'M16.5 3.5a2.121 2.121 0 0 1 3 3L7 19l-4 1 1-4L16.5 3.5z');

        svg.append(bottomLine, pencil);
        button.appendChild(svg);
      }

      function createManagementMessage(message, className) {
        const paragraph = document.createElement('p');
        paragraph.className = className;
        paragraph.textContent = message;
        return paragraph;
      }

      function createManagementActionButton(title, onClick, appendIcon) {
        const button = document.createElement('button');
        button.type = 'button';
        button.className = 'management-item-action-btn';
        button.title = title;
        button.setAttribute('aria-label', title);
        button.addEventListener('click', onClick);
        appendIcon(button);
        return button;
      }

      function createManagementMeta(label, values, extraClass = '') {
        const small = document.createElement('small');
        small.className = extraClass
          ? `management-item-meta ${extraClass}`
          : 'management-item-meta';
        const normalized = Array.isArray(values) ? values.filter(Boolean).join(', ') : values;
        small.textContent = `${label}: ${normalized || '无'}`;
        return small;
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
        try {
          const config = getCurrentConfig();

          const result = await postJsonApi('/api/config', config);

          if (result.success) {
            mergeConfigData({
              ...config,
              bilibili: {
                ...(window.configData.bilibili || {}),
                enable_danmaku_command: config.enable_danmaku_command
              },
              youtube: {
                ...(window.configData.youtube || {}),
                proxy: config.youtube_proxy,
                cookies_from_browser: config.youtube_cookies_from_browser,
                cookies_file: config.youtube_cookies_file,
                deno_path: config.youtube_deno_path
              },
              twitch: {
                ...(window.configData.twitch || {}),
                proxy: config.twitch_proxy,
                proxy_region: config.twitch_proxy_region
              }
            });
            updateDanmakuCommandToggle(config.enable_danmaku_command);
            try {
              await saveBannedKeywords();
              showNotification(result.message || '配置保存成功', 'success');
            } catch (keywordError) {
              console.error('Failed to save banned keywords:', keywordError);
              showNotification(`配置已保存，但禁用关键词保存失败: ${keywordError.message}`, 'error');
            }
          } else {
            showNotification('配置保存失败: ' + (result.error || '未知错误'), 'error');
          }
        } catch (error) {
          console.error('Failed to save system config:', error);
          showNotification('配置保存失败', 'error');
        }
      }

      async function saveBannedKeywords() {
        const result = await postJsonApi('/api/banned-keywords', {
          streaming_banned_keywords: readBannedKeywordLines('streaming-banned-keywords'),
          danmaku_banned_keywords: readBannedKeywordLines('danmaku-banned-keywords')
        });
        if (!result.success) {
          throw new Error(result.message || '未知错误');
        }
      }

      function readBannedKeywordLines(elementId) {
        const value = document.getElementById(elementId)?.value || '';
        return value
          .split('\n')
          .map(keyword => keyword.trim())
          .filter(Boolean);
      }

      // Areas and channels are fetched the first time the management view is
      // opened, then only on explicit refresh.
      function loadManagementListsOnce() {
        const areasContent = document.getElementById('areas-content');
        if (areasContent && areasContent.dataset.loaded !== 'true') {
          loadAreas();
        }

        const channelsContent = document.getElementById('channels-content');
        if (channelsContent && channelsContent.dataset.loaded !== 'true') {
          loadChannels();
        }
      }

      // Area management functions
      async function loadAreas() {
        try {
          const result = await managementRequest('/api/manage/areas');

          if (result.success) {
            const areasContent = document.getElementById('areas-content');
            if (!areasContent) return;
            areasContent.dataset.loaded = 'true';

            if (result.data.areas.length === 0) {
              areasContent.replaceChildren(
                createManagementMessage('暂无分区', 'management-empty-message')
              );
            } else {
              const grid = document.createElement('div');
              grid.className = 'management-item-grid';

              result.data.areas.forEach(area => {
                const card = document.createElement('div');
                card.className = 'management-item-card';

                const header = document.createElement('div');
                header.className = 'management-item-header';

                const title = document.createElement('div');
                title.className = 'management-item-title';

                const id = document.createElement('strong');
                id.textContent = `ID: ${area.id}`;
                title.append(id, document.createTextNode(` - ${area.name}`));

                const actions = document.createElement('div');
                actions.className = 'management-item-actions';
                actions.append(
                  createManagementActionButton('编辑分区', () => editArea(area.id), appendEditIcon),
                  createManagementActionButton('删除分区', () => deleteArea(area.id), appendAntiCollisionRemoveIcon)
                );

                const divider = document.createElement('hr');
                divider.className = 'management-item-divider';

                header.append(title, actions);
                card.append(
                  header,
                  divider,
                  createManagementMeta('关键词', area.title_keywords),
                  createManagementMeta('别名', area.aliases)
                );
                grid.appendChild(card);
              });

              areasContent.replaceChildren(grid);
            }
          } else {
            const areasContent = document.getElementById('areas-content');
            if (areasContent) {
              areasContent.replaceChildren(
                createManagementMessage(`加载失败: ${result.message}`, 'management-error-message')
              );
            }
          }
        } catch (error) {
          console.error('Load areas error:', error);
          const areasContent = document.getElementById('areas-content');
          if (areasContent) {
            areasContent.replaceChildren(
              createManagementMessage(`加载失败: ${error.message}`, 'management-error-message')
            );
          }
        }
      }

      async function addArea() {
        const area = readAreaForm();

        if (!area.id || !area.name) {
          showNotification('请填写分区ID和名称', 'error');
          return;
        }

        try {
          const result = await managementJsonRequest('/api/manage/areas', 'POST', area);
          if (result.success) {
            showManagementSuccess(result, '分区添加成功');
            clearAreaForm();
            loadAreas();
          } else {
            showNotification(`添加失败: ${result.message}`, 'error');
          }
        } catch (error) {
          console.error('Add area error:', error);
          showNotification(`添加失败: ${error.message}`, 'error');
        }
      }

      async function readManagementResponse(response) {
        if (response.status === 401) {
          throw new Error(unauthorizedApiError());
        }
        const contentType = response.headers.get('content-type') || '';
        const bodyText = await response.text();
        if (!contentType.includes('application/json')) {
          throw new Error(`Expected JSON, got: ${contentType || 'unknown'}. Response: ${bodyText}`);
        }

        let result = null;
        try {
          result = bodyText ? JSON.parse(bodyText) : null;
        } catch (error) {
          throw new Error(response.ok ? '服务器返回了无效 JSON' : formatHttpError(response, bodyText));
        }

        if (!response.ok) {
          throw new Error(result?.message || formatHttpError(response, bodyText));
        }
        if (!result) {
          throw new Error('服务器返回空响应');
        }
        return result;
      }

      async function managementRequest(path, options = {}) {
        const response = await fetchWithWebUiAuth(path, options);
        return readManagementResponse(response);
      }

      function managementJsonRequest(path, method, payload) {
        return managementRequest(path, {
          method,
          headers: { 'Content-Type': 'application/json' },
          body: JSON.stringify(payload)
        });
      }

      function deleteManagementResource(path) {
        return managementRequest(path, { method: 'DELETE' });
      }

      function showManagementSuccess(result, fallbackMessage) {
        showNotification(result.message || fallbackMessage, 'success');
      }

      function readInputValue(id) {
        return document.getElementById(id)?.value?.trim() || '';
      }

      function parseCommaSeparatedInput(id) {
        return readInputValue(id)
          .split(',')
          .map(item => item.trim())
          .filter(Boolean);
      }

      function readAreaForm() {
        return {
          id: readIntegerInput('area-id', 0),
          name: readInputValue('area-name'),
          title_keywords: parseCommaSeparatedInput('area-keywords'),
          aliases: parseCommaSeparatedInput('area-aliases')
        };
      }

      function parseInteger(value, fallback = 0) {
        const normalized = String(value ?? '').trim();
        if (!normalized) {
          return fallback;
        }
        if (!/^[+-]?\d+$/.test(normalized)) {
          return fallback;
        }

        const parsed = Number(normalized);
        return Number.isSafeInteger(parsed) ? parsed : fallback;
      }

      function readIntegerInput(id, fallback = 0) {
        return parseInteger(document.getElementById(id)?.value, fallback);
      }

      function setInputValue(id, value) {
        const input = document.getElementById(id);
        if (input) {
          input.value = value;
        }
      }

      function setElementText(id, value) {
        const element = document.getElementById(id);
        if (element) {
          element.textContent = value;
        }
      }

      function setCheckboxChecked(id, checked) {
        const checkbox = document.getElementById(id);
        if (checkbox) {
          checkbox.checked = !!checked;
        }
      }

      // Channel management functions
      async function loadChannels() {
        try {
          const result = await managementRequest('/api/manage/channels');

          if (result.success) {
            const channelsContent = document.getElementById('channels-content');
            if (!channelsContent) return;
            channelsContent.dataset.loaded = 'true';

            if (result.data.channels.length === 0) {
              channelsContent.replaceChildren(
                createManagementMessage('暂无频道', 'management-empty-message')
              );
            } else {
              const grid = document.createElement('div');
              grid.className = 'management-item-grid';

              result.data.channels.forEach(channel => {
                const platforms = channel.platforms || {};
                const card = document.createElement('div');
                card.className = 'management-item-card';

                const header = document.createElement('div');
                header.className = 'management-item-header';

                const title = document.createElement('div');
                title.className = 'management-item-title';
                const name = document.createElement('strong');
                name.textContent = channel.name;
                title.appendChild(name);

                const actions = document.createElement('div');
                actions.className = 'management-item-actions';
                actions.append(
                  createManagementActionButton('编辑频道', () => editChannel(channel.name), appendEditIcon),
                  createManagementActionButton('删除频道', () => deleteChannel(channel.name), appendAntiCollisionRemoveIcon)
                );

                const divider = document.createElement('hr');
                divider.className = 'management-item-divider';

                header.append(title, actions);
                card.append(
                  header,
                  divider,
                  createManagementMeta('别名', channel.aliases),
                  createManagementMeta('YouTube', platforms.youtube),
                  createManagementMeta('Twitch', platforms.twitch)
                );

                if (channel.riot_puuid) {
                  card.appendChild(createManagementMeta('Riot PUUID', channel.riot_puuid));
                }

                grid.appendChild(card);
              });

              channelsContent.replaceChildren(grid);
            }
          } else {
            const channelsContent = document.getElementById('channels-content');
            if (channelsContent) {
              channelsContent.replaceChildren(
                createManagementMessage(`加载失败: ${result.message}`, 'management-error-message')
              );
            }
          }
        } catch (error) {
          console.error('Load channels error:', error);
          const channelsContent = document.getElementById('channels-content');
          if (channelsContent) {
            channelsContent.replaceChildren(
              createManagementMessage(`加载失败: ${error.message}`, 'management-error-message')
            );
          }
        }
      }

      async function addChannel() {
        const payload = readChannelForm();

        if (!payload.name) {
          showNotification('请填写频道名称', 'error');
          return;
        }

        if (Object.keys(payload.platforms).length === 0) {
          showNotification('请至少填写一个平台的频道ID（YouTube或Twitch）', 'error');
          return;
        }

        try {
          const result = await managementJsonRequest('/api/manage/channels', 'POST', payload);
          if (result.success) {
            showManagementSuccess(result, '频道添加成功');
            clearChannelForm();
            loadChannels();
          } else {
            showNotification(`添加失败: ${result.message}`, 'error');
          }
        } catch (error) {
          console.error('Add channel error:', error);
          showNotification(`添加失败: ${error.message}`, 'error');
        }
      }

      async function updateChannel() {
        const payload = readChannelForm();

        if (!payload.name) {
          showNotification('请填写频道名称', 'error');
          return;
        }

        if (Object.keys(payload.platforms).length === 0) {
          showNotification('请至少填写一个平台的频道ID（YouTube或Twitch）', 'error');
          return;
        }

        try {
          const result = await managementJsonRequest('/api/manage/channels', 'PUT', payload);
          if (result.success) {
            showManagementSuccess(result, '频道更新成功');
            clearChannelForm();
            loadChannels();
          } else {
            showNotification(`更新失败: ${result.message}`, 'error');
          }
        } catch (error) {
          showNotification(`更新失败: ${error.message}`, 'error');
        }
      }

      function readChannelForm() {
        const youtube = readInputValue('channel-youtube');
        const twitch = readInputValue('channel-twitch');
        const platforms = {};
        if (youtube) platforms.youtube = youtube;
        if (twitch) platforms.twitch = twitch;

        return {
          name: readInputValue('channel-name'),
          aliases: parseCommaSeparatedInput('channel-aliases'),
          platforms,
          riot_puuid: readInputValue('channel-riot') || null
        };
      }

      async function editChannel(channelName) {
        try {
          const result = await managementRequest('/api/manage/channels');
          if (!result.success) {
            showNotification(`加载失败: ${result.message}`, 'error');
            return;
          }

          const channel = result.data.channels.find(c => c.name === channelName);
          if (!channel) {
            showNotification('未找到频道配置', 'error');
            return;
          }

          const platforms = channel.platforms || {};
          setInputValue('channel-name', channel.name);
          setInputValue('channel-aliases', (channel.aliases || []).join(', '));
          setInputValue('channel-youtube', platforms.youtube || '');
          setInputValue('channel-twitch', platforms.twitch || '');
          setInputValue('channel-riot', channel.riot_puuid || '');

          isEditingChannel = true;
          setElementText('channel-form-title', '编辑频道');
          setElementText('channel-submit-btn', '更新频道');
          document.getElementById('channel-name').scrollIntoView({ behavior: 'smooth' });
        } catch (error) {
          showNotification(`加载失败: ${error.message}`, 'error');
        }
      }

      function clearChannelForm() {
        setInputValue('channel-name', '');
        setInputValue('channel-aliases', '');
        setInputValue('channel-youtube', '');
        setInputValue('channel-twitch', '');
        setInputValue('channel-riot', '');

        isEditingChannel = false;
        setElementText('channel-form-title', '添加频道');
        setElementText('channel-submit-btn', '添加频道');
      }

      function readHolodexAddChannelData(control) {
        return {
          name: control.dataset.channelName || '',
          youtubeId: control.dataset.youtubeId || '',
          twitchId: control.dataset.twitchId || ''
        };
      }

      function buildHolodexAddChannelPayload(channelData) {
        const platforms = {};
        if (channelData.youtubeId) platforms.youtube = channelData.youtubeId;
        if (channelData.twitchId) platforms.twitch = channelData.twitchId;

        return {
          name: channelData.name,
          aliases: [],
          platforms,
          riot_puuid: null
        };
      }

      function addHolodexChannelToCache(payload) {
        if (!channelsData || !Array.isArray(channelsData.channels)) {
          channelsData = { channels: [] };
        }

        const exists = channelsData.channels.some(channel => {
          const platforms = channel.platforms || {};
          return normalizeHolodexChannelValue(channel.name) === normalizeHolodexChannelValue(payload.name)
            || (payload.platforms.youtube && normalizeHolodexChannelValue(platforms.youtube) === normalizeHolodexChannelValue(payload.platforms.youtube))
            || (payload.platforms.twitch && normalizeHolodexChannelValue(platforms.twitch) === normalizeHolodexChannelValue(payload.platforms.twitch));
        });

        if (!exists) {
          channelsData.channels.push(payload);
        }
      }

      function createHolodexChannelAddedIndicator() {
        const indicator = document.createElement('span');
        indicator.className = 'holodex-channel-added-icon';
        indicator.title = '已添加到 channels.json';
        indicator.appendChild(createSvgIcon('0 0 24 24', 'M20 6 9 17l-5-5'));
        return indicator;
      }

      function markHolodexChannelAdded(channelData) {
        document.querySelectorAll('.holodex-channel-add').forEach(control => {
          const data = readHolodexAddChannelData(control);
          const sameChannel = normalizeHolodexChannelValue(data.name) === normalizeHolodexChannelValue(channelData.name)
            || (channelData.youtubeId && normalizeHolodexChannelValue(data.youtubeId) === normalizeHolodexChannelValue(channelData.youtubeId))
            || (channelData.twitchId && normalizeHolodexChannelValue(data.twitchId) === normalizeHolodexChannelValue(channelData.twitchId));

          if (sameChannel) {
            control.classList.add('holodex-channel-add-added');
            control.replaceChildren(createHolodexChannelAddedIndicator());
          }
        });
      }

      function setHolodexAddChannelConfirmVisible(control, visible) {
        control.classList.toggle('holodex-channel-add-active', visible);
      }

      function setHolodexAddChannelButtonsDisabled(control, disabled) {
        control.querySelectorAll('button').forEach(button => {
          button.disabled = disabled;
        });
      }

      async function confirmHolodexAddChannel(button) {
        const control = button.closest('.holodex-channel-add');
        if (!control) return;

        const channelData = readHolodexAddChannelData(control);
        const payload = buildHolodexAddChannelPayload(channelData);
        if (!payload.name || Object.keys(payload.platforms).length === 0) {
          showNotification('频道信息不完整，无法添加', 'error');
          return;
        }

        setHolodexAddChannelButtonsDisabled(control, true);

        try {
          const result = await managementJsonRequest('/api/manage/channels', 'POST', payload);
          const alreadyExists = /already exists/i.test(result.message || '');

          if (result.success || alreadyExists) {
            addHolodexChannelToCache(payload);
            markHolodexChannelAdded(channelData);
            showNotification(
              alreadyExists ? 'channels.json 已有该频道' : (result.message || '已添加到 channels.json'),
              'success'
            );

            const channelsContent = document.getElementById('channels-content');
            if (channelsContent && !isElementHidden(channelsContent)) {
              loadChannels();
            }
          } else {
            showNotification('添加失败: ' + (result.message || 'Unknown error'), 'error');
            setHolodexAddChannelButtonsDisabled(control, false);
          }
        } catch (error) {
          showNotification('添加失败: ' + error.message, 'error');
          setHolodexAddChannelButtonsDisabled(control, false);
        }
      }

      async function editArea(areaId) {
        try {
          const result = await managementRequest('/api/manage/areas');
          if (!result.success) {
            showNotification(`加载失败: ${result.message}`, 'error');
            return;
          }

          const area = result.data.areas.find(item => item.id === areaId);
          if (!area) {
            showNotification('未找到分区配置', 'error');
            return;
          }

          setInputValue('area-id', area.id);
          setInputValue('area-name', area.name);
          setInputValue('area-keywords', (area.title_keywords || []).join(', '));
          setInputValue('area-aliases', (area.aliases || []).join(', '));

          editingAreaId = areaId;
          setElementText('area-form-title', '编辑分区');
          setElementText('area-submit-btn', '更新分区');
          document.getElementById('area-id').scrollIntoView({ behavior: 'smooth' });
        } catch (error) {
          showNotification(`加载失败: ${error.message}`, 'error');
        }
      }

      function clearAreaForm() {
        setInputValue('area-id', '');
        setInputValue('area-name', '');
        setInputValue('area-keywords', '');
        setInputValue('area-aliases', '');

        editingAreaId = null;
        setElementText('area-form-title', '添加新分区');
        setElementText('area-submit-btn', '添加分区');
      }

      async function updateArea(originalId) {
        const area = readAreaForm();

        if (!area.id || !area.name) {
          showNotification('请填写分区ID和名称', 'error');
          return;
        }

        try {
          // If ID changed, we need to delete the old one and add the new one
          if (originalId !== area.id) {
            // Delete old area
            const deleteResult = await deleteManagementResource(`/api/manage/areas/${originalId}`);
            if (!deleteResult.success) {
              throw new Error(deleteResult.message || '删除原分区失败');
            }

            // Add new area with new ID
            const result = await managementJsonRequest('/api/manage/areas', 'POST', area);
            if (result.success) {
              showManagementSuccess(result, '分区更新成功');
              clearAreaForm();
              loadAreas();
            } else {
              showNotification(`更新失败: ${result.message}`, 'error');
            }
            return;
          }

          // Update existing area (ID unchanged)
          const result = await managementJsonRequest('/api/manage/areas', 'PUT', area);
          if (result.success) {
            showManagementSuccess(result, '分区更新成功');
            clearAreaForm();
            loadAreas();
          } else {
            showNotification(`更新失败: ${result.message}`, 'error');
          }
        } catch (error) {
          console.error('Update area error:', error);
          showNotification(`更新失败: ${error.message}`, 'error');
        }
      }

      // Delete functions
      async function deleteArea(areaId) {
        if (!confirm(`确定要删除分区 ID ${areaId} 吗？此操作不可撤销。`)) {
          return;
        }

        try {
          const result = await deleteManagementResource(`/api/manage/areas/${areaId}`);
          if (result.success) {
            showManagementSuccess(result, '分区删除成功');
            loadAreas();
          } else {
            showNotification(`删除失败: ${result.message}`, 'error');
          }
        } catch (error) {
          showNotification(`删除失败: ${error.message}`, 'error');
        }
      }

      async function deleteChannel(channelName) {
        if (!confirm(`确定要删除频道 "${channelName}" 吗？此操作不可撤销。`)) {
          return;
        }

        try {
          const result = await deleteManagementResource(`/api/manage/channels/${encodeURIComponent(channelName)}`);
          if (result.success) {
            showManagementSuccess(result, '频道删除成功');
            loadChannels();
          } else {
            showNotification(`删除失败: ${result.message}`, 'error');
          }
        } catch (error) {
          showNotification(`删除失败: ${error.message}`, 'error');
        }
      }

      // Animated refresh functions
      async function refreshAreas() {
        const btn = document.getElementById('refreshAreasBtn');
        const icon = document.getElementById('refreshAreasIcon');

        // Disable button and start spinning animation
        setButtonLoading(btn, icon, true);

        try {
          await loadAreas();
          showNotification('分区列表已刷新', 'success');
        } catch (error) {
          console.error('Error refreshing areas:', error);
          showNotification('刷新分区列表失败', 'error');
        } finally {
          // Re-enable button and stop spinning
          setButtonLoading(btn, icon, false);
        }
      }

      async function refreshChannels() {
        const btn = document.getElementById('refreshChannelsBtn');
        const icon = document.getElementById('refreshChannelsIcon');

        // Disable button and start spinning animation
        setButtonLoading(btn, icon, true);

        try {
          await loadChannels();
          showNotification('频道列表已刷新', 'success');
        } catch (error) {
          console.error('Error refreshing channels:', error);
          showNotification('刷新频道列表失败', 'error');
        } finally {
          // Re-enable button and stop spinning
          setButtonLoading(btn, icon, false);
        }
      }

      function clearLogs() {
        logLines = [];
        const logOutput = document.getElementById('log-output');
        if (logOutput) {
          logOutput.replaceChildren(document.createTextNode('日志已清空'));
        }
      }

      async function refreshLogs() {
        try {
          const data = await getJson('/api/logs');
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

      function showNotification(message, type = 'success') {
        const notification = document.createElement('div');
        notification.className = `notification ${type}`;
        notification.textContent = message;
        // Toasts go into a dedicated stack so several at once queue up
        // instead of drawing on top of each other.
        (document.getElementById('toast-region') || document.body).appendChild(notification);

        setTimeout(() => {
          notification.remove();
        }, 3000);
      }

      function setButtonLoading(button, icon, loading) {
        if (button) {
          button.disabled = loading;
          button.classList.toggle('is-loading', loading);
        }
        if (icon) {
          icon.classList.toggle('is-spinning', loading);
        }
      }

      function formatHttpError(response, bodyText = '') {
        const trimmed = bodyText.trim();
        if (trimmed) {
          return trimmed.length > 200 ? `${trimmed.slice(0, 200)}...` : trimmed;
        }
        return `HTTP ${response.status}: ${response.statusText}`;
      }

      async function readJsonApiResponse(response) {
        if (response.status === 401) {
          throw new Error(unauthorizedApiError());
        }
        const bodyText = await response.text();
        let result = null;
        if (bodyText) {
          try {
            result = JSON.parse(bodyText);
          } catch (error) {
            if (response.ok) {
              throw new Error('服务器返回了无效 JSON');
            }
            throw new Error(formatHttpError(response, bodyText));
          }
        }

        if (!response.ok) {
          throw new Error(result?.message || formatHttpError(response, bodyText));
        }
        if (!result) {
          throw new Error('服务器返回空响应');
        }
        return result;
      }

      async function getJson(path) {
        const response = await fetchWithWebUiAuth(path);
        return readJsonApiResponse(response);
      }

      async function postJsonApi(path, payload) {
        const request = {
          method: 'POST',
          headers: { 'Content-Type': 'application/json' }
        };
        if (payload !== undefined) {
          request.body = JSON.stringify(payload);
        }

        const response = await fetchWithWebUiAuth(path, request);
        return readJsonApiResponse(response);
      }

      async function refreshDashboardStatusEndpoint(options) {
        const btn = document.getElementById(options.buttonId);
        const icon = document.getElementById(options.iconId);
        setButtonLoading(btn, icon, true);

        try {
          const result = await getJson(options.endpoint);
          if (!result.success) {
            throw new Error(result.message || options.defaultError);
          }
          await refreshStatus();
          showNotification(options.successMessage, 'success');
        } catch (error) {
          console.error(options.logMessage, error);
          showNotification(`${options.failureMessage}: ${error.message || options.defaultError}`, 'error');
        } finally {
          setButtonLoading(btn, icon, false);
        }
      }

      function refreshBilibiliStatus() {
        return refreshDashboardStatusEndpoint({
          endpoint: '/api/status',
          buttonId: 'refreshBilibiliBtn',
          iconId: 'refreshBilibiliIcon',
          successMessage: 'Bilibili status refreshed',
          failureMessage: '刷新失败',
          defaultError: '服务器未响应',
          logMessage: 'Error refreshing Bilibili status:'
        });
      }

      function togglePlatformMonitor(platform, toggleId, endpoint) {
        const toggle = document.getElementById(toggleId);
        if (!toggle) {
          return;
        }

        const state = getMonitorToggleSaveState(platform, toggleId);
        state.desired = toggle.checked;

        if (state.timer) {
          clearTimeout(state.timer);
        }
        state.timer = setTimeout(() => {
          state.timer = null;
          flushPlatformMonitorToggle(platform, toggleId, endpoint);
        }, monitorToggleSaveDebounceMs);
      }

      function getMonitorToggleSaveState(platform, toggleId) {
        let state = monitorToggleSaveState.get(toggleId);
        if (!state) {
          const confirmed = window.configData[platform]?.enable_monitor !== false;
          state = {
            confirmed,
            desired: confirmed,
            inFlight: false,
            timer: null
          };
          monitorToggleSaveState.set(toggleId, state);
        }
        return state;
      }

      async function flushPlatformMonitorToggle(platform, toggleId, endpoint) {
        const state = getMonitorToggleSaveState(platform, toggleId);
        if (state.inFlight) {
          return;
        }

        const toggle = document.getElementById(toggleId);
        const enabled = state.desired;
        state.inFlight = true;
        try {
          const result = await postJsonApi(endpoint, { enabled });
          if (result.success) {
            state.confirmed = enabled;
            window.configData[platform] = {
              ...(window.configData[platform] || {}),
              enable_monitor: enabled
            };
            if (state.desired === enabled) {
              showNotification(result.message, 'success');
              refreshStatus().catch((error) => {
                console.debug('Failed to refresh status after monitor toggle:', error);
              });
            }
          } else {
            if (state.desired === enabled) {
              state.desired = state.confirmed;
              if (toggle) {
                toggle.checked = state.confirmed;
              }
              showNotification(result.message || '保存失败', 'error');
            }
          }
        } catch (error) {
          console.error(`Failed to toggle ${platform} monitor:`, error);
          if (state.desired === enabled) {
            state.desired = state.confirmed;
            if (toggle) {
              toggle.checked = state.confirmed;
            }
            showNotification('保存失败: ' + error.message, 'error');
          }
        } finally {
          state.inFlight = false;
          if (state.desired !== state.confirmed) {
            flushPlatformMonitorToggle(platform, toggleId, endpoint);
          }
        }
      }

      function toggleYouTubeMonitor() {
        return togglePlatformMonitor('youtube', 'youtube-monitor-toggle', '/api/toggle-youtube-monitor');
      }

      function toggleTwitchMonitor() {
        return togglePlatformMonitor('twitch', 'twitch-monitor-toggle', '/api/toggle-twitch-monitor');
      }

      async function refreshPlatformStatus(label, endpoint, buttonId, iconId) {
        const btn = document.getElementById(buttonId);
        const icon = document.getElementById(iconId);

        setButtonLoading(btn, icon, true);

        try {
          const data = await getJson(endpoint);
          if (data.success) {
            showNotification(`${label} status refreshed`, 'success');
            await refreshStatus();
          } else {
            showNotification(data.message || `Failed to refresh ${label} status`, 'error');
          }
        } catch (error) {
          console.error(`Error refreshing ${label} status:`, error);
          showNotification(`Failed to refresh ${label} status: ${error.message}`, 'error');
        } finally {
          setButtonLoading(btn, icon, false);
        }
      }

      function refreshYouTubeStatus() {
        return refreshPlatformStatus('YouTube', '/api/refresh/youtube', 'refreshYouTubeBtn', 'refreshYouTubeIcon');
      }

      function refreshTwitchStatus() {
        return refreshPlatformStatus('Twitch', '/api/refresh/twitch', 'refreshTwitchBtn', 'refreshTwitchIcon');
      }

      function syncInfoRowLabelCenter(rowElement, valueElement) {
        if (!rowElement || !valueElement) {
          return;
        }

        const lineHeight = parseFloat(getComputedStyle(valueElement).lineHeight);
        const isMultiLine = Number.isFinite(lineHeight)
          && lineHeight > 0
          && valueElement.offsetHeight > lineHeight * 1.5;
        rowElement.classList.toggle('info-row-label-center', isMultiLine);
      }

      function syncPlatformTitleRowCenters() {
        syncInfoRowLabelCenter(
          document.getElementById('yt-title-row'),
          document.getElementById('yt-title')
        );
        syncInfoRowLabelCenter(
          document.getElementById('tw-title-row'),
          document.getElementById('tw-title')
        );
      }

      // Mirrors the Bilibili room state into the top bar so the current state
      // is readable from every view.
      function updateAppLiveBadge(isLive) {
        const badge = document.getElementById('app-live-badge');
        const text = document.getElementById('app-live-badge-text');
        if (!badge) return;

        badge.classList.toggle('is-live', !!isLive);
        if (text) {
          text.textContent = isLive ? '直播中' : '未开播';
        }
      }

      function setPlatformLiveInfoVisibility(platform, isLive) {
        const rowIds = platform === 'youtube'
          ? ['yt-title-row', 'yt-topic-row']
          : platform === 'twitch'
            ? ['tw-title-row', 'tw-game-row']
              : [];
        for (const id of rowIds) {
          const row = document.getElementById(id);
          if (row) {
            row.style.display = isLive ? '' : 'none';
          }
        }
      }

      function schedulePlatformTitleRowCenters() {
        requestAnimationFrame(() => {
          requestAnimationFrame(syncPlatformTitleRowCenters);
        });
      }

      function pushBiliNetworkSample(series, value) {
        series.push(Number.isFinite(value) && value > 0 ? value : 0);
        if (series.length > biliNetworkHistoryLimit) {
          series.splice(0, series.length - biliNetworkHistoryLimit);
        }
      }

      function formatNetworkRate(kbps) {
        if (!Number.isFinite(kbps) || kbps <= 0) {
          return '-';
        }
        if (kbps >= 1000) {
          return `${(kbps / 1000).toFixed(2)} Mb/s`;
        }
        return `${Math.round(kbps)} Kb/s`;
      }

      function formatBytes(bytes) {
        if (!Number.isFinite(bytes) || bytes <= 0) {
          return '-';
        }
        const units = ['B', 'KiB', 'MiB', 'GiB', 'TiB'];
        let value = bytes;
        let unit = 0;
        while (value >= 1024 && unit < units.length - 1) {
          value /= 1024;
          unit += 1;
        }
        return unit === 0 ? `${bytes} ${units[unit]}` : `${value.toFixed(1)} ${units[unit]}`;
      }

      function formatSpeedRatio(value) {
        return Number.isFinite(value) && value > 0 ? `${value.toFixed(2)}x` : '-';
      }

      function formatFps(value) {
        if (!Number.isFinite(value) || value < 0) {
          return '-';
        }
        return value >= 100 ? `${Math.round(value)}` : value.toFixed(1);
      }

      function formatFrameCount(value) {
        return Number.isFinite(value) && value >= 0 ? Math.round(value).toLocaleString() : '-';
      }

      function createBiliNetworkBar(type, heightPercent) {
        const bar = document.createElement('span');
        bar.className = `bili-network-bar ${type} active`;
        bar.style.height = `${heightPercent}%`;
        return bar;
      }

      function renderBiliNetworkGraph(showCache) {
        const graph = document.getElementById('bili-network-graph');
        if (!graph) {
          return;
        }

        // Cache on: mirrored halves. Cache off: full-height single-sided push bars.
        graph.classList.toggle('single-sided', !showCache);

        const activeSeries = showCache
          ? biliNetworkHistory.cache.concat(biliNetworkHistory.push)
          : biliNetworkHistory.push;
        const maxRate = Math.max(1, ...activeSeries);
        const scale = document.getElementById('bili-network-scale');
        if (scale) {
          scale.textContent = `Scale ${formatNetworkRate(maxRate)}`;
        }
        const graphWidth = window.matchMedia('(max-width: 520px)').matches ? 32 : biliNetworkHistoryLimit;
        const pushStart = Math.max(0, biliNetworkHistory.push.length - graphWidth);
        const pushSeries = biliNetworkHistory.push.slice(pushStart);
        const cacheStart = Math.max(0, biliNetworkHistory.cache.length - graphWidth);
        const cacheSeries = biliNetworkHistory.cache.slice(cacheStart);
        const heightScale = showCache ? 50 : 100;
        const fragment = document.createDocumentFragment();

        for (let i = 0; i < graphWidth; i += 1) {
          const pushValue = pushSeries[i - (graphWidth - pushSeries.length)] || 0;
          const cacheValue = cacheSeries[i - (graphWidth - cacheSeries.length)] || 0;
          const cacheHeight = showCache ? Math.max(2, Math.round((cacheValue / maxRate) * heightScale)) : 0;
          const pushHeight = Math.max(2, Math.round((pushValue / maxRate) * heightScale));

          const column = document.createElement('span');
          column.className = 'bili-network-column';
          if (showCache) {
            column.appendChild(createBiliNetworkBar('cache', cacheHeight));
          }
          column.appendChild(createBiliNetworkBar('push', pushHeight));
          fragment.appendChild(column);
        }

        graph.replaceChildren(fragment);
      }

      function applyBiliStreamQualityColor(element, quality) {
        element.classList.toggle('bili-network-quality-smooth', quality === '流畅');
        element.classList.toggle('bili-network-quality-unstable', quality === '波动');
        element.classList.toggle('bili-network-quality-stalled', quality === '卡顿');
      }

      function updateBiliNetworkPanel(bili) {
        const panel = document.getElementById('bili-network-panel');
        if (!panel) {
          return;
        }

        lastBiliNetworkLive = typeof bili.is_live === 'boolean' ? bili.is_live : lastBiliNetworkLive;
        lastBiliNetworkQuality = bili.stream_quality || lastBiliNetworkQuality;
        const hasPush = Number.isFinite(bili.stream_bitrate_kbps)
          || Number.isFinite(bili.stream_speed)
          || Number.isFinite(bili.stream_fps)
          || Number.isFinite(bili.stream_frame);
        const hasCache = bili.hls_cache_active && (Number.isFinite(bili.stream_cache_bitrate_kbps) || Number.isFinite(bili.stream_cache_speed));
        if (!lastBiliNetworkLive || (!hasPush && !hasCache && !lastBiliNetworkQuality)) {
          panel.classList.add('hidden');
          return;
        }

        panel.classList.remove('hidden');
        pushBiliNetworkSample(biliNetworkHistory.push, bili.stream_bitrate_kbps);
        pushBiliNetworkSample(biliNetworkHistory.cache, hasCache ? bili.stream_cache_bitrate_kbps : 0);

        const quality = document.getElementById('bili-network-quality');
        quality.textContent = lastBiliNetworkQuality || 'Live';
        applyBiliStreamQualityColor(quality, lastBiliNetworkQuality);

        updateBiliNetworkMeter('push', {
          bitrateKbps: bili.stream_bitrate_kbps,
          speed: bili.stream_speed,
          totalBytes: bili.stream_total_bytes
        });
        const pushFrame = document.getElementById('bili-network-push-frame');
        if (pushFrame) {
          pushFrame.textContent = `FPS ${formatFps(bili.stream_fps)} / Frame ${formatFrameCount(bili.stream_frame)}`;
        }

        const cacheMeter = document.getElementById('bili-network-cache-meter');
        setElementDisplay(cacheMeter, hasCache, '');
        if (hasCache) {
          updateBiliNetworkMeter('cache', {
            bitrateKbps: bili.stream_cache_bitrate_kbps,
            speed: bili.stream_cache_speed,
            totalBytes: bili.stream_cache_total_bytes
          });
        }

        renderBiliNetworkGraph(hasCache);
      }

      function updateBiliNetworkMeter(kind, metrics) {
        setElementText(`bili-network-${kind}-rate`, formatNetworkRate(metrics.bitrateKbps));
        setElementText(`bili-network-${kind}-speed-ratio`, formatSpeedRatio(metrics.speed));
        setElementText(`bili-network-${kind}-total`, `Total ${formatBytes(metrics.totalBytes)}`);
      }

      async function refreshNetworkStatus() {
        if (networkRefreshInFlight || !lastBiliNetworkLive) {
          return;
        }

        networkRefreshInFlight = true;
        try {
          const result = await getJson('/api/network-status');
          if (result.success && result.data) {
            updateBiliNetworkPanel({
              ...result.data,
              is_live: lastBiliNetworkLive,
              stream_quality: lastBiliNetworkQuality,
            });
          }
        } catch (error) {
          console.debug('Failed to refresh network status:', error);
        } finally {
          networkRefreshInFlight = false;
        }
      }

      async function refreshStatus() {
        if (statusRefreshInFlight) {
          statusRefreshQueued = true;
          return;
        }

        statusRefreshInFlight = true;
        statusRefreshQueued = false;
        lastStatusRefreshMs = Date.now();

        try {
          const data = await getJson('/api/status');

          if (!data.success) {
            // API returned an error
            throw new Error(data.message || '服务器返回错误');
          }

          if (data.success && data.data) {
            // Update Bilibili status
            const bili = data.data.bilibili;
            document.getElementById('bili-status').className =
              `status-indicator ${bili.is_live ? 'status-live' : 'status-offline'}`;
            updateAppLiveBadge(bili.is_live);
            document.getElementById('bili-title').textContent = bili.title || '-';
            document.getElementById('bili-area').textContent =
              bili.area_name ? `${bili.area_name} (${bili.area_id})` : (bili.area_id || '-');

            updateBiliNetworkPanel(bili);

            updateDanmakuCommandToggle(bili.enable_danmaku_command);

            // Update YouTube status
            if (data.data.youtube) {
              const yt = data.data.youtube;
              document.getElementById('yt-status').className =
                `status-indicator ${yt.is_live ? 'status-live' : 'status-offline'}`;
              setPlatformLiveInfoVisibility('youtube', yt.is_live);
              document.getElementById('yt-channel-name').textContent = yt.channel_name || '-';
              document.getElementById('yt-title').textContent = yt.title || '-';
              document.getElementById('yt-topic').textContent = yt.topic || '-';
              document.getElementById('yt-area').textContent =
                yt.area_name ? `${yt.area_name} (${yt.area_id})` : (yt.area_id || '-');
              document.getElementById('yt-quality').textContent = yt.quality ? getQualityDisplayText(yt.quality, 'youtube') : '-';
              // Update crop status
              document.getElementById('yt-crop-status').textContent = yt.crop_enabled ? '开启' : '关闭';
              document.getElementById('yt-hls-cache-status').textContent =
                formatHlsCacheStatus(yt.ffmpeg_cache_enabled, yt.ffmpeg_cache_latency_secs);
            } else {
              document.getElementById('yt-status').className = 'status-indicator status-offline';
              setPlatformLiveInfoVisibility('youtube', false);
              document.getElementById('yt-channel-name').textContent = '-';
              document.getElementById('yt-title').textContent = '-';
              document.getElementById('yt-topic').textContent = '-';
              document.getElementById('yt-area').textContent = '-';
              document.getElementById('yt-quality').textContent = '-';
              document.getElementById('yt-crop-status').textContent = '关闭';
              document.getElementById('yt-hls-cache-status').textContent = '关闭';
            }

            // Update Twitch status
            if (data.data.twitch) {
              const tw = data.data.twitch;
              document.getElementById('tw-status').className =
                `status-indicator ${tw.is_live ? 'status-live' : 'status-offline'}`;
              setPlatformLiveInfoVisibility('twitch', tw.is_live);
              document.getElementById('tw-channel-name').textContent = tw.channel_name || '-';
              document.getElementById('tw-title').textContent = tw.title || '-';
              document.getElementById('tw-game').textContent = tw.game || '-';
              document.getElementById('tw-area').textContent =
                tw.area_name ? `${tw.area_name} (${tw.area_id})` : (tw.area_id || '-');
              document.getElementById('tw-quality').textContent = tw.quality ? getQualityDisplayText(tw.quality, 'twitch') : '-';
              // Update crop status
              document.getElementById('tw-crop-status').textContent = tw.crop_enabled ? '开启' : '关闭';
              document.getElementById('tw-hls-cache-status').textContent =
                formatHlsCacheStatus(tw.ffmpeg_cache_enabled, tw.ffmpeg_cache_latency_secs);
            } else {
              document.getElementById('tw-status').className = 'status-indicator status-offline';
              setPlatformLiveInfoVisibility('twitch', false);
              document.getElementById('tw-channel-name').textContent = '-';
              document.getElementById('tw-title').textContent = '-';
              document.getElementById('tw-game').textContent = '-';
              document.getElementById('tw-area').textContent = '-';
              document.getElementById('tw-quality').textContent = '-';
              document.getElementById('tw-crop-status').textContent = '关闭';
              document.getElementById('tw-hls-cache-status').textContent = '关闭';
            }

            // Update Priority Channel status
          } else {
            console.error('Invalid API response:', data);
            showNotification('获取状态失败：响应格式错误', 'error');
            // Show error state
            document.getElementById('bili-title').textContent = '数据错误';
            document.getElementById('yt-channel-name').textContent = '数据错误';
            document.getElementById('tw-channel-name').textContent = '数据错误';
          }
        } catch (error) {
          console.error('Failed to refresh status:', error);

          // Suppress network errors (when server is down)
          if (error.message && error.message.includes('NetworkError')) {
            return;
          }

          // Show more helpful error messages for other errors
          let errorMsg = error.message;
          if (errorMsg.includes('权限错误') || errorMsg.includes('Permission denied')) {
            errorMsg = '配置文件权限错误！请在可执行文件所在目录运行程序。';
          } else if (errorMsg.includes('不存在') || errorMsg.includes('No such file')) {
            errorMsg = '配置文件不存在！请先运行 "bilistream setup" 创建配置。';
          } else if (errorMsg.includes('Failed to fetch')) {
            // Also suppress generic fetch failures
            return;
          }

          showNotification(errorMsg, 'error');

          // Show connection error state
          document.getElementById('bili-title').textContent = '配置错误';
          document.getElementById('yt-channel-name').textContent = '配置错误';
          document.getElementById('tw-channel-name').textContent = '配置错误';
        } finally {
          statusRefreshInFlight = false;
          schedulePlatformTitleRowCenters();
          if (statusRefreshQueued) {
            refreshStatus();
          }
        }
      }

      async function startStream() {
        try {
          const data = await postJsonApi('/api/start', {});

          // Check if face verification is required
          if (data.data && data.data.requires_face_auth) {
            showFaceAuthModal(data.data.qr_url);
            showNotification(data.message || '需要人脸验证', 'error');
            return;
          }

          // Check if the operation was successful
          if (data.success) {
            showNotification(data.message || '直播已开始', 'success');
          } else {
            showNotification(data.message || '开播失败', 'error');
          }
          setTimeout(refreshStatus, 2000);
        } catch (error) {
          console.error('Start stream error:', error);
          showNotification('操作失败: ' + error.message, 'error');
        }
      }

      async function stopStream() {
        try {
          const data = await postJsonApi('/api/stop');
          showNotification(data.message || (data.success ? '直播已停止' : '停播失败'), data.success ? 'success' : 'error');
          setTimeout(refreshStatus, 2000);
        } catch (error) {
          showNotification('操作失败: ' + error.message, 'error');
        }
      }

      async function restartStream() {
        if (!confirm('确定要停止当前流并重新加载配置吗？')) {
          return;
        }

        const btn = document.getElementById('restartStreamBtn');
        const icon = document.getElementById('restartStreamIcon');

        // Disable button and start spinning animation
        setButtonLoading(btn, icon, true);

        try {
          const data = await postJsonApi('/api/restart');
          showNotification(data.message || (data.success ? '已重启流' : '重启失败'), data.success ? 'success' : 'error');
          setTimeout(refreshStatus, 2000);
        } catch (error) {
          showNotification('操作失败: ' + error.message, 'error');
        } finally {
          // Re-enable button and stop spinning after a delay
          setTimeout(() => {
            setButtonLoading(btn, icon, false);
          }, 2000);
        }
      }

      function postTitleUpdate(title) {
        return postJsonApi('/api/title', { title });
      }

      function postChannelUpdate(payload) {
        return postJsonApi('/api/channel', payload);
      }

      async function changeTitle() {
        const title = document.getElementById('title-input').value.trim();
        if (!title) {
          showNotification('请输入直播标题', 'error');
          return;
        }

        try {
          const data = await postTitleUpdate(title);
          if (data.success) {
            showNotification(data.message || '直播标题已更新', 'success');
            document.getElementById('title-input').value = '';
            setTimeout(refreshStatus, 2000);
          } else {
            showNotification(data.message || '更新失败', 'error');
          }
        } catch (error) {
          showNotification('更新失败: ' + error.message, 'error');
        }
      }

      // Inline row edit helpers
      function showInfoRowEdit(valueEl, editContainer) {
        const valueContainer = valueEl?.parentElement;
        if (!valueContainer || !editContainer) {
          return;
        }

        valueContainer.classList.add('hidden');
        valueContainer.style.display = '';
        editContainer.classList.remove('hidden');
        editContainer.style.display = '';
      }

      function hideInfoRowEdit(valueEl, editContainer) {
        const valueContainer = valueEl?.parentElement;
        if (!valueContainer || !editContainer) {
          return;
        }

        editContainer.classList.add('hidden');
        editContainer.style.display = '';
        valueContainer.classList.remove('hidden');
        valueContainer.style.display = '';
      }

      function handleTitleEditKeydown(event) {
        if (event.key === 'Enter') {
          saveTitleEdit();
        } else if (event.key === 'Escape') {
          cancelTitleEdit();
        }
      }

      // Inline title editing functions
      function toggleTitleEdit() {
        const titleSpan = document.getElementById('bili-title');
        const editContainer = document.getElementById('title-edit-container');
        const editInput = document.getElementById('title-edit-input');

        showInfoRowEdit(titleSpan, editContainer);

        // Set current title as input value
        editInput.value = titleSpan.textContent === '-' ? '' : titleSpan.textContent;
        editInput.focus();
        editInput.select();
      }

      function cancelTitleEdit() {
        const titleSpan = document.getElementById('bili-title');
        const editContainer = document.getElementById('title-edit-container');

        hideInfoRowEdit(titleSpan, editContainer);
      }

      // YouTube channel editing functions
      function toggleYtChannelEdit() {
        const channelSpan = document.getElementById('yt-channel-name');
        const editContainer = document.getElementById('yt-channel-edit-container');
        const editSelect = document.getElementById('yt-channel-edit-select');

        showInfoRowEdit(channelSpan, editContainer);

        // Load channels if not already loaded
        if (!channelsData) {
          loadChannelData().then(() => {
            populateYtChannelEditSelect();
          });
        } else {
          populateYtChannelEditSelect();
        }

        editSelect.focus();
      }

      function populateYtChannelEditSelect() {
        const editSelect = document.getElementById('yt-channel-edit-select');
        editSelect.replaceChildren(createSelectOption('', '选择频道...'));
        appendPlatformChannelOptions(editSelect, 'youtube');
      }

      function cancelYtChannelEdit() {
        const channelSpan = document.getElementById('yt-channel-name');
        const editContainer = document.getElementById('yt-channel-edit-container');

        hideInfoRowEdit(channelSpan, editContainer);
      }

      async function saveYtChannelEdit() {
        const editSelect = document.getElementById('yt-channel-edit-select');
        const selectedValue = editSelect.value;

        if (!selectedValue) {
          showNotification('请选择频道', 'error');
          return;
        }

        try {
          const channelInfo = JSON.parse(selectedValue);
          const data = await postChannelUpdate({
            platform: 'youtube',
            channel_id: channelInfo.id,
            channel_name: channelInfo.name
          });

          if (!data.success) {
            showNotification(data.message || 'YouTube频道更新失败', 'error');
            return;
          }

          showNotification(data.message || 'YouTube频道已更新', 'success');
          cancelYtChannelEdit();
          // Refresh status immediately to show updated info
          refreshStatus();
        } catch (error) {
          showNotification('更新失败: ' + error.message, 'error');
        }
      }

      // YouTube area editing functions
      function toggleYtAreaEdit() {
        const areaSpan = document.getElementById('yt-area');
        const editContainer = document.getElementById('yt-area-edit-container');
        const editSelect = document.getElementById('yt-area-edit-select');

        showInfoRowEdit(areaSpan, editContainer);

        // Load areas if not already loaded
        if (!areasData) {
          loadChannelData().then(() => {
            populateYtAreaEditSelect();
          });
        } else {
          populateYtAreaEditSelect();
        }

        editSelect.focus();
      }

      function populateYtAreaEditSelect() {
        const editSelect = document.getElementById('yt-area-edit-select');
        editSelect.replaceChildren(createAreaOption('', '选择分区...'));

        if (areasData && areasData.areas) {
          appendAreaOptions(editSelect, areasData.areas);
        }
      }

      function cancelYtAreaEdit() {
        const areaSpan = document.getElementById('yt-area');
        const editContainer = document.getElementById('yt-area-edit-container');

        hideInfoRowEdit(areaSpan, editContainer);
      }

      async function saveYtAreaEdit() {
        const editSelect = document.getElementById('yt-area-edit-select');
        const areaId = parseInteger(editSelect.value, 0);

        if (!areaId) {
          showNotification('请选择分区', 'error');
          return;
        }

        try {
          const data = await postChannelUpdate({
            platform: 'youtube',
            area_id: areaId
          });

          if (!data.success) {
            showNotification(data.message || 'YouTube配置分区更新失败', 'error');
            return;
          }

          showNotification(data.message || 'YouTube配置分区已更新', 'success');
          cancelYtAreaEdit();
          // Refresh status immediately to show updated info
          refreshStatus();
        } catch (error) {
          showNotification('更新失败: ' + error.message, 'error');
        }
      }

      // YouTube quality editing functions
      function toggleYtQualityEdit() {
        const qualitySpan = document.getElementById('yt-quality');
        const editContainer = document.getElementById('yt-quality-edit-container');
        const editSelect = document.getElementById('yt-quality-edit-select');

        showInfoRowEdit(qualitySpan, editContainer);

        // Populate quality options for YouTube
        populateQualityOptions(editSelect, 'youtube');

        // Set current quality as selected - convert display text back to technical value
        const currentDisplayText = qualitySpan.textContent;
        if (currentDisplayText && currentDisplayText !== '-') {
          const technicalValue = getTechnicalQualityValue(currentDisplayText, 'youtube');
          editSelect.value = technicalValue;
        }

        editSelect.focus();
      }

      function cancelYtQualityEdit() {
        const qualitySpan = document.getElementById('yt-quality');
        const editContainer = document.getElementById('yt-quality-edit-container');

        hideInfoRowEdit(qualitySpan, editContainer);
      }

      async function saveYtQualityEdit() {
        const editSelect = document.getElementById('yt-quality-edit-select');
        const quality = editSelect.value;

        if (!quality) {
          showNotification('请选择画质', 'error');
          return;
        }

        try {
          const data = await postChannelUpdate({
            platform: 'youtube',
            quality // Send technical value to server
          });

          if (!data.success) {
            showNotification(data.message || 'YouTube画质更新失败', 'error');
            return;
          }

          showNotification(data.message || 'YouTube画质已更新', 'success');
          cancelYtQualityEdit();
          // Update the display immediately with display text
          document.getElementById('yt-quality').textContent = getQualityDisplayText(quality, 'youtube');
          // Refresh status to get updated info
          refreshStatus();
        } catch (error) {
          showNotification('更新失败: ' + error.message, 'error');
        }
      }

      // Twitch quality editing functions
      function toggleTwQualityEdit() {
        const qualitySpan = document.getElementById('tw-quality');
        const editContainer = document.getElementById('tw-quality-edit-container');
        const editSelect = document.getElementById('tw-quality-edit-select');

        showInfoRowEdit(qualitySpan, editContainer);

        // Populate quality options for Twitch
        populateQualityOptions(editSelect, 'twitch');

        // Set current quality as selected - convert display text back to technical value
        const currentDisplayText = qualitySpan.textContent;
        if (currentDisplayText && currentDisplayText !== '-') {
          const technicalValue = getTechnicalQualityValue(currentDisplayText, 'twitch');
          editSelect.value = technicalValue;
        }

        editSelect.focus();
      }

      function cancelTwQualityEdit() {
        const qualitySpan = document.getElementById('tw-quality');
        const editContainer = document.getElementById('tw-quality-edit-container');

        hideInfoRowEdit(qualitySpan, editContainer);
      }

      async function saveTwQualityEdit() {
        const editSelect = document.getElementById('tw-quality-edit-select');
        const quality = editSelect.value;

        if (!quality) {
          showNotification('请选择画质', 'error');
          return;
        }

        try {
          const data = await postChannelUpdate({
            platform: 'twitch',
            quality // Send technical value to server
          });

          if (!data.success) {
            showNotification(data.message || 'Twitch画质更新失败', 'error');
            return;
          }

          showNotification(data.message || 'Twitch画质已更新', 'success');
          cancelTwQualityEdit();
          // Update the display immediately with display text
          document.getElementById('tw-quality').textContent = getQualityDisplayText(quality, 'twitch');
          // Refresh status to get updated info
          refreshStatus();
        } catch (error) {
          showNotification('更新失败: ' + error.message, 'error');
        }
      }

      // Twitch channel editing functions
      function toggleTwChannelEdit() {
        const channelSpan = document.getElementById('tw-channel-name');
        const editContainer = document.getElementById('tw-channel-edit-container');
        const editSelect = document.getElementById('tw-channel-edit-select');

        showInfoRowEdit(channelSpan, editContainer);

        // Load channels if not already loaded
        if (!channelsData) {
          loadChannelData().then(() => {
            populateTwChannelEditSelect();
          });
        } else {
          populateTwChannelEditSelect();
        }

        editSelect.focus();
      }

      function populateTwChannelEditSelect() {
        const editSelect = document.getElementById('tw-channel-edit-select');
        editSelect.replaceChildren(createSelectOption('', '选择频道...'));
        appendPlatformChannelOptions(editSelect, 'twitch');
      }

      function cancelTwChannelEdit() {
        const channelSpan = document.getElementById('tw-channel-name');
        const editContainer = document.getElementById('tw-channel-edit-container');

        hideInfoRowEdit(channelSpan, editContainer);
      }

      async function saveTwChannelEdit() {
        const editSelect = document.getElementById('tw-channel-edit-select');
        const selectedValue = editSelect.value;

        if (!selectedValue) {
          showNotification('请选择频道', 'error');
          return;
        }

        try {
          const channelInfo = JSON.parse(selectedValue);
          const data = await postChannelUpdate({
            platform: 'twitch',
            channel_id: channelInfo.id,
            channel_name: channelInfo.name
          });

          if (!data.success) {
            showNotification(data.message || 'Twitch频道更新失败', 'error');
            return;
          }

          showNotification(data.message || 'Twitch频道已更新', 'success');
          cancelTwChannelEdit();
          // Refresh status immediately to show updated info
          refreshStatus();
        } catch (error) {
          showNotification('更新失败: ' + error.message, 'error');
        }
      }

      // Twitch area editing functions
      function toggleTwAreaEdit() {
        const areaSpan = document.getElementById('tw-area');
        const editContainer = document.getElementById('tw-area-edit-container');
        const editSelect = document.getElementById('tw-area-edit-select');

        showInfoRowEdit(areaSpan, editContainer);

        // Load areas if not already loaded
        if (!areasData) {
          loadChannelData().then(() => {
            populateTwAreaEditSelect();
          });
        } else {
          populateTwAreaEditSelect();
        }

        editSelect.focus();
      }

      function populateTwAreaEditSelect() {
        const editSelect = document.getElementById('tw-area-edit-select');
        editSelect.replaceChildren(createAreaOption('', '选择分区...'));

        if (areasData && areasData.areas) {
          appendAreaOptions(editSelect, areasData.areas);
        }
      }

      function cancelTwAreaEdit() {
        const areaSpan = document.getElementById('tw-area');
        const editContainer = document.getElementById('tw-area-edit-container');

        hideInfoRowEdit(areaSpan, editContainer);
      }

      async function saveTwAreaEdit() {
        const editSelect = document.getElementById('tw-area-edit-select');
        const areaId = parseInteger(editSelect.value, 0);

        if (!areaId) {
          showNotification('请选择分区', 'error');
          return;
        }

        try {
          const data = await postChannelUpdate({
            platform: 'twitch',
            area_id: areaId
          });

          if (!data.success) {
            showNotification(data.message || 'Twitch配置分区更新失败', 'error');
            return;
          }

          showNotification(data.message || 'Twitch配置分区已更新', 'success');
          cancelTwAreaEdit();
          // Refresh status immediately to show updated info
          refreshStatus();
        } catch (error) {
          showNotification('更新失败: ' + error.message, 'error');
        }
      }

      // Area editing functions
      function toggleAreaEdit() {
        const areaSpan = document.getElementById('bili-area');
        const editContainer = document.getElementById('area-edit-container');
        const editSelect = document.getElementById('area-edit-select');

        showInfoRowEdit(areaSpan, editContainer);

        // Load areas if not already loaded
        if (!areasData) {
          loadChannelData().then(() => {
            populateAreaEditSelect();
          });
        } else {
          populateAreaEditSelect();
        }

        editSelect.focus();
      }

      function populateAreaEditSelect() {
        const editSelect = document.getElementById('area-edit-select');
        editSelect.replaceChildren(createAreaOption('', '选择分区...'));
        appendAreaOptions(editSelect, getAreaList());
      }

      function cancelAreaEdit() {
        const areaSpan = document.getElementById('bili-area');
        const editContainer = document.getElementById('area-edit-container');

        hideInfoRowEdit(areaSpan, editContainer);
      }

      async function saveAreaEdit() {
        const editSelect = document.getElementById('area-edit-select');
        const areaId = parseInteger(editSelect.value, 0);

        if (!areaId) {
          showNotification('请选择分区', 'error');
          return;
        }

        try {
          const data = await postJsonApi('/api/area', { area_id: areaId });
          if (!data.success) {
            showNotification(data.message || '分区更新失败', 'error');
            return;
          }

          showNotification(data.message || '分区已更新', 'success');
          cancelAreaEdit();
          setTimeout(refreshStatus, 2000);
        } catch (error) {
          showNotification('更新失败: ' + error.message, 'error');
        }
      }

      async function saveTitleEdit() {
        const editInput = document.getElementById('title-edit-input');
        const title = editInput.value.trim();

        if (!title) {
          showNotification('请输入直播标题', 'error');
          return;
        }

        try {
          const data = await postTitleUpdate(title);
          if (data.success) {
            showNotification(data.message || '直播标题已更新', 'success');
            cancelTitleEdit();
            setTimeout(refreshStatus, 2000);
          } else {
            showNotification(data.message || '更新失败', 'error');
          }
        } catch (error) {
          showNotification('更新失败: ' + error.message, 'error');
        }
      }

      // Channel management functions
      async function loadChannelData() {
        // Start continuous spinning animation
        const icon = document.getElementById('loadChannelIcon');
        setButtonLoading(null, icon, true);

        try {
          const [channelsResult, areasResult] = await Promise.all([
            getJson('/api/channels'),
            getJson('/api/areas')
          ]);
          channelsData = channelsResult;
          areasData = normalizeAreaData(areasResult);

          // Populate the removed legacy channel-management area select if present.
          const areaSelect = document.getElementById('area-select');
          if (areaSelect) {
            areaSelect.replaceChildren(createAreaOption('', '不修改分区'));
          }

          const areasList = getAreaList();

          if (areasList.length > 0) {
            if (areaSelect) {
              // Populate channel management area select
              appendAreaOptions(areaSelect, areasList, true);
            }

          } else {
            console.error('No areas data found:', areasData);
            showNotification('未找到分区数据', 'error');
          }

          // Update the removed legacy channel list if the controls still exist.
          if (document.getElementById('platform-select') && document.getElementById('channel-select')) {
            updateChannelList();
          }
        } catch (error) {
          console.error('Failed to load channel data:', error);
          showNotification('加载频道数据失败: ' + error.message, 'error');
        } finally {
          // Stop spinning animation when complete
          setButtonLoading(null, icon, false);
        }
      }

      function updateChannelList() {
        const platformSelect = document.getElementById('platform-select');
        const channelSelect = document.getElementById('channel-select');
        if (!platformSelect || !channelSelect) return;

        const platform = platformSelect.value;

        channelSelect.replaceChildren(createSelectOption('', '从 channels.json 选择或手动输入...'));
        appendPlatformChannelOptions(channelSelect, platform);

        // Don't clear inputs - allow manual entry to persist
        // Update quality options based on platform
        updateQualityOptions();
      }

      function updateQualityOptions() {
        const platformSelect = document.getElementById('platform-select');
        const qualitySelect = document.getElementById('quality-select');
        if (!platformSelect || !qualitySelect) return;

        const platform = platformSelect.value;

        // Store current value
        const currentValue = qualitySelect.value;

        // Clear all options except the first "不修改画质" option
        const firstOption = qualitySelect.querySelector('option[value=""]');
        if (firstOption) {
          qualitySelect.replaceChildren(firstOption);
        } else {
          qualitySelect.replaceChildren();
        }

        // Add platform-specific quality options if platform is selected
        if (platform) {
          populateQualityOptions(qualitySelect, platform, false); // false = don't add empty option
        }

        // Try to restore previous value if it's still valid
        if (currentValue && qualitySelect.querySelector(`option[value="${currentValue}"]`)) {
          qualitySelect.value = currentValue;
        }
      }

      function updateChannelInfo() {
        const channelSelect = document.getElementById('channel-select');
        const selectedValue = channelSelect.value;

        if (!selectedValue) {
          document.getElementById('channel-id-input').value = '';
          document.getElementById('channel-name-input').value = '';
          return;
        }

        try {
          const channelInfo = JSON.parse(selectedValue);
          document.getElementById('channel-id-input').value = channelInfo.id;
          document.getElementById('channel-name-input').value = channelInfo.name;
        } catch (error) {
          console.error('Failed to parse channel info:', error);
        }
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

      // Quality mapping functions for display vs technical values
      function getQualityDisplayText(technicalValue, platform = 'youtube') {
        const qualityMappings = {
          youtube: {
            'best': '最佳质量',
            'best[height<=1080]': '超清 (1080p)',
            'best[height<=720]': '高清 (720p)',
            'best[height<=480]': '标清 (480p)',
            'best[height<=360]': '流畅 (360p)',
            'worst': '最低质量'
          },
          twitch: {
            'best': '原画质量',
            'high': '高质量 (720p)',
            'medium': '中等质量 (540p)',
            'low': '低质量 (360p)',
            'audio_only': '仅音频',
            'worst': '最低质量'
          }
        };

        return qualityMappings[platform][technicalValue] || technicalValue;
      }

      function getTechnicalQualityValue(displayText, platform = 'youtube') {
        const reverseMappings = {
          youtube: {
            '最佳质量': 'best',
            '超清 (1080p)': 'best[height<=1080]',
            '高清 (720p)': 'best[height<=720]',
            '标清 (480p)': 'best[height<=480]',
            '流畅 (360p)': 'best[height<=360]',
            '最低质量': 'worst'
          },
          twitch: {
            '原画质量': 'best',
            '高质量 (720p)': 'high',
            '中等质量 (540p)': 'medium',
            '低质量 (360p)': 'low',
            '仅音频': 'audio_only',
            '最低质量': 'worst'
          }
        };

        return reverseMappings[platform][displayText] || displayText;
      }

      // Unified function to populate quality options for any select element
      function populateQualityOptions(selectElement, platform, includeEmpty = true) {
        if (!selectElement) return;

        // Clear existing options
        selectElement.replaceChildren();

        // Add empty option if requested
        if (includeEmpty) {
          selectElement.appendChild(createSelectOption('', '选择画质...'));
        }

        // Get quality mappings for the platform
        const qualityMappings = {
          youtube: {
            'best': '最佳质量',
            'best[height<=1080]': '超清 (1080p)',
            'best[height<=720]': '高清 (720p)',
            'best[height<=480]': '标清 (480p)',
            'best[height<=360]': '流畅 (360p)',
            'worst': '最低质量'
          },
          twitch: {
            'best': '原画质量',
            'high': '高质量 (720p)',
            'medium': '中等质量 (540p)',
            'low': '低质量 (360p)',
            'audio_only': '仅音频',
            'worst': '最低质量'
          }
        };

        // Add options for the specified platform
        const platformQualities = qualityMappings[platform];
        if (platformQualities) {
          Object.entries(platformQualities).forEach(([value, text]) => {
            const option = document.createElement('option');
            option.value = value;
            option.textContent = text;
            selectElement.appendChild(option);
          });
        }
      }

      async function applyChannelChange() {
        const platform = document.getElementById('platform-select').value;
        const channelId = document.getElementById('channel-id-input').value.trim();
        const channelName = document.getElementById('channel-name-input').value.trim();
        const areaId = document.getElementById('area-select').value;
        const quality = document.getElementById('quality-select').value;
        const riotApiKey = document.getElementById('riot-api-key-input').value.trim();
        const enableLolMonitor = document.getElementById('enable-lol-monitor-inline').checked;

        if (!channelId || !channelName) {
          showNotification('请输入频道ID和频道名称', 'error');
          return;
        }

        try {
          // First, update channel info
          const channelPayload = {
            platform: platform,
            channel_id: channelId,
            channel_name: channelName
          };

          if (areaId) {
            channelPayload.area_id = parseInteger(areaId, 0);

            // Include Riot API Key if area is 86 and key is provided
            if (areaId === '86' && riotApiKey) {
              channelPayload.riot_api_key = riotApiKey;
            }
          }

          if (quality) {
            channelPayload.quality = quality;
          }

          const channelData = await postJsonApi('/api/channel', channelPayload);
          if (!channelData.success) {
            showNotification(channelData.message || '频道更新失败', 'error');
            return;
          }

          // If area is 86, also update enable_lol_monitor setting
          if (areaId === '86') {
            const configPayload = {
              enable_lol_monitor: enableLolMonitor
            };

            // Include Riot API Key in config update if provided
            if (riotApiKey) {
              configPayload.riot_api_key = riotApiKey;
            }

            const configData = await postJsonApi('/api/config', configPayload);
            if (!configData.success) {
              showNotification(configData.message || 'LoL 监控配置更新失败', 'error');
              return;
            }

            // Update global config
            window.configData.enable_lol_monitor = enableLolMonitor;
            if (riotApiKey) {
              window.configData.riot_api_key = riotApiKey;
            }
          }

          showNotification(channelData.message || '频道已更新', 'success');

          // Clear Riot API Key input after successful update
          if (riotApiKey) {
            document.getElementById('riot-api-key-input').value = '';
          }

          setTimeout(refreshStatus, 2000);
        } catch (error) {
          showNotification('更新失败: ' + error.message, 'error');
        }
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
              if (areasList.some(area => area.id === 235)) {
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

      // Check for updates function
      let CURRENT_VERSION = null; // Will be fetched from API
      let IS_TAURI = false; // Will be fetched from API
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

      // Load theme on page load
      loadTheme();

      // Check setup status on page load
      ensureWebUiAccess().then(() => checkSetupStatus()).then(needsSetup => {
        if (!needsSetup) {
          // Load version first
          loadVersion();

          // Only load data if setup is complete
          initStatusRefresh(); // This will load config and start auto-refresh
          loadChannelData();

          // Auto-check for updates after 2 seconds
          setTimeout(autoCheckUpdates, 2000);
        }
      });

      // Add event listener for switch buttons using data attributes
      document.addEventListener('click', function (e) {
        const addStart = e.target.closest('.holodex-add-channel-start');
        if (addStart) {
          const control = addStart.closest('.holodex-channel-add');
          if (control) setHolodexAddChannelConfirmVisible(control, true);
          return;
        }

        const addReject = e.target.closest('.holodex-add-channel-reject');
        if (addReject) {
          const control = addReject.closest('.holodex-channel-add');
          if (control) setHolodexAddChannelConfirmVisible(control, false);
          return;
        }

        const addConfirm = e.target.closest('.holodex-add-channel-confirm');
        if (addConfirm) {
          confirmHolodexAddChannel(addConfirm);
          return;
        }

        const switchButton = e.target.closest('.switch-button');
        if (switchButton) {
          const action = readHolodexStreamActionData(switchButton);
          switchToHolodexStream(
            action.channelId,
            action.suggestedAreaId,
            action.title,
            action.topicId,
            action.status,
            action.platform,
            action.twitchChannelId,
            action.externalLink
          );
          return;
        }

        const cropSwitchButton = e.target.closest('.crop-switch-button');
        if (cropSwitchButton) {
          const action = readHolodexStreamActionData(cropSwitchButton);
          cropAndSwitchToHolodexStream(
            action.channelId,
            action.suggestedAreaId,
            action.title,
            action.topicId,
            action.status,
            action.platform,
            action.twitchChannelId,
            action.externalLink
          );
          return;
        }
      });

      // YouTube cookies editing functions
      function toggleYtCookiesEdit() {
        const statusSpan = document.getElementById('yt-cookies-status');
        const editContainer = document.getElementById('yt-cookies-edit-container');

        showInfoRowEdit(statusSpan, editContainer);
      }

      function cancelYtCookiesEdit() {
        const statusSpan = document.getElementById('yt-cookies-status');
        const editContainer = document.getElementById('yt-cookies-edit-container');

        hideInfoRowEdit(statusSpan, editContainer);
      }

      async function saveYtCookiesEdit() {
        const browserSelect = document.getElementById('yt-cookies-browser-select');
        const fileInput = document.getElementById('yt-cookies-file-input');

        const cookiesFromBrowser = browserSelect.value;
        const cookiesFile = fileInput.value.trim();

        try {
          const data = await postChannelUpdate({
            platform: 'youtube',
            cookies_from_browser: cookiesFromBrowser,
            cookies_file: cookiesFile
          });
          if (!data.success) {
            showNotification(data.message || 'YouTube Cookies 更新失败', 'error');
            return;
          }

          showNotification(data.message || 'YouTube Cookies 已更新', 'success');

          // Update status display
          updateYtCookiesStatus(cookiesFromBrowser, cookiesFile);

          cancelYtCookiesEdit();
          refreshStatus();
        } catch (error) {
          showNotification('更新失败: ' + error.message, 'error');
        }
      }

      function updateYtCookiesStatus(browser, file) {
        const statusSpan = document.getElementById('yt-cookies-status');
        if (browser) {
          statusSpan.textContent = `浏览器: ${browser}`;
        } else if (file) {
          statusSpan.textContent = `文件: ${file.split('/').pop()}`;
        } else {
          statusSpan.textContent = '未配置';
        }
      }

      // Crop Configuration Functions
      let cropImage = null;
      let cropStartX = 0;
      let cropStartY = 0;
      let isDrawing = false;
      let cropCanvas = null;
      let cropCtx = null;

      function readCropRect() {
        return {
          x: readIntegerInput('cropX', 0) || 0,
          y: readIntegerInput('cropY', 0) || 0,
          width: readIntegerInput('cropWidth', 0) || 0,
          height: readIntegerInput('cropHeight', 0) || 0
        };
      }

      function validateCropRect(rect) {
        if (rect.x < 0 || rect.y < 0) {
          return '裁剪区域坐标不能为负数';
        }
        if (rect.width <= 0 || rect.height <= 0) {
          return '请先输入有效的裁剪区域尺寸';
        }
        return '';
      }

      function setCropRectInputs(rect) {
        setInputValue('cropX', Math.round(rect.x));
        setInputValue('cropY', Math.round(rect.y));
        setInputValue('cropWidth', Math.round(rect.width));
        setInputValue('cropHeight', Math.round(rect.height));
      }

      function positionCropBox(cropBox, rect, scaleX, scaleY) {
        if (!cropBox) return;

        setElementDisplay(cropBox, true);
        cropBox.style.left = (rect.x / scaleX) + 'px';
        cropBox.style.top = (rect.y / scaleY) + 'px';
        cropBox.style.width = (rect.width / scaleX) + 'px';
        cropBox.style.height = (rect.height / scaleY) + 'px';
      }

      function bindCropRectInputListeners() {
        ['cropX', 'cropY', 'cropWidth', 'cropHeight'].forEach(id => {
          const input = document.getElementById(id);
          if (!input) {
            return;
          }
          input.removeEventListener('input', updateCropBox);
          input.addEventListener('input', updateCropBox);
        });
      }

      function showCropCanvasContainer() {
        document.getElementById('cropCanvasContainer')?.classList.remove('hidden');
      }

      function hideCropCanvasContainer() {
        document.getElementById('cropCanvasContainer')?.classList.add('hidden');
      }

      function setCropStatusLabel(platform, label) {
        const statusId = platform === 'youtube' ? 'yt-crop-status' : 'tw-crop-status';
        setElementText(statusId, label);
      }

      function createCropUpdatePayload(platform, enabled, rect = {}) {
        return {
          platform,
          enabled,
          ...rect
        };
      }

      function postCropUpdate(payload) {
        return postJsonApi('/api/crop/update', payload);
      }

      function openCropConfig(platform) {
        document.getElementById('cropModal').classList.add('active');
        if (platform) {
          document.getElementById('cropPlatform').value = platform;
        }
        loadCurrentCropSettings();
        // Auto-capture frame when opening from platform cards
        if (platform) {
          setTimeout(() => autoCaptureFrame(), 300);
        }
      }

      function closeCropModal() {
        document.getElementById('cropModal').classList.remove('active');
        // Reset canvas
        hideCropCanvasContainer();
        cropImage = null;
      }

      async function clearCropConfig(platform) {
        if (!confirm(`确定要清除${platform === 'youtube' ? 'YouTube' : 'Twitch'}的裁剪设置吗？`)) {
          return;
        }

        try {
          const result = await postCropUpdate(createCropUpdatePayload(platform, false));

          if (result.success) {
            showNotification('裁剪设置已清除', 'success');
            setCropStatusLabel(platform, '关闭');
          } else {
            showNotification(result.message || '清除失败', 'error');
          }
        } catch (error) {
          showNotification('清除失败: ' + error.message, 'error');
        }
      }

      async function autoCaptureFrame() {
        const platform = document.getElementById('cropPlatform').value;

        showNotification('正在捕获直播帧...', 'info');

        try {
          const result = await postJsonApi(`/api/crop/capture/${platform}`);

          if (result.success && result.message) {
            // Load the captured image (base64 is in message field)
            cropImage = new Image();
            cropImage.onload = function () {
              cropCanvas = document.getElementById('cropCanvas');
              cropCtx = cropCanvas.getContext('2d');

              // Show container FIRST so canvas has dimensions
              showCropCanvasContainer();

              // Set canvas size to image size
              cropCanvas.width = cropImage.width;
              cropCanvas.height = cropImage.height;

              // Fill with white background before drawing transparent sources.
              cropCtx.fillStyle = 'white';
              cropCtx.fillRect(0, 0, cropCanvas.width, cropCanvas.height);

              // Draw image
              cropCtx.drawImage(cropImage, 0, 0);

              // Setup canvas interaction
              setupCanvasInteraction();

              showNotification('直播帧已捕获，请在图片上选择裁剪区域', 'success');
            };
            cropImage.onerror = function (e) {
              console.error('Image load error:', e);
              showNotification('图片加载失败', 'error');
            };
            cropImage.src = result.message;
          } else {
            console.error('Capture failed:', result);
            showNotification(result.message || '捕获失败，请确保直播正在进行', 'error');
          }
        } catch (error) {
          console.error('Capture error:', error);
          showNotification('捕获失败: ' + error.message, 'error');
        }
      }

      async function loadCurrentCropSettings() {
        const platform = document.getElementById('cropPlatform')?.value;
        if (!platform) return;

        try {
          const result = await getJson(`/api/crop/${platform}`);
          if (result.success && result.data && result.data.enabled) {
            setCropRectInputs(result.data);
          }
        } catch (error) {
          console.error('Failed to load crop settings:', error);
        }
      }

      function loadCropImage(event) {
        const file = event.target.files?.[0];
        if (!file) return;

        const reader = new FileReader();
        reader.onload = function (e) {
          cropImage = new Image();
          cropImage.onload = function () {
            cropCanvas = document.getElementById('cropCanvas');
            cropCtx = cropCanvas.getContext('2d');

            // Show container FIRST so canvas has dimensions
            showCropCanvasContainer();

            // Set canvas size to image size
            cropCanvas.width = cropImage.width;
            cropCanvas.height = cropImage.height;

            // Fill with white background before drawing transparent sources.
            cropCtx.fillStyle = 'white';
            cropCtx.fillRect(0, 0, cropCanvas.width, cropCanvas.height);

            // Draw image
            cropCtx.drawImage(cropImage, 0, 0);

            // Setup canvas interaction
            setupCanvasInteraction();

            showNotification('图片已加载，请在图片上拖动鼠标选择裁剪区域', 'success');
          };
          cropImage.onerror = function (err) {
            console.error('Image load error:', err);
            showNotification('图片加载失败', 'error');
          };
          cropImage.src = e.target.result;
        };
        reader.onerror = function (err) {
          console.error('FileReader error:', err);
          showNotification('文件读取失败', 'error');
        };
        reader.readAsDataURL(file);
      }

      let currentAspectRatio = 'free';
      let isDragging = false;
      let dragStartX = 0;
      let dragStartY = 0;
      let boxStartX = 0;
      let boxStartY = 0;
      let boxStartWidth = 0;
      let boxStartHeight = 0;

      const CROP_ASPECT_RATIOS = {
        '1:1': 1,
        '16:9': 16 / 9,
        '16:10': 16 / 10,
        '9:16': 9 / 16,
        '10:16': 10 / 16
      };

      function getCropAspectRatioValue(ratio) {
        return CROP_ASPECT_RATIOS[ratio] || 1;
      }

      function createCenteredCropRect(canvas, ratio) {
        const canvasWidth = canvas.width;
        const canvasHeight = canvas.height;
        const ratioValue = getCropAspectRatioValue(ratio);
        const maxWidth = canvasWidth * 0.6;
        const maxHeight = canvasHeight * 0.6;

        let width;
        let height;
        if (ratioValue >= 1) {
          width = maxWidth;
          height = width / ratioValue;
          if (height > maxHeight) {
            height = maxHeight;
            width = height * ratioValue;
          }
        } else {
          height = maxHeight;
          width = height * ratioValue;
          if (width > maxWidth) {
            width = maxWidth;
            height = width / ratioValue;
          }
        }

        return {
          x: (canvasWidth - width) / 2,
          y: (canvasHeight - height) / 2,
          width,
          height
        };
      }

      function applyAspectRatio() {
        currentAspectRatio = document.getElementById('cropAspectRatio').value;

        if (!cropCanvas || currentAspectRatio === 'free') return;

        setCropRectInputs(createCenteredCropRect(cropCanvas, currentAspectRatio));

        updateCropBox();
        showNotification(`已应用 ${currentAspectRatio} 比例`, 'success');
      }

      function setupCanvasInteraction() {
        const canvas = document.getElementById('cropCanvas');
        const cropBox = document.getElementById('cropBox');

        // Remove old listeners by cloning
        const newCanvas = canvas.cloneNode(true);
        canvas.parentNode.replaceChild(newCanvas, canvas);
        cropCanvas = newCanvas;

        // Redraw the image on the new canvas
        if (cropImage) {
          cropCtx = cropCanvas.getContext('2d');
          cropCtx.fillStyle = 'white';
          cropCtx.fillRect(0, 0, cropCanvas.width, cropCanvas.height);
          cropCtx.drawImage(cropImage, 0, 0);
        }

        let resizeEdge = null; // 'n', 's', 'e', 'w', 'ne', 'nw', 'se', 'sw'
        const edgeThreshold = 10; // pixels from edge to trigger resize

        function getResizeEdge(mouseX, mouseY, x, y, width, height) {
          const threshold = edgeThreshold;
          let edge = '';

          // Check vertical edges
          if (Math.abs(mouseY - y) < threshold) edge += 'n';
          else if (Math.abs(mouseY - (y + height)) < threshold) edge += 's';

          // Check horizontal edges
          if (Math.abs(mouseX - x) < threshold) edge += 'w';
          else if (Math.abs(mouseX - (x + width)) < threshold) edge += 'e';

          return edge || null;
        }

        function getCursorForEdge(edge) {
          if (!edge) return 'crosshair';
          const cursors = {
            'n': 'ns-resize',
            's': 'ns-resize',
            'e': 'ew-resize',
            'w': 'ew-resize',
            'ne': 'nesw-resize',
            'nw': 'nwsw-resize',
            'se': 'nwsw-resize',
            'sw': 'nesw-resize'
          };
          return cursors[edge] || 'crosshair';
        }

        newCanvas.addEventListener('mousedown', (e) => {
          const rect = newCanvas.getBoundingClientRect();
          const scaleX = newCanvas.width / rect.width;
          const scaleY = newCanvas.height / rect.height;

          const mouseX = (e.clientX - rect.left) * scaleX;
          const mouseY = (e.clientY - rect.top) * scaleY;

          const { x, y, width, height } = readCropRect();

          // Check if clicking on edge for resizing
          if (mouseX >= x - edgeThreshold && mouseX <= x + width + edgeThreshold &&
            mouseY >= y - edgeThreshold && mouseY <= y + height + edgeThreshold) {
            resizeEdge = getResizeEdge(mouseX, mouseY, x, y, width, height);
            if (resizeEdge) {
              isDrawing = true;
              cropStartX = mouseX;
              cropStartY = mouseY;
              boxStartX = x;
              boxStartY = y;
              boxStartWidth = width;
              boxStartHeight = height;
              return;
            }
          }

          // Check if clicking inside existing box for dragging
          if (mouseX >= x && mouseX <= x + width && mouseY >= y && mouseY <= y + height) {
            isDragging = true;
            dragStartX = mouseX;
            dragStartY = mouseY;
            boxStartX = x;
            boxStartY = y;
            newCanvas.style.cursor = 'move';
          } else {
            // Start new selection
            cropStartX = mouseX;
            cropStartY = mouseY;
            isDrawing = true;
            resizeEdge = null;

            positionCropBox(cropBox, { x: mouseX, y: mouseY, width: 0, height: 0 }, scaleX, scaleY);
          }
        });

        newCanvas.addEventListener('mousemove', (e) => {
          const rect = newCanvas.getBoundingClientRect();
          const scaleX = newCanvas.width / rect.width;
          const scaleY = newCanvas.height / rect.height;

          const currentX = (e.clientX - rect.left) * scaleX;
          const currentY = (e.clientY - rect.top) * scaleY;

          const { x, y, width, height } = readCropRect();

          // Update cursor based on position
          if (!isDrawing && !isDragging) {
            if (currentX >= x - edgeThreshold && currentX <= x + width + edgeThreshold &&
              currentY >= y - edgeThreshold && currentY <= y + height + edgeThreshold) {
              const edge = getResizeEdge(currentX, currentY, x, y, width, height);
              if (edge) {
                newCanvas.style.cursor = getCursorForEdge(edge);
                return;
              } else if (currentX >= x && currentX <= x + width && currentY >= y && currentY <= y + height) {
                newCanvas.style.cursor = 'move';
                return;
              }
            }
            newCanvas.style.cursor = 'crosshair';
          }

          if (isDragging) {
            // Move the box
            const deltaX = currentX - dragStartX;
            const deltaY = currentY - dragStartY;

            let newX = boxStartX + deltaX;
            let newY = boxStartY + deltaY;

            // Constrain to canvas
            newX = Math.max(0, Math.min(newX, newCanvas.width - width));
            newY = Math.max(0, Math.min(newY, newCanvas.height - height));

            setCropRectInputs({ x: newX, y: newY, width, height });
            updateCropBox();
          } else if (isDrawing && resizeEdge) {
            // Resize the box by edge
            let newX = boxStartX;
            let newY = boxStartY;
            let newWidth = boxStartWidth;
            let newHeight = boxStartHeight;

            const deltaX = currentX - cropStartX;
            const deltaY = currentY - cropStartY;

            if (currentAspectRatio === 'free') {
              // Free resize - no aspect ratio constraint
              if (resizeEdge.includes('n')) {
                newY = boxStartY + deltaY;
                newHeight = boxStartHeight - deltaY;
              }
              if (resizeEdge.includes('s')) {
                newHeight = boxStartHeight + deltaY;
              }
              if (resizeEdge.includes('w')) {
                newX = boxStartX + deltaX;
                newWidth = boxStartWidth - deltaX;
              }
              if (resizeEdge.includes('e')) {
                newWidth = boxStartWidth + deltaX;
              }

              // Constrain to canvas and minimum size
              if (newWidth < 10) newWidth = 10;
              if (newHeight < 10) newHeight = 10;
              if (newX < 0) { newWidth += newX; newX = 0; }
              if (newY < 0) { newHeight += newY; newY = 0; }
              if (newX + newWidth > newCanvas.width) newWidth = newCanvas.width - newX;
              if (newY + newHeight > newCanvas.height) newHeight = newCanvas.height - newY;
            } else {
              // Locked aspect ratio - resize proportionally
              const ratio = getCropAspectRatioValue(currentAspectRatio);

              if (resizeEdge.includes('e') || resizeEdge.includes('w')) {
                // Horizontal resize
                newWidth = boxStartWidth + deltaX;
                newHeight = newWidth / ratio;
                if (resizeEdge.includes('w')) {
                  newX = boxStartX + boxStartWidth - newWidth;
                }
                if (resizeEdge.includes('n')) {
                  newY = boxStartY + boxStartHeight - newHeight;
                }
              } else {
                // Vertical resize
                newHeight = boxStartHeight + deltaY;
                newWidth = newHeight * ratio;
                if (resizeEdge.includes('n')) {
                  newY = boxStartY + boxStartHeight - newHeight;
                }
              }

              // Constrain to minimum size first
              if (newWidth < 10) {
                newWidth = 10;
                newHeight = newWidth / ratio;
              }
              if (newHeight < 10) {
                newHeight = 10;
                newWidth = newHeight * ratio;
              }

              // Constrain to canvas bounds while maintaining aspect ratio
              // Check if box exceeds canvas bounds
              if (newX < 0) {
                newX = 0;
                newWidth = boxStartX + boxStartWidth;
                newHeight = newWidth / ratio;
                // Adjust Y position if resizing from top
                if (resizeEdge.includes('n')) {
                  newY = boxStartY + boxStartHeight - newHeight;
                }
              }
              if (newY < 0) {
                newY = 0;
                newHeight = boxStartY + boxStartHeight;
                newWidth = newHeight * ratio;
                // Adjust X position if resizing from left
                if (resizeEdge.includes('w')) {
                  newX = boxStartX + boxStartWidth - newWidth;
                }
              }
              if (newX + newWidth > newCanvas.width) {
                newWidth = newCanvas.width - newX;
                newHeight = newWidth / ratio;
                // Adjust Y position if resizing from top
                if (resizeEdge.includes('n')) {
                  newY = boxStartY + boxStartHeight - newHeight;
                }
              }
              if (newY + newHeight > newCanvas.height) {
                newHeight = newCanvas.height - newY;
                newWidth = newHeight * ratio;
                // Adjust X position if resizing from left
                if (resizeEdge.includes('w')) {
                  newX = boxStartX + boxStartWidth - newWidth;
                }
              }

              // Final check: if still out of bounds after adjustment, clamp to fit
              if (newX < 0) newX = 0;
              if (newY < 0) newY = 0;
              if (newX + newWidth > newCanvas.width) {
                newWidth = newCanvas.width - newX;
                newHeight = newWidth / ratio;
              }
              if (newY + newHeight > newCanvas.height) {
                newHeight = newCanvas.height - newY;
                newWidth = newHeight * ratio;
              }
            }

            setCropRectInputs({ x: newX, y: newY, width: newWidth, height: newHeight });
            updateCropBox();
          } else if (isDrawing) {
            // Draw new box
            let width = Math.abs(currentX - cropStartX);
            let height = Math.abs(currentY - cropStartY);

            // Apply aspect ratio if selected
            if (currentAspectRatio !== 'free') {
              const ratio = getCropAspectRatioValue(currentAspectRatio);
              if (width / height > ratio) {
                width = height * ratio;
              } else {
                height = width / ratio;
              }
            }

            const x = currentX > cropStartX ? cropStartX : cropStartX - width;
            const y = currentY > cropStartY ? cropStartY : cropStartY - height;

            positionCropBox(cropBox, { x, y, width, height }, scaleX, scaleY);
            setCropRectInputs({ x, y, width, height });
          }
        });

        newCanvas.addEventListener('mouseup', () => {
          isDrawing = false;
          isDragging = false;
          resizeEdge = null;
          newCanvas.style.cursor = 'crosshair';
        });

        newCanvas.addEventListener('mouseleave', () => {
          isDrawing = false;
          isDragging = false;
          resizeEdge = null;
          newCanvas.style.cursor = 'crosshair';
        });

        // Update crop box when inputs change
        bindCropRectInputListeners();
      }

      function updateCropBox() {
        if (!cropCanvas) return;
        const rect = cropCanvas.getBoundingClientRect();
        const scaleX = cropCanvas.width / rect.width;
        const scaleY = cropCanvas.height / rect.height;

        const cropBox = document.getElementById('cropBox');
        positionCropBox(cropBox, readCropRect(), scaleX, scaleY);
      }

      async function applyCrop() {
        const platform = document.getElementById('cropPlatform').value;
        const cropRect = readCropRect();
        const validationError = validateCropRect(cropRect);

        if (validationError) {
          showNotification(validationError, 'error');
          return;
        }
        const { x, y, width, height } = cropRect;

        try {
          const result = await postCropUpdate(createCropUpdatePayload(platform, true, { x, y, width, height }));

          if (result.success) {
            showNotification('裁剪配置已保存', 'success');
            setCropStatusLabel(platform, '开启');

            // Check if there's a pending Holodex switch
            if (window.pendingHolodexSwitch) {
              const { channelId, suggestedAreaId, title, topicId, status, platform, twitchChannelId, externalLink } = window.pendingHolodexSwitch;
              window.pendingHolodexSwitch = null; // Clear the pending switch

              closeCropModal();

              // Now switch to the Holodex stream
              showNotification('裁剪已保存，正在切换频道...', 'info');
              await switchToHolodexStream(channelId, suggestedAreaId, title, topicId, status, platform, twitchChannelId, externalLink);
            } else {
              showNotification('裁剪配置已保存，请重启流使其生效', 'success');
              closeCropModal();
            }
          } else {
            showNotification(result.message || '保存失败', 'error');
          }
        } catch (error) {
          showNotification('保存失败: ' + error.message, 'error');
        }
      }

      async function applyCropAndRestart() {
        const platform = document.getElementById('cropPlatform').value;
        const cropRect = readCropRect();
        const validationError = validateCropRect(cropRect);

        if (validationError) {
          showNotification(validationError, 'error');
          return;
        }
        const { x, y, width, height } = cropRect;

        try {
          // Check if there's a pending Holodex switch
          if (window.pendingHolodexSwitch) {
            const { channelId, suggestedAreaId, title, topicId, status, platform, twitchChannelId, externalLink } = window.pendingHolodexSwitch;
            window.pendingHolodexSwitch = null; // Clear the pending switch

            // Step 1: Switch to the new channel
            showNotification('正在切换频道...', 'info');
            await switchToHolodexStream(channelId, suggestedAreaId, title, topicId, status, platform, twitchChannelId, externalLink);

            // Wait a bit for the switch to complete
            await new Promise(resolve => setTimeout(resolve, 1000));
          }

          // Step 2: Apply the crop
          const cropResult = await postCropUpdate(createCropUpdatePayload(platform, true, { x, y, width, height }));

          if (cropResult.success) {
            showNotification('裁剪配置已保存，正在重启流...', 'success');
            setCropStatusLabel(platform, '开启');

            closeCropModal();

            // Step 3: Restart the stream
            const restartResult = await postJsonApi('/api/restart');

            if (restartResult.success) {
              showNotification('裁剪已应用并重启流', 'success');
            } else {
              showNotification('裁剪已保存，但重启失败: ' + (restartResult.message || ''), 'error');
            }
          } else {
            showNotification(cropResult.message || '保存失败', 'error');
          }
        } catch (error) {
          showNotification('操作失败: ' + error.message, 'error');
        }
      }

      async function disableCrop() {
        const platform = document.getElementById('cropPlatform').value;

        try {
          const result = await postCropUpdate(createCropUpdatePayload(platform, false));

          if (result.success) {
            showNotification('裁剪已禁用，请重启流使其生效', 'success');
            closeCropModal();
          } else {
            showNotification(result.message || '禁用失败', 'error');
          }
        } catch (error) {
          showNotification('禁用失败: ' + error.message, 'error');
        }
      }

      function selectAspectRatio(ratio) {
        if (!ratio) return;

        // Update currentAspectRatio
        currentAspectRatio = ratio;

        // Update selected ratio state.
        document.querySelectorAll('.ratio-button').forEach(btn => {
          btn.classList.remove('active');
        });

        const activeBtn = document.getElementById(`ratio-${ratio.replace(':', '-')}`);
        if (activeBtn) {
          activeBtn.classList.add('active');
        }

        // If canvas is loaded, apply the aspect ratio
        if (cropCanvas && ratio !== 'free') {
          setCropRectInputs(createCenteredCropRect(cropCanvas, ratio));

          updateCropBox();
        }
      }
