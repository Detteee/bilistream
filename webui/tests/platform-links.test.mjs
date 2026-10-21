import test from 'node:test';
import assert from 'node:assert/strict';

import { platformRoomUrl } from '../src/js/platform-links.js';

test('bilibili links the room it pushes to', () => {
  assert.equal(platformRoomUrl('bilibili', { room: 12345 }), 'https://live.bilibili.com/12345');
});

test('bilibili has no link without a configured room', () => {
  assert.equal(platformRoomUrl('bilibili', {}), '');
  assert.equal(platformRoomUrl('bilibili', { room: 0 }), '');
  assert.equal(platformRoomUrl('bilibili', null), '');
});

test('youtube links the channel live page, which resolves either way', () => {
  assert.equal(
    platformRoomUrl('youtube', { channel_id: 'UC1111111111111111111111' }),
    'https://www.youtube.com/channel/UC1111111111111111111111/live'
  );
  assert.equal(platformRoomUrl('youtube', { channel_id: '' }), '');
});

test('twitch links the login, which is the channel id', () => {
  assert.equal(platformRoomUrl('twitch', { channel_id: 'demo_games' }), 'https://www.twitch.tv/demo_games');
  assert.equal(platformRoomUrl('twitch', {}), '');
});

test('niconico prefers the live page and falls back to the channel page', () => {
  assert.equal(
    platformRoomUrl('niconico', { channel_id: 'ch2598414', live_id: 'lv351182284' }),
    'https://live.nicovideo.jp/watch/lv351182284'
  );
  assert.equal(
    platformRoomUrl('niconico', { channel_id: 'ch2598414' }),
    'https://ch.nicovideo.jp/ch2598414'
  );
  assert.equal(platformRoomUrl('niconico', {}), '');
});

test('an unknown platform yields no link rather than a guess', () => {
  assert.equal(platformRoomUrl('priority', { channel_id: 'x' }), '');
});
