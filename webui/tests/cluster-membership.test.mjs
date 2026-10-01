import test from 'node:test';
import assert from 'node:assert/strict';
import {
  TRACKED_OPERATIONS_KEY,
  describeOperation,
  lifecycleSummary,
  leaveBlocked,
  loadTrackedOperations,
  missingOperationIsPending,
  newOperationBlock,
  operationUnsettled,
  pollDelay,
  removalConsequences,
  saveTrackedOperations,
  setupAvailability,
  submitFailureIsDefinite,
} from '../src/js/cluster-membership-state.js';

function memoryStorage() {
  const values = new Map();
  return {
    getItem: key => values.get(key) ?? null,
    setItem: (key, value) => values.set(key, String(value)),
    removeItem: key => values.delete(key),
    dump: () => JSON.stringify([...values]),
  };
}

const status = overrides => ({ operation_id: 'a', kind: 'add', phase: 'preparing', coordinator_node_id: 'node-001', message: null, pending_node_ids: [], terminal: false, retryable: true, ...overrides });

test('create and join need a saved panel password and an unjoined lifecycle', () => {
  assert.deepEqual(setupAvailability({ lifecycle: 'standalone', password_required: true }), { create: true, join: true, reason: '' });
  const noPassword = setupAvailability({ lifecycle: 'standalone', password_required: false });
  assert.equal(noPassword.create || noPassword.join, false);
  assert.match(noPassword.reason, /面板密码/);
  for (const lifecycle of ['incompatible_held', 'left']) {
    assert.equal(setupAvailability({ lifecycle, password_required: true }).create, true, lifecycle);
  }
  assert.deepEqual(setupAvailability({ lifecycle: 'join_ready', password_required: true }), { create: true, join: false, reason: '' });
  for (const lifecycle of ['managed', 'pairing']) {
    assert.deepEqual(setupAvailability({ lifecycle, password_required: true }), { create: false, join: false, reason: '' });
  }
});

test('held lifecycles explain that they stay paused and do not expire', () => {
  assert.match(lifecycleSummary({ lifecycle: 'incompatible_held' }), /旧版共享令牌.*暂停/);
  assert.match(lifecycleSummary({ lifecycle: 'join_ready' }), /不会自动恢复单机转播/);
  assert.match(lifecycleSummary({ lifecycle: 'pairing' }), /不会自动过期/);
  assert.match(lifecycleSummary({ lifecycle: 'left' }), /不会自动恢复单机转播/);
  assert.match(lifecycleSummary({ lifecycle: 'managed', members: [{}, {}], revision: 4 }), /2 台.*版本 4/);
});

test('leave is offered only while this server is managed and idle', () => {
  const managed = { lifecycle: 'managed', revision: 3 };
  assert.equal(leaveBlocked(managed), '');
  assert.equal(leaveBlocked({ lifecycle: 'left' }), null);
  assert.equal(leaveBlocked({ lifecycle: 'join_ready' }), null);
  assert.equal(leaveBlocked({ lifecycle: 'standalone' }), null);
  assert.match(leaveBlocked({ ...managed, operation: status() }), /尚未在所有服务器完成/);
  assert.match(leaveBlocked(managed, [{ operation_id: 'pending' }]), /结果尚未确认/);
});

test('a new operation waits until the previous one reached every member', () => {
  const managed = { lifecycle: 'managed', revision: 3 };
  assert.equal(newOperationBlock(managed), '');
  assert.match(newOperationBlock({ ...managed, operation: status() }), /尚未在所有服务器完成/);
  const finishedButUndelivered = status({ phase: 'completed', terminal: true, pending_node_ids: ['node-003'] });
  assert.equal(operationUnsettled(finishedButUndelivered), true);
  assert.match(newOperationBlock({ ...managed, operation: finishedButUndelivered }), /尚未在所有服务器完成/);
  assert.equal(newOperationBlock({ ...managed, operation: status({ phase: 'completed', terminal: true }) }), '');
  assert.match(newOperationBlock(managed, [{ operation_id: 'b' }]), /结果尚未确认/);
  assert.match(newOperationBlock({ lifecycle: 'join_ready' }), /尚未加入/);
});

