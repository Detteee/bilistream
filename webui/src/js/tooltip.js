// tooltip.js — one shared tooltip for every `[data-tooltip]` anchor.
//
// Native `title` tooltips are slow, unstyled, and unreadable on touch, so the
// dense surfaces (cluster tiles, status chips) use this instead. A single node
// is created on first use and repositioned per anchor: no wrapper element, no
// per-anchor listeners, no layout thrash while hovering.
//
// Anchors that still carry a native `title` join the same bubble. The text is
// moved to `data-tooltip` on first hover and put back on leave, so the native
// bubble never appears next to ours and the accessible name is never lost.

const SHOW_DELAY = 180;
const GAP = 6;
const EDGE = 8;
// Tried in order, first side with room wins: a chip near the bottom edge flips
// up, and an anchor in a narrow column flips sideways instead of being clamped
// over itself.
const SIDES = ['top', 'bottom', 'right', 'left'];

let tip = null;
let anchor = null;
let timer = 0;
const adopted = new Map();

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

function restoreTitles() {
  if (!adopted.size) return;
  for (const [node, text] of adopted) {
    delete node.dataset.tooltip;
    node.setAttribute('title', text);
  }
  adopted.clear();
}

function hide() {
  clearTimeout(timer);
  timer = 0;
  anchor = null;
  restoreTitles();
  if (tip) {
    tip.removeAttribute('data-visible');
    tip.setAttribute('aria-hidden', 'true');
  }
}

function adoptTitle(node) {
  if (!node || node === document.body || node === document.documentElement) return null;
  if (node.dataset.tooltip) return node;
  const text = node.getAttribute('title');
  if (!text) return null;
  adopted.set(node, text);
  node.dataset.tooltip = text;
  node.removeAttribute('title');
  return node;
}

function pickSide(target, box) {
  const a = target.getBoundingClientRect();
  const room = {
    top: a.top - EDGE,
    bottom: window.innerHeight - a.bottom - EDGE,
    left: a.left - EDGE,
    right: window.innerWidth - a.right - EDGE,
  };
  const need = {
    top: box.height + GAP,
    bottom: box.height + GAP,
    left: box.width + GAP,
    right: box.width + GAP,
  };
  const preferred = target.dataset.tooltipSide;
  const order =
    preferred && SIDES.includes(preferred)
      ? [preferred, ...SIDES.filter(side => side !== preferred)]
      : SIDES;
  const fits = order.find(side => room[side] >= need[side]);
  if (fits) return fits;
  // Nothing fits: take the roomiest side and let the clamp keep us on screen.
  return order.reduce(
    (best, side) => (room[side] - need[side] > room[best] - need[best] ? side : best),
    order[0]
  );
}

function place(target) {
  const el = ensureTip();
  const text = target.dataset.tooltip;
  if (!text) return;
  el.textContent = text;
  el.dataset.visible = '';
  el.removeAttribute('aria-hidden');

  // Measure after the text lands, then pick a side and clamp to the viewport.
  const a = target.getBoundingClientRect();
  const box = el.getBoundingClientRect();
  const side = pickSide(target, box);
  // A bubble wider or taller than the viewport (long help text on a phone)
  // cannot be clamped on both sides, so pin it to the leading edge instead of
  // letting the second clamp push it off screen.
  const clamp = (v, limit) => Math.min(Math.max(EDGE, v), Math.max(EDGE, limit));
  const clampX = v => clamp(v, window.innerWidth - box.width - EDGE);
  const clampY = v => clamp(v, window.innerHeight - box.height - EDGE);

  let top;
  let left;
  if (side === 'top' || side === 'bottom') {
    top = side === 'top' ? a.top - box.height - GAP : a.bottom + GAP;
    left = clampX(a.left + a.width / 2 - box.width / 2);
    // Keep the arrow on the anchor even when the bubble was clamped.
    const arrow = Math.min(Math.max(12, a.left + a.width / 2 - left), box.width - 12);
    el.style.setProperty('--arrow-x', `${Math.round(arrow)}px`);
  } else {
    left = side === 'right' ? a.right + GAP : a.left - box.width - GAP;
    top = clampY(a.top + a.height / 2 - box.height / 2);
    const arrow = Math.min(Math.max(12, a.top + a.height / 2 - top), box.height - 12);
    el.style.setProperty('--arrow-y', `${Math.round(arrow)}px`);
  }

  el.dataset.side = side;
  el.style.top = `${Math.round(clampY(top))}px`;
  el.style.left = `${Math.round(clampX(left))}px`;
}

function anchorFor(event) {
  const node = event.target instanceof Element ? event.target : null;
  if (!node) return null;
  const explicit = node.closest('[data-tooltip]');
  if (explicit) return explicit.dataset.tooltip ? explicit : null;
  return adoptTitle(node.closest('[title]'));
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
