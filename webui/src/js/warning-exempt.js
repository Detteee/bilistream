// 违规后可续播频道 (PRD 10-20-warn-hold-exempt). Pure state for the panel:
// which monitored channel identities are exempt and which built-in reason ids
// may release their hold. The server stores resolved identities; the picker
// works on roster channels and expands them to their platform identities.

export const EXEMPT_REASON_ROWS = Object.freeze([
  { id: 'idle_recording', name: '挂机/录播等消极直播', risk: '低风险', tone: '' },
  { id: 'game_mode_off', name: '未开启该游戏直播模式', risk: '中风险', tone: 'warn' },
  { id: 'game_info_violation', name: '存在违规信息', risk: '高风险', tone: 'danger' },
  { id: 'content_unsuitable', name: '直播内容不适宜', risk: '高风险', tone: 'danger' },
]);
export const DEFAULT_EXEMPT_REASONS = Object.freeze(['idle_recording']);
export const HIGH_RISK_WARNING = '此类违规继续开播极易导致直播间被关闭。';
const REASON_IDS = new Set(EXEMPT_REASON_ROWS.map(row => row.id));
const PLATFORM_LABELS = Object.freeze({ YT: 'YouTube', TW: 'Twitch', NC: 'Niconico' });

export function platformLabel(platform) {
  return PLATFORM_LABELS[platform] || String(platform || '');
}

/** Only built-in ids; a missing list falls back to the low-risk default. */
export function normalizeReasons(list) {
  if (!Array.isArray(list)) return [...DEFAULT_EXEMPT_REASONS];
  const seen = new Set();
  const out = [];
  for (const id of list) {
    if (REASON_IDS.has(id) && !seen.has(id)) { seen.add(id); out.push(id); }
  }
  return out;
}

/** Canonical (platform, channel id) entries, deduplicated in order. */
export function normalizeEntries(list) {
  const seen = new Set();
  const out = [];
  for (const entry of Array.isArray(list) ? list : []) {
    if (!entry || typeof entry !== 'object') continue;
    const platform = String(entry.platform || '').toUpperCase();
    const channelId = String(entry.channel_id || '').trim();
    if (!PLATFORM_LABELS[platform] || !channelId) continue;
    const key = `${platform}:${channelId}`;
    if (seen.has(key)) continue;
    seen.add(key);
    out.push({ platform, channel_id: channelId, name: String(entry.name || '').trim() });
  }
  return out;
}

/** One row per selected roster channel, grouped by display name. */
export function groupedEntries(entries) {
  const rows = [];
  const byName = new Map();
  for (const entry of normalizeEntries(entries)) {
    const key = entry.name;
    if (!byName.has(key)) { byName.set(key, []); rows.push({ name: key, entries: byName.get(key) }); }
    byName.get(key).push(entry);
  }
  return rows;
}

/** Whether a roster channel is already on the list. */
export function channelSelected(entries, channel) {
  return normalizeEntries(entries).some(entry => entry.name === channel?.name);
}

/** Check/uncheck a roster channel, expanding it to every platform identity. */
export function toggleChannel(entries, channel, checked) {
  const name = String(channel?.name || '').trim();
  const rest = normalizeEntries(entries).filter(entry => entry.name !== name);
  if (!checked || !name) return rest;
  const added = (Array.isArray(channel.platforms) ? channel.platforms : []).map(identity => ({
    platform: String(identity.platform || '').toUpperCase(),
    channel_id: String(identity.channel_id || ''),
    name,
  }));
  return normalizeEntries([...rest, ...added]);
}

/** Remove a roster channel by display name. */
export function removeChannel(entries, name) {
  return normalizeEntries(entries).filter(entry => entry.name !== name);
}

/** Picker rows for the search box, marked selected. */
/** Entries whose roster channel still exists (the part the form may save). */
export function knownEntries(entries, catalog) {
  const names = new Set((Array.isArray(catalog) ? catalog : []).map(channel => channel?.name).filter(Boolean));
  return normalizeEntries(entries).filter(entry => names.has(entry.name));
}

/** Entries whose roster channel is gone: shown removable, never saved again. */
export function staleEntries(entries, catalog) {
  const names = new Set((Array.isArray(catalog) ? catalog : []).map(channel => channel?.name).filter(Boolean));
  return normalizeEntries(entries).filter(entry => !names.has(entry.name));
}

export function pickerRows(catalog, entries, search) {
  const query = String(search || '').trim().toLowerCase();
  const selected = new Set(normalizeEntries(entries).map(entry => entry.name));
  return (Array.isArray(catalog) ? catalog : [])
    .filter(channel => channel && typeof channel.name === 'string')
    .filter(channel => !query || channel.name.toLowerCase().includes(query))
    .map(channel => ({
      name: channel.name,
      platforms: (Array.isArray(channel.platforms) ? channel.platforms : []).map(identity => platformLabel(identity.platform)),
      selected: selected.has(channel.name),
    }));
}
