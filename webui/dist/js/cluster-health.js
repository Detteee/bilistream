// Reachability through the node's advertised control URL, including its tunnel.
export function selfCheckDisplay(check, stale = false) {
  if (stale) {
    return { state: 'pending', label: '已过期', title: '节点心跳已过期，等待新的隧道自检结果' };
  }
  switch (check?.state) {
    case 'healthy': {
      const latency = Number.isFinite(check.latency_ms) && check.latency_ms >= 0
        ? ` · ${check.latency_ms} ms` : '';
      return { state: 'healthy', label: `可达${latency}`, title: '已通过本节点对外 URL 验证节点身份和即时响应' };
    }
    case 'failing':
      return { state: 'failing', label: '检测失败', title: selfCheckFailureMessage(check.failure) };
    case 'unreachable':
      return { state: 'unreachable', label: '不可达', title: `连续自检失败；${selfCheckFailureMessage(check.failure)}` };
    default:
      return { state: 'pending', label: '待检测', title: '等待本节点通过对外 URL 完成隧道自检' };
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
