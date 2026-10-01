// api.js — authenticated JSON client for every /api call.
import { bindDialog } from './dialog.js';
import { showNotification } from './dom.js';

let webUiAccessReady = false;
let webUiLoginPromise = null;
let webUiAuth = null;
const authMutations = new Set();

// Include response-body consumption in the deadline. Aborting a write cannot
// undo a server commit, so callers must refresh before retrying an uncertain write.
async function fetchWithDeadline(path, options = {}, consume = response => response) {
  const { timeoutMs = (options.method && options.method !== 'GET' ? 60000 : 10000), signal, ...request } = options;
  const controller = new AbortController();
  const abort = () => controller.abort(signal.reason);
  signal?.addEventListener('abort', abort, { once: true });
  if (signal?.aborted) abort();
  const timer = setTimeout(() => controller.abort(), timeoutMs);
  try {
    const response = await fetch(path, { ...request, signal: controller.signal });
    return await consume(response);
  } catch (error) {
    if (controller.signal.aborted && !signal?.aborted) {
      throw new Error(request.method && request.method !== 'GET'
        ? '请求超时，请刷新确认操作结果后再重试'
        : '连接超时，正在等待重连');
    }
    throw error;
  } finally {
    clearTimeout(timer);
    signal?.removeEventListener('abort', abort);
  }
}
function eventStreamUrl() {
  return '/api/events';
}
function unauthorizedApiError() {
  return '需要访问密码才能打开控制面板';
}
function showWebUiLoginGate(onSuccess) {
  let gate = document.getElementById('webui-login-gate');
  if (!gate) {
    gate = document.createElement('div');
    gate.id = 'webui-login-gate';
    gate.setAttribute('role', 'dialog');
    gate.setAttribute('aria-modal', 'true');
    gate.setAttribute('aria-labelledby', 'webui-login-title');

    const dialog = document.createElement('form');
    dialog.className = 'webui-login-dialog';
    dialog.id = 'webui-login-form';

    const title = document.createElement('h2');
    title.id = 'webui-login-title';
    title.textContent = '访问密码';

    const hint = document.createElement('p');
    hint.textContent = '请输入访问密码。';

    const label = document.createElement('label');
    label.setAttribute('for', 'webui-login-password');
    label.textContent = '密码';

    const input = document.createElement('input');
    input.id = 'webui-login-password';
    input.name = 'password';
    input.type = 'password';
    input.autocomplete = 'current-password';
    input.required = true;

    const error = document.createElement('p');
    error.id = 'webui-login-error';

    const submit = document.createElement('button');
    submit.id = 'webui-login-submit';
    submit.type = 'submit';
    submit.className = 'btn-primary';
    submit.textContent = '进入';

    dialog.append(title, hint, label, input, error, submit);
    gate.appendChild(dialog);
    document.body.appendChild(gate);
    bindDialog(gate);

    dialog.addEventListener('submit', async (event) => {
      event.preventDefault();
      error.textContent = '';
      submit.disabled = true;
      try {
        const { response, body } = await fetchWithDeadline('/api/login', {
          method: 'POST',
          headers: { 'Content-Type': 'application/json' },
          body: JSON.stringify({ password: input.value }),
          timeoutMs: 10000,
        }, async response => ({ response, body: await response.json().catch(() => null) }));
        if (!response.ok || body?.success === false) {
          error.textContent = body?.message || '密码错误';
          input.focus();
          input.select();
          return;
        }
        input.value = '';
        gate.classList.add('hidden');
        dialog._webUiLoginSuccess?.();
      } catch (loginError) {
        error.textContent = loginError.message || '登录失败';
      } finally {
        submit.disabled = false;
      }
    });
  }

  gate.classList.remove('hidden');
  const form = document.getElementById('webui-login-form');
  if (form) {
    form._webUiLoginSuccess = onSuccess;
  }
  const error = document.getElementById('webui-login-error');
  if (error) {
    error.textContent = '';
  }
  document.getElementById('webui-login-password')?.focus();
}
function promptWebUiLogin() {
  if (webUiLoginPromise) {
    return webUiLoginPromise;
  }
  webUiLoginPromise = new Promise((resolve) => {
    showWebUiLoginGate(() => resolve());
  }).finally(() => {
    webUiLoginPromise = null;
  });
  return webUiLoginPromise;
}
async function refreshWebUiAuth() {
  const auth = await fetchWithDeadline('/api/auth', {}, readJsonApiResponse);
  if (typeof auth.required !== 'boolean' || typeof auth.authenticated !== 'boolean') throw new Error('无法读取密码状态');
  webUiAuth = auth;
  webUiAccessReady = !auth.required || auth.authenticated;
  const logout = document.getElementById('webui-logout');
  if (logout) {
    logout.classList.toggle('hidden', !auth.required);
    logout.onclick = async () => {
      logout.disabled = true;
      try {
        const response = await fetchWithDeadline('/api/logout', { method: 'POST' });
        if (!response.ok) throw new Error('退出登录失败，请重试');
        window.location.reload();
      } catch (error) {
        showNotification(error.message, 'error');
        logout.disabled = false;
      }
    };
  }
  document.dispatchEvent(new CustomEvent('webui-auth-changed', { detail: auth }));
  return auth;
}
function getWebUiAuth() { return webUiAuth; }
async function ensureWebUiAccess({ strict = false } = {}) {
  try {
    const auth = await refreshWebUiAuth();
    if (auth.required && !auth.authenticated) {
      await promptWebUiLogin();
      return await refreshWebUiAuth();
    }
    return auth;
  } catch (error) {
    if (strict) throw error;
    // Old servers without /api/auth should still load the panel.
    webUiAccessReady = true;
  }
}

