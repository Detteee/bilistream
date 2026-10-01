// Pure projections for managed cluster membership. No DOM, no network.
// Tracked operations hold identifiers and labels only, never a password.

export const TRACKED_OPERATIONS_KEY = 'clusterMembershipOperations';
const MAX_TRACKED = 8;

export const LIFECYCLE_LABELS = {
  standalone: '单机运行',
  incompatible_held: '旧版配置已暂停',
  join_ready: '等待加入',
  pairing: '已被预约加入',
  managed: '已加入集群',
  left: '已离开集群',
};

export function lifecycleSummary(view) {
  switch (view?.lifecycle) {
    case 'standalone':
      return '本服务器单独运行。可以新建集群，或准备让本服务器加入已有集群。';
    case 'incompatible_held':
      return '检测到旧版共享令牌的多服务器配置。旧协议已不再支持，本服务器已暂停转播；请新建集群，或准备加入后由已有成员添加本服务器。';
    case 'join_ready':
      return '本服务器已准备加入：请在集群中任意一台服务器的面板添加本服务器（填写本服务器的地址和面板密码）。加入完成或收到取消通知前，本服务器保持暂停，不会自动恢复单机转播。';
    case 'pairing':
      return '本服务器已被一次添加操作预约，正在等待协调服务器完成。预约不会自动过期；收到完成或取消通知前，本服务器保持暂停。';
    case 'managed': {
      const count = Array.isArray(view.members) ? view.members.length : 0;
      return `已加入集群，共 ${count} 台服务器，成员版本 ${view.revision || 0}。任意成员的面板都可以添加、移除或编辑服务器。`;
    }
    case 'left':
      return '本服务器已离开集群，监控已全部关闭，不会自动恢复单机转播。需要继续使用时，请新建集群或准备重新加入。';
    default:
      return '正在读取多服务器状态…';
  }
}

// Create/join need a saved panel password: enrollment authenticates with it.
export function setupAvailability(view) {
  const lifecycle = view?.lifecycle;
  const allowed = ['standalone', 'incompatible_held', 'left', 'join_ready'].includes(lifecycle);
  if (!allowed) return { create: false, join: false, reason: '' };
  if (!view.password_required) {
    return { create: false, join: false, reason: '请先设置面板密码，其他服务器添加本服务器时需要验证它。' };
  }
  return { create: true, join: lifecycle !== 'join_ready', reason: '' };
}

export function operationUnsettled(op) {
  return !!op && (!op.terminal || (Array.isArray(op.pending_node_ids) && op.pending_node_ids.length > 0));
}

// A new operation started before the previous FINISH reached every member is
// aborted by the servers, so the UI refuses it up front.
// null hides the control. A string disables it; empty means the managed node may leave.
export function leaveBlocked(view, tracked = []) {
  if (view?.lifecycle !== 'managed') return null;
  return newOperationBlock(view, tracked);
}

export function newOperationBlock(view, tracked = []) {
  if (view?.lifecycle !== 'managed') return '本服务器尚未加入集群';
  if (operationUnsettled(view.operation)) return '上一项成员操作尚未在所有服务器完成，请等待完成或重试后再提交新的操作。';
  if (tracked.length) return '上一项成员操作的结果尚未确认，请先查看或重试该操作。';
  return '';
}

const PHASE_LABELS = {
  submitting: '正在提交',
  preparing: '准备中：等待各服务器暂停并确认',
  committing: '提交中：等待各服务器安装新成员列表',
  finishing: '收尾中：通知各服务器恢复',
  aborting: '取消中：通知各服务器释放暂停',
  completed: '已完成',
  aborted: '已取消',
  needs_attention: '需要处理',
  unknown: '结果未确认',
  forwarding: '已交由其他服务器协调',
};

const KIND_LABELS = {
  add: '添加服务器',
  remove: '移除服务器',
  update_node: '编辑服务器',
  set_public_node: '更改状态页节点',
};

