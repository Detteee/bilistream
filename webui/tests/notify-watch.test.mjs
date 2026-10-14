import assert from 'node:assert/strict';
import { test } from 'node:test';
import {
  addWatch, createWatchMigrationAcknowledgements, filterCandidates, groupedWatchSupported, normalizeWatch,
  toggleWatchCandidate, watchCatalog, watchEntryError, watchForSave, watchKey,
  watchMigrationText, watchRowLive, watchRows, watchSelection,
} from '../src/js/notify-watch.js';
import { configToForm, formToPatch, mergeDraft, notifyRefusal, saveBody } from '../src/js/notify-state.js';

const configured = { kind: 'configured', name: '星のチャンネル' };
const settings = {
  configured_channels: [{ name: configured.name, platforms: ['youtube', 'twitch', 'niconico'] }],
  config: { watch: { enabled: false, channels: [] } },
  revision: 4, credentials_generation: 7,
};
const yt = { platform: 'youtube', channel_id: 'UCexample', name: configured.name };
const tw = { platform: 'twitch', channel_id: 'ExampleLogin', name: configured.name };
const legacy = { watch_candidates: [yt, tw], config: settings.config };

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
  const loaded = { ...settings.config, watch: { enabled: true, channels: [{ ...configured, unresolved: true }], migration_notice: { at: 1, folded: 2 } } };
  const list = JSON.parse(configToForm(loaded).watch_channels);
  assert.deepEqual(list, [configured]);
  const row = watchRows(list, [])[0];
  assert.equal(row.missing, true);
  assert.deepEqual(watchForSave(list, { configured_channels: [], config: loaded }).list, [configured]);
  assert.match(watchForSave(list, { configured_channels: [] }).error, /找不到/);
  assert.deepEqual(toggleWatchCandidate(list, configured, false).list, []);
});

test('legacy catalog matches saved accounts by identity, keeping partial follows and name collisions', () => {
  const catalog = watchCatalog(legacy);
  assert.equal(catalog.length, 1);
  assert.equal(groupedWatchSupported(legacy), false);
  assert.deepEqual(watchSelection(catalog[0], [yt]), { checked: true, partial: true });
  const unrelated = { platform: 'twitch', channel_id: 'unrelated', name: configured.name };
  const before = [yt, unrelated];
  const rows = watchRows(before, catalog);
  assert.equal(rows.length, 2);
  assert.equal(rows[0].unavailable, false);
  assert.equal(rows[1].historical, true);
  assert.equal(rows[1].unavailable, true);
  const all = toggleWatchCandidate(before, catalog[0], true).list;
  assert.deepEqual(all, [yt, unrelated, tw]);
  const loaded = { ...legacy, config: { watch: { channels: before } } };
  assert.deepEqual(watchForSave(all, loaded).list, all);
  assert.equal(watchForSave(all, loaded).error, '');
  assert.deepEqual(toggleWatchCandidate(all, catalog[0], false).list, [unrelated]);
});

test('legacy configured selections are atomic flat writes within the old account limit', () => {
  const before = Array.from({ length: 49 }, (_, i) => ({ platform: 'youtube', channel_id: `UC${i}`, name: `频道${i}` }));
  assert.equal(toggleWatchCandidate(before, watchCatalog(legacy)[0], true).list, before);
  assert.match(toggleWatchCandidate(before, watchCatalog(legacy)[0], true).error, /50/);
  assert.deepEqual(toggleWatchCandidate([], watchCatalog(legacy)[0], true).list, [yt, tw]);
});

test('50 grouped channels may cover more than 50 accounts', () => {
  let list = [];
  for (let i = 0; i < 50; i++) list = addWatch(list, { kind: 'configured', name: `Name ${i}` }).list;
  assert.equal(list.length, 50);
  const configured_channels = list.map(c => ({ name: c.name, platforms: ['youtube', 'twitch', 'niconico'] }));
  assert.equal(watchForSave(list, { ...settings, configured_channels }).error, '');
  assert.match(addWatch(list, { kind: 'configured', name: 'Extra' }).error, /50/);
});

test('capability loss cannot send unsupported objects or silently lose unresolved targets', () => {
  const encoded = watchForSave([configured], legacy);
  assert.deepEqual(encoded.list, [yt, tw]);
  assert.ok(encoded.list.every(c => !('kind' in c)));
  const unresolved = [{ kind: 'configured', name: '已移除' }];
  assert.equal(watchForSave(unresolved, legacy).list, unresolved);
  assert.ok(watchForSave(unresolved, legacy).error);
});

