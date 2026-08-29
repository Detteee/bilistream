// state.js — extracted from app.js

export const state = {
  channelsData: null,
  areasData: null,
  managedDataGeneration: 0,
  activeView: 'overview',
  monitorToggleSaveState: new Map(),
  localNodeCanEnableMonitorToggles: true,
  hooks: {},
};

// Global config data for access across functions
window.configData = {
  enable_lol_monitor: false,
  riot_api_key: '',
  holodex_api_key: '',
  youtube_api_key: '',
  priority_channel: {},
  cluster: {},
  bilibili: {},
  youtube: {},
  twitch: {},
  niconico: {}
};
function mergeConfigData(config) {
  window.configData = {
    ...window.configData,
    ...config,
    niconico: { ...window.configData.niconico, ...config.niconico },
    cluster: { ...window.configData.cluster, ...config.cluster },
    priority_channel: { ...window.configData.priority_channel, ...config.priority_channel },
    bilibili: { ...window.configData.bilibili, ...config.bilibili },
    youtube: { ...window.configData.youtube, ...config.youtube },
    twitch: { ...window.configData.twitch, ...config.twitch }
  };
  return window.configData;
}
function updateMonitorToggleStates(config = window.configData) {
  const youtubeToggle = document.getElementById('youtube-monitor-toggle');
  const twitchToggle = document.getElementById('twitch-monitor-toggle');
  const niconicoToggle = document.getElementById('niconico-monitor-toggle');

  if (youtubeToggle) {
    applyMonitorToggleConfigState(youtubeToggle, 'youtube-monitor-toggle', config.youtube?.enable_monitor !== false);
  }
  if (twitchToggle) {
    applyMonitorToggleConfigState(twitchToggle, 'twitch-monitor-toggle', config.twitch?.enable_monitor !== false);
  }
  if (niconicoToggle) {
    applyMonitorToggleConfigState(niconicoToggle, 'niconico-monitor-toggle', config.niconico?.enable_monitor === true);
  }
}

function applyPriorityAutoRestartToggle(config = window.configData) {
  const autoRestartToggle = document.getElementById('priority-auto-restart-toggle');
  if (autoRestartToggle && config.priority_channel) {
    applyMonitorToggleConfigState(autoRestartToggle, 'priority-auto-restart-toggle', !!config.priority_channel.auto_restart);
  }
}
function applyMonitorToggleConfigState(toggle, toggleId, enabled) {
  const toggleState = state.monitorToggleSaveState.get(toggleId);
  if (toggleState?.timer || toggleState?.inFlight) {
    return;
  }

  toggle.checked = enabled;
  if (toggleState) {
    toggleState.confirmed = enabled;
    toggleState.desired = enabled;
  }
}
function updateDanmakuCommandToggle(enabled) {
  const toggle = document.getElementById('bili-danmaku-command-toggle');
  if (toggle && typeof enabled === 'boolean') {
    applyMonitorToggleConfigState(toggle, 'bili-danmaku-command-toggle', enabled);
  }
}
// Stored as holodex_monitor_gate; the switch shows the inverse, yt-dlp 兜底.
function applyHolodexMonitorGateToggle(enabled) {
  const toggle = document.getElementById('holodex-monitor-gate-toggle');
  if (toggle) {
    toggle.checked = !enabled;
  }
}

export function invalidateManagedData() {
  state.managedDataGeneration += 1;
  state.channelsData = null;
  state.areasData = null;
  for (const id of ['channels-content', 'areas-content']) {
    const list = document.getElementById(id);
    if (list) delete list.dataset.loaded;
  }
}
function isViewActive(name) {
  return state.activeView === name;
}
function createAreaOption(value, label) {
  const option = document.createElement('option');
  option.value = value;
  option.textContent = label;
  return option;
}

function createSelectOption(value, label) {
  return createAreaOption(value, label);
}
function normalizeAreaData(data) {
  return Array.isArray(data) ? { areas: data } : (data || { areas: [] });
}
function getAreaList(data = state.areasData) {
  return normalizeAreaData(data).areas || [];
}
/// Bilibili's catch-all 其他单机. Pinned first in every picker so it is not
/// buried in id order.
const DEFAULT_AREA_ID = 235;

function isDefaultArea(area) {
  return Number(area?.id) === DEFAULT_AREA_ID;
}

function getSortedAreas(areas) {
  const defaults = [];
  const rest = [];
  for (const area of areas || []) {
    (isDefaultArea(area) ? defaults : rest).push(area);
  }
  return defaults.concat(rest);
}
function appendAreaOptions(select, areas, includeId = false) {
  getSortedAreas(areas).forEach(area => {
    const label = includeId ? `${area.name} (${area.id})` : area.name;
    select.appendChild(createAreaOption(area.id, label));
  });
}
function niconicoChannelIdFromCatalog(channel) {
  const fromPlatform = String(channel?.platforms?.niconico || '').trim();
  if (fromPlatform) return fromPlatform;
  if (!(channel?.niconico_name || '').trim()) return '';
  const aliases = Array.isArray(channel.aliases) ? channel.aliases : [];
  return aliases
    .map(alias => String(alias).trim())
    .find(alias => alias && !/^UC[\w-]{20,}$/i.test(alias)) || '';
}
function restreamNameForPlatform(channel, platform) {
  if (platform === 'niconico') {
    const niconicoName = (channel.niconico_name || '').trim();
    if (niconicoName) return niconicoName;
  }
  return channel.name;
}
function createPlatformChannelOption(channel, platform) {
  const platforms = channel.platforms || {};
  const id = platform === 'niconico'
    ? niconicoChannelIdFromCatalog(channel)
    : platforms[platform];
  const restreamName = restreamNameForPlatform(channel, platform);
  const option = createSelectOption(
    JSON.stringify({
      id,
      name: restreamName
    }),
    restreamName
  );
  if (platform === 'niconico' && channel.name && channel.name !== restreamName) {
    option.title = channel.name;
  }
  return option;
}
function appendPlatformChannelOptions(select, platform) {
  if (!state.channelsData || !Array.isArray(state.channelsData.channels)) return;

  state.channelsData.channels.forEach(channel => {
    const platforms = channel.platforms || {};
    const hasPlatform = platform === 'niconico'
      ? Boolean(niconicoChannelIdFromCatalog(channel))
      : Boolean(platforms[platform]);
    if (hasPlatform) {
      select.appendChild(createPlatformChannelOption(channel, platform));
    }
  });
}
// Helper function to get area name by ID
function getAreaName(areaId) {
  if (!state.areasData || !state.areasData.areas) return areaId.toString();
  const area = state.areasData.areas.find(a => a.id === areaId);
  return area ? area.name : areaId.toString();
}

export {
  mergeConfigData,
  updateMonitorToggleStates,
  applyPriorityAutoRestartToggle,
  applyMonitorToggleConfigState,
  updateDanmakuCommandToggle,
  applyHolodexMonitorGateToggle,
  isViewActive,
  createAreaOption,
  createSelectOption,
  normalizeAreaData,
  getAreaList,
  getSortedAreas,
  appendAreaOptions,
  createPlatformChannelOption,
  appendPlatformChannelOptions,
  getAreaName,
};
