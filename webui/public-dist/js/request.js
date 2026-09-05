// One in-flight request and one successfully parsed representation per URL.
export function createJsonPoller(fetchImpl = (...args) => fetch(...args), timeoutMs = 10_000) {
  const cached = new Map();
  const pending = new Map();

  return function getJson(path) {
    if (pending.has(path)) {
      return pending.get(path);
    }
    // Start in a microtask so even a synchronously throwing fetch implementation
    // cannot finish before the pending entry is installed.
    const request = Promise.resolve().then(async () => {
      const controller = new AbortController();
      const timer = setTimeout(() => controller.abort(), timeoutMs);
      try {
        const previous = cached.get(path);
        const headers = { Accept: 'application/json' };
        if (previous?.etag) {
          headers['If-None-Match'] = previous.etag;
        }
        const response = await fetchImpl(path, { headers, cache: 'no-cache', signal: controller.signal });
        if (response.status === 304) {
          if (!previous) {
            throw new Error('304 without a cached representation');
          }
          // Replay the body so a transient error display can recover on a 304.
          return previous.body;
        }
        if (!response.ok) {
          throw new Error(`HTTP ${response.status}`);
        }
        const body = await response.json();
        // Parsing must succeed before committing either the body or its ETag.
        cached.set(path, { body, etag: response.headers.get('etag') });
        return body;
      } finally {
        clearTimeout(timer);
        pending.delete(path);
      }
    });
    pending.set(path, request);
    return request;
  };
}