test('unavailable historical entries never show live, including stale legacy status', () => {
  const manual = { kind: 'manual', name: configured.name, platforms: { twitch: 'other' } };
  const rows = watchRows([configured, manual], watchCatalog(settings));
  const status = { watch: { live: ['twitch:other'], live_entries: [{ kind: 'manual', name: configured.name }] } };
  assert.equal(watchRowLive(rows[0], status), false);
  assert.equal(watchRowLive(rows[1], status), false);
  assert.equal(watchRowLive(rows[0], { watch: { live_entries: [configured] } }), true);
  assert.equal(watchRowLive(watchRows([tw], watchCatalog(legacy))[0], { watch: { live: ['twitch:examplelogin'] } }), true);
  const old = { platform: 'twitch', channel_id: 'unmatched', name: '旧关注项' };
  assert.equal(watchRowLive(watchRows([old], watchCatalog(legacy))[0], { watch: { live: ['twitch:unmatched'] } }), false);
});

test('new saves reject uncatalogued flat targets and unknown configured names on both API versions', () => {
  const outside = { platform: 'twitch', channel_id: 'unmatched', name: '旧关注项' };
  for (const api of [settings, legacy]) {
    assert.match(watchForSave([outside], api).error, /仅可关注/);
    assert.ok(watchForSave([{ kind: 'configured', name: '不存在' }], api).error);
  }
  assert.equal(watchForSave([yt, tw], legacy).error, '');
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

test('migration and stable refusal copy name the product channel list', () => {
  assert.match(watchMigrationText({ config: { watch: { migration_notice: { at: 1, folded: 2 } } } }), /2 项合并为频道/);
  assert.equal(watchMigrationText(settings), '');
  for (const code of ['watch_duplicate', 'watch_too_many', 'watch_unknown_channel']) {
    const refusal = notifyRefusal({ status: 400, body: { data: { code } } }, null, { route: 'config' });
    assert.equal(refusal.kind, 'watch');
    assert.doesNotMatch(refusal.banner.lead, /channels\.json/);
  }
});

test('migration acknowledgement survives reload and stays scoped to the node and migration', () => {
  const values = new Map();
  const storage = { getItem: key => values.get(key), setItem: (key, value) => values.set(key, value) };
  const notice = { at: 1792032000, folded: 1 };
  const firstPage = createWatchMigrationAcknowledgements(() => storage);
  assert.equal(firstPage.isDismissed('jp', notice), false);
  firstPage.dismiss('jp', notice);
  const reloadedPage = createWatchMigrationAcknowledgements(() => storage);
  assert.equal(reloadedPage.isDismissed('jp', { ...notice }), true);
  assert.equal(reloadedPage.isDismissed('ny', notice), false);
  assert.equal(reloadedPage.isDismissed('jp', { ...notice, at: notice.at + 1 }), false);
  assert.equal(reloadedPage.isDismissed('jp', { ...notice, folded: 2 }), false);
  assert.equal(reloadedPage.isDismissed('jp', null), false);
  // Acknowledging a later notice does not discard the earlier acknowledgement.
  reloadedPage.dismiss('jp', { ...notice, at: notice.at + 1 });
  assert.equal(createWatchMigrationAcknowledgements(() => storage).isDismissed('jp', notice), true);
});

test('storage access failure or quota exhaustion still allows page-local acknowledgement', () => {
  const notice = { at: 1792032000, folded: 1 };
  for (const storage of [
    () => { throw new Error('storage denied'); },
    () => ({ getItem() { throw new Error('read denied'); }, setItem() { throw new Error('write denied'); } }),
    () => ({ getItem() { return null; }, setItem() { throw new Error('quota'); } }),
  ]) {
    const page = createWatchMigrationAcknowledgements(storage);
    assert.equal(page.isDismissed('jp', notice), false);
    assert.doesNotThrow(() => page.dismiss('jp', notice));
    assert.equal(page.isDismissed('jp', notice), true);
    assert.equal(page.isDismissed('ny', notice), false);
    assert.equal(createWatchMigrationAcknowledgements(storage).isDismissed('jp', notice), false);
  }
});

test('unknown node or migration identity never creates an unscoped persistent dismissal', () => {
  const values = new Map();
  const page = createWatchMigrationAcknowledgements(() => ({ getItem: key => values.get(key), setItem: (key, value) => values.set(key, value) }));
  page.dismiss('', { at: 1792032000, folded: 1 });
  page.dismiss('jp', { folded: 1 });
  assert.equal(values.size, 0);
  assert.equal(page.isDismissed('', { at: 1792032000, folded: 1 }), true);
  assert.equal(page.isDismissed('ny', { at: 1792032000, folded: 1 }), false);
  assert.equal(page.isDismissed('jp', { folded: 1 }), true);
});
