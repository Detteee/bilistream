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
// cred-*: §16.y / Addendum F rev 2, seen from ny (jp is the sender and writer).
// ok (all synced), appid-pending, appsecret-pending, joining (rule 9: all three
// pending, not capable), rebind (AppID changed, openid void on every node),
// offline (ca silent: N3 离线), conflict (PUT → 409 credentials_conflict),
// basegen (PUT → 400 base_generation_required), holder-down (PUT → 503).
// F responses carry only *_set flags, credentials_generation and codes, never
// the AppID, AppSecret or openid value.
export const CRED_MODES = ['cred-ok', 'cred-appid-pending', 'cred-appsecret-pending', 'cred-joining', 'cred-rebind', 'cred-offline', 'cred-conflict', 'cred-basegen', 'cred-holder-down'];
export const WATCH_MODES = ['watch', 'watch-empty', 'watch-missing', 'watch-legacy', 'watch-next-migration', 'watch-other-node'];
export const WATCH_CHANNELS = [
  { name: '星海频道', platforms: ['youtube', 'twitch', 'niconico'] },
  { name: '周末深夜一起听音乐和聊游戏的很长很长的频道名称', platforms: ['youtube', 'twitch'] },
  { name: '晚风 & Friends', platforms: ['youtube'] },
  ...Array.from({ length: 21 }, (_, i) => ({ name: `频道 ${String(i + 4).padStart(2, '0')}`, platforms: i % 2 ? ['youtube'] : ['youtube', 'twitch'] })),
];
const WATCH_LEGACY_CHANNELS = WATCH_CHANNELS.flatMap((c, i) => c.platforms.map(platform => ({
  platform, channel_id: platform === 'youtube' ? `UCexample${i}` : platform === 'twitch' ? `example${i}` : `ch${i}`, name: c.name,
})));
export const NOTIFY_MODES = ['ok', 'low', 'pending-after-alert', 'unknown', 'legacy', ...NY_MODES, ...PUSH_MODES, ...CRED_MODES, ...WATCH_MODES];

