// QQ 通知 mock (/api/notify, /api/notify/status) with the Addendum A rev 2
// fields. Modes: ok (budget above reserve, passive window open), low (budget at
// reserve, 下播 skipped_quota with 下播 pending), pending-after-alert (a later
// failover alert went out by wakeup; 下播 is still pending), unknown (no budget fields).
// The ledger (last_admin_msg_id, last_admin_at, passive_replies_used) and the
// admin openid are never part of any response.
// legacy: a backend older than §17 — no live_events_enabled and none of the new fields.
export const NOTIFY_MODES = ['ok', 'low', 'pending-after-alert', 'unknown', 'legacy'];

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
  return {
    setMode(next) { if (!NOTIFY_MODES.includes(next)) return false; mode = next; return true; },
    // Returns [httpStatus, body] or null when the path is not a notify route.
    route(method, path, body = {}) {
      if (path === '/api/notify/status' && method === 'GET') return [200, { success: true, data: status() }];
      if (path !== '/api/notify') return null;
      if (method === 'GET') return [200, { success: true, data: settings() }];
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
