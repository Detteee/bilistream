// streams.js — the stream list and the 点播 flow.
//
// Nothing here mutates server state. A click ends at a copyable danmaku
// command; sending it is the viewer's own action in the live chat, which is
// where their identity and your moderation already are.

import { createSvgIcon, createStreamThumbnail, reconcileChildren } from '../../src/js/dom.js';
import {
  formatClock,
  formatDuration,
  formatScheduledStart,
  timestampMs,
} from '../../src/js/format.js';

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

let durationTickerId = null;

function isLiveStream(stream) {
  return String(stream.status || '').toLowerCase() === 'live';
}

function watchUrl(stream) {
  if (stream.link) {
    return stream.link;
  }
  if (!stream.is_placeholder && stream.id) {
    return `https://www.youtube.com/watch?v=${stream.id}`;
  }
  return '';
}

function streamStartMs(stream, isLive) {
  const raw = isLive
    ? (stream.start_actual || stream.available_at || stream.published_at || stream.start_scheduled)
    : stream.start_scheduled;
  return timestampMs(raw);
}

function placeholderKind(stream) {
  const link = (stream.link || '').toLowerCase();
  if (link.includes('nicovideo.jp')) {
    return 'niconico';
  }
  if (stream.placeholder_type === 'twitch' || link.includes('twitch.tv')) {
    return 'twitch';
  }
  return 'radio';
}

function createStrokeIcon(pathData) {
  const svg = createSvgIcon('0 0 24 24', pathData);
  svg.setAttribute('width', '15');
  svg.setAttribute('height', '15');
  svg.setAttribute('fill', 'none');
  svg.setAttribute('stroke', 'currentColor');
  svg.setAttribute('stroke-width', '2');
  svg.setAttribute('stroke-linecap', 'round');
  svg.setAttribute('stroke-linejoin', 'round');
  return svg;
}

function createPlayIcon() {
  const svg = createSvgIcon(
    '0 0 24 24',
    'M7 4.5a1 1 0 0 1 1.53-.85l11 7.5a1 1 0 0 1 0 1.7l-11 7.5A1 1 0 0 1 7 19.5v-15Z',
  );
  svg.setAttribute('width', '15');
  svg.setAttribute('height', '15');
  svg.setAttribute('fill', 'currentColor');
  return svg;
}

function createPlaceholderIcon(kind) {
  const wrapper = document.createElement('span');
  wrapper.className = kind === 'niconico'
    ? 'holodex-duration-niconico-icon'
    : kind === 'twitch'
      ? 'holodex-duration-twitch-icon'
      : 'holodex-duration-radio-icon';
  if (kind === 'niconico') {
    wrapper.appendChild(createSvgIcon(
      '0 0 24 24',
      'M.4787 7.534v12.1279A2.0213 2.0213 0 0 0 2.5 21.6832h2.3888l1.323 2.0948a.4778.4778 0 0 0 .4043.2205.4778.4778 0 0 0 .441-.2205l1.323-2.0948h6.9828l1.323 2.0948a.4778.4778 0 0 0 .441.2205c.1838 0 .3308-.0735.4043-.2205l1.323-2.0948h2.6462a2.0213 2.0213 0 0 0 2.0213-2.0213V7.5339a2.0213 2.0213 0 0 0-2.0213-1.9845h-7.681l4.4468-4.4469L17.1637 0l-5.1452 5.1452L6.8 0 5.6973 1.1025l4.4102 4.4102H2.5367a2.0213 2.0213 0 0 0-2.058 2.058z',
    ));
  } else if (kind === 'twitch') {
    wrapper.appendChild(createSvgIcon('0 0 24 24', 'M11.64 5.93H13.07V10.21H11.64M15.57 5.93H17V10.21H15.57M7 2L3.43 5.57V18.43H7.71V22L11.29 18.43H14.14L20.57 12V2M19.14 11.29L16.29 14.14H13.43L10.93 16.64V14.14H7.71V3.43H19.14Z'));
  } else {
    wrapper.appendChild(createSvgIcon('0 0 24 24', 'M12 10C10.9 10 10 10.9 10 12S10.9 14 12 14 14 13.1 14 12 13.1 10 12 10M18 12C18 8.7 15.3 6 12 6S6 8.7 6 12C6 14.2 7.2 16.1 9 17.2L10 15.5C8.8 14.8 8 13.5 8 12.1C8 9.9 9.8 8.1 12 8.1S16 9.9 16 12.1C16 13.6 15.2 14.9 14 15.5L15 17.2C16.8 16.2 18 14.2 18 12M12 2C6.5 2 2 6.5 2 12C2 15.7 4 18.9 7 20.6L8 18.9C5.6 17.5 4 14.9 4 12C4 7.6 7.6 4 12 4S20 7.6 20 12C20 15 18.4 17.5 16 18.9L17 20.6C20 18.9 22 15.7 22 12C22 6.5 17.5 2 12 2Z'));
  }
  return wrapper;
}

function appendDurationText(duration, text) {
  const span = document.createElement('span');
  span.className = 'holodex-duration-text';
  span.textContent = text;
  duration.appendChild(span);
}

