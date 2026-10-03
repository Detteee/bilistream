// streams.js — the stream list and the 点播 flow.
//
// Nothing here mutates server state. A click ends at a copyable danmaku
// command; sending it is the viewer's own action in the live chat, which is
// where their identity and your moderation already are.

import { reconcileChildren } from '../../src/js/dom.js';
import { toCardModel, streamStartMs } from '../../src/js/stream-model.js';
import { streamIsOnAir } from '../../src/js/on-air.js';
import {
  createScheduleDivider,
  createStrokeIcon,
  createWatchLink,
  renderCard,
  startDurationTicker,
  stopDurationTicker,
  updateViewerCount,
  watchUrl,
} from '../../src/js/stream-card.js';

export { startDurationTicker, stopDurationTicker };

/// Mirrors the reasons the server sends, so a greyed button can say why.
const REASON_LABELS = {
  danmaku_disabled: '弹幕点播当前已关闭',
  restreaming: '转播期间暂停点播',
  banned_keyword: '不可点播',
  earlier_banned_keyword: '不可点播',
  unsupported_platform: '该平台不支持弹幕点播',
  unknown_channel: '该频道不在点播列表中',
  no_command_name: '该频道没有可用的点播名称',
};

let lastStreams = null;
let renderedGate = null;
let renderedCards = new Map();
let scheduleDivider = null;
// Status, stream eligibility, and area tokens must all be current before a
// command can be generated. A stream poll arriving first cannot open the gate.
let danmakuEnabled = null;
let statusFresh = false;
let streamsFresh = false;
let areasFresh = false;
/// True while an active node is pushing. Distinct from the config switch: a
/// restream keeps that on and only gates the processor.
let restreaming = false;
/// The publishing node's platform / channel / title, or null while idle.
let onAir = null;

function rerenderStreams() {
  if (lastStreams) renderStreams(lastStreams, { fresh: streamsFresh });
  revalidatePendingStream();
}

export function setStatusFreshness(fresh) {
  if (statusFresh === !!fresh) return;
  statusFresh = !!fresh;
  rerenderStreams();
}

export function setStreamsFreshness(fresh) {
  if (streamsFresh === !!fresh) return;
  streamsFresh = !!fresh;
  rerenderStreams();
}

export function setOnAir(next) {
  const normalized = next && (next.platform || next.channel_name || next.title)
    ? {
      platform: next.platform || '',
      channel_name: next.channel_name || '',
      title: next.title || '',
    }
    : null;
  if (JSON.stringify(onAir) === JSON.stringify(normalized)) return;
  onAir = normalized;
  rerenderStreams();
}

export function setDanmakuEnabled(enabled, isRestreamingNow) {
  const next = !!enabled;
  const nextRestreaming = !!isRestreamingNow;
  if (danmakuEnabled === next && restreaming === nextRestreaming) {
    return;
  }
  danmakuEnabled = next;
  restreaming = nextRestreaming;
  rerenderStreams();
}

function streamIsSwitchable(stream) {
  if (!statusFresh || !streamsFresh || !areasFresh || !areas.some(areaIsUsable)) return false;
  // Same payload as the 转播中 badge (`clusterIsRestreaming`). Do not wait
  // for a second poll: if JP is already pushing, 切换 is not requestable.
  if (restreaming || danmakuEnabled === false) {
    return false;
  }
  // A Holodex snapshot taken while the processor was down still says
  // danmaku_disabled; once 转播 ends and the gate is on, that stale reason
  // must not keep 切换 grey.
  if (danmakuEnabled === true && stream.reason === 'danmaku_disabled') {
    return true;
  }
  return !!stream.switchable;
}

