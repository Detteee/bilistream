import test from 'node:test';
import assert from 'node:assert/strict';
import { mkdtemp, mkdir, writeFile, rm } from 'node:fs/promises';
import { join } from 'node:path';
import { tmpdir } from 'node:os';
import { fileURLToPath } from 'node:url';
import { generateAssets, checkOutputs, validatePublicModules } from '../build.mjs';

test('public bundle cannot silently import admin-only or third-party modules', () => {
  validatePublicModules([
    'public-src/js/main.js',
    'src/js/status-cards.js',
    'src/js/stream-model.js',
    'src/js/on-air.js',
    'src/js/stream-card.js',
  ]);
  for (const name of ['src/js/api.js', 'src/js/settings.js', 'src/js/overview.js', 'src/js/state.js', 'node_modules/example/index.js']) {
    assert.throws(() => validatePublicModules(['public-src/js/main.js', name]), /non-public module/);
  }
});

test('generated-file checking catches modified, missing and obsolete assets', async () => {
  const root = await mkdtemp(join(tmpdir(), 'bilistream-build-check-'));
  try {
    await mkdir(join(root, 'dist/js'), { recursive: true });
    await writeFile(join(root, 'dist/js/main.js'), 'old');
    await writeFile(join(root, 'dist/js/obsolete.js'), 'old module');
    const outputs = new Map([['dist/js/main.js', Buffer.from('new')], ['public-dist/index.html', Buffer.from('page')]]);
    assert.deepEqual(await checkOutputs(root, outputs), ['dist/js/main.js', 'dist/js/obsolete.js', 'public-dist/index.html']);
    await writeFile(join(root, 'dist/js/main.js'), 'new');
    await rm(join(root, 'dist/js/obsolete.js'));
    await mkdir(join(root, 'public-dist'));
    await writeFile(join(root, 'public-dist/index.html'), 'page');
    assert.deepEqual(await checkOutputs(root, outputs), []);
  } finally { await rm(root, { recursive: true, force: true }); }
});

test('committed bundles match the pinned build and contain only runtime assets', async () => {
  const { outputs } = await generateAssets();
  assert.deepEqual(await checkOutputs(fileURLToPath(new URL('..', import.meta.url)), outputs), []);
  assert.deepEqual([...outputs.keys()].filter(name => name.endsWith('.js')).sort(), [
    'dist/js/main.js', 'dist/js/theme.js', 'public-dist/js/main.js',
  ]);
});
