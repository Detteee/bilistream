// notify-state.js — pure helpers for the QQ 通知 panel (no DOM access), so
// they can be tested with node --test.

export const DEFAULTS = Object.freeze({
  confirm_secs: 60,
  recovery_hold_secs: 30,
  cooldown_secs: 1800,
  critical_cooldown_secs: 600,
  token_url: 'https://bots.qq.com/app/getAppAccessToken',
  api_base: 'https://api.sgroup.qq.com',
});

const BAD_CREDENTIALS = [100016, 100007, 10004];

// §7 error map: code → [short, fix hint].
export function errorCopy(code, reason = '') {
  const n = Number(code);
  if (BAD_CREDENTIALS.includes(n)) return ['凭据无效', 'AppID 或 AppSecret 无效。请在 QQ 开放平台核对后重新填写 AppSecret。'];
  if (n === 401 || (n >= 11241 && n <= 11244) || n === 11251) return ['鉴权失败', '正在重新获取令牌；如持续出现，请检查 AppSecret。'];
  if (n === 40034105) return ['主动消息不可用', 'QQ 已限制主动推送。管理员给机器人发一条消息后，可在回复窗口内送达。'];
  if (n === 40054013) return ['主动消息已关闭', '管理员在 QQ 中关闭了「允许主动发送」，请重新打开。'];
  if (n === 40054004) return ['未添加机器人', '管理员尚未添加该机器人，请先在 QQ 中添加。'];
  if (n === 40034122 || n === 40034123) return ['召回额度已用完', '本周期的召回消息已用完。管理员发来消息或进入下个周期后恢复。'];
  if (n === 429 || n === 40034100) return ['触发频率限制', '已暂停发送至少 60 秒。'];
  if ([40034006, 40054007, 40054018, 304003, 22006].includes(n)) return ['内容未通过审核', '消息内容被 QQ 拒绝，已改用精简文本。'];
  if (!code) return [reason || '无法连接 QQ', '网络异常，正在重试。'];
  return [reason || 'QQ 返回错误', ''];
}

export function isBadCredentials(status) {
  return Boolean(status?.bad_credentials) || BAD_CREDENTIALS.includes(Number(localOutcome(status)?.code));
}

// This node's own last attempt. `local_last_send` is the new name; older
// backends send it as `last_outcome`.
export function localOutcome(status) {
  return status?.local_last_send ?? status?.last_outcome ?? null;
}

// The outcome the badge and 上次发送 row use: the sender node's last send
// (`sender.last_send`, mode in `via`) when the backend reports it, otherwise
// this node's own. Normalised to { ok, at, mode, code, reason, trace_id, node_id }.
export function senderOutcome(status) {
  if (status && 'sender' in status && status.sender) {
    const last = status.sender.last_send;
    if (!last) return null;
    const local = localOutcome(status);
    const same = local && local.at === last.at && local.node_id === status.sender.node;
    return {
      ok: Boolean(last.ok), at: last.at, mode: last.via ?? last.mode ?? null, code: last.code ?? null,
      reason: last.reason ?? (same ? local.reason : null) ?? null,
      trace_id: last.trace_id ?? (same ? local.trace_id : null) ?? null,
      node_id: typeof status.sender.node === 'string' ? status.sender.node.trim() : '',
      ...(last.status ? { status: last.status } : {}),
    };
  }
  return localOutcome(status);
}

// A local failure worth a detail row: this node failed and that failure is
// not the outcome already shown (it is older than the sender's last send, or
// came from a different node).
export function staleLocalFailure(status) {
  const local = localOutcome(status);
  const shown = senderOutcome(status);
  if (!local || local.ok || isSkipped(local) || shown === local) return null;
  if (shown && shown.at === local.at && shown.node_id === local.node_id) return null;
  return local;
}

// `abcdefgh…wxyz` keeping the start and end, for long opaque ids.
export function middleTruncate(text, max = 16) {
  const value = String(text ?? '');
  if (value.length <= max) return value;
  const head = Math.ceil((max - 1) / 2);
  const tail = Math.floor((max - 1) / 2);
  return `${value.slice(0, head)}…${value.slice(value.length - tail)}`;
}

const UTC8 = new Intl.DateTimeFormat('zh-CN', {
  timeZone: 'Asia/Shanghai', month: '2-digit', day: '2-digit', hour: '2-digit', minute: '2-digit', hour12: false,
});

export function formatUtc8(seconds) {
  if (!seconds) return '—';
  const parts = Object.fromEntries(UTC8.formatToParts(new Date(seconds * 1000)).map(p => [p.type, p.value]));
  return `${parts.month}-${parts.day} ${parts.hour}:${parts.minute}`;
}