function switchDisabledReason(stream) {
  if (!statusFresh) return '直播状态未同步，暂不可点播';
  if (!streamsFresh) return '直播列表未更新，暂不可点播';
  if (!areasFresh) return '分区未加载，正在重试';
  if (!areas.some(areaIsUsable)) return '暂无可用分区';
  if (restreaming) {
    return REASON_LABELS.restreaming;
  }
  if (danmakuEnabled === false) {
    return REASON_LABELS.danmaku_disabled;
  }
  return reasonText(stream);
}

function reasonText(stream) {
  const label = REASON_LABELS[stream.reason] || '不可点播';
  const keywords = Array.isArray(stream.reason_keywords) ? stream.reason_keywords : [];
  const showsKeywords =
    stream.reason === 'banned_keyword' || stream.reason === 'earlier_banned_keyword';
  if (!showsKeywords || keywords.length === 0) {
    return label;
  }
  return `${label}：标题/分区包含 ${keywords.join('/')}`;
}

/// A regular account cannot send a danmaku longer than this, so a command over
/// it needs a shorter form even though the formal one is what we lead with.
const DANMAKU_REGULAR_LIMIT = 20;

let areas = [];
let pendingStream = null;
let selectedAreaId = null;
let copyResetTimer = null;
let commandAreaId = null;
let commandSession = 0;

/// Bilibili's catch-all 其他单机. Pinned first, same as the dashboard picker.
const DEFAULT_AREA_ID = 235;

function pinDefaultAreaFirst(list) {
  const defaults = [];
  const rest = [];
  for (const area of list) {
    (Number(area?.id) === DEFAULT_AREA_ID ? defaults : rest).push(area);
  }
  return defaults.concat(rest);
}

export function setAreas(list) {
  const next = pinDefaultAreaFirst(Array.isArray(list) ? list : []);
  const changed = JSON.stringify(areas) !== JSON.stringify(next);
  areasFresh = Array.isArray(list);
  areas = next;
  if (changed && pendingStream) {
    if (!document.getElementById('command-modal')?.classList.contains('hidden')) closeCommandModal();
    else if (!document.getElementById('area-modal')?.classList.contains('hidden')) openAreaModal(pendingStream);
  }
  rerenderStreams();
}

/// `%转播%<平台>%<频道>%<分区>` — the format danmaku.rs parses.
function buildCommand(platform, channel, area) {
  return `%转播%${platform}%${channel}%${area}`;
}

function length(text) {
  return [...text].length;
}

/// Every token that could stand in for an area in a command. The parser strips
/// whitespace before matching, so a name or alias containing any can never
/// resolve and is left out entirely.
function areaTokens(area) {
  if (!area) {
    return [];
  }
  const candidates = [area.name, ...(Array.isArray(area.aliases) ? area.aliases : [])];
  const seen = new Set();
  return candidates.filter((token) => {
    if (!token || /\s/.test(token) || seen.has(token)) {
      return false;
    }
    seen.add(token);
    return true;
  });
}

function shortestToken(tokens) {
  return tokens.reduce((shortest, token) => (length(token) < length(shortest) ? token : shortest));
}

/// An area nobody could name in a working command is not offered at all.
function areaIsUsable(area) {
  return areaTokens(area).length > 0;
}

function areaById(id) {
  return areas.find((area) => area.id === id) || null;
}

function isLiveStream(stream) {
  return String(stream.status || '').toLowerCase() === 'live';
}

function createSwitchButton(stream) {
  const button = document.createElement('button');
  button.type = 'button';
  button.className = 'holodex-stream-btn holodex-stream-btn-switch';

  const icon = createStrokeIcon([
    { d: 'M22 12c0 6-4.39 10-9.806 10C7.792 22 4.24 19.665 3 16m-1-4C2 6 6.39 2 11.807 2C16.208 2 19.758 4.335 21 8' },
    { d: 'm7 17l-4-1l-1 4M17 7l4 1l1-4' },
  ]);
  const label = document.createElement('span');
  label.textContent = '切换';
  button.append(icon, label);

  if (!streamIsSwitchable(stream)) {
    button.disabled = true;
    button.setAttribute('aria-disabled', 'true');
    button.title = switchDisabledReason(stream);
    return button;
  }

  button.addEventListener('click', () => startSwitch(stream));
  return button;
}

