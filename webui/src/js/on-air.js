// on-air.js — which 直播与预告 row the publishing node is pushing right now.
//
// Shared by the public page and the admin dashboard. It matches the normalized
// card model (stream-model.js), never a raw payload row, so the field-vocabulary
// differences between /api/public/streams and /api/holodex/streams live in one
// place. The admin stage reads the node's `active_stream`, the public page its
// `stream`; both are accepted here.

function norm(value) {
  return String(value ?? '').trim().toLocaleLowerCase().replace(/\s+/g, ' ');
}

function platformCode(platform) {
  const value = String(platform ?? '').trim().toUpperCase();
  if (value === 'YOUTUBE') return 'YT';
  if (value === 'TWITCH') return 'TW';
  if (value === 'NICONICO' || value === 'NICO') return 'NC';
  return value;
}

/// The card's platform code (YT/TW/NC). The public payload carries
/// `command_platform`; the admin payload does not, so fall back to the watch
/// link and placeholder markers — the same derivation the public page used.
function cardPlatform(model) {
  const command = platformCode(model.command?.platform);
  if (command === 'YT' || command === 'TW' || command === 'NC') {
    return command;
  }
  const link = String(model.link || '').toLowerCase();
  if (link.includes('nicovideo.jp')) return 'NC';
  if (model.placeholderType === 'twitch' || link.includes('twitch.tv')) return 'TW';
  return model.isPlaceholder === false ? 'YT' : '';
}

/// @param {object | null} model — normalized card model (stream-model.js)
/// @param {{ platform?: string, channel_name?: string, title?: string } | null} onAir
export function streamIsOnAir(model, onAir) {
  if (!onAir || !model || model.isLive !== true) {
    return false;
  }
  const platform = platformCode(onAir.platform);
  if (!platform || cardPlatform(model) !== platform) {
    return false;
  }
  const names = [model.channelName, model.command?.name, model.command?.short]
    .map(norm)
    .filter(Boolean);
  const target = norm(onAir.channel_name);
  if (target && names.includes(target)) {
    return true;
  }
  const airTitle = norm(onAir.title);
  const cardTitle = norm(model.title);
  return Boolean(airTitle && cardTitle && airTitle === cardTitle);
}

function hasPositive(value) {
  return Number.isFinite(value) && value > 0;
}

function hasRtmpTx(network) {
  return !!network && (hasPositive(network.stream_bitrate_kbps) || hasPositive(network.stream_speed));
}

/// Owner that is actually pushing. An idle active node is 活跃, not 转播中.
/// Same condition the server uses (`ClusterNodeSnapshot::is_restreaming`).
export function isRestreaming(node) {
  return !!node?.ffmpeg_running && hasRtmpTx(node?.network);
}

/// Same condition as the 转播中 badge: an active node with ffmpeg on the wire.
export function clusterIsRestreaming(nodes) {
  return Array.isArray(nodes) && nodes.some((node) => node.role === 'active' && isRestreaming(node));
}

/// Whose stream the 直播与预告 highlight should point at. Null when idle.
/// Reads both the public payload's `stream` and the admin snapshot's
/// `active_stream`, which carry the same fields.
export function activeRestream(nodes) {
  if (!Array.isArray(nodes)) return null;
  const node = nodes.find((item) => item.role === 'active' && isRestreaming(item));
  const stream = node?.stream || node?.active_stream;
  if (!stream) return null;
  return {
    platform: stream.platform || '',
    channel_name: stream.channel_name || '',
    title: stream.title || '',
  };
}