export function formatDuration(secs) {
  const s = Math.max(0, Math.floor(secs));
  if (s < 60) return `${s} 秒`;
  const m = Math.floor(s / 60);
  if (m < 60) return `${m} 分钟`;
  const h = Math.floor(m / 60);
  if (h < 24) return `${h} 小时 ${m % 60} 分钟`;
  return `${Math.floor(h / 24)} 天 ${h % 24} 小时`;
}

export function formatCountdown(secs) {
  const s = Math.max(0, Math.floor(secs));
  return `${Math.floor(s / 60)}:${String(s % 60).padStart(2, '0')}`;
}

// Server config → form values (minutes for cooldowns).
export function configToForm(config = {}) {
  const qq = config.qq || {};
  return {
    enabled: Boolean(config.enabled),
    app_id: qq.app_id || '',
    token_url: qq.token_url && qq.token_url !== DEFAULTS.token_url ? qq.token_url : '',
    api_base: qq.api_base && qq.api_base !== DEFAULTS.api_base ? qq.api_base : '',
    confirm_secs: String(config.confirm_secs ?? DEFAULTS.confirm_secs),
    recovery_hold_secs: String(config.recovery_hold_secs ?? DEFAULTS.recovery_hold_secs),
    cooldown_mins: String(Math.round((config.cooldown_secs ?? DEFAULTS.cooldown_secs) / 60)),
    critical_cooldown_mins: String(Math.round((config.critical_cooldown_secs ?? DEFAULTS.critical_cooldown_secs) / 60)),
    commands_enabled: config.commands_enabled !== false,
    watch_enabled: Boolean(config.watch?.enabled),
    // JSON so sameForm can compare it like the other fields.
    watch_channels: JSON.stringify(normalizeWatch(config.watch?.channels || [])),
    // rev 4: an older backend has no key at all → the form has no key either,
    // so it can never be dirty or sent.
    ...(liveEventsSupported(config) ? { live_events_enabled: config.live_events_enabled !== false } : {}),
  };
}

// ---------------------------------------------------------------------------
// 开播通知关注列表

export const WATCH_PLATFORMS = Object.freeze({ youtube: 'YouTube', twitch: 'Twitch', niconico: 'Niconico' });
export const WATCH_MAX = 50;

// Same identity as the server: platform + id, Twitch logins case-insensitive.
export function watchKey(channel) {
  const id = String(channel?.channel_id ?? '').trim();
  return `${channel?.platform}:${channel?.platform === 'twitch' ? id.toLowerCase() : id}`;
}

export function normalizeWatch(list) {
  return (Array.isArray(list) ? list : []).map(c => ({
    platform: String(c.platform || ''),
    channel_id: String(c.channel_id || '').trim(),
    name: String(c.name || '').trim(),
  }));
}

// Mirrors validate_watch in src/notify/config.rs. Returns '' when valid.
export function watchEntryError(channel, list = []) {
  if (!WATCH_PLATFORMS[channel?.platform]) return '请选择平台';
  const id = String(channel.channel_id ?? '').trim();
  if (!id) return '请填写频道 ID';
  if (id.length > 64 || !/^[0-9A-Za-z_.@-]+$/.test(id)) return '频道 ID 只能包含字母、数字和 _ - . @';
  if (String(channel.name ?? '').trim().length > 40) return '名称最多 40 个字';
  if (list.some(c => watchKey(c) === watchKey(channel))) return '已在关注列表中';
  if (list.length >= WATCH_MAX) return `最多关注 ${WATCH_MAX} 个频道`;
  return '';
}

export function addWatch(list, channel) {
  const error = watchEntryError(channel, list);
  if (error) return { list, error };
  return { list: [...list, ...normalizeWatch([channel])], error: '' };
}

export function removeWatch(list, key) {
  return list.filter(c => watchKey(c) !== key);
}

// Picker results: configured channels not yet watched, matching the query
// on name, id or platform.
export function filterCandidates(candidates, query, list, limit = 8) {
  const q = String(query || '').trim().toLowerCase();
  const taken = new Set(list.map(watchKey));
  return normalizeWatch(candidates)
    .filter(c => !taken.has(watchKey(c)))
    .filter(c => !q || [c.name, c.channel_id, c.platform, WATCH_PLATFORMS[c.platform]]
      .some(v => String(v || '').toLowerCase().includes(q)))
    .slice(0, limit);
}

function intOr(value, fallback) {
  const text = String(value ?? '').trim();
  return text === '' ? fallback : Number(text);
}