function createStreamCard(stream) {
  const model = toCardModel(stream);
  const onAirNow = streamIsOnAir(model, onAir);
  const href = watchUrl(model);
  const actions = [];
  if (href) {
    actions.push(createWatchLink(href));
  }
  actions.push(createSwitchButton(stream));
  const card = renderCard(model, {
    actions,
    onAir: onAirNow,
    href,
    thumbnail: model.thumbnail,
    note: streamIsSwitchable(stream) ? '' : switchDisabledReason(stream),
    areaHint: model.suggestedAreaName ? `建议分区: ${model.suggestedAreaName}` : '',
  });
  updateViewerCount(card, stream.live_viewers);
  return card;
}

function setStreamCounts(live, upcoming) {
  const liveEl = document.getElementById('public-live-count');
  const upcomingEl = document.getElementById('public-upcoming-count');
  const liveText = String(live);
  const upcomingText = String(upcoming);
  if (liveEl && liveEl.textContent !== liveText) liveEl.textContent = liveText;
  if (upcomingEl && upcomingEl.textContent !== upcomingText) upcomingEl.textContent = upcomingText;
}

export function renderStreams(streams, { fresh = true } = {}) {
  streamsFresh = fresh && Array.isArray(streams);
  const onAirGate = onAir ? `${onAir.platform}\n${onAir.channel_name}\n${onAir.title}` : '';
  const gate = `${danmakuEnabled}:${restreaming}:${statusFresh}:${streamsFresh}:${areasFresh}:${areas.some(areaIsUsable)}:${onAirGate}`;
  const unchanged = lastStreams === streams && renderedGate === gate;
  lastStreams = Array.isArray(streams) ? streams : null;
  revalidatePendingStream();
  const container = document.getElementById('holodex-streams');
  const status = document.getElementById('holodex-status');
  if (!container) {
    return;
  }

  stopDurationTicker();

  if (!Array.isArray(streams) || streams.length === 0) {
    container.replaceChildren();
    renderedCards.clear();
    setStreamCounts(0, 0);
    setStatus(status, '当前没有正在直播或即将开播的频道');
    return;
  }

  setStatus(status, streamsFresh ? null : '直播列表暂时无法更新，显示上次结果；点播已暂停');
  if (unchanged) {
    startDurationTicker();
    return;
  }
  renderedGate = gate;
  const nextCards = new Map();
  const occurrences = new Map();
  const desired = [];
  const appendCard = (stream) => {
    const baseKey = `${stream.command_platform}:${stream.id}`;
    const occurrence = occurrences.get(baseKey) || 0;
    occurrences.set(baseKey, occurrence + 1);
    const key = `${baseKey}:${occurrence}`;
    const { live_viewers, ...content } = stream;
    const onAirFlag = streamIsOnAir(toCardModel(stream), onAir) ? '1' : '0';
    const signature = `${renderedGate}:${onAirFlag}:${JSON.stringify(content)}`;
    const previous = renderedCards.get(key);
    const element = previous?.signature === signature ? previous.element : createStreamCard(stream);
    updateViewerCount(element, live_viewers);
    nextCards.set(key, { signature, element });
    desired.push(element);
  };

  const live = streams.filter(isLiveStream);
  const upcoming = streams.filter((stream) => !isLiveStream(stream));
  setStreamCounts(live.length, upcoming.length);
  upcoming.sort((a, b) => {
    const timeA = streamStartMs(toCardModel(a), false) ?? Infinity;
    const timeB = streamStartMs(toCardModel(b), false) ?? Infinity;
    return timeA - timeB;
  });

  for (const stream of live) {
    appendCard(stream);
  }
  if (upcoming.length && live.length) {
    scheduleDivider ??= createScheduleDivider();
    desired.push(scheduleDivider);
  }
  for (const stream of upcoming) {
    appendCard(stream);
  }

  reconcileChildren(container, desired);
  renderedCards = nextCards;
  startDurationTicker();
}

