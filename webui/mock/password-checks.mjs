import assert from 'node:assert/strict';
import { join } from 'node:path';

// Operate only compiled-page DOM controls against synthetic installation state.
export async function checkPanelPasswords({ base, context, command, evaluate, waitFor, capture, captureDir }) {
  const mode = value => fetch(`${base}/mock/password-mode?mode=${value}`);
  const reload = () => command('browsingContext.reload', { context, wait: 'complete' });
  const stats = async () => (await fetch(`${base}/mock/auth-stats`)).json();
  const loginVisible = "!!document.getElementById('webui-login-gate')?.checkVisibility()";
  const login = async password => {
    await waitFor(loginVisible);
    await evaluate(`document.getElementById('webui-login-password').value=${JSON.stringify(password)}; document.getElementById('webui-login-submit').click()`);
    await waitFor(`!(${loginVisible})`);
  };
  const settings = async () => {
    await evaluate("document.getElementById('tab-settings').click()");
    await waitFor("document.getElementById('panel-password-save').checkVisibility() && !document.getElementById('panel-password-save').disabled");
  };
  const passwordValues = () => evaluate("document.getElementById('panel-password-current').value + document.getElementById('panel-password-new').value");
  await command('browsingContext.setViewport', { context, viewport: { width: 1440, height: 1100 }, devicePixelRatio: 1 });
  await settings();
  await evaluate("document.getElementById('config-interval').value='87'; document.getElementById('panel-password-new').value='synthetic-panel-password'; document.getElementById('panel-password-save').click()");
  await waitFor(loginVisible);
  assert.equal(await passwordValues(), '', 'creation clears password inputs before login');
  await login('synthetic-panel-password');
  await waitFor("!document.getElementById('panel-password-save').disabled");
  assert.equal(await evaluate("document.getElementById('config-interval').value"), '87', 'create preserves unrelated drafts');
  assert.equal(await evaluate("document.getElementById('webui-logout').classList.contains('hidden')"), false);

  const beforeWrong = await stats();
  await evaluate("document.getElementById('panel-password-current').value='wrong'; document.getElementById('panel-password-new').value='synthetic-next-password'; document.getElementById('panel-password-save').click()");
  await waitFor("document.getElementById('panel-password-error').textContent.includes('当前密码错误') && !document.getElementById('panel-password-save').disabled");
  assert.equal(await evaluate(loginVisible), false, 'wrong current password stays inline');
  assert.equal((await stats()).passwordAttempts, beforeWrong.passwordAttempts + 1, 'wrong password not replayed');

  await evaluate("document.getElementById('panel-password-current').value='synthetic-panel-password'; document.getElementById('panel-password-save').click()");
  await login('synthetic-next-password');
  await waitFor("!document.getElementById('panel-password-save').disabled");
  assert.equal(await passwordValues(), '');
  assert.equal(await evaluate("document.getElementById('config-interval').value"), '87', 'change preserves unrelated drafts');

  // A lost session may prompt login, but must never repeat the pending write.
  const beforeExpired = await stats();
  await evaluate(`(async () => { await fetch('${base}/mock/password-mode?mode=expire'); document.getElementById('panel-password-current').value='synthetic-next-password'; document.getElementById('panel-password-new').value='should-not-be-saved'; document.getElementById('panel-password-save').click(); })()`);
  await login('synthetic-next-password');
  await waitFor("!document.getElementById('panel-password-save').disabled");
  assert.equal((await stats()).passwordAttempts, beforeExpired.passwordAttempts + 1, 'expired mutation not replayed after login');
  assert.equal(await passwordValues(), '');
  assert.equal(await evaluate("document.getElementById('config-interval').value"), '87');

  await mode('remote'); await reload(); await settings();
  assert.equal(await evaluate("document.getElementById('panel-password-clear').disabled"), true, 'remote listener cannot clear');
  assert.equal(await evaluate("document.getElementById('panel-password-clear-hint').checkVisibility()"), true);
  await mode('local'); await reload(); await settings();
  await evaluate("document.getElementById('config-interval').value='89'; document.getElementById('panel-password-current').value='wrong'; document.getElementById('panel-password-clear').click()");
  await waitFor("document.getElementById('panel-password-error').textContent.includes('当前密码错误') && !document.getElementById('panel-password-clear').disabled");
  await evaluate("document.getElementById('panel-password-current').value='synthetic-next-password'; document.getElementById('panel-password-clear').click()");
  await waitFor("document.getElementById('panel-password-status').textContent.includes('未设置') && !document.getElementById('panel-password-save').disabled");
  assert.equal(await passwordValues(), '');
  assert.equal(await evaluate("document.getElementById('webui-logout').classList.contains('hidden')"), true);
  assert.equal(await evaluate("document.getElementById('config-interval').value"), '89', 'clear preserves unrelated drafts');
  assert.equal(await evaluate("!!document.querySelector('[id*=reset-password], [id*=password-reset]')"), false, 'no browser reset');
  await evaluate("document.getElementById('panel-security-title').closest('section').scrollIntoView({block:'center'})");
  await capture(join(captureDir, 'panel-password-settings.png'));
  for (const width of [768, 390, 320]) {
    await command('browsingContext.setViewport', { context, viewport: { width, height: 900 }, devicePixelRatio: 1 });
    assert.equal(await evaluate('document.documentElement.scrollWidth <= innerWidth'), true, `security overflow at ${width}`);
  }

  const setupStep = async () => {
    await waitFor("document.getElementById('setup-page').classList.contains('active')");
    await evaluate("document.getElementById('setup-step-1-next-btn').click(); document.getElementById('setup-room').value='10000'; document.getElementById('setup-step-2-next-btn').click()");
  };
  const startSetup = async () => { await fetch(`${base}/mock/setup-mode`); await reload(); };
  const lastSetup = async () => (await (await fetch(`${base}/mock/writes`)).json()).filter(row => row.path === '/api/setup/save-config').at(-1).patch;
  await mode('missing'); await startSetup(); await setupStep();
  assert.equal(await evaluate("document.getElementById('setup-panel-password-enabled').checked"), false);
  await evaluate("document.getElementById('setup-save-btn').click()");
  await waitFor("document.getElementById('main-page').checkVisibility() && !document.getElementById('setup-page').classList.contains('active')");
  assert.equal('panel_password' in await lastSetup(), false, 'skip omits password');

  await startSetup(); await setupStep();
  await evaluate("document.getElementById('setup-panel-password-enabled').click(); document.getElementById('setup-save-btn').click()");
  await waitFor("document.getElementById('setup-panel-password-error').textContent.includes('请填写')");
  await evaluate("document.getElementById('setup-panel-password').value='synthetic-wizard-password'; document.getElementById('setup-panel-password').dispatchEvent(new Event('input'))");
  await evaluate("document.getElementById('setup-security-title').scrollIntoView({block:'start'})");
  await capture(join(captureDir, 'panel-password-wizard.png'));
  const beforeSetup = await stats();
  await evaluate("document.getElementById('setup-save-btn').click()");
  await waitFor("document.getElementById('main-page').checkVisibility() && !document.getElementById('setup-page').classList.contains('active')");
  assert.equal((await stats()).setupAttempts, beforeSetup.setupAttempts + 1);
  assert.equal(await evaluate(loginVisible), false, 'wizard cookie retains access');
  assert.equal((await lastSetup()).panel_password, 'synthetic-wizard-password');

  await startSetup(); await setupStep();
  assert.equal(await evaluate("document.getElementById('setup-panel-password-status').textContent.includes('已设置')"), true);
  assert.equal(await evaluate("document.getElementById('setup-panel-password-choice').checkVisibility()"), false, 'configured wizard has no skip/replace toggle');
  await evaluate("document.getElementById('setup-save-btn').click()");
  await waitFor("document.getElementById('main-page').checkVisibility() && !document.getElementById('setup-page').classList.contains('active')");
  assert.equal('panel_password' in await lastSetup(), false);

  // The server commits but the response/cookie is lost: discover and sign in.
  await mode('missing'); await startSetup(); await setupStep();
  await mode('lose-setup-response');
  const beforeLost = await stats();
  await evaluate("document.getElementById('setup-panel-password-enabled').click(); document.getElementById('setup-panel-password').value='synthetic-recovered-password'; document.getElementById('setup-save-btn').click()");
  await login('synthetic-recovered-password');
  await waitFor("document.getElementById('main-page').checkVisibility() && !document.getElementById('setup-page').classList.contains('active')");
  assert.equal((await stats()).setupAttempts, beforeLost.setupAttempts + 1, 'lost setup response not replayed');
  assert.equal(await evaluate("JSON.stringify(localStorage).includes('synthetic-recovered-password') || JSON.stringify(sessionStorage).includes('synthetic-recovered-password')"), false);
  console.log('Compiled password checks passed: wizard skip/set/configured/lost-response, settings create/change/clear/wrong-current/expired-session, remote clear disabled, drafts retained, no reset, desktop/mobile layout.');
}
