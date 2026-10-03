// Pure admin translations. List-row compatibility belongs to toCardModel;
// DOM events, crop sessions and requests belong to overview.js.
import { placeholderKind } from './stream-model.js';

// Match plugins::twitch::channel_id_from_url. Keep the original link in
// actions/payloads; canonical casing is only for channel identity.
export function parseTwitchLoginFromLink(link) {
  if (!link) return '';
  try {
    const url = new URL(String(link).trim());
    if (url.protocol !== 'http:' && url.protocol !== 'https:') return '';
    if (url.hostname !== 'twitch.tv' && url.hostname !== 'www.twitch.tv') return '';
    return (url.pathname.split('/')[1] || '').trim().toLowerCase();
  } catch {
    return '';
  }
}

export function toAdminStreamAction(model) {
  const kind = model.isPlaceholder ? placeholderKind(model) : '';
  return {
    platform: model.isPlaceholder
      ? (kind === 'twitch' ? 'twitch' : kind === 'niconico' ? 'niconico' : 'external')
      : 'youtube',
    channelId: model.channelId,
    twitchId: model.isPlaceholder ? parseTwitchLoginFromLink(model.link) : '',
    externalLink: model.link,
    suggestedAreaId: model.suggestedAreaId || '',
    title: model.title,
    topicId: model.topic,
    status: model.status,
  };
}

// POST names intentionally differ from the current list vocabulary.
export function createHolodexSwitchPayload(action, areaId) {
  return {
    channel_id: action.channelId,
    area_id: areaId,
    title: action.title || null,
    topic_id: action.topicId || null,
    status: action.status || null,
    platform: action.platform || 'youtube',
    twitch_channel_id: action.twitchId || null,
    external_link: action.externalLink || null,
  };
}

export function toAdminChannelCandidate(model) {
  return {
    name: model.channelName,
    youtubeId: model.channelId,
    twitchId: parseTwitchLoginFromLink(model.link),
  };
}

function normalizeChannelValue(value) {
  return String(value || '').trim().toLowerCase();
}

export function sameChannelCandidate(candidate, other) {
  return ['name', 'youtubeId', 'twitchId'].some(key => {
    const value = normalizeChannelValue(candidate[key]);
    return value !== '' && value === normalizeChannelValue(other[key]);
  });
}

export function hasConfiguredChannel(candidate, channels) {
  return Array.isArray(channels) && channels.some(channel => sameChannelCandidate(candidate, {
    name: channel.name,
    youtubeId: channel.platforms?.youtube,
    twitchId: channel.platforms?.twitch,
  }));
}

export function buildHolodexAddChannelPayload(candidate) {
  const platforms = {};
  if (candidate.youtubeId) platforms.youtube = candidate.youtubeId;
  if (candidate.twitchId) platforms.twitch = candidate.twitchId;
  return { name: candidate.name, aliases: [], platforms, riot_puuid: null };
}
