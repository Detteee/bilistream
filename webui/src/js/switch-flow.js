// switch-flow.js — switching the restream target: a direct switch, the area
// picker when the row has no suggested area, and the POST that asks the server
// to switch.
//
// The dashboard's 切换/crop buttons and the public page's command flow are
// different paths; this one is the dashboard's. The pending action survives the
// modal, so an action started from a card still carries its channel, topic,
// platform and link after the user picks an area.

import { createSwitchPayload } from './admin-stream-actions.js';
import { getJson, postJsonApi } from './api.js';
import { bindDialog, bindListboxKeyboard } from './dialog.js';
import { parseInteger, showNotification } from './dom.js';
import { getAreaList, getSortedAreas, normalizeAreaData, state } from './state.js';
import { refreshStatus } from './status-refresh.js';

/// The switch being prepared: the action kept while the area picker is open,
/// and the area the user picked. One object because both the entry point and the
/// modal's handlers assign them, and ESM bindings are read-only.
const switchFlowState = { pending: null, selectedAreaId: null };
async function switchToStream(channelId, suggestedAreaId, title, topicId, status, platform, twitchChannelId, externalLink) {
  if (!suggestedAreaId) {
    // Show modal for area selection
    switchFlowState.pending = { channelId, areaId: null, title, topicId, status, platform, twitchChannelId, externalLink };
    await showAreaModal();
  } else {
    // Direct switch with suggested area
    await performSwitch(channelId, suggestedAreaId, title, topicId, status, platform, twitchChannelId, externalLink);
  }
}

async function showAreaModal() {
  const modal = document.getElementById('area-modal');
  const list = document.getElementById('modal-area-list');
  const confirm = document.getElementById('confirm-area-selection-btn');
  if (!modal || !list) return;

  // Load areas if not already loaded
  if (!state.areasData) {
    try {
      state.areasData = normalizeAreaData(await getJson('/api/areas'));
    } catch (error) {
      showNotification('加载分区列表失败', 'error');
      return;
    }
  }

  switchFlowState.selectedAreaId = null;
  if (confirm) {
    confirm.disabled = true;
  }

  const areas = getSortedAreas(getAreaList());
  if (areas.length === 0) {
    const empty = document.createElement('p');
    empty.className = 'area-option-empty';
    empty.textContent = '没有可用分区';
    list.replaceChildren(empty);
  } else {
    list.replaceChildren(...areas.map((area) => createAreaPickerOption(area)));
  }

  modal.classList.remove('hidden');
  list.querySelector('.area-option')?.focus();
}

function createAreaPickerOption(area) {
  const button = document.createElement('button');
  button.type = 'button';
  button.className = 'area-option';
  button.setAttribute('role', 'option');
  button.setAttribute('aria-selected', 'false');
  button.dataset.id = String(area.id);
  button.textContent = `${area.name} (${area.id})`;
  button.addEventListener('click', () => {
    if (switchFlowState.selectedAreaId === area.id) {
      confirmAreaSelection();
      return;
    }
    selectAreaOption(area.id);
  });
  return button;
}

function selectAreaOption(id) {
  switchFlowState.selectedAreaId = id;
  const confirm = document.getElementById('confirm-area-selection-btn');
  if (confirm) {
    confirm.disabled = false;
  }
  document.querySelectorAll('#modal-area-list .area-option').forEach((button) => {
    const on = Number(button.dataset.id) === id;
    button.classList.toggle('is-selected', on);
    button.setAttribute('aria-selected', on ? 'true' : 'false');
  });
}

function closeAreaModal() {
  document.getElementById('area-modal')?.classList.add('hidden');
  switchFlowState.pending = null;
  switchFlowState.selectedAreaId = null;
}

function initAreaModalControls() {
  bindDialog('area-modal', closeAreaModal);
  bindListboxKeyboard(document.getElementById('modal-area-list'));
  document
    .getElementById('confirm-area-selection-btn')
    ?.addEventListener('click', confirmAreaSelection);
  document
    .getElementById('cancel-area-selection-btn')
    ?.addEventListener('click', closeAreaModal);
}

async function confirmAreaSelection() {
  const areaId = parseInteger(switchFlowState.selectedAreaId, 0);

  if (!areaId) {
    showNotification('请选择分区', 'error');
    return;
  }

  if (!switchFlowState.pending) {
    console.error('No pending switch data');
    closeAreaModal();
    return;
  }

  const channelId = switchFlowState.pending.channelId;
  const { title, topicId, status, platform, twitchChannelId, externalLink } = switchFlowState.pending;
  closeAreaModal();
  await performSwitch(channelId, areaId, title, topicId, status, platform, twitchChannelId, externalLink);
}
async function performSwitch(channelId, areaId, title, topicId, status, platform, twitchChannelId, externalLink) {
  try {
    const payload = createSwitchPayload({
      channelId,
      title,
      topicId,
      status,
      platform,
      twitchId: twitchChannelId,
      externalLink
    }, areaId);
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
    showNotification('频道切换失败：' + error.message, 'error');
  }
}

export { initAreaModalControls, switchToStream };
