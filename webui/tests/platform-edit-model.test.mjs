import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { test } from 'node:test';
import {
  PLATFORM_EDIT_ROWS,
  channelEditPayload,
  fieldEditIds
} from '../src/js/platform-edit-model.js';

const html = readFileSync(new URL('../src/index.html', import.meta.url), 'utf8');
const overview = readFileSync(new URL('../src/js/overview.js', import.meta.url), 'utf8');

test('the editor rows cover three platforms by three fields, once each', () => {
  assert.equal(PLATFORM_EDIT_ROWS.length, 9);
  const keys = PLATFORM_EDIT_ROWS.map((row) => `${row.prefix}/${row.kind}`);
  assert.equal(new Set(keys).size, 9);
  assert.deepEqual(
    [...new Set(PLATFORM_EDIT_ROWS.map((row) => row.platform))],
    ['youtube', 'twitch', 'niconico']
  );
});

test('every derived element id exists in the page', () => {
  for (const row of PLATFORM_EDIT_ROWS) {
    const ids = fieldEditIds(row);
    for (const [name, id] of Object.entries(ids)) {
      assert.match(
        html,
        new RegExp(`id="${id}"`),
        `${row.prefix}/${row.kind} ${name} expects #${id} in index.html`
      );
    }
  }
});

test('the channel row value span is the only id that is not kind-derived', () => {
  const channel = fieldEditIds({ prefix: 'yt', kind: 'channel' });
  assert.equal(channel.span, 'yt-channel-name');
  const area = fieldEditIds({ prefix: 'yt', kind: 'area' });
  assert.equal(area.span, 'yt-area');
  assert.equal(area.select, 'yt-area-edit-select');
});

test('a channel option turns into the request body the API expects', () => {
  assert.deepEqual(channelEditPayload('{"id":"UC123","name":"Example"}'), {
    channel_id: 'UC123',
    channel_name: 'Example'
  });
  assert.throws(() => channelEditPayload('not json'), SyntaxError);
});

test('overview.js drives the rows instead of carrying one copy per platform', () => {
  assert.match(overview, /bindFieldEditRows\(\)/);
  assert.match(overview, /from '\.\/platform-edit-model\.js'/);
  for (const prefix of ['Yt', 'Tw', 'Nc']) {
    for (const field of ['Channel', 'Area', 'Quality']) {
      for (const verb of ['toggle', 'cancel', 'save']) {
        assert.doesNotMatch(
          overview,
          new RegExp(`${verb}${prefix}${field}Edit`),
          `${verb}${prefix}${field}Edit should be gone`
        );
      }
    }
  }
});