// Form → partial NotifyConfig patch with only the changed keys.
export function formToPatch(form, baseline) {
  const full = (f) => ({
    enabled: f.enabled,
    confirm_secs: intOr(f.confirm_secs, DEFAULTS.confirm_secs),
    recovery_hold_secs: intOr(f.recovery_hold_secs, DEFAULTS.recovery_hold_secs),
    cooldown_secs: intOr(f.cooldown_mins, DEFAULTS.cooldown_secs / 60) * 60,
    critical_cooldown_secs: intOr(f.critical_cooldown_mins, DEFAULTS.critical_cooldown_secs / 60) * 60,
    qq: {
      app_id: f.app_id.trim(),
      token_url: f.token_url.trim() || DEFAULTS.token_url,
      api_base: f.api_base.trim() || DEFAULTS.api_base,
    },
    commands_enabled: f.commands_enabled !== false,
    ...('live_events_enabled' in f ? { live_events_enabled: f.live_events_enabled !== false } : {}),
    watch: { enabled: Boolean(f.watch_enabled), channels: JSON.parse(f.watch_channels || '[]') },
  });
  const next = full(form);
  const base = baseline ? full(baseline) : {};
  const patch = {};
  for (const [key, value] of Object.entries(next)) {
    if (key === 'qq') {
      const qq = {};
      for (const [k, v] of Object.entries(value)) {
        if (base.qq?.[k] !== v) qq[k] = v;
      }
      if (Object.keys(qq).length) patch.qq = qq;
    } else if (key === 'watch') {
      // Arrays are replaced whole by the merge patch.
      if (JSON.stringify(base.watch) !== JSON.stringify(value)) patch.watch = value;
    } else if (base[key] !== value) {
      patch[key] = value;
    }
  }
  return patch;
}

export function sameForm(a, b) {
  if (!a || !b) return false;
  return Object.keys(a).every(key => a[key] === b[key]);
}

// §10 validation. Returns { field: message } for blocking errors and
// { warnings } for non-blocking ones.
// The admin openid is write-only (§16): it is never part of `form`; the typed
// text comes in as `openidText` and is only checked, never echoed.
export function validateForm(form, { secretConfigured = false, secretEntered = false, openidSet = false, openidText = '', openidPending = false } = {}) {
  const errors = {};
  const warnings = {};
  const appId = form.app_id.trim();
  if (appId && !/^\d{5,20}$/.test(appId)) errors.app_id = 'AppID 应为数字';
  else if (form.enabled && !appId) warnings.app_id = '启用通知前请填写 AppID';
  if (form.enabled && !secretConfigured && !secretEntered) warnings.app_secret = '启用通知前请填写 AppSecret';
  const openid = String(openidText || '').trim();
  if (openid && !/^[0-9A-Za-z]{1,128}$/.test(openid)) errors.admin_openid = 'openid 只能包含字母和数字';
  else if (openid && !/^[0-9A-Fa-f]{32}$/.test(openid)) warnings.admin_openid = 'openid 通常为 32 位十六进制字符，请检查是否有多余字符';
  // §16.x: while the writer's openid is syncing here, nothing is missing.
  else if (form.enabled && !openidSet && !openidPending && !openid) warnings.admin_openid = '启用通知前请绑定管理员 openid';
  const ranges = [
    ['confirm_secs', 0, 600],
    ['recovery_hold_secs', 0, 1800],
    ['cooldown_mins', 1, 1440],
    ['critical_cooldown_mins', 1, 1440],
  ];
  for (const [key, min, max] of ranges) {
    const text = String(form[key] ?? '').trim();
    if (text === '') continue;
    const n = Number(text);
    if (!Number.isInteger(n) || n < min || n > max) errors[key] = `需为 ${min}–${max} 的整数`;
  }
  if (!errors.cooldown_mins && !errors.critical_cooldown_mins
    && intOr(form.critical_cooldown_mins, 10) > intOr(form.cooldown_mins, 30)) {
    warnings.critical_cooldown_mins = '严重事件冷却一般不应长于普通冷却';
  }
  for (const key of ['token_url', 'api_base']) {
    const url = form[key].trim();
    if (url && !/^https:\/\/[^\s/]+/.test(url)) errors.hosts = '请填写 https:// 开头的地址，或留空使用默认值';
  }
  return { errors, warnings };
}

// §5 state selection. `settings` is GET /api/notify data, `status` is
// GET /api/notify/status data.
export function noCapableNode(status) {
  if (status && 'warning' in status) return status.warning === 'no_capable_node';
  return hasNodeCredentials(status) && !status.nodes.some(n => nodeReady(n));
}

