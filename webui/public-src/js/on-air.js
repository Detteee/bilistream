// Which 直播与预告 row is the one the server is pushing right now.
//
// The public node payload has no channel id, so the match is the platform
// code plus the channel name (Holodex name or command name) or the title.

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

function listPlatform(stream) {
  const command = platformCode(stream?.command_platform);
  if (command === 'YT' || command === 'TW' || command === 'NC') {
    return command;
  }
  const link = String(stream?.link || '').toLowerCase();
  if (link.includes('nicovideo.jp')) return 'NC';
  if (stream?.placeholder_type === 'twitch' || link.includes('twitch.tv')) return 'TW';
  if (stream && stream.is_placeholder === false) return 'YT';
  return '';
}

/// @param {object | null | undefined} stream
/// @param {{ platform?: string, channel_name?: string, title?: string } | null | undefined} onAir
export function streamIsOnAir(stream, onAir) {
  if (!onAir || String(stream?.status || '').toLowerCase() !== 'live') {
    return false;
  }
  const platform = platformCode(onAir.platform);
  if (!platform || listPlatform(stream) !== platform) {
    return false;
  }
  const names = [stream.channel_name, stream.command_channel, stream.command_channel_short]
    .map(norm)
    .filter(Boolean);
  const target = norm(onAir.channel_name);
  if (target && names.includes(target)) {
    return true;
  }
  const airTitle = norm(onAir.title);
  const cardTitle = norm(stream.title);
  return Boolean(airTitle && cardTitle && airTitle === cardTitle);
}
