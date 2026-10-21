// tooltip.js — one shared tooltip for every `[data-tooltip]` anchor.
//
// Native `title` tooltips are slow, unstyled, and unreadable on touch, so the
// dense surfaces (cluster tiles, status chips) use this instead. A single node
// is created on first use and repositioned per anchor: no wrapper element, no
// per-anchor listeners, no layout thrash while hovering.

const SHOW_DELAY = 180;
const GAP = 6;
const EDGE = 8;

let tip = null;
let anchor = null;
let timer = 0;

function ensureTip() {
  if (tip) return tip;
  tip = document.createElement('div');
  tip.className = 'ui-tooltip';
  tip.id = 'ui-tooltip';
  tip.setAttribute('role', 'tooltip');
  tip.setAttribute('aria-hidden', 'true');
  document.body.appendChild(tip);
  return tip;
}

function hide() {
  clearTimeout(timer);
  timer = 0;
  anchor = null;
  if (tip) {
    tip.removeAttribute('data-visible');
    tip.setAttribute('aria-hidden', 'true');
  }
}

function place(target) {
  const el = ensureTip();
  const text = target.dataset.tooltip;
  if (!text) return;
  el.textContent = text;
  el.dataset.visible = '';
  el.removeAttribute('aria-hidden');

  // Measure after the text lands, then place above the anchor unless that
  // would leave the viewport — flipping below is cheaper than clamping.
  const a = target.getBoundingClientRect();
  const box = el.getBoundingClientRect();
  const above = a.top - box.height - GAP;
  const side = above >= EDGE ? 'top' : 'bottom';
  const top = side === 'top' ? above : a.bottom + GAP;
  const centred = a.left + a.width / 2 - box.width / 2;
  const left = Math.min(Math.max(EDGE, centred), window.innerWidth - box.width - EDGE);

  el.dataset.side = side;
  el.style.top = `${Math.round(top)}px`;
  el.style.left = `${Math.round(left)}px`;
  // Keep the arrow pointing at the anchor even when the bubble was clamped.
  const arrow = Math.min(Math.max(12, a.left + a.width / 2 - left), box.width - 12);
  el.style.setProperty('--arrow-x', `${Math.round(arrow)}px`);
}

function anchorFor(event) {
  const node = event.target instanceof Element ? event.target.closest('[data-tooltip]') : null;
  return node && node.dataset.tooltip ? node : null;
}

function show(target) {
  anchor = target;
  clearTimeout(timer);
  timer = setTimeout(() => {
    if (anchor === target && target.isConnected) place(target);
  }, SHOW_DELAY);
}

let started = false;

export function initTooltips() {
  if (started) return;
  started = true;

  document.addEventListener('pointerover', event => {
    if (event.pointerType === 'touch') return;
    const target = anchorFor(event);
    if (!target || target === anchor) return;
    show(target);
  });

  document.addEventListener('pointerout', event => {
    const target = anchorFor(event);
    if (target && target.contains(event.relatedTarget)) return;
    if (target) hide();
  });

  document.addEventListener('focusin', event => {
    const target = anchorFor(event);
    if (target) show(target);
  });

  document.addEventListener('focusout', () => hide());

  // The bubble is fixed-position, so any scroll or viewport change invalidates
  // it; hiding is cheaper and steadier than tracking the anchor.
  document.addEventListener('scroll', hide, true);
  window.addEventListener('resize', hide);
  document.addEventListener('keydown', event => {
    if (event.key === 'Escape') hide();
  });
  document.addEventListener('pointerdown', hide, true);
}
