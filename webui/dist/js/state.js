// state.js — extracted from app.js

export const state = {
  channelsData: null,
  areasData: null,
  activeView: 'overview',
  monitorToggleSaveState: new Map(),
  hooks: {},
};

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
    toggle.checked = enabled;
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
  if (!state.channelsData || !Array.isArray(state.channelsData.channels)) return;

  state.channelsData.channels.forEach(channel => {
    const platforms = channel.platforms || {};
    if (platforms[platform]) {
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
  applyMonitorToggleConfigState,
  updateDanmakuCommandToggle,
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