export function selectState({ loading = false, loadError = null, settings = null, status = null } = {}) {
  if (loading && !settings) return { key: 'loading', badge: '读取中', tone: '' };
  if (loadError && !settings) return { key: 'load-error', badge: '读取失败', tone: 'unhealthy' };
  const cfg = settings?.config || {};
  const qq = cfg.qq || {};
  const secret = appSecretSet(settings);
  const missing = [];
  if (!qq.app_id) missing.push('AppID');
  if (!secret) missing.push('AppSecret');
  if (!settings?.openid_set && !openidPending(settings, status)) missing.push('管理员 openid');
  if (!cfg.enabled) {
    return missing.length
      ? { key: 'off', badge: '未启用', tone: 'draining', missing }
      : { key: 'off-configured', badge: '已关闭', tone: 'draining', missing };
  }
  if (missing.length) return { key: 'incomplete', badge: '未配置', tone: 'sync-warning', missing };
  const outcome = senderOutcome(status);
  const qqFailure = isBadCredentials(status) || (outcome && !outcome.ok && !isSkipped(outcome));
  if (qqFailure) return { key: 'failed', badge: '发送失败', tone: 'unhealthy', missing };
  // `status.warning` (1a) is the source of truth so the panel and the QQ alert
  // agree; the per-node check is only a fallback when the field is absent.
  if (noCapableNode(status)) return { key: 'no-sender', badge: '无法发送', tone: 'unhealthy', missing };
  // §16.7, only once the backend reports per-node credentials (1b).
  if (hasNodeCredentials(status)) {
    const nodes = status.nodes;
    const self = nodes.find(n => n.is_local);
    // §16.x B1: a pending openid (writer has it, sync on its way) is not
    // missing. Only AppID / AppSecret can make a pending node 未就绪.
    const pending = openidPending(settings, status);
    if (self && (pending ? true : !nodeReady(self))) {
      const lacks = [
        self.appid_set === false && 'AppID',
        !self.appsecret_set && 'AppSecret',
        !pending && !self.openid_set && '管理员 openid',
      ].filter(Boolean);
      if (lacks.length || (!pending && !nodeReady(self))) {
        return { key: 'node-unready', badge: '本节点未就绪', tone: 'sync-warning', missing, lacks };
      }
    }
  }
  if (Number(status?.undelivered) > 0) return { key: 'failed', badge: '发送失败', tone: 'unhealthy', missing };
  if (status?.gateway?.state === 'failed') return { key: 'gateway-failed', badge: '网关已停止', tone: 'unhealthy', missing };
  if (status?.authority === 'no_quorum_fallback') return { key: 'fallback', badge: '无多数派', tone: 'sync-warning', missing };
  if (status?.authority === 'delegate') return { key: 'delegate', badge: '代发', tone: 'sync-warning', missing };
  if (status?.authority === 'primary') {
    if (cfg.qq?.keep_online !== false && status?.gateway?.state !== 'online') {
      return { key: 'gateway-down', badge: '网关未连接', tone: 'sync-warning', missing };
    }
    return { key: 'sender', badge: '已连接', tone: 'active', missing };
  }
  if (status?.authority_node_id) return { key: 'standby', badge: '待命', tone: '', missing };
  return { key: 'electing', badge: '等待选举', tone: 'sync-warning', missing };
}

// 1b feature detection (§16.10). Today's `nodes` entries already exist for
// 可发送节点, so a non-empty array is not enough: the entries must carry the
// boolean credential fields.
export function hasNodeCredentials(status) {
  const nodes = status?.nodes;
  return Array.isArray(nodes) && nodes.length > 0
    && nodes.every(n => typeof n?.openid_set === 'boolean' && typeof n?.appsecret_set === 'boolean');
}

function nodeReady(node) {
  return node.appsecret_set === true && node.openid_set === true && node.notify_capable !== false;
}

// 凭据同步 rows (§16.6/16.9). `pending` is the set of node ids that still had
// the openid when an unbind was confirmed; `localUnbound` is true when this
// node has no openid (after a reload the server may still be clearing peers).
// Each row: { node_id, local, secret: {text, tone}, openid: {text, tone} }.
export function credentialRows(status, { pending = new Set(), localUnbound = false, cleared = new Map(), now = Date.now(), writerSet = undefined } = {}) {
  if (!hasNodeCredentials(status)) return [];
  return status.nodes.map(n => {
    const secret = n.appsecret_set ? { text: 'AppSecret 已同步', tone: 'active' } : { text: 'AppSecret 未设置', tone: 'sync-warning' };
    let openid;
    const unbinding = pending.has(n.node_id) || localUnbound;
    if (n.openid_set && unbinding) {
      openid = n.fresh === false ? { text: '等待节点上线后清除', tone: '' } : { text: '解绑中…', tone: 'sync-warning' };
    } else if (n.openid_set) openid = { text: 'openid 已同步', tone: 'active' };
    else if (cleared.has(n.node_id) && now - cleared.get(n.node_id) < 10_000) openid = { text: '已清除', tone: '' };
    else if (writerSet === true) openid = { text: 'openid 待同步', tone: '' };   // §16.x: on its way
    else if (localUnbound) openid = null;   // whole cluster unbound on purpose: nothing to flag
    else openid = { text: 'openid 未同步', tone: 'sync-warning' };
    return { node_id: n.node_id, local: Boolean(n.is_local), secret, openid };
  });
}