test('operation descriptions name the servers that need attention, not a manager role', () => {
  const info = describeOperation(status({ phase: 'needs_attention', pending_node_ids: ['node-002', 'node-003'], message: '无法连接' }), { label: 'Example Node 002' });
  assert.equal(info.title, '添加服务器 · Example Node 002');
  assert.equal(info.phase, '需要处理');
  assert.equal(info.attention, '需要在线确认：node-002、node-003');
  assert.equal(info.coordinator, '协调服务器：node-001');
  assert.equal(info.retryable, true);
  assert.equal(info.success, false);
  const done = describeOperation(status({ phase: 'completed', terminal: true, retryable: false }));
  assert.equal(done.success, true);
  assert.equal(done.settled, true);
  assert.equal(describeOperation(status({ phase: 'completed', terminal: true, pending_node_ids: ['node-003'] })).phase, '已完成，仍在通知部分服务器');
  assert.equal(describeOperation(status({ phase: 'aborted', terminal: true })).success, false);
});

test('removal consequences cover active, public/index and local servers', () => {
  const context = { localMemberId: 'm1', publicMemberId: 'm2', activeNodeId: 'node-002' };
  const remote = removalConsequences({ member_id: 'm2', node_id: 'node-002' }, context);
  assert.equal(remote.needsReplacement, true);
  assert.ok(remote.lines.some(line => line.includes('活跃')));
  assert.ok(remote.lines.some(line => line.includes('接替')));
  assert.ok(remote.lines.some(line => line.includes('即使它当前离线')));
  const local = removalConsequences({ member_id: 'm1', node_id: 'node-001' }, context);
  assert.equal(local.needsReplacement, false);
  assert.ok(local.lines.some(line => line.includes('交由其他成员协调')));
  assert.ok(local.lines.every(line => !line.includes('活跃（转播）')));
});

test('only definite refusals may forget an operation ID', () => {
  for (const code of [400, 401, 403, 409, 429, 502]) assert.equal(submitFailureIsDefinite({ status: code }), true, String(code));
  for (const error of [{ status: 500 }, { status: 503 }, { status: 504 }, new Error('network'), {}]) {
    assert.equal(submitFailureIsDefinite(error), false);
  }
});

test('a forwarded self-removal stays pending through 404 until this server has left', () => {
  const self = { self_removal: true };
  assert.equal(missingOperationIsPending(self, { lifecycle: 'managed' }, 10), true);
  assert.equal(missingOperationIsPending(self, { lifecycle: 'left' }, 1), false);
  assert.equal(missingOperationIsPending(self, { lifecycle: 'managed' }, 20), false, 'bounded');
  assert.equal(missingOperationIsPending({}, { lifecycle: 'managed' }, 4), true);
  assert.equal(missingOperationIsPending({}, { lifecycle: 'managed' }, 5), false);
  assert.equal(pollDelay(0), 2000);
  assert.ok(pollDelay(3) > pollDelay(1));
  assert.equal(pollDelay(50), 15000);
});

test('tracked operations persist identifiers only and survive a reload', () => {
  const storage = memoryStorage();
  saveTrackedOperations(storage, [{ operation_id: '0b6f0f7e-0000-4000-8000-000000000001', kind: 'add', label: 'https://node-002.example.com', target_password: 'synthetic-target-password', created_at: 5 }]);
  assert.equal(storage.dump().includes('synthetic-target-password'), false);
  const loaded = loadTrackedOperations(storage);
  assert.deepEqual(loaded, [{ operation_id: '0b6f0f7e-0000-4000-8000-000000000001', kind: 'add', label: 'https://node-002.example.com', self_removal: false, created_at: 5 }]);
  saveTrackedOperations(storage, []);
  assert.equal(storage.getItem(TRACKED_OPERATIONS_KEY), null);
  storage.setItem(TRACKED_OPERATIONS_KEY, '{not json');
  assert.deepEqual(loadTrackedOperations(storage), []);
  const many = Array.from({ length: 12 }, (_, i) => ({ operation_id: `id-${i}` }));
  assert.equal(saveTrackedOperations(storage, many).length, 8);
});
