import assert from 'node:assert/strict';
import { test } from 'node:test';
import { toCardModel } from '../src/js/stream-model.js';
import {
  buildHolodexAddChannelPayload,
  createHolodexSwitchPayload,
  hasConfiguredChannel,
  parseTwitchLoginFromLink,
  sameChannelCandidate,
  toAdminChannelCandidate,
  toAdminStreamAction,
} from '../src/js/admin-stream-actions.js';

// Only the DOM surface used to construct cards. Compiled event/crop/modal
// wiring is exercised separately by mock/stream-action-checks.mjs.
function element() {
  const node = {
    className: '', dataset: {}, children: [], textContent: '',
    addEventListener() {}, setAttribute() {},
    append(...children) { this.children.push(...children); },
    appendChild(child) { this.append(child); return child; },
    prepend(child) { this.children.unshift(child); },
    querySelector(selector) {
      const name = selector.slice(1);
      for (const child of this.children) {
        if (child.classList.contains(name)) return child;
        const descendant = child.querySelector(selector);
        if (descendant) return descendant;
      }
      return null;
    },
  };
  node.classList = {
    contains(name) { return node.className.split(' ').includes(name); },
    toggle(name, force) {
      const classes = new Set(node.className.split(' ').filter(Boolean));
      if (force ?? !classes.has(name)) classes.add(name); else classes.delete(name);
      node.className = [...classes].join(' ');
    },
    add(name) { this.toggle(name, true); },
  };
  return node;
}
globalThis.window = {};
globalThis.document = {
  addEventListener() {}, getElementById() { return null; }, querySelector() { return null; },
  createElement: element, createElementNS: element,
};
const { createStreamCard, readHolodexStreamActionData } = await import('../src/js/overview.js');

const fixtures = [
  ['youtube', false, 'https://www.youtube.com/watch?v=example_video_001', null],
  ['twitch', true, 'https://www.twitch.tv/example_channel_001', 'example_channel_001'],
  ['niconico', true, 'https://live.nicovideo.jp/watch/lv001', null],
];
function rowFor(platform, isPlaceholder, link, legacy) {
  return {
    id: 'example_video_001', title: '示例直播 001', status: 'live',
    channel_id: 'UC_EXAMPLE_001', channel_name: '示例频道 001',
    suggested_area_id: 371, suggested_area_name: '示例分区',
    ...(legacy
      ? { external_link: link, topic_id: 'singing', stream_type: isPlaceholder ? 'placeholder' : 'stream' }
      : { link, topic: 'singing', is_placeholder: isPlaceholder }),
    placeholder_type: platform === 'twitch' ? 'twitch' : null,
  };
}

for (const legacy of [false, true]) {
  for (const [platform, placeholder, link, twitchId] of fixtures) {
    test(`${legacy ? 'legacy' : 'current'} ${platform} card carries switch and crop action data`, () => {
      const model = toCardModel(rowFor(platform, placeholder, link, legacy));
      const card = createStreamCard(model);
      const action = readHolodexStreamActionData(card.querySelector('.switch-button'));
      const expected = {
        channel_id: 'UC_EXAMPLE_001', area_id: 371, title: '示例直播 001',
        topic_id: 'singing', status: 'live', platform,
        twitch_channel_id: twitchId, external_link: link,
      };
      assert.deepEqual(createHolodexSwitchPayload(toAdminStreamAction(model), 371), expected);
      assert.deepEqual(createHolodexSwitchPayload({ ...action, twitchId: action.twitchChannelId }, action.suggestedAreaId), expected);
      const crop = card.querySelector('.crop-switch-button');
      if (platform === 'niconico') assert.equal(crop, null);
      else assert.deepEqual(readHolodexStreamActionData(crop), action);
    });
  }
}

