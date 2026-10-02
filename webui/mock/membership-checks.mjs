import assert from 'node:assert/strict';
import { join } from 'node:path';

// Compiled-page DOM controls only, against the synthetic membership mock.
export async function checkClusterMembership({ base, context, command, evaluate, waitFor, capture, captureDir, documentCapture }) {
  const mode = value => fetch(`${base}/mock/password-mode?mode=${value}`);
  const membershipMode = value => fetch(`${base}/mock/membership-mode?mode=${value}`);
  const reload = () => command('browsingContext.reload', { context, wait: 'complete' });
  const writes = async () => (await fetch(`${base}/mock/writes`)).json();
  const loginVisible = "!!document.getElementById('webui-login-gate')?.checkVisibility()";
  const badge = text => `document.getElementById('cluster-membership-badge').textContent===${JSON.stringify(text)}`;
  const memberCount = n => `document.querySelectorAll('#cluster-member-list .cluster-member-card').length===${n}`;
  const card = nodeId => `[...document.querySelectorAll('#cluster-member-list .cluster-member-card')].find(c => c.querySelector('.cluster-peer-id').textContent.startsWith(${JSON.stringify(nodeId)}))`;
  const noOperation = "document.querySelectorAll('#cluster-operation-list .cluster-operation:not([data-phase=completed]):not([data-phase=aborted])').length===0";
  const settings = async () => {
    await evaluate("document.getElementById('tab-settings').click(); window.confirm = () => true");
    await waitFor("document.getElementById('cluster-membership-badge').textContent!=='读取中'");
  };
  const drafts = () => evaluate("[document.getElementById('config-interval').value, document.getElementById('config-cluster-heartbeat').value].join('/')");
  const secretsStored = secret => evaluate(`JSON.stringify(localStorage).includes(${JSON.stringify(secret)}) || JSON.stringify(sessionStorage).includes(${JSON.stringify(secret)})`);

  await command('browsingContext.setViewport', { context, viewport: { width: 1440, height: 1100 }, devicePixelRatio: 1 });
  await mode('missing'); await reload(); await settings();
  await waitFor(badge('单机运行'));
  assert.equal(await evaluate("document.getElementById('cluster-create-btn').disabled && document.getElementById('cluster-prepare-join-btn').disabled"), true, 'no password: setup disabled');
  assert.equal(await evaluate("document.getElementById('cluster-membership-password-hint').checkVisibility()"), true);
  assert.equal(await evaluate("!!document.getElementById('cluster-peer-node-id') || !!document.getElementById('config-cluster-enabled')"), false, 'legacy peer editor removed');

  await mode('configured'); await reload();
  await waitFor(loginVisible);
  await evaluate("document.getElementById('webui-login-password').value='synthetic-panel-password'; document.getElementById('webui-login-submit').click()");
  await waitFor(`!(${loginVisible})`);
  await settings();
  await waitFor("!document.getElementById('cluster-create-btn').disabled");
  await evaluate("document.getElementById('config-interval').value='86'; document.getElementById('config-cluster-heartbeat').value='9'");
  await evaluate("document.getElementById('config-cluster-node-id').value='node-001'; document.getElementById('config-cluster-node-name').value='Example Node 001'; document.getElementById('config-cluster-api-url').value='https://node-001.example.com'; document.getElementById('cluster-prepare-join-btn').click()");
  await waitFor(badge('等待加入'));
  assert.match(await evaluate("document.getElementById('cluster-membership-summary').textContent"), /不会自动恢复单机转播/);
  assert.equal(await drafts(), '86/9', 'prepare-join keeps drafts');
  await evaluate("document.getElementById('cluster-settings').scrollIntoView({block:'start'}); window.scrollBy(0,-90)");
  await capture(join(captureDir, 'membership-join-ready.png'));

  await evaluate("document.getElementById('cluster-create-btn').click()");
  await waitFor(`${badge('已加入集群')} && ${memberCount(1)}`);
  assert.equal(await drafts(), '86/9', 'create keeps drafts');
  assert.equal(await evaluate("document.getElementById('config-cluster-node-id').disabled"), true, 'managed identity read-only');

  // A refused target password: inline error, input cleared, nothing tracked.
  await evaluate("document.getElementById('cluster-add-url').value='https://node-002.example.com'; document.getElementById('cluster-add-password').value='synthetic-wrong-password'; document.getElementById('cluster-add-btn').click()");
  await waitFor("document.getElementById('cluster-add-error').textContent.includes('密码错误')");
  assert.equal(await evaluate("document.getElementById('cluster-add-password').value"), '');
  await waitFor(`${noOperation} && !document.getElementById('cluster-add-btn').disabled`);
  assert.equal(await evaluate(memberCount(1)), true);

  await evaluate("document.getElementById('cluster-add-password').value='synthetic-target-password'; document.getElementById('cluster-add-btn').click()");
  assert.equal(await evaluate("document.getElementById('cluster-add-password').value"), '', 'password cleared at submit outcome');
  await waitFor("document.querySelector('#cluster-operation-list .cluster-operation')?.textContent.includes('添加服务器')");
  await waitFor(`${memberCount(2)} && ${noOperation}`);

  // Lost reply, then a reload: the same ID is resumed and never resubmitted.
  await membershipMode('lose-response');
  await evaluate("document.getElementById('cluster-add-url').value='https://node-003.example.com'; document.getElementById('cluster-add-password').value='synthetic-target-password'; document.getElementById('cluster-add-btn').click()");
  await waitFor("(localStorage.getItem('clusterMembershipOperations')||'').includes('node-003')");
  const lostId = await evaluate("JSON.parse(localStorage.getItem('clusterMembershipOperations')).at(-1).operation_id");
  for (let i = 0; i < 50 && !(await (await fetch(`${base}/mock/membership-stats`)).json()).operations.includes(lostId); i++) {
    await new Promise(resolve => setTimeout(resolve, 100));
  }
  await reload(); await settings();
  await waitFor(`document.querySelector('[data-operation-id="${lostId}"]') !== null || ${memberCount(3)}`);
  await waitFor(`${memberCount(3)} && ${noOperation}`);
  assert.equal((await writes()).filter(row => row.path === '/api/cluster/membership/operations' && row.patch.operation_id === lostId).length, 1, 'uncertain add not resubmitted');
  assert.equal(await evaluate("localStorage.getItem('clusterMembershipOperations') === null"), true);

  // Leaving asks the other servers. A missing or still-accepted answer keeps this server joined.
  await waitFor("document.getElementById('cluster-leave-btn').checkVisibility()");
  await membershipMode('leave-unreachable');
  await evaluate("document.getElementById('cluster-leave-btn').click()");
  await waitFor("document.getElementById('cluster-leave-error').textContent.includes('没有确认')");
  assert.equal(await evaluate(badge('已加入集群')), true);
  assert.equal(await evaluate(memberCount(3)), true);
  await membershipMode('leave-present');
  await evaluate("document.getElementById('cluster-leave-btn').click()");
  await waitFor("document.getElementById('cluster-leave-error').textContent.includes('仍承认')");
  assert.equal(await evaluate(badge('已加入集群')), true);
  assert.equal(await evaluate(memberCount(3)), true);
  await membershipMode('ok');
  if (documentCapture) {
    await evaluate(`{
      document.querySelectorAll('#cluster-operation-list button').forEach(button => { if (button.textContent === '关闭') button.click(); });
      document.querySelectorAll('.notification').forEach(node => node.remove());
      const leaveError = document.getElementById('cluster-leave-error');
      if (leaveError) leaveError.textContent = '';
      const url = document.getElementById('cluster-add-url');
      url.value = '';
      document.getElementById('cluster-settings').scrollIntoView({block:'start'});
      window.scrollBy(0, -90);
    }`);
    await waitFor("document.querySelectorAll('#cluster-operation-list .cluster-operation').length===0 && document.querySelectorAll('.notification').length===0 && document.getElementById('cluster-add-url').value===''");
    await documentCapture();
  }

  // A stalled operation shows who must answer and offers Retry for the same ID.
  await evaluate("document.getElementById('config-interval').value='85'; document.getElementById('config-cluster-heartbeat').value='9'");
  await membershipMode('needs-attention');
  await evaluate(`${card('node-002')}.querySelector('.cluster-member-actions button').click()`);
  await evaluate(`{ const c = ${card('node-002')}; c.querySelector('input[data-field=priority]').value='5'; c.querySelector('[data-membership-submit=update]').click(); }`);
  await waitFor("document.querySelector('#cluster-operation-list [data-phase=needs_attention]')?.textContent.includes('node-002')");
  await membershipMode('ok');
  assert.equal(await evaluate("document.getElementById('cluster-add-btn').disabled"), true, 'new operations wait for the pending one');
  assert.equal(await evaluate("[...document.querySelectorAll('#cluster-operation-list button')].some(b => b.textContent==='重试')"), true);
  await evaluate("document.getElementById('cluster-operation-list').scrollIntoView({block:'center'})");
  await capture(join(captureDir, 'membership-retry.png'));
  const retryId = await evaluate("document.querySelector('#cluster-operation-list [data-phase=needs_attention]').dataset.operationId");
  await evaluate("[...document.querySelectorAll('#cluster-operation-list button')].find(b => b.textContent==='重试').click()");
  await waitFor(`${card('node-002')}?.textContent.includes('优先级 5') && ${noOperation}`);
  assert.ok((await writes()).some(row => row.path === `/api/cluster/membership/operations/${retryId}/retry`));
  assert.equal(await drafts(), '85/9', 'operations keep drafts');

  // The public/index node changes through its own operation.
  await evaluate("document.getElementById('public-status-settings').scrollIntoView({block:'center'}); { const s = document.getElementById('config-public-status-node'); s.value='node-002'; s.dispatchEvent(new Event('change')); }");
  await waitFor("!document.getElementById('public-status-role-btn').disabled");
  await evaluate("document.getElementById('public-status-role-btn').click()");
  await waitFor(`${card('node-002')}?.textContent.includes('状态页') && ${noOperation}`);
  const publicWrite = (await writes()).findLast(row => row.patch.kind === 'set_public_node');
  assert.equal(publicWrite.patch.public_member_id, '0'.repeat(63) + '2');

  // Removing the public node requires a replacement or disabling the page.
  await evaluate(`${card('node-002')}.querySelector('.cluster-member-remove').click()`);
  await waitFor(`!!${card('node-002')}.querySelector('.cluster-member-remove-confirm')`);
  assert.match(await evaluate(`${card('node-002')}.querySelector('.cluster-member-remove-confirm').textContent`), /接替/);
  await evaluate(`${card('node-002')}.scrollIntoView({block:'center'})`);
  await capture(join(captureDir, 'membership-remove.png'));
  for (const width of [768, 390, 320]) {
    await command('browsingContext.setViewport', { context, viewport: { width, height: 900 }, devicePixelRatio: 1 });
    assert.equal(await evaluate('document.documentElement.scrollWidth <= innerWidth'), true, `membership overflow at ${width}`);
  }
  await command('browsingContext.setViewport', { context, viewport: { width: 390, height: 844 }, devicePixelRatio: 1 });
  await evaluate("document.getElementById('cluster-settings').scrollIntoView({block:'start'}); window.scrollBy(0,-90)");
  await capture(join(captureDir, 'membership-mobile.png'));
  await command('browsingContext.setViewport', { context, viewport: { width: 1440, height: 1100 }, devicePixelRatio: 1 });
  const removesBefore = (await writes()).filter(row => row.patch.kind === 'remove').length;
  await evaluate(`${card('node-002')}.querySelector('.cluster-member-remove-confirm-btn').click()`);
  assert.equal((await writes()).filter(row => row.patch.kind === 'remove').length, removesBefore, 'replacement is required');
  await membershipMode('cleanup-pending');
  await evaluate(`{ const s = ${card('node-002')}.querySelector('.cluster-member-replacement'); s.value='${'0'.repeat(63)}3'; ${card('node-002')}.querySelector('.cluster-member-remove-confirm-btn').click(); }`);
  await waitFor(`${memberCount(2)} && ${noOperation}`);
  await waitFor("document.getElementById('cluster-operation-list').textContent.includes('等待清理：node-002') && !document.getElementById('cluster-add-btn').disabled");
  assert.equal(await evaluate(`${card('node-003')}.textContent.includes('状态页')`), true);
  await membershipMode('ok');

  // Ordinary settings save after membership changes: topology is rebased, not edited.
  await evaluate("document.getElementById('save-system-config-btn').click()");
  await waitFor("!document.getElementById('save-system-config-btn').disabled");
  const config = JSON.parse(await evaluate("fetch('/api/config').then(r => r.text())"));
  assert.equal(config.cluster.heartbeat_interval_secs, 9);
  assert.equal(config.cluster.peers.length, 1);
  assert.equal(config.interval, 85);

  // Departed cleanup survives a reload without blocking new membership edits.
  await reload(); await settings();
  await waitFor("document.getElementById('cluster-operation-list').textContent.includes('等待清理：node-002') && !document.getElementById('cluster-add-btn').disabled");
  await evaluate("[...document.querySelectorAll('#cluster-operation-list button')].find(b => b.textContent==='重试').click()");
  await waitFor("!document.getElementById('cluster-operation-list').textContent.includes('等待清理：node-002')");

  // Self-removal is forwarded; brief 404s stay pending until this server has left.
  await evaluate(`${card('node-001')}.querySelector('.cluster-member-remove').click()`);
  await evaluate(`${card('node-001')}.querySelector('.cluster-member-remove-confirm-btn').click()`);
  await waitFor(badge('已离开集群'));
  assert.equal(await evaluate("document.getElementById('cluster-member-add').classList.contains('hidden')"), true);
  assert.equal(await evaluate("localStorage.getItem('clusterMembershipOperations') === null"), true);
  await evaluate("document.getElementById('cluster-settings').scrollIntoView({block:'start'}); window.scrollBy(0,-90)");
  await capture(join(captureDir, 'membership-left.png'));

  // Every other server rejects this key: the same panel becomes left and can prepare to join again.
  await membershipMode('restore-managed');
  await reload(); await settings();
  await waitFor(`${badge('已加入集群')} && document.getElementById('cluster-leave-btn').checkVisibility()`);
  await membershipMode('leave-absent');
  await evaluate("document.getElementById('cluster-leave-btn').click()");
  await waitFor(`${badge('已离开集群')} && document.getElementById('cluster-prepare-join-btn').checkVisibility()`);
  assert.equal(await evaluate("document.getElementById('cluster-member-add').classList.contains('hidden')"), true);
  assert.equal(await secretsStored('synthetic-panel-password'), false);

  // The final member uses local dissolution, with no nonexistent coordinator.
  await membershipMode('ok');
  await evaluate("document.getElementById('cluster-create-btn').click()");
  await waitFor(`${badge('已加入集群')} && ${memberCount(1)}`);
  assert.equal(await evaluate("document.getElementById('cluster-leave-btn').checkVisibility()"), false);
  await evaluate(`${card('node-001')}.querySelector('.cluster-member-remove').click()`);
  assert.match(await evaluate(`${card('node-001')}.querySelector('.cluster-member-remove-confirm').textContent`), /结束集群/);
  await evaluate(`${card('node-001')}.querySelector('.cluster-member-remove-confirm-btn').click()`);
  await waitFor(badge('已离开集群'));

  for (const secret of ['synthetic-target-password', 'synthetic-wrong-password', 'synthetic-panel-password']) {
    assert.equal(await secretsStored(secret), false, 'no password in browser storage');
  }
  assert.ok((await writes()).filter(row => row.patch.kind === 'add').every(row => row.patch.target_password === '<sent>'));
  console.log('Compiled membership checks passed: no-password gate, prepare-join, create, add wrong/right password, lost reply + reload resume, leave unreachable/still-accepted/all-rejected, needs-attention retry, update node, public node change, remove with replacement, departed cleanup retry across reload, final-node dissolution, settings save rebase, forwarded self-removal to left, drafts retained, no stored passwords, 1440/768/390/320px.');
}