// Single writer (§16.11): only the sender-authority node changes the openid.
// `openid_writer` absent (sparse /status, older backend) means unknown: keep
// the controls enabled and let the server's 409 answer. Null means no writer.
export function openidGate(status) {
  if (!status || !('openid_writer' in status)) return { enabled: true, hint: '' };
  const writer = status.openid_writer;
  if (writer?.is_local) return { enabled: true, hint: handoffHint(status) };
  if (typeof writer?.node_id === 'string' && writer.node_id.trim()) {
    return { enabled: false, owner: writer.node_id.trim(), hint: `请在 ${writer.node_id.trim()} 上绑定或解绑。只有当前发送节点可以修改管理员 openid，修改后会自动同步到本节点。` };
  }
  return { enabled: false, hint: `暂无可写节点。${noWriterReason(status)}` };
}

// gateway_owner is informational and can lag ~30 s after a handoff: a
// mismatch only adds a hint; the server's no_writer refusal decides.
export function isHandoff(status) {
  const w = status?.openid_writer?.node_id;
  const g = status?.gateway_owner?.node_id;
  return Boolean(w && g && w !== g);
}

function handoffHint(status) {
  return isHandoff(status) ? '发送节点切换中，绑定可能暂时失败。' : '';
}

export function noWriterReason(status) {
  if (isHandoff(status)) return '发送节点切换中，请稍后重试。';
  if (status?.authority === 'no_quorum_fallback') return '当前为无多数派代发，暂不能修改绑定。';
  if (status?.authority === 'none' && Array.isArray(status?.nodes) && status.nodes.length > 1) return '集群暂无多数派，暂不能修改绑定。';
  if (status?.worker_running !== false && (status?.enabled === false || status?.credentials_ready === false)) return '请先启用通知并保存 AppID 和 AppSecret。';
  return '请稍后重试。';
}

// One parser for every notify refusal: {success:false, message, data:{code,…}}.
// Returns { kind, owner?, banner: {lead, text, tone}, focus? } or null when the
// error is not a coded refusal (callers fall back to the message).
export function notifyRefusal(error, status = null, { route = 'openid' } = {}) {
  const data = error?.body?.data;
  const code = data && typeof data === 'object' ? data.code : null;
  switch (code) {
    case 'not_owner':
      return { kind: 'not_owner', owner: data.owner || null, banner: { lead: '本节点不是发送节点，未修改管理员 openid。', text: data.owner ? `请在 ${data.owner} 上绑定或解绑。` : '请在当前发送节点上绑定或解绑。', tone: 'warn' } };
    case 'revision_conflict':
      return { kind: 'revision', banner: route === 'config'
        ? { lead: '设置已在其他地方更新，已刷新，请检查后重新保存。', text: '', tone: 'warn' }
        : { lead: '绑定状态已在其他地方更新，已刷新，请重试。', text: '', tone: 'warn' } };
    case 'no_writer':
      return { kind: 'no_writer', banner: { lead: '暂无可写节点。', text: noWriterReason(status), tone: 'warn' } };
    case 'already_bound':
      return { kind: 'already_bound', banner: { lead: '已绑定，请先解除绑定再重新绑定。', text: '', tone: 'warn', action: { label: '去解除绑定', target: 'notify-clear-openid' } } };
    case 'credentials_missing':
      return { kind: 'credentials_missing', banner: { lead: '请先填写并保存 AppID 与 AppSecret。', text: '', tone: 'warn', action: { label: '去填写', target: 'credentials' } } };
    case 'bad_credentials':
      return { kind: 'bad_credentials', banner: { lead: '凭据无效。', text: '请检查 AppID 和 AppSecret 后重新填写。', tone: 'danger', action: { label: '重新填写', target: 'notify-app-secret' } } };
    default:
      return null;
  }
}

// AppSecret flag: `appsecret_set`, with the deprecated name as fallback.
export function appSecretSet(settings) {
  return Boolean(settings?.appsecret_set ?? settings?.app_secret_configured);
}

