import assert from 'node:assert/strict';
import { test } from 'node:test';
import { placeholderKind, updateViewerCount, watchUrl } from '../src/js/stream-card.js';
import { toCardModel } from '../src/js/stream-model.js';

test('placeholderKind is Niconico first, then Twitch, else radio', () => {
  assert.equal(placeholderKind(toCardModel({
    link: 'https://www.nicovideo.jp/watch/lv1',
    placeholder_type: 'twitch',
  })), 'niconico');
  assert.equal(placeholderKind(toCardModel({
    external_link: 'https://www.twitch.tv/example_channel_001',
  })), 'twitch');
  assert.equal(placeholderKind(toCardModel({
    is_placeholder: true,
    placeholder_type: 'twitch',
  })), 'twitch');
  assert.equal(placeholderKind(toCardModel({ link: '' })), 'radio');
});

test('watchUrl prefers the platform link and skips YouTube ids on placeholders', () => {
  assert.equal(
    watchUrl(toCardModel({ link: 'https://www.twitch.tv/example_channel_001' })),
    'https://www.twitch.tv/example_channel_001',
  );
  assert.equal(
    watchUrl(toCardModel({ id: 'vid1', is_placeholder: false })),
    'https://www.youtube.com/watch?v=vid1',
  );
  assert.equal(
    watchUrl(toCardModel({ id: 'lv1', is_placeholder: true })),
    '',
  );
});

function viewersHost() {
  const classes = new Set();
  const element = {
    textContent: '',
    classList: {
      toggle(name, force) {
        if (force) classes.add(name);
        else classes.delete(name);
      },
      contains: (name) => classes.has(name),
    },
  };
  return {
    card: { querySelector: () => element },
    element,
    hidden: () => classes.has('hidden'),
  };
}

test('updateViewerCount paints a finite count and hides empties', () => {
  const { card, element, hidden } = viewersHost();
  updateViewerCount(card, 1234);
  assert.equal(element.textContent, `• ${Number(1234).toLocaleString()} 观看`);
  assert.equal(hidden(), false);
  updateViewerCount(card, null);
  assert.equal(element.textContent, '');
  assert.equal(hidden(), true);
});
