import assert from 'node:assert/strict';
import { test } from 'node:test';
import { fallbackThumbnailUrl, streamStartMs, toCardModel } from '../src/js/stream-model.js';

test('toCardModel maps admin field vocabulary', () => {
  const model = toCardModel({
    id: 'vid1',
    title: '示例直播',
    topic_id: 'singing',
    status: 'LIVE',
    external_link: 'https://www.twitch.tv/example_channel_001',
    stream_type: 'placeholder',
    placeholder_type: 'twitch',
    thumbnail: '',
    channel_name: '示例频道 001',
    channel_id: 'UC123',
    channel_photo: '/t/avatar',
    start_scheduled: '2026-10-03T01:00:00Z',
    start_actual: '2026-10-03T01:05:00Z',
    live_viewers: 12,
    suggested_area_name: '聊天',
  });
  assert.equal(model.link, 'https://www.twitch.tv/example_channel_001');
  assert.equal(model.isPlaceholder, true);
  assert.equal(model.placeholderType, 'twitch');
  assert.equal(model.topic, 'singing');
  assert.equal(model.isLive, true);
  assert.equal(model.channelId, 'UC123');
  assert.equal(model.command, null);
});

test('toCardModel maps public field vocabulary and command', () => {
  const model = toCardModel({
    id: 'vid2',
    title: '示例预告',
    topic: 'game',
    status: 'upcoming',
    link: 'https://www.youtube.com/watch?v=vid2',
    is_placeholder: false,
    placeholder_type: '',
    channel_name: '示例频道 002',
    command_platform: 'YT',
    command_channel: '示例频道 002',
    command_channel_short: '002',
  });
  assert.equal(model.link, 'https://www.youtube.com/watch?v=vid2');
  assert.equal(model.isPlaceholder, false);
  assert.equal(model.topic, 'game');
  assert.equal(model.isLive, false);
  assert.deepEqual(model.command, {
    platform: 'YT',
    name: '示例频道 002',
    short: '002',
  });
});

test('streamStartMs prefers the live clock then the schedule', () => {
  const live = toCardModel({
    status: 'live',
    start_actual: '2026-10-03T01:05:00Z',
    available_at: '2026-10-03T01:00:00Z',
    start_scheduled: '2026-10-03T00:55:00Z',
  });
  assert.equal(streamStartMs(live, true), Date.parse('2026-10-03T01:05:00Z'));
  const upcoming = toCardModel({
    status: 'upcoming',
    start_actual: '2026-10-03T01:05:00Z',
    start_scheduled: '2026-10-03T00:55:00Z',
  });
  assert.equal(streamStartMs(upcoming, false), Date.parse('2026-10-03T00:55:00Z'));
  assert.equal(streamStartMs(toCardModel({ status: 'live' }), true), null);
});

test('fallbackThumbnailUrl is the admin-only YouTube/Twitch still', () => {
  assert.equal(
    fallbackThumbnailUrl(toCardModel({
      stream_type: 'placeholder',
      placeholder_type: 'twitch',
      external_link: 'https://www.twitch.tv/example_channel_001',
    })),
    'https://static-cdn.jtvnw.net/previews-ttv/live_user_example_channel_001-640x360.jpg',
  );
  assert.equal(
    fallbackThumbnailUrl(toCardModel({ id: 'vid1', is_placeholder: false })),
    'https://i.ytimg.com/vi/vid1/sddefault.jpg',
  );
  assert.equal(
    fallbackThumbnailUrl(toCardModel({
      id: 'lv123',
      stream_type: 'placeholder',
      external_link: 'https://live.nicovideo.jp/watch/lv123',
    })),
    '',
  );
});