export function describeOperation(op, entry = {}) {
  const kind = KIND_LABELS[op?.kind || entry.kind] || '成员操作';
  const phase = op?.phase || entry.phase || 'unknown';
  const pending = Array.isArray(op?.pending_node_ids) ? op.pending_node_ids : [];
  const label = op?.terminal && pending.length && phase === 'completed'
    ? '已完成，仍在通知部分服务器'
    : PHASE_LABELS[phase] || phase;
  return {
    title: entry.label ? `${kind} · ${entry.label}` : kind,
    phase: label,
    attention: pending.length ? `需要在线确认：${pending.join('、')}` : '',
    message: op?.message || entry.message || '',
    coordinator: op?.coordinator_node_id ? `协调服务器：${op.coordinator_node_id}` : '',
    retryable: !!op?.retryable || phase === 'unknown',
    settled: !!op && !operationUnsettled(op),
    success: !!op?.terminal && phase === 'completed',
  };
}

export function removalConsequences(member, context = {}) {
  const lines = [];
  const isLocal = member.member_id && member.member_id === context.localMemberId;
  const isPublic = member.member_id && member.member_id === context.publicMemberId;
  const isActive = member.node_id && member.node_id === context.activeNodeId;
  lines.push('移除期间所有服务器会暂停转播；所有保留的服务器必须在线，并需要原成员中的多数确认。');
  if (isActive) lines.push('它是当前活跃（转播）服务器：移除前会先停止它的转播并确认已停止，完成后由剩余服务器重新选出活跃节点。');
  if (isPublic) lines.push('它负责公开状态页和共享 YouTube 索引：请选择接替的服务器，或关闭状态页。');
  if (isLocal) lines.push('这是本服务器：操作会交由其他成员协调。完成后本服务器关闭全部监控，并且不会自动恢复单机转播。');
  else lines.push('它的集群权限会在所有保留的服务器上撤销，即使它当前离线；它自身的清理结果另行显示。');
  return { lines, needsReplacement: !!isPublic };
}

// Definite refusals left nothing to resume; anything else may have committed.
export function submitFailureIsDefinite(error) {
  return [400, 401, 403, 409, 429, 502].includes(error?.status);
}

export function pollDelay(attempt) {
  return Math.min(15000, Math.round(2000 * 1.5 ** Math.max(0, attempt)));
}

// The departing panel may not know a forwarded self-removal yet.
export function missingOperationIsPending(entry, view, misses) {
  if (entry?.self_removal) return view?.lifecycle !== 'left' && misses < 20;
  return misses < 5;
}

function sanitizeEntry(entry) {
  if (!entry || typeof entry.operation_id !== 'string') return null;
  return {
    operation_id: entry.operation_id,
    kind: typeof entry.kind === 'string' ? entry.kind : '',
    label: typeof entry.label === 'string' ? entry.label.slice(0, 80) : '',
    self_removal: entry.self_removal === true,
    created_at: Number(entry.created_at) || 0,
  };
}

export function loadTrackedOperations(storage) {
  try {
    const parsed = JSON.parse(storage?.getItem(TRACKED_OPERATIONS_KEY) || '[]');
    return Array.isArray(parsed) ? parsed.map(sanitizeEntry).filter(Boolean).slice(-MAX_TRACKED) : [];
  } catch {
    return [];
  }
}

export function saveTrackedOperations(storage, entries) {
  const clean = entries.map(sanitizeEntry).filter(Boolean).slice(-MAX_TRACKED);
  try {
    if (clean.length) storage?.setItem(TRACKED_OPERATIONS_KEY, JSON.stringify(clean));
    else storage?.removeItem(TRACKED_OPERATIONS_KEY);
  } catch { /* storage is optional; polling still works in this page */ }
  return clean;
}

export function newOperationId(cryptoImpl = globalThis.crypto) {
  return cryptoImpl.randomUUID();
}
