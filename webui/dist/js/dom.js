// dom.js — extracted from app.js

function isDashboardVisible() {
  const mainPage = document.getElementById('main-page');
  return document.visibilityState === 'visible' && mainPage && !mainPage.classList.contains('hidden');
}
function isElementHidden(element) {
  return !element || getComputedStyle(element).display === 'none';
}
function setElementDisplay(element, visible, display = 'block') {
  if (element) {
    element.style.display = visible ? display : 'none';
  }
}
// Keep unchanged cards attached, including their image state and keyboard focus.
function reconcileChildren(container, elements) {
  const focused = container.contains(document.activeElement) ? document.activeElement : null;
  const keep = new Set(elements);
  for (const child of [...container.children]) {
    if (!keep.has(child)) child.remove();
  }
  elements.forEach((element, index) => {
    if (container.children[index] !== element) {
      container.insertBefore(element, container.children[index] || null);
    }
  });
  if (focused?.isConnected && document.activeElement === document.body) {
    focused.focus({ preventScroll: true });
  }
}

function createStreamThumbnail(src) {
  const image = document.createElement('img');
  image.alt = '';
  image.width = 640;
  image.height = 360;
  image.loading = 'lazy';
  image.decoding = 'async';
  image.addEventListener('error', () => {
    const placeholder = document.createElement('div');
    placeholder.className = 'holodex-stream-thumb-placeholder';
    placeholder.textContent = '封面暂时不可用';
    image.replaceWith(placeholder);
  }, { once: true });
  image.src = src;
  return image;
}
const SVG_NS = 'http://www.w3.org/2000/svg';
function createSvgIcon(viewBox, pathData, className = '') {
  const svg = document.createElementNS(SVG_NS, 'svg');
  svg.setAttribute('viewBox', viewBox);
  svg.setAttribute('aria-hidden', 'true');
  if (className) {
    svg.classList.add(className);
  }

  const paths = Array.isArray(pathData) ? pathData : [pathData];
  paths.forEach(pathDefinition => {
    const path = document.createElementNS(SVG_NS, 'path');
    if (typeof pathDefinition === 'string') {
      path.setAttribute('d', pathDefinition);
    } else {
      Object.entries(pathDefinition).forEach(([name, value]) => {
        path.setAttribute(name, value);
      });
    }
    svg.appendChild(path);
  });
  return svg;
}
function appendAntiCollisionRemoveIcon(button) {
  const svgNamespace = 'http://www.w3.org/2000/svg';
  const svg = document.createElementNS(svgNamespace, 'svg');
  svg.classList.add('cluster-btn-icon');
  svg.setAttribute('viewBox', '0 0 24 24');
  svg.setAttribute('fill', 'none');
  svg.setAttribute('stroke', 'currentColor');
  svg.setAttribute('stroke-width', '2');
  svg.setAttribute('stroke-linecap', 'round');
  svg.setAttribute('stroke-linejoin', 'round');
  svg.setAttribute('aria-hidden', 'true');

  const polyline = document.createElementNS(svgNamespace, 'polyline');
  polyline.setAttribute('points', '3,6 5,6 21,6');

  const path = document.createElementNS(svgNamespace, 'path');
  path.setAttribute('d', 'm19,6v14a2,2 0 0,1 -2,2H7a2,2 0 0,1 -2,-2V6m3,0V4a2,2 0 0,1 2,-2h4a2,2 0 0,1 2,2v2');

  const leftLine = document.createElementNS(svgNamespace, 'line');
  leftLine.setAttribute('x1', '10');
  leftLine.setAttribute('y1', '11');
  leftLine.setAttribute('x2', '10');
  leftLine.setAttribute('y2', '17');

  const rightLine = document.createElementNS(svgNamespace, 'line');
  rightLine.setAttribute('x1', '14');
  rightLine.setAttribute('y1', '11');
  rightLine.setAttribute('x2', '14');
  rightLine.setAttribute('y2', '17');

  svg.append(polyline, path, leftLine, rightLine);
  button.appendChild(svg);
}
function appendEditIcon(button) {
  const svgNamespace = 'http://www.w3.org/2000/svg';
  const svg = document.createElementNS(svgNamespace, 'svg');
  svg.classList.add('cluster-btn-icon');
  svg.setAttribute('viewBox', '0 0 24 24');
  svg.setAttribute('fill', 'none');
  svg.setAttribute('stroke', 'currentColor');
  svg.setAttribute('stroke-width', '2');
  svg.setAttribute('stroke-linecap', 'round');
  svg.setAttribute('stroke-linejoin', 'round');
  svg.setAttribute('aria-hidden', 'true');

  const bottomLine = document.createElementNS(svgNamespace, 'path');
  bottomLine.setAttribute('d', 'M12 20h9');

  const pencil = document.createElementNS(svgNamespace, 'path');
  pencil.setAttribute('d', 'M16.5 3.5a2.121 2.121 0 0 1 3 3L7 19l-4 1 1-4L16.5 3.5z');

  svg.append(bottomLine, pencil);
  button.appendChild(svg);
}
function readInputValue(id) {
  return document.getElementById(id)?.value?.trim() || '';
}
function parseCommaSeparatedInput(id) {
  return readInputValue(id)
    .split(',')
    .map(item => item.trim())
    .filter(Boolean);
}
function parseInteger(value, fallback = 0) {
  const normalized = String(value ?? '').trim();
  if (!normalized) {
    return fallback;
  }
  if (!/^[+-]?\d+$/.test(normalized)) {
    return fallback;
  }

  const parsed = Number(normalized);
  return Number.isSafeInteger(parsed) ? parsed : fallback;
}
function readIntegerInput(id, fallback = 0) {
  return parseInteger(document.getElementById(id)?.value, fallback);
}
function setInputValue(id, value) {
  const input = document.getElementById(id);
  if (input) {
    input.value = value;
  }
}
function setElementText(id, value) {
  const element = document.getElementById(id);
  if (element) {
    element.textContent = value;
  }
}
function setCheckboxChecked(id, checked) {
  const checkbox = document.getElementById(id);
  if (checkbox) {
    checkbox.checked = !!checked;
  }
}
function showNotification(message, type = 'success') {
  const notification = document.createElement('div');
  notification.className = `notification ${type}`;
  notification.textContent = message;
  // Toasts go into a dedicated stack so several at once queue up
  // instead of drawing on top of each other.
  (document.getElementById('toast-region') || document.body).appendChild(notification);

  setTimeout(() => {
    notification.remove();
  }, 3000);
}
function setButtonLoading(button, icon, loading) {
  if (button) {
    button.disabled = loading;
    button.classList.toggle('is-loading', loading);
  }
  if (icon) {
    icon.classList.toggle('is-spinning', loading);
  }
}

export {
  isDashboardVisible,
  isElementHidden,
  setElementDisplay,
  reconcileChildren,
  createStreamThumbnail,
  createSvgIcon,
  appendAntiCollisionRemoveIcon,
  appendEditIcon,
  readInputValue,
  parseCommaSeparatedInput,
  parseInteger,
  readIntegerInput,
  setInputValue,
  setElementText,
  setCheckboxChecked,
  showNotification,
  setButtonLoading,
  SVG_NS,
};
