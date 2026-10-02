import assert from 'node:assert/strict';
import { test } from 'node:test';
import { streamIsOnAir } from '../public-src/js/on-air.js';
import { activeRestream } from '../public-src/js/nodes.js';
import { nextStatusFold } from '../public-src/js/status-fold.js';

const closed = { restreaming: null, open: false };

test('idle status stays closed', () => {
  const next = nextStatusFold(closed, { type: 'status', restreaming: false });
  assert.deepEqual(next, { restreaming: false, open: false });
});

test('转播中 opens the fold', () => {
  const next = nextStatusFold(closed, { type: 'status', restreaming: true });
  assert.deepEqual(next, { restreaming: true, open: true });
});

test('a repeated poll keeps a manual toggle', () => {
  const idle = nextStatusFold(closed, { type: 'status', restreaming: false });
  const opened = nextStatusFold(idle, { type: 'toggle' });
  assert.equal(opened.open, true);
  assert.strictEqual(nextStatusFold(opened, { type: 'status', restreaming: false }), opened);

  const live = nextStatusFold(closed, { type: 'status', restreaming: true });
  const shut = nextStatusFold(live, { type: 'toggle' });
  assert.equal(shut.open, false);
  assert.strictEqual(nextStatusFold(shut, { type: 'status', restreaming: true }), shut);
});

test('转播中 changing restores that mode default', () => {
  const live = nextStatusFold(closed, { type: 'status', restreaming: true });
  const shut = nextStatusFold(live, { type: 'toggle' });
  assert.deepEqual(nextStatusFold(shut, { type: 'status', restreaming: false }), {
    restreaming: false,
    open: false,
  });

  const idle = nextStatusFold(closed, { type: 'status', restreaming: false });
  const opened = nextStatusFold(idle, { type: 'toggle' });
  assert.deepEqual(nextStatusFold(opened, { type: 'status', restreaming: true }), {
    restreaming: true,
    open: true,
  });
});

test('refresh-failed keeps the fold', () => {
  const live = nextStatusFold(closed, { type: 'status', restreaming: true });
  assert.strictEqual(nextStatusFold(live, { type: 'refresh-failed' }), live);
  assert.strictEqual(nextStatusFold(closed, { type: 'refresh-failed' }), closed);
});

const publishing = {
  role: 'active',
  ffmpeg_running: true,
  network: { stream_bitrate_kbps: 4000, stream_speed: 1 },
  stream: { platform: 'YT', channel_name: '示例频道 001', title: '示例直播' },
};

test('activeRestream is the publishing node only', () => {
  assert.equal(activeRestream(null), null);
  assert.equal(activeRestream([{ ...publishing, ffmpeg_running: false }]), null);
  assert.equal(activeRestream([{ ...publishing, role: 'standby' }]), null);
  assert.deepEqual(activeRestream([publishing]), {
    platform: 'YT',
    channel_name: '示例频道 001',
    title: '示例直播',
  });
});

const onAir = { platform: 'YT', channel_name: '示例频道 001', title: '示例直播' };

test('streamIsOnAir matches the live row the server is pushing', () => {
  const live = {
    status: 'live',
    command_platform: 'YT',
    channel_name: 'Holodex Name',
    command_channel: '示例频道 001',
    title: '别的标题',
  };
  assert.equal(streamIsOnAir(live, onAir), true);
  assert.equal(streamIsOnAir({ ...live, status: 'upcoming' }, onAir), false);
  assert.equal(streamIsOnAir({ ...live, command_platform: 'TW', placeholder_type: 'twitch' }, onAir), false);
  assert.equal(streamIsOnAir({
    status: 'live',
    command_platform: 'YT',
    channel_name: '其他频道',
    title: '示例直播',
  }, onAir), true);
  assert.equal(streamIsOnAir({
    status: 'live',
    command_platform: 'YT',
    channel_name: '其他频道',
    title: '另一场',
  }, onAir), false);
});
