// QQ 通知 mock (/api/notify, /api/notify/status) with the Addendum A rev 2
// fields. Modes: ok (budget above reserve, passive window open), low (budget at
// reserve, 下播 skipped_quota with 下播 pending), pending-after-alert (a later
// failover alert went out by wakeup; 下播 is still pending), unknown (no budget fields).
// The ledger (last_admin_msg_id, last_admin_at, passive_replies_used) and the
// admin openid are never part of any response.
// legacy: a backend older than §17 — no live_events_enabled and none of the new fields.
// ny-*: seen from ny, a non-sending node, with jp as the writer (§16.x,
// Addendum C): bound, pending (jp bound, ny not yet), appsecret (ca lacks
// its AppSecret), stale (ca heartbeat timed out), old (no readiness fields).
// ny-handoff: mid gateway handoff — openid_writer, sender and authority_node_id
// are all null while ny is still pending.
// ny-pending-appsecret: ny pending and also missing its own AppSecret.
export const NY_MODES = ['ny-bound', 'ny-pending', 'ny-appsecret', 'ny-stale', 'ny-old', 'ny-handoff', 'ny-pending-appsecret'];
// push-*: Addendum E rev 2 top-level active_push on /status (low budget so the
// warn rule is visible): ok, no_permission, rate_limited, null (no result yet).
export const PUSH_MODES = ['push-ok', 'push-no-permission', 'push-rate-limited', 'push-null'];
export const NOTIFY_MODES = ['ok', 'low', 'pending-after-alert', 'unknown', 'legacy', ...NY_MODES, ...PUSH_MODES];

