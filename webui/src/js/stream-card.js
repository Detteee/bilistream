// stream-card.js — the 直播与预告 card, shared by the admin dashboard and the
// public status page.
//
// One renderer for both pages; the page passes a normalized model
// (stream-model.js) plus its own actions and link targets. Pure DOM
// construction: nothing here fetches, mutates state, or writes the server.

import { createSvgIcon, createStreamThumbnail } from './dom.js';
import { formatClock, formatDuration, formatScheduledStart } from './format.js';
import { streamStartMs } from './stream-model.js';

// Placeholder platform ------------------------------------------------------

/// Niconico first, then Twitch, else radio. Reads the normalized model so the
/// admin `external_link` rows and the public `link` rows resolve identically.
export function placeholderKind(model) {
  const link = String(model.link || '').toLowerCase();
  if (link.includes('nicovideo.jp')) {
    return 'niconico';
  }
  if (model.placeholderType === 'twitch' || link.includes('twitch.tv')) {
    return 'twitch';
  }
  return 'radio';
}

// Icons ----------------------------------------------------------------------

export function createStrokeIcon(pathData) {
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

const WATCH_PATH = 'M7 4.5a1 1 0 0 1 1.53-.85l11 7.5a1 1 0 0 1 0 1.7l-11 7.5A1 1 0 0 1 7 19.5v-15Z';

export function createPlayIcon() {
  const svg = createSvgIcon('0 0 24 24', WATCH_PATH);
  svg.setAttribute('width', '15');
  svg.setAttribute('height', '15');
  svg.setAttribute('fill', 'currentColor');
  return svg;
}

export function createWatchLink(href) {
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

// Duration overlay ----------------------------------------------------------

function appendDurationText(duration, text) {
  const span = document.createElement('span');
  span.className = 'holodex-duration-text';
  span.textContent = text;
  duration.appendChild(span);
}

function createPlaceholderDurationOverlay(model) {
  const kind = placeholderKind(model);
  const duration = document.createElement('div');
  duration.className = kind === 'niconico'
    ? 'holodex-stream-duration holodex-stream-duration-niconico'
    : kind === 'twitch'
      ? 'holodex-stream-duration holodex-stream-duration-twitch'
      : 'holodex-stream-duration holodex-stream-duration-radio';

  const startMs = streamStartMs(model, model.isLive);
  if (model.isLive && startMs) {
    duration.dataset.tick = 'live';
    duration.dataset.startMs = String(startMs);
    appendDurationText(duration, formatDuration(Date.now() - startMs));
  } else if (model.startScheduled) {
    const start = new Date(model.startScheduled);
    appendDurationText(duration, Number.isNaN(start.getTime()) ? '预告' : formatClock(start));
  }

  const hover = document.createElement('span');
  hover.className = 'holodex-duration-hover';
  hover.textContent = kind === 'niconico' ? 'ニコニコ' : kind === 'twitch' ? '外部配信' : '外部直播';
  duration.append(hover, createPlaceholderIcon(kind));
  return duration;
}

function createDurationOverlay(model) {
  if (model.isPlaceholder) {
    return createPlaceholderDurationOverlay(model);
  }

  const startMs = streamStartMs(model, model.isLive);
  if (!(model.isLive && startMs)) {
    return null;
  }

  const duration = document.createElement('div');
  duration.className = 'holodex-stream-duration holodex-stream-duration-live';
  duration.dataset.tick = 'live';
  duration.dataset.startMs = String(startMs);
  appendDurationText(duration, formatDuration(Date.now() - startMs));
  return duration;
}

// Duration ticker ------------------------------------------------------------

let durationTickerId = null;

/// The one ticker both pages drive. `shouldTick` decides whether a running
/// page may start it (public: tab visible; admin: Holodex panel open).
export function startDurationTicker(shouldTick = () => document.visibilityState === 'visible') {
  stopDurationTicker();
  if (!shouldTick()) {
    return;
  }
  updateDurations();
  if (document.querySelector('.holodex-stream-duration[data-tick="live"], .holodex-stream-scheduled[data-start]')) {
    durationTickerId = setInterval(updateDurations, 1000);
  }
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

// Card pieces ----------------------------------------------------------------

export function createScheduleDivider() {
  const divider = document.createElement('div');
  divider.className = 'holodex-schedule-divider';

  const label = document.createElement('span');
  label.className = 'holodex-schedule-divider-label';
  label.textContent = '预告';

  divider.appendChild(label);
  return divider;
}

export function updateViewerCount(card, viewers) {
  const element = card.querySelector('.holodex-stream-viewers');
  if (!element) return;
  const text = Number.isFinite(viewers) && viewers >= 0 ? `• ${viewers.toLocaleString()} 观看` : '';
  if (element.textContent !== text) element.textContent = text;
  element.classList.toggle('hidden', !text);
}

/// The card's watch target: the platform link when present, else the YouTube
/// watch page. `''` when nothing can be watched (a linked placeholder without
/// a fallback id would be a dead destination).
export function watchUrl(model) {
  if (model.link) {
    return model.link;
  }
  return !model.isPlaceholder && model.id ? `https://www.youtube.com/watch?v=${model.id}` : '';
}

function createThumb(model, onAir, href, thumbnail) {
  const thumb = document.createElement('div');
  thumb.className = 'holodex-stream-thumb';

  const media = href ? document.createElement('a') : document.createElement('div');
  if (href) {
    media.className = 'holodex-stream-thumb-link';
    media.href = href;
    media.target = '_blank';
    media.rel = 'noopener noreferrer';
    media.setAttribute('aria-label', `观看 ${model.channelName || model.title || '直播'}`);
  }

  if (thumbnail) {
    media.appendChild(createStreamThumbnail(thumbnail));
  } else {
    const placeholder = document.createElement('div');
    placeholder.className = 'holodex-stream-thumb-placeholder';
    media.appendChild(placeholder);
  }
  thumb.appendChild(media);

  if (model.topic) {
    const topic = document.createElement('div');
    topic.className = 'holodex-stream-thumb-top';
    const label = document.createElement('span');
    label.className = 'holodex-stream-topic';
    label.textContent = model.topic;
    topic.appendChild(label);
    thumb.appendChild(topic);
  }

  if (onAir) {
    const badge = document.createElement('div');
    badge.className = 'holodex-stream-thumb-on-air';
    const mark = document.createElement('span');
    mark.className = 'holodex-stream-on-air';
    mark.textContent = '正在转播';
    badge.appendChild(mark);
    thumb.appendChild(badge);
  }

  const duration = createDurationOverlay(model);
  if (duration) {
    const bottom = document.createElement('div');
    bottom.className = 'holodex-stream-thumb-bottom';
    bottom.appendChild(duration);
    thumb.appendChild(bottom);
  }

  return thumb;
}

function createAvatar(model, photo, href) {
  if (!photo) {
    return null;
  }
  const avatar = href ? document.createElement('a') : document.createElement('span');
  avatar.className = 'holodex-stream-avatar';
  if (href) {
    avatar.href = href;
    avatar.target = '_blank';
    avatar.rel = 'noopener noreferrer';
    avatar.title = model.channelName || 'channel';
  }
  const image = document.createElement('img');
  image.src = photo;
  image.alt = '';
  image.loading = 'lazy';
  image.decoding = 'async';
  avatar.appendChild(image);
  return avatar;
}

function createChannelRow(model, href, extra) {
  const row = document.createElement('div');
  row.className = 'holodex-stream-channel-row';

  const channel = href ? document.createElement('a') : document.createElement('span');
  channel.className = 'holodex-stream-channel';
  channel.textContent = model.channelName || '-';
  if (href) {
    channel.href = href;
    channel.target = '_blank';
    channel.rel = 'noopener noreferrer';
  }
  row.appendChild(channel);

  if (extra) {
    row.appendChild(extra);
  }
  return row;
}

function createStatusMeta(model) {
  const statusMeta = document.createElement('div');
  statusMeta.className = 'holodex-stream-meta';

  if (!model.isLive) {
    const scheduled = document.createElement('span');
    scheduled.className = 'holodex-stream-scheduled';
    if (model.startScheduled) scheduled.dataset.start = model.startScheduled;
    scheduled.textContent = model.startScheduled
      ? formatScheduledStart(model.startScheduled)
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

// The card -------------------------------------------------------------------

/// Renders one 直播与预告 card from a normalized model. Page-specific input:
/// - `actions`: elements for the action row (watch, switch, crop, add-channel)
/// - `href`: the watch target; the thumb + avatar link to the platform
/// - `channelHref` / `avatarHref`: wrap channel/avatar in a link (admin links
///   to Holodex; the public page passes '' → plain text)
/// - `note` / `areaHint`: optional caption lines (public's disabled reason)
export function renderCard(model, {
  actions = [],
  onAir = false,
  href = watchUrl(model),
  thumbnail = model.thumbnail,
  channelHref = '',
  avatarHref = '',
  avatarPhoto = model.channelPhoto,
  note = '',
  areaHint = '',
  channelExtra = null,
} = {}) {
  const card = document.createElement('div');
  card.className = onAir ? 'holodex-stream-card is-on-air' : 'holodex-stream-card';
  card.append(createThumb(model, onAir, href, thumbnail));

  const body = document.createElement('div');
  body.className = 'holodex-stream-body';

  const contentRow = document.createElement('div');
  contentRow.className = 'holodex-stream-content-row';

  const avatar = createAvatar(model, avatarPhoto, avatarHref);
  if (avatar) {
    contentRow.appendChild(avatar);
  }

  const lines = document.createElement('div');
  lines.className = 'holodex-stream-lines';

  const title = document.createElement('h4');
  title.className = 'holodex-stream-title';
  title.textContent = model.title || '-';
  lines.appendChild(title);

  lines.appendChild(createChannelRow(model, channelHref, channelExtra));
  lines.appendChild(createStatusMeta(model));

  if (note) {
    const caption = document.createElement('p');
    caption.className = 'holodex-stream-note';
    caption.textContent = note;
    lines.appendChild(caption);
  }
  if (areaHint) {
    const hint = document.createElement('p');
    hint.className = 'holodex-stream-area-hint';
    hint.textContent = areaHint;
    lines.appendChild(hint);
  }

  contentRow.appendChild(lines);
  body.appendChild(contentRow);

  const actionRow = document.createElement('div');
  actionRow.className = 'holodex-stream-actions';
  for (const action of actions) {
    actionRow.appendChild(action);
  }
  body.appendChild(actionRow);

  card.appendChild(body);
  return card;
}