export function createNotifyState(now = () => Math.floor(Date.now() / 1000)) {
  const config = {
    enabled: true, confirm_secs: 60, recovery_hold_secs: 30, cooldown_secs: 1800, critical_cooldown_secs: 600,
    qq: { app_id: '1020304050', keep_online: true }, commands_enabled: true, live_events_enabled: true,
    watch: { enabled: false, channels: [] },
  };
  let mode = 'ok';
  let revision = 1;
  const migrationAt = now() - 3600;
  const watch = () => WATCH_MODES.includes(mode);
  const watchFields = () => !watch() ? {} : mode === 'watch-legacy' ? { watch_candidates: WATCH_LEGACY_CHANNELS }
    : { configured_channels: mode === 'watch-empty' ? [] : WATCH_CHANNELS };
  const watchStatus = () => !watch() ? {} : { ...(mode === 'watch-other-node' ? { node_id: 'jp' } : {}), watch: {
    live: mode === 'watch-empty' ? [] : mode === 'watch-legacy' ? ['youtube:UCexample0', 'twitch:historical_login'] : ['youtube:UCexample0'],
    ...(mode === 'watch-legacy' ? {} : { live_entries: mode === 'watch-empty' ? [] : [{ kind: 'configured', name: WATCH_CHANNELS[0].name }] }),
  } };
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
  const settings = () => ({ ...(legacy() ? {} : { pending_live_events: pending() }), revision, config: shownConfig(), secret_revision: 1, openid_revision: 1, appsecret_set: true, openid_set: true, openid_bound_at: now() - 86400, ...budget(), ...watchFields() });
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
  const cred = () => CRED_MODES.includes(mode) || (watch() && mode !== 'watch-legacy');
  const ny = () => NY_MODES.includes(mode) || cred();
  let generation = 7;
  let lastPut = null;
  const credNy = () => {
    switch (mode) {
      case 'cred-appid-pending': return { appid_set: false, notify_capable: false, missing: ['appid_pending'] };
      case 'cred-appsecret-pending': return { appsecret_set: false, notify_capable: false, missing: ['appsecret_pending'] };
      case 'cred-joining': return { appid_set: false, appsecret_set: false, openid_set: false, notify_capable: false, missing: ['appid_pending', 'appsecret_pending', 'openid_pending'] };
      case 'cred-rebind': return { openid_set: false, notify_capable: false, missing: ['openid_rebind'] };
      default: return {};
    }
  };
  const credSettings = base => {
    const { app_id, ...rest } = base.config.qq;   // write-only under F: never echoed
    const n = credNy();
    const has = n.appid_set ?? true;
    // F rev 3 rule 16: appid_set + last-4 hint only.
    const qq = { ...rest, appid_set: has, app_id_hint: has ? String(app_id).slice(-4) : null };
    return {
      ...base, config: { ...base.config, qq }, credentials_generation: generation,
      appid_set: n.appid_set ?? true, appsecret_set: n.appsecret_set ?? true,
      openid_set: n.openid_set ?? true, openid_rebind: mode === 'cred-rebind',
      writer_openid_set: mode !== 'cred-rebind',
      ...(mode === 'cred-rebind' || mode === 'cred-joining' ? { openid_bound_at: null } : {}),
    };
  };
  const nyOpenid = () => !['ny-pending', 'ny-handoff', 'ny-pending-appsecret'].includes(mode);
  const nyNodes = () => {
    const node = (node_id, extra = {}) => ({ node_id, is_local: node_id === 'ny', fresh: true, notify_capable: true,
      appid_set: true, appsecret_set: true, openid_set: true, missing: [], ...extra });
    if (cred()) {
      const ca = mode === 'cred-offline' ? node('ca', { fresh: false, notify_capable: false, missing: ['stale'] })
        : mode === 'cred-rebind' ? node('ca', { openid_set: false, notify_capable: false, missing: ['openid_rebind'] }) : node('ca');
      const jp = mode === 'cred-rebind' ? node('jp', { openid_set: false, notify_capable: false, missing: ['openid_rebind'] }) : node('jp');
      return [jp, node('ny', credNy()), ca].map(n => ({ ...n, credentials_generation: generation - (n.node_id === 'ny' && /pending|joining/.test(mode) ? 1 : 0) }));
    }
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
      undelivered: 0, nodes: nyNodes(), ...(mode === 'ny-old' ? {} : { writer_openid_set: mode !== 'cred-rebind' }),
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
    // Test hook: shape of the last F save (never the values).
    lastPut: () => lastPut,
    setMode(next) {
      if (!NOTIFY_MODES.includes(next)) return false;
      if (watch() || WATCH_MODES.includes(next)) {
        const populated = ['watch', 'watch-next-migration', 'watch-other-node'].includes(next);
        config.watch = { enabled: false, channels: populated ? [
          { kind: 'configured', name: WATCH_CHANNELS[0].name },
          { kind: 'manual', name: '电台夜话', platforms: { youtube: 'UCmanualradio', twitch: 'manualradio' } },
        ] : next === 'watch-missing' ? [{ kind: 'configured', name: '已移除的频道', unresolved: true }]
          : next === 'watch-legacy' ? [WATCH_LEGACY_CHANNELS[0], { platform: 'twitch', channel_id: 'historical_login', name: '旧频道' }] : [],
        ...(next !== 'watch-legacy' ? { migration_notice: populated ? { at: migrationAt + (next === 'watch-next-migration' ? 60 : 0), folded: 2 } : null } : {}) };
      }
      mode = next;
      return true;
    },
    // Returns [httpStatus, body] or null when the path is not a notify route.
    route(method, path, body = {}) {
      if (path === '/api/notify/status' && method === 'GET') {
        const push = activePush();
        return [200, { success: true, data: { ...(ny() ? nyStatus() : status()), ...(push ? { active_push: push } : {}), ...watchStatus() } }];
      }
      if (path !== '/api/notify') return null;
      if (method === 'GET') return [200, { success: true, data: cred() ? credSettings(settings()) : ny() ? nySettings(settings()) : settings() }];
      if (method === 'PUT' && cred()) {
        lastPut = { keys: Object.keys(body).sort(), base_generation: body.base_generation, has_app_secret: 'app_secret' in body, app_id_in_patch: Boolean(body.config?.qq && 'app_id' in body.config.qq) };
        // F rev 2: all-or-nothing; any refusal applies nothing.
        if (mode === 'cred-holder-down') return [503, { success: false, message: 'holder unreachable', data: { code: 'credentials_holder_unreachable' } }];
        if (mode === 'cred-basegen' || body.base_generation == null) return [400, { success: false, message: 'base_generation required', data: { code: 'base_generation_required' } }];
        if (mode === 'cred-conflict' || body.base_generation !== generation) return [409, { success: false, message: 'stale', data: { code: 'credentials_conflict', current_generation: generation + 1 } }];
        if (body.config?.qq && 'app_id' in body.config.qq && !String(body.config.qq.app_id).trim()) return [400, { success: false, message: 'AppID 不能为空', data: { code: 'credentials_invalid' } }];
        if (body.config?.qq?.app_id || body.app_secret) generation += 1;
      }
      if (method === 'PUT') {
        if (body.expected_revision != null && body.expected_revision !== revision) return [409, { success: false, message: '设置已更新', data: { code: 'revision_conflict' } }];
        const patch = body.config || {};
        if (mode === 'watch-legacy' && patch.watch?.channels?.some(c => c.kind)) {
          return [400, { success: false, message: 'unsupported watch entry', data: { code: 'invalid_field', field: 'watch.channels' } }];
        }
        if (watch() && mode !== 'watch-legacy' && Array.isArray(patch.watch?.channels)) {
          const known = mode === 'watch-empty' ? [] : WATCH_CHANNELS;
          const previous = config.watch.channels;
          if (patch.watch.channels.some(c => c.kind !== 'configured' && !previous.some(old => JSON.stringify(old) === JSON.stringify(c)))) {
            return [400, { success: false, message: 'only configured channels may be added', data: { code: 'invalid_field', field: 'watch.channels' } }];
          }
          if (patch.watch.channels.some(c => c.kind === 'configured' && !known.some(k => k.name === c.name) && !previous.some(old => old.kind === 'configured' && old.name === c.name))) {
            return [400, { success: false, message: 'unknown configured channel', data: { code: 'watch_unknown_channel' } }];
          }
        }
        if ('live_events_enabled' in patch && typeof patch.live_events_enabled !== 'boolean') {
          return [400, { success: false, message: 'invalid field', data: { code: 'invalid_field', field: 'live_events_enabled' } }];
        }
        for (const [k, v] of Object.entries(patch)) config[k] = v && typeof v === 'object' && !Array.isArray(v) ? { ...config[k], ...v } : v;
        if (watch() && mode !== 'watch-legacy' && patch.watch) config.watch.migration_notice = null;
        revision += 1;
        return [200, { success: true, data: cred() ? credSettings(settings()) : settings() }];
      }
      return null;
    },
  };
}
