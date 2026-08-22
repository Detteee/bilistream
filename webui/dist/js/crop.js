// crop.js — extracted from app.js

import { setElementDisplay, readIntegerInput, setInputValue, setElementText, showNotification } from './dom.js';
import { state } from './state.js';
import { getJson, postJsonApi } from './api.js';

function initCropModalControls() {
  document
    .getElementById('cropPlatform')
    ?.addEventListener('change', loadCurrentCropSettings);
  document
    .getElementById('cropImageUpload')
    ?.addEventListener('change', loadCropImage);
  document
    .querySelectorAll('[data-crop-ratio]')
    .forEach(button => {
      button.addEventListener('click', () => selectAspectRatio(button.dataset.cropRatio));
    });
  document
    .getElementById('crop-disable-btn')
    ?.addEventListener('click', disableCrop);
  document
    .getElementById('crop-apply-restart-btn')
    ?.addEventListener('click', applyCropAndRestart);
  document
    .getElementById('crop-apply-btn')
    ?.addEventListener('click', applyCrop);
  document
    .getElementById('crop-close-btn')
    ?.addEventListener('click', closeCropModal);
}
// Crop Configuration Functions
let cropImage = null;
let cropStartX = 0;
let cropStartY = 0;
let isDrawing = false;
let cropCanvas = null;
let cropCtx = null;
function readCropRect() {
  return {
    x: readIntegerInput('cropX', 0) || 0,
    y: readIntegerInput('cropY', 0) || 0,
    width: readIntegerInput('cropWidth', 0) || 0,
    height: readIntegerInput('cropHeight', 0) || 0
  };
}
function validateCropRect(rect) {
  if (rect.x < 0 || rect.y < 0) {
    return '裁剪区域坐标不能为负数';
  }
  if (rect.width <= 0 || rect.height <= 0) {
    return '请先输入有效的裁剪区域尺寸';
  }
  return '';
}
function setCropRectInputs(rect) {
  setInputValue('cropX', Math.round(rect.x));
  setInputValue('cropY', Math.round(rect.y));
  setInputValue('cropWidth', Math.round(rect.width));
  setInputValue('cropHeight', Math.round(rect.height));
}
function positionCropBox(cropBox, rect, scaleX, scaleY) {
  if (!cropBox) return;

  setElementDisplay(cropBox, true);
  cropBox.style.left = (rect.x / scaleX) + 'px';
  cropBox.style.top = (rect.y / scaleY) + 'px';
  cropBox.style.width = (rect.width / scaleX) + 'px';
  cropBox.style.height = (rect.height / scaleY) + 'px';
}
function bindCropRectInputListeners() {
  ['cropX', 'cropY', 'cropWidth', 'cropHeight'].forEach(id => {
    const input = document.getElementById(id);
    if (!input) {
      return;
    }
    input.removeEventListener('input', updateCropBox);
    input.addEventListener('input', updateCropBox);
  });
}
function showCropCanvasContainer() {
  document.getElementById('cropCanvasContainer')?.classList.remove('hidden');
}
function hideCropCanvasContainer() {
  document.getElementById('cropCanvasContainer')?.classList.add('hidden');
}
function setCropStatusLabel(platform, label) {
  const statusId = platform === 'youtube' ? 'yt-crop-status' : 'tw-crop-status';
  setElementText(statusId, label);
}
function createCropUpdatePayload(platform, enabled, rect = {}) {
  return {
    platform,
    enabled,
    ...rect
  };
}
function postCropUpdate(payload) {
  return postJsonApi('/api/crop/update', payload);
}
function openCropConfig(platform) {
  document.getElementById('cropModal').classList.add('active');
  if (platform) {
    document.getElementById('cropPlatform').value = platform;
  }
  loadCurrentCropSettings();
  // Auto-capture frame when opening from platform cards
  if (platform) {
    setTimeout(() => autoCaptureFrame(), 300);
  }
}
function closeCropModal() {
  document.getElementById('cropModal').classList.remove('active');
  // Reset canvas
  hideCropCanvasContainer();
  cropImage = null;
}
async function clearCropConfig(platform) {
  if (!confirm(`确定要清除${platform === 'youtube' ? 'YouTube' : 'Twitch'}的裁剪设置吗？`)) {
    return;
  }

  try {
    const result = await postCropUpdate(createCropUpdatePayload(platform, false));

    if (result.success) {
      showNotification('裁剪设置已清除', 'success');
      setCropStatusLabel(platform, '关闭');
    } else {
      showNotification(result.message || '清除失败', 'error');
    }
  } catch (error) {
    showNotification('清除失败: ' + error.message, 'error');
  }
}
async function autoCaptureFrame() {
  const platform = document.getElementById('cropPlatform').value;

  showNotification('正在捕获直播帧...', 'info');

  try {
    const result = await postJsonApi(`/api/crop/capture/${platform}`);

    if (result.success && result.message) {
      // Load the captured image (base64 is in message field)
      cropImage = new Image();
      cropImage.onload = function () {
        cropCanvas = document.getElementById('cropCanvas');
        cropCtx = cropCanvas.getContext('2d');

        // Show container FIRST so canvas has dimensions
        showCropCanvasContainer();

        // Set canvas size to image size
        cropCanvas.width = cropImage.width;
        cropCanvas.height = cropImage.height;

        // Fill with white background before drawing transparent sources.
        cropCtx.fillStyle = 'white';
        cropCtx.fillRect(0, 0, cropCanvas.width, cropCanvas.height);

        // Draw image
        cropCtx.drawImage(cropImage, 0, 0);

        // Setup canvas interaction
        setupCanvasInteraction();

        showNotification('直播帧已捕获，请在图片上选择裁剪区域', 'success');
      };
      cropImage.onerror = function (e) {
        console.error('Image load error:', e);
        showNotification('图片加载失败', 'error');
      };
      cropImage.src = result.message;
    } else {
      console.error('Capture failed:', result);
      showNotification(result.message || '捕获失败，请确保直播正在进行', 'error');
    }
  } catch (error) {
    console.error('Capture error:', error);
    showNotification('捕获失败: ' + error.message, 'error');
  }
}
async function loadCurrentCropSettings() {
  const platform = document.getElementById('cropPlatform')?.value;
  if (!platform) return;

  try {
    const result = await getJson(`/api/crop/${platform}`);
    if (result.success && result.data && result.data.enabled) {
      setCropRectInputs(result.data);
    }
  } catch (error) {
    console.error('Failed to load crop settings:', error);
  }
}
function loadCropImage(event) {
  const file = event.target.files?.[0];
  if (!file) return;

  const reader = new FileReader();
  reader.onload = function (e) {
    cropImage = new Image();
    cropImage.onload = function () {
      cropCanvas = document.getElementById('cropCanvas');
      cropCtx = cropCanvas.getContext('2d');

      // Show container FIRST so canvas has dimensions
      showCropCanvasContainer();

      // Set canvas size to image size
      cropCanvas.width = cropImage.width;
      cropCanvas.height = cropImage.height;

      // Fill with white background before drawing transparent sources.
      cropCtx.fillStyle = 'white';
      cropCtx.fillRect(0, 0, cropCanvas.width, cropCanvas.height);

      // Draw image
      cropCtx.drawImage(cropImage, 0, 0);

      // Setup canvas interaction
      setupCanvasInteraction();

      showNotification('图片已加载，请在图片上拖动鼠标选择裁剪区域', 'success');
    };
    cropImage.onerror = function (err) {
      console.error('Image load error:', err);
      showNotification('图片加载失败', 'error');
    };
    cropImage.src = e.target.result;
  };
  reader.onerror = function (err) {
    console.error('FileReader error:', err);
    showNotification('文件读取失败', 'error');
  };
  reader.readAsDataURL(file);
}
let currentAspectRatio = 'free';
let isDragging = false;
let dragStartX = 0;
let dragStartY = 0;
let boxStartX = 0;
let boxStartY = 0;
let boxStartWidth = 0;
let boxStartHeight = 0;
const CROP_ASPECT_RATIOS = {
  '1:1': 1,
  '16:9': 16 / 9,
  '16:10': 16 / 10,
  '9:16': 9 / 16,
  '10:16': 10 / 16
}
function getCropAspectRatioValue(ratio) {
  return CROP_ASPECT_RATIOS[ratio] || 1;
}
function createCenteredCropRect(canvas, ratio) {
  const canvasWidth = canvas.width;
  const canvasHeight = canvas.height;
  const ratioValue = getCropAspectRatioValue(ratio);
  const maxWidth = canvasWidth * 0.6;
  const maxHeight = canvasHeight * 0.6;

  let width;
  let height;
  if (ratioValue >= 1) {
    width = maxWidth;
    height = width / ratioValue;
    if (height > maxHeight) {
      height = maxHeight;
      width = height * ratioValue;
    }
  } else {
    height = maxHeight;
    width = height * ratioValue;
    if (width > maxWidth) {
      width = maxWidth;
      height = width / ratioValue;
    }
  }

  return {
    x: (canvasWidth - width) / 2,
    y: (canvasHeight - height) / 2,
    width,
    height
  };
}
function applyAspectRatio() {
  currentAspectRatio = document.getElementById('cropAspectRatio').value;

  if (!cropCanvas || currentAspectRatio === 'free') return;

  setCropRectInputs(createCenteredCropRect(cropCanvas, currentAspectRatio));

  updateCropBox();
  showNotification(`已应用 ${currentAspectRatio} 比例`, 'success');
}
function setupCanvasInteraction() {
  const canvas = document.getElementById('cropCanvas');
  const cropBox = document.getElementById('cropBox');

  // Remove old listeners by cloning
  const newCanvas = canvas.cloneNode(true);
  canvas.parentNode.replaceChild(newCanvas, canvas);
  cropCanvas = newCanvas;

  // Redraw the image on the new canvas
  if (cropImage) {
    cropCtx = cropCanvas.getContext('2d');
    cropCtx.fillStyle = 'white';
    cropCtx.fillRect(0, 0, cropCanvas.width, cropCanvas.height);
    cropCtx.drawImage(cropImage, 0, 0);
  }

  let resizeEdge = null; // 'n', 's', 'e', 'w', 'ne', 'nw', 'se', 'sw'
  const edgeThreshold = 10; // pixels from edge to trigger resize

  function getResizeEdge(mouseX, mouseY, x, y, width, height) {
    const threshold = edgeThreshold;
    let edge = '';

    // Check vertical edges
    if (Math.abs(mouseY - y) < threshold) edge += 'n';
    else if (Math.abs(mouseY - (y + height)) < threshold) edge += 's';

    // Check horizontal edges
    if (Math.abs(mouseX - x) < threshold) edge += 'w';
    else if (Math.abs(mouseX - (x + width)) < threshold) edge += 'e';

    return edge || null;
  }

  function getCursorForEdge(edge) {
    if (!edge) return 'crosshair';
    const cursors = {
      'n': 'ns-resize',
      's': 'ns-resize',
      'e': 'ew-resize',
      'w': 'ew-resize',
      'ne': 'nesw-resize',
      'nw': 'nwsw-resize',
      'se': 'nwsw-resize',
      'sw': 'nesw-resize'
    };
    return cursors[edge] || 'crosshair';
  }

  newCanvas.addEventListener('mousedown', (e) => {
    const rect = newCanvas.getBoundingClientRect();
    const scaleX = newCanvas.width / rect.width;
    const scaleY = newCanvas.height / rect.height;

    const mouseX = (e.clientX - rect.left) * scaleX;
    const mouseY = (e.clientY - rect.top) * scaleY;

    const { x, y, width, height } = readCropRect();

    // Check if clicking on edge for resizing
    if (mouseX >= x - edgeThreshold && mouseX <= x + width + edgeThreshold &&
      mouseY >= y - edgeThreshold && mouseY <= y + height + edgeThreshold) {
      resizeEdge = getResizeEdge(mouseX, mouseY, x, y, width, height);
      if (resizeEdge) {
        isDrawing = true;
        cropStartX = mouseX;
        cropStartY = mouseY;
        boxStartX = x;
        boxStartY = y;
        boxStartWidth = width;
        boxStartHeight = height;
        return;
      }
    }

    // Check if clicking inside existing box for dragging
    if (mouseX >= x && mouseX <= x + width && mouseY >= y && mouseY <= y + height) {
      isDragging = true;
      dragStartX = mouseX;
      dragStartY = mouseY;
      boxStartX = x;
      boxStartY = y;
      newCanvas.style.cursor = 'move';
    } else {
      // Start new selection
      cropStartX = mouseX;
      cropStartY = mouseY;
      isDrawing = true;
      resizeEdge = null;

      positionCropBox(cropBox, { x: mouseX, y: mouseY, width: 0, height: 0 }, scaleX, scaleY);
    }
  });

  newCanvas.addEventListener('mousemove', (e) => {
    const rect = newCanvas.getBoundingClientRect();
    const scaleX = newCanvas.width / rect.width;
    const scaleY = newCanvas.height / rect.height;

    const currentX = (e.clientX - rect.left) * scaleX;
    const currentY = (e.clientY - rect.top) * scaleY;

    const { x, y, width, height } = readCropRect();

    // Update cursor based on position
    if (!isDrawing && !isDragging) {
      if (currentX >= x - edgeThreshold && currentX <= x + width + edgeThreshold &&
        currentY >= y - edgeThreshold && currentY <= y + height + edgeThreshold) {
        const edge = getResizeEdge(currentX, currentY, x, y, width, height);
        if (edge) {
          newCanvas.style.cursor = getCursorForEdge(edge);
          return;
        } else if (currentX >= x && currentX <= x + width && currentY >= y && currentY <= y + height) {
          newCanvas.style.cursor = 'move';
          return;
        }
      }
      newCanvas.style.cursor = 'crosshair';
    }

    if (isDragging) {
      // Move the box
      const deltaX = currentX - dragStartX;
      const deltaY = currentY - dragStartY;

      let newX = boxStartX + deltaX;
      let newY = boxStartY + deltaY;

      // Constrain to canvas
      newX = Math.max(0, Math.min(newX, newCanvas.width - width));
      newY = Math.max(0, Math.min(newY, newCanvas.height - height));

      setCropRectInputs({ x: newX, y: newY, width, height });
      updateCropBox();
    } else if (isDrawing && resizeEdge) {
      // Resize the box by edge
      let newX = boxStartX;
      let newY = boxStartY;
      let newWidth = boxStartWidth;
      let newHeight = boxStartHeight;

      const deltaX = currentX - cropStartX;
      const deltaY = currentY - cropStartY;

      if (currentAspectRatio === 'free') {
        // Free resize - no aspect ratio constraint
        if (resizeEdge.includes('n')) {
          newY = boxStartY + deltaY;
          newHeight = boxStartHeight - deltaY;
        }
        if (resizeEdge.includes('s')) {
          newHeight = boxStartHeight + deltaY;
        }
        if (resizeEdge.includes('w')) {
          newX = boxStartX + deltaX;
          newWidth = boxStartWidth - deltaX;
        }
        if (resizeEdge.includes('e')) {
          newWidth = boxStartWidth + deltaX;
        }

        // Constrain to canvas and minimum size
        if (newWidth < 10) newWidth = 10;
        if (newHeight < 10) newHeight = 10;
        if (newX < 0) { newWidth += newX; newX = 0; }
        if (newY < 0) { newHeight += newY; newY = 0; }
        if (newX + newWidth > newCanvas.width) newWidth = newCanvas.width - newX;
        if (newY + newHeight > newCanvas.height) newHeight = newCanvas.height - newY;
      } else {
        // Locked aspect ratio - resize proportionally
        const ratio = getCropAspectRatioValue(currentAspectRatio);

        if (resizeEdge.includes('e') || resizeEdge.includes('w')) {
          // Horizontal resize
          newWidth = boxStartWidth + deltaX;
          newHeight = newWidth / ratio;
          if (resizeEdge.includes('w')) {
            newX = boxStartX + boxStartWidth - newWidth;
          }
          if (resizeEdge.includes('n')) {
            newY = boxStartY + boxStartHeight - newHeight;
          }
        } else {
          // Vertical resize
          newHeight = boxStartHeight + deltaY;
          newWidth = newHeight * ratio;
          if (resizeEdge.includes('n')) {
            newY = boxStartY + boxStartHeight - newHeight;
          }
        }

        // Constrain to minimum size first
        if (newWidth < 10) {
          newWidth = 10;
          newHeight = newWidth / ratio;
        }
        if (newHeight < 10) {
          newHeight = 10;
          newWidth = newHeight * ratio;
        }

        // Constrain to canvas bounds while maintaining aspect ratio
        // Check if box exceeds canvas bounds
        if (newX < 0) {
          newX = 0;
          newWidth = boxStartX + boxStartWidth;
          newHeight = newWidth / ratio;
          // Adjust Y position if resizing from top
          if (resizeEdge.includes('n')) {
            newY = boxStartY + boxStartHeight - newHeight;
          }
        }
        if (newY < 0) {
          newY = 0;
          newHeight = boxStartY + boxStartHeight;
          newWidth = newHeight * ratio;
          // Adjust X position if resizing from left
          if (resizeEdge.includes('w')) {
            newX = boxStartX + boxStartWidth - newWidth;
          }
        }
        if (newX + newWidth > newCanvas.width) {
          newWidth = newCanvas.width - newX;
          newHeight = newWidth / ratio;
          // Adjust Y position if resizing from top
          if (resizeEdge.includes('n')) {
            newY = boxStartY + boxStartHeight - newHeight;
          }
        }
        if (newY + newHeight > newCanvas.height) {
          newHeight = newCanvas.height - newY;
          newWidth = newHeight * ratio;
          // Adjust X position if resizing from left
          if (resizeEdge.includes('w')) {
            newX = boxStartX + boxStartWidth - newWidth;
          }
        }

        // Final check: if still out of bounds after adjustment, clamp to fit
        if (newX < 0) newX = 0;
        if (newY < 0) newY = 0;
        if (newX + newWidth > newCanvas.width) {
          newWidth = newCanvas.width - newX;
          newHeight = newWidth / ratio;
        }
        if (newY + newHeight > newCanvas.height) {
          newHeight = newCanvas.height - newY;
          newWidth = newHeight * ratio;
        }
      }

      setCropRectInputs({ x: newX, y: newY, width: newWidth, height: newHeight });
      updateCropBox();
    } else if (isDrawing) {
      // Draw new box
      let width = Math.abs(currentX - cropStartX);
      let height = Math.abs(currentY - cropStartY);

      // Apply aspect ratio if selected
      if (currentAspectRatio !== 'free') {
        const ratio = getCropAspectRatioValue(currentAspectRatio);
        if (width / height > ratio) {
          width = height * ratio;
        } else {
          height = width / ratio;
        }
      }

      const x = currentX > cropStartX ? cropStartX : cropStartX - width;
      const y = currentY > cropStartY ? cropStartY : cropStartY - height;

      positionCropBox(cropBox, { x, y, width, height }, scaleX, scaleY);
      setCropRectInputs({ x, y, width, height });
    }
  });

  newCanvas.addEventListener('mouseup', () => {
    isDrawing = false;
    isDragging = false;
    resizeEdge = null;
    newCanvas.style.cursor = 'crosshair';
  });

  newCanvas.addEventListener('mouseleave', () => {
    isDrawing = false;
    isDragging = false;
    resizeEdge = null;
    newCanvas.style.cursor = 'crosshair';
  });

  // Update crop box when inputs change
  bindCropRectInputListeners();
}
function updateCropBox() {
  if (!cropCanvas) return;
  const rect = cropCanvas.getBoundingClientRect();
  const scaleX = cropCanvas.width / rect.width;
  const scaleY = cropCanvas.height / rect.height;

  const cropBox = document.getElementById('cropBox');
  positionCropBox(cropBox, readCropRect(), scaleX, scaleY);
}
function loadCapturedCropFrame(imageSrc, onReady) {
  cropImage = new Image();
  cropImage.onload = function () {
    cropCanvas = document.getElementById('cropCanvas');
    cropCtx = cropCanvas.getContext('2d');
    showCropCanvasContainer();
    cropCanvas.width = cropImage.width;
    cropCanvas.height = cropImage.height;
    cropCtx.fillStyle = 'white';
    cropCtx.fillRect(0, 0, cropCanvas.width, cropCanvas.height);
    cropCtx.drawImage(cropImage, 0, 0);
    setupCanvasInteraction();
    onReady?.();
  };
  cropImage.onerror = function (e) {
    console.error('Image load error:', e);
    showNotification('图片加载失败', 'error');
  };
  cropImage.src = imageSrc;
}