function createPlaceholderDurationOverlay(stream, isLive) {
  const kind = placeholderKind(stream);
  const duration = document.createElement('div');
  duration.className = kind === 'niconico'
    ? 'holodex-stream-duration holodex-stream-duration-niconico'
    : kind === 'twitch'
      ? 'holodex-stream-duration holodex-stream-duration-twitch'
      : 'holodex-stream-duration holodex-stream-duration-radio';

  const startMs = streamStartMs(stream, isLive);
  if (isLive && startMs) {
    duration.dataset.tick = 'live';
    duration.dataset.startMs = String(startMs);
    appendDurationText(duration, formatDuration(Date.now() - startMs));
  } else if (stream.start_scheduled) {
    const start = new Date(stream.start_scheduled);
    appendDurationText(duration, Number.isNaN(start.getTime()) ? '预告' : formatClock(start));
  }

  const hover = document.createElement('span');
  hover.className = 'holodex-duration-hover';
  hover.textContent = kind === 'niconico' ? 'ニコニコ' : kind === 'twitch' ? '外部配信' : '外部直播';
  duration.append(hover, createPlaceholderIcon(kind));
  return duration;
}

function createDurationOverlay(stream, isLive) {
  if (stream.is_placeholder) {
    return createPlaceholderDurationOverlay(stream, isLive);
  }

  const startMs = streamStartMs(stream, isLive);
  if (!(isLive && startMs)) {
    return null;
  }

  const duration = document.createElement('div');
  duration.className = 'holodex-stream-duration holodex-stream-duration-live';
  duration.dataset.tick = 'live';
  duration.dataset.startMs = String(startMs);
  appendDurationText(duration, formatDuration(Date.now() - startMs));
  return duration;
}

export function stopDurationTicker() {
  if (durationTickerId) {
    clearInterval(durationTickerId);
    durationTickerId = null;
  }
}

function updateDurations() {
  const now = Date.now();
  document.querySelectorAll('.holodex-stream-duration[data-tick="live"]').forEach((el) => {
    const startMs = Number(el.dataset.startMs);
    if (!startMs) {
      return;
    }
    const textEl = el.querySelector('.holodex-duration-text');
    const text = formatDuration(now - startMs);
    if (textEl && textEl.textContent !== text) textEl.textContent = text;
  });
  document.querySelectorAll('.holodex-stream-scheduled[data-start]').forEach(el => {
    const text = formatScheduledStart(el.dataset.start);
    if (el.textContent !== text) el.textContent = text;
  });
}

export function startDurationTicker() {
  stopDurationTicker();
  if (document.visibilityState !== 'visible') {
    return;
  }
  updateDurations();
  if (document.querySelector('.holodex-stream-duration[data-tick="live"], .holodex-stream-scheduled[data-start]')) {
    durationTickerId = setInterval(updateDurations, 1000);
  }
}

