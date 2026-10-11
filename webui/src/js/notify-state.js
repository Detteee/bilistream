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
  if (n === 40054013) return ['管理员已拒收', '管理员关闭了该机器人的主动消息，请在 QQ 中重新允许。'];
  if (n === 40054004) return ['未添加机器人', '管理员尚未添加该机器人，请先在 QQ 中添加。'];
  if (n === 40034122 || n === 40034123) return ['召回额度已用完', '本周期的召回消息已用完，等待管理员发消息或下个周期。'];
  if (n === 429 || n === 40034100) return ['触发频率限制', '已暂停发送至少 60 秒。'];
  if ([40034006, 40054007, 40054018, 304003, 22006].includes(n)) return ['内容被拒', '消息内容被 QQ 拒绝，已改用精简文本。'];
  if (!code) return [reason || '无法连接 QQ', '网络异常，正在重试。'];
  return [reason || 'QQ 返回错误', ''];
}

export function isBadCredentials(status) {
  return Boolean(status?.bad_credentials) || BAD_CREDENTIALS.includes(Number(status?.last_outcome?.code));
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
  if (m < 60) return `${m} 分`;
  const h = Math.floor(m / 60);
  if (h < 24) return `${h} 小时 ${m % 60} 分`;
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
    admin_openid: qq.admin_openid || '',
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
      admin_openid: f.admin_openid.trim(),
      token_url: f.token_url.trim() || DEFAULTS.token_url,
      api_base: f.api_base.trim() || DEFAULTS.api_base,
    },
    commands_enabled: f.commands_enabled !== false,
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
export function validateForm(form, { secretConfigured = false, secretEntered = false } = {}) {
  const errors = {};
  const warnings = {};
  const appId = form.app_id.trim();
  if (appId && !/^\d{5,20}$/.test(appId)) errors.app_id = 'AppID 应为数字';
  else if (form.enabled && !appId) warnings.app_id = '启用通知前请填写 AppID';
  if (form.enabled && !secretConfigured && !secretEntered) warnings.app_secret = '启用通知前请填写 AppSecret';
  const openid = form.admin_openid.trim();
  if (openid && !/^[0-9A-Za-z]{1,128}$/.test(openid)) errors.admin_openid = 'openid 只能包含字母和数字';
  else if (openid && !/^[0-9A-Fa-f]{32}$/.test(openid)) warnings.admin_openid = 'openid 通常是 32 位十六进制，请确认没有多余字符';
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
export function selectState({ loading = false, loadError = null, settings = null, status = null } = {}) {
  if (loading && !settings) return { key: 'loading', badge: '读取中', tone: '' };
  if (loadError && !settings) return { key: 'load-error', badge: '读取失败', tone: 'unhealthy' };
  const cfg = settings?.config || {};
  const qq = cfg.qq || {};
  const secret = Boolean(settings?.app_secret_configured);
  const missing = [];
  if (!qq.app_id) missing.push('AppID');
  if (!secret) missing.push('AppSecret');
  if (!qq.admin_openid) missing.push('管理员 openid');
  if (!cfg.enabled) {
    return missing.length
      ? { key: 'off', badge: '未启用', tone: 'draining', missing }
      : { key: 'off-configured', badge: '已关闭', tone: 'draining', missing };
  }
  if (missing.length) return { key: 'incomplete', badge: '未配置', tone: 'sync-warning', missing };
  const outcome = status?.last_outcome;
  if (isBadCredentials(status) || (outcome && !outcome.ok) || Number(status?.undelivered) > 0) {
    return { key: 'failed', badge: '发送失败', tone: 'unhealthy', missing };
  }
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
