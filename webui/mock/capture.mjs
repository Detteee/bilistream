// Node 22+ and Firefox. Uses WebDriver BiDi; no automation package required.
import { spawn } from 'node:child_process';
import { mkdtemp, mkdir, writeFile, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import assert from 'node:assert/strict';
import { createMockServer } from './server.mjs';

const server = createMockServer();
await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
const base = `http://127.0.0.1:${server.address().port}`;
const profile = await mkdtemp(join(tmpdir(), 'bilistream-mock-'));
const browser = spawn(process.env.FIREFOX || 'firefox', ['--headless', '--no-remote', '--profile', profile, '--remote-debugging-port', '0']);
const root = fileURLToPath(new URL('../../', import.meta.url));
let ws;
try {
  const address = await new Promise((resolve, reject) => {
    const timer = setTimeout(() => reject(new Error('Firefox startup timed out')), 20000);
    const read = data => { const match = data.toString().match(/WebDriver BiDi listening on (ws:\/\/[^\s]+)/); if (match) { clearTimeout(timer); resolve(match[1]); } };
    browser.stdout.on('data', read); browser.stderr.on('data', read);
    browser.once('error', reject);
    browser.once('exit', code => { clearTimeout(timer); reject(new Error(`Firefox exited ${code}`)); });
  });
  ws = new WebSocket(`${address}/session`);
  await new Promise((resolve, reject) => { ws.onopen = resolve; ws.onerror = reject; });
  let seq = 0;
  const pending = new Map();
  ws.onmessage = ({ data }) => {
    const message = JSON.parse(data); const call = pending.get(message.id);
    if (call) { pending.delete(message.id); clearTimeout(call.timer); message.type === 'error' ? call.reject(new Error(message.message)) : call.resolve(message.result); }
  };
  const command = (method, params) => new Promise((resolve, reject) => {
    const id = ++seq;
    const timer = setTimeout(() => { pending.delete(id); reject(new Error(`BiDi timeout: ${method}`)); }, 20000);
    pending.set(id, { resolve, reject, timer }); ws.send(JSON.stringify({ id, method, params }));
  });
  await command('session.new', { capabilities: {} });
  const { context } = await command('browsingContext.create', { type: 'tab' });
  const evaluate = async expression => {
    const result = await command('script.evaluate', { expression, target: { context }, awaitPromise: true });
    if (result.type === 'exception') throw new Error(result.exceptionDetails.text);
    return result.result?.value;
  };
  const waitFor = async expression => {
    for (let i = 0; i < 100; i++) { if (await evaluate(expression)) return; await new Promise(resolve => setTimeout(resolve, 100)); }
    throw new Error(`UI condition timed out: ${expression}`);
  };
  await command('browsingContext.setViewport', { context, viewport: { width: 1440, height: 1320 }, devicePixelRatio: 1 });
  await command('browsingContext.navigate', { context, url: base, wait: 'complete' });
  await waitFor("document.documentElement.classList.contains('app-ready') && document.getElementById('yt-title').textContent.includes('Demo')");
  await evaluate("document.getElementById('theme-toggle').click(); document.fonts.ready");
  await new Promise(resolve => setTimeout(resolve, 900));
  await waitFor("document.querySelectorAll('.holodex-stream-card').length > 0");
  const capture = async path => {
    // Let view transitions and layout animations finish before documenting UI.
    await new Promise(resolve => setTimeout(resolve, 650));
    const { data } = await command('browsingContext.captureScreenshot', { context, origin: 'viewport' });
    await writeFile(path, Buffer.from(data, 'base64'));
  };
  assert.equal(await evaluate("document.querySelector('.card[data-platform=\"cluster\"]').classList.contains('hidden')"), true);
  assert.equal(await evaluate("getComputedStyle(document.querySelector('.dashboard')).gridTemplateColumns.split(' ').length"), 3);
  await capture(join(root, 'screenshot_of_webui.png'));
  await command('browsingContext.setViewport', { context, viewport: { width: 1440, height: 1100 }, devicePixelRatio: 1 });
  await evaluate("document.getElementById('tab-settings').click()");
  await waitFor("document.getElementById('config-websub-callback-url').value.includes('example.com')");
  // Real controls and their dirty patches: show, hide, reload, then show again.
  const setControls = async (visible, rss) => {
    await evaluate(`document.getElementById('config-show-priority-checkbox').checked=${visible}; document.getElementById('config-youtube-rss-checkbox').checked=${rss}; document.getElementById('save-system-config-btn').click()`);
    await waitFor(`!document.getElementById('save-system-config-btn').disabled && window.configData.show_priority_channel===${visible} && window.configData.youtube_rss_enabled===${rss}`);
  };
  await setControls(true, false);
  await waitFor("document.getElementById('youtube-key-status').textContent.includes('已关闭')");
  await setControls(false, false);
  await command('browsingContext.reload', { context, wait: 'complete' });
  await waitFor("window.configData?.youtube_api_key && document.getElementById('config-youtube-rss-checkbox').checked===false");
  assert.equal(await evaluate("document.getElementById('priority-channel-card').classList.contains('hidden')"), true);
  assert.equal(await evaluate("window.configData.priority_channel.enabled && window.configData.priority_channel.auto_restart"), true);
  await setControls(true, true);
  // Optional platform cards and write-only session values use real form saves.
  await evaluate("document.getElementById('config-show-twitch-checkbox').checked=false; document.getElementById('config-show-niconico-checkbox').checked=true; document.getElementById('config-nc-user-session').value='mock-session-value'; document.getElementById('save-system-config-btn').click()");
  await waitFor("!document.getElementById('save-system-config-btn').disabled && window.configData.show_twitch===false && window.configData.niconico.user_session_configured===true");
  assert.equal(await evaluate("document.getElementById('config-nc-user-session').value"), '');
  await evaluate("document.getElementById('check-nico-session').click()");
  await waitFor("document.getElementById('niconico-session-status').textContent.includes('仍被接受')");
  await evaluate("document.querySelector('.platform-session-settings').scrollIntoView({block:'center'})");
  await waitFor("document.querySelectorAll('.notification').length===0");
  await capture(join(root, 'docs/images/niconico-session.png'));
  await evaluate("window.scrollTo(0,0)");
  await evaluate("document.getElementById('config-show-twitch-checkbox').checked=true; document.getElementById('config-nc-clear-session').checked=true; document.getElementById('save-system-config-btn').click()");
  await waitFor("!document.getElementById('save-system-config-btn').disabled && window.configData.niconico.user_session_configured===false");

  await evaluate("document.getElementById('tab-overview').click()");
  await waitFor("!document.getElementById('priority-channel-card').classList.contains('hidden')");
  await evaluate("document.getElementById('refreshPriorityBtn').click()");
  await evaluate("document.getElementById('tab-settings').click()");
  await waitFor("!document.getElementById('save-system-config-btn').disabled");
  await waitFor("document.querySelectorAll('.notification').length === 0");
  await new Promise(resolve => setTimeout(resolve, 300));
  await mkdir(join(root, 'docs/images'), { recursive: true });
  await capture(join(root, 'docs/images/settings.png'));
  await evaluate("document.getElementById('public-status-settings').scrollIntoView({block:'center'})");
  assert.equal(await evaluate("[...document.getElementById('config-public-status-node').options].some(option => option.textContent==='本机')"), true);
  assert.equal(await evaluate("window.configData.cluster.enabled"), false);
  await evaluate("document.getElementById('config-public-status-node').value='local'; document.getElementById('public-status-save-btn').click()");
  await waitFor("!document.getElementById('public-status-save-btn').disabled && window.configData.cluster.public_status.node_id==='local'");
  assert.equal(await evaluate("window.configData.cluster.enabled"), false);
  await waitFor("document.querySelectorAll('.notification').length===0");
  await capture(join(root, 'docs/images/public-status.png'));
  await evaluate("document.getElementById('config-public-status-node').value=''; document.getElementById('public-status-save-btn').click()");
  await waitFor("!document.getElementById('public-status-save-btn').disabled && window.configData.cluster.public_status.node_id===''");
  await evaluate("document.getElementById('tab-manage').click(); document.getElementById('area-catalog-load').click()");
  await waitFor("!document.getElementById('area-catalog-select').disabled");
  await evaluate("document.getElementById('area-catalog-select').value='329'; document.getElementById('area-catalog-select').dispatchEvent(new Event('change'))");
  assert.equal(await evaluate("document.getElementById('area-name').value"), '无畏契约');
  assert.equal(await evaluate("document.querySelectorAll('#area-catalog-select option').length > 300"), true);
  await evaluate("document.getElementById('area-catalog-select').value='1000'");
  for (const width of [1440, 768, 390, 320]) {
    await command('browsingContext.setViewport', { context, viewport: { width, height: 900 }, devicePixelRatio: 1 });
    assert.equal(await evaluate('document.documentElement.scrollWidth <= innerWidth'), true, `loaded area management overflow at ${width}`);
  }
  await command('browsingContext.setViewport', { context, viewport: { width: 1440, height: 1100 }, devicePixelRatio: 1 });
  await evaluate("document.getElementById('area-catalog-select').value='329'; window.scrollTo(0,0)");
  await capture(join(root, 'docs/images/area-picker.png'));
  await evaluate("document.getElementById('channel-youtube').value='https://www.youtube.com/@example'; document.getElementById('manage-resolve-youtube').click()");
  await waitFor("document.getElementById('channel-youtube').value.startsWith('UC')");
  await fetch(`${base}/mock/setup-mode`);
  await command('browsingContext.reload', { context, wait: 'complete' });
  await waitFor("document.getElementById('setup-page').classList.contains('active')");
  await evaluate("document.getElementById('setup-step-1-next-btn').click(); document.getElementById('setup-room').value='10000'; document.getElementById('setup-step-2-next-btn').click(); document.getElementById('setup-load-areas').click()");
  await waitFor("document.getElementById('setup-area-hint').textContent.includes('已加载') && document.getElementById('setup-yt-channel-select').options.length > 2");
  assert.equal(await evaluate("['yt','tw','nc'].every(p => document.getElementById('setup-'+p+'-channel-select').value==='')"), true);
  await command('browsingContext.setViewport', { context, viewport: { width: 1440, height: 1280 }, devicePixelRatio: 1 });
  await evaluate("window.scrollTo(0,0)");
  await capture(join(root, 'docs/images/setup.png'));
  await command('browsingContext.setViewport', { context, viewport: { width: 1440, height: 1100 }, devicePixelRatio: 1 });
  await evaluate("document.getElementById('setup-favorites-panel').open=true; document.getElementById('setup-load-favorites').click()");
  assert.equal(await evaluate("document.getElementById('setup-favorites-status').textContent.includes('请填写')"), true);
  await evaluate("document.getElementById('setup-holodex').value='demo-api-key'; document.getElementById('setup-holodex-jwt').value='demo-jwt'");
  for (const [mode, expected] of [['error', '凭据无效'], ['empty', '没有可导入']]) {
    await fetch(`${base}/mock/favorites-mode?mode=${mode}`);
    await evaluate("document.getElementById('setup-load-favorites').click()");
    await waitFor(`!document.getElementById('setup-load-favorites').disabled && document.getElementById('setup-favorites-status').textContent.includes('${expected}')`);
  }
  // A response for an older JWT must not replace the new account's choices.
  await fetch(`${base}/mock/favorites-mode?mode=slow`);
  await evaluate("document.getElementById('setup-load-favorites').click(); document.getElementById('setup-holodex-jwt').value='new-demo-jwt'; document.getElementById('setup-holodex-jwt').dispatchEvent(new Event('input'))");
  await waitFor("!document.getElementById('setup-load-favorites').disabled");
  assert.equal(await evaluate("document.getElementById('setup-favorites-choices').classList.contains('hidden')"), true);
  await fetch(`${base}/mock/favorites-mode?mode=ok`);
  await evaluate("document.getElementById('setup-load-favorites').click()");
  await waitFor("document.querySelectorAll('.setup-favorite-option').length===160");
  await evaluate("document.querySelectorAll('.setup-favorite-option input')[0].click(); document.querySelectorAll('.setup-favorite-option input')[1].click()");
  await evaluate("document.getElementById('setup-favorites-search').value='Demo Channel 100'; document.getElementById('setup-favorites-search').dispatchEvent(new Event('input')); document.getElementById('setup-favorites-select').click()");
  assert.equal(await evaluate("document.querySelectorAll('.setup-favorite-option').length"), 1);
  assert.equal(await evaluate("document.getElementById('setup-favorites-count').textContent.includes('已选 3')"), true);
  await evaluate("document.getElementById('setup-favorites-search').value=''; document.getElementById('setup-favorites-search').dispatchEvent(new Event('input'))");
  await evaluate("{ const select=document.getElementById('setup-yt-channel-select'); select.value=[...select.options].find(o=>o.textContent.includes('Demo Favourite')).value; select.dispatchEvent(new Event('change')); }");
  assert.equal(await evaluate("document.getElementById('setup-yt-id').value"), 'UC0000000000000000000000');
  await evaluate("document.querySelectorAll('.setup-favorite-option input')[0].click()");
  assert.equal(await evaluate("document.getElementById('setup-yt-channel-select').value"), '');
  await evaluate("document.querySelectorAll('.setup-favorite-option input')[0].click(); document.getElementById('setup-yt-channel-select').value='manual'; document.getElementById('setup-yt-channel-select').dispatchEvent(new Event('change')); document.getElementById('setup-yt-area').value='1000'");
  for (const width of [1440, 768, 390, 320]) {
    await command('browsingContext.setViewport', { context, viewport: { width, height: 900 }, devicePixelRatio: 1 });
    assert.equal(await evaluate('document.documentElement.scrollWidth <= innerWidth'), true, `loaded setup overflow at ${width}`);
    assert.equal(await evaluate("document.getElementById('setup-favorites-list').getBoundingClientRect().height <= 290"), true, `favorites height at ${width}`);
  }
  await command('browsingContext.setViewport', { context, viewport: { width: 1440, height: 1100 }, devicePixelRatio: 1 });
  await evaluate("document.getElementById('setup-favorites-panel').scrollIntoView({block:'start'}); window.scrollBy(0,-24)");
  await evaluate("{ const select=document.getElementById('setup-yt-channel-select'); select.value=[...select.options].find(o=>o.textContent.includes('Demo Favourite')).value; select.dispatchEvent(new Event('change')); }");
  await evaluate("document.getElementById('setup-yt-area').value='329'");
  await capture(join(root, 'docs/images/holodex-favorites.png'));
  await evaluate("document.getElementById('setup-yt-channel-select').value='manual'; document.getElementById('setup-yt-channel-select').dispatchEvent(new Event('change'))");
  await evaluate("document.getElementById('setup-favorites-panel').open=false; document.getElementById('setup-yt-name').value='Demo Studio'; document.getElementById('setup-yt-id').value='https://www.youtube.com/@example'; document.getElementById('setup-resolve-youtube').click()");
  await waitFor("document.getElementById('setup-yt-id').value.startsWith('UC')");
  await evaluate("document.getElementById('setup-yt-area').value='329'; document.getElementById('setup-nc-channel-select').value='manual'; document.getElementById('setup-nc-channel-select').dispatchEvent(new Event('change')); document.getElementById('setup-nc-name').value='Demo Niconico'; document.getElementById('setup-nc-id').value='demo-channel'; document.getElementById('setup-save-btn').click()");
  await waitFor("!document.getElementById('setup-page').classList.contains('active')");
  let writes = await (await fetch(`${base}/mock/writes`)).json();
  const setup = writes.find(row => row.path === '/api/setup/save-config').patch;
  assert.equal(setup.youtube_channel_id, 'UCvUc0m317LWTTPZoBQV479A');
  assert.equal(setup.niconico_channel_id, 'demo-channel');
  assert.equal(setup.youtube_enable_monitor, true);
  assert.equal(setup.twitch_enable_monitor, false);
  assert.equal(setup.niconico_enable_monitor, true);
  assert.equal(setup.selected_youtube_channels.length, 3);
  assert.ok(setup.selected_areas.some(area => area.id === 329));
  // Import-only setup, with no source enabled and no selected source areas.
  await fetch(`${base}/mock/setup-mode`);
  await command('browsingContext.reload', { context, wait: 'complete' });
  await waitFor("document.getElementById('setup-page').classList.contains('active')");
  await evaluate("document.getElementById('setup-step-1-next-btn').click(); document.getElementById('setup-room').value='10000'; document.getElementById('setup-step-2-next-btn').click(); document.getElementById('setup-favorites-panel').open=true; document.getElementById('setup-holodex').value='demo-api-key'; document.getElementById('setup-holodex-jwt').value='demo-jwt'; document.getElementById('setup-load-favorites').click()");
  await waitFor("document.querySelectorAll('.setup-favorite-option').length===160");
  await evaluate("document.querySelector('.setup-favorite-option input').click(); document.getElementById('setup-save-btn').click()");
  await waitFor("!document.getElementById('setup-page').classList.contains('active')");
  writes = await (await fetch(`${base}/mock/writes`)).json();
  const importOnly = writes.filter(row => row.path === '/api/setup/save-config').at(-1).patch;
  assert.equal(importOnly.youtube_enable_monitor || importOnly.twitch_enable_monitor || importOnly.niconico_enable_monitor, false);
  assert.equal(importOnly.selected_youtube_channels.length, 1);
  assert.equal(importOnly.selected_areas.length, 0);
  assert.ok(writes.filter(row => row.path === '/api/config').length >= 3);
  assert.ok(writes.filter(row => row.path === '/api/config').every(row => !('priority_channel' in row.patch)));
  await command('browsingContext.setViewport', { context, viewport: { width: 390, height: 844 }, devicePixelRatio: 1 });
  await evaluate("document.getElementById('tab-overview').click()");
  await new Promise(resolve => setTimeout(resolve, 300));
  assert.equal(await evaluate('document.documentElement.scrollWidth <= innerWidth'), true, 'mobile overview overflow');
  for (const view of ['settings', 'manage']) {
    await evaluate(`document.getElementById('tab-${view}').click()`);
    await new Promise(resolve => setTimeout(resolve, 200));
    assert.equal(await evaluate('document.documentElement.scrollWidth <= innerWidth'), true, `mobile ${view} overflow`);
  }
  console.log('Mock browser checks passed: show/hide/save/reload, monitoring preserved, RSS state, platform visibility, write-only session, official areas, channel URLs, favorites preview/search/import-only, no-target monitors, standalone public page, 302 areas/160 favorites at 1440/768/390/320px. Screenshots updated.');
  await command('session.end', {});
} finally {
  ws?.close(); browser.kill(); server.closeAllConnections(); server.close();
  await new Promise(resolve => browser.exitCode !== null ? resolve() : browser.once('exit', resolve));
  await rm(profile, { recursive: true, force: true });
}
