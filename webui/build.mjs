// Generated dist trees are committed so ordinary Cargo builds need no Node.js.
import { build, transform } from 'esbuild';
import { readdir, readFile, mkdir, writeFile, rm } from 'node:fs/promises';
import { dirname, extname, join, relative, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { gzipSync } from 'node:zlib';

const root = dirname(fileURLToPath(import.meta.url));
const targets = ['chrome109', 'firefox115', 'safari16.4'];
const sharedPublicModules = new Set([
  'src/js/dom.js', 'src/js/format.js', 'src/js/dialog.js',
  'src/js/cluster-health.js', 'src/js/cluster-network.js', 'src/js/status-cards.js',
  'src/js/stream-model.js', 'src/js/on-air.js', 'src/js/stream-card.js',
  // The public page shares the tooltip so its chips read like the dashboard's
  // instead of falling back to native title bubbles.
  'src/js/tooltip.js',
]);

export function validatePublicModules(inputs) {
  for (const input of inputs) {
    if (!input.startsWith('public-src/js/') && !sharedPublicModules.has(input)) {
      throw new Error(`Public bundle imports a non-public module: ${input}`);
    }
  }
}

// The one runtime import a page bundle may have: the admin main.js loading the
// 统计 chunk with import('./analytics.js'). Everything else stays a single
// self-contained file, and the admin page has exactly these two outputs.
export const LAZY_CHUNKS = new Map([['dist', 'dist/js/analytics.js']]);

export function validateRuntimeImports(destination, outputs) {
  const lazy = LAZY_CHUNKS.get(destination);
  const main = `${destination}/js/main.js`;
  for (const [name, output] of Object.entries(outputs)) {
    for (const dependency of output.imports || []) {
      if (name !== main || dependency.kind !== 'dynamic-import' || dependency.path !== lazy) {
        throw new Error(`Page bundles must not depend on runtime JavaScript imports: ${name} → ${dependency.path}`);
      }
    }
  }
  const expected = [main, ...(lazy ? [lazy] : [])].sort();
  const produced = Object.keys(outputs).sort();
  if (JSON.stringify(produced) !== JSON.stringify(expected)) {
    throw new Error(`Unexpected page bundle outputs for ${destination}: ${produced.join(', ')}`);
  }
}

async function filesIn(directory) {
  const files = [];
  for (const entry of await readdir(directory, { withFileTypes: true })) {
    const path = join(directory, entry.name);
    if (entry.isDirectory()) files.push(...await filesIn(path));
    else if (entry.isFile()) files.push(path);
    else throw new Error(`UI assets must be regular files: ${path}`);
  }
  return files.sort();
}

export async function generateAssets() {
  const outputs = new Map();
  const reports = [];
  for (const [source, destination] of [['src', 'dist'], ['public-src', 'public-dist']]) {
    // Admin only: ESM splitting so the 统计 view (analytics.js + uPlot) is a
    // lazy chunk. No hash in the names: the server serves every file under
    // /_assets/<snapshot hash>/, so a relative import() stays in one version.
    const admin = source === 'src';
    const result = await build({
      absWorkingDir: root,
      entryPoints: { main: `${source}/js/main.js` },
      outdir: `${destination}/js`,
      splitting: admin, entryNames: '[name]', chunkNames: '[name]',
      bundle: true, minify: true, format: 'esm', target: targets,
      charset: 'utf8', legalComments: 'eof', write: false, metafile: true,
      logLevel: 'silent',
    });
    if (result.warnings.length) throw new Error(result.warnings.map(w => w.text).join('\n'));
    validateRuntimeImports(destination, result.metafile.outputs);
    const inputs = Object.keys(result.metafile.inputs).map(path => path.replaceAll('\\', '/'));
    if (source === 'public-src') {
      validatePublicModules(inputs);
    }
    for (const file of result.outputFiles) {
      const name = relative(root, file.path).replaceAll('\\', '/');
      outputs.set(name, file.contents);
      const modules = Object.keys(result.metafile.outputs[name]?.inputs || {}).length;
      reports.push({ page: name, modules, bytes: file.contents.length, gzip: gzipSync(file.contents).length });
    }
    for (const path of await filesIn(join(root, source))) {
      const name = relative(join(root, source), path).replaceAll('\\', '/');
      const extension = extname(name);
      if (extension === '.js') {
        if (source === 'src' && name === 'js/theme.js') {
          const result = await transform(await readFile(path, 'utf8'), { minify: true, target: targets, charset: 'utf8', legalComments: 'eof' });
          if (result.warnings.length) throw new Error(result.warnings.map(w => w.text).join('\n'));
          outputs.set(`${destination}/${name}`, Buffer.from(result.code));
        }
        continue;
      }
      if (!['.html', '.css', '.png', '.svg'].includes(extension)) {
        throw new Error(`Unexpected UI source file: ${path}`);
      }
      let bytes = await readFile(path);
      if (extension === '.css') {
        const result = await transform(bytes.toString('utf8'), { loader: 'css', minify: true, target: targets, charset: 'utf8', legalComments: 'eof' });
        if (result.warnings.length) throw new Error(result.warnings.map(w => w.text).join('\n'));
        bytes = Buffer.from(result.code);
      }
      outputs.set(`${destination}/${name}`, bytes);
    }
  }
  for (const name of ['dist/index.html', 'public-dist/index.html', 'dist/js/theme.js', 'dist/js/main.js', 'dist/js/analytics.js', 'public-dist/js/main.js']) {
    if (!outputs.has(name)) throw new Error(`Missing required UI source for ${name}`);
  }
  return { outputs, reports };
}

export async function checkOutputs(directory, outputs) {
  const expected = new Set(outputs.keys());
  const changed = [];
  for (const tree of ['dist', 'public-dist']) {
    let actual;
    try { actual = await filesIn(join(directory, tree)); }
    catch (error) { if (error.code === 'ENOENT') actual = []; else throw error; }
    for (const path of actual) {
      const name = relative(directory, path).replaceAll('\\', '/');
      const bytes = outputs.get(name);
      if (!bytes || !Buffer.from(bytes).equals(await readFile(path))) changed.push(name);
      expected.delete(name);
    }
  }
  return [...changed, ...expected].sort();
}

async function main() {
  const args = process.argv.slice(2);
  if (args.length && (args.length !== 1 || args[0] !== '--check')) throw new Error('Usage: node build.mjs [--check]');
  const { outputs, reports } = await generateAssets();
  if (args[0] === '--check') {
    const changed = await checkOutputs(root, outputs);
    if (changed.length) throw new Error(`Generated UI is stale; run npm --prefix webui run build:\n${changed.join('\n')}`);
  } else {
    // Build both pages successfully before replacing either generated tree.
    for (const tree of ['dist', 'public-dist']) await rm(join(root, tree), { recursive: true, force: true });
    for (const [name, bytes] of outputs) {
      const path = join(root, name);
      await mkdir(dirname(path), { recursive: true });
      await writeFile(path, bytes);
    }
  }
  for (const report of reports) console.log(`${report.page}: ${report.modules} modules, ${report.bytes} bytes (${report.gzip} gzip)`);
}

if (resolve(process.argv[1] || '') === fileURLToPath(import.meta.url)) {
  await main();
}