// Bind box view model from status.bind (+ seconds since it was fetched).
export function bindView(bind, elapsed = 0) {
  if (!bind || bind.state === 'idle') return { hidden: true };
  if (bind.state === 'waiting') {
    const left = Math.max(0, (bind.remaining_secs ?? 0) - elapsed);
    if (left <= 0) return { hidden: false, state: 'expired', code: '', badge: '已过期', tone: 'sync-warning', text: '配对码已过期，请重新点击「自动获取」。', cancel: false };
    return {
      hidden: false,
      state: 'waiting',
      code: bind.code || '',
      badge: `剩余 ${formatCountdown(left)}`,
      tone: '',
      text: bind.mode === 'any'
        ? '在 QQ 向机器人发送任意消息（或此码）'
        : '在 QQ 向机器人发送此码',
      cancel: true,
    };
  }
  if (bind.state === 'bound') {
    return {
      hidden: false,
      state: 'bound',
      code: '',
      badge: '绑定成功',
      tone: 'active',
      text: `已获取管理员 openid${bind.via === 'friend_add' ? '（来自加好友事件）' : ''} · ${formatUtc8(bind.at)}`,
      cancel: false,
    };
  }
  if (bind.state === 'expired') {
    return { hidden: false, state: 'expired', code: '', badge: '已过期', tone: 'sync-warning', text: '配对码已过期，请重新点击「自动获取」。', cancel: false };
  }
  return { hidden: true };
}

// B1: DELETE /api/notify/openid returns only the openid fields; merge them so
// AppID, 启用 and the rest of the settings survive until the GET reload lands.
export function mergeOpenidFields(settings, data) {
  if (!settings) return settings;
  const pick = {};
  for (const k of ['openid_set', 'openid_bound_at', 'openid_revision']) if (data && k in data) pick[k] = data[k];
  if (!('openid_set' in pick)) { pick.openid_set = false; pick.openid_bound_at = null; }
  return { ...settings, ...pick };
}

// B2: a refusal is re-derived from the latest status on every render, and the
// writer refusals drop once the openid controls are enabled again.
// R1: `fresh` says whether `status` was fetched after the refusal. A writer
// refusal is only dropped against fresh status; the status the panel had when
// the 409 arrived is stale by definition.
export function liveRefusal(error, route, status, fresh = true) {
  const r = notifyRefusal(error, status, { route });
  if (!r) return null;
  if (fresh && (r.kind === 'not_owner' || r.kind === 'no_writer') && status && 'openid_writer' in status && openidGate(status).enabled) return null;
  return r;
}

// L1: sequence tokens for overlapping loads. Only the newest request may apply
// its response; an older one landing last is dropped.
export function createSeq() {
  let last = 0;
  return { next: () => ++last, isCurrent: token => token === last, get last() { return last; } };
}

// Keep unsaved edits across a reload: take a server-side change only for
// fields the user hasn't touched (draft still equals the previous baseline).
export function mergeDraft(previous, next, draft, keys) {
  const out = { ...draft };
  for (const key of keys) {
    if (previous?.[key] !== next[key] && draft[key] === previous?.[key]) out[key] = next[key];
  }
  return out;
}

// L3: credential refusals keep their recovery hint until AppID or AppSecret
// actually changes; other refusals clear on any edit.
const CREDENTIAL_REFUSALS = ['bad_credentials', 'credentials_missing'];
export function refusalSurvivesEdit(kind, atRefusal, now) {
  if (!CREDENTIAL_REFUSALS.includes(kind)) return false;
  return (now.app_id || '').trim() === (atRefusal.app_id || '').trim()
    && (now.app_secret || '').trim() === (atRefusal.app_secret || '').trim();
}

// L2: unbind = DELETE (with the revision) → merge the openid fields → reload
// GET /api/notify. Returns the merged settings and the reload promise.
export async function unbindFlow({ settings, remove, reload }) {
  const result = await remove({ expected_openid_revision: settings.openid_revision });
  const merged = mergeOpenidFields(settings, result?.data);
  return { merged, reloaded: reload() };
}

// ---------------------------------------------------------------------------
// §17 本直播间开播/下播 (Addendum A rev 2)

// `skipped_quota` is a held notice, not a failure (§17.4): it never turns the
// badge red, never feeds 错误 / Trace ID and never counts as `ok === false`.
export function isSkipped(outcome) {
  return outcome?.status === 'skipped_quota';
}

// Browser-local `MM-DD HH:mm`, no zone label (§17.3a).
export function formatLocalShort(seconds) {
  const d = new Date(seconds * 1000);
  const p = n => String(n).padStart(2, '0');
  return `${p(d.getMonth() + 1)}-${p(d.getDate())} ${p(d.getHours())}:${p(d.getMinutes())}`;
}

