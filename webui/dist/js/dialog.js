// Shared keyboard and focus behavior for the existing custom dialogs.
const stack = [];
const inertBefore = new Map();
const registered = new WeakSet();
let lastOutsideFocus = null;

function focusable(dialog) {
  return [...dialog.querySelectorAll('button, a[href], input, select, textarea, [tabindex]')]
    .filter(element => !element.disabled && element.tabIndex >= 0 && element.getClientRects().length && !element.closest('[inert]'));
}

function isolateTopDialog() {
  for (const [element, inert] of inertBefore) element.inert = inert;
  inertBefore.clear();
  const top = stack.at(-1)?.dialog;
  if (!top) return;
  for (let child = top; child && child !== document.body; child = child.parentElement) {
    for (const sibling of child.parentElement?.children || []) {
      if (sibling === child) continue;
      inertBefore.set(sibling, sibling.inert);
      sibling.inert = true;
    }
  }
}

function focusDialog(dialog) {
  (focusable(dialog)[0] || dialog).focus();
}

document.addEventListener('focusin', event => {
  const top = stack.at(-1)?.dialog;
  if (top && !top.contains(event.target)) focusDialog(top);
  else if (!event.target.closest('[role="dialog"]')) lastOutsideFocus = event.target;
});

document.addEventListener('keydown', event => {
  const top = stack.at(-1);
  if (!top) return;
  if (event.key === 'Escape') {
    event.preventDefault();
    event.stopImmediatePropagation();
    top.onClose?.();
  } else if (event.key === 'Tab') {
    const items = focusable(top.dialog);
    const index = items.indexOf(document.activeElement);
    if (!items.length || index < 0 || (!event.shiftKey && index === items.length - 1) || (event.shiftKey && index === 0)) {
      event.preventDefault();
      (event.shiftKey ? items.at(-1) : items[0])?.focus();
    }
  }
}, true);

export function bindDialog(target, onClose) {
  const dialog = typeof target === 'string' ? document.getElementById(target) : target;
  if (!dialog || registered.has(dialog)) return;
  registered.add(dialog);
  dialog.tabIndex = -1;
  const entry = { dialog, onClose, returnFocus: null };
  const sync = () => {
    const open = dialog.getClientRects().length > 0 && !dialog.hidden;
    const index = stack.indexOf(entry);
    dialog.setAttribute('aria-hidden', String(!open));
    if (open && index < 0) {
      entry.returnFocus = dialog.contains(document.activeElement) ? lastOutsideFocus : document.activeElement;
      stack.push(entry);
      isolateTopDialog();
      if (!dialog.contains(document.activeElement)) focusDialog(dialog);
    } else if (!open && index >= 0) {
      const wasTop = index === stack.length - 1;
      stack.splice(index, 1);
      isolateTopDialog();
      if (wasTop) {
        if (entry.returnFocus?.isConnected && !entry.returnFocus.closest('[inert]')) entry.returnFocus.focus();
        else if (stack.length) focusDialog(stack.at(-1).dialog);
      }
    }
  };
  new MutationObserver(sync).observe(dialog, { attributes: true, attributeFilter: ['class', 'style', 'hidden'] });
  sync();
}

export function bindListboxKeyboard(list) {
  list?.addEventListener('keydown', event => {
    const items = [...list.querySelectorAll('[role="option"]')];
    const index = items.indexOf(document.activeElement);
    let next;
    if (event.key === 'ArrowDown' || event.key === 'ArrowRight') next = (index + 1) % items.length;
    else if (event.key === 'ArrowUp' || event.key === 'ArrowLeft') next = (index - 1 + items.length) % items.length;
    else if (event.key === 'Home') next = 0;
    else if (event.key === 'End') next = items.length - 1;
    else return;
    event.preventDefault();
    items[next]?.focus();
  });
}
