// analytics-loader.js — loads the lazy 统计 chunk (W14 P1) and owns its
// failure state. Lives in main.js on purpose: when the chunk can't load, none
// of its code is available, so the error panel must be built here.
//
// The import is relative ('./analytics.js'): the page is served under
// /_assets/<snapshot version>/, so the chunk always comes from the same
// version as main.js. A failed import() is cached by the browser for that URL,
// so recovery is a full page reload (which also picks up a new version).

export const CHUNK_ERROR_TITLE = '统计模块加载失败';
export const CHUNK_ERROR_BODY = '页面可能已更新，请重新加载后再试。';
export const CHUNK_ERROR_BUTTON = '重新加载页面';

// Replaces the view's content with the error panel and focuses the button.
export function renderChunkError(view, { doc = view.ownerDocument, reload } = {}) {
  const el = (tag, className, text) => {
    const node = doc.createElement(tag);
    if (className) node.className = className;
    if (text !== undefined) node.textContent = text;
    return node;
  };
  const panel = el('section', 'panel analytics-chunk-error');
  panel.setAttribute('role', 'alert');
  panel.setAttribute('aria-labelledby', 'analytics-chunk-error-title');
  const head = el('div', 'panel__head');
  const title = el('h2', 'panel__title', CHUNK_ERROR_TITLE);
  title.id = 'analytics-chunk-error-title';
  head.append(title);
  const body = el('div', 'panel__body');
  const text = el('p', 'hint', CHUNK_ERROR_BODY);
  text.dataset.tone = 'danger';
  const button = el('button', 'btn-primary analytics-reload-btn', CHUNK_ERROR_BUTTON);
  button.type = 'button';
  button.id = 'analytics-reload-btn';
  button.addEventListener('click', () => reload());
  body.append(text, button);
  panel.append(head, body);
  for (const child of [...view.children]) child.hidden = true;
  view.append(panel);
  view.setAttribute('aria-busy', 'false');
  button.focus();
  return panel;
}

// importer: () => import('./analytics.js'); getView: () => the #view-analytics
// element; deps: injected app functions (keeps the chunk free of app modules).
export function createAnalyticsLoader({ importer, getView, deps, reload, isActive = () => true }) {
  let pending = null;
  let mounted = null;
  let failed = false;

  function load() {
    if (!pending) {
      pending = Promise.resolve().then(importer);
      // Swallow here so a prefetch failure never surfaces as unhandled; mount()
      // reports it.
      pending.catch(() => {});
    }
    return pending;
  }

  async function mount() {
    const view = getView();
    if (!view || failed) return null;
    try {
      const module = await load();
      // The user may have left 统计 while the chunk was loading.
      if (!isActive()) return null;
      mounted = module;
      module.mountAnalytics(view, deps);
      return module;
    } catch (error) {
      failed = true;
      renderChunkError(view, { reload });
      return null;
    }
  }

  return {
    load,
    prefetch: () => { load(); },
    mount,
    unmount: () => mounted?.unmountAnalytics(),
    refreshIfVisible: () => mounted?.refreshIfVisible(),
    get failed() { return failed; },
  };
}