// The read-only budget fields, taken by name only so nothing else (the ledger
// fields in particular) can reach the page. Fresh `/status` `quota` first,
// then the GET /api/notify copy.
const BUDGET_KEYS = ['wakeup_remaining', 'wakeup_reserve', 'wakeup_next_close_at', 'passive_window_open'];
export function pickBudget(status, settings) {
  const out = {};
  for (const source of [status?.quota, settings]) {
    if (!source || typeof source !== 'object') continue;
    for (const key of BUDGET_KEYS) if (!(key in out) && key in source) out[key] = source[key];
  }
  return out;
}

// §17.3 view model. `available` is false when /status is missing or stale.
// Returns { main, tone, next|null, passive }.
// §17.8: `push` is the top-level `active_push` (undefined on an older
// backend, which keeps the §17 rules). With it, the budget is the fallback:
// 「备用：」 prefix, warn and the passive hint only while push is not `ok`.
export function wakeupBudgetView(budget, { available = true, push = undefined } = {}) {
  const remaining = budget?.wakeup_remaining;
  const reserve = budget?.wakeup_reserve;
  const known = available && Number.isInteger(remaining) && Number.isInteger(reserve);
  const hasPush = push !== undefined;
  const prefix = hasPush ? '备用：' : '';
  if (!known) return { main: `${prefix}唤醒额度 —`, tone: 'muted', next: null, passive: false };
  const pushOk = hasPush && push?.last_result === 'ok';
  const low = remaining <= reserve && !pushOk;
  const at = budget.wakeup_next_close_at;
  const windowOpen = budget.passive_window_open === true;
  return {
    main: `${prefix}唤醒额度 ${remaining}/4 · 保留 ${reserve} 用于故障提醒${low ? '，开播/下播只用被动回复' : ''}`,
    tone: low ? 'warn' : 'ok',
    next: Number.isFinite(at) && at > 0 ? ` · 下一档 ${formatLocalShort(at)} 到期` : null,
    passive: hasPush ? windowOpen && pushLimited(push) : windowOpen,
  };
}

export function pushLimited(push) {
  return push?.last_result === 'no_permission' || push?.last_result === 'rate_limited';
}

// RFC3339 (or unix seconds) → browser-local MM-DD HH:mm; '—' when unusable.
export function formatPushTime(value) {
  const ms = typeof value === 'number' ? value * 1000 : Date.parse(value ?? '');
  return Number.isFinite(ms) && ms > 0 ? formatLocalShort(ms / 1000) : '—';
}

// §17.8 主动推送 row. null = no row (field absent, older backend).
// Returns { segments: [text…] (joined with ' · '), tone: '' | 'warn' }.
export function activePushView(status, { available = true } = {}) {
  if (!status || !('active_push' in status)) return null;
  const p = status.active_push;
  if (!available || !p || p.last_result == null) return { segments: ['—'], tone: '' };
  const month = `本月 ${Number(p.month_sent) || 0} 条`;
  switch (p.last_result) {
    case 'ok': return { segments: ['可用', formatPushTime(p.last_at), month], tone: '' };
    case 'no_permission': return { segments: ['受限（无主动消息权限）', `自 ${formatPushTime(p.refused_since)}`], tone: 'warn' };
    case 'rate_limited': return { segments: ['受限（频率限制）', `自 ${formatPushTime(p.refused_since)}`, month], tone: 'warn' };
    default: return { segments: ['—'], tone: '' };
  }
}

export const LIVE_EVENTS_HINT = '占用 QQ 发送额度，故障提醒优先';
export const LIVE_EVENTS_HINT_PUSH = '优先主动推送；被限制时改用回复窗口或唤醒额度，故障提醒优先';

// §17.4 上次发送 text for a skipped notice. The pending note is separate.
export function skippedRow(outcome, localNode = '') {
  if (!isSkipped(outcome)) return null;
  const from = outcome.node_id && outcome.node_id !== localNode ? `（来自 ${outcome.node_id}）` : '';
  return `已跳过：额度不足 · ${formatUtc8(outcome.at)}${from}`;
}

// Addendum D: 待补发 comes only from `pending_live_events` (never from
// last_send). Missing or empty → nothing pending. Unknown codes are ignored.
const PENDING_LABELS = { live_start: '开播', live_end: '下播' };
export function pendingLiveNote(source, senderNode = '', localNode = '') {
  const list = Array.isArray(source?.pending_live_events) ? source.pending_live_events : [];
  const names = ['live_start', 'live_end'].filter(k => list.includes(k)).map(k => PENDING_LABELS[k]);
  if (!names.length) return '';
  const sender = typeof senderNode === 'string' ? senderNode.trim() : '';
  const from = sender && sender !== localNode ? `（来自 ${sender}）` : '';
  return `${names.join('/')}通知待补发，管理员下次给机器人发消息时一并送达。${from}`;
}

