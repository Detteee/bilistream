// Channel-name watch-list state: every saved entry names a configured channel,
// or is a retained manual entry the operator can remove. No DOM access.

export const WATCH_PLATFORMS = Object.freeze({ youtube: 'YouTube', twitch: 'Twitch', niconico: 'Niconico' });
export const WATCH_MAX = 50;
const platforms = Object.keys(WATCH_PLATFORMS);
const text = value => String(value ?? '').trim();

export function watchKey(channel) {
  if (channel?.kind === 'configured' || channel?.kind === 'manual') return `${channel.kind}:${text(channel.name)}`;
  const id = text(channel?.channel_id);
  return `${channel?.platform}:${channel?.platform === 'twitch' ? id.toLowerCase() : id}`;
}

function accountValues(value = {}) {
  return Object.fromEntries(platforms.filter(p => text(value[p])).map(p => [p, text(value[p])]));
}

// Strip response-only fields, including unresolved / migration metadata, from
// the draft so neither a status refresh nor a save treats them as user edits.
export function normalizeWatch(list) {
  return (Array.isArray(list) ? list : []).map(c => (c.kind === 'manual'
    ? { kind: 'manual', name: text(c.name), platforms: accountValues(c.platforms) }
    : { kind: 'configured', name: text(c.name) }));
}

function accountKeys(channel) {
  if (channel.kind !== 'manual') return [];
  return Object.entries(channel.platforms || {}).map(([platform, channel_id]) => watchKey({ platform, channel_id }));
}

export function watchEntryError(channel, list = []) {
  const name = text(channel?.name);
  if (channel?.kind === 'manual') return '仅可关注频道管理中已配置的频道。';
  if (channel?.kind !== 'configured') return '请选择已配置的频道';
  if (!name) return '请选择已配置的频道';
  if (list.some(c => watchKey(c) === watchKey(channel))) return '已在关注列表中';
  const taken = new Set(list.flatMap(accountKeys));
  if (accountKeys(channel).some(key => taken.has(key))) return '该账号已在关注列表中';
  if (list.length >= WATCH_MAX) return `最多关注 ${WATCH_MAX} 个频道`;
  return '';
}

export function addWatch(list, channel) {
  const error = watchEntryError(channel, list);
  return error ? { list, error } : { list: [...list, ...normalizeWatch([channel])], error: '' };
}

export function removeWatch(list, key) {
  return list.filter(c => watchKey(c) !== key);
}

// Saved rows match by account identity, never by display name (which may
// belong to another bot). The catalog is the server's configured channels.
export function watchCatalog(settings) {
  return (settings?.configured_channels || []).map(c => ({
    kind: 'configured', name: text(c.name),
    platforms: platforms.filter(p => Array.isArray(c.platforms) && c.platforms.includes(p)),
  }));
}

// Every match stays browsable, including selected channels. Platform names
// and account ids do not compete with channel names as search identities.
export function filterCandidates(candidates, query) {
  const q = text(query).toLocaleLowerCase();
  return candidates.filter(c => !q || text(c.name).toLocaleLowerCase().includes(q));
}

export function watchSelection(candidate, list) {
  return { checked: list.some(c => watchKey(c) === watchKey(candidate)), partial: false };
}

export function toggleWatchCandidate(list, candidate, selected) {
  return selected ? addWatch(list, candidate) : { list: removeWatch(list, watchKey(candidate)), error: '' };
}

// Defensive boundary for a page whose data changed while a draft was open:
// a retained historical entry may stay or be removed, but a new save cannot
// create or edit one.
export function watchForSave(list, settings) {
  const catalog = watchCatalog(settings);
  const entries = normalizeWatch(list);
  const saved = new Set(normalizeWatch(settings?.config?.watch?.channels).map(c => JSON.stringify(c)));
  const retained = entry => saved.has(JSON.stringify(entry));
  {
    for (const entry of entries) {
      if (retained(entry)) continue;
      if (entry.kind !== 'configured') return { list, error: '仅可关注频道管理中已配置的频道。' };
      if (!catalog.some(c => c.name === entry.name)) return { list, error: '频道管理中找不到这个频道，请刷新后重新选择。' };
    }
    if (entries.length > WATCH_MAX) return { list, error: `最多关注 ${WATCH_MAX} 个频道` };
    return { list: entries, error: '' };
  }
  const allowedAccounts = new Set(catalog.flatMap(c => c.targets.map(watchKey)));
  const encoded = [];
  for (const entry of entries) {
    if (entry.kind === 'manual' || (!entry.kind && !retained(entry) && !allowedAccounts.has(watchKey(entry)))) {
      return { list, error: '仅可关注频道管理中已配置的频道。' };
    }
    const targets = entry.kind === 'configured' ? catalog.find(c => c.name === entry.name)?.targets
      : [entry];
    if (!targets?.length) return { list, error: '当前节点无法保存这个频道，请刷新频道列表后重试。' };
    for (const target of targets) {
      if (!encoded.some(c => watchKey(c) === watchKey(target))) encoded.push(target);
    }
  }
  if (encoded.length > WATCH_MAX) return { list, error: '已达到当前节点的关注上限，请先移除频道。' };
  return { list: encoded, error: '' };
}

export function watchRows(list, catalog) {
  const rows = new Map();
  for (const entry of list) {
    // Every entry is tagged `configured` or `manual` (serde tag in watch_config.rs).
    const configured = entry.kind === 'configured' ? catalog.find(c => c.name === entry.name) : null;
    const key = watchKey(entry);
    if (!rows.has(key)) rows.set(key, {
      key, name: configured?.name || entry.name || '未命名频道',
      historical: entry.kind === 'manual',
      unavailable: !configured,
      missing: entry.kind === 'configured' && !configured,
      platforms: [], members: [],
    });
    const row = rows.get(key);
    const available = entry.kind === 'configured'
      ? configured?.platforms || []
      : Object.keys(accountValues(entry.platforms));
    row.platforms = platforms.filter(p => row.platforms.includes(p) || available.includes(p));
    row.members.push(entry);
  }
  return [...rows.values()];
}

export function watchRowLive(row, status) {
  if (row.unavailable) return false;
  return (status?.watch?.live_entries || []).some(c => watchKey(c) === row.key);
}

export function watchRefusal(error) {
  const { code, field } = error?.body?.data || {};
  const copy = {
    watch_duplicate: '该频道或直播账号已在关注列表中。',
    watch_too_many: '关注列表最多 50 项，请先移除不需要的频道。',
    watch_unknown_channel: '频道管理中找不到这个频道，请刷新后重新选择。',
  }[code] || (code === 'invalid_field' && String(field || '').startsWith('watch') ? '关注列表只能选择已配置的频道，旧关注项仅可保留或移除。' : '');
  if (!copy) return null;
  return { kind: 'watch', focus: code === 'watch_too_many' ? 'watch-remove' : 'notify-watch-picker-toggle', banner: {
    lead: copy, text: '', tone: 'warn',
    ...(code === 'watch_unknown_channel' ? { action: { label: '刷新', target: 'refresh' } } : {}),
  } };
}
