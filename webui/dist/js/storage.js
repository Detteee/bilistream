import { getJson, postJsonApi, deleteJson, fetchWithWebUiAuth } from './api.js';
import { showNotification } from './dom.js';

let cookieRevision = null;
let filterRevision = null;
let filterBaseline = null;
let generation = 0;
let cookieBusy = false;

export async function loadManagedStorage() {
  const current = ++generation;
  const results = await Promise.allSettled([
    getJson('/api/youtube/cookies'), getJson('/api/player-filter'), getJson('/api/storage')
  ]);
  if (current !== generation) return;
  const [cookies, filter, storage] = results;
  if (cookies.status === 'fulfilled' && cookies.value.success) {
    const data = cookies.value.data;
    cookieRevision = data.revision;
    document.getElementById('yt-cookie-status').textContent = data.configured
      ? `已加密保存 ${data.count} 条 Cookie` : '尚未保存 Cookie';
  } else {
    cookieRevision = null;
    document.getElementById('yt-cookie-status').textContent = 'Cookie 状态暂不可用';
  }
  if (filter.status === 'fulfilled' && filter.value.success) {
    const input = document.getElementById('config-player-filter');
    if (document.activeElement !== input && (filterBaseline === null || input.value === filterBaseline)) {
      input.value = filter.value.data.content;
      filterBaseline = input.value;
      filterRevision = filter.value.data.revision;
    }
  }
  if (storage.status === 'fulfilled') {
    document.getElementById('storage-state').textContent = storage.value.ready
      ? '配置与登录信息已加密保存，重启自动解锁。' : storage.value.message;
  }
}

async function cookieAction(clear = false) {
  if (cookieBusy) return;
  if (cookieRevision === null) { showNotification('请先重新加载 Cookie 状态', 'error'); return; }
  cookieBusy = true;
  const buttons = ['yt-cookie-save', 'yt-cookie-clear'].map(id => document.getElementById(id));
  buttons.forEach(button => { button.disabled = true; });
  try {
    const input = document.getElementById('yt-cookie-paste');
    const fileInput = document.getElementById('yt-cookie-upload');
    const file = fileInput.files[0];
    if (file && file.size > 1024 * 1024) throw new Error('Cookie 文件不能超过 1 MB');
    const content = clear ? '' : (file ? await file.text() : input.value);
    const result = clear
      ? await deleteJson('/api/youtube/cookies', { expected_revision: cookieRevision })
      : await postJsonApi('/api/youtube/cookies', { content, expected_revision: cookieRevision });
    if (!result.success) throw new Error(result.message);
    if (input.value === content || file) input.value = '';
    if (fileInput.files[0] === file) fileInput.value = '';
    showNotification(result.message, 'success');
  } catch (error) {
    showNotification(error.message, 'error');
  } finally {
    cookieBusy = false;
    buttons.forEach(button => { button.disabled = false; });
    await loadManagedStorage();
  }
}

async function savePlayerFilter() {
  if (filterRevision === null) { showNotification('请先重新加载过滤词', 'error'); return; }
  const button = document.getElementById('save-player-filter');
  button.disabled = true;
  try {
    const content = document.getElementById('config-player-filter').value;
    const result = await postJsonApi('/api/player-filter', { content, expected_revision: filterRevision });
    if (!result.success) throw new Error(result.message);
    filterRevision = result.data;
    filterBaseline = content;
    showNotification(result.message, 'success');
  } catch (error) { showNotification(error.message, 'error'); }
  finally { button.disabled = false; }
}

async function exportBackup() {
  const input = document.getElementById('storage-backup-password');
  const button = document.getElementById('storage-backup');
  button.disabled = true;
  try {
    const password = input.value;
    if ([...password].length < 12) throw new Error('备份密码至少需要 12 个字符');
    const blob = await fetchWithWebUiAuth('/api/storage/backup', {
      method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ password })
    }, async response => {
      if (!response.ok) { const data = await response.json(); throw new Error(data.message || '备份失败'); }
      return response.blob();
    });
    const url = URL.createObjectURL(blob);
    const link = document.createElement('a');
    link.href = url; link.download = 'bilistream.backup'; link.click();
    setTimeout(() => URL.revokeObjectURL(url), 1000);
    if (input.value === password) input.value = '';
    showNotification('加密备份已下载，请妥善保管密码', 'success');
  } catch (error) { showNotification(error.message, 'error'); }
  finally { button.disabled = false; }
}

export async function restoreBackup(prefix = 'storage') {
  const input = document.getElementById(`${prefix}-restore-password`);
  const fileInput = document.getElementById(`${prefix}-restore-file`);
  const button = document.getElementById(`${prefix}-restore`);
  button.disabled = true;
  try {
    const file = fileInput.files[0];
    if (!file || file.size > 32 * 1024 * 1024) throw new Error('请选择不超过 32 MB 的加密备份');
    const bytes = new Uint8Array(await file.arrayBuffer());
    const chunks = [];
    for (let index = 0; index < bytes.length; index += 8192) {
      chunks.push(String.fromCharCode(...bytes.subarray(index, index + 8192)));
    }
    const result = await postJsonApi('/api/storage/restore', { password: input.value, backup_base64: btoa(chunks.join('')) });
    if (!result.success) throw new Error(result.message);
    input.value = ''; fileInput.value = '';
    showNotification('备份已恢复', 'success');
    window.location.reload();
  } catch (error) { showNotification(error.message, 'error'); }
  finally { button.disabled = false; }
}

export function initStorageControls() {
  document.getElementById('yt-cookie-save')?.addEventListener('click', () => cookieAction());
  document.getElementById('yt-cookie-clear')?.addEventListener('click', () => cookieAction(true));
  document.getElementById('save-player-filter')?.addEventListener('click', savePlayerFilter);
  document.getElementById('storage-backup')?.addEventListener('click', exportBackup);
  document.getElementById('storage-restore')?.addEventListener('click', () => restoreBackup());
  document.getElementById('setup-restore')?.addEventListener('click', () => restoreBackup('setup'));
}