function isWebUiAccessReady() {
  return webUiAccessReady;
}
async function fetchWithWebUiAuth(path, options = {}, consume = response => response) {
  const { replayAfterLogin = true, ...request } = options;
  let unauthorized = false;
  const result = await fetchWithDeadline(path, request, response => {
    if (response.status === 401) {
      unauthorized = true;
      return null;
    }
    return consume(response);
  });
  if (!unauthorized) {
    return result;
  }
  webUiAccessReady = false;
  if (!replayAfterLogin) {
    throw Object.assign(new Error('登录已失效，请登录后确认状态，再重新提交'), { status: 401 });
  }
  // Another request may already have created a session or cleared the password.
  await ensureWebUiAccess({ strict: true });
  return fetchWithDeadline(path, request, consume);
}
async function readManagementResponse(response) {
  if (response.status === 401) {
    throw new Error(unauthorizedApiError());
  }
  const contentType = response.headers.get('content-type') || '';
  const bodyText = await response.text();
  if (!contentType.includes('application/json')) {
    throw new Error(`Expected JSON, got: ${contentType || 'unknown'}. Response: ${bodyText}`);
  }

  let result = null;
  try {
    result = bodyText ? JSON.parse(bodyText) : null;
  } catch (error) {
    throw new Error(response.ok ? '服务器返回了无效 JSON' : formatHttpError(response, bodyText));
  }

  if (!response.ok) {
    throw Object.assign(new Error(result?.message || formatHttpError(response, bodyText)), { status: response.status });
  }
  if (!result) {
    throw new Error('服务器返回空响应');
  }
  return result;
}
async function managementRequest(path, options = {}) {
  return fetchWithWebUiAuth(path, options, readManagementResponse);
}
function managementJsonRequest(path, method, payload) {
  return managementRequest(path, {
    method,
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify(payload)
  });
}
function deleteManagementResource(path) {
  return managementRequest(path, { method: 'DELETE' });
}
function formatHttpError(response, bodyText = '') {
  if (response.status === 409) return '配置已被其他操作修改，请重新加载后再保存';
  const trimmed = bodyText.trim();
  if (trimmed) {
    return trimmed.length > 200 ? `${trimmed.slice(0, 200)}...` : trimmed;
  }
  return `HTTP ${response.status}: ${response.statusText}`;
}
async function readJsonApiResponse(response) {
  if (response.status === 401) {
    throw Object.assign(new Error(unauthorizedApiError()), { status: 401 });
  }
  const bodyText = await response.text();
  let result = null;
  if (bodyText) {
    try {
      result = JSON.parse(bodyText);
    } catch (error) {
      if (response.ok) {
        throw new Error('服务器返回了无效 JSON');
      }
      throw new Error(formatHttpError(response, bodyText));
    }
  }

  if (!response.ok) {
    throw Object.assign(new Error(result?.message || formatHttpError(response, bodyText)), { status: response.status });
  }
  if (!result) {
    throw new Error('服务器返回空响应');
  }
  return result;
}
async function jsonRequest(method, path, payload, options = {}) {
  const request = {
    ...options,
    method,
    headers: { 'Content-Type': 'application/json' }
  };
  if (payload !== undefined) {
    request.body = JSON.stringify(payload);
  }

  const pending = fetchWithWebUiAuth(path, request, readJsonApiResponse);
  if (options.replayAfterLogin !== false) return pending;
  authMutations.add(pending);
  try { return await pending; }
  finally { authMutations.delete(pending); }
}

async function settleWebUiAuthMutations() {
  await Promise.allSettled([...authMutations]);
}

async function getJson(path, options = {}) {
  return fetchWithWebUiAuth(path, options, readJsonApiResponse);
}

function postJson(path, payload, options = {}) {
  return jsonRequest('POST', path, payload, options);
}

function postJsonApi(path, payload, options = {}) {
  return postJson(path, payload, options);
}

function putJson(path, payload) {
  return jsonRequest('PUT', path, payload);
}

function deleteJson(path, payload) {
  return jsonRequest('DELETE', path, payload);
}

export {
  eventStreamUrl,
  unauthorizedApiError,
  showWebUiLoginGate,
  promptWebUiLogin,
  ensureWebUiAccess,
  refreshWebUiAuth,
  getWebUiAuth,
  settleWebUiAuthMutations,
  isWebUiAccessReady,
  fetchWithWebUiAuth,
  readManagementResponse,
  managementRequest,
  managementJsonRequest,
  deleteManagementResource,
  formatHttpError,
  readJsonApiResponse,
  jsonRequest,
  getJson,
  postJson,
  postJsonApi,
  putJson,
  deleteJson,
};
