import { state } from './state.js';
import { showNotification } from './dom.js';

// One writer per switch. Repeated clicks replace the queued intent; status
// polling must not repaint a switch while its write is pending.
export function saveBooleanToggle(id, confirmed, save, onSaved) {
  const toggle = document.getElementById(id);
  if (!toggle) return Promise.resolve();
  let entry = state.monitorToggleSaveState.get(id);
  if (!entry) {
    entry = { confirmed, desired: confirmed, inFlight: false };
    state.monitorToggleSaveState.set(id, entry);
  }
  entry.desired = toggle.checked;
  if (entry.inFlight) return entry.promise;
  entry.inFlight = true;
  toggle.dataset.saving = 'true';
  entry.promise = (async () => {
    let submitted;
    do {
      submitted = entry.desired;
      try {
        const result = await save(submitted);
        if (!result.success) throw new Error(result.message || '保存失败');
        entry.confirmed = submitted;
        onSaved?.(submitted);
        if (entry.desired === submitted && result.message) showNotification(result.message, 'success');
      } catch (error) {
        showNotification(`保存失败: ${error.message}`, 'error');
        if (entry.desired === submitted) {
          entry.desired = entry.confirmed;
          break;
        }
      }
    } while (entry.desired !== submitted);
  })().finally(() => {
    entry.inFlight = false;
    delete toggle.dataset.saving;
    toggle.checked = entry.confirmed;
    state.hooks.refreshStatus?.();
  });
  return entry.promise;
}