function createThumb(stream, isLive) {
  const thumb = document.createElement('div');
  thumb.className = 'holodex-stream-thumb';

  const href = watchUrl(stream);
  const media = href ? document.createElement('a') : document.createElement('div');
  if (href) {
    media.className = 'holodex-stream-thumb-link';
    media.href = href;
    media.target = '_blank';
    media.rel = 'noopener noreferrer';
    media.setAttribute('aria-label', `观看 ${stream.channel_name || stream.title || '直播'}`);
  }

  if (stream.thumbnail) {
    media.appendChild(createStreamThumbnail(stream.thumbnail));
  } else {
    const placeholder = document.createElement('div');
    placeholder.className = 'holodex-stream-thumb-placeholder';
    media.appendChild(placeholder);
  }
  thumb.appendChild(media);

  if (stream.topic) {
    const topic = document.createElement('div');
    topic.className = 'holodex-stream-thumb-top';
    const label = document.createElement('span');
    label.className = 'holodex-stream-topic';
    label.textContent = stream.topic;
    topic.appendChild(label);
    thumb.appendChild(topic);
  }

  const duration = createDurationOverlay(stream, isLive);
  if (duration) {
    const bottom = document.createElement('div');
    bottom.className = 'holodex-stream-thumb-bottom';
    bottom.appendChild(duration);
    thumb.appendChild(bottom);
  }

  return thumb;
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

function createWatchLink(href) {
  const watch = document.createElement('a');
  watch.className = 'holodex-stream-watch';
  watch.href = href;
  watch.target = '_blank';
  watch.rel = 'noopener noreferrer';

  const label = document.createElement('span');
  label.textContent = '观看';
  watch.append(createPlayIcon(), label);
  return watch;
}

function createScheduleDivider() {
  const divider = document.createElement('div');
  divider.className = 'holodex-schedule-divider';

  const label = document.createElement('span');
  label.className = 'holodex-schedule-divider-label';
  label.textContent = '预告';

  divider.appendChild(label);
  return divider;
}

function createAvatar(stream) {
  if (!stream.channel_photo) {
    return null;
  }
  const avatar = document.createElement('span');
  avatar.className = 'holodex-stream-avatar';
  const image = document.createElement('img');
  image.src = stream.channel_photo;
  image.alt = '';
  image.loading = 'lazy';
  image.decoding = 'async';
  avatar.appendChild(image);
  return avatar;
}

function createStatusMeta(stream, isLive) {
  const statusMeta = document.createElement('div');
  statusMeta.className = 'holodex-stream-meta';

  if (!isLive) {
    const scheduled = document.createElement('span');
    scheduled.className = 'holodex-stream-scheduled';
    if (stream.start_scheduled) scheduled.dataset.start = stream.start_scheduled;
    scheduled.textContent = stream.start_scheduled
      ? formatScheduledStart(stream.start_scheduled)
      : '预告';
    statusMeta.appendChild(scheduled);
    return statusMeta;
  }

  const liveLabel = document.createElement('span');
  liveLabel.className = 'holodex-stream-live-label';
  liveLabel.textContent = '直播中';
  statusMeta.appendChild(liveLabel);

  const viewers = document.createElement('span');
  viewers.className = 'holodex-stream-viewers';
  statusMeta.appendChild(viewers);
  return statusMeta;
}

function updateViewerCount(card, viewers) {
  const element = card.querySelector('.holodex-stream-viewers');
  if (!element) return;
  const text = Number.isFinite(viewers) && viewers >= 0 ? `• ${viewers.toLocaleString()} 观看` : '';
  if (element.textContent !== text) element.textContent = text;
  element.classList.toggle('hidden', !text);
}

function createStreamCard(stream, isLive) {
  const card = document.createElement('div');
  card.className = 'holodex-stream-card';
  card.append(createThumb(stream, isLive));

  const body = document.createElement('div');
  body.className = 'holodex-stream-body';

  const contentRow = document.createElement('div');
  contentRow.className = 'holodex-stream-content-row';

  const avatar = createAvatar(stream);
  if (avatar) {
    contentRow.appendChild(avatar);
  }

  const lines = document.createElement('div');
  lines.className = 'holodex-stream-lines';

  const title = document.createElement('h4');
  title.className = 'holodex-stream-title';
  title.textContent = stream.title || '-';
  lines.appendChild(title);

  const channelRow = document.createElement('div');
  channelRow.className = 'holodex-stream-channel-row';
  const channel = document.createElement('span');
  channel.className = 'holodex-stream-channel';
  channel.textContent = stream.channel_name || '-';
  channelRow.appendChild(channel);
  lines.appendChild(channelRow);

  lines.appendChild(createStatusMeta(stream, isLive));

  // The 切换 tooltip already has this, but a greyed button is easy to miss,
  // and a keyword block without the words reads as arbitrary.
  if (!streamIsSwitchable(stream)) {
    const note = document.createElement('p');
    note.className = 'holodex-stream-note';
    note.textContent = switchDisabledReason(stream);
    lines.appendChild(note);
  }

  if (stream.suggested_area_name) {
    const area = document.createElement('p');
    area.className = 'holodex-stream-area-hint';
    area.textContent = `建议分区: ${stream.suggested_area_name}`;
    lines.appendChild(area);
  }

  contentRow.appendChild(lines);
  body.appendChild(contentRow);

  const actions = document.createElement('div');
  actions.className = 'holodex-stream-actions';
  const href = watchUrl(stream);
  if (href) {
    actions.appendChild(createWatchLink(href));
  }
  actions.appendChild(createSwitchButton(stream));
  body.appendChild(actions);

  card.appendChild(body);
  updateViewerCount(card, stream.live_viewers);
  return card;
}

export function renderStreams(streams, { fresh = true } = {}) {
  streamsFresh = fresh && Array.isArray(streams);
  const gate = `${danmakuEnabled}:${restreaming}:${statusFresh}:${streamsFresh}:${areasFresh}:${areas.some(areaIsUsable)}`;
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
  const appendCard = (stream, isLive) => {
    const baseKey = `${stream.command_platform}:${stream.id}`;
    const occurrence = occurrences.get(baseKey) || 0;
    occurrences.set(baseKey, occurrence + 1);
    const key = `${baseKey}:${occurrence}`;
    const { live_viewers, ...content } = stream;
    const signature = `${renderedGate}:${JSON.stringify(content)}`;
    const previous = renderedCards.get(key);
    const element = previous?.signature === signature ? previous.element : createStreamCard(stream, isLive);
    updateViewerCount(element, live_viewers);
    nextCards.set(key, { signature, element });
    desired.push(element);
  };

  const live = streams.filter(isLiveStream);
  const upcoming = streams.filter((stream) => !isLiveStream(stream));
  upcoming.sort((a, b) => {
    const timeA = streamStartMs(a, false) ?? Infinity;
    const timeB = streamStartMs(b, false) ?? Infinity;
    return timeA - timeB;
  });

  for (const stream of live) {
    appendCard(stream, true);
  }
  if (upcoming.length && live.length) {
    scheduleDivider ??= createScheduleDivider();
    desired.push(scheduleDivider);
  }
  for (const stream of upcoming) {
    appendCard(stream, false);
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
