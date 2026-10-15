// One queue for overview, detail and cache revalidation. The history API
// admits two queries per node, including the time spent waiting for peers.
import { historyQuery } from './analytics-format.js';

const CONCURRENCY = 2;
const FRESH_MS = 30_000;
const STALE_MS = 600_000;
const CACHE_MAX = 20;
const BUSY_RETRY_MS = [250, 500, 1000, 1500];
const cancelled = () => Object.assign(new Error('统计请求已取消'), { name: 'AbortError' });

export function createHistoryClient(getJson, {
  now = Date.now,
  delay = ms => new Promise(resolve => setTimeout(resolve, ms)),
} = {}) {
  const cache = new Map();
  const pending = new Map();
  const queue = [];
  let active = 0;
  let epoch = 0;

  function finish(job, error, data) {
    pending.delete(job.key);
    for (const listener of job.listeners) {
      listener.signal?.removeEventListener('abort', listener.abort);
      if (error) listener.reject(error);
      else listener.resolve(data);
    }
    job.listeners.clear();
  }

  async function run(job) {
    // Hold the representation used by this validator, even if another
    // request evicts it from the LRU while this HTTP request is in flight.
    const previous = job.epoch === epoch ? cache.get(job.path) : null;
    let retriedWithoutValidator = false;
    for (let attempt = 0; ; attempt++) {
      if (!job.listeners.size) throw cancelled();
      try {
        const reply = await getJson(job.path, {
          conditional: true,
          etag: retriedWithoutValidator ? undefined : previous?.etag,
        });
        if (reply?.notModified && !previous) {
          if (retriedWithoutValidator) throw new Error('统计缓存已失效，请重试');
          retriedWithoutValidator = true;
          continue;
        }
        const data = reply?.notModified ? previous.data : reply?.data ?? null;
        const etag = reply?.etag ?? (reply?.notModified ? previous.etag : null);
        if (job.epoch === epoch && job.listeners.size) {
          cache.delete(job.path);
          cache.set(job.path, { at: now(), data, etag });
          while (cache.size > CACHE_MAX) cache.delete(cache.keys().next().value);
        }
        return data;
      } catch (error) {
        if (error?.status !== 429 || attempt >= BUSY_RETRY_MS.length) throw error;
        await delay(BUSY_RETRY_MS[attempt]);
      }
    }
  }

  function drain() {
    while (active < CONCURRENCY && queue.length) {
      const job = queue.shift();
      if (!job.listeners.size) { pending.delete(job.key); continue; }
      job.started = true;
      active++;
      run(job).then(data => finish(job, null, data), error => finish(job, error)).finally(() => {
        active--;
        drain();
      });
    }
  }

  function fetchFresh(path, signal) {
    if (signal?.aborted) return Promise.reject(cancelled());
    const key = `${epoch}:${path}`;
    let job = pending.get(key);
    if (!job) {
      job = { key, path, epoch, started: false, listeners: new Set() };
      pending.set(key, job);
      queue.push(job);
    }
    const promise = new Promise((resolve, reject) => {
      const listener = { resolve, reject, signal, abort: null };
      listener.abort = () => {
        job.listeners.delete(listener);
        reject(cancelled());
        if (!job.started && !job.listeners.size) {
          const index = queue.indexOf(job);
          if (index !== -1) queue.splice(index, 1);
          pending.delete(job.key);
        }
        // An in-flight HTTP cancellation may leave the server query alive.
        // Keep its slot until it settles instead of over-admitting new work.
      };
      job.listeners.add(listener);
      signal?.addEventListener('abort', listener.abort, { once: true });
    });
    drain();
    return promise;
  }

  return {
    clear() { epoch++; cache.clear(); },
    request(name, params = {}, { signal, onCached } = {}) {
      if (signal?.aborted) return Promise.reject(cancelled());
      const path = `/api/history/${name}?${historyQuery(params)}`;
      const hit = cache.get(path);
      const age = hit ? now() - hit.at : Infinity;
      if (hit && age < FRESH_MS) return Promise.resolve(hit.data);
      if (hit && age < STALE_MS) onCached?.(hit.data);
      // Callers keep cached rows visible, label them while this is pending,
      // and handle a failed revalidation just like a foreground request.
      return fetchFresh(path, signal);
    },
  };
}

export function localHistoryScope(status, scope) {
  return scope === 'local' || (scope && scope === status?.node_id)
    || (!scope && status?.cluster_nodes?.length === 1);
}

export function historyStateNotice(status, scope, hasData) {
  const local = localHistoryScope(status, scope);
  if (local && status?.enabled === false) return { text: '本节点统计已关闭。', tone: '' };
  if (local && status?.writer?.state && status.writer.state !== 'running') {
    return { text: '本节点统计记录已暂停，请查看日志。已有历史仍可读取。', tone: 'warn' };
  }
  if (!hasData) return { text: '此时间范围内暂无历史数据。', tone: '' };
  return null;
}
