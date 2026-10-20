// live-events-view.js — §17 DOM updates for the 通知类型 group, kept apart
// from notify.js so they can run against a fake DOM in node --test.
import { LIVE_EVENTS_HINT, LIVE_EVENTS_HINT_PUSH, activePushView, liveEventsGate, pickBudget, wakeupBudgetView } from './notify-state.js';

export const STATUS_STALE_MS = 90_000;  // three missed polls
export const LIVE_EVENTS_TICK_MS = 10_000;

const setText = (el, text) => { if (el && el.textContent !== text) el.textContent = text; };
const setHidden = (el, hidden) => { if (el && el.hidden !== hidden) el.hidden = hidden; };

// Budget text as phrases; each phrase is its own nowrap span, so the line can
// only break at the ` · ` / `，` separators between them (§17.6, review B3).
// The producer hands over the phrases, so the layout contract lives in one
// file: re-parsing `main` here used to make any copy edit fail silently into a
// single unbreakable span.
export function budgetSegments(view) {
  if (Array.isArray(view?.segments)) return view.segments;
  return [{ text: view?.main ?? '' }];
}

function fillSegments(doc, el, segments) {
  const sig = JSON.stringify(segments);
  if (el.dataset.sig === sig) return;   // no flicker on identical polls
  el.dataset.sig = sig;
  el.replaceChildren(...segments.map(seg => {
    if (seg.sep) return doc.createTextNode(seg.sep);
    const span = doc.createElement('span');
    span.className = seg.suffix ? 'notify-budget-seg notify-budget-suffix' : 'notify-budget-seg';
    span.textContent = seg.text;
    return span;
  }));
}

// els: { group, line, main, next, passive, error, owner, input, help }.
// state: { status, settings, statusAt, now, error, disabled }.
// `disabled` is the panel-wide lock (loading/busy); the writer gate adds to it,
// so a redraw both disables and re-enables the switch (Clerk L2).
// #notify-types starts hidden in the markup; the first render reveals it.
export function renderLiveEventsInto(doc, els, { status, settings, statusAt, now, error = '', disabled = false }) {
  setHidden(els.group, false);
  if (!els.line) return null;
  const available = Boolean(status) && now - statusAt < STATUS_STALE_MS;
  const push = status && 'active_push' in status ? status.active_push : undefined;
  const view = wakeupBudgetView(pickBudget(status, settings), { available, push });
  setText(els.help, push !== undefined ? LIVE_EVENTS_HINT_PUSH : LIVE_EVENTS_HINT);
  if (els.line.dataset.tone !== view.tone) els.line.dataset.tone = view.tone;
  fillSegments(doc, els.main, budgetSegments(view));
  const nextSegs = view.next ? [{ sep: ' · ' }, { text: view.next.replace(/^ · /, '') }] : [];
  fillSegments(doc, els.next, nextSegs);
  setHidden(els.next, !view.next);
  setHidden(els.passive, !view.passive);
  setLiveEventsError(els, error);
  const gate = liveEventsGate(status);
  setText(els.owner, gate.enabled ? '' : gate.hint);
  setHidden(els.owner, gate.enabled);
  if (els.input) {
    const off = disabled || !gate.enabled;
    if (els.input.disabled !== off) els.input.disabled = off;
  }
  return view;
}

// Review B2: the error is linked to the switch and marks it invalid only
// while it shows; the error element itself is role="alert" in the markup.
const ERROR_ID = 'notify-live-events-error';
export function setLiveEventsError(els, text) {
  setText(els.error, text);
  setHidden(els.error, !text);
  const input = els.input;
  if (!input) return;
  const ids = (input.getAttribute('aria-describedby') || '').split(/\s+/).filter(id => id && id !== ERROR_ID);
  if (text) ids.push(ERROR_ID);
  const next = ids.join(' ');
  if (input.getAttribute('aria-describedby') !== next) input.setAttribute('aria-describedby', next);
  if (text) input.setAttribute('aria-invalid', 'true');
  else input.removeAttribute('aria-invalid');
}

// §17.8 主动推送 value: one nowrap span per segment, joined by ' · ' text so
// it wraps only at the separators. Returns the value element or null.
export function activePushValueInto(doc, status, { available = true } = {}) {
  const view = activePushView(status, { available });
  if (!view) return null;
  const v = doc.createElement('span');
  v.className = 'cluster-node-meta-value notify-status-value notify-push';
  if (view.tone) v.dataset.tone = view.tone;
  view.segments.forEach((text, i) => {
    if (i) v.append(doc.createTextNode(' · '));
    const span = doc.createElement('span');
    span.className = 'notify-push-seg';
    span.textContent = text;
    v.append(span);
  });
  return v;
}