// §17.5: the switch follows the writer rules. Absent writer → enabled.
export function liveEventsGate(status) {
  if (!status || !('openid_writer' in status)) return { enabled: true, hint: '' };
  const writer = status.openid_writer;
  if (writer?.is_local) return { enabled: true, hint: '' };
  if (typeof writer?.node_id === 'string' && writer.node_id.trim()) return { enabled: false, hint: `请在 ${writer.node_id.trim()} 上修改。只有当前发送节点可以保存通知设置。` };
  return { enabled: false, hint: `暂无可写节点。${noWriterReason(status)}` };
}

// rev 4: the feature exists only when GET /api/notify's config carries the key.
export function liveEventsSupported(config) {
  return Boolean(config) && typeof config === 'object' && 'live_events_enabled' in config;
}

// readForm helper: add the switch value only when the backend supports it.
export function withLiveEvents(form, supported, checked) {
  if (!supported) return form;
  return { ...form, live_events_enabled: Boolean(checked) };
}

// `invalid_field` naming the switch → inline copy under the row (§17.2).
export function liveEventsFieldError(error) {
  const data = error?.body?.data;
  return data?.code === 'invalid_field' && data.field === 'live_events_enabled'
    ? '开播/下播开关的值无效，请刷新后重试。' : '';
}

// ---------------------------------------------------------------------------
// §16.x Non-sending node state (Addendum C)

// `writer_openid_set` from fresh /status when it carries the key, else from
// GET /api/notify. Absent (no writer, older or stale writer) → undefined.
export function writerOpenidSet(settings, status) {
  if (status && 'writer_openid_set' in status) return status.writer_openid_set;
  if (settings && 'writer_openid_set' in settings) return settings.writer_openid_set;
  return undefined;
}

// This node has no openid yet, but the writer has one: it is on its way.
export function openidPending(settings, status) {
  return Boolean(settings) && !settings.openid_set && writerOpenidSet(settings, status) === true;
}

// Writer name for copy, or '' when unknown (handoff: all three can be null,
// missing or blank at once). Only non-empty trimmed strings count.
export function writerName(status) {
  for (const v of [status?.openid_writer?.node_id, status?.sender?.node, status?.authority_node_id]) {
    if (typeof v === 'string' && v.trim()) return v.trim();
  }
  return '';
}

// Badge + hint for 管理员 openid. Never carries the openid itself.
export function openidView(settings, status) {
  if (!settings) return { badge: '读取中', tone: '', pending: false, hint: null };
  if (settings.openid_set) return { badge: '已绑定', tone: 'active', pending: false, hint: null };
  if (openidPending(settings, status)) {
    const w = writerName(status);
    return w
      ? { badge: `已在 ${w} 绑定 · 待同步`, tone: '', pending: true, hint: `openid 在发送节点 ${w} 上绑定，会自动同步到本节点。` }
      : { badge: '已在发送节点绑定 · 待同步', tone: '', pending: true, hint: 'openid 在发送节点上绑定，会自动同步到本节点。' };
  }
  return { badge: '未绑定', tone: 'sync-warning', pending: false, hint: null };
}

const GAP_COPY = {
  openid_pending: '缺 openid（待同步）', openid: '缺 openid', appsecret: '缺 AppSecret', appid: '缺 AppID', stale: '心跳超时',
};

// Addendum C readiness: every node carries a `missing` array.
export function hasReadiness(status) {
  const nodes = status?.nodes;
  return Array.isArray(nodes) && nodes.length > 0 && nodes.every(n => Array.isArray(n?.missing));
}

// 可发送节点 row from nodes[].missing (fixed codes only). Unknown codes →
// 配置不完整 (once per node). Returns { text, warn } or null.
export function readinessRow(status) {
  if (!hasReadiness(status)) return null;
  const ready = status.nodes.filter(n => n.missing.length === 0);
  const gaps = status.nodes.filter(n => n.missing.length).map(n => {
    const parts = [];
    for (const code of n.missing) {
      const text = GAP_COPY[code] || '配置不完整';
      if (!parts.includes(text)) parts.push(text);
    }
    return `${n.node_id} ${parts.join('、')}`;
  });
  return { count: `${ready.length} / ${status.nodes.length}`, gaps, warn: gaps.length > 0 };
}
