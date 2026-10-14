import test from 'node:test';
import assert from 'node:assert/strict';
import { mkdtemp, mkdir, writeFile, rm } from 'node:fs/promises';
import { join } from 'node:path';
import { tmpdir } from 'node:os';
import { fileURLToPath } from 'node:url';
import { readFile } from 'node:fs/promises';
import { generateAssets, checkOutputs, validatePublicModules, validateRuntimeImports } from '../build.mjs';

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
    'dist/js/analytics.js', 'dist/js/main.js', 'dist/js/theme.js', 'public-dist/js/main.js',
  ]);
});

const lazy = { kind: 'dynamic-import', path: 'dist/js/analytics.js' };
test('the only runtime import allowed is main.js → import(./analytics.js)', () => {
  validateRuntimeImports('dist', { 'dist/js/main.js': { imports: [lazy] }, 'dist/js/analytics.js': { imports: [] } });
  validateRuntimeImports('public-dist', { 'public-dist/js/main.js': { imports: [] } });
  const fails = (destination, outputs) => assert.throws(() => validateRuntimeImports(destination, outputs), /runtime JavaScript imports|Unexpected page bundle outputs/);
  // a static import of the chunk, another dynamic import, a shared chunk, an import from the chunk itself
  fails('dist', { 'dist/js/main.js': { imports: [{ kind: 'import-statement', path: 'dist/js/analytics.js' }] }, 'dist/js/analytics.js': { imports: [] } });
  fails('dist', { 'dist/js/main.js': { imports: [lazy, { kind: 'dynamic-import', path: 'dist/js/other.js' }] }, 'dist/js/analytics.js': { imports: [] }, 'dist/js/other.js': { imports: [] } });
  fails('dist', { 'dist/js/main.js': { imports: [lazy, { kind: 'import-statement', path: 'dist/js/chunk.js' }] }, 'dist/js/analytics.js': { imports: [{ kind: 'import-statement', path: 'dist/js/chunk.js' }] }, 'dist/js/chunk.js': { imports: [] } });
  fails('dist', { 'dist/js/main.js': { imports: [lazy] }, 'dist/js/analytics.js': { imports: [{ kind: 'dynamic-import', path: 'https://cdn.example/x.js' }] } });
  fails('dist', { 'dist/js/main.js': { imports: [] } }); // chunk missing
  // the public page may not lazy-load anything, not even analytics.js
  fails('public-dist', { 'public-dist/js/main.js': { imports: [{ kind: 'dynamic-import', path: 'public-dist/js/analytics.js' }] }, 'public-dist/js/analytics.js': { imports: [] } });
});

test('a real build with any other runtime import still fails', async () => {
  const { build } = await import('esbuild');
  const root = await mkdtemp(join(tmpdir(), 'bilistream-build-guard-'));
  try {
    await writeFile(join(root, 'main.js'), "import('./analytics.js'); import('./extra.js');");
    await writeFile(join(root, 'analytics.js'), 'export const a = 1;');
    await writeFile(join(root, 'extra.js'), 'export const b = 2;');
    const result = await build({ absWorkingDir: root, entryPoints: { main: 'main.js' }, outdir: 'dist/js', splitting: true, bundle: true, format: 'esm', entryNames: '[name]', chunkNames: '[name]', write: false, metafile: true, logLevel: 'silent' });
    assert.throws(() => validateRuntimeImports('dist', result.metafile.outputs), /runtime JavaScript imports: dist\/js\/main.js → dist\/js\/extra.js/);
  } finally { await rm(root, { recursive: true, force: true }); }
});

test('uPlot lives only in the lazy chunk, and the public page never references it', async () => {
  const { outputs } = await generateAssets();
  const text = name => Buffer.from(outputs.get(name)).toString('utf8');
  const main = text('dist/js/main.js');
  const chunk = text('dist/js/analytics.js');
  assert.match(chunk, /u-over/, 'uPlot is bundled into analytics.js');
  assert.match(chunk, /MIT/, 'uPlot licence comment kept (legalComments: eof)');
  assert.doesNotMatch(main, /u-over|u-cursor-pt|uPlot/, 'main.js carries no uPlot code');
  assert.equal((main.match(/import\(/g) || []).length, 1);
  assert.match(main, /import\("\.\/analytics\.js"\)/, 'relative import, no hash, same /_assets/<version>/ prefix');
  for (const name of [...outputs.keys()].filter(n => n.startsWith('public-dist/') && /\.(js|html|css)$/.test(n))) {
    assert.doesNotMatch(text(name), /analytics/i, `${name} must not reference the 统计 chunk`);
  }
  const lock = JSON.parse(await readFile(new URL('../package-lock.json', import.meta.url), 'utf8'));
  assert.equal(lock.packages['node_modules/uplot'].version, '1.6.32');
  assert.equal(JSON.parse(await readFile(new URL('../package.json', import.meta.url), 'utf8')).dependencies.uplot, '1.6.32');
});
