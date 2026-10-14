// Channel-name watch-list state. New servers resolve configured names; older
// servers still receive their original flat account entries. No DOM access.

export const WATCH_PLATFORMS = Object.freeze({ youtube: 'YouTube', twitch: 'Twitch', niconico: 'Niconico' });
export const WATCH_MAX = 50;
const platforms = Object.keys(WATCH_PLATFORMS);
const text = value => String(value ?? '').trim();

export function groupedWatchSupported(settings) {
  return Array.isArray(settings?.configured_channels);
}

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
  return (Array.isArray(list) ? list : []).map(c => {
    if (c.kind === 'configured') return { kind: 'configured', name: text(c.name) };
    if (c.kind === 'manual') return { kind: 'manual', name: text(c.name), platforms: accountValues(c.platforms) };
    return { platform: text(c.platform), channel_id: text(c.channel_id), name: text(c.name) };
  });
}

function accountKeys(channel) {
  if (channel.kind === 'configured') return [];
  if (channel.kind === 'manual') return Object.entries(channel.platforms || {}).map(([platform, channel_id]) => watchKey({ platform, channel_id }));
  return [watchKey(channel)];
}

export function watchEntryError(channel, list = []) {
  const name = text(channel?.name);
  if (channel?.kind === 'manual') return '仅可关注频道管理中已配置的频道。';
  if (channel?.kind === 'configured') {
    if (!name) return '请选择已配置的频道';
  } else {
    if (!WATCH_PLATFORMS[channel?.platform]) return '请选择平台';
    const id = text(channel.channel_id);
    if (!id) return '请填写频道 ID';
    if (id.length > 64 || !/^[0-9A-Za-z_.@-]+$/.test(id)) return '频道 ID 只能包含字母、数字和 _ - . @';
    if ([...name].length > 40) return '名称最多 40 个字';
  }
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

// Legacy catalog rows are grouped for presentation only. Saved rows match by
// account identity, never by display name (which may belong to another bot).
export function watchCatalog(settings) {
  if (groupedWatchSupported(settings)) {
    return settings.configured_channels.map(c => ({
      kind: 'configured', name: text(c.name),
      platforms: platforms.filter(p => Array.isArray(c.platforms) && c.platforms.includes(p)),
    }));
  }
  const byName = new Map();
  for (const target of normalizeWatch(settings?.watch_candidates)) {
    const name = target.name || target.channel_id;
    if (!byName.has(name)) byName.set(name, { kind: 'configured', name, platforms: [], targets: [] });
    const channel = byName.get(name);
    if (!channel.platforms.includes(target.platform)) channel.platforms.push(target.platform);
    if (!channel.targets.some(c => watchKey(c) === watchKey(target))) channel.targets.push(target);
  }
  return [...byName.values()];
}

// Every match stays browsable, including selected channels. Platform names
// and account ids do not compete with channel names as search identities.
export function filterCandidates(candidates, query) {
  const q = text(query).toLocaleLowerCase();
  return candidates.filter(c => !q || text(c.name).toLocaleLowerCase().includes(q));
}

export function watchSelection(candidate, list) {
  if (!candidate.targets) return { checked: list.some(c => watchKey(c) === watchKey(candidate)), partial: false };
  const keys = new Set(list.map(watchKey));
  const count = candidate.targets.filter(c => keys.has(watchKey(c))).length;
  return { checked: count > 0, partial: count > 0 && count < candidate.targets.length };
}

export function toggleWatchCandidate(list, candidate, selected) {
  if (!candidate.targets) return selected ? addWatch(list, candidate) : { list: removeWatch(list, watchKey(candidate)), error: '' };
  const keys = new Set(candidate.targets.map(watchKey));
  if (!selected) return { list: list.filter(c => !keys.has(watchKey(c))), error: '' };
  let next = list;
  for (const target of candidate.targets) {
    if (next.some(c => watchKey(c) === watchKey(target))) continue;
    const added = addWatch(next, target);
    if (added.error) return { list, error: added.error }; // one selection is atomic
    next = added.list;
  }
  return { list: next, error: '' };
}

// Defensive boundary for a page whose backend changed while a draft was open.
// Never post grouped entries to a legacy server, and never discard an account
// that cannot be resolved there. Historical unmatched entries may be retained
// unchanged or removed, but cannot be created or edited by a new save.
export function watchForSave(list, settings) {
  const catalog = watchCatalog(settings);
  const entries = normalizeWatch(list);
  const saved = new Set(normalizeWatch(settings?.config?.watch?.channels).map(c => JSON.stringify(c)));
  const retained = entry => saved.has(JSON.stringify(entry));
  if (groupedWatchSupported(settings)) {
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
    const configured = entry.kind === 'configured' ? catalog.find(c => c.name === entry.name)
      : !entry.kind ? catalog.find(c => c.targets?.some(t => watchKey(t) === watchKey(entry))) : null;
    const key = entry.kind ? watchKey(entry) : configured ? watchKey(configured) : `legacy:${entry.name || watchKey(entry)}`;
    if (!rows.has(key)) rows.set(key, {
      key, name: configured?.name || entry.name || '未命名频道',
      historical: entry.kind === 'manual' || (!entry.kind && !configured),
      unavailable: !configured,
      missing: entry.kind === 'configured' && !configured,
      platforms: [], members: [],
    });
    const row = rows.get(key);
    const available = entry.kind === 'configured' ? configured?.platforms || []
      : entry.kind === 'manual' ? Object.keys(accountValues(entry.platforms)) : [entry.platform];
    row.platforms = platforms.filter(p => row.platforms.includes(p) || available.includes(p));
    row.members.push(entry);
  }
  return [...rows.values()];
}

export function watchRowLive(row, status) {
  if (row.unavailable) return false;
  if (Array.isArray(status?.watch?.live_entries)) return status.watch.live_entries.some(c => watchKey(c) === row.key);
  const live = new Set(status?.watch?.live || []);
  return row.members.some(c => live.has(watchKey(c)) || accountKeys(c).some(key => live.has(key)));
}

export function watchMigrationText(settings) {
  const notice = settings?.config?.watch?.migration_notice;
  if (!notice) return '';
  return Number.isInteger(notice.folded) && notice.folded > 0
    ? `已按频道合并关注项：${notice.folded} 项合并为频道。`
    : '已按频道合并关注项。';
}

// Acknowledgement is browser-local, scoped to a node and the server's migration
// identity. It never changes the watch list or clears the server's notice.
export function createWatchMigrationAcknowledgements(getStorage = () => null) {
  const dismissed = new Set();
  const identity = (nodeId, notice) => {
    if (!notice) return null;
    const node = typeof nodeId === 'string' ? nodeId.trim() : '';
    const at = Number.isSafeInteger(notice.at) && notice.at >= 0 ? notice.at : null;
    const folded = Number.isSafeInteger(notice.folded) && notice.folded >= 0 ? notice.folded : null;
    return { key: `bilistream.notify.watch.migration:${JSON.stringify([node, at, folded])}`, persistent: Boolean(node) && at !== null };
  };
  return {
    isDismissed(nodeId, notice) {
      const id = identity(nodeId, notice);
      if (!id) return false;
      if (dismissed.has(id.key)) return true;
      try {
        if (id.persistent && getStorage()?.getItem(id.key) === '1') {
          dismissed.add(id.key);
          return true;
        }
      } catch { /* storage is optional; acknowledgement still works in this page */ }
      return false;
    },
    dismiss(nodeId, notice) {
      const id = identity(nodeId, notice);
      if (!id) return;
      dismissed.add(id.key);
      try { if (id.persistent) getStorage()?.setItem(id.key, '1'); }
      catch { /* denied storage / quota: keep the in-memory acknowledgement */ }
    },
  };
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
