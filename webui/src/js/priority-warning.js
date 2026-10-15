import { setElementDisplay } from './dom.js';

// A local hold release, independent of the saved targets and auto-restart
// toggle. Status refresh is authoritative, including after a failed request.
export function createPriorityWarningControl({ row, button, post, refresh, notify }) {
  let channel = '';
  let pending = false;
  function render(priority) {
    channel = priority?.warning_held === true ? priority.channel_name || '' : '';
    row?.classList.toggle('hidden', !channel);
    setElementDisplay(row, !!channel, 'flex');
    if (button) button.disabled = pending || !channel;
  }
  async function release() {
    if (pending || !channel) return;
    const requestedChannel = channel;
    pending = true;
    if (button) button.disabled = true;
    try {
      const result = await post('/api/priority-channel/release-warning', { channel_name: requestedChannel });
      if (!result.success) throw new Error(result.message || '解除临时跳过失败');
      notify(result.message || '临时跳过已解除', 'success');
    } catch (error) {
      notify(error.message || '解除临时跳过失败', 'error');
    } finally {
      try { await refresh(); } finally {
        pending = false;
        if (button) button.disabled = !channel;
      }
    }
  }
  button?.addEventListener('click', release);
  render(null);
  return { render, release };
}
