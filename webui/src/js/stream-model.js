// stream-model.js — the one card-shaped model both 直播与预告 pages render.
//
// The admin /api/holodex/streams rows and the public /api/public/streams rows
// use different field vocabulary for the same facts (external_link vs link,
// stream_type vs is_placeholder, topic_id vs topic). Normalizing here puts that
// mapping in a single place, so the shared card renderer and the on-air matcher
// never branch on which payload produced a row.

import { timestampMs } from './format.js';

/// Normalizes either payload row into the card model. Public-only fields
/// (command, switchability) are carried through as `null`/optionals for admin
/// rows, so the shared renderer can ignore them.
export function toCardModel(row) {
  const link = row.link || row.external_link || '';
  const isPlaceholder = row.is_placeholder === true || row.stream_type === 'placeholder';
  const status = String(row.status ?? '').toLowerCase();
  return {
    id: row.id ?? '',
    title: row.title ?? '',
    topic: row.topic ?? row.topic_id ?? '',
    isLive: status === 'live',
    isPlaceholder,
    placeholderType: row.placeholder_type ?? '',
    link,
    thumbnail: row.thumbnail ?? '',
    channelName: row.channel_name ?? '',
    channelId: row.channel_id ?? '',
    channelPhoto: row.channel_photo ?? '',
    startScheduled: row.start_scheduled ?? '',
    startActual: row.start_actual ?? '',
    availableAt: row.available_at ?? '',
    publishedAt: row.published_at ?? '',
    liveViewers: row.live_viewers ?? null,
    suggestedAreaName: row.suggested_area_name ?? '',
    command: row.command_platform
      ? {
          platform: row.command_platform,
          name: row.command_channel,
          short: row.command_channel_short,
        }
      : null,
  };
}

/// The stream's start time in ms, in the same source order both pages already
/// use: actual, then available, then published, then the schedule.
export function streamStartMs(model, isLive) {
  const raw = isLive
    ? (model.startActual || model.availableAt || model.publishedAt || model.startScheduled)
    : model.startScheduled;
  return timestampMs(raw);
}

/// The thumbnail to paint when the payload carries none. The public server
/// already resolves thumbnails (`thumbnail_for` in public/streams.rs), so this
/// is the admin-page fallback: a Twitch placeholder preview from the login, a
/// YouTube still from the video id, else nothing (never a Niconico lv id).
export function fallbackThumbnailUrl(model) {
  const placeholder = model.isPlaceholder;
  if (placeholder && model.placeholderType === 'twitch' && model.link) {
    const rest = model.link.split('twitch.tv/')[1]?.trim().split(/[/?#]/)[0];
    if (rest) return `https://static-cdn.jtvnw.net/previews-ttv/live_user_${rest}-640x360.jpg`;
  }
  if (!placeholder && model.id) {
    return `https://i.ytimg.com/vi/${model.id}/sddefault.jpg`;
  }
  return '';
}