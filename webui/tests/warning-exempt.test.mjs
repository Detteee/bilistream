import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { test } from 'node:test';
import {
  DEFAULT_EXEMPT_REASONS, EXEMPT_REASON_ROWS, HIGH_RISK_WARNING, channelSelected, groupedEntries,
  knownEntries, normalizeEntries, normalizeReasons, pickerRows, platformLabel, removeChannel,
  staleEntries, toggleChannel,
} from '../src/js/warning-exempt.js';

const catalog = [
  { name: '星海频道', platforms: [{ platform: 'YT', channel_id: 'UC1' }, { platform: 'TW', channel_id: 'starsea' }] },
  { name: '电台夜话', platforms: [{ platform: 'YT', channel_id: 'UC2' }] },
];

test('the built-in reason rows mirror the PRD table and default to low risk', () => {
  assert.deepEqual(EXEMPT_REASON_ROWS.map(row => row.id), [
    'idle_recording', 'game_mode_off', 'game_info_violation', 'content_unsuitable',
  ]);
  assert.deepEqual([...DEFAULT_EXEMPT_REASONS], ['idle_recording']);
  assert.equal(EXEMPT_REASON_ROWS[0].tone, '');
  assert.equal(EXEMPT_REASON_ROWS[1].tone, 'warn');
  assert.equal(EXEMPT_REASON_ROWS[2].tone, 'danger');
  assert.equal(EXEMPT_REASON_ROWS[3].tone, 'danger');
  assert.equal(HIGH_RISK_WARNING, '此类违规继续开播极易导致直播间被关闭。');
});

test('reason ids normalize: unknown and duplicate ids drop, missing list uses the default', () => {
  assert.deepEqual(normalizeReasons(undefined), ['idle_recording']);
  assert.deepEqual(normalizeReasons([]), []);
  assert.deepEqual(normalizeReasons(['game_mode_off', 'nope', 'idle_recording', 'game_mode_off']), ['game_mode_off', 'idle_recording']);
});

test('stored identities normalize: platform whitelist, blank ids and duplicates drop', () => {
  assert.deepEqual(
    normalizeEntries([
      { platform: 'yt', channel_id: 'UC1', name: '星海频道' },
      { platform: 'YT', channel_id: 'UC1', name: '重复' },
      { platform: 'XX', channel_id: 'x', name: 'x' },
      { platform: 'TW', channel_id: '  ', name: 'x' },
      null,
    ]),
    [{ platform: 'YT', channel_id: 'UC1', name: '星海频道' }],
  );
  assert.equal(platformLabel('NC'), 'Niconico');
  assert.equal(platformLabel('XX'), 'XX');
});

test('selecting a catalog channel expands every platform identity, deselect removes it', () => {
  const selected = toggleChannel([], catalog[0], true);
  assert.deepEqual(selected, [
    { platform: 'YT', channel_id: 'UC1', name: '星海频道' },
    { platform: 'TW', channel_id: 'starsea', name: '星海频道' },
  ]);
  assert.equal(channelSelected(selected, catalog[0]), true);
  assert.equal(channelSelected(selected, catalog[1]), false);
  assert.deepEqual(removeChannel(selected, '星海频道'), []);
  assert.deepEqual(toggleChannel(selected, catalog[1], true).length, 3);
});

test('grouped rows name each channel once with its platform labels', () => {
  const entries = toggleChannel([], catalog[0], true);
  assert.deepEqual(groupedEntries(entries).map(row => row.name), ['星海频道']);
});

test('the picker filters by name and marks selected rows', () => {
  const entries = toggleChannel([], catalog[0], true);
  assert.deepEqual(pickerRows(catalog, entries, ''), [
    { name: '星海频道', platforms: ['YouTube', 'Twitch'], selected: true },
    { name: '电台夜话', platforms: ['YouTube'], selected: false },
  ]);
  assert.deepEqual(pickerRows(catalog, entries, '夜话').map(row => row.name), ['电台夜话']);
});

test('gone roster identities are shown removable but never saved again', () => {
  const entries = normalizeEntries([
    { platform: 'YT', channel_id: 'UC1', name: '星海频道' },
    { platform: 'YT', channel_id: 'UCgone', name: '已移除频道' },
  ]);
  assert.deepEqual(knownEntries(entries, catalog).map(entry => entry.name), ['星海频道']);
  assert.deepEqual(staleEntries(entries, catalog).map(entry => entry.name), ['已移除频道']);
});

test('the settings page wires the card and sends the fields', async () => {
  const html = await readFile(new URL('../src/index.html', import.meta.url), 'utf8');
  for (const id of [
    'warning-exempt-section', 'warning-exempt-list', 'warning-exempt-picker-toggle',
    'warning-exempt-search', 'warning-exempt-results', 'warning-exempt-reasons', 'warning-exempt-empty',
  ]) {
    assert.match(html, new RegExp(`id="${id}"`), id);
  }
  assert.match(html, /违规后可续播频道/);
  assert.match(html, /反复违规继续开播极易导致直播间被关闭。/);
  assert.match(html, /直播间锁定等不针对单个频道的限制也不会续播。/);
  const js = await readFile(new URL('../src/js/settings.js', import.meta.url), 'utf8');
  assert.match(js, /renderWarningHoldExempt\(config\)/);
  assert.match(js, /warning_hold_exempt: knownEntries\(exemptEntries, exemptCatalog\)\.map/);
  assert.match(js, /已不在频道管理中/);
  assert.match(js, /input\.dataset\.channel = row\.name/);
  assert.match(js, /warning_hold_exempt_reasons: \[\.\.\.exemptReasons\]/);
  const css = await readFile(new URL('../src/styles.css', import.meta.url), 'utf8');
  assert.match(css, /\.warning-exempt-row \{/);
  assert.match(css, /\.risk-tag--danger \{ color: var\(--danger\); \}/);
});