export function createNotifyState(now = () => Math.floor(Date.now() / 1000)) {
  const config = {
    enabled: true, confirm_secs: 60, recovery_hold_secs: 30, cooldown_secs: 1800, critical_cooldown_secs: 600,
    qq: { app_id: '1020304050', keep_online: true }, commands_enabled: true, live_events_enabled: true,
    watch: { enabled: false, channels: [] },
  };
  let mode = 'ok';
  let revision = 1;
  const budget = () => {
    const t = now();
    if (mode === 'unknown' || mode === 'legacy') return {};
    if (mode === 'pending-after-alert') return { wakeup_remaining: 1, wakeup_reserve: 2, wakeup_next_close_at: t + 3 * 86400, passive_window_open: false };
    if (mode.startsWith('push-')) return { wakeup_remaining: 2, wakeup_reserve: 2, wakeup_next_close_at: t + 2 * 86400, passive_window_open: true };
    if (mode === 'low') return { wakeup_remaining: 2, wakeup_reserve: 2, wakeup_next_close_at: t + 2 * 86400, passive_window_open: false };
    return { wakeup_remaining: 3, wakeup_reserve: 2, wakeup_next_close_at: t + 6 * 3600, passive_window_open: true };
  };
  const pending = () => (mode === 'low' || mode === 'pending-after-alert' ? ['live_end'] : []);
  const legacy = () => mode === 'legacy';
  const shownConfig = () => { if (!legacy()) return config; const { live_events_enabled, ...rest } = config; return rest; };
  const settings = () => ({ ...(legacy() ? {} : { pending_live_events: pending() }), revision, config: shownConfig(), secret_revision: 1, openid_revision: 1, appsecret_set: true, openid_set: true, openid_bound_at: now() - 86400, ...budget() });
  const status = () => {
    const t = now();
    const last = mode === 'low' ? { ok: false, status: 'skipped_quota', at: t - 600 }
      : mode === 'pending-after-alert' ? { ok: true, via: 'wakeup', at: t - 120 }
      : { ok: true, via: 'passive', at: t - 300 };
    return {
      node_id: 'jp', authority: 'primary', authority_node_id: 'jp', gateway: { state: 'online', since: t - 7200 },
      openid_writer: { node_id: 'jp', is_local: true },
      sender: { node: 'jp', last_send: last }, local_last_send: { ...last, node_id: 'jp' },
      undelivered: 0, ...(legacy() ? {} : { pending_live_events: pending() }), quota: { wakeups_left: budget().wakeup_remaining ?? null, ...budget() },
    };
  };
  const ny = () => NY_MODES.includes(mode);
  const nyOpenid = () => !['ny-pending', 'ny-handoff', 'ny-pending-appsecret'].includes(mode);
  const nyNodes = () => {
    const node = (node_id, extra = {}) => ({ node_id, is_local: node_id === 'ny', fresh: true, notify_capable: true,
      appid_set: true, appsecret_set: true, openid_set: true, missing: [], ...extra });
    if (mode === 'ny-old') return [{ node_id: 'jp', notify_capable: true }, { node_id: 'ny', is_local: true, notify_capable: true }, { node_id: 'ca', notify_capable: true }];
    const ca = mode === 'ny-appsecret' ? node('ca', { appsecret_set: false, notify_capable: false, missing: ['appsecret'] })
      : mode === 'ny-stale' ? node('ca', { fresh: false, notify_capable: false, missing: ['stale'] }) : node('ca');
    const nyNode = mode === 'ny-pending-appsecret' ? node('ny', { openid_set: false, appsecret_set: false, notify_capable: false, missing: ['appsecret', 'openid_pending'] })
      : mode === 'ny-pending' || mode === 'ny-handoff' ? node('ny', { openid_set: false, notify_capable: false, missing: ['openid_pending'] }) : node('ny');
    return [node('jp'), nyNode, ca];
  };
  const nySettings = base => ({ ...base, openid_set: nyOpenid(), ...(mode === 'ny-pending-appsecret' ? { appsecret_set: false } : {}), ...(mode === 'ny-old' ? {} : { writer_openid_set: true }) });
  const nyStatus = () => {
    const t = now();
    return {
      node_id: 'ny', authority: 'none', authority_node_id: 'jp', gateway: {},
      openid_writer: { node_id: 'jp', is_local: false }, gateway_owner: { node_id: 'jp' },
      sender: { node: 'jp', last_send: { ok: true, via: 'passive', at: t - 300 } }, local_last_send: null,
      undelivered: 0, nodes: nyNodes(), ...(mode === 'ny-old' ? {} : { writer_openid_set: true }),
      pending_live_events: [], quota: budget(),
      ...(mode === 'ny-handoff' ? { authority_node_id: null, openid_writer: null, gateway_owner: null, sender: null } : {}),
    };
  };
  const activePush = () => {
    const iso = secs => new Date(secs * 1000).toISOString();
    const t = now();
    switch (mode) {
      case 'push-ok': return { last_result: 'ok', last_at: iso(t - 240), refused_since: null, month_sent: 37 };
      case 'push-no-permission': return { last_result: 'no_permission', last_at: iso(t - 3 * 86400), refused_since: iso(t - 2 * 86400), month_sent: 12 };
      case 'push-rate-limited': return { last_result: 'rate_limited', last_at: iso(t - 3600), refused_since: iso(t - 1800), month_sent: 998 };
      case 'push-null': return { last_result: null, last_at: null, refused_since: null, month_sent: 0 };
      default: return undefined;
    }
  };
  return {
    setMode(next) { if (!NOTIFY_MODES.includes(next)) return false; mode = next; return true; },
    // Returns [httpStatus, body] or null when the path is not a notify route.
    route(method, path, body = {}) {
      if (path === '/api/notify/status' && method === 'GET') {
        const push = activePush();
        return [200, { success: true, data: { ...(ny() ? nyStatus() : status()), ...(push ? { active_push: push } : {}) } }];
      }
      if (path !== '/api/notify') return null;
      if (method === 'GET') return [200, { success: true, data: ny() ? nySettings(settings()) : settings() }];
      if (method === 'PUT') {
        if (body.expected_revision != null && body.expected_revision !== revision) return [409, { success: false, message: '设置已更新', data: { code: 'revision_conflict' } }];
        const patch = body.config || {};
        if ('live_events_enabled' in patch && typeof patch.live_events_enabled !== 'boolean') {
          return [400, { success: false, message: 'invalid field', data: { code: 'invalid_field', field: 'live_events_enabled' } }];
        }
        for (const [k, v] of Object.entries(patch)) config[k] = v && typeof v === 'object' && !Array.isArray(v) ? { ...config[k], ...v } : v;
        revision += 1;
        return [200, { success: true, data: settings() }];
      }
      return null;
    },
  };
}
