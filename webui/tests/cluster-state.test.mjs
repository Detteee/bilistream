import test from 'node:test';
import assert from 'node:assert/strict';
import { clusterNodeUsable, formatClusterNodeStatus, formatClusterHealthReason, selfCheckDisplay } from '../dist/js/cluster-health.js';

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
