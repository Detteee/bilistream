import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { test } from 'node:test';

const src = name => readFile(new URL(`../src/${name}`, import.meta.url), 'utf8');

test('basic settings expose the auto-stop toggle and its threshold', async () => {
  const html = await src('index.html');
  for (const id of ['config-auto-stop-checkbox', 'config-auto-stop-hours']) {
    assert.match(html, new RegExp(`id="${id}"`), id);
  }
  assert.match(html, /源结束后自动停播/);
  assert.match(html, /满该时长立即停播/);
  assert.match(html, /未满该时长时，源结束后等待一个检测间隔再停播。/);
  // The threshold is a day-scale number; a stray max would make it unsettable.
  assert.match(html, /id="config-auto-stop-hours" min="1" max="168"/);
});

test('settings load, save and dim the threshold with the toggle', async () => {
  const js = await src('js/settings.js');
  assert.match(
    js,
    /setCheckboxChecked\('config-auto-stop-checkbox', config\.auto_stop_after_source_end !== false\)/,
  );
  assert.match(js, /setInputValue\('config-auto-stop-hours', config\.auto_stop_threshold_hours \?\? 3\)/);
  assert.match(
    js,
    /auto_stop_after_source_end: document\.getElementById\('config-auto-stop-checkbox'\)\.checked/,
  );
  assert.match(js, /auto_stop_threshold_hours: readIntegerInput\('config-auto-stop-hours', 3\)/);
  assert.match(js, /hours\.disabled = !enabled/);
  assert.match(js, /addEventListener\('change', syncAutoStopControls\)/);
});
