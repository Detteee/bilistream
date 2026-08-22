// api.js — authenticated JSON client for every /api call.

let webUiAccessReady = false;
let webUiLoginPromise = null;
function eventStreamUrl() {
  return '/api/events';
}
function unauthorizedApiError() {
  return '需要访问密码才能打开控制面板';
}
function ensureWebUiLoginStyles() {
  if (document.getElementById('webui-login-style')) {
    return;
  }
  const style = document.createElement('style');
  style.id = 'webui-login-style';
  style.textContent = `
    #webui-login-gate {
      position: fixed;
      inset: 0;
      z-index: 400;
      display: flex;
      align-items: center;
      justify-content: center;
      padding: 24px;
      background: rgba(4, 4, 10, 0.72);
      backdrop-filter: blur(6px);
      -webkit-backdrop-filter: blur(6px);
    }
    #webui-login-gate.hidden { display: none !important; }
    #webui-login-gate .webui-login-dialog {
      width: min(420px, 100%);
      padding: 28px;
      border: 1px solid var(--line, #2a2a38);
      border-radius: 16px;
      background: var(--surface-1, #16161f);
      color: var(--text, #f3f3f7);
      box-shadow: 0 18px 48px rgba(0, 0, 0, 0.35);
    }
    #webui-login-gate h2 {
      margin: 0 0 8px;
      font-size: 1.25rem;
    }
    #webui-login-gate p {
      margin: 0 0 16px;
      color: var(--text-mute, #9a9aab);
      font-size: 0.95rem;
    }
    #webui-login-gate label {
      display: block;
      margin-bottom: 6px;
      font-size: 0.9rem;
    }
    #webui-login-password {
      width: 100%;
      margin-bottom: 12px;
      padding: 10px 12px;
      border: 1px solid var(--line, #2a2a38);
      border-radius: 10px;
      background: var(--surface-2, #1e1e2a);
      color: inherit;
    }
    #webui-login-error {
      min-height: 1.2em;
      margin: 0 0 12px;
      color: #f07178;
      font-size: 0.9rem;
    }
    #webui-login-submit {
      width: 100%;
    }
  `;
  document.head.appendChild(style);
}
function showWebUiLoginGate(onSuccess) {
  ensureWebUiLoginStyles();
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

    dialog.addEventListener('submit', async (event) => {
      event.preventDefault();
      error.textContent = '';
      submit.disabled = true;
      try {
        const response = await fetch('/api/login', {
          method: 'POST',
          headers: { 'Content-Type': 'application/json' },
          body: JSON.stringify({ password: input.value })
        });
        const body = await response.json().catch(() => null);
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
async function ensureWebUiAccess() {
  try {
    const response = await fetch('/api/auth');
    const auth = await response.json();
    if (auth.required && !auth.authenticated) {
      await promptWebUiLogin();
    }
  } catch (error) {
    // Old servers without /api/auth should still load the panel.
  }
  webUiAccessReady = true;
}

function isWebUiAccessReady() {
  return webUiAccessReady;
}
async function fetchWithWebUiAuth(path, options = {}) {
  const response = await fetch(path, options);
  if (response.status !== 401) {
    return response;
  }
  await promptWebUiLogin();
  return fetch(path, options);
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
    throw new Error(result?.message || formatHttpError(response, bodyText));
  }
  if (!result) {
    throw new Error('服务器返回空响应');
  }
  return result;
}
async function managementRequest(path, options = {}) {
  const response = await fetchWithWebUiAuth(path, options);
  return readManagementResponse(response);
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
  const trimmed = bodyText.trim();
  if (trimmed) {
    return trimmed.length > 200 ? `${trimmed.slice(0, 200)}...` : trimmed;
  }
  return `HTTP ${response.status}: ${response.statusText}`;
}
async function readJsonApiResponse(response) {
  if (response.status === 401) {
    throw new Error(unauthorizedApiError());
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
    throw new Error(result?.message || formatHttpError(response, bodyText));
  }
  if (!result) {
    throw new Error('服务器返回空响应');
  }
  return result;
}
async function jsonRequest(method, path, payload) {
  const request = {
    method,
    headers: { 'Content-Type': 'application/json' }
  };
  if (payload !== undefined) {
    request.body = JSON.stringify(payload);
  }

  const response = await fetchWithWebUiAuth(path, request);
  return readJsonApiResponse(response);
}

async function getJson(path) {
  const response = await fetchWithWebUiAuth(path);
  return readJsonApiResponse(response);
}

function postJson(path, payload) {
  return jsonRequest('POST', path, payload);
}

function postJsonApi(path, payload) {
  return postJson(path, payload);
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
  ensureWebUiLoginStyles,
  showWebUiLoginGate,
  promptWebUiLogin,
  ensureWebUiAccess,
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
