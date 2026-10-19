import assert from 'node:assert/strict';
import { test } from 'node:test';
import {
  addWatch, filterCandidates, normalizeWatch,
  toggleWatchCandidate, watchCatalog, watchEntryError, watchForSave, watchKey,
  watchRowLive, watchRows, watchSelection,
} from '../src/js/notify-watch.js';
import { configToForm, formToPatch, mergeDraft, notifyRefusal, saveBody } from '../src/js/notify-state.js';

const configured = { kind: 'configured', name: '星のチャンネル' };
const settings = {
  configured_channels: [{ name: configured.name, platforms: ['youtube', 'twitch', 'niconico'] }],
  config: { watch: { enabled: false, channels: [] } },
  revision: 4, credentials_generation: 7,
};

test('full name catalog is browsable without a query, and selected choices stay visible', () => {
  const source = { configured_channels: Array.from({ length: 60 }, (_, i) => ({ name: `Channel ${i}`, platforms: ['youtube'] })) };
  const catalog = watchCatalog(source);
  assert.equal(filterCandidates(catalog, '').length, 60);
  assert.equal(filterCandidates(catalog, ' channel 59 ')[0].name, 'Channel 59');
  assert.equal(filterCandidates(catalog, 'YouTube').length, 0);
  const selected = toggleWatchCandidate([], catalog[59], true).list;
  assert.equal(filterCandidates(catalog, '').length, 60);
  assert.deepEqual(watchSelection(catalog[59], selected), { checked: true, partial: false });
});

test('one configured name selects all platforms in one entry and one generation-aware save', () => {
  const candidate = watchCatalog(settings)[0];
  const added = toggleWatchCandidate([], candidate, true);
  assert.equal(added.error, '');
  assert.deepEqual(added.list, [configured]);
  const base = configToForm(settings.config);
  const form = { ...base, watch_channels: JSON.stringify(added.list) };
  const body = saveBody(settings, formToPatch(form, base));
  assert.deepEqual(body.config.watch, { enabled: false, channels: [configured] });
  assert.equal(body.base_generation, 7);
  assert.equal(body.base_watch_revision, undefined);
  const synced = saveBody({ ...settings, watch_revision: 3 }, formToPatch(form, base));
  assert.equal(synced.base_watch_revision, 3);
  assert.equal(body.expected_revision, 4);
  assert.deepEqual(toggleWatchCandidate(added.list, candidate, false).list, []);
});

test('historical unmatched entries are unavailable and may only be retained unchanged or removed', () => {
  const historical = { kind: 'manual', name: configured.name, platforms: { youtube: 'UCmanual', twitch: 'Example' } };
  const loaded = { ...settings, config: { watch: { enabled: true, channels: [historical] } } };
  const row = watchRows([historical], watchCatalog(loaded))[0];
  assert.equal(row.historical, true);
  assert.equal(row.unavailable, true);
  assert.equal(watchSelection(watchCatalog(loaded)[0], [historical]).checked, false, 'display-name collision cannot select a configured channel');
  assert.deepEqual(watchForSave([historical, configured], loaded).list, [historical, configured]);
  assert.equal(watchForSave([], loaded).error, '');
  assert.match(watchEntryError(historical), /仅可关注/);
  assert.match(addWatch([], historical).error, /仅可关注/);
  assert.match(watchForSave([historical], settings).error, /仅可关注/);
  assert.match(watchForSave([{ ...historical, platforms: { twitch: 'replacement' } }], loaded).error, /仅可关注/);
});

test('unresolved configured entries remain removable and round-trip without response metadata', () => {
  const loaded = { ...settings.config, watch: { enabled: true, channels: [{ ...configured, unresolved: true }] } };
  const list = JSON.parse(configToForm(loaded).watch_channels);
  assert.deepEqual(list, [configured]);
  const row = watchRows(list, [])[0];
  assert.equal(row.missing, true);
  assert.deepEqual(watchForSave(list, { configured_channels: [], config: loaded }).list, [configured]);
  assert.match(watchForSave(list, { configured_channels: [] }).error, /找不到/);
  assert.deepEqual(toggleWatchCandidate(list, configured, false).list, []);
});

test('50 grouped channels may cover more than 50 accounts', () => {
  let list = [];
  for (let i = 0; i < 50; i++) list = addWatch(list, { kind: 'configured', name: `Name ${i}` }).list;
  assert.equal(list.length, 50);
  const configured_channels = list.map(c => ({ name: c.name, platforms: ['youtube', 'twitch', 'niconico'] }));
  assert.equal(watchForSave(list, { ...settings, configured_channels }).error, '');
  assert.match(addWatch(list, { kind: 'configured', name: 'Extra' }).error, /50/);
});

test('unavailable historical entries are never live', () => {
  const manual = { kind: 'manual', name: configured.name, platforms: { twitch: 'other' } };
  const rows = watchRows([configured, manual], watchCatalog(settings));
  const status = { watch: { live_entries: [{ kind: 'manual', name: configured.name }] } };
  assert.equal(watchRowLive(rows[0], status), false);
  assert.equal(watchRowLive(rows[1], status), false);
  assert.equal(watchRowLive(rows[0], { watch: { live_entries: [configured] } }), true);
});
test('new saves reject flat account entries and unknown configured names', () => {
  const outside = { platform: 'twitch', channel_id: 'unmatched', name: '旧关注项' };
  assert.match(watchForSave([outside], settings).error, /找不到|仅可关注/);
  assert.ok(watchForSave([{ kind: 'configured', name: '不存在' }], settings).error);
  assert.equal(watchForSave([configured], settings).error, '');
});
test('remote settings refresh preserves a watch draft and advances unchanged fields', () => {
  const previous = configToForm(settings.config);
  const draft = { ...previous, watch_channels: JSON.stringify([configured]) };
  const remote = configToForm({ ...settings.config, cooldown_secs: 3600 });
  const merged = mergeDraft(previous, remote, draft, ['watch_channels', 'cooldown_mins']);
  assert.equal(merged.watch_channels, draft.watch_channels);
  assert.equal(merged.cooldown_mins, '60');
  assert.deepEqual(normalizeWatch(JSON.parse(merged.watch_channels)), [configured]);
});

test('stable refusal copy names the product channel list', () => {
  for (const code of ['watch_duplicate', 'watch_too_many', 'watch_unknown_channel']) {
    const refusal = notifyRefusal({ status: 400, body: { data: { code } } }, null, { route: 'config' });
    assert.equal(refusal.kind, 'watch');
    assert.doesNotMatch(refusal.banner.lead, /channels\.json/);
  }
});

test('a stale follow-list base refreshes and is not saved again by itself', () => {
  for (const code of ['watch_conflict', 'base_watch_revision_required']) {
    const refusal = notifyRefusal(
      { status: code === 'watch_conflict' ? 409 : 400, body: { success: false, message: 'stale', data: { code, current_revision: 4 } } },
      null,
      { route: 'config' },
    );
    assert.equal(refusal.kind, 'credentials_conflict');
    assert.equal(refusal.banner.action.target, 'refresh');
    assert.equal(refusal.banner.action.label, '刷新');
  }
});
