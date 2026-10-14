// live-events-view.js — §17 DOM updates for the 通知类型 group, kept apart
// from notify.js so they can run against a fake DOM in node --test.
import { liveEventsGate, pickBudget, wakeupBudgetView } from './notify-state.js';

export const STATUS_STALE_MS = 90_000;  // three missed polls
export const LIVE_EVENTS_TICK_MS = 10_000;

const setText = (el, text) => { if (el && el.textContent !== text) el.textContent = text; };
const setHidden = (el, hidden) => { if (el && el.hidden !== hidden) el.hidden = hidden; };

// Budget text as phrases; each phrase is its own nowrap span, so the line can
// only break at the ` · ` / `，` separators between them (§17.6, review B3).
export function budgetSegments(view) {
  const m = /^唤醒额度 (\d+\/4) · (保留 \d+ 用于故障提醒)(，(开播\/下播只用被动回复))?$/.exec(view.main);
  if (!m) return [{ text: view.main }];
  const out = [{ text: `唤醒额度 ${m[1]}` }, { sep: ' · ' }, { text: m[2] }];
  if (m[4]) out.push({ sep: '，' }, { text: m[4], suffix: true });
  return out;
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

// els: { group, line, main, next, passive, error, owner, input }.
// state: { status, settings, statusAt, now, error, supported }.
// rev 4: `supported` false (older backend) hides the whole 通知类型 group.
export function renderLiveEventsInto(doc, els, { status, settings, statusAt, now, error = '', supported = true }) {
  setHidden(els.group, !supported);
  if (!supported || !els.line) return null;
  const available = Boolean(status) && now - statusAt < STATUS_STALE_MS;
  const view = wakeupBudgetView(pickBudget(status, settings), { available });
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
  if (els.input && !gate.enabled) els.input.disabled = true;
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
