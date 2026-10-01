// Node 22+ and Firefox. Uses WebDriver BiDi; no automation package required.
import { spawn } from 'node:child_process';
import { mkdtemp, mkdir, writeFile, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import assert from 'node:assert/strict';
import { createMockServer } from './server.mjs';
import { checkPanelPasswords } from './password-checks.mjs';
import { checkClusterMembership } from './membership-checks.mjs';

const server = createMockServer();
await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
const base = `http://127.0.0.1:${server.address().port}`;
const profile = await mkdtemp(join(tmpdir(), 'bilistream-mock-'));
const browser = spawn(process.env.FIREFOX || 'firefox', ['--headless', '--no-remote', '--profile', profile, '--remote-debugging-port', '0']);
const root = fileURLToPath(new URL('../../', import.meta.url));
const captureDir = process.argv[2] || join(root, 'docs/images');
await mkdir(captureDir, { recursive: true });
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
    for (let i = 0; i < 100; i++) { if (await evaluate(`document.readyState === 'complete' && (${expression})`)) return; await new Promise(resolve => setTimeout(resolve, 100)); }
    throw new Error(`UI condition timed out: ${expression}`);
  };
  await command('browsingContext.setViewport', { context, viewport: { width: 1440, height: 1320 }, devicePixelRatio: 1 });
  await command('browsingContext.navigate', { context, url: base, wait: 'complete' });
  await waitFor("document.documentElement.classList.contains('app-ready') && document.getElementById('yt-title').textContent.includes('示例')");
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
  await capture(join(captureDir, 'screenshot_of_webui.png'));
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
  await waitFor("window.configData?.youtube_api_key_configured && document.getElementById('config-youtube-rss-checkbox').checked===false");
  assert.equal(await evaluate("document.getElementById('priority-channel-card').classList.contains('hidden')"), true);
  assert.equal(await evaluate("window.configData.priority_channel.enabled && window.configData.priority_channel.auto_restart"), true);
  await setControls(true, true);
  assert.equal(await evaluate("document.getElementById('priority-channel-card').textContent.includes('其他单机 (235)')"), true);
  await evaluate("document.getElementById('tab-overview').click()");
  await waitFor("document.querySelector('.holodex-stream-watch')?.checkVisibility()");
  assert.equal(await evaluate("[...document.querySelectorAll('.crop-switch-button')].every(button => !button.textContent.trim() && button.getAttribute('aria-label')==='裁剪切换' && button.parentElement.firstElementChild===button)"), true);
  assert.equal(await evaluate("[...document.querySelectorAll('.holodex-stream-actions')].every(row => Math.abs(row.querySelector('.holodex-stream-secondary').getBoundingClientRect().width - row.querySelector('.switch-button').getBoundingClientRect().width)<1)"), true);
  const actionContentsCentered = "[...document.querySelectorAll('.holodex-stream-watch, .holodex-stream-btn-switch')].every(button => { const box=button.getBoundingClientRect(), text=button.querySelector('span').getBoundingClientRect(), icon=button.querySelector('svg').getBoundingClientRect(); return box.width>0 && Math.abs((box.left+box.right)/2-(icon.left+text.right)/2)<1 && icon.right<=text.left; })";
  assert.equal(await evaluate(actionContentsCentered), true, 'desktop action content midpoint');
  await evaluate("document.getElementById('tab-settings').click()");
  await evaluate("document.getElementById('config-interval').value='88'; document.getElementById('config-nc-user-session').value='synthetic-direct-check'; document.getElementById('config-nc-user-session').dispatchEvent(new Event('input'))");
  assert.equal(await evaluate("document.getElementById('check-nico-session').textContent"), '保存并检测');
  await evaluate("document.getElementById('check-nico-session').click()");
  assert.equal(await evaluate("document.getElementById('niconico-session-status').textContent.includes('正在')"), true);
  await waitFor("!document.getElementById('check-nico-session').disabled && document.getElementById('niconico-session-status').textContent.includes('仍被接受')");
  assert.equal(await evaluate("document.getElementById('config-nc-user-session').value"), '');
  assert.equal(await evaluate("document.getElementById('config-nc-user-session').placeholder"), '•'.repeat('synthetic-direct-check'.length));
  assert.equal(await evaluate("document.getElementById('config-interval').value"), '88');
  assert.equal((await (await fetch(`${base}/api/config`)).json()).interval, 30);
  await evaluate("document.getElementById('config-interval').value='30'");
  // Optional platform cards and write-only session values use real form saves.
  await evaluate("document.getElementById('config-show-twitch-checkbox').checked=false; document.getElementById('config-show-niconico-checkbox').checked=true; document.getElementById('config-nc-user-session').value='mock-session-value'; document.getElementById('save-system-config-btn').click()");
  await waitFor("!document.getElementById('save-system-config-btn').disabled && window.configData.show_twitch===false && window.configData.niconico.user_session_configured===true");
  assert.equal(await evaluate("document.getElementById('config-nc-user-session').value"), '');
  await evaluate("document.getElementById('check-nico-session').click()");
  await waitFor("document.getElementById('niconico-session-status').textContent.includes('仍被接受')");
  await evaluate("document.querySelector('.platform-session-settings').scrollIntoView({block:'center'})");
  await waitFor("document.querySelectorAll('.notification').length===0");
  await capture(join(captureDir, 'niconico-session.png'));
  await evaluate("window.scrollTo(0,0)");
  assert.equal(await evaluate("document.getElementById('config-nc-user-session').placeholder"), '•'.repeat('mock-session-value'.length));
  await evaluate("document.getElementById('config-show-twitch-checkbox').checked=true; document.getElementById('config-nc-clear-session').click()");
  await waitFor("!document.getElementById('save-system-config-btn').disabled && window.configData.niconico.user_session_configured===false");
  assert.equal(await evaluate("window.configData.show_twitch"), false);
  assert.equal(await evaluate("document.getElementById('config-show-twitch-checkbox').checked"), true);
  assert.equal(await evaluate("document.getElementById('config-nc-clear-session').hidden"), true);
  assert.equal(await evaluate("document.getElementById('config-nc-user-session').placeholder"), '仅填写 user_session 的值');
  await evaluate("document.getElementById('save-system-config-btn').click()");
  await waitFor("!document.getElementById('save-system-config-btn').disabled && window.configData.show_twitch===true");
  assert.equal(await evaluate("document.getElementById('config-youtube-api-key').placeholder"), '•'.repeat(39) + '\n' + '•'.repeat(39));
  assert.equal(await evaluate("document.getElementById('config-youtube-api-key').value"), '');

  await evaluate("document.getElementById('tab-overview').click()");
  await waitFor("!document.getElementById('priority-channel-card').classList.contains('hidden')");
  await evaluate("document.getElementById('refreshPriorityBtn').click()");
  await evaluate("document.getElementById('tab-settings').click()");
  await waitFor("!document.getElementById('save-system-config-btn').disabled");
  await waitFor("document.querySelectorAll('.notification').length === 0");
  await new Promise(resolve => setTimeout(resolve, 300));

  await capture(join(captureDir, 'settings.png'));
  assert.equal(await evaluate("window.configData.youtube_api_key"), '');
  await waitFor("document.getElementById('yt-cookie-status').textContent.includes('尚未')");
  const cookieFixture = '# Netscape HTTP Cookie File\n.example.invalid\tTRUE\t/\tTRUE\t0\tsession\tsynthetic-only\n';
  await evaluate(`document.getElementById('yt-cookie-paste').value=${JSON.stringify(cookieFixture)}; document.getElementById('yt-cookie-save').click()`);
  await waitFor("document.getElementById('yt-cookie-status').textContent.includes('1 条') && !document.getElementById('yt-cookie-save').disabled");
  assert.equal(await evaluate("document.getElementById('yt-cookie-paste').value"), '');
  await evaluate("document.getElementById('yt-cookie-status').scrollIntoView({block:'center'})");
  await waitFor("document.querySelectorAll('.notification').length===0");
  await capture(join(captureDir, 'youtube-cookies.png'));
  await evaluate("document.getElementById('yt-cookie-clear').click()");
  await waitFor("document.getElementById('yt-cookie-status').textContent.includes('尚未') && !document.getElementById('yt-cookie-clear').disabled");
  assert.equal(await evaluate("document.getElementById('config-player-filter-group').checkVisibility()"), false);
  await evaluate("document.getElementById('config-lol-monitor-checkbox').click(); document.getElementById('config-player-filter-group').open=true");
  assert.equal(await evaluate("document.getElementById('config-player-filter').checkVisibility()"), true);
  assert.equal(await evaluate("document.getElementById('config-player-filter').closest('.settings-lol')!==null"), true);
  await evaluate("document.getElementById('config-player-filter').value='synthetic filter'; document.getElementById('config-lol-monitor-checkbox').click()");
  assert.equal(await evaluate("document.getElementById('config-player-filter-group').checkVisibility()"), false);
  await evaluate("document.getElementById('config-lol-monitor-checkbox').click()");
  assert.equal(await evaluate("document.getElementById('config-player-filter').value"), 'synthetic filter');
  await evaluate("document.getElementById('save-player-filter').click()");
  await waitFor("!document.getElementById('save-player-filter').disabled");
  assert.equal(await evaluate("fetch('/api/player-filter').then(r=>r.json()).then(r=>r.data.content)"), 'synthetic filter');
  await evaluate("document.getElementById('config-lol-monitor-checkbox').click(); document.getElementById('save-system-config-btn').click()");
  await waitFor("!document.getElementById('save-system-config-btn').disabled");
  await evaluate("document.getElementById('storage-state').scrollIntoView({block:'center'})");
  await waitFor("document.querySelectorAll('.notification').length===0");
  await capture(join(captureDir, 'data-backup.png'));

  await evaluate("document.getElementById('public-status-settings').scrollIntoView({block:'center'})");
  assert.equal(await evaluate("[...document.getElementById('config-public-status-node').options].some(option => option.textContent==='本机')"), true);
  assert.equal(await evaluate("window.configData.cluster.enabled"), false);
  await evaluate("document.getElementById('config-public-status-node').value='local'; document.getElementById('public-status-save-btn').click()");
  await waitFor("!document.getElementById('public-status-save-btn').disabled && window.configData.cluster.public_status.node_id==='local'");
  assert.equal(await evaluate("window.configData.cluster.enabled"), false);
  await waitFor("document.querySelectorAll('.notification').length===0");
  await capture(join(captureDir, 'public-status.png'));
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
  await capture(join(captureDir, 'area-picker.png'));
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
  await capture(join(captureDir, 'setup.png'));
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
  await evaluate("document.getElementById('setup-favorites-search').value='示例收藏频道 100'; document.getElementById('setup-favorites-search').dispatchEvent(new Event('input')); document.getElementById('setup-favorites-select').click()");
  assert.equal(await evaluate("document.querySelectorAll('.setup-favorite-option').length"), 1);
  assert.equal(await evaluate("document.getElementById('setup-favorites-count').textContent.includes('已选 3')"), true);
  await evaluate("document.getElementById('setup-favorites-search').value=''; document.getElementById('setup-favorites-search').dispatchEvent(new Event('input'))");
  await evaluate("{ const select=document.getElementById('setup-yt-channel-select'); select.value=[...select.options].find(o=>o.textContent.includes('示例收藏频道 001')).value; select.dispatchEvent(new Event('change')); }");
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
  await evaluate("{ const select=document.getElementById('setup-yt-channel-select'); select.value=[...select.options].find(o=>o.textContent.includes('示例收藏频道 001')).value; select.dispatchEvent(new Event('change')); }");
  await evaluate("document.getElementById('setup-yt-area').value='329'");
  await capture(join(captureDir, 'holodex-favorites.png'));
  await evaluate("document.getElementById('setup-yt-channel-select').value='manual'; document.getElementById('setup-yt-channel-select').dispatchEvent(new Event('change'))");
  await evaluate("document.getElementById('setup-favorites-panel').open=false; document.getElementById('setup-yt-name').value='示例频道 001'; document.getElementById('setup-yt-id').value='https://www.youtube.com/@example'; document.getElementById('setup-resolve-youtube').click()");
  await waitFor("document.getElementById('setup-yt-id').value.startsWith('UC')");
  await evaluate("document.getElementById('setup-yt-area').value='329'; document.getElementById('setup-nc-channel-select').value='manual'; document.getElementById('setup-nc-channel-select').dispatchEvent(new Event('change')); document.getElementById('setup-nc-name').value='示例频道 004'; document.getElementById('setup-nc-id').value='demo-channel'; document.getElementById('setup-save-btn').click()");
  await waitFor("!document.getElementById('setup-page').classList.contains('active')");
  let writes = await (await fetch(`${base}/mock/writes`)).json();
  const setup = writes.find(row => row.path === '/api/setup/save-config').patch;
  assert.equal(setup.youtube_channel_id, 'UC4444444444444444444444');
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
  assert.equal(await evaluate(actionContentsCentered), true, 'mobile action content midpoint');
  for (const view of ['settings', 'manage']) {
    await evaluate(`document.getElementById('tab-${view}').click()`);
    await new Promise(resolve => setTimeout(resolve, 200));
    assert.equal(await evaluate('document.documentElement.scrollWidth <= innerWidth'), true, `mobile ${view} overflow`);
  }
  await evaluate("document.getElementById('tab-settings').click()");
  await waitFor("document.getElementById('config-clear-holodex-key-status').textContent==='已保存'");
  await evaluate("document.getElementById('config-interval').value='77'; document.getElementById('config-holodex-key').value='unsaved-key-draft'");
  // A concurrent save makes the clear action stale; neither stored data nor drafts may disappear.
  await fetch(`${base}/api/config`, { method: 'POST', headers: {'Content-Type':'application/json'}, body: JSON.stringify({ auto_cover: false }) });
  await evaluate("document.getElementById('config-clear-holodex-key').click()");
  await waitFor("!document.getElementById('save-system-config-btn').disabled");
  assert.equal(await evaluate("document.getElementById('config-holodex-key').value"), 'unsaved-key-draft');
  assert.equal(await evaluate("document.getElementById('config-interval').value"), '77');
  assert.equal((await (await fetch(`${base}/api/config`)).json()).holodex_api_key_configured, true);
  await evaluate("document.getElementById('reload-system-config-btn').click()");
  await waitFor("document.getElementById('config-holodex-key').value===''");
  await evaluate("document.getElementById('config-clear-holodex-key').click()");
  await waitFor("!document.getElementById('save-system-config-btn').disabled && document.getElementById('config-clear-holodex-key').hidden");
  assert.equal((await (await fetch(`${base}/api/config`)).json()).holodex_api_key_configured, false);
  await evaluate("document.getElementById('config-holodex-key').value='synthetic-replacement'; document.getElementById('config-yt-proxy').value='http://127.0.0.1:7890'; document.getElementById('save-system-config-btn').click()");
  await waitFor("!document.getElementById('save-system-config-btn').disabled && document.getElementById('config-holodex-key').value===''");
  assert.equal(await evaluate("document.getElementById('config-holodex-key').placeholder"), '•'.repeat('synthetic-replacement'.length));
  assert.equal(await evaluate("document.getElementById('config-yt-proxy').value"), 'http://127.0.0.1:7890');
  assert.equal(await evaluate("document.getElementById('config-yt-proxy').type"), 'text');
  await evaluate("document.getElementById('config-yt-proxy').value=''; document.getElementById('reload-system-config-btn').click()");
  await waitFor("document.getElementById('config-yt-proxy').value==='http://127.0.0.1:7890'");
  await evaluate("document.getElementById('config-yt-proxy').value='http://127.0.0.1:8080'; document.getElementById('save-system-config-btn').click()");
  await waitFor("!document.getElementById('save-system-config-btn').disabled && window.configData.youtube.proxy==='http://127.0.0.1:8080'");
  await evaluate("document.getElementById('config-yt-proxy').value=''; document.getElementById('save-system-config-btn').click()");
  await waitFor("!document.getElementById('save-system-config-btn').disabled && document.getElementById('config-yt-proxy').value==='http://127.0.0.1:8080'");
  for (const width of [1440, 768, 390, 320]) {
    await command('browsingContext.setViewport', { context, viewport: { width, height: 900 }, devicePixelRatio: 1 });
    assert.equal(await evaluate('document.documentElement.scrollWidth <= innerWidth'), true, `credential settings overflow at ${width}`);
    assert.equal(await evaluate("[...document.querySelectorAll('.credential-heading')].every(h => !h.checkVisibility() || h.getBoundingClientRect().height < 48)"), true, `compact credential headings at ${width}`);
  }
  await evaluate("document.getElementById('config-clear-yt-proxy').click()");
  await waitFor("!document.getElementById('save-system-config-btn').disabled && document.getElementById('config-clear-yt-proxy').hidden");
  assert.equal((await (await fetch(`${base}/api/config`)).json()).youtube.proxy_configured, false);
  await evaluate("document.getElementById('config-yt-proxy').value='http://test-user:synthetic-password@proxy.invalid:8080'; document.getElementById('save-system-config-btn').click()");
  await waitFor("!document.getElementById('save-system-config-btn').disabled && document.getElementById('config-yt-proxy').value.includes('•')");
  assert.equal(await evaluate("document.getElementById('config-yt-proxy').value"), 'http://test-user:' + '•'.repeat(18) + '@proxy.invalid:8080');
  await evaluate("document.getElementById('config-yt-proxy').value=document.getElementById('config-yt-proxy').value.replace(':8080',':8081'); document.getElementById('save-system-config-btn').click()");
  await waitFor("!document.getElementById('save-system-config-btn').disabled && window.configData.youtube.proxy.endsWith(':8081')");
  const keptProxy = (await (await fetch(`${base}/mock/writes`)).json()).findLast(row => row.path === '/api/config' && row.patch.youtube_proxy_keep_password);
  assert.equal(keptProxy.patch.youtube_proxy, 'http://test-user:@proxy.invalid:8081');
  assert.equal(await evaluate("document.getElementById('config-yt-proxy').value"), 'http://test-user:' + '•'.repeat(18) + '@proxy.invalid:8081');
  // A stale session save reports an error immediately and does not check an older credential.
  const checksBefore = (await (await fetch(`${base}/mock/writes`)).json()).filter(row => row.path === '/api/niconico/session/check').length;
  await fetch(`${base}/api/config`, { method: 'POST', headers: {'Content-Type':'application/json'}, body: JSON.stringify({ auto_cover: true }) });
  await evaluate("document.getElementById('config-nc-user-session').value='synthetic-stale-session'; document.getElementById('config-nc-user-session').dispatchEvent(new Event('input')); document.getElementById('check-nico-session').click()");
  await waitFor("!document.getElementById('check-nico-session').disabled && document.getElementById('niconico-session-status').dataset.state==='unavailable'");
  assert.equal(await evaluate("document.getElementById('config-nc-user-session').value"), 'synthetic-stale-session');
  assert.equal((await (await fetch(`${base}/mock/writes`)).json()).filter(row => row.path === '/api/niconico/session/check').length, checksBefore);
  writes = await (await fetch(`${base}/mock/writes`)).json();
  assert.ok(writes.filter(row => row.path === '/api/config').every(row => !JSON.stringify(row.patch).includes('•')));
  const performanceChecks = JSON.parse(await evaluate(`(async () => {
    const pause = ms => new Promise(resolve => setTimeout(resolve, ms));
    const until = async check => { for (let n=0;n<100;n++) { if (check()) return; await pause(20); } throw new Error('Log UI did not settle'); };
    let logRequests = 0;
    const fetchOriginal = window.fetch;
    let lines = Array.from({length: 600}, (_, i) => 'INFO synthetic log ' + i);
    let networkRequests = 0;
    window.fetch = (path, options) => {
      if (path === '/api/logs') { logRequests++; return Promise.resolve(new Response(JSON.stringify({success:true,logs:lines.join(String.fromCharCode(10))}), {headers:{'Content-Type':'application/json'}})); }
      if (path === '/api/network-status') { networkRequests++; return Promise.resolve(new Response(JSON.stringify({success:true,data:{ffmpeg_running:true}}), {headers:{'Content-Type':'application/json'}})); }
      return fetchOriginal(path, options);
    };
    try {
      document.getElementById('tab-logs').click();
      const refresh = async () => {
        const before = logRequests;
        document.getElementById('refresh-logs-btn').click();
        await until(() => logRequests > before);
        await pause(40);
      };
      await until(() => document.getElementById('log-output').children.length === 500);
      const output = document.getElementById('log-output');
      const first = output.firstElementChild;
      const next = first.nextElementSibling;
      const bounded = output.children.length === 500 && first.textContent.includes('log 100');
      await refresh();
      const retained = first === output.firstElementChild;
      lines.push('ERROR synthetic log 600');
      await refresh();
      const appended = output.children.length === 500 && output.firstElementChild === next && output.lastElementChild.classList.contains('error');
      document.getElementById('clear-logs-btn').click();
      const cleared = output.textContent === '日志已清空';
      document.getElementById('setup-page').classList.add('hidden');
      document.getElementById('main-page').classList.remove('hidden');
      document.getElementById('tab-settings').click();
      networkRequests = 0;
      await pause(2100);
      const hiddenSkipped = networkRequests === 0;
      document.getElementById('tab-overview').click();
      await until(() => networkRequests > 0);
      return JSON.stringify({bounded,retained,appended,cleared,hiddenSkipped,visibleFetched:networkRequests>0});
    } finally { window.fetch = fetchOriginal; }
  })()`));
  assert.deepEqual(performanceChecks, {bounded:true,retained:true,appended:true,cleared:true,hiddenSkipped:true,visibleFetched:true});
  // A same-version package must not reload the old page during installation.
  const updateChecks = JSON.parse(await evaluate(`(async () => {
    const pause = ms => new Promise(resolve => setTimeout(resolve, ms));
    const until = async check => { for(let n=0;n<180;n++) { if(check()) return; await pause(50); } throw new Error('Update UI did not settle'); };
    const fetchOriginal = window.fetch;
    const phases = ['downloading', 'restarting', 'failed'];
    let polls = 0;
    window.fetch = (path, options) => {
      let body;
      if (path === '/api/update/check') body = {success:true,data:{has_update:true,current_version:'0.7.0',latest_version:'0.7.0',download_url:'https://example.invalid/update.tar.gz'}};
      else if (path === '/api/update/download') body = {success:true};
      else if (path === '/api/update/status') body = {phase:phases[Math.min(polls++,2)],message:'synthetic update stop'};
      else if (path === '/api/version') body = {success:true,data:{version:'0.7.0'}};
      else return fetchOriginal(path,options);
      return Promise.resolve(new Response(JSON.stringify(body),{headers:{'Content-Type':'application/json'}}));
    };
    try {
      document.getElementById('check-updates-btn').click();
      await until(() => !document.getElementById('update-notification').classList.contains('hidden'));
      document.getElementById('auto-update-btn').click();
      await until(() => polls === 3 && !document.getElementById('auto-update-btn').disabled);
      return JSON.stringify({polls,failedVisible:document.getElementById('update-progress').textContent.includes('更新失败') && [...document.querySelectorAll('.notification.error')].some(node => node.textContent.includes('synthetic update stop')),retryEnabled:!document.getElementById('auto-update-btn').disabled});
    } finally { window.fetch = fetchOriginal; }
  })()`));
  assert.deepEqual(updateChecks, {polls:3,failedVisible:true,retryEnabled:true});
  console.log('Mock browser checks passed: show/hide/save/reload, monitoring preserved, RSS state, platform visibility, write-only keys/session, managed Cookie import/clear, player filters, backup layout, official areas, channel URLs, favorites preview/search/import-only, no-target monitors, standalone public page, 302 areas/160 favorites at 1440/768/390/320px. Log-node retention and hidden-view polling passed. Screenshots saved.');
  await checkPanelPasswords({ base, context, command, evaluate, waitFor, capture, captureDir });
  // Validation screenshots only; never written into the packaged docs/images.
  await checkClusterMembership({
    base, context, command, evaluate, waitFor, captureDir,
    capture: process.argv[2] ? capture : async () => {},
    documentCapture: async () => { await capture(join(root, 'docs/images/cluster-membership.png')); },
  });
  await command('session.end', {});
} finally {
  ws?.close(); browser.kill(); server.closeAllConnections(); server.close();
  await new Promise(resolve => browser.exitCode !== null ? resolve() : browser.once('exit', resolve));
  await rm(profile, { recursive: true, force: true });
}