for (const legacy of [false, true]) {
  test(`${legacy ? 'legacy' : 'current'} row retains optional data through manual area selection`, () => {
    const model = toCardModel({
      ...rowFor('twitch', true, ' https://WWW.TWITCH.TV/Example_Channel_001/videos?x=1#live ', legacy),
      title: null, status: 'upcoming', suggested_area_id: null,
    });
    const action = toAdminStreamAction(model);
    assert.equal(action.suggestedAreaId, '');
    const card = createStreamCard(model);
    assert.equal(card.querySelector('.crop-switch-button'), null);
    assert.equal(readHolodexStreamActionData(card.querySelector('.switch-button')).suggestedAreaId, null);
    assert.deepEqual(createHolodexSwitchPayload(action, 235), {
      channel_id: 'UC_EXAMPLE_001', area_id: 235, title: null,
      topic_id: 'singing', status: 'upcoming', platform: 'twitch',
      twitch_channel_id: 'example_channel_001',
      external_link: ' https://WWW.TWITCH.TV/Example_Channel_001/videos?x=1#live ',
    });
  });

  test(`${legacy ? 'legacy' : 'current'} channel candidate produces IDs and matches configured channels`, () => {
    const candidate = toAdminChannelCandidate(toCardModel(rowFor(
      'twitch', true, 'https://WWW.TWITCH.TV/Example_Channel_001', legacy,
    )));
    assert.deepEqual(buildHolodexAddChannelPayload(candidate), {
      name: '示例频道 001', aliases: [],
      platforms: { youtube: 'UC_EXAMPLE_001', twitch: 'example_channel_001' },
      riot_puuid: null,
    });
    assert.equal(hasConfiguredChannel(candidate, [{ name: ' 示例频道 001 ', platforms: {} }]), true);
    assert.equal(hasConfiguredChannel(candidate, [{ name: '示例频道 002', platforms: { youtube: ' uc_example_001 ' } }]), true);
    assert.equal(hasConfiguredChannel(candidate, [{ name: '示例频道 002', platforms: { twitch: ' Example_Channel_001 ' } }]), true);
    assert.equal(hasConfiguredChannel(candidate, [{ name: '示例频道 002', platforms: { twitch: 'example_channel_002' } }]), false);
    assert.equal(hasConfiguredChannel(candidate, null), false);
    assert.equal(sameChannelCandidate(candidate, { name: '', twitchId: 'EXAMPLE_CHANNEL_001' }), true);
  });
}

test('absent action fields serialize as the existing optional POST values', () => {
  assert.deepEqual(createHolodexSwitchPayload(toAdminStreamAction(toCardModel({})), 235), {
    channel_id: '', area_id: 235, title: null, topic_id: null, status: null,
    platform: 'youtube', twitch_channel_id: null, external_link: null,
  });
  const missing = toAdminChannelCandidate(toCardModel({}));
  assert.equal(hasConfiguredChannel(missing, [{ name: '', platforms: {} }]), false);
  assert.deepEqual(buildHolodexAddChannelPayload(missing).platforms, {});
});

test('YouTube and Niconico candidates retain the Holodex channel ID, without a Twitch ID', () => {
  for (const platform of ['youtube', 'niconico']) {
    const link = platform === 'youtube' ? '' : 'https://live.nicovideo.jp/watch/lv001';
    const model = toCardModel(rowFor(platform, platform === 'niconico', link, false));
    assert.deepEqual(buildHolodexAddChannelPayload(toAdminChannelCandidate(model)), {
      name: '示例频道 001', aliases: [], platforms: { youtube: 'UC_EXAMPLE_001' }, riot_puuid: null,
    });
  }
});

test('Twitch URL identity follows the Rust absolute-URL, host and case contract', () => {
  for (const url of [
    'https://twitch.tv/example_channel_001',
    'https://www.twitch.tv/example_channel_001',
    'http://twitch.tv/example_channel_001/videos',
    'http://www.twitch.tv/example_channel_001/',
    'https://WWW.TWITCH.TV/Example_Channel_001',
    'HTTPS://TWITCH.TV/EXAMPLE_CHANNEL_001?x=1#live',
    ' https://www.twitch.tv/Example_Channel_001?x=1#live ',
  ]) {
    assert.equal(parseTwitchLoginFromLink(url), 'example_channel_001', url);
  }
  for (const url of [
    '', null, undefined, 'https://twitch.tv', 'https://twitch.tv/',
    'https://example.com/twitch.tv/example_channel_001',
    'https://twitch.tv.example.com/example_channel_001',
    'https://twitch.tv@example.com/example_channel_001',
    'twitch.tv/example_channel_001', '//twitch.tv/example_channel_001',
    '/example_channel_001', 'ftp://twitch.tv/example_channel_001',
  ]) {
    assert.equal(parseTwitchLoginFromLink(url), '', String(url));
  }
});
