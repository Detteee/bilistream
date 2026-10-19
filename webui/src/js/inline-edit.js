// inline-edit.js — showing and hiding an inline editor row, plus the HLS cache
// editor built on it.
//
// The dashboard edits several values in place: a row swaps its display value for
// an editor. The same two helpers drive the tile's title, channel, area and
// quality edits, so they live here rather than in the file that happens to have
// the most callers. The HLS cache editor is included because it is
// self-contained: it reads and writes its own endpoint and needs nothing from
// the platform edit code.

import { getJson, postJsonApi } from './api.js';
import { parseInteger, showNotification } from './dom.js';
import { formatHlsCacheStatus } from './format.js';

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
function toggleNcHlsCacheEdit() {
  toggleHlsCacheEdit('nc', 'niconico');
}
function cancelNcHlsCacheEdit() {
  cancelHlsCacheEdit('nc');
}
function saveNcHlsCacheEdit() {
  return saveHlsCacheEdit('nc', 'niconico', 'Niconico');
}

export {
  cancelHlsCacheEdit,
  cancelNcHlsCacheEdit,
  cancelTwHlsCacheEdit,
  cancelYtHlsCacheEdit,
  hideInfoRowEdit,
  hlsCacheDom,
  readHlsCacheEditorValues,
  saveHlsCacheEdit,
  saveNcHlsCacheEdit,
  saveTwHlsCacheEdit,
  saveYtHlsCacheEdit,
  setHlsCacheEditorValues,
  setHlsCacheLatencyInputState,
  showInfoRowEdit,
  toggleHlsCacheEdit,
  toggleNcHlsCacheEdit,
  toggleTwHlsCacheEdit,
  toggleYtHlsCacheEdit,
};
