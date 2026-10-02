// DOM for the public status accordion. The chevron lives in the 直播状态 app
// bar. Fold decisions live in status-fold.js so a poll can be tested without
// a document.

let foldTicket = 0;

function foldElements() {
  return {
    fold: document.getElementById('public-fold'),
    trigger: document.getElementById('public-fold-trigger'),
    panel: document.getElementById('public-fold-panel'),
  };
}

/// @param {{ restreaming: boolean | null, open: boolean }} state
export function applyPublicFold(state) {
  const { fold, trigger, panel } = foldElements();
  if (!fold || !trigger || !panel) return;

  const open = !!state.open;
  const ticket = ++foldTicket;
  const label = open ? '折叠状态' : '展开状态';
  trigger.setAttribute('aria-expanded', open ? 'true' : 'false');
  trigger.setAttribute('aria-label', label);
  trigger.title = label;
  trigger.classList.toggle('is-open', open);
  // inert only removes the panel from tab order. It does not cancel the
  // grid-row animation, so the fold can close without leaving links focusable.
  panel.inert = !open;

  if (open) {
    fold.hidden = false;
    // Let the collapsed row paint before growing it, or the open skips the animation.
    requestAnimationFrame(() => {
      if (ticket === foldTicket) fold.classList.add('is-open');
    });
    return;
  }

  fold.classList.remove('is-open');
  if (fold.hidden) return;
  const hide = (event) => {
    if (event && (event.target !== panel || event.propertyName !== 'grid-template-rows')) return;
    if (ticket === foldTicket && !fold.classList.contains('is-open')) fold.hidden = true;
  };
  panel.addEventListener('transitionend', hide, { once: true });
  setTimeout(hide, 500);
}

function spawnRipple(trigger, event) {
  const host = trigger.querySelector('.public-fold__ripples');
  if (!host) return;
  const rect = trigger.getBoundingClientRect();
  const fromPointer = event.detail > 0 || event.clientX !== 0 || event.clientY !== 0;
  const x = fromPointer ? event.clientX - rect.left : rect.width / 2;
  const y = fromPointer ? event.clientY - rect.top : rect.height / 2;
  const size = Math.max(rect.width, rect.height);
  const ripple = document.createElement('span');
  ripple.className = 'public-fold__ripple';
  ripple.style.width = `${size}px`;
  ripple.style.height = `${size}px`;
  ripple.style.left = `${x - size / 2}px`;
  ripple.style.top = `${y - size / 2}px`;
  host.appendChild(ripple);
  ripple.addEventListener('animationend', () => ripple.remove(), { once: true });
  setTimeout(() => ripple.remove(), 700);
}

/// @param {() => void} onToggle
export function bindPublicFold(onToggle) {
  const trigger = document.getElementById('public-fold-trigger');
  if (!trigger || trigger.dataset.bound === '1') return;
  trigger.dataset.bound = '1';
  trigger.addEventListener('click', (event) => {
    spawnRipple(trigger, event);
    onToggle();
  });
  const finePointer = window.matchMedia('(hover: hover) and (pointer: fine)');
  const track = (event) => {
    const rect = trigger.getBoundingClientRect();
    trigger.style.setProperty('--mouse-x', `${event.clientX - rect.left}px`);
    trigger.style.setProperty('--mouse-y', `${event.clientY - rect.top}px`);
    trigger.classList.add('is-pointer');
  };
  const clearPointer = () => {
    trigger.classList.remove('is-pointer');
    trigger.style.removeProperty('--mouse-x');
    trigger.style.removeProperty('--mouse-y');
  };
  const enableTracking = () => {
    trigger.addEventListener('mousemove', track);
    trigger.addEventListener('mouseleave', clearPointer);
  };
  if (finePointer.matches) enableTracking();
  else finePointer.addEventListener('change', () => {
    if (finePointer.matches) enableTracking();
  }, { once: true });
}
