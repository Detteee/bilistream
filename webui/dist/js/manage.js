// manage.js — extracted from app.js

import { appendAntiCollisionRemoveIcon, appendEditIcon, readInputValue, parseCommaSeparatedInput, readIntegerInput, setInputValue, setElementText, showNotification, setButtonLoading } from './dom.js';
import { managementRequest, managementJsonRequest, deleteManagementResource } from './api.js';
import { state, invalidateManagedData } from './state.js';

let editingAreaId = null;
let editingChannelName = null;
function submitAreaForm() {
  if (editingAreaId !== null) {
    return updateArea(editingAreaId);
  }
  return addArea();
}
function submitChannelForm() {
  if (editingChannelName !== null) {
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
  const generation = state.managedDataGeneration;
  try {
    const result = await managementRequest('/api/manage/areas');
    if (generation !== state.managedDataGeneration) return loadAreas();

    if (result.success) {
      state.areasData = result.data;
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
      notifyAreasJsonChanged();
    } else {
      showNotification(`添加失败: ${result.message}`, 'error');
    }
  } catch (error) {
    console.error('Add area error:', error);
    showNotification(`添加失败: ${error.message}`, 'error');
  }
}
function showManagementSuccess(result, fallbackMessage) {
  invalidateManagedData();
  showNotification(result.message || fallbackMessage, 'success');
}
function notifyAreasJsonChanged() {
  document.dispatchEvent(new Event('areas-json-changed'));
}
function readAreaForm() {
  return {
    id: readIntegerInput('area-id', 0),
    name: readInputValue('area-name'),
    title_keywords: parseCommaSeparatedInput('area-keywords'),
    aliases: parseCommaSeparatedInput('area-aliases')
  };
}
// Channel management functions
async function loadChannels() {
  const generation = state.managedDataGeneration;
  try {
    const result = await managementRequest('/api/manage/channels');
    if (generation !== state.managedDataGeneration) return loadChannels();

    if (result.success) {
      state.channelsData = result.data;
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

  if (payload.name !== editingChannelName) {
    showNotification('频道名称是配置标识，编辑时不能更改；请另行添加频道', 'error');
    return;
  }

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

    editingChannelName = channel.name;
    document.getElementById('channel-name').readOnly = true;
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

  editingChannelName = null;
  document.getElementById('channel-name').readOnly = false;
  setElementText('channel-form-title', '添加频道');
  setElementText('channel-submit-btn', '添加频道');
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
    document.getElementById('area-id').readOnly = true;
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
  document.getElementById('area-id').readOnly = false;
  setElementText('area-form-title', '添加新分区');
  setElementText('area-submit-btn', '添加分区');
}
async function updateArea(originalId) {
  const area = readAreaForm();

  if (area.id !== originalId) {
    showNotification('分区 ID 是配置标识，编辑时不能更改；请另行添加分区', 'error');
    return;
  }

  if (!area.id || !area.name) {
    showNotification('请填写分区ID和名称', 'error');
    return;
  }

  try {
    const result = await managementJsonRequest('/api/manage/areas', 'PUT', area);
    if (result.success) {
      showManagementSuccess(result, '分区更新成功');
      clearAreaForm();
      loadAreas();
      notifyAreasJsonChanged();
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
      notifyAreasJsonChanged();
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

export {
  submitAreaForm,
  submitChannelForm,
  initManagementControls,
  createManagementMessage,
  createManagementActionButton,
  createManagementMeta,
  loadManagementListsOnce,
  loadAreas,
  addArea,
  showManagementSuccess,
  readAreaForm,
  loadChannels,
  addChannel,
  updateChannel,
  readChannelForm,
  editChannel,
  clearChannelForm,
  editArea,
  clearAreaForm,
  updateArea,
  deleteArea,
  deleteChannel,
  refreshAreas,
  refreshChannels,
  editingAreaId,
  editingChannelName,
};
