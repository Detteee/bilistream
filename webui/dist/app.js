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
      let networkRefreshInFlight = false;
      let lastBiliNetworkLive = false;
      let lastBiliNetworkQuality = null;
      const biliNetworkHistory = { cache: [], push: [] };
      const biliNetworkHistoryLimit = 48;
      let faceAuthUrl = null;

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
          youtubeToggle.checked = config.youtube?.enable_monitor !== false;
        }
        if (twitchToggle) {
          twitchToggle.checked = config.twitch?.enable_monitor !== false;
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

      function startLogRefresh() {
        if (logRefreshIntervalId) {
          clearInterval(logRefreshIntervalId);
        }

        logRefreshIntervalId = setInterval(() => {
          if (isDashboardVisible()) {
            refreshLogs();
          }
        }, 5000);
      }

      // Refresh logs only while the dashboard is visible.
      startLogRefresh();

      document.addEventListener('visibilitychange', () => {
        if (isDashboardVisible()) {
          refreshLogs();
          refreshStatus();
        }
      });

      // Load config and set up status refresh interval
      async function initStatusRefresh() {
        try {
          const response = await fetch('/api/config');
          if (response.ok) {
            const config = await response.json();

            // Store config globally and apply config-driven controls once.
            mergeConfigData(config);
            updateMonitorToggleStates(config);
            updateDanmakuCommandToggle(config.bilibili?.enable_danmaku_command !== false);


            // Show/hide Holodex sections based on API key configuration
            const holodexSection = document.getElementById('holodex-section');
            const holodexApiConfig = document.getElementById('holodex-api-config');
            const holodexLoginBtn = document.getElementById('holodex-login-btn');
            const holodexStreamsSection = document.getElementById('holodex-streams-section');

            // Always show the Holodex section
            holodexSection.style.display = 'block';

            if (config.holodex_api_key && config.holodex_api_key.trim() !== '') {
              // API key is configured - show streams section, hide api key setup
              holodexApiConfig.style.display = 'none';
              holodexStreamsSection.style.display = 'block';
              if (holodexLoginBtn) holodexLoginBtn.style.display = 'inline-flex';
              loadHolodexAuthStatus();
            } else {
              // No API key - show config section, hide streams
              holodexApiConfig.style.display = 'block';
              holodexStreamsSection.style.display = 'none';
              if (holodexLoginBtn) holodexLoginBtn.style.display = 'none';
            }

            if (config.interval) {
              statusRefreshInterval = config.interval * 1000; // Convert to milliseconds
              console.log('Status refresh interval set to', config.interval, 'seconds');
            }

            console.log('LOL Monitor enabled:', config.enable_lol_monitor || false);
            console.log('Holodex API configured:', !!config.holodex_api_key);
          }
        } catch (error) {
          console.log('Failed to load config, using default interval:', error);
        }

        // Start status refresh with configured interval
        if (statusIntervalId) {
          clearInterval(statusIntervalId);
        }
        statusIntervalId = setInterval(() => {
          if (isDashboardVisible()) {
            refreshStatus();
          }
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

      function toggleChannelManagement() {
        const container = document.getElementById('channel-management-container');
        const toggle = document.getElementById('channel-toggle');
        if (container.style.display === 'none') {
          container.style.display = 'block';
          toggle.textContent = '▲';
        } else {
          container.style.display = 'none';
          toggle.textContent = '▼';
        }
      }

      function toggleHolodex() {
        const container = document.getElementById('holodex-container');
        const toggle = document.getElementById('holodex-toggle');
        if (container.style.display === 'none') {
          container.style.display = 'block';
          toggle.textContent = '▲';
          refreshHolodexStreams();
        } else {
          container.style.display = 'none';
          toggle.textContent = '▼';
        }
      }

      async function refreshHolodexStreams() {
        // Start continuous spinning animation
        const icon = document.getElementById('refreshHolodexIcon');
        if (icon) {
          icon.style.animation = 'spin 1s linear infinite';
        }

        const statusDiv = document.getElementById('holodex-status');
        const streamsDiv = document.getElementById('holodex-streams');

        stopHolodexDurationTicker();
        statusDiv.style.display = '';
        statusDiv.innerHTML = '⏳ 加载中...';
        statusDiv.style.background = '#89b4fa';
        statusDiv.style.color = '#1e1e2e';
        streamsDiv.innerHTML = '';

        try {
          const response = await fetch(`/api/holodex/streams?favorites=${holodexUseFavorites ? 'true' : 'false'}`);
          const data = await response.json();

          if (!data.success) {
            statusDiv.style.display = '';
            statusDiv.innerHTML = `⚠️ ${data.message}`;
            statusDiv.style.background = '#f9e2af';
            statusDiv.style.color = '#1e1e2e';
            return;
          }

          const streams = data.data || [];
          const isFavorites = data.source === 'favorites';

          // Separate live and scheduled streams
          const liveStreams = streams.filter(s => s.status === 'live');
          const scheduledStreams = streams.filter(s => s.status !== 'live');

          if (streams.length === 0) {
            statusDiv.style.display = '';
            statusDiv.innerHTML = isFavorites
              ? '✅ 收藏夹 - 当前无直播或预告'
              : '当前无直播或预告';
            statusDiv.style.background = '#a6e3a1';
            statusDiv.style.color = '#1e1e2e';
            return;
          }

          // Sort scheduled streams by time - nearest first
          scheduledStreams.sort((a, b) => {
            const timeA = a.start_scheduled ? new Date(a.start_scheduled).getTime() : Infinity;
            const timeB = b.start_scheduled ? new Date(b.start_scheduled).getTime() : Infinity;
            return timeA - timeB;
          });

          statusDiv.style.display = 'none';
          statusDiv.innerHTML = '';

          // Render live streams first
          liveStreams.forEach(stream => {
            streamsDiv.appendChild(createStreamCard(stream, true));
          });

          // Add divider if both live and scheduled exist
          if (liveStreams.length > 0 && scheduledStreams.length > 0) {
            const divider = document.createElement('div');
            divider.style.cssText = 'grid-column: 1 / -1; height: 2px; background: linear-gradient(to right, transparent, var(--heading-color), transparent); margin: 10px 0; position: relative;';
            divider.innerHTML = '<span style="position: absolute; top: -10px; left: 50%; transform: translateX(-50%); background: var(--card-bg); padding: 0 15px; color: var(--heading-color); font-size: 12px; font-weight: 600;">预告</span>';
            streamsDiv.appendChild(divider);
          }

          // Render scheduled streams (now sorted by time)
          scheduledStreams.forEach(stream => {
            streamsDiv.appendChild(createStreamCard(stream, false));
          });

          startHolodexDurationTicker();

        } catch (error) {
          statusDiv.style.display = '';
          statusDiv.innerHTML = `❌ 请求失败: ${error.message}`;
          statusDiv.style.background = '#f38ba8';
          statusDiv.style.color = '#1e1e2e';
        } finally {
          // Stop spinning animation when complete
          if (icon) {
            icon.style.animation = '';
          }
        }
      }

      const TWITCH_ICON_SVG = '<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" aria-hidden="true"><path d="M11.64 5.93H13.07V10.21H11.64M15.57 5.93H17V10.21H15.57M7 2L3.43 5.57V18.43H7.71V22L11.29 18.43H14.14L20.57 12V2M19.14 11.29L16.29 14.14H13.43L10.93 16.64V14.14H7.71V3.43H19.14Z"></path></svg>';
      const RADIO_ICON_SVG = '<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" aria-hidden="true"><path d="M12 10C10.9 10 10 10.9 10 12S10.9 14 12 14 14 13.1 14 12 13.1 10 12 10M18 12C18 8.7 15.3 6 12 6S6 8.7 6 12C6 14.2 7.2 16.1 9 17.2L10 15.5C8.8 14.8 8 13.5 8 12.1C8 9.9 9.8 8.1 12 8.1S16 9.9 16 12.1C16 13.6 15.2 14.9 14 15.5L15 17.2C16.8 16.2 18 14.2 18 12M12 2C6.5 2 2 6.5 2 12C2 15.7 4 18.9 7 20.6L8 18.9C5.6 17.5 4 14.9 4 12C4 7.6 7.6 4 12 4S20 7.6 20 12C20 15 18.4 17.5 16 18.9L17 20.6C20 18.9 22 15.7 22 12C22 6.5 17.5 2 12 2Z"></path></svg>';

      let holodexDurationIntervalId = null;

      function getHolodexPlaceholderKind(stream) {
        const link = (stream.external_link || '').toLowerCase();
        if (link.includes('twitch.tv')) {
          return 'twitch';
        }
        return 'radio';
      }

      function buildHolodexPlaceholderDurationOverlay(stream, isLive) {
        const kind = getHolodexPlaceholderKind(stream);
        const durationClass = kind === 'twitch'
          ? 'holodex-stream-duration holodex-stream-duration-twitch'
          : 'holodex-stream-duration holodex-stream-duration-radio';
        const iconSvg = kind === 'twitch' ? TWITCH_ICON_SVG : RADIO_ICON_SVG;
        const iconClass = kind === 'twitch' ? 'holodex-duration-twitch-icon' : 'holodex-duration-radio-icon';
        const hoverText = kind === 'twitch' ? '外部配信' : '外部直播';
        const startMs = getHolodexStreamStartMs(stream, isLive);

        if (isLive && startMs) {
          const initial = formatHolodexDuration(Date.now() - startMs);
          return `<div class="${durationClass}" data-tick="live" data-start-ms="${startMs}">
              <span class="holodex-duration-text">${initial}</span>
              <span class="holodex-duration-hover">${hoverText}</span>
              <span class="${iconClass}">${iconSvg}</span>
            </div>`;
        }

        if (stream.start_scheduled) {
          const start = new Date(stream.start_scheduled);
          const clock = Number.isNaN(start.getTime())
            ? '预告'
            : `${String(start.getHours()).padStart(2, '0')}:${String(start.getMinutes()).padStart(2, '0')}`;
          return `<div class="${durationClass}">
              <span class="holodex-duration-text">${clock}</span>
              <span class="holodex-duration-hover">${hoverText}</span>
              <span class="${iconClass}">${iconSvg}</span>
            </div>`;
        }

        return `<div class="${durationClass}">
            <span class="holodex-duration-hover">${hoverText}</span>
            <span class="${iconClass}">${iconSvg}</span>
          </div>`;
      }

      function buildHolodexDurationOverlay(stream, isLive, isPlaceholder) {
        if (isPlaceholder) {
          return buildHolodexPlaceholderDurationOverlay(stream, isLive);
        }

        const startMs = getHolodexStreamStartMs(stream, isLive);
        if (isLive && startMs) {
          const initial = formatHolodexDuration(Date.now() - startMs);
          return `<div class="holodex-stream-duration holodex-stream-duration-live" data-tick="live" data-start-ms="${startMs}">
              <span class="holodex-duration-text">${initial}</span>
            </div>`;
        }

        return '';
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

        const diffHours = diffMs / (1000 * 60 * 60);
        if (diffHours < 24) {
          const hours = Math.max(1, Math.ceil(diffHours));
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

      function buildHolodexAvatarBlock(stream) {
        if (!stream.channel_id) return '';
        const holodexUrl = getHolodexChannelUrl(stream.channel_id);
        const photoUrl = getHolodexChannelPhotoUrl(stream);
        const label = escapeHolodexHtml(stream.channel_name || 'channel');
        return `<a class="holodex-stream-avatar" href="${escapeHolodexHtml(holodexUrl)}" target="_blank" rel="noopener noreferrer" title="${label}">
            <img src="${escapeHolodexHtml(photoUrl)}" alt="" loading="lazy">
          </a>`;
      }

      function buildHolodexChannelBlock(stream) {
        if (!stream.channel_name) return '';
        const channelName = escapeHolodexHtml(stream.channel_name);
        const holodexUrl = getHolodexChannelUrl(stream.channel_id);
        if (holodexUrl) {
          return `<a class="holodex-stream-channel" href="${escapeHolodexHtml(holodexUrl)}" target="_blank" rel="noopener noreferrer">${channelName}</a>`;
        }
        return `<p class="holodex-stream-channel">${channelName}</p>`;
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
        const thumbUrl = stream.thumbnail || (isPlaceholder ? '' : `https://i.ytimg.com/vi/${stream.id}/mqdefault.jpg`);

        const viewers = stream.live_viewers ? `${stream.live_viewers.toLocaleString()} 观看` : '';

        let areaInfo = '';
        if (stream.suggested_area_id && stream.suggested_area_name) {
          areaInfo = `<p class="holodex-stream-area-hint">🎯 建议分区: ${escapeHolodexHtml(stream.suggested_area_name)}</p>`;
        }

        const topicBlock = stream.topic_id
          ? `<span class="holodex-stream-topic">${escapeHolodexHtml(stream.topic_id)}</span>`
          : '';
        const durationBlock = buildHolodexDurationOverlay(stream, isLive, isPlaceholder);

        const thumbInner = thumbUrl
          ? `<img src="${escapeHolodexHtml(thumbUrl)}" alt="">`
          : '<div class="holodex-stream-thumb-placeholder"></div>';

        let statusMeta = '';
        if (!isLive) {
          const scheduleText = stream.start_scheduled
            ? formatHolodexScheduledStart(stream.start_scheduled)
            : '预告';
          statusMeta = `<span class="holodex-stream-scheduled">${escapeHolodexHtml(scheduleText)}</span>`;
        } else if (viewers) {
          statusMeta = `<span class="holodex-stream-live-label">直播中</span><span>• ${viewers}</span>`;
        } else {
          statusMeta = '<span class="holodex-stream-live-label">直播中</span>';
        }

        const cropButton = isLive ? `<button class="holodex-stream-btn holodex-stream-btn-crop crop-switch-button" 
               data-platform="${platform}"
               data-channel-id="${stream.channel_id}" 
               data-twitch-id="${twitchChannelId}"
               data-external-link="${stream.external_link || ''}"
               data-suggested-area-id="${stream.suggested_area_id || ''}" 
               data-title="${stream.title.replace(/"/g, '&quot;')}" 
               data-topic-id="${stream.topic_id || ''}" 
               data-status="${stream.status || ''}">
              <svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" width="14" height="14" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M6.13 1L6 16a2 2 0 0 0 2 2h15"></path><path d="M1 6.13L16 6a2 2 0 0 1 2 2v15"></path></svg>
              <span>切换</span>
            </button>` : '';

        const switchButton = `<button class="holodex-stream-btn holodex-stream-btn-switch switch-button" 
               data-platform="${platform}"
               data-channel-id="${stream.channel_id}" 
               data-twitch-id="${twitchChannelId}"
               data-external-link="${stream.external_link || ''}"
               data-suggested-area-id="${stream.suggested_area_id || ''}" 
               data-title="${stream.title.replace(/"/g, '&quot;')}" 
               data-topic-id="${stream.topic_id || ''}" 
               data-status="${stream.status || ''}">
              <svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" width="14" height="14" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M22 12c0 6-4.39 10-9.806 10C7.792 22 4.24 19.665 3 16m-1-4C2 6 6.39 2 11.807 2C16.208 2 19.758 4.335 21 8"/><path d="m7 17l-4-1l-1 4M17 7l4 1l1-4"/></svg>
              <span>切换</span>
            </button>`;

        streamCard.innerHTML = `
          <div class="holodex-stream-thumb">
            <a class="holodex-stream-thumb-link" href="${escapeHolodexHtml(watchUrl)}" target="_blank" rel="noopener noreferrer">
              ${thumbInner}
            </a>
            <div class="holodex-stream-thumb-top">
              ${topicBlock}
            </div>
            ${durationBlock ? `<div class="holodex-stream-thumb-bottom">${durationBlock}</div>` : ''}
          </div>
          <div class="holodex-stream-body">
            <div class="holodex-stream-content-row">
              ${buildHolodexAvatarBlock(stream)}
              <div class="holodex-stream-lines">
                <h4 class="holodex-stream-title">${escapeHolodexHtml(stream.title)}</h4>
                ${buildHolodexChannelBlock(stream)}
                <div class="holodex-stream-meta">${statusMeta}</div>
                ${areaInfo}
              </div>
            </div>
            <div class="holodex-stream-actions">
              <a class="holodex-stream-watch" href="${escapeHolodexHtml(watchUrl)}" target="_blank" rel="noopener noreferrer">
                📺 观看
              </a>
              ${cropButton}
              ${switchButton}
            </div>
          </div>
        `;

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

        // Load areas if not already loaded
        if (!areasData) {
          try {
            const response = await fetch('/api/areas');
            const data = await response.json();
            areasData = data;
          } catch (error) {
            showNotification('加载分区列表失败', 'error');
            return;
          }
        }

        // Populate select
        select.innerHTML = '<option value="">选择分区...</option>';
        if (areasData && areasData.areas) {
          areasData.areas.forEach(area => {
            const option = document.createElement('option');
            option.value = area.id;
            option.textContent = `${area.name} (${area.id})`;
            select.appendChild(option);
          });
        }

        modal.style.display = 'flex';
      }

      function closeAreaModal() {
        document.getElementById('area-modal').style.display = 'none';
        pendingSwitchData = null;
      }

      function showFaceAuthModal(qrUrl) {
        faceAuthUrl = qrUrl;
        const modal = document.getElementById('face-auth-modal');
        const container = document.getElementById('face-auth-qr-container');

        // Clear previous content and generate QR code using QR Server API (same as login)
        container.innerHTML = '';

        const qrImg = document.createElement('img');
        qrImg.src = `https://api.qrserver.com/v1/create-qr-code/?size=256x256&data=${encodeURIComponent(qrUrl)}`;
        qrImg.style.width = '256px';
        qrImg.style.height = '256px';
        qrImg.style.display = 'block';
        qrImg.onerror = function () {
          // Fallback if QR API fails
          container.innerHTML = `<p style="color: var(--text-secondary); margin-bottom: 10px;">无法生成二维码，请点击下方链接：</p><a href="${qrUrl}" target="_blank" style="color: #89b4fa; word-break: break-all; text-decoration: underline; padding: 10px;">${qrUrl}</a>`;
        };

        container.appendChild(qrImg);
        modal.style.display = 'flex';
      }

      function closeFaceAuthModal() {
        document.getElementById('face-auth-modal').style.display = 'none';
        faceAuthUrl = null;
      }

      function retryStartStream() {
        closeFaceAuthModal();
        startStream();
      }

      async function confirmAreaSelection() {
        const select = document.getElementById('modal-area-select');
        const areaId = parseInt(select.value);

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

      async function performSwitch(channelId, areaId, title, topicId, status, platform, twitchChannelId, externalLink) {
        try {
          console.log('Switching to channel:', channelId, 'platform:', platform, 'area:', areaId, 'status:', status);
          const response = await fetch('/api/holodex/switch', {
            method: 'POST',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify({
              channel_id: channelId,
              area_id: areaId,
              title: title || null,
              topic_id: topicId || null,
              status: status || null,
              platform: platform || 'youtube',
              twitch_channel_id: twitchChannelId || null,
              external_link: externalLink || null
            })
          });

          const result = await response.json();
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
          const response = await fetch(captureUrl, {
            method: 'POST'
          });

          const result = await response.json();

          if (result.success && result.message) {
            // Load the captured image
            cropImage = new Image();
            cropImage.onload = function () {
              cropCanvas = document.getElementById('cropCanvas');
              cropCtx = cropCanvas.getContext('2d');

              // Show container FIRST so canvas has dimensions
              document.getElementById('cropCanvasContainer').style.display = 'block';

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

        if (authBar) authBar.style.display = 'inline-flex';
        if (loginBtn) loginBtn.style.display = 'none';

        if (holodexAuthState.logged_in && !holodexAuthState.expired) {
          if (heartIcon) heartIcon.style.display = 'inline-flex';
          if (favoritesMode) favoritesMode.style.display = 'inline-flex';
          if (jwtSkip) jwtSkip.style.display = 'none';
          if (favoritesLabel) {
            favoritesLabel.textContent = holodexAuthState.username
              ? `${holodexAuthState.username}的收藏`
              : '收藏夹';
          }
          if (favoritesBtn) favoritesBtn.title = '管理 Holodex 登录';
        } else if (holodexAuthState.expired) {
          if (heartIcon) heartIcon.style.display = 'none';
          if (favoritesMode) favoritesMode.style.display = 'none';
          if (jwtSkip) jwtSkip.style.display = 'none';
          if (favoritesLabel) favoritesLabel.textContent = '⚠️ 重新登录';
          if (favoritesBtn) favoritesBtn.title = 'Holodex 登录已过期';
        } else {
          if (heartIcon) heartIcon.style.display = 'none';
          if (favoritesMode) favoritesMode.style.display = 'none';
          if (jwtSkip) jwtSkip.style.display = 'none';
          if (favoritesLabel) favoritesLabel.textContent = '🔗 使用Holodex收藏夹';
          if (favoritesBtn) favoritesBtn.title = '登录后使用 Holodex 收藏夹';
        }

        if (logoutBtn) {
          logoutBtn.style.display = holodexAuthState.logged_in ? 'inline-flex' : 'none';
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
        modal.style.display = 'flex';
        loadHolodexAuthStatus();
      }

      function closeHolodexLoginModal() {
        const modal = document.getElementById('holodex-login-modal');
        if (modal) modal.style.display = 'none';
      }

      function updateHolodexLoginButton() {
        updateHolodexAuthBar();
      }

      function updateHolodexModalHint() {
        const hint = document.getElementById('holodex-modal-auth-hint');
        if (!hint) return;

        if (holodexAuthState.logged_in && !holodexAuthState.expired) {
          hint.style.display = 'none';
          return;
        }

        hint.style.display = '';

        if (holodexAuthState.expired) {
          hint.textContent = '⚠️ JWT 已过期，请重新登录 Holodex 并保存新的 token';
          hint.style.background = 'var(--input-bg)';
          hint.style.color = '#f9e2af';
        } else {
          hint.textContent = '未登录 — 当前使用 channels.json 频道列表 (YouTube + Twitch 外部配信)';
          hint.style.background = 'var(--input-bg)';
          hint.style.color = 'var(--text-secondary)';
        }
      }

      async function toggleHolodexSkipJwtVerify() {
        const toggle = document.getElementById('holodex-skip-jwt-verify');
        if (!toggle) return;

        try {
          const response = await fetch('/api/config', {
            method: 'POST',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify({ holodex_skip_jwt_verify: toggle.checked })
          });
          const data = await response.json();
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
          const response = await fetch('/api/holodex/auth/status');
          const data = await response.json();
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
          const response = await fetch('/api/config', {
            method: 'POST',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify({ holodex_jwt: jwt })
          });
          const data = await response.json();
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
          const response = await fetch('/api/config', {
            method: 'POST',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify({ holodex_jwt: '' })
          });
          const data = await response.json();
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
          const response = await fetch('/api/config', {
            method: 'POST',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify({
              holodex_api_key: apiKey
            })
          });

          const data = await response.json();
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

      function toggleLogs() {
        const container = document.getElementById('log-container');
        const toggle = document.getElementById('log-toggle');
        if (container.style.display === 'none') {
          container.style.display = 'block';
          toggle.textContent = '▲';
          refreshLogs();
        } else {
          container.style.display = 'none';
          toggle.textContent = '▼';
        }
      }

      function toggleSystemConfig() {
        const container = document.getElementById('system-config-container');
        const toggle = document.getElementById('system-config-toggle');
        if (container.style.display === 'none') {
          container.style.display = 'block';
          toggle.textContent = '▲';
          loadSystemConfig();
        } else {
          container.style.display = 'none';
          toggle.textContent = '▼';
        }
      }

      function toggleConfigRiotApiKey() {
        const checkbox = document.getElementById('config-lol-monitor-checkbox');
        const riotGroup = document.getElementById('config-riot-api-group');
        const intervalGroup = document.getElementById('config-lol-interval-group');

        if (checkbox.checked) {
          riotGroup.style.display = 'block';
          intervalGroup.style.display = 'block';
        } else {
          riotGroup.style.display = 'none';
          intervalGroup.style.display = 'none';
        }
      }

      function toggleAntiCollisionList() {
        const checkbox = document.getElementById('config-anti-collision-checkbox');
        const section = document.getElementById('anti-collision-section');

        if (checkbox.checked) {
          section.style.display = 'block';
        } else {
          section.style.display = 'none';
        }
      }

      function formatHlsCacheStatus(enabled, latencySecs) {
        return enabled ? `${latencySecs || 8}秒` : '关闭';
      }

      function setHlsCacheLatencyInputState(platform, enabled) {
        const latencyGroup = document.getElementById(`${platform}-hls-cache-latency-group`);
        const latencyInput = document.getElementById(`${platform}-hls-cache-latency`);
        if (latencyGroup) {
          latencyGroup.style.display = enabled ? 'flex' : 'none';
        }
        if (latencyInput) {
          latencyInput.disabled = !enabled;
        }
      }

      function toggleYtHlsCacheEdit() {
        const statusSpan = document.getElementById('yt-hls-cache-status');
        const editContainer = document.getElementById('yt-hls-cache-edit-container');

        showInfoRowEdit(statusSpan, editContainer);

        fetch('/api/ffmpeg-cache/youtube')
          .then(response => response.json())
          .then(result => {
            const cache = result.data || {};
            document.getElementById('yt-hls-cache-enabled').checked = !!cache.enabled;
            document.getElementById('yt-hls-cache-latency').value = cache.latency_secs || 8;
            setHlsCacheLatencyInputState('yt', !!cache.enabled);
          })
          .catch(() => {
            document.getElementById('yt-hls-cache-enabled').checked = false;
            document.getElementById('yt-hls-cache-latency').value = 8;
            setHlsCacheLatencyInputState('yt', false);
          });

        document.getElementById('yt-hls-cache-enabled').onchange = () => {
          setHlsCacheLatencyInputState('yt', document.getElementById('yt-hls-cache-enabled').checked);
        };
      }

      function cancelYtHlsCacheEdit() {
        const statusSpan = document.getElementById('yt-hls-cache-status');
        const editContainer = document.getElementById('yt-hls-cache-edit-container');

        hideInfoRowEdit(statusSpan, editContainer);
      }

      async function saveYtHlsCacheEdit() {
        const enabled = document.getElementById('yt-hls-cache-enabled').checked;
        const latencySecs = Math.min(60, Math.max(1, parseInt(document.getElementById('yt-hls-cache-latency').value) || 8));

        try {
          const response = await fetch('/api/ffmpeg-cache/update', {
            method: 'POST',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify({
              platform: 'youtube',
              enabled,
              latency_secs: latencySecs
            })
          });
          const result = await response.json();
          if (!result.success) {
            showNotification(result.message || 'HLS 缓存配置保存失败', 'error');
            return;
          }

          document.getElementById('yt-hls-cache-status').textContent = formatHlsCacheStatus(enabled, latencySecs);
          cancelYtHlsCacheEdit();
          showNotification('YouTube HLS 缓存已更新', 'success');
        } catch (error) {
          showNotification('HLS 缓存配置保存失败: ' + error.message, 'error');
        }
      }

      function toggleTwHlsCacheEdit() {
        const statusSpan = document.getElementById('tw-hls-cache-status');
        const editContainer = document.getElementById('tw-hls-cache-edit-container');

        showInfoRowEdit(statusSpan, editContainer);

        fetch('/api/ffmpeg-cache/twitch')
          .then(response => response.json())
          .then(result => {
            const cache = result.data || {};
            document.getElementById('tw-hls-cache-enabled').checked = !!cache.enabled;
            document.getElementById('tw-hls-cache-latency').value = cache.latency_secs || 8;
            setHlsCacheLatencyInputState('tw', !!cache.enabled);
          })
          .catch(() => {
            document.getElementById('tw-hls-cache-enabled').checked = false;
            document.getElementById('tw-hls-cache-latency').value = 8;
            setHlsCacheLatencyInputState('tw', false);
          });

        document.getElementById('tw-hls-cache-enabled').onchange = () => {
          setHlsCacheLatencyInputState('tw', document.getElementById('tw-hls-cache-enabled').checked);
        };
      }

      function cancelTwHlsCacheEdit() {
        const statusSpan = document.getElementById('tw-hls-cache-status');
        const editContainer = document.getElementById('tw-hls-cache-edit-container');

        hideInfoRowEdit(statusSpan, editContainer);
      }

      async function saveTwHlsCacheEdit() {
        const enabled = document.getElementById('tw-hls-cache-enabled').checked;
        const latencySecs = Math.min(60, Math.max(1, parseInt(document.getElementById('tw-hls-cache-latency').value) || 8));

        try {
          const response = await fetch('/api/ffmpeg-cache/update', {
            method: 'POST',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify({
              platform: 'twitch',
              enabled,
              latency_secs: latencySecs
            })
          });
          const result = await response.json();
          if (!result.success) {
            showNotification(result.message || 'HLS 缓存配置保存失败', 'error');
            return;
          }

          document.getElementById('tw-hls-cache-status').textContent = formatHlsCacheStatus(enabled, latencySecs);
          cancelTwHlsCacheEdit();
          showNotification('Twitch HLS 缓存已更新', 'success');
        } catch (error) {
          showNotification('HLS 缓存配置保存失败: ' + error.message, 'error');
        }
      }

      async function loadSystemConfig() {
        try {
          const response = await fetch('/api/config');
          const config = await response.json();
          mergeConfigData(config);

          // Load basic settings
          document.getElementById('config-interval').value = config.interval || 30;
          document.getElementById('config-auto-cover-checkbox').checked = config.auto_cover || false;
          document.getElementById('config-danmaku-command-checkbox').checked = config.bilibili?.enable_danmaku_command !== false;
          document.getElementById('config-anti-collision-checkbox').checked = config.enable_anti_collision || false;
          toggleAntiCollisionList(); // Show/hide anti-collision section based on checkbox

          // Load API keys
          document.getElementById('config-holodex-key').value = config.holodex_api_key || '';
          document.getElementById('config-riot-key').value = config.riot_api_key || '';

          // Load LoL monitor settings
          const lolMonitorEnabled = config.enable_lol_monitor || false;
          document.getElementById('config-lol-monitor-checkbox').checked = lolMonitorEnabled;
          document.getElementById('config-lol-interval').value = config.lol_monitor_interval || 1;
          toggleConfigRiotApiKey(); // Show/hide riot API fields based on checkbox

          // Load Twitch settings
          document.getElementById('config-tw-region').value = (config.twitch && config.twitch.proxy_region) || 'asl';

          // Load YouTube cookies settings
          document.getElementById('config-yt-cookies-browser').value = (config.youtube && config.youtube.cookies_from_browser) || '';
          document.getElementById('config-yt-cookies-file').value = (config.youtube && config.youtube.cookies_file) || '';
          document.getElementById('config-yt-deno-path').value = (config.youtube && config.youtube.deno_path) || '';

          // Load proxy settings
          document.getElementById('config-yt-proxy').value = (config.youtube && config.youtube.proxy) || '';
          document.getElementById('config-tw-proxy').value = (config.twitch && config.twitch.proxy) || '';

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
            const response = await fetch('/api/config');
            config = mergeConfigData(await response.json());
          }

          updateMonitorToggleStates(config);
        } catch (error) {
          console.error('Failed to load monitor toggle states:', error);
        }
      }

      async function loadBannedKeywords() {
        try {
          const response = await fetch('/api/banned-keywords');
          const data = await response.json();

          document.getElementById('streaming-banned-keywords').value =
            (data.streaming_banned_keywords || []).join('\n');
          document.getElementById('danmaku-banned-keywords').value =
            (data.danmaku_banned_keywords || []).join('\n');
        } catch (error) {
          console.error('Failed to load banned keywords:', error);
        }
      }

      // Danmaku Command Toggle Functions
      async function loadDanmakuCommandState(config = window.configData) {
        try {
          if (!config.bilibili || typeof config.bilibili.enable_danmaku_command !== 'boolean') {
            const response = await fetch('/api/config');
            config = mergeConfigData(await response.json());
          }

          updateDanmakuCommandToggle(config.bilibili?.enable_danmaku_command !== false);
        } catch (error) {
          console.error('Failed to load danmaku command state:', error);
        }
      }

      async function toggleDanmakuCommand() {
        const toggle = document.getElementById('bili-danmaku-command-toggle');
        const enabled = toggle.checked;

        try {
          const response = await fetch('/api/config', {
            method: 'POST',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify({
              enable_danmaku_command: enabled
            })
          });

          const result = await response.json();
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
        if (Object.keys(list).length === 0) {
          container.innerHTML = '<div style="color: var(--text-secondary); text-align: center; padding: 20px;">暂无防撞车名单</div>';
          return;
        }

        const html = `
          <table style="width: 100%; border-collapse: collapse;">
            <thead>
              <tr style="border-bottom: 2px solid var(--card-border);">
                <th style="text-align: left; padding: 12px 8px; color: var(--text-primary); font-weight: 600;">用户名</th>
                <th style="text-align: left; padding: 12px 8px; color: var(--text-primary); font-weight: 600;">房间号</th>
                <th style="text-align: center; padding: 12px 8px; color: var(--text-primary); font-weight: 600; width: 60px;">操作</th>
              </tr>
            </thead>
            <tbody>
              ${Object.entries(list).map(([username, roomId]) => `
                <tr style="border-bottom: 1px solid var(--card-border);">
                  <td style="padding: 12px 8px; color: var(--text-primary); font-weight: 500;">${username}</td>
                  <td style="padding: 12px 8px; color: var(--text-secondary);">${roomId}</td>
                  <td style="padding: 12px 8px; text-align: center;">
                    <button onclick="removeAntiCollisionEntry('${username}')" title="删除"
                      style="background: none; border: none; color: #F38BA8; cursor: pointer; padding: 4px; border-radius: 4px; display: flex; align-items: center; transition: background 0.2s; margin: 0 auto;"
                      onmouseover="this.style.background='var(--button-hover-bg)'" onmouseout="this.style.background='none'">
                      <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
                        <polyline points="3,6 5,6 21,6"></polyline>
                        <path d="m19,6v14a2,2 0 0,1 -2,2H7a2,2 0 0,1 -2,-2V6m3,0V4a2,2 0 0,1 2,-2h4a2,2 0 0,1 2,2v2"></path>
                        <line x1="10" y1="11" x2="10" y2="17"></line>
                        <line x1="14" y1="11" x2="14" y2="17"></line>
                      </svg>
                    </button>
                  </td>
                </tr>
              `).join('')}
            </tbody>
          </table>
        `;

        container.innerHTML = html;
      }

      function addAntiCollisionEntry() {
        const username = document.getElementById('anti-collision-username').value.trim();
        const roomId = parseInt(document.getElementById('anti-collision-roomid').value);

        if (!username || !roomId || isNaN(roomId)) {
          showNotification('请填写用户名和有效的房间号', 'error');
          return;
        }

        // Add to global anti-collision list
        if (!window.currentAntiCollisionList) {
          window.currentAntiCollisionList = {};
        }
        window.currentAntiCollisionList[username] = roomId;

        loadAntiCollisionList(window.currentAntiCollisionList);

        // Clear inputs
        document.getElementById('anti-collision-username').value = '';
        document.getElementById('anti-collision-roomid').value = '';

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
          interval: parseInt(document.getElementById('config-interval').value) || 30,
          auto_cover: document.getElementById('config-auto-cover-checkbox').checked,
          enable_danmaku_command: document.getElementById('config-danmaku-command-checkbox').checked,
          enable_anti_collision: document.getElementById('config-anti-collision-checkbox').checked,
          holodex_api_key: document.getElementById('config-holodex-key').value.trim(),
          riot_api_key: document.getElementById('config-riot-key').value.trim(),
          enable_lol_monitor: document.getElementById('config-lol-monitor-checkbox').checked,
          lol_monitor_interval: parseInt(document.getElementById('config-lol-interval').value) || 1,
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

          const response = await fetch('/api/config', {
            method: 'POST',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify(config)
          });

          const result = await response.json();

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
            // Save banned keywords separately
            await saveBannedKeywords();
            showNotification('配置保存成功', 'success');
          } else {
            showNotification('配置保存失败: ' + (result.error || '未知错误'), 'error');
          }
        } catch (error) {
          console.error('Failed to save system config:', error);
          showNotification('配置保存失败', 'error');
        }
      }

      async function saveBannedKeywords() {
        try {
          const streamingKeywords = document.getElementById('streaming-banned-keywords').value
            .split('\n')
            .map(k => k.trim())
            .filter(k => k.length > 0);

          const danmakuKeywords = document.getElementById('danmaku-banned-keywords').value
            .split('\n')
            .map(k => k.trim())
            .filter(k => k.length > 0);

          const response = await fetch('/api/banned-keywords', {
            method: 'POST',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify({
              streaming_banned_keywords: streamingKeywords,
              danmaku_banned_keywords: danmakuKeywords
            })
          });

          const result = await response.json();
          if (!result.success) {
            console.error('Failed to save banned keywords:', result.message);
          }
        } catch (error) {
          console.error('Failed to save banned keywords:', error);
        }
      }

      function toggleManagement() {
        const container = document.getElementById('management-container');
        const toggle = document.getElementById('management-toggle');
        if (container.style.display === 'none') {
          container.style.display = 'block';
          toggle.textContent = '▲';
          // Don't auto-load since subsections are collapsed by default
        } else {
          container.style.display = 'none';
          toggle.textContent = '▼';
        }
      }

      function toggleAreaManagement() {
        const container = document.getElementById('area-management-content');
        const toggle = document.getElementById('area-management-toggle');
        if (container.style.display === 'none') {
          container.style.display = 'block';
          toggle.textContent = '▲';
        } else {
          container.style.display = 'none';
          toggle.textContent = '▼';
        }
      }

      function toggleChannelConfig() {
        const container = document.getElementById('channel-management-content');
        const toggle = document.getElementById('channel-management-toggle');
        if (container.style.display === 'none') {
          container.style.display = 'block';
          toggle.textContent = '▲';
        } else {
          container.style.display = 'none';
          toggle.textContent = '▼';
        }
      }

      function toggleAreasList() {
        const container = document.getElementById('areas-content');
        const toggle = document.getElementById('areas-list-toggle');
        const refreshBtn = document.getElementById('refreshAreasBtn');
        if (container.style.display === 'none') {
          container.style.display = 'block';
          toggle.textContent = '▲';
          refreshBtn.style.display = 'flex';
          if (container.innerHTML === '加载中...') {
            loadAreas();
          }
        } else {
          container.style.display = 'none';
          toggle.textContent = '▼';
          refreshBtn.style.display = 'none';
        }
      }

      function toggleChannelsList() {
        const container = document.getElementById('channels-content');
        const toggle = document.getElementById('channels-list-toggle');
        const refreshBtn = document.getElementById('refreshChannelsBtn');
        if (container.style.display === 'none') {
          container.style.display = 'block';
          toggle.textContent = '▲';
          refreshBtn.style.display = 'flex';
          if (container.innerHTML === '加载中...') {
            loadChannels();
          }
        } else {
          container.style.display = 'none';
          toggle.textContent = '▼';
          refreshBtn.style.display = 'none';
        }
      }

      // Area management functions
      async function loadAreas() {
        try {
          const response = await fetch('/api/manage/areas');

          if (!response.ok) {
            throw new Error(`HTTP error! status: ${response.status}`);
          }

          const contentType = response.headers.get('content-type');
          if (!contentType || !contentType.includes('application/json')) {
            const text = await response.text();
            throw new Error(`Expected JSON, got: ${contentType}. Response: ${text}`);
          }

          const result = await response.json();

          if (result.success) {
            const areasContent = document.getElementById('areas-content');
            if (result.data.areas.length === 0) {
              areasContent.innerHTML = '<p style="color: var(--text-secondary);">暂无分区</p>';
            } else {
              areasContent.innerHTML = `
                <div style="display: grid; grid-template-columns: repeat(auto-fit, minmax(min(250px, 100%), 1fr)); gap: 12px;">
                  ${result.data.areas.map(area => `
                    <div style="padding: 10px; border: 1px solid var(--card-border); border-radius: 12px; background: var(--card-bg); color: var(--text-primary);">
                      <div style="display: flex; align-items: center; justify-content: space-between;">
                        <div><strong>ID: ${area.id}</strong> - ${area.name}</div>
                        <div style="display: flex; gap: 4px;">
                          <button onclick="editArea(${area.id})" title="编辑分区"
                            style="background: none; border: none; color: var(--heading-color); cursor: pointer; padding: 4px; border-radius: 4px; display: flex; align-items: center; transition: background 0.2s;"
                            onmouseover="this.style.background='var(--button-hover-bg)'" onmouseout="this.style.background='none'">
                            <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
                              <path d="M12 20h9"></path>
                              <path d="M16.5 3.5a2.121 2.121 0 0 1 3 3L7 19l-4 1 1-4L16.5 3.5z"></path>
                            </svg>
                          </button>
                          <button onclick="deleteArea(${area.id})" title="删除分区"
                            style="background: none; border: none; color: var(--heading-color); cursor: pointer; padding: 4px; border-radius: 4px; display: flex; align-items: center; transition: background 0.2s;"
                            onmouseover="this.style.background='var(--button-hover-bg)'" onmouseout="this.style.background='none'">
                            <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
                              <polyline points="3,6 5,6 21,6"></polyline>
                              <path d="m19,6v14a2,2 0 0,1 -2,2H7a2,2 0 0,1 -2,-2V6m3,0V4a2,2 0 0,1 2,-2h4a2,2 0 0,1 2,2v2"></path>
                              <line x1="10" y1="11" x2="10" y2="17"></line>
                              <line x1="14" y1="11" x2="14" y2="17"></line>
                            </svg>
                          </button>
                        </div>
                      </div>
                      <hr style="margin: 8px 0; border: none; height: 1px; background: var(--card-border);">
                      <small style="color: var(--text-secondary);">关键词: ${area.title_keywords.join(', ') || '无'}</small>
                      <br><small style="color: var(--text-secondary);">别名: ${area.aliases.join(', ') || '无'}</small>
                    </div>
                  `).join('')}
                </div>
              `;
            }
          } else {
            document.getElementById('areas-content').innerHTML = `<p style="color: red;">加载失败: ${result.message}</p>`;
          }
        } catch (error) {
          console.error('Load areas error:', error);
          document.getElementById('areas-content').innerHTML = `<p style="color: red;">加载失败: ${error.message}</p>`;
        }
      }

      async function addArea() {
        const id = parseInt(document.getElementById('area-id').value);
        const name = document.getElementById('area-name').value.trim();
        const keywords = document.getElementById('area-keywords').value.split(',').map(k => k.trim()).filter(k => k);
        const aliases = document.getElementById('area-aliases').value.split(',').map(a => a.trim()).filter(a => a);

        if (!id || !name) {
          alert('请填写分区ID和名称');
          return;
        }

        try {
          const response = await fetch('/api/manage/areas', {
            method: 'POST',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify({
              id: id,
              name: name,
              title_keywords: keywords,
              aliases: aliases
            })
          });

          if (!response.ok) {
            throw new Error(`HTTP error! status: ${response.status}`);
          }

          const contentType = response.headers.get('content-type');
          if (!contentType || !contentType.includes('application/json')) {
            const text = await response.text();
            throw new Error(`Expected JSON, got: ${contentType}. Response: ${text}`);
          }

          const result = await response.json();
          if (result.success) {
            alert('分区添加成功');
            // Clear form
            document.getElementById('area-id').value = '';
            document.getElementById('area-name').value = '';
            document.getElementById('area-keywords').value = '';
            document.getElementById('area-aliases').value = '';
            loadAreas();
          } else {
            alert(`添加失败: ${result.message}`);
          }
        } catch (error) {
          console.error('Add area error:', error);
          alert(`添加失败: ${error.message}`);
        }
      }

      // Channel management functions
      async function loadChannels() {
        try {
          const response = await fetch('/api/manage/channels');
          const result = await response.json();

          if (result.success) {
            const channelsContent = document.getElementById('channels-content');
            if (result.data.channels.length === 0) {
              channelsContent.innerHTML = '<p style="color: var(--text-secondary);">暂无频道</p>';
            } else {
              channelsContent.innerHTML = `
                <div style="display: grid; grid-template-columns: repeat(auto-fit, minmax(min(250px, 100%), 1fr)); gap: 12px;">
                  ${result.data.channels.map(channel => `
                    <div style="padding: 10px; border: 1px solid var(--card-border); border-radius: 12px; background: var(--card-bg); color: var(--text-primary); word-wrap: break-word; overflow-wrap: break-word;">
                      <div style="display: flex; align-items: center; justify-content: space-between;">
                        <div><strong>${channel.name}</strong></div>
                        <div style="display: flex; gap: 4px;">
                          <button onclick="editChannel('${channel.name}')" title="编辑频道"
                            style="background: none; border: none; color: var(--heading-color); cursor: pointer; padding: 4px; border-radius: 4px; display: flex; align-items: center; transition: background 0.2s;"
                            onmouseover="this.style.background='var(--button-hover-bg)'" onmouseout="this.style.background='none'">
                            <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
                              <path d="M12 20h9"></path>
                              <path d="M16.5 3.5a2.121 2.121 0 0 1 3 3L7 19l-4 1 1-4L16.5 3.5z"></path>
                            </svg>
                          </button>
                          <button onclick="deleteChannel('${channel.name}')" title="删除频道"
                            style="background: none; border: none; color: var(--heading-color); cursor: pointer; padding: 4px; border-radius: 4px; display: flex; align-items: center; transition: background 0.2s;"
                            onmouseover="this.style.background='var(--button-hover-bg)'" onmouseout="this.style.background='none'">
                            <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
                              <polyline points="3,6 5,6 21,6"></polyline>
                              <path d="m19,6v14a2,2 0 0,1 -2,2H7a2,2 0 0,1 -2,-2V6m3,0V4a2,2 0 0,1 2,-2h4a2,2 0 0,1 2,2v2"></path>
                              <line x1="10" y1="11" x2="10" y2="17"></line>
                              <line x1="14" y1="11" x2="14" y2="17"></line>
                            </svg>
                          </button>
                        </div>
                      </div>
                      <hr style="margin: 8px 0; border: none; height: 1px; background: var(--card-border);">
                      <small style="color: var(--text-secondary);">别名: ${channel.aliases.join(', ') || '无'}</small>
                      <br><small style="color: var(--text-secondary); word-break: break-all;">YouTube: ${channel.platforms.youtube || '无'}</small>
                      <br><small style="color: var(--text-secondary);">Twitch: ${channel.platforms.twitch || '无'}</small>
                      ${channel.riot_puuid ? `<br><small style="color: var(--text-secondary); word-break: break-all; line-height: 1.3;">Riot PUUID: ${channel.riot_puuid}</small>` : ''}
                    </div>
                  `).join('')}
                </div>
              `;
            }
          } else {
            document.getElementById('channels-content').innerHTML = `<p style="color: red;">加载失败: ${result.message}</p>`;
          }
        } catch (error) {
          document.getElementById('channels-content').innerHTML = `<p style="color: red;">加载失败: ${error.message}</p>`;
        }
      }

      async function addChannel() {
        const name = document.getElementById('channel-name').value.trim();
        const aliases = document.getElementById('channel-aliases').value.split(',').map(a => a.trim()).filter(a => a);
        const youtube = document.getElementById('channel-youtube').value.trim();
        const twitch = document.getElementById('channel-twitch').value.trim();
        const riot = document.getElementById('channel-riot').value.trim();

        if (!name) {
          alert('请填写频道名称');
          return;
        }

        const platforms = {};
        if (youtube) platforms.youtube = youtube;
        if (twitch) platforms.twitch = twitch;

        // Validate that at least one platform is provided
        if (Object.keys(platforms).length === 0) {
          alert('请至少填写一个平台的频道ID（YouTube或Twitch）');
          return;
        }

        try {
          const response = await fetch('/api/manage/channels', {
            method: 'POST',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify({
              name: name,
              aliases: aliases,
              platforms: platforms,
              riot_puuid: riot || null
            })
          });

          if (!response.ok) {
            throw new Error(`HTTP error! status: ${response.status}`);
          }

          const contentType = response.headers.get('content-type');
          if (!contentType || !contentType.includes('application/json')) {
            const text = await response.text();
            throw new Error(`Expected JSON, got: ${contentType}. Response: ${text}`);
          }

          const result = await response.json();
          if (result.success) {
            alert('频道添加成功');
            clearChannelForm();
            loadChannels();
          } else {
            alert(`添加失败: ${result.message}`);
          }
        } catch (error) {
          console.error('Add channel error:', error);
          alert(`添加失败: ${error.message}`);
        }
      }

      async function updateChannel() {
        const name = document.getElementById('channel-name').value.trim();
        const aliases = document.getElementById('channel-aliases').value.split(',').map(a => a.trim()).filter(a => a);
        const youtube = document.getElementById('channel-youtube').value.trim();
        const twitch = document.getElementById('channel-twitch').value.trim();
        const riot = document.getElementById('channel-riot').value.trim();

        if (!name) {
          alert('请填写频道名称');
          return;
        }

        const platforms = {};
        if (youtube) platforms.youtube = youtube;
        if (twitch) platforms.twitch = twitch;

        // Validate that at least one platform is provided
        if (Object.keys(platforms).length === 0) {
          alert('请至少填写一个平台的频道ID（YouTube或Twitch）');
          return;
        }

        try {
          const response = await fetch('/api/manage/channels', {
            method: 'PUT',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify({
              name: name,
              aliases: aliases,
              platforms: platforms,
              riot_puuid: riot || null
            })
          });

          const result = await response.json();
          if (result.success) {
            alert('频道更新成功');
            clearChannelForm();
            loadChannels();
          } else {
            alert(`更新失败: ${result.message}`);
          }
        } catch (error) {
          alert(`更新失败: ${error.message}`);
        }
      }

      function editChannel(channelName) {
        // Load channel data into form for editing
        fetch('/api/manage/channels')
          .then(response => response.json())
          .then(result => {
            if (result.success) {
              const channel = result.data.channels.find(c => c.name === channelName);
              if (channel) {
                document.getElementById('channel-name').value = channel.name;
                document.getElementById('channel-aliases').value = channel.aliases.join(', ');
                document.getElementById('channel-youtube').value = channel.platforms.youtube || '';
                document.getElementById('channel-twitch').value = channel.platforms.twitch || '';
                document.getElementById('channel-riot').value = channel.riot_puuid || '';

                // Change form to edit mode
                document.getElementById('channel-form-title').textContent = '编辑频道';
                document.getElementById('channel-submit-btn').textContent = '更新频道';
                document.getElementById('channel-submit-btn').onclick = () => updateChannel();

                // Scroll to form
                document.getElementById('channel-name').scrollIntoView({ behavior: 'smooth' });
              }
            }
          });
      }

      function clearChannelForm() {
        document.getElementById('channel-name').value = '';
        document.getElementById('channel-aliases').value = '';
        document.getElementById('channel-youtube').value = '';
        document.getElementById('channel-twitch').value = '';
        document.getElementById('channel-riot').value = '';

        // Reset form to add mode
        document.getElementById('channel-form-title').textContent = '添加频道';
        document.getElementById('channel-submit-btn').textContent = '添加频道';
        document.getElementById('channel-submit-btn').onclick = addChannel;
      }

      function editArea(areaId) {
        // Load area data into form for editing
        fetch('/api/manage/areas')
          .then(response => response.json())
          .then(result => {
            if (result.success) {
              const area = result.data.areas.find(a => a.id === areaId);
              if (area) {
                document.getElementById('area-id').value = area.id;
                document.getElementById('area-name').value = area.name;
                document.getElementById('area-keywords').value = area.title_keywords.join(', ');
                document.getElementById('area-aliases').value = area.aliases.join(', ');

                // Change form to edit mode
                document.getElementById('area-form-title').textContent = '编辑分区';
                document.getElementById('area-submit-btn').textContent = '更新分区';
                document.getElementById('area-submit-btn').onclick = () => updateArea(areaId);

                // Scroll to form
                document.getElementById('area-id').scrollIntoView({ behavior: 'smooth' });
              }
            }
          });
      }

      function clearAreaForm() {
        document.getElementById('area-id').value = '';
        document.getElementById('area-name').value = '';
        document.getElementById('area-keywords').value = '';
        document.getElementById('area-aliases').value = '';

        // Reset form to add mode
        document.getElementById('area-form-title').textContent = '添加新分区';
        document.getElementById('area-submit-btn').textContent = '添加分区';
        document.getElementById('area-submit-btn').onclick = addArea;
      }

      async function updateArea(originalId) {
        const id = parseInt(document.getElementById('area-id').value);
        const name = document.getElementById('area-name').value.trim();
        const keywords = document.getElementById('area-keywords').value.split(',').map(k => k.trim()).filter(k => k);
        const aliases = document.getElementById('area-aliases').value.split(',').map(a => a.trim()).filter(a => a);

        if (!id || !name) {
          alert('请填写分区ID和名称');
          return;
        }

        try {
          // If ID changed, we need to delete the old one and add the new one
          if (originalId !== id) {
            // Delete old area
            const deleteResponse = await fetch(`/api/manage/areas/${originalId}`, {
              method: 'DELETE'
            });

            if (!deleteResponse.ok) {
              throw new Error(`删除原分区失败: ${deleteResponse.status}`);
            }

            // Add new area with new ID
            const response = await fetch('/api/manage/areas', {
              method: 'POST',
              headers: { 'Content-Type': 'application/json' },
              body: JSON.stringify({
                id: id,
                name: name,
                title_keywords: keywords,
                aliases: aliases
              })
            });

            if (!response.ok) {
              throw new Error(`HTTP error! status: ${response.status}`);
            }

            const contentType = response.headers.get('content-type');
            if (!contentType || !contentType.includes('application/json')) {
              const text = await response.text();
              throw new Error(`Expected JSON, got: ${contentType}. Response: ${text}`);
            }

            const result = await response.json();
            if (result.success) {
              alert('分区更新成功');
              clearAreaForm();
              loadAreas();
            } else {
              alert(`更新失败: ${result.message}`);
            }
            return;
          }

          // Update existing area (ID unchanged)
          const response = await fetch('/api/manage/areas', {
            method: 'PUT',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify({
              id: id,
              name: name,
              title_keywords: keywords,
              aliases: aliases
            })
          });

          if (!response.ok) {
            throw new Error(`HTTP error! status: ${response.status}`);
          }

          const contentType = response.headers.get('content-type');
          if (!contentType || !contentType.includes('application/json')) {
            const text = await response.text();
            throw new Error(`Expected JSON, got: ${contentType}. Response: ${text}`);
          }

          const result = await response.json();
          if (result.success) {
            alert('分区更新成功');
            clearAreaForm();
            loadAreas();
          } else {
            alert(`更新失败: ${result.message}`);
          }
        } catch (error) {
          console.error('Update area error:', error);
          alert(`更新失败: ${error.message}`);
        }
      }

      // Delete functions
      async function deleteArea(areaId) {
        if (!confirm(`确定要删除分区 ID ${areaId} 吗？此操作不可撤销。`)) {
          return;
        }

        try {
          const response = await fetch(`/api/manage/areas/${areaId}`, {
            method: 'DELETE'
          });

          const result = await response.json();
          if (result.success) {
            alert('分区删除成功');
            loadAreas();
          } else {
            alert(`删除失败: ${result.message}`);
          }
        } catch (error) {
          alert(`删除失败: ${error.message}`);
        }
      }

      async function deleteChannel(channelName) {
        if (!confirm(`确定要删除频道 "${channelName}" 吗？此操作不可撤销。`)) {
          return;
        }

        try {
          const response = await fetch(`/api/manage/channels/${encodeURIComponent(channelName)}`, {
            method: 'DELETE'
          });

          const result = await response.json();
          if (result.success) {
            alert('频道删除成功');
            loadChannels();
          } else {
            alert(`删除失败: ${result.message}`);
          }
        } catch (error) {
          alert(`删除失败: ${error.message}`);
        }
      }

      // Animated refresh functions
      async function refreshAreas() {
        const btn = document.getElementById('refreshAreasBtn');
        const icon = document.getElementById('refreshAreasIcon');

        // Disable button and start spinning animation
        btn.disabled = true;
        btn.style.opacity = '0.7';
        btn.style.cursor = 'not-allowed';
        icon.style.animation = 'spin 1s linear infinite';

        try {
          await loadAreas();
          showNotification('分区列表已刷新', 'success');
        } catch (error) {
          console.error('Error refreshing areas:', error);
          showNotification('刷新分区列表失败', 'error');
        } finally {
          // Re-enable button and stop spinning
          btn.disabled = false;
          btn.style.opacity = '1';
          btn.style.cursor = 'pointer';
          icon.style.animation = '';
        }
      }

      async function refreshChannels() {
        const btn = document.getElementById('refreshChannelsBtn');
        const icon = document.getElementById('refreshChannelsIcon');

        // Disable button and start spinning animation
        btn.disabled = true;
        btn.style.opacity = '0.7';
        btn.style.cursor = 'not-allowed';
        icon.style.animation = 'spin 1s linear infinite';

        try {
          await loadChannels();
          showNotification('频道列表已刷新', 'success');
        } catch (error) {
          console.error('Error refreshing channels:', error);
          showNotification('刷新频道列表失败', 'error');
        } finally {
          // Re-enable button and stop spinning
          btn.disabled = false;
          btn.style.opacity = '1';
          btn.style.cursor = 'pointer';
          icon.style.animation = '';
        }
      }

      function clearLogs() {
        logLines = [];
        document.getElementById('log-output').innerHTML = '日志已清空';
      }

      async function refreshLogs() {
        try {
          const response = await fetch('/api/logs');
          if (!response.ok) return;

          const data = await response.json();
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

            // Format and display logs
            const logOutput = document.getElementById('log-output');
            logOutput.innerHTML = logLines.map(line => {
              // Escape HTML to prevent XSS
              const escaped = line
                .replace(/&/g, '&amp;')
                .replace(/</g, '&lt;')
                .replace(/>/g, '&gt;');
              // Don't replace spaces - let white-space: pre handle it

              // Color code log levels
              let coloredLine = escaped;
              if (line.includes('ERROR') || line.includes('❌')) {
                coloredLine = `<span style="color: #f87171;">${escaped}</span>`;
              } else if (line.includes('WARN') || line.includes('⚠️')) {
                coloredLine = `<span style="color: #fbbf24;">${escaped}</span>`;
              } else if (line.includes('INFO') || line.includes('✅') || line.includes('🚀')) {
                coloredLine = `<span style="color: #60a5fa;">${escaped}</span>`;
              } else if (line.includes('DEBUG') || line.includes('🔄')) {
                coloredLine = `<span style="color: #a78bfa;">${escaped}</span>`;
              }
              return coloredLine;
            }).join('\n');

            // Auto scroll if enabled
            if (document.getElementById('auto-scroll-checkbox').checked) {
              logOutput.parentElement.scrollTop = logOutput.parentElement.scrollHeight;
            }
          }
        } catch (error) {
          // Silently fail - logs are optional
          console.log('Failed to fetch logs:', error);
        }
      }

      function showNotification(message, type = 'success') {
        const notification = document.createElement('div');
        notification.className = `notification ${type}`;
        notification.textContent = message;
        document.body.appendChild(notification);

        setTimeout(() => {
          notification.remove();
        }, 3000);
      }

      async function refreshBilibiliStatus() {
        const btn = document.getElementById('refreshBilibiliBtn');
        const icon = document.getElementById('refreshBilibiliIcon');

        // Disable button and start spinning animation
        btn.disabled = true;
        btn.style.opacity = '0.7';
        btn.style.cursor = 'not-allowed';
        icon.style.animation = 'spin 1s linear infinite';

        try {
          // Fetch status directly to check if server is responding
          const response = await fetch('/api/status');

          if (!response.ok) {
            throw new Error('服务器响应错误');
          }

          // Refresh the display
          await refreshStatus();
          showNotification('Bilibili status refreshed', 'success');
        } catch (error) {
          console.error('Error refreshing Bilibili status:', error);
          showNotification('刷新失败: ' + (error.message || '服务器未响应'), 'error');
        } finally {
          // Re-enable button and stop spinning
          btn.disabled = false;
          btn.style.opacity = '1';
          btn.style.cursor = 'pointer';
          icon.style.animation = '';
        }
      }

      // YouTube Monitor Toggle
      async function toggleYouTubeMonitor() {
        const toggle = document.getElementById('youtube-monitor-toggle');
        const enabled = toggle.checked;

        try {
          const response = await fetch('/api/toggle-youtube-monitor', {
            method: 'POST',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify({ enabled: enabled })
          });

          const result = await response.json();
          if (result.success) {
            window.configData.youtube = {
              ...(window.configData.youtube || {}),
              enable_monitor: enabled
            };
            showNotification(result.message, 'success');
            // Refresh status to show updated state
            await refreshStatus();
          } else {
            // Revert toggle if save failed
            toggle.checked = !enabled;
            showNotification(result.message || '保存失败', 'error');
          }
        } catch (error) {
          console.error('Failed to toggle YouTube monitor:', error);
          // Revert toggle if save failed
          toggle.checked = !enabled;
          showNotification('保存失败: ' + error.message, 'error');
        }
      }

      // Twitch Monitor Toggle
      async function toggleTwitchMonitor() {
        const toggle = document.getElementById('twitch-monitor-toggle');
        const enabled = toggle.checked;

        try {
          const response = await fetch('/api/toggle-twitch-monitor', {
            method: 'POST',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify({ enabled: enabled })
          });

          const result = await response.json();
          if (result.success) {
            window.configData.twitch = {
              ...(window.configData.twitch || {}),
              enable_monitor: enabled
            };
            showNotification(result.message, 'success');
            // Refresh status to show updated state
            await refreshStatus();
          } else {
            // Revert toggle if save failed
            toggle.checked = !enabled;
            showNotification(result.message || '保存失败', 'error');
          }
        } catch (error) {
          console.error('Failed to toggle Twitch monitor:', error);
          // Revert toggle if save failed
          toggle.checked = !enabled;
          showNotification('保存失败: ' + error.message, 'error');
        }
      }

      async function refreshYouTubeStatus() {
        const btn = document.getElementById('refreshYouTubeBtn');
        const icon = document.getElementById('refreshYouTubeIcon');

        // Disable button and start spinning animation
        btn.disabled = true;
        btn.style.opacity = '0.7';
        btn.style.cursor = 'not-allowed';
        icon.style.animation = 'spin 1s linear infinite';

        try {
          const response = await fetch('/api/refresh/youtube');
          const data = await response.json();
          if (data.success) {
            showNotification('YouTube status refreshed', 'success');
            // Refresh the main status to show updated data
            await refreshStatus();
          } else {
            showNotification(data.message || 'Failed to refresh YouTube status', 'error');
          }
        } catch (error) {
          console.error('Error refreshing YouTube status:', error);
          showNotification('Failed to refresh YouTube status', 'error');
        } finally {
          // Re-enable button and stop spinning
          btn.disabled = false;
          btn.style.opacity = '1';
          btn.style.cursor = 'pointer';
          icon.style.animation = '';
        }
      }

      async function refreshTwitchStatus() {
        const btn = document.getElementById('refreshTwitchBtn');
        const icon = document.getElementById('refreshTwitchIcon');

        // Disable button and start spinning animation
        btn.disabled = true;
        btn.style.opacity = '0.7';
        btn.style.cursor = 'not-allowed';
        icon.style.animation = 'spin 1s linear infinite';

        try {
          const response = await fetch('/api/refresh/twitch');
          const data = await response.json();
          if (data.success) {
            showNotification('Twitch status refreshed', 'success');
            // Refresh the main status to show updated data
            await refreshStatus();
          } else {
            showNotification(data.message || 'Failed to refresh Twitch status', 'error');
          }
        } catch (error) {
          console.error('Error refreshing Twitch status:', error);
          showNotification('Failed to refresh Twitch status', 'error');
        } finally {
          // Re-enable button and stop spinning
          btn.disabled = false;
          btn.style.opacity = '1';
          btn.style.cursor = 'pointer';
          icon.style.animation = '';
        }
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
        syncInfoRowLabelCenter(
        );
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

      function renderBiliNetworkGraph(showCache) {
        const graph = document.getElementById('bili-network-graph');
        if (!graph) {
          return;
        }

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
        const columns = [];

        for (let i = 0; i < graphWidth; i += 1) {
          const pushValue = pushSeries[i - (graphWidth - pushSeries.length)] || 0;
          const cacheValue = cacheSeries[i - (graphWidth - cacheSeries.length)] || 0;
          const cacheHeight = showCache ? Math.max(2, Math.round((cacheValue / maxRate) * 50)) : 0;
          const pushHeight = Math.max(2, Math.round((pushValue / maxRate) * 50));
          const cacheBar = showCache
            ? `<span class="bili-network-bar cache active" style="height: ${cacheHeight}%"></span>`
            : '';
          const pushBar = `<span class="bili-network-bar push active" style="height: ${pushHeight}%"></span>`;
          columns.push(`<span class="bili-network-column">${cacheBar}${pushBar}</span>`);
        }

        graph.innerHTML = columns.join('');
      }

      function applyBiliStreamQualityColor(element, quality) {
        if (quality === '流畅') {
          element.style.color = '#10b981';
        } else if (quality === '波动') {
          element.style.color = '#f59e0b';
        } else if (quality === '卡顿') {
          element.style.color = '#ef4444';
        } else {
          element.style.color = '';
        }
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
          panel.style.display = 'none';
          return;
        }

        panel.style.display = '';
        pushBiliNetworkSample(biliNetworkHistory.push, bili.stream_bitrate_kbps);
        pushBiliNetworkSample(biliNetworkHistory.cache, hasCache ? bili.stream_cache_bitrate_kbps : 0);

        const quality = document.getElementById('bili-network-quality');
        quality.textContent = lastBiliNetworkQuality || 'Live';
        applyBiliStreamQualityColor(quality, lastBiliNetworkQuality);

        document.getElementById('bili-network-push-rate').textContent = formatNetworkRate(bili.stream_bitrate_kbps);
        document.getElementById('bili-network-push-speed-ratio').textContent = formatSpeedRatio(bili.stream_speed);
        document.getElementById('bili-network-push-total').textContent = `Total ${formatBytes(bili.stream_total_bytes)}`;
        const pushFrame = document.getElementById('bili-network-push-frame');
        if (pushFrame) {
          pushFrame.textContent = `FPS ${formatFps(bili.stream_fps)} / Frame ${formatFrameCount(bili.stream_frame)}`;
        }

        const cacheMeter = document.getElementById('bili-network-cache-meter');
        cacheMeter.style.display = hasCache ? '' : 'none';
        if (hasCache) {
          document.getElementById('bili-network-cache-rate').textContent = formatNetworkRate(bili.stream_cache_bitrate_kbps);
          document.getElementById('bili-network-cache-speed-ratio').textContent = formatSpeedRatio(bili.stream_cache_speed);
          document.getElementById('bili-network-cache-total').textContent = `Total ${formatBytes(bili.stream_cache_total_bytes)}`;
        }

        renderBiliNetworkGraph(hasCache);
      }

      async function refreshNetworkStatus() {
        if (networkRefreshInFlight || !lastBiliNetworkLive) {
          return;
        }

        networkRefreshInFlight = true;
        try {
          const response = await fetch('/api/network-status');
          if (!response.ok) {
            throw new Error(`HTTP ${response.status}: ${response.statusText}`);
          }
          const result = await response.json();
          if (result.success && result.data) {
            updateBiliNetworkPanel({
              ...result.data,
              is_live: lastBiliNetworkLive,
              stream_quality: lastBiliNetworkQuality,
            });
          }
        } catch (error) {
          console.log('Failed to refresh network status:', error);
        } finally {
          networkRefreshInFlight = false;
        }
      }

      async function refreshStatus() {
        if (statusRefreshInFlight) {
          return;
        }

        statusRefreshInFlight = true;

        try {
          const response = await fetch('/api/status');

          if (!response.ok) {
            const errorText = await response.text();
            console.error('API Error:', response.status, errorText);
            throw new Error(`HTTP ${response.status}: ${response.statusText}`);
          }

          const data = await response.json();

          if (!data.success) {
            // API returned an error
            throw new Error(data.message || '服务器返回错误');
          }

          if (data.success && data.data) {
            // Update Bilibili status
            const bili = data.data.bilibili;
            document.getElementById('bili-status').className =
              `status-indicator ${bili.is_live ? 'status-live' : 'status-offline'}`;
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
            console.log('Server appears to be down, suppressing error notification');
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
            console.log('Connection failed, suppressing error notification');
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
        }
      }

      async function startStream() {
        try {
          const response = await fetch('/api/start', {
            method: 'POST',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify({})
          });

          // Check if response is ok before parsing JSON
          if (!response.ok) {
            const errorText = await response.text();
            try {
              const errorData = JSON.parse(errorText);
              showNotification(errorData.message || '开播失败', 'error');
            } catch {
              showNotification('开播失败: ' + errorText, 'error');
            }
            return;
          }

          const data = await response.json();

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
          const response = await fetch('/api/stop', {
            method: 'POST'
          });
          const data = await response.json();
          showNotification(data.message || '直播已停止', 'success');
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
        btn.disabled = true;
        btn.style.opacity = '0.7';
        btn.style.cursor = 'not-allowed';
        icon.style.animation = 'spin 1s linear infinite';

        try {
          const response = await fetch('/api/restart', {
            method: 'POST'
          });
          const data = await response.json();
          showNotification(data.message || '已重启流', 'success');
          setTimeout(refreshStatus, 2000);
        } catch (error) {
          showNotification('操作失败: ' + error.message, 'error');
        } finally {
          // Re-enable button and stop spinning after a delay
          setTimeout(() => {
            btn.disabled = false;
            btn.style.opacity = '1';
            btn.style.cursor = 'pointer';
            icon.style.animation = '';
          }, 2000);
        }
      }

      async function changeTitle() {
        const title = document.getElementById('title-input').value.trim();
        if (!title) {
          showNotification('请输入直播标题', 'error');
          return;
        }

        try {
          const response = await fetch('/api/title', {
            method: 'POST',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify({ title: title })
          });
          const data = await response.json();
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
        valueEl.parentElement.style.display = 'none';
        editContainer.style.display = 'flex';
      }

      function hideInfoRowEdit(valueEl, editContainer) {
        editContainer.style.display = 'none';
        valueEl.parentElement.style.display = 'flex';
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

        // Add keyboard event listener
        editInput.onkeydown = function (event) {
          if (event.key === 'Enter') {
            saveTitleEdit();
          } else if (event.key === 'Escape') {
            cancelTitleEdit();
          }
        };
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
        editSelect.innerHTML = '<option value="">选择频道...</option>';

        if (channelsData && channelsData.channels) {
          channelsData.channels.forEach(channel => {
            if (channel.platforms && channel.platforms.youtube) {
              const option = document.createElement('option');
              option.value = JSON.stringify({
                id: channel.platforms.youtube,
                name: channel.name
              });
              option.textContent = channel.name;
              editSelect.appendChild(option);
            }
          });
        }
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
          const response = await fetch('/api/channel', {
            method: 'POST',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify({
              platform: 'youtube',
              channel_id: channelInfo.id,
              channel_name: channelInfo.name
            })
          });
          const data = await response.json();
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
        editSelect.innerHTML = '<option value="">选择分区...</option>';

        if (areasData && areasData.areas) {
          // Sort areas: 其他单机 (235) first, then others
          const sortedAreas = [...areasData.areas].sort((a, b) => {
            if (a.id === 235) return -1;
            if (b.id === 235) return 1;
            return 0;
          });

          sortedAreas.forEach(area => {
            const option = document.createElement('option');
            option.value = area.id;
            option.textContent = area.name; // Removed (${area.id}) for cleaner display
            editSelect.appendChild(option);
          });
        }
      }

      function cancelYtAreaEdit() {
        const areaSpan = document.getElementById('yt-area');
        const editContainer = document.getElementById('yt-area-edit-container');

        hideInfoRowEdit(areaSpan, editContainer);
      }

      async function saveYtAreaEdit() {
        const editSelect = document.getElementById('yt-area-edit-select');
        const areaId = parseInt(editSelect.value);

        if (!areaId) {
          showNotification('请选择分区', 'error');
          return;
        }

        try {
          const response = await fetch('/api/channel', {
            method: 'POST',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify({
              platform: 'youtube',
              area_id: areaId
            })
          });
          const data = await response.json();
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
          const response = await fetch('/api/channel', {
            method: 'POST',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify({
              platform: 'youtube',
              quality: quality // Send technical value to server
            })
          });
          const data = await response.json();
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
          const response = await fetch('/api/channel', {
            method: 'POST',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify({
              platform: 'twitch',
              quality: quality // Send technical value to server
            })
          });
          const data = await response.json();
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
        editSelect.innerHTML = '<option value="">选择频道...</option>';

        if (channelsData && channelsData.channels) {
          channelsData.channels.forEach(channel => {
            if (channel.platforms && channel.platforms.twitch) {
              const option = document.createElement('option');
              option.value = JSON.stringify({
                id: channel.platforms.twitch,
                name: channel.name
              });
              option.textContent = channel.name;
              editSelect.appendChild(option);
            }
          });
        }
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
          const response = await fetch('/api/channel', {
            method: 'POST',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify({
              platform: 'twitch',
              channel_id: channelInfo.id,
              channel_name: channelInfo.name
            })
          });
          const data = await response.json();
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
        editSelect.innerHTML = '<option value="">选择分区...</option>';

        if (areasData && areasData.areas) {
          // Sort areas: 其他单机 (235) first, then others
          const sortedAreas = [...areasData.areas].sort((a, b) => {
            if (a.id === 235) return -1;
            if (b.id === 235) return 1;
            return 0;
          });

          sortedAreas.forEach(area => {
            const option = document.createElement('option');
            option.value = area.id;
            option.textContent = area.name; // Removed (${area.id}) for cleaner display
            editSelect.appendChild(option);
          });
        }
      }

      function cancelTwAreaEdit() {
        const areaSpan = document.getElementById('tw-area');
        const editContainer = document.getElementById('tw-area-edit-container');

        hideInfoRowEdit(areaSpan, editContainer);
      }

      async function saveTwAreaEdit() {
        const editSelect = document.getElementById('tw-area-edit-select');
        const areaId = parseInt(editSelect.value);

        if (!areaId) {
          showNotification('请选择分区', 'error');
          return;
        }

        try {
          const response = await fetch('/api/channel', {
            method: 'POST',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify({
              platform: 'twitch',
              area_id: areaId
            })
          });
          const data = await response.json();
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
        editSelect.innerHTML = '<option value="">选择分区...</option>';

        if (areasData && areasData.areas) {
          // Sort areas: 其他单机 (235) first, then others
          const sortedAreas = [...areasData.areas].sort((a, b) => {
            if (a.id === 235) return -1;
            if (b.id === 235) return 1;
            return 0;
          });

          sortedAreas.forEach(area => {
            const option = document.createElement('option');
            option.value = area.id;
            option.textContent = area.name; // Removed (${area.id}) for cleaner display
            editSelect.appendChild(option);
          });
        }
      }

      function cancelAreaEdit() {
        const areaSpan = document.getElementById('bili-area');
        const editContainer = document.getElementById('area-edit-container');

        hideInfoRowEdit(areaSpan, editContainer);
      }

      async function saveAreaEdit() {
        const editSelect = document.getElementById('area-edit-select');
        const areaId = parseInt(editSelect.value);

        if (!areaId) {
          showNotification('请选择分区', 'error');
          return;
        }

        try {
          const response = await fetch('/api/area', {
            method: 'POST',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify({ area_id: areaId })
          });
          const data = await response.json();
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
          const response = await fetch('/api/title', {
            method: 'POST',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify({ title: title })
          });
          const data = await response.json();
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
        if (icon) {
          icon.style.animation = 'spin 1s linear infinite';
        }

        try {
          const [channelsResponse, areasResponse] = await Promise.all([
            fetch('/api/channels'),
            fetch('/api/areas')
          ]);

          [channelsData, areasData] = await Promise.all([
            channelsResponse.json(),
            areasResponse.json()
          ]);

          // Populate area select
          const areaSelect = document.getElementById('area-select');
          areaSelect.innerHTML = '<option value="">不修改分区</option>';

          // Handle both array and object responses
          let areasList = [];
          if (Array.isArray(areasData)) {
            areasList = areasData;
          } else if (areasData && areasData.areas) {
            areasList = areasData.areas;
          }

          if (areasList.length > 0) {
            // Sort areas: 其他单机 (235) first, then others
            const sortedAreas = [...areasList].sort((a, b) => {
              if (a.id === 235) return -1;
              if (b.id === 235) return 1;
              return 0;
            });

            // Populate channel management area select
            sortedAreas.forEach(area => {
              const option = document.createElement('option');
              option.value = area.id;
              option.textContent = `${area.name} (${area.id})`;
              areaSelect.appendChild(option);
            });

            console.log('Successfully populated', areasList.length, 'areas');
          } else {
            console.error('No areas data found:', areasData);
            showNotification('未找到分区数据', 'error');
          }

          // Update channel list
          updateChannelList();
        } catch (error) {
          console.error('Failed to load channel data:', error);
          showNotification('加载频道数据失败: ' + error.message, 'error');
        } finally {
          // Stop spinning animation when complete
          if (icon) {
            icon.style.animation = '';
          }
        }
      }

      function updateChannelList() {
        const platform = document.getElementById('platform-select').value;
        const channelSelect = document.getElementById('channel-select');

        channelSelect.innerHTML = '<option value="">从 channels.json 选择或手动输入...</option>';

        // Only populate if channels data is available
        if (channelsData && channelsData.channels) {
          channelsData.channels.forEach(channel => {
            if (channel.platforms && channel.platforms[platform]) {
              const option = document.createElement('option');
              option.value = JSON.stringify({
                id: channel.platforms[platform],
                name: channel.name
              });
              option.textContent = channel.name;
              channelSelect.appendChild(option);
            }
          });
        }

        // Don't clear inputs - allow manual entry to persist
        // Update quality options based on platform
        updateQualityOptions();
      }

      function updateQualityOptions() {
        const platform = document.getElementById('platform-select').value;
        const qualitySelect = document.getElementById('quality-select');

        // Store current value
        const currentValue = qualitySelect.value;

        // Clear all options except the first "不修改画质" option
        const firstOption = qualitySelect.querySelector('option[value=""]');
        qualitySelect.innerHTML = '';
        if (firstOption) {
          qualitySelect.appendChild(firstOption);
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

        // Show LOL monitor checkbox if area is 86 (英雄联盟)
        if (areaId === '86') {
          lolMonitorGroup.style.display = 'block';

          // Load current enable_lol_monitor state
          if (window.configData) {
            enableCheckbox.checked = window.configData.enable_lol_monitor || false;
          }

          // Show Riot API Key input if checkbox is checked
          if (enableCheckbox.checked) {
            riotApiKeyGroup.style.display = 'block';
          } else {
            riotApiKeyGroup.style.display = 'none';
          }
        } else {
          lolMonitorGroup.style.display = 'none';
          riotApiKeyGroup.style.display = 'none';
        }
      }

      function toggleRiotApiKeyInputInline() {
        const enableCheckbox = document.getElementById('enable-lol-monitor-inline');
        const riotApiKeyGroup = document.getElementById('riot-api-key-group');

        // Show/hide Riot API Key input based on checkbox
        if (enableCheckbox.checked) {
          riotApiKeyGroup.style.display = 'block';
        } else {
          riotApiKeyGroup.style.display = 'none';
        }
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
        selectElement.innerHTML = '';

        // Add empty option if requested
        if (includeEmpty) {
          const emptyOption = document.createElement('option');
          emptyOption.value = '';
          emptyOption.textContent = '选择画质...';
          selectElement.appendChild(emptyOption);
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
            channelPayload.area_id = parseInt(areaId);

            // Include Riot API Key if area is 86 and key is provided
            if (areaId === '86' && riotApiKey) {
              channelPayload.riot_api_key = riotApiKey;
            }
          }

          if (quality) {
            channelPayload.quality = quality;
          }

          const channelResponse = await fetch('/api/channel', {
            method: 'POST',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify(channelPayload)
          });

          const channelData = await channelResponse.json();

          // If area is 86, also update enable_lol_monitor setting
          if (areaId === '86') {
            const configPayload = {
              enable_lol_monitor: enableLolMonitor
            };

            // Include Riot API Key in config update if provided
            if (riotApiKey) {
              configPayload.riot_api_key = riotApiKey;
            }

            await fetch('/api/config', {
              method: 'POST',
              headers: { 'Content-Type': 'application/json' },
              body: JSON.stringify(configPayload)
            });

            // Update global config
            window.configData.enable_lol_monitor = enableLolMonitor;
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
          document.getElementById(`setup-step-${i}`).style.display = 'none';
          document.getElementById(`step-dot-${i}`).classList.remove('active');
        }

        // Show target step
        document.getElementById(`setup-step-${step}`).style.display = 'block';
        document.getElementById(`step-dot-${step}`).classList.add('active');
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
        group.style.display = checkbox.checked ? 'block' : 'none';
      }

      async function checkLoginStatus() {
        try {
          const response = await fetch('/api/setup/login-status');
          const data = await response.json();

          const statusDiv = document.getElementById('login-status');
          const statusText = document.getElementById('login-status-text');

          if (data.logged_in) {
            statusDiv.style.background = '#d1fae5';
            statusDiv.style.color = '#065f46';
            statusText.textContent = '✅ 已登录 Bilibili';
          } else {
            statusDiv.style.background = '#fee2e2';
            statusDiv.style.color = '#991b1b';
            statusText.textContent = '❌ 未登录，请点击下方按钮登录';
          }
        } catch (error) {
          console.error('Failed to check login status:', error);
          showNotification('检查登录状态失败', 'error');
        }
      }

      let loginPollInterval = null;
      let currentAuthCode = null;

      async function showQrCode() {
        try {
          // Get QR code from API
          const response = await fetch('/api/setup/qrcode');
          const data = await response.json();

          if (!data.success || !data.data) {
            showNotification(data.message || '获取二维码失败', 'error');
            return;
          }

          const { qr_url, auth_code } = data.data;
          currentAuthCode = auth_code;

          // Generate QR code using external API
          const qrContainer = document.getElementById('qr-code-display');
          qrContainer.innerHTML = '';

          // Create QR code using QR Server API
          const qrImg = document.createElement('img');
          qrImg.src = `https://api.qrserver.com/v1/create-qr-code/?size=200x200&data=${encodeURIComponent(qr_url)}`;
          qrImg.style.width = '200px';
          qrImg.style.height = '200px';
          qrContainer.appendChild(qrImg);

          // Show QR code container
          document.getElementById('qr-code-container').style.display = 'block';
          document.getElementById('show-qr-btn').textContent = '🔄 刷新二维码';

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
            const response = await fetch('/api/setup/poll-login', {
              method: 'POST',
              headers: { 'Content-Type': 'application/json' },
              body: JSON.stringify({ auth_code: currentAuthCode })
            });
            const data = await response.json();

            if (data.success && data.data) {
              const { status, message } = data.data;
              document.getElementById('qr-status').textContent = message;

              if (status === 'success') {
                clearInterval(loginPollInterval);
                loginPollInterval = null;
                showNotification('登录成功！', 'success');
                document.getElementById('qr-code-container').style.display = 'none';
                await checkLoginStatus();
              } else if (status === 'expired') {
                clearInterval(loginPollInterval);
                loginPollInterval = null;
                showNotification('二维码已过期，请重新获取', 'error');
                document.getElementById('qr-status').textContent = '二维码已过期';
                document.getElementById('qr-status').style.color = '#ef4444';
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
          const response = await fetch('/api/setup/login', {
            method: 'POST'
          });
          const data = await response.json();

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
        const room = document.getElementById('setup-room').value;
        if (!room || room <= 0) {
          showNotification('请输入有效的直播间号', 'error');
          goToStep(2);
          return;
        }

        // Collect all configuration
        const config = {
          room: parseInt(room),
          interval: parseInt(document.getElementById('setup-interval').value) || 60,
          auto_cover: document.getElementById('setup-auto-cover').checked,
          enable_danmaku_command: document.getElementById('setup-danmaku-command').checked,
          anti_collision: document.getElementById('setup-anti-collision').checked,

          // YouTube
          youtube_channel_name: document.getElementById('setup-yt-name').value || null,
          youtube_channel_id: document.getElementById('setup-yt-id').value || null,
          youtube_area_v2: parseInt(document.getElementById('setup-yt-area').value) || null,
          youtube_quality: document.getElementById('setup-yt-quality').value || null,
          youtube_proxy: document.getElementById('setup-yt-proxy').value || null,

          // Twitch
          twitch_channel_name: document.getElementById('setup-tw-name').value || null,
          twitch_channel_id: document.getElementById('setup-tw-id').value || null,
          twitch_area_v2: parseInt(document.getElementById('setup-tw-area').value) || null,
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
          const response = await fetch('/api/setup/save-config', {
            method: 'POST',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify(config)
          });

          const data = await response.json();

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
      async function checkSetupStatus() {
        try {
          const response = await fetch('/api/setup-status');
          const data = await response.json();

          if (data.needs_setup) {
            // Show setup page
            document.getElementById('setup-page').classList.add('active');
            document.getElementById('main-page').classList.add('hidden');

            // Load areas for dropdowns
            await loadAreasForSetup();

            // Load channels for dropdowns
            await loadChannelsForSetup();

            // Check login status
            await checkLoginStatus();
          } else {
            // Show main page
            document.getElementById('setup-page').classList.remove('active');
            document.getElementById('main-page').classList.remove('hidden');
          }

          return data.needs_setup;
        } catch (error) {
          console.error('Failed to check setup status:', error);
          // On error, show main page
          document.getElementById('setup-page').classList.remove('active');
          document.getElementById('main-page').classList.remove('hidden');
          return false;
        }
      }

      async function loadAreasForSetup() {
        try {
          const response = await fetch('/api/areas');
          const areasData = await response.json();

          let areasList = [];
          if (Array.isArray(areasData)) {
            areasList = areasData;
          } else if (areasData && areasData.areas) {
            areasList = areasData.areas;
          }

          if (areasList.length > 0) {
            const ytAreaSelect = document.getElementById('setup-yt-area');
            const twAreaSelect = document.getElementById('setup-tw-area');

            // Sort areas: 其他单机 (235) first, then others
            const sortedAreas = [...areasList].sort((a, b) => {
              if (a.id === 235) return -1;
              if (b.id === 235) return 1;
              return 0;
            });

            [ytAreaSelect, twAreaSelect].forEach(select => {
              select.innerHTML = '';
              sortedAreas.forEach(area => {
                const option = document.createElement('option');
                option.value = area.id;
                option.textContent = `${area.name} (${area.id})`;
                if (area.id === 235) option.selected = true;
                select.appendChild(option);
              });
            });
          }
        } catch (error) {
          console.error('Failed to load areas:', error);
        }
      }

      async function loadChannelsForSetup() {
        try {
          const response = await fetch('/api/channels');
          const channelsData = await response.json();

          console.log('Loaded channels data:', channelsData);

          if (channelsData && channelsData.channels) {
            const ytChannelSelect = document.getElementById('setup-yt-channel-select');
            const twChannelSelect = document.getElementById('setup-tw-channel-select');

            console.log('Found', channelsData.channels.length, 'channels');

            // Populate YouTube channels
            ytChannelSelect.innerHTML = '<option value="">从 channels.json 选择或手动输入...</option>';
            channelsData.channels.forEach(channel => {
              if (channel.platforms && channel.platforms.youtube) {
                const option = document.createElement('option');
                option.value = JSON.stringify({
                  id: channel.platforms.youtube,
                  name: channel.name
                });
                option.textContent = channel.name;
                ytChannelSelect.appendChild(option);
              }
            });
            console.log('Populated YouTube channels:', ytChannelSelect.options.length - 1);

            // Populate Twitch channels
            twChannelSelect.innerHTML = '<option value="">从 channels.json 选择或手动输入...</option>';
            channelsData.channels.forEach(channel => {
              if (channel.platforms && channel.platforms.twitch) {
                const option = document.createElement('option');
                option.value = JSON.stringify({
                  id: channel.platforms.twitch,
                  name: channel.name
                });
                option.textContent = channel.name;
                twChannelSelect.appendChild(option);
              }
            });
            console.log('Populated Twitch channels:', twChannelSelect.options.length - 1);
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

      // Fetch current version from API
      async function loadVersion() {
        try {
          const response = await fetch('/api/version');
          const data = await response.json();
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
          const response = await fetch('/api/update/check');

          if (!response.ok) {
            throw new Error('无法获取版本信息');
          }

          const data = await response.json();

          if (!data.success) {
            throw new Error(data.message || '检查更新失败');
          }

          const updateInfo = data.data;
          latestUpdateInfo = updateInfo;

          console.log('Update info:', updateInfo);

          // Compare versions
          if (updateInfo.has_update) {
            // New version available
            const updateNotification = document.getElementById('update-notification');
            const updateMessage = document.getElementById('update-message');
            const updateLink = document.getElementById('update-link');
            const autoUpdateBtn = document.getElementById('auto-update-btn');

            let message = `最新版本 v${updateInfo.latest_version} 已发布！当前版本：v${updateInfo.current_version}`;
            if (updateInfo.asset_name) {
              const sizeMB = (updateInfo.asset_size / 1024 / 1024).toFixed(1);
              const buildType = IS_TAURI ? '桌面版 (Tauri)' : '标准版';
              message += `\n文件: ${updateInfo.asset_name} (${sizeMB} MB) — ${buildType}`;
            }
            updateMessage.textContent = message;
            updateMessage.style.whiteSpace = 'pre-line';

            // Set manual download link
            if (updateInfo.download_url) {
              updateLink.href = updateInfo.download_url;
              autoUpdateBtn.style.display = 'block';
            } else {
              // No direct download available, hide auto-update button
              autoUpdateBtn.style.display = 'none';
              updateLink.href = `https://github.com/${GITHUB_REPO}/releases/latest`;
            }

            updateNotification.style.display = 'block';

            showNotification(`发现新版本 v${updateInfo.latest_version}！`, 'success');
          } else {
            showNotification('已是最新版本！', 'success');
            // Hide update notification if it was shown before
            document.getElementById('update-notification').style.display = 'none';
          }
        } catch (error) {
          console.error('Failed to check for updates:', error);
          showNotification('检查更新失败: ' + error.message, 'error');
        }
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
          updateProgress.style.display = 'block';
          updateProgress.textContent = '正在下载更新，请稍候...';

          showNotification('开始下载更新...', 'success');

          const response = await fetch('/api/update/download', {
            method: 'POST',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify({ download_url: latestUpdateInfo.download_url })
          });

          const data = await response.json();

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
      function autoCheckUpdates() {
        const mainPage = document.getElementById('main-page');
        if (mainPage && !mainPage.classList.contains('hidden')) {
          // Check for updates silently (without notification)
          fetch('/api/update/check')
            .then(response => response.json())
            .then(data => {
              if (data.success && data.data && data.data.has_update) {
                const updateInfo = data.data;
                latestUpdateInfo = updateInfo;

                const updateNotification = document.getElementById('update-notification');
                const updateMessage = document.getElementById('update-message');
                const updateLink = document.getElementById('update-link');
                const autoUpdateBtn = document.getElementById('auto-update-btn');

                let message = `最新版本 v${updateInfo.latest_version} 已发布！当前版本：v${updateInfo.current_version}`;
                if (updateInfo.asset_name) {
                  const sizeMB = (updateInfo.asset_size / 1024 / 1024).toFixed(1);
                  message += `\n文件: ${updateInfo.asset_name} (${sizeMB} MB)`;
                }
                updateMessage.textContent = message;
                updateMessage.style.whiteSpace = 'pre-line';

                if (updateInfo.download_url) {
                  updateLink.href = updateInfo.download_url;
                  autoUpdateBtn.style.display = 'block';
                } else {
                  autoUpdateBtn.style.display = 'none';
                  updateLink.href = `https://github.com/${GITHUB_REPO}/releases/latest`;
                }

                updateNotification.style.display = 'block';
              }
            })
            .catch(error => {
              console.log('Auto-update check failed (silent):', error);
            });
        }
      }

      // Theme toggle function
      function toggleTheme() {
        const body = document.body;
        const themeToggle = document.getElementById('theme-toggle');

        if (body.classList.contains('light-theme')) {
          // Switch to dark theme (Dracula)
          body.classList.remove('light-theme');
          themeToggle.textContent = '🌙 暗色';
          localStorage.setItem('theme', 'dark');
        } else {
          // Switch to light theme
          body.classList.add('light-theme');
          themeToggle.textContent = '☀️ 亮色';
          localStorage.setItem('theme', 'light');
        }
      }

      // Load saved theme preference
      function loadTheme() {
        const savedTheme = localStorage.getItem('theme');
        const themeToggle = document.getElementById('theme-toggle');

        if (savedTheme === 'light') {
          document.body.classList.add('light-theme');
          if (themeToggle) themeToggle.textContent = '☀️ 亮色';
        } else {
          // Default to dark theme (Dracula)
          document.body.classList.remove('light-theme');
          if (themeToggle) themeToggle.textContent = '🌙 暗色';
        }
      }

      // Load theme on page load
      loadTheme();

      // Check setup status on page load
      checkSetupStatus().then(needsSetup => {
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
        if (e.target.closest('.switch-button')) {
          const button = e.target.closest('.switch-button');
          const channelId = button.dataset.channelId;
          const suggestedAreaId = button.dataset.suggestedAreaId ? parseInt(button.dataset.suggestedAreaId) : null;
          const title = button.dataset.title;
          const topicId = button.dataset.topicId;
          const status = button.dataset.status;
          const platform = button.dataset.platform || 'youtube';
          const twitchChannelId = button.dataset.twitchId || '';
          const externalLink = button.dataset.externalLink || '';

          switchToHolodexStream(channelId, suggestedAreaId, title, topicId, status, platform, twitchChannelId, externalLink);
        }

        if (e.target.closest('.crop-switch-button')) {
          const button = e.target.closest('.crop-switch-button');
          const channelId = button.dataset.channelId;
          const suggestedAreaId = button.dataset.suggestedAreaId ? parseInt(button.dataset.suggestedAreaId) : null;
          const title = button.dataset.title;
          const topicId = button.dataset.topicId;
          const status = button.dataset.status;
          const platform = button.dataset.platform || 'youtube';
          const twitchChannelId = button.dataset.twitchId || '';
          const externalLink = button.dataset.externalLink || '';

          cropAndSwitchToHolodexStream(channelId, suggestedAreaId, title, topicId, status, platform, twitchChannelId, externalLink);
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
          const response = await fetch('/api/channel', {
            method: 'POST',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify({
              platform: 'youtube',
              cookies_from_browser: cookiesFromBrowser,
              cookies_file: cookiesFile
            })
          });
          const data = await response.json();
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
        const container = document.getElementById('cropCanvasContainer');
        if (container) container.style.display = 'none';
        cropImage = null;
      }

      async function clearCropConfig(platform) {
        if (!confirm(`确定要清除${platform === 'youtube' ? 'YouTube' : 'Twitch'}的裁剪设置吗？`)) {
          return;
        }

        try {
          const response = await fetch('/api/crop/update', {
            method: 'POST',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify({
              platform,
              enabled: false
            })
          });

          const result = await response.json();

          if (result.success) {
            showNotification('裁剪设置已清除', 'success');
            // Update status display
            const statusId = platform === 'youtube' ? 'yt-crop-status' : 'tw-crop-status';
            document.getElementById(statusId).textContent = '关闭';
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
          const response = await fetch(`/api/crop/capture/${platform}`, {
            method: 'POST'
          });

          const result = await response.json();

          console.log('Capture result:', result); // Debug log

          if (result.success && result.message) {
            // Load the captured image (base64 is in message field)
            cropImage = new Image();
            cropImage.onload = function () {
              console.log('Image loaded:', cropImage.width, 'x', cropImage.height); // Debug log
              cropCanvas = document.getElementById('cropCanvas');
              cropCtx = cropCanvas.getContext('2d');

              // Show container FIRST so canvas has dimensions
              document.getElementById('cropCanvasContainer').style.display = 'block';

              // Set canvas size to image size
              cropCanvas.width = cropImage.width;
              cropCanvas.height = cropImage.height;

              // Fill with white background first (for debugging)
              cropCtx.fillStyle = 'white';
              cropCtx.fillRect(0, 0, cropCanvas.width, cropCanvas.height);

              // Draw image
              cropCtx.drawImage(cropImage, 0, 0);

              // Setup canvas interaction
              setupCanvasInteraction();

              showNotification('直播帧已捕获，请在图片上选择裁剪区域', 'success');
            };
            cropImage.onerror = function (e) {
              console.error('Image load error:', e); // Debug log
              showNotification('图片加载失败', 'error');
            };
            cropImage.src = result.message;
          } else {
            console.error('Capture failed:', result); // Debug log
            showNotification(result.message || '捕获失败，请确保直播正在进行', 'error');
          }
        } catch (error) {
          console.error('Capture error:', error); // Debug log
          showNotification('捕获失败: ' + error.message, 'error');
        }
      }

      async function loadCurrentCropSettings() {
        const platform = document.getElementById('cropPlatform').value;
        try {
          const response = await fetch(`/api/crop/${platform}`);
          const result = await response.json();
          if (result.success && result.data && result.data.enabled) {
            document.getElementById('cropX').value = result.data.x;
            document.getElementById('cropY').value = result.data.y;
            document.getElementById('cropWidth').value = result.data.width;
            document.getElementById('cropHeight').value = result.data.height;
          }
        } catch (error) {
          console.error('Failed to load crop settings:', error);
        }
      }

      function loadCropImage(event) {
        const file = event.target.files[0];
        if (!file) return;

        console.log('Loading image file:', file.name, file.size); // Debug log

        const reader = new FileReader();
        reader.onload = function (e) {
          console.log('FileReader loaded, data length:', e.target.result.length); // Debug log
          cropImage = new Image();
          cropImage.onload = function () {
            console.log('Image loaded:', cropImage.width, 'x', cropImage.height); // Debug log
            cropCanvas = document.getElementById('cropCanvas');
            cropCtx = cropCanvas.getContext('2d');

            // Show container FIRST so canvas has dimensions
            document.getElementById('cropCanvasContainer').style.display = 'block';

            // Set canvas size to image size
            cropCanvas.width = cropImage.width;
            cropCanvas.height = cropImage.height;

            console.log('Canvas size set:', cropCanvas.width, 'x', cropCanvas.height); // Debug log

            // Fill with white background first (for debugging)
            cropCtx.fillStyle = 'white';
            cropCtx.fillRect(0, 0, cropCanvas.width, cropCanvas.height);

            // Draw image
            cropCtx.drawImage(cropImage, 0, 0);

            console.log('Image drawn to canvas'); // Debug log

            // Check if canvas is visible
            const rect = cropCanvas.getBoundingClientRect();
            console.log('Canvas display size:', rect.width, 'x', rect.height); // Debug log
            console.log('Canvas style:', window.getComputedStyle(cropCanvas).display); // Debug log

            // Setup canvas interaction
            setupCanvasInteraction();

            showNotification('图片已加载，请在图片上拖动鼠标选择裁剪区域', 'success');
          };
          cropImage.onerror = function (err) {
            console.error('Image load error:', err); // Debug log
            showNotification('图片加载失败', 'error');
          };
          cropImage.src = e.target.result;
        };
        reader.onerror = function (err) {
          console.error('FileReader error:', err); // Debug log
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

      function applyAspectRatio() {
        currentAspectRatio = document.getElementById('cropAspectRatio').value;

        if (!cropCanvas || currentAspectRatio === 'free') return;

        // Generate centered box with selected aspect ratio
        const canvasWidth = cropCanvas.width;
        const canvasHeight = cropCanvas.height;

        let width, height;
        const ratio = currentAspectRatio === '1:1' ? 1 :
          currentAspectRatio === '16:9' ? 16 / 9 :
            currentAspectRatio === '16:10' ? 16 / 10 :
              currentAspectRatio === '9:16' ? 9 / 16 :
                currentAspectRatio === '10:16' ? 10 / 16 : 1;

        // Calculate size to fit 60% of canvas
        if (ratio >= 1) {
          width = canvasWidth * 0.6;
          height = width / ratio;
          if (height > canvasHeight * 0.6) {
            height = canvasHeight * 0.6;
            width = height * ratio;
          }
        } else {
          height = canvasHeight * 0.6;
          width = height * ratio;
          if (width > canvasWidth * 0.6) {
            width = canvasWidth * 0.6;
            height = width / ratio;
          }
        }

        const x = (canvasWidth - width) / 2;
        const y = (canvasHeight - height) / 2;

        document.getElementById('cropX').value = Math.round(x);
        document.getElementById('cropY').value = Math.round(y);
        document.getElementById('cropWidth').value = Math.round(width);
        document.getElementById('cropHeight').value = Math.round(height);

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

          const x = parseInt(document.getElementById('cropX').value) || 0;
          const y = parseInt(document.getElementById('cropY').value) || 0;
          const width = parseInt(document.getElementById('cropWidth').value) || 0;
          const height = parseInt(document.getElementById('cropHeight').value) || 0;

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

            cropBox.style.display = 'block';
            cropBox.style.left = (mouseX / scaleX) + 'px';
            cropBox.style.top = (mouseY / scaleY) + 'px';
            cropBox.style.width = '0px';
            cropBox.style.height = '0px';
          }
        });

        newCanvas.addEventListener('mousemove', (e) => {
          const rect = newCanvas.getBoundingClientRect();
          const scaleX = newCanvas.width / rect.width;
          const scaleY = newCanvas.height / rect.height;

          const currentX = (e.clientX - rect.left) * scaleX;
          const currentY = (e.clientY - rect.top) * scaleY;

          const x = parseInt(document.getElementById('cropX').value) || 0;
          const y = parseInt(document.getElementById('cropY').value) || 0;
          const width = parseInt(document.getElementById('cropWidth').value) || 0;
          const height = parseInt(document.getElementById('cropHeight').value) || 0;

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

            document.getElementById('cropX').value = Math.round(newX);
            document.getElementById('cropY').value = Math.round(newY);
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
              const ratio = currentAspectRatio === '1:1' ? 1 :
                currentAspectRatio === '16:9' ? 16 / 9 :
                  currentAspectRatio === '16:10' ? 16 / 10 :
                    currentAspectRatio === '9:16' ? 9 / 16 :
                      currentAspectRatio === '10:16' ? 10 / 16 : 1;

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

            document.getElementById('cropX').value = Math.round(newX);
            document.getElementById('cropY').value = Math.round(newY);
            document.getElementById('cropWidth').value = Math.round(newWidth);
            document.getElementById('cropHeight').value = Math.round(newHeight);
            updateCropBox();
          } else if (isDrawing) {
            // Draw new box
            let width = Math.abs(currentX - cropStartX);
            let height = Math.abs(currentY - cropStartY);

            // Apply aspect ratio if selected
            if (currentAspectRatio !== 'free') {
              const ratio = currentAspectRatio === '1:1' ? 1 :
                currentAspectRatio === '16:9' ? 16 / 9 :
                  currentAspectRatio === '16:10' ? 16 / 10 :
                    currentAspectRatio === '9:16' ? 9 / 16 :
                      currentAspectRatio === '10:16' ? 10 / 16 : 1;
              if (width / height > ratio) {
                width = height * ratio;
              } else {
                height = width / ratio;
              }
            }

            const x = currentX > cropStartX ? cropStartX : cropStartX - width;
            const y = currentY > cropStartY ? cropStartY : cropStartY - height;

            cropBox.style.left = (x / scaleX) + 'px';
            cropBox.style.top = (y / scaleY) + 'px';
            cropBox.style.width = (width / scaleX) + 'px';
            cropBox.style.height = (height / scaleY) + 'px';

            document.getElementById('cropX').value = Math.round(x);
            document.getElementById('cropY').value = Math.round(y);
            document.getElementById('cropWidth').value = Math.round(width);
            document.getElementById('cropHeight').value = Math.round(height);
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
        ['cropX', 'cropY', 'cropWidth', 'cropHeight'].forEach(id => {
          const input = document.getElementById(id);
          input.removeEventListener('input', updateCropBox);
          input.addEventListener('input', updateCropBox);
        });
      }

      function updateCropBox() {
        if (!cropCanvas) return;
        const rect = cropCanvas.getBoundingClientRect();
        const scaleX = cropCanvas.width / rect.width;
        const scaleY = cropCanvas.height / rect.height;

        const x = parseInt(document.getElementById('cropX').value) || 0;
        const y = parseInt(document.getElementById('cropY').value) || 0;
        const width = parseInt(document.getElementById('cropWidth').value) || 0;
        const height = parseInt(document.getElementById('cropHeight').value) || 0;

        const cropBox = document.getElementById('cropBox');
        cropBox.style.display = 'block';
        cropBox.style.left = (x / scaleX) + 'px';
        cropBox.style.top = (y / scaleY) + 'px';
        cropBox.style.width = (width / scaleX) + 'px';
        cropBox.style.height = (height / scaleY) + 'px';
      }

      async function applyCrop() {
        const platform = document.getElementById('cropPlatform').value;
        const x = parseInt(document.getElementById('cropX').value) || 0;
        const y = parseInt(document.getElementById('cropY').value) || 0;
        const width = parseInt(document.getElementById('cropWidth').value) || 0;
        const height = parseInt(document.getElementById('cropHeight').value) || 0;

        if (width === 0 || height === 0) {
          showNotification('请先输入裁剪区域尺寸', 'error');
          return;
        }

        try {
          const response = await fetch('/api/crop/update', {
            method: 'POST',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify({
              platform,
              enabled: true,
              x,
              y,
              width,
              height
            })
          });

          const result = await response.json();

          if (result.success) {
            showNotification('裁剪配置已保存', 'success');
            // Update status display
            const statusId = platform === 'youtube' ? 'yt-crop-status' : 'tw-crop-status';
            document.getElementById(statusId).textContent = '开启';

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
        const x = parseInt(document.getElementById('cropX').value) || 0;
        const y = parseInt(document.getElementById('cropY').value) || 0;
        const width = parseInt(document.getElementById('cropWidth').value) || 0;
        const height = parseInt(document.getElementById('cropHeight').value) || 0;

        if (width === 0 || height === 0) {
          showNotification('请先输入裁剪区域尺寸', 'error');
          return;
        }

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
          const cropResponse = await fetch('/api/crop/update', {
            method: 'POST',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify({
              platform,
              enabled: true,
              x,
              y,
              width,
              height
            })
          });

          const cropResult = await cropResponse.json();

          if (cropResult.success) {
            showNotification('裁剪配置已保存，正在重启流...', 'success');
            // Update status display
            const statusId = platform === 'youtube' ? 'yt-crop-status' : 'tw-crop-status';
            document.getElementById(statusId).textContent = '开启';

            closeCropModal();

            // Step 3: Restart the stream
            const restartResponse = await fetch('/api/restart', {
              method: 'POST'
            });

            const restartResult = await restartResponse.json();

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
          const response = await fetch('/api/crop/update', {
            method: 'POST',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify({
              platform,
              enabled: false
            })
          });

          const result = await response.json();

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
        // Update currentAspectRatio
        currentAspectRatio = ratio;

        // Update button styles - reset all buttons
        document.querySelectorAll('.ratio-button').forEach(btn => {
          btn.classList.remove('active');
          btn.style.background = 'var(--input-bg)';
          btn.style.color = 'var(--text-primary)';
          btn.style.border = '2px solid var(--input-border)';
          btn.style.boxShadow = 'none';
        });

        // Highlight the selected button with a border
        const activeBtn = document.getElementById(`ratio-${ratio.replace(':', '-')}`);
        if (activeBtn) {
          activeBtn.classList.add('active');
          activeBtn.style.border = '2px solid #89b4fa';
          activeBtn.style.boxShadow = '0 0 0 2px rgba(137, 180, 250, 0.2)';
        }

        // If canvas is loaded, apply the aspect ratio
        if (cropCanvas && ratio !== 'free') {
          const canvasWidth = cropCanvas.width;
          const canvasHeight = cropCanvas.height;

          let width, height;
          const ratioValue = ratio === '1:1' ? 1 :
            ratio === '16:9' ? 16 / 9 :
              ratio === '16:10' ? 16 / 10 :
                ratio === '9:16' ? 9 / 16 :
                  ratio === '10:16' ? 10 / 16 : 1;

          // Calculate size to fit 60% of canvas
          if (ratioValue >= 1) {
            width = canvasWidth * 0.6;
            height = width / ratioValue;
            if (height > canvasHeight * 0.6) {
              height = canvasHeight * 0.6;
              width = height * ratioValue;
            }
          } else {
            height = canvasHeight * 0.6;
            width = height * ratioValue;
            if (width > canvasWidth * 0.6) {
              width = canvasWidth * 0.6;
              height = width / ratioValue;
            }
          }

          const x = (canvasWidth - width) / 2;
          const y = (canvasHeight - height) / 2;

          document.getElementById('cropX').value = Math.round(x);
          document.getElementById('cropY').value = Math.round(y);
          document.getElementById('cropWidth').value = Math.round(width);
          document.getElementById('cropHeight').value = Math.round(height);

          updateCropBox();
        }
      }
