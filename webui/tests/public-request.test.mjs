import assert from 'node:assert/strict';
import { test } from 'node:test';
import { createJsonPoller } from '../public-src/js/request.js';

const json = (value, etag) => new Response(JSON.stringify(value), {
  headers: etag ? { etag } : {},
});

test('overlapping polls share one request per URL', async () => {
  const requests = [];
  const get = createJsonPoller((path) => new Promise((resolve) => requests.push({ path, resolve })));
  const first = get('/status');
  assert.strictEqual(get('/status'), first);
  const other = get('/streams');
  await Promise.resolve();
  assert.deepEqual(requests.map(({ path }) => path), ['/status', '/streams']);
  requests[0].resolve(json({ live: true }, 'v1'));
  requests[1].resolve(json([]));
  await Promise.all([first, other]);
});

test('304 replays the successful body after a transient error', async () => {
  const headers = [];
  const responses = [json({ live: true }, 'v1'), new Response(null, { status: 503 }), new Response(null, { status: 304 })];
  const get = createJsonPoller(async (_, options) => {
    headers.push(options.headers);
    return responses.shift();
  });
  const body = await get('/status');
  await assert.rejects(get('/status'), /503/);
  assert.strictEqual(await get('/status'), body);
  assert.equal(headers[2]['If-None-Match'], 'v1');
});

test('malformed JSON never replaces the last valid ETag or representation', async () => {
  const headers = [];
  const responses = [json([1], 'v1'), new Response('{', { headers: { etag: 'bad' } }), new Response(null, { status: 304 })];
  const get = createJsonPoller(async (_, options) => {
    headers.push(options.headers);
    return responses.shift();
  });
  const body = await get('/streams');
  await assert.rejects(get('/streams'), SyntaxError);
  assert.strictEqual(await get('/streams'), body);
  assert.equal(headers[2]['If-None-Match'], 'v1');
});

test('a representation without an ETag clears the old validator', async () => {
  const headers = [];
  const responses = [json(1, 'v1'), json(2), json(3)];
  const get = createJsonPoller(async (_, options) => {
    headers.push(options.headers);
    return responses.shift();
  });
  await get('/status');
  await get('/status');
  await get('/status');
  assert.equal(headers[2]['If-None-Match'], undefined);
});

test('unexpected 304 rejects and leaves the URL available for retry', async () => {
  const responses = [new Response(null, { status: 304 }), json({ recovered: true })];
  const get = createJsonPoller(async () => responses.shift());
  await assert.rejects(get('/status'), /without a cached/);
  assert.deepEqual(await get('/status'), { recovered: true });
});

test('the deadline aborts a stalled poll and allows the next one', async () => {
  let calls = 0;
  const get = createJsonPoller((_, { signal }) => {
    if (++calls > 1) return Promise.resolve(json({ recovered: true }));
    return new Promise((_, reject) => signal.addEventListener('abort', () => reject(signal.reason), { once: true }));
  }, 5);
  await assert.rejects(get('/status'), { name: 'AbortError' });
  assert.deepEqual(await get('/status'), { recovered: true });
});

test('synchronous fetch errors also release the pending entry', async () => {
  let calls = 0;
  const get = createJsonPoller(() => {
    if (++calls === 1) throw new Error('fetch unavailable');
    return Promise.resolve(json([]));
  });
  await assert.rejects(get('/streams'), /fetch unavailable/);
  assert.deepEqual(await get('/streams'), []);
});
