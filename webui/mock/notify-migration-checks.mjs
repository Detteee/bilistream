import assert from 'node:assert/strict';

// Compiled controls only: a real reload verifies browser persistence, and a
// held HTTP response verifies ordering independently of source implementation.
export async function checkNotifyMigration({ base, context, command, evaluate, waitFor }) {
  const el = id => `document.getElementById(${JSON.stringify(id)})`;
  const banner = el('notify-watch-migration');
  const options = "[...document.querySelectorAll('#notify-watch-results input')]";
  const rows = "document.querySelectorAll('#notify-watch-list > li').length";
  const writes = async () => (await (await fetch(`${base}/mock/writes`)).json()).filter(w => w.path === '/api/notify');
  const ready = async () => {
    await waitFor("document.documentElement.classList.contains('app-ready')");
    await evaluate(`${el('tab-settings')}.click()`);
    await waitFor(`!${el('notify-watch-search')}.disabled && !${el('notify-app-secret')}.disabled`);
  };
  const reload = async () => { await command('browsingContext.reload', { context, wait: 'complete' }); await ready(); };
  const open = async mode => { assert.ok((await fetch(`${base}/mock/notify-mode?mode=${mode}`)).ok); await reload(); };
  const save = async () => {
    await evaluate(`${el('notify-save-btn')}.click()`);
    await waitFor(`${el('notify-save-btn')}.getAttribute('aria-busy')!=='true' && !${el('notify-app-secret')}.disabled`);
  };
  const refresh = async () => {
    await evaluate(`${el('notify-refresh-btn')}.click()`);
    await waitFor(`!${el('notify-refresh-icon')}.classList.contains('is-spinning')`);
  };

  try {
    await open('watch');
    const before = (await writes()).length;
    assert.equal(await evaluate(`${banner}.hidden`), false);
    await evaluate(`${el('notify-watch-picker-toggle')}.click(); ${options}[23].click(); ${el('notify-confirm-secs')}.value='61'; ${el('notify-confirm-secs')}.dispatchEvent(new Event('input',{bubbles:true})); ${el('notify-watch-migration-dismiss')}.click()`);
    assert.equal(await evaluate(`${banner}.hidden`), true);
    assert.equal(await evaluate(rows), 3, 'acknowledgement preserves watch draft');
    assert.equal(await evaluate(`${el('notify-confirm-secs')}.value`), '61', 'unrelated draft preserved');
    assert.equal(await evaluate('document.activeElement.id'), 'notify-watch-picker-toggle');
    await refresh();
    assert.equal(await evaluate(`${banner}.hidden`), true);
    assert.equal((await writes()).length, before, 'acknowledgement sends no PUT');
    await reload();
    assert.equal(await evaluate(`${banner}.hidden`), true, 'same migration stays acknowledged after reload');
    assert.equal(await evaluate(rows), 2, 'acknowledgement did not save the draft');
    await open('watch-other-node');
    assert.equal(await evaluate(`${banner}.hidden`), false, 'other node with the same migration is independent');
    await open('watch-next-migration');
    assert.equal(await evaluate(`${banner}.hidden`), false, 'distinct migration on original node still appears');

    // Browsers may deny access to the storage property itself.
    await evaluate(`window.migrationStorageDescriptor=Object.getOwnPropertyDescriptor(window,'localStorage'); Object.defineProperty(window,'localStorage',{configurable:true,get(){throw new Error('storage denied');}}); ${el('notify-watch-migration-dismiss')}.click()`);
    assert.equal(await evaluate(`${banner}.hidden`), true);
    await refresh();
    assert.equal(await evaluate(`${banner}.hidden`), true, 'denied storage still acknowledges in this page');
    await evaluate("if(window.migrationStorageDescriptor) Object.defineProperty(window,'localStorage',window.migrationStorageDescriptor); else delete window.localStorage; delete window.migrationStorageDescriptor");
    await reload();
    assert.equal(await evaluate(`${banner}.hidden`), false, 'storage-disabled fallback does not pretend to persist');
    assert.equal((await writes()).length, before);

    // A conflict supplies the actual panel's Refresh action. Hold that GET
    // after the server creates its old snapshot, then permit the next save.
    await evaluate(`window.migrationOriginalFetch=window.fetch; window.migrationFailNext=true; window.migrationDelayNext=false; window.migrationOverride=''; window.fetch=async(url,init)=>{
      if(url==='/api/notify' && init?.method==='PUT' && window.migrationFailNext){window.migrationFailNext=false;return new Response(JSON.stringify({success:false,message:'stale',data:{code:'credentials_conflict'}}),{status:409,headers:{'Content-Type':'application/json'}});}
      const response=await window.migrationOriginalFetch(url,init);
      if(url!=='/api/notify' || (init?.method && init.method!=='GET')) return response;
      const body=await response.json();
      if(window.migrationOverride==='newer'){
        body.data.revision+=1; body.data.config.confirm_secs=45; body.data.config.recovery_hold_secs=67;
        body.data.config.watch.migration_notice={at:1900000000,folded:1};
        body.data.configured_channels.push({name:'新配置频道',platforms:['youtube']});
      } else if(window.migrationOverride==='legacy'){
        delete body.data.revision; body.data.config.recovery_hold_secs=91;
      }
      window.migrationOverride='';
      const result=new Response(JSON.stringify(body),{status:response.status,headers:{'Content-Type':'application/json'}});
      if(!window.migrationDelayNext) return result;
      window.migrationDelayNext=false;
      const text=result.text.bind(result); result.text=async()=>{const data=await text();setTimeout(()=>{window.migrationOldReadDone=true;},0);return data;};
      return new Promise(resolve=>{window.migrationRelease=()=>resolve(result);});
    }; ${el('notify-watch-picker-toggle')}.click(); ${options}[23].click()`);
    await save();
    await waitFor(`${el('notify-alert')}.querySelector('button')!==null`);
    await evaluate(`window.migrationDelayNext=true; ${el('notify-alert')}.querySelector('button').click()`);
    await waitFor("typeof window.migrationRelease==='function'");
    await save();
    assert.equal(await evaluate(`${banner}.hidden`), true, 'save response clears notice immediately');
    assert.equal(await evaluate(rows), 3);
    assert.equal((await writes()).length, before + 1, 'only the explicit successful save writes');
    await evaluate('window.migrationRelease()');
    await waitFor('window.migrationOldReadDone===true');
    assert.equal(await evaluate(`${banner}.hidden`), true, 'late old GET cannot restore notice');
    assert.equal(await evaluate(rows), 3, 'late old GET cannot restore old list');

    // A later higher revision is accepted and still merges untouched fields.
    await evaluate(`${el('notify-confirm-secs')}.value='61'; ${el('notify-confirm-secs')}.dispatchEvent(new Event('input',{bubbles:true})); window.migrationFailNext=true`);
    await save();
    await evaluate(`window.migrationOverride='newer'; ${el('notify-alert')}.querySelector('button').click()`);
    await waitFor(`${el('notify-recovery-secs')}.value==='67'`);
    assert.equal(await evaluate(`${banner}.hidden`), false, 'newer migration is still shown');
    assert.equal(await evaluate(`${el('notify-confirm-secs')}.value`), '61');
    assert.equal(await evaluate(`${options}.length`), 25, 'newer catalog applies');
    // Older API responses may omit a revision; preserve that compatibility.
    await evaluate('window.migrationFailNext=true');
    await save();
    await evaluate(`window.migrationOverride='legacy'; ${el('notify-alert')}.querySelector('button').click()`);
    await waitFor(`${el('notify-recovery-secs')}.value==='91'`);
    assert.equal(await evaluate(`${el('notify-confirm-secs')}.value`), '61');
    console.log('QQ migration browser checks passed: acknowledgement without writes/draft loss, reload persistence, node/future-migration scope, denied storage fallback, immediate save clearing, late stale GET ignored, newer and revision-less GET accepted.');
  } finally {
    await evaluate("if(window.migrationOriginalFetch) window.fetch=window.migrationOriginalFetch; if(window.migrationStorageDescriptor) Object.defineProperty(window,'localStorage',window.migrationStorageDescriptor); delete window.migrationOriginalFetch; delete window.migrationStorageDescriptor; window.migrationRelease?.();");
    await fetch(`${base}/mock/notify-mode?mode=ok`);
  }
}
