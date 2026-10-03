import assert from 'node:assert/strict';
import { test } from 'node:test';
import {
  activeRestream,
  clusterIsRestreaming,
  isRestreaming,
  streamIsOnAir,
} from '../src/js/on-air.js';
import { toCardModel } from '../src/js/stream-model.js';

const onAir = { platform: 'YT', channel_name: '示例频道 001', title: '示例直播' };

function model(row) {
  return toCardModel(row);
}

test('streamIsOnAir matches YT/TW/NC on the normalized model', () => {
  assert.equal(streamIsOnAir(model({
    status: 'live',
    command_platform: 'YT',
    channel_name: 'Holodex Name',
    command_channel: '示例频道 001',
    title: '别的标题',
  }), onAir), true);

  assert.equal(streamIsOnAir(model({
    status: 'live',
    stream_type: 'placeholder',
    placeholder_type: 'twitch',
    external_link: 'https://www.twitch.tv/example_channel_001',
    channel_name: '示例频道 001',
    title: '示例直播',
  }), { platform: 'TW', channel_name: '示例频道 001', title: '示例直播' }), true);

  assert.equal(streamIsOnAir(model({
    status: 'live',
    is_placeholder: true,
    link: 'https://live.nicovideo.jp/watch/lv1',
    channel_name: '示例频道 001',
    title: '示例直播',
  }), { platform: 'NC', channel_name: '示例频道 001', title: '示例直播' }), true);
});

test('streamIsOnAir matches title when names differ, and rejects idle or other platforms', () => {
  const live = {
    status: 'live',
    command_platform: 'YT',
    channel_name: '其他频道',
    title: '示例直播',
  };
  assert.equal(streamIsOnAir(model(live), onAir), true);
  assert.equal(streamIsOnAir(model({ ...live, status: 'upcoming' }), onAir), false);
  assert.equal(streamIsOnAir(model({
    ...live,
    command_platform: 'TW',
    placeholder_type: 'twitch',
    is_placeholder: true,
  }), onAir), false);
  assert.equal(streamIsOnAir(model({ ...live, title: '另一场' }), onAir), false);
  assert.equal(streamIsOnAir(model(live), null), false);
});

test('admin YouTube rows match without command_platform', () => {
  assert.equal(streamIsOnAir(model({
    status: 'live',
    id: 'vid1',
    channel_name: '示例频道 001',
    title: '别的标题',
  }), onAir), true);
});

const pushing = {
  role: 'active',
  ffmpeg_running: true,
  network: { stream_bitrate_kbps: 4000, stream_speed: 1 },
};

test('isRestreaming requires ffmpeg and RTMP TX', () => {
  assert.equal(isRestreaming(pushing), true);
  assert.equal(isRestreaming({ ...pushing, ffmpeg_running: false }), false);
  assert.equal(isRestreaming({
    ...pushing,
    network: { stream_bitrate_kbps: 0, stream_speed: 0 },
  }), false);
  assert.equal(isRestreaming({
    ...pushing,
    network: { stream_bitrate_kbps: 0, stream_speed: 1 },
  }), true);
});

test('activeRestream reads public stream and admin active_stream', () => {
  assert.equal(activeRestream(null), null);
  assert.equal(activeRestream([{ ...pushing, ffmpeg_running: false, stream: { platform: 'YT' } }]), null);
  assert.equal(activeRestream([{ ...pushing, role: 'standby', stream: { platform: 'YT' } }]), null);
  assert.equal(clusterIsRestreaming([{ ...pushing, stream: { platform: 'YT' } }]), true);

  assert.deepEqual(activeRestream([{
    ...pushing,
    stream: { platform: 'YT', channel_name: '示例频道 001', title: '示例直播' },
  }]), onAir);
  assert.deepEqual(activeRestream([{
    ...pushing,
    active_stream: { platform: 'TW', channel_name: '示例频道 001', title: '示例直播' },
  }]), { platform: 'TW', channel_name: '示例频道 001', title: '示例直播' });
});
