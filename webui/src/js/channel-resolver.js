import { postJsonApi } from './api.js';
import { showNotification } from './dom.js';

// An explicit action avoids fetching on every keystroke. Saving also resolves
// on the server, so pasting a URL and immediately saving is supported.
export function bindYoutubeResolver(inputId, buttonId) {
  const input = document.getElementById(inputId);
  const button = document.getElementById(buttonId);
  if (!input || !button) return;
  button.addEventListener('click', async () => {
    const original = input.value.trim();
    if (!original) { showNotification('请先填写 YouTube 频道主页、@handle 或 UC ID', 'error'); return; }
    button.disabled = true;
    const label = button.textContent; button.textContent = '识别中…';
    try {
      const result = await postJsonApi('/api/channels/resolve-youtube', { input: original });
      if (!result.success || !result.data?.channel_id) throw new Error(result.message || '未找到频道 ID');
      if (input.value.trim() !== original) return; // do not overwrite a newer edit
      input.value = result.data.channel_id;
      showNotification('已识别 UC 频道 ID，请确认后保存', 'success');
    } catch (error) { showNotification(error.message, 'error'); }
    finally { button.disabled = false; button.textContent = label; }
  });
}
