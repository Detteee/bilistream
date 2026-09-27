// Wire-state translations shared by admin and public views. Maintenance is
// operator intent; automatic faults use unhealthy even while fault-latched.
export function clusterNodeUsable(node) {
  return !(node.draining || node.role === 'draining' || node.network_unstable
    || node.role === 'unhealthy' || node.health?.healthy === false || node.healthy === false);
}

export function formatClusterRole(role) {
  return { active: '活跃', standby: '备用', draining: '维护', unhealthy: '故障' }[role] || '未知';
}

export function formatClusterNodeStatus(node) {
  if (node.draining || node.role === 'draining') return '维护';
  if (node.health?.reason === 'waiting_for_heartbeat' || node.waiting_for_heartbeat) return '等待';
  if (!clusterNodeUsable(node)) return '故障';
  return formatClusterRole(node.role);
}

export function formatClusterHealthReason(reason) {
  return {
    healthy: '健康',
    network_isolated: '网络隔离',
    heartbeat_timeout: '心跳超时',
    waiting_for_heartbeat: '等待心跳',
    api_unreachable: '节点 API 不可达',
    external_api_unreachable: '外部 API 不可达',
    ffmpeg_repeated_failures: '推流反复失败',
    node_fault_latched: '故障锁定',
    stream_metrics_degraded: '推流指标异常',
    draining: '操作员维护，暂停接管',
    network_unstable: '节点故障锁定',
  }[reason] || reason || '-';
}

// Reachability through the node's advertised control URL, including its tunnel.
export function selfCheckDisplay(check, stale = false) {
  if (stale) {
    return { state: 'pending', label: '已过期', title: '节点心跳已过期，等待新的 API 自检结果' };
  }
  switch (check?.state) {
    case 'healthy': {
      const latency = Number.isFinite(check.latency_ms) && check.latency_ms >= 0
        ? `${check.latency_ms} ms` : '—';
      return { state: 'healthy', label: latency, title: 'API 自检：本节点对外 URL 可达，已验证节点身份和即时响应；延迟为往返耗时' };
    }
    case 'failing':
      return { state: 'failing', label: '检测失败', title: `API 自检失败：${selfCheckFailureMessage(check.failure)}` };
    case 'unreachable':
      return { state: 'unreachable', label: '自检不通', title: `本节点访问自身对外 URL 连续失败；${selfCheckFailureMessage(check.failure)}。此路径与浏览器访问、节点心跳独立，可能同时存在心跳` };
    default:
      return { state: 'pending', label: '待检测', title: '等待本节点通过对外 URL 完成 API 自检' };
  }
}

function selfCheckFailureMessage(failure) {
  switch (failure?.kind) {
    case 'timed_out': return '隧道访问超时';
    case 'http_status': return `隧道返回 HTTP ${failure.detail}`;
    case 'invalid_url': return '节点对外 URL 配置无效';
    case 'identity_mismatch': return '对外 URL 指向了其他节点';
    case 'challenge_mismatch': return '收到了旧的或被缓存的响应';
    case 'invalid_response': return '隧道未返回有效的节点响应';
    case 'body_too_large': return '隧道返回的响应过大';
    case 'rejected': return '节点拒绝了自检请求';
    default: return '无法通过对外 URL 访问本节点';
  }
}

const YT_INDEX_REASONS = {
  no_key: '没有可用的 YouTube key',
  budget_spent: '今日 YouTube 配额已用完',
};

// The public-status node's YouTube index chip: { text, title, warn, websub }
// or null. websub is 'ok' / 'warn' while it subscribes, else null. An offline
// node already reads as offline, so it gets no chip.
export function ytIndexChip(node) {
  const state = node?.yt_index;
  if (!state || node.health?.stale) return null;
  if (state.state === 'index') {
    const websub = node.websub;
    const lines = ['YouTube 索引：为集群提供 YouTube 查询'];
    if (websub) {
      lines.push(`WebSub 订阅：已验证 ${websub.verified} · 等待 ${websub.pending} · 失败 ${websub.failed}`);
    }
    return {
      text: 'YT',
      title: lines.join('\n'),
      warn: false,
      websub: websub ? (websub.pending || websub.failed ? 'warn' : 'ok') : null,
    };
  }
  const reason = YT_INDEX_REASONS[state.state === 'local' ? state.reason : ''];
  if (!reason) return null;
  return {
    text: 'YT ⚠',
    title: `YouTube 索引：${reason}，未为集群提供 YouTube 查询\n其他节点改用各自的 key；没有 key 的节点只用 Holodex`,
    warn: true,
    websub: null,
  };
}

// Quiet title-row caption on other nodes' cards, or null.
export function ytIndexPeerLine(node) {
  const state = node?.yt_index;
  if (!state || node.health?.stale) return null;
  if (state.state === 'follows') return `跟随 ${state.node} 的 YouTube 索引`;
  if (state.state === 'local' && state.reason === 'index_down') return 'YouTube 本地查询（索引节点不可用）';
  return null;
}
