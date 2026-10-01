import test from 'node:test';
import assert from 'node:assert/strict';

// These tests exercise the request boundary without opening a login dialog.
globalThis.document = { addEventListener() {}, getElementById() { return null; }, dispatchEvent() {} };
const { getJson, postJsonApi, settleWebUiAuthMutations } = await import('../src/js/api.js');

test('sensitive mutations return expiry without a login prompt or replay', async () => {
  const original = globalThis.fetch;
  const requests = [];
  globalThis.fetch = async (path, options) => {
    requests.push({ path, options });
    return new Response(JSON.stringify({ success: false }), { status: 401 });
  };
  try {
    for (const path of ['/api/auth/password', '/api/setup/save-config']) {
      await assert.rejects(postJsonApi(path, { new_password: 'synthetic' }, { replayAfterLogin: false }), error => error.status === 401);
    }
    assert.equal(requests.length, 2);
    assert.ok(requests.every(request => !('replayAfterLogin' in request.options)));
  } finally { globalThis.fetch = original; }
});

test('wrong current password remains a 403 operator error', async () => {
  const original = globalThis.fetch;
  let requests = 0;
  globalThis.fetch = async () => {
    requests++;
    return new Response(JSON.stringify({ success: false, message: '当前密码错误' }), { status: 403 });
  };
  try {
    await assert.rejects(postJsonApi('/api/auth/password', { action: 'clear', current_password: 'synthetic' }, { replayAfterLogin: false }), error => error.status === 403 && error.message === '当前密码错误');
    assert.equal(requests, 1);
  } finally { globalThis.fetch = original; }
});

test('auth recheck waits for the wizard response carrying its session cookie', async () => {
  const original = globalThis.fetch;
  let respond;
  globalThis.fetch = () => new Promise(resolve => { respond = resolve; });
  try {
    const save = postJsonApi('/api/setup/save-config', { panel_password: 'synthetic' }, { replayAfterLogin: false });
    let settled = false;
    const recheck = settleWebUiAuthMutations().then(() => { settled = true; });
    await new Promise(resolve => setTimeout(resolve, 0));
    assert.equal(settled, false);
    respond(new Response(JSON.stringify({ success: true })));
    await save;
    await recheck;
    assert.equal(settled, true);
  } finally { globalThis.fetch = original; }
});

test('a stale read after password clear refreshes authority without asking for a password', async () => {
  const original = globalThis.fetch;
  const requests = [];
  globalThis.fetch = async path => {
    requests.push(path);
    if (requests.length === 1) return new Response('{}', { status: 401 });
    if (path === '/api/auth') return Response.json({ required: false, authenticated: true, can_create_password: true, can_clear_password: false });
    return Response.json({ interval: 60 });
  };
  try {
    assert.deepEqual(await getJson('/api/config'), { interval: 60 });
    assert.deepEqual(requests, ['/api/config', '/api/auth', '/api/config']);
  } finally { globalThis.fetch = original; }
});