export function setStatus(element, message) {
  const status = element || document.getElementById('holodex-status');
  if (!status) {
    return;
  }
  status.textContent = message || '';
  status.classList.toggle('hidden', !message);
}

// 点播 flow ------------------------------------------------------------------

function revalidatePendingStream() {
  if (!pendingStream) return null;
  const matches = (lastStreams || []).filter(stream => stream.id === pendingStream.id
    && stream.command_platform === pendingStream.command_platform
    && stream.command_channel === pendingStream.command_channel);
  const current = matches.includes(pendingStream) ? pendingStream : matches.length === 1 ? matches[0] : null;
  if (!current || !streamIsSwitchable(current)
    || current.command_channel_short !== pendingStream.command_channel_short) {
    closeAreaModal();
    closeCommandModal();
    return null;
  }
  pendingStream = current;
  return current;
}

function startSwitch(stream) {
  if (!streamIsSwitchable(stream)) {
    return;
  }
  pendingStream = stream;

  const suggested = areaById(stream.suggested_area_id);
  if (suggested && areaIsUsable(suggested)) {
    showCommand(stream, suggested);
    return;
  }
  openAreaModal(stream);
}

function openAreaModal(stream) {
  const modal = document.getElementById('area-modal');
  const list = document.getElementById('area-modal-list');
  const label = document.getElementById('area-modal-stream');
  const confirm = document.getElementById('area-modal-confirm');
  if (!modal || !list) {
    return;
  }

  if (label) {
    label.textContent = `${stream.channel_name || ''} · ${stream.title || ''}`.trim();
  }

  selectedAreaId = null;
  if (confirm) {
    confirm.disabled = true;
  }

  const usable = areas.filter(areaIsUsable);
  if (usable.length === 0) {
    const empty = document.createElement('p');
    empty.className = 'area-option-empty';
    empty.textContent = '没有可用分区';
    list.replaceChildren(empty);
  } else {
    list.replaceChildren(...usable.map((area) => createAreaOption(area)));
  }

  modal.classList.remove('hidden');
  list.querySelector('.area-option')?.focus();
}

function createAreaOption(area) {
  const button = document.createElement('button');
  button.type = 'button';
  button.className = 'area-option';
  button.setAttribute('role', 'option');
  button.setAttribute('aria-selected', 'false');
  button.dataset.id = String(area.id);
  button.textContent = area.name;
  button.addEventListener('click', () => {
    if (selectedAreaId === area.id) {
      confirmArea();
      return;
    }
    selectArea(area.id);
  });
  return button;
}

function selectArea(id) {
  selectedAreaId = id;
  const confirm = document.getElementById('area-modal-confirm');
  if (confirm) {
    confirm.disabled = false;
  }
  document.querySelectorAll('#area-modal-list .area-option').forEach((button) => {
    const on = Number(button.dataset.id) === id;
    button.classList.toggle('is-selected', on);
    button.setAttribute('aria-selected', on ? 'true' : 'false');
  });
}

export function closeAreaModal() {
  document.getElementById('area-modal')?.classList.add('hidden');
  selectedAreaId = null;
  pendingStream = null;
}

export function confirmArea() {
  const stream = revalidatePendingStream();
  const chosen = areaById(selectedAreaId);
  if (!chosen || !areaIsUsable(chosen) || !stream) {
    return;
  }
  closeAreaModal();
  showCommand(stream, chosen);
}

