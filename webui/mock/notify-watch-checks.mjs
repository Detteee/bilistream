import assert from 'node:assert/strict';
import { join } from 'node:path';

// Real compiled-page controls and requests, including keyboard defaults. No
// imports of the state helpers: this checks their connection to the live DOM.
export async function checkNotifyWatch({ base, context, command, evaluate, waitFor, capture, captureDir }) {
  const el = id => `document.getElementById(${JSON.stringify(id)})`;
  const options = "[...document.querySelectorAll('#notify-watch-results input')]";
  const rows = "[...document.querySelectorAll('#notify-watch-list > li')]";
  const historical = `${rows}.find(row=>row.dataset.unavailable==='true')`;
  const onlyConfiguredControls = () => evaluate(`${el('notify-watch')}.querySelector('input:not([type="checkbox"]):not([type="search"]),textarea,#notify-watch-add,#notify-watch-manual')===null && !${el('notify-watch')}.textContent.includes('手动添加')`);
  const read = async expression => JSON.parse(await evaluate(`JSON.stringify(${expression})`));
  const writes = async () => (await (await fetch(`${base}/mock/writes`)).json()).filter(w => w.path === '/api/notify');
  const type = (id, value) => evaluate(`${el(id)}.value=${JSON.stringify(value)}; ${el(id)}.dispatchEvent(new Event('input', {bubbles:true}))`);
  const key = value => command('input.performActions', { context, actions: [{ type: 'key', id: 'watch-keyboard', actions: [{ type: 'keyDown', value }, { type: 'keyUp', value }] }] });
  const ready = async () => {
    await waitFor("document.documentElement.classList.contains('app-ready')");
    await evaluate(`${el('tab-notify')}.click()`);
    await waitFor(`!${el('notify-watch-search')}.disabled && !${el('notify-app-secret')}.disabled`);
  };
  const open = async mode => {
    assert.ok((await fetch(`${base}/mock/notify-mode?mode=${mode}`)).ok);
    await command('browsingContext.reload', { context, wait: 'complete' });
    await ready();
  };
  const expand = async () => {
    await evaluate(`${el('notify-watch-picker-toggle')}.focus()`);
    await key(' ');
    await waitFor(`${el('notify-watch-picker')}.open`);
  };
  const save = async () => {
    await evaluate(`${el('notify-save-btn')}.click()`);
    await waitFor(`${el('notify-save-btn')}.getAttribute('aria-busy')!=='true' && !${el('notify-app-secret')}.disabled`);
  };
  const refresh = async () => {
    await evaluate(`${el('notify-refresh-btn')}.click()`);
    await waitFor(`!${el('notify-refresh-icon')}.classList.contains('is-spinning')`);
  };
  const shot = async name => {
    await evaluate(`${el('notify-watch')}.scrollIntoView({block:'start'})`);
    await capture(join(captureDir, name));
  };
  const measures = [];

  try {
    await open('watch');
    const before = (await writes()).length;
    assert.equal(await evaluate(`${el('notify-watch-enabled')}.checked`), false);
    assert.equal(await evaluate(`${el('notify-watch-picker')}.open`), false);
    assert.equal(await onlyConfiguredControls(), true);
    await expand();
    assert.equal(await evaluate(`${options}.length`), 24, 'all channels browseable without typing');
    assert.equal(await evaluate(`${options}[0].checked`), true);
    assert.equal(await evaluate(`${rows}.length`), 2);
    assert.equal(await evaluate(`${historical}.querySelector('.notify-watch-live').textContent`), '不可用');
    assert.equal(await evaluate(`${historical}.querySelector('.notify-watch-live').classList.contains('active')`), false);
    // Browse beyond the old eight-result cap and select through native Space.
    await evaluate(`${options}[23].scrollIntoView({block:'nearest'}); ${options}[23].focus(); window.watchFocus=${options}[23]`);
    await key(' ');
    assert.equal(await evaluate(`${options}[23].checked`), true);
    assert.equal(await evaluate(`${rows}.length`), 3);
    assert.equal(await evaluate('document.activeElement===window.watchFocus'), true);
    await refresh();
    assert.equal(await evaluate(`document.activeElement===window.watchFocus && ${options}[23]===window.watchFocus`), true);
    assert.equal((await writes()).length, before, 'selections are drafts');
    await key('\uE00C');
    assert.equal(await evaluate(`${el('notify-watch-picker')}.open`), false);
    assert.equal(await evaluate('document.activeElement.id'), 'notify-watch-picker-toggle');
    assert.equal(await evaluate(`${rows}.length`), 3, 'selected channels visible when dropdown closes');

    await expand();
    await type('notify-watch-search', '星海');
    assert.equal(await evaluate(`${options}.length`), 1);
    await evaluate(`${el('notify-watch-search')}.focus(); window.watchSearch=${el('notify-watch-search')}`);
    await refresh();
    assert.equal(await evaluate(`${el('notify-watch-search')}.value`), '星海');
    assert.equal(await evaluate('document.activeElement===window.watchSearch'), true);
    await type('notify-watch-search', '没有这个频道');
    assert.equal(await evaluate(`${options}.length`), 0);
    assert.match(await evaluate(`${el('notify-watch-no-results')}.textContent`), /没有匹配/);
    assert.match(await evaluate(`${el('notify-watch-no-results')}.textContent`), /频道管理/);
    assert.equal(await evaluate(`${rows}.length`), 3);
    await shot('qq-watch-no-match.png');
    await evaluate(`${el('notify-watch-search-clear')}.click()`);
    assert.equal(await evaluate(`${options}.length`), 24);
    assert.equal(await evaluate('document.activeElement.id'), 'notify-watch-search');

    // A selected multi-platform name is removed as one channel, then restored.
    await evaluate(`${rows}[0].querySelector('button').click()`);
    assert.equal(await evaluate(`${options}[0].checked`), false);
    assert.equal(await evaluate(`${rows}.length`), 2);
    assert.equal(await evaluate("document.activeElement.classList.contains('notify-watch-remove')"), true);
    await evaluate(`${options}[0].click()`);
    assert.equal(await evaluate(`${rows}.length`), 3);

    assert.equal((await writes()).length, before);
    await save();
    const saved = (await writes()).slice(before);
    assert.equal(saved.length, 1, 'one settings PUT');
    assert.equal(saved[0].patch.base_generation, 7);
    const channels = saved[0].patch.config.watch.channels;
    assert.equal(channels.length, 3);
    assert.deepEqual(channels.find(c => c.name === '星海频道'), { kind: 'configured', name: '星海频道' });
    assert.deepEqual(channels.find(c => c.name === '电台夜话'), { kind: 'manual', name: '电台夜话', platforms: { youtube: 'UCmanualradio', twitch: 'manualradio' } }, 'historical payload is retained unchanged');
    assert.equal(channels.filter(c => c.kind !== 'configured').length, 1, 'no new manual entry');
    assert.equal(await evaluate(`${options}.length`), 24, 'save response preserves full picker');
    await command('browsingContext.reload', { context, wait: 'complete' });
    await ready();
    assert.equal(await evaluate(`${rows}.length`), 3, 'saved grouped list reloads');
    assert.equal(await evaluate(`${historical}.querySelector('.notify-watch-live').textContent`), '不可用');
    await evaluate(`${historical}.querySelector('.notify-watch-remove').click()`);
    await save();
    assert.equal((await writes()).at(-1).patch.config.watch.channels.every(c => c.kind === 'configured'), true, 'historical row is removable');
    assert.equal(await evaluate(`${rows}.length`), 2);

    // A refused save keeps the complete draft and does not retry itself.
    await expand();
    await evaluate(`${options}[1].click(); window.watchOriginalFetch=window.fetch; window.watchRefused=0; window.fetch=(url,init)=>{ if(url==='/api/notify' && init?.method==='PUT'){window.watchRefused++;return Promise.resolve(new Response(JSON.stringify({success:false,message:'missing channel',data:{code:'watch_unknown_channel'}}),{status:400,headers:{'Content-Type':'application/json'}}));} return window.watchOriginalFetch(url,init); }`);
    await save();
    assert.equal(await evaluate(`${rows}.length`), 3);
    assert.match(await evaluate(`${el('notify-alert')}.textContent`), /频道管理/);
    assert.equal(await evaluate('window.watchRefused'), 1);
    // Rev 6: a refusal focuses its recovery target, else the banner; this
    // code has a 刷新 action on the banner, so the banner gets focus.
    assert.equal(await evaluate('document.activeElement.id'), 'notify-alert');
    await evaluate('window.fetch=window.watchOriginalFetch; delete window.watchOriginalFetch');

    await open('watch-empty');
    await expand();
    assert.equal(await evaluate(`${options}.length`), 0);
    assert.match(await evaluate(`${el('notify-watch-no-results')}.textContent`), /频道管理/);
    assert.equal(await onlyConfiguredControls(), true);
    await shot('qq-watch-empty.png');
    await open('watch-missing');
    assert.match(await evaluate(`${rows}[0].textContent`), /未找到/);
    const missingBefore = (await writes()).length;
    await expand();
    await evaluate(`${options}[0].click()`);
    await save();
    assert.deepEqual((await writes()).slice(missingBefore)[0].patch.config.watch.channels[0], { kind: 'configured', name: '已移除的频道' });
    await shot('qq-watch-missing.png');

    for (const theme of ['dark', 'light']) {
      for (const width of [320, 375, 768, 1280]) {
        await open('watch');
        await command('browsingContext.setViewport', { context, viewport: { width, height: 1100 }, devicePixelRatio: 1 });
        await evaluate(`if(document.documentElement.classList.contains('light-theme')!==${theme === 'light'}) ${el('theme-toggle')}.click()`);
        assert.equal(await evaluate("document.documentElement.classList.contains('light-theme')"), theme === 'light');
        await expand();
        await evaluate(`${options}[1].click()`); // long selected name
        const measure = await read(`({width:innerWidth,page:document.documentElement.scrollWidth,list:${el('notify-watch-results')}.getBoundingClientRect().width,scroll:${el('notify-watch-results')}.scrollHeight,bound:${el('notify-watch-results')}.clientHeight,targets:[...document.querySelectorAll('#notify-watch button,#notify-watch-picker-toggle,.notify-watch-option,#notify-watch-search')].filter(e=>e.checkVisibility()).map(e=>e.getBoundingClientRect().height)})`);
        assert.ok(measure.page <= measure.width, `${theme} ${width}: page overflow ${JSON.stringify(measure)}`);
        assert.ok(measure.scroll > measure.bound && measure.bound <= 324, 'bounded browsable dropdown');
        assert.ok(measure.targets.every(h => h >= 44), '44 px interactive rows/buttons');
        assert.equal(await evaluate(`${el('notify-watch')}.querySelector('[style]')===null`), true, 'CSP-safe markup');
        assert.equal(await onlyConfiguredControls(), true);
        measures.push({ theme, viewport: width, ...measure });
        await shot(`qq-watch-dropdown-${theme}-${width}.png`);
        await evaluate(`${el('notify-watch-picker')}.open=false`);
        await shot(`qq-watch-selected-${theme}-${width}.png`);
      }
    }
    console.log('QQ watch browser checks passed: configured-only choices with no manual/account controls, browse all 24 names, checkbox/Space/Escape, stable draft/focus, optional search, grouped save/reload, unavailable historical retention/removal, empty/unresolved, 320/375/768/1280 dark/light.');
    console.log(JSON.stringify(measures));
  } finally {
    await fetch(`${base}/mock/notify-mode?mode=ok`);
    await evaluate('if(window.watchOriginalFetch) window.fetch=window.watchOriginalFetch; delete window.watchOriginalFetch; delete window.watchFocus; delete window.watchSearch');
  }
}
