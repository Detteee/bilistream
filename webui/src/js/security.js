// Panel credentials have their own save flow; never reload other Settings drafts.
import { ensureWebUiAccess, getWebUiAuth, postJsonApi } from './api.js';

let saving = false;
let needsRefresh = false;
const element = id => document.getElementById(id);

function renderPanelSecurity(auth = getWebUiAuth()) {
  const configured = !!auth?.required;
  element('panel-password-status').textContent = !auth ? '正在读取密码状态…'
    : configured ? '已设置面板密码'
    : auth.can_create_password ? '未设置密码，仅供本机访问'
    : '请在服务器本机直接打开面板以设置密码';
  element('panel-password-current-group').classList.toggle('hidden', !configured);
  element('panel-password-clear').classList.toggle('hidden', !configured);
  element('panel-password-clear').disabled = saving || needsRefresh || !auth?.can_clear_password;
  element('panel-password-clear-hint').classList.toggle('hidden', !configured || !!auth.can_clear_password);
  const save = element('panel-password-save');
  save.textContent = needsRefresh ? '重新检查密码状态' : configured ? '更改密码' : '设置密码';
  save.disabled = saving || (!needsRefresh && !(configured || auth?.can_create_password));
  element('panel-password-new').disabled = saving || needsRefresh || !(configured || auth?.can_create_password);
  element('panel-password-current').disabled = saving || needsRefresh;
}

function clearPasswordInputs() {
  element('panel-password-current').value = '';
  element('panel-password-new').value = '';
}

async function reconcilePasswordState() {
  await ensureWebUiAccess({ strict: true });
  needsRefresh = false;
}

async function savePanelPassword(clear = false) {
  if (saving) return;
  const error = element('panel-password-error');
  error.textContent = '';
  if (needsRefresh) {
    saving = true;
    renderPanelSecurity();
    try {
      await reconcilePasswordState();
      error.textContent = '已刷新状态，请确认后重新填写并提交。';
    } catch {
      error.textContent = '无法确认密码状态，请恢复连接后重新检查。';
    } finally { saving = false; renderPanelSecurity(); }
    return;
  }
  const auth = getWebUiAuth();
  const current = element('panel-password-current');
  const next = element('panel-password-new');
  if (auth?.required && !current.value.trim()) {
    error.textContent = '请填写当前密码'; current.focus(); return;
  }
  if (!clear && !next.value.trim()) {
    error.textContent = '请填写新密码'; next.focus(); return;
  }
  if (clear && !auth?.can_clear_password) return;
  if (!auth?.required && !auth?.can_create_password) return;
  const payload = clear ? { action: 'clear', current_password: current.value }
    : auth.required ? { action: 'change', current_password: current.value, new_password: next.value }
    : { action: 'create', new_password: next.value };
  saving = true;
  renderPanelSecurity();
  try {
    const result = await postJsonApi('/api/auth/password', payload, { replayAfterLogin: false });
    if (!result.success) { error.textContent = result.message || '密码保存失败'; return; }
    clearPasswordInputs();
    needsRefresh = true;
    await reconcilePasswordState();
    error.textContent = clear ? '密码已清除，之前的登录已失效。' : '密码已保存，之前的登录已失效。';
  } catch (failure) {
    error.textContent = failure.message || '无法确认密码是否已保存';
    if (!failure.status || failure.status === 401 || failure.status === 409 || failure.status >= 500) {
      clearPasswordInputs();
      needsRefresh = true;
      try {
        await reconcilePasswordState();
        error.textContent = '已刷新密码状态，请确认后重新填写并提交。';
      } catch {
        error.textContent = '无法确认密码状态，请恢复连接后重新检查。';
      }
    }
  } finally {
    saving = false;
    renderPanelSecurity();
  }
}

function initPanelSecurity() {
  element('panel-password-save')?.addEventListener('click', () => savePanelPassword());
  element('panel-password-clear')?.addEventListener('click', () => savePanelPassword(true));
  document.addEventListener('webui-auth-changed', event => renderPanelSecurity(event.detail));
  renderPanelSecurity();
}

export { initPanelSecurity };