/// The command to show, plus a shorter one when the formal names do not fit a
/// regular account's danmaku.
function commandForms(stream, area) {
  const tokens = areaTokens(area);
  if (tokens.length === 0 || !stream.command_channel) {
    return null;
  }

  const platform = stream.command_platform;
  const primary = buildCommand(platform, stream.command_channel, tokens[0]);
  const shortChannel = stream.command_channel_short || stream.command_channel;
  const short = buildCommand(platform, shortChannel, shortestToken(tokens));

  return {
    primary,
    // Only worth showing when it is both needed and actually shorter.
    short: length(primary) > DANMAKU_REGULAR_LIMIT && short !== primary ? short : null,
    // Remaining ways to name the same area, for a viewer who prefers one.
    alternatives: tokens.slice(1),
  };
}

function showCommand(stream, area) {
  if (!streamIsSwitchable(stream)) return;
  const modal = document.getElementById('command-modal');
  const input = document.getElementById('command-text');
  if (!modal || !input) {
    return;
  }

  const forms = commandForms(stream, area);
  if (!forms) {
    return;
  }

  pendingStream = stream;
  commandAreaId = area.id;
  commandSession += 1;
  input.value = forms.primary;
  renderShortForm(forms.short);
  renderAlternatives(forms.alternatives);
  setFeedback('');
  resetCopyButtons();
  modal.classList.remove('hidden');
  input.focus();
  input.select();
}

function renderShortForm(short) {
  const row = document.getElementById('command-short-row');
  const input = document.getElementById('command-short-text');
  if (!row || !input) {
    return;
  }
  row.classList.toggle('hidden', !short);
  input.value = short || '';
}

function renderAlternatives(aliases) {
  const note = document.getElementById('command-alternatives');
  if (!note) {
    return;
  }
  note.textContent = aliases.length
    ? `分区也可写作：${aliases.join('、')}`
    : '';
  note.classList.toggle('hidden', aliases.length === 0);
}

export function closeCommandModal() {
  document.getElementById('command-modal')?.classList.add('hidden');
  pendingStream = null;
  commandAreaId = null;
  commandSession += 1;
  resetCopyButtons();
}

function setFeedback(message) {
  const feedback = document.getElementById('command-feedback');
  if (feedback) {
    feedback.textContent = message || '';
    feedback.classList.toggle('hidden', !message);
  }
}

function copyButtonFor(inputId) {
  return document.getElementById(inputId === 'command-short-text' ? 'command-short-copy' : 'command-copy');
}

function setCopyState(button, copied) {
  const icon = button?.querySelector('use');
  if (!button || !icon) {
    return;
  }
  icon.setAttribute('href', copied ? '#i-check' : '#i-copy');
  button.classList.toggle('is-copied', copied);
  const label = copied ? '已复制' : '复制';
  button.title = label;
  button.setAttribute('aria-label', label);
}

function resetCopyButtons() {
  if (copyResetTimer) {
    clearTimeout(copyResetTimer);
    copyResetTimer = null;
  }
  setCopyState(document.getElementById('command-copy'), false);
  setCopyState(document.getElementById('command-short-copy'), false);
}

export async function copyCommand(inputId = 'command-text') {
  const stream = revalidatePendingStream();
  const area = areaById(commandAreaId);
  if (!stream || !area || document.getElementById('command-modal')?.classList.contains('hidden')) return;
  const input = document.getElementById(inputId);
  if (!input) {
    return;
  }

  const button = copyButtonFor(inputId);
  const session = commandSession;
  try {
    // Clipboard access needs a secure context; select-and-copy is the
    // fallback when the page is opened over plain http.
    await navigator.clipboard.writeText(input.value);
    if (session !== commandSession) return;
    setFeedback('已复制，去直播间发送即可。');
    setCopyState(button, true);
    if (copyResetTimer) {
      clearTimeout(copyResetTimer);
    }
    copyResetTimer = setTimeout(() => setCopyState(button, false), 1600);
  } catch {
    if (session !== commandSession) return;
    input.select();
    setCopyState(button, false);
    setFeedback('复制失败，请手动选中并复制。');
  }
}
