import test from 'node:test';
import assert from 'node:assert/strict';
import { clusterNodeUsable, formatClusterNodeStatus, formatClusterHealthReason, selfCheckDisplay, ytIndexChip, ytIndexPeerLine } from '../dist/js/cluster-health.js';

test('maintenance intent takes precedence while automatic faults remain faults on both surfaces', () => {
  for (const node of [
    { role: 'unhealthy', network_unstable: true, health: { healthy: false, reason: 'ffmpeg_repeated_failures' } },
    { role: 'unhealthy', healthy: false },
  ]) {
    assert.equal(formatClusterNodeStatus(node), '故障');
    assert.equal(clusterNodeUsable(node), false);
    assert.equal(formatClusterNodeStatus({ ...node, role: 'draining' }), '维护');
  }
  assert.equal(formatClusterNodeStatus({ role: 'standby', health: { healthy: true } }), '备用');
  assert.equal(formatClusterNodeStatus({ role: 'active', healthy: true, ffmpeg_running: false }), '活跃');
});

test('first heartbeat waiting is distinct from a failed node, including the public projection', () => {
  assert.equal(formatClusterNodeStatus({ role: 'unhealthy', health: { healthy: false, reason: 'waiting_for_heartbeat' } }), '等待');
  assert.equal(formatClusterNodeStatus({ role: 'unhealthy', healthy: false, waiting_for_heartbeat: true }), '等待');
  assert.equal(formatClusterNodeStatus({ role: 'draining', healthy: false, waiting_for_heartbeat: true }), '维护');
  assert.equal(formatClusterHealthReason('stream_metrics_degraded'), '推流指标异常');
  assert.equal(formatClusterHealthReason('draining'), '操作员维护，暂停接管');
});

test('stale node heartbeats cannot keep displaying a successful tunnel check', () => {
  const check = { state: 'healthy', latency_ms: 24 };
  assert.equal(selfCheckDisplay(check).state, 'healthy');
  assert.equal(selfCheckDisplay(check, true).state, 'pending');
  assert.equal(selfCheckDisplay(check, true).label, '已过期');
});

test('the YouTube index chip names the index node and warns when it cannot serve', () => {
  const index = { yt_index: { state: 'index' }, health: { stale: false } };
  assert.deepEqual(
    { ...ytIndexChip(index), title: undefined },
    { text: 'YT 索引', title: undefined, warn: false, websub: null },
  );
  const subscribed = { ...index, websub: { verified: 36, pending: 1, failed: 0 } };
  assert.equal(ytIndexChip(subscribed).websub, 'warn');
  assert.match(ytIndexChip(subscribed).title, /已验证 36 · 等待 1 · 失败 0/);
  assert.equal(ytIndexChip({ ...index, websub: { verified: 37, pending: 0, failed: 0 } }).websub, 'ok');

  const noKey = { yt_index: { state: 'local', reason: 'no_key' }, health: { stale: false } };
  assert.equal(ytIndexChip(noKey).text, 'YT 索引 ⚠');
  assert.equal(ytIndexChip(noKey).warn, true);
  assert.match(ytIndexChip(noKey).title, /没有可用的 YouTube key/);
  assert.match(ytIndexChip({ ...noKey, yt_index: { state: 'local', reason: 'budget_spent' } }).title, /配额已用完/);

  assert.equal(ytIndexChip({ ...index, health: { stale: true } }), null, 'offline already shows');
  assert.equal(ytIndexChip({ yt_index: { state: 'local', reason: 'index_down' } }), null);
  assert.equal(ytIndexChip({}), null, 'old node');
});

test('other nodes say whose index they follow', () => {
  assert.equal(ytIndexPeerLine({ yt_index: { state: 'follows', node: 'ny' } }), '跟随 ny 的 YouTube 索引');
  assert.equal(
    ytIndexPeerLine({ yt_index: { state: 'local', reason: 'index_down' } }),
    'YouTube 本地查询（索引节点不可用）',
  );
  assert.equal(ytIndexPeerLine({ yt_index: { state: 'index' } }), null);
  assert.equal(ytIndexPeerLine({}), null);
});