async function switchPendingHolodexStream() {
  const pending = window.pendingHolodexSwitch;
  if (!pending) {
    return false;
  }
  window.pendingHolodexSwitch = null;
  const switchFn = state.hooks.switchToHolodexStream;
  if (typeof switchFn !== 'function') {
    throw new Error('频道切换不可用');
  }
  await switchFn(
    pending.channelId,
    pending.suggestedAreaId,
    pending.title,
    pending.topicId,
    pending.status,
    pending.platform,
    pending.twitchChannelId,
    pending.externalLink
  );
  return true;
}

async function applyCrop() {
  const platform = document.getElementById('cropPlatform').value;
  const cropRect = readCropRect();
  const validationError = validateCropRect(cropRect);

  if (validationError) {
    showNotification(validationError, 'error');
    return;
  }
  const { x, y, width, height } = cropRect;

  try {
    const result = await postCropUpdate(createCropUpdatePayload(platform, true, { x, y, width, height }));

    if (result.success) {
      showNotification('裁剪配置已保存', 'success');
      setCropStatusLabel(platform, '开启');

      // Check if there's a pending Holodex switch
      if (window.pendingHolodexSwitch) {
        closeCropModal();
        showNotification('裁剪已保存，正在切换频道...', 'info');
        await switchPendingHolodexStream();
      } else {
        showNotification('裁剪配置已保存，请重启流使其生效', 'success');
        closeCropModal();
      }
    } else {
      showNotification(result.message || '保存失败', 'error');
    }
  } catch (error) {
    showNotification('保存失败: ' + error.message, 'error');
  }
}
async function applyCropAndRestart() {
  const platform = document.getElementById('cropPlatform').value;
  const cropRect = readCropRect();
  const validationError = validateCropRect(cropRect);

  if (validationError) {
    showNotification(validationError, 'error');
    return;
  }
  const { x, y, width, height } = cropRect;

  try {
    // Check if there's a pending Holodex switch
    if (window.pendingHolodexSwitch) {
      showNotification('正在切换频道...', 'info');
      await switchPendingHolodexStream();
      await new Promise(resolve => setTimeout(resolve, 1000));
    }

    // Step 2: Apply the crop
    const cropResult = await postCropUpdate(createCropUpdatePayload(platform, true, { x, y, width, height }));

    if (cropResult.success) {
      showNotification('裁剪配置已保存，正在重启流...', 'success');
      setCropStatusLabel(platform, '开启');

      closeCropModal();

      // Step 3: Restart the stream
      const restartResult = await postJsonApi('/api/restart');

      if (restartResult.success) {
        showNotification('裁剪已应用并重启流', 'success');
      } else {
        showNotification('裁剪已保存，但重启失败: ' + (restartResult.message || ''), 'error');
      }
    } else {
      showNotification(cropResult.message || '保存失败', 'error');
    }
  } catch (error) {
    showNotification('操作失败: ' + error.message, 'error');
  }
}
async function disableCrop() {
  const platform = document.getElementById('cropPlatform').value;

  try {
    const result = await postCropUpdate(createCropUpdatePayload(platform, false));

    if (result.success) {
      showNotification('裁剪已禁用，请重启流使其生效', 'success');
      closeCropModal();
    } else {
      showNotification(result.message || '禁用失败', 'error');
    }
  } catch (error) {
    showNotification('禁用失败: ' + error.message, 'error');
  }
}
function selectAspectRatio(ratio) {
  if (!ratio) return;

  // Update currentAspectRatio
  currentAspectRatio = ratio;

  // Update selected ratio state.
  document.querySelectorAll('.ratio-button').forEach(btn => {
    btn.classList.remove('active');
  });

  const activeBtn = document.getElementById(`ratio-${ratio.replace(':', '-')}`);
  if (activeBtn) {
    activeBtn.classList.add('active');
  }

  // If canvas is loaded, apply the aspect ratio
  if (cropCanvas && ratio !== 'free') {
    setCropRectInputs(createCenteredCropRect(cropCanvas, ratio));

    updateCropBox();
  }
}

export {
  initCropModalControls,
  readCropRect,
  validateCropRect,
  setCropRectInputs,
  positionCropBox,
  bindCropRectInputListeners,
  showCropCanvasContainer,
  hideCropCanvasContainer,
  setCropStatusLabel,
  createCropUpdatePayload,
  postCropUpdate,
  openCropConfig,
  closeCropModal,
  clearCropConfig,
  autoCaptureFrame,
  loadCurrentCropSettings,
  loadCropImage,
  getCropAspectRatioValue,
  createCenteredCropRect,
  applyAspectRatio,
  setupCanvasInteraction,
  loadCapturedCropFrame,
  updateCropBox,
  applyCrop,
  applyCropAndRestart,
  disableCrop,
  selectAspectRatio,
  cropImage,
  cropStartX,
  cropStartY,
  isDrawing,
  cropCanvas,
  cropCtx,
  currentAspectRatio,
  isDragging,
  dragStartX,
  dragStartY,
  boxStartX,
  boxStartY,
  boxStartWidth,
  boxStartHeight,
  CROP_ASPECT_RATIOS,
};
