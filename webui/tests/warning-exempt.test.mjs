import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { test } from 'node:test';
import {
  DEFAULT_EXEMPT_REASONS, EXEMPT_REASON_ROWS, HIGH_RISK_WARNING, LOW_RISK_REASON_IDS, channelReasonRows,
  channelSelected, groupedEntries, isLowRiskReason, knownEntries, normalizeEntries, normalizeReasons,
  pickerRows, platformLabel, removeChannel, staleEntries, toggleChannel,
} from '../src/js/warning-exempt.js';

const catalog = [
  { name: '星海频道', platforms: [{ platform: 'YT', channel_id: 'UC1' }, { platform: 'TW', channel_id: 'starsea' }] },
  { name: '电台夜话', platforms: [{ platform: 'YT', channel_id: 'UC2' }] },
];

test('the built-in reason rows mirror the PRD table and default to low risk', () => {
  assert.deepEqual(EXEMPT_REASON_ROWS.map(row => row.id), [
    'idle_recording', 'game_mode_off', 'game_info_violation', 'content_unsuitable', 'unrecognized',
  ]);
  assert.deepEqual([...DEFAULT_EXEMPT_REASONS], ['idle_recording']);
  assert.equal(EXEMPT_REASON_ROWS[0].tone, '');
  assert.equal(EXEMPT_REASON_ROWS[1].tone, 'warn');
  assert.equal(EXEMPT_REASON_ROWS[2].tone, 'danger');
  assert.equal(EXEMPT_REASON_ROWS[3].tone, 'danger');
  assert.equal(EXEMPT_REASON_ROWS[4].tone, 'danger');
  assert.equal(EXEMPT_REASON_ROWS[4].name, '未识别的原因');
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

test('low risk is global while only mid/high reasons use channel filters', () => {
  assert.deepEqual([...LOW_RISK_REASON_IDS], ['idle_recording']);
  assert.equal(isLowRiskReason('idle_recording'), true);
  assert.equal(isLowRiskReason('game_mode_off'), false);
  assert.deepEqual(
    channelReasonRows().map(row => row.id),
    ['game_mode_off', 'game_info_violation', 'content_unsuitable', 'unrecognized'],
  );
});

test('the settings page wires the card and sends the fields', async () => {
  const html = await readFile(new URL('../src/index.html', import.meta.url), 'utf8');
  for (const id of [
    'warning-exempt-section', 'warning-exempt-list', 'warning-exempt-picker-toggle',
    'warning-exempt-search', 'warning-exempt-results', 'warning-exempt-reasons',
    'warning-exempt-empty', 'warning-exempt-all-low-risk',
  ]) {
    assert.match(html, new RegExp(`id="${id}"`), id);
  }
  assert.match(html, /违规后可续播/);
  assert.match(html, /应用到全部频道/);
  assert.match(html, /中高风险 · 指定频道/);
  assert.match(html, /反复违规继续开播极易导致直播间被关闭。/);
  assert.match(html, /未勾选「未识别的原因」时/);
  assert.match(html, /直播间锁定等不针对单个频道的限制也不会续播。/);
  assert.doesNotMatch(html, /识别规则/);
  const js = await readFile(new URL('../src/js/settings.js', import.meta.url), 'utf8');
  assert.match(js, /renderWarningHoldExempt\(config\)/);
  assert.match(js, /warning_hold_exempt: knownEntries\(exemptEntries, exemptCatalog\)\.map/);
  assert.match(js, /已不在频道管理中/);
  assert.match(js, /input\.dataset\.channel = row\.name/);
  assert.match(js, /warning_hold_exempt_all_low_risk: exemptAllLowRisk/);
  assert.match(js, /channelReasonRows\(\)\.map/);
  assert.match(js, /warning_hold_exempt_reasons: normalizeReasons\(exemptReasons\)/);
  assert.match(js, /patch\.expected\.warning_hold_exempt = structuredClone\(exemptServerEntries\)/);
  const css = await readFile(new URL('../src/styles.css', import.meta.url), 'utf8');
  assert.match(css, /\.warning-exempt-row \{/);
  assert.match(css, /\.risk-tag--danger \{ color: var\(--danger\); \}/);
});

test('channel folds use the public-fold chevron and hide the native marker', async () => {
  const html = await readFile(new URL('../src/index.html', import.meta.url), 'utf8');
  const css = await readFile(new URL('../src/styles.css', import.meta.url), 'utf8');
  assert.match(html, /class="channel-picker warning-exempt-picker"/);
  assert.match(html, /class="channel-picker notify-watch-picker"/);
  assert.match(html, /id="warning-exempt-picker-toggle"[\s\S]*?polyline points="6 9 12 15 18 9"/);
  assert.match(html, /id="notify-watch-picker-toggle"[\s\S]*?polyline points="6 9 12 15 18 9"/);
  assert.equal([...html.matchAll(/class="channel-picker__chevron"/g)].length, 2);
  assert.equal([...html.matchAll(/class="channel-picker__ripples"/g)].length, 2);
  assert.doesNotMatch(html, /warning-exempt-chevron|notify-watch-chevron|⌄|warning-exempt-rules/);
  assert.doesNotMatch(
    html.slice(html.indexOf('warning-exempt-picker'), html.indexOf('warning-exempt-picker-content') + 40),
    /href="#i-chevron"/,
  );
  assert.match(css, /\.channel-picker > summary::-webkit-details-marker/);
  assert.match(css, /\.channel-picker > summary::marker/);
  assert.match(css, /\.channel-picker__chevron-icon/);
  assert.match(css, /\.channel-picker__ripples/);
  assert.match(css, /\.channel-picker\[open\] > summary \.channel-picker__chevron/);
});

test('违规后可续播频道 sits below 禁用关键词', async () => {
  const html = await readFile(new URL('../src/index.html', import.meta.url), 'utf8');
  const keywords = html.indexOf('禁用关键词');
  const exempt = html.indexOf('id="warning-exempt-section"');
  const actions = html.indexOf('id="save-system-config-btn"');
  assert.ok(keywords > 0 && exempt > keywords && actions > exempt);
});
