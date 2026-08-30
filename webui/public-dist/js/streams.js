// streams.js — the stream list and the 点播 flow.
//
// Nothing here mutates server state. A click ends at a copyable danmaku
// command; sending it is the viewer's own action in the live chat, which is
// where their identity and your moderation already are.

import { createSvgIcon } from '/shared/js/dom.js';
import { formatScheduledStart } from '/shared/js/format.js';

/// Mirrors the reasons the server sends, so a greyed button can say why.
const REASON_LABELS = {
  danmaku_disabled: '弹幕点播当前已关闭',
  banned_keyword: '该直播不可点播',
  unsupported_platform: '该平台不支持弹幕点播',
  unknown_channel: '该频道不在点播列表中',
  no_command_name: '该频道没有可用的点播名称',
};

/// A regular account cannot send a danmaku longer than this, so a command over
/// it needs a shorter form even though the formal one is what we lead with.
const DANMAKU_REGULAR_LIMIT = 20;

let areas = [];
let pendingStream = null;

export function setAreas(list) {
  areas = Array.isArray(list) ? list : [];
}

/// `%转播%<平台>%<频道>%<分区>` — the format danmaku.rs parses.
export function buildCommand(platform, channel, area) {
  return `%转播%${platform}%${channel}%${area}`;
}

function length(text) {
  return [...text].length;
}

/// Every token that could stand in for an area in a command. The parser strips
/// whitespace before matching, so a name or alias containing any can never
/// resolve and is left out entirely.
export function areaTokens(area) {
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

function createThumb(stream) {
  const thumb = document.createElement('div');
  thumb.className = 'holodex-stream-thumb';

  if (stream.thumbnail) {
    const image = document.createElement('img');
    image.loading = 'lazy';
    image.alt = '';
    image.src = stream.thumbnail;
    thumb.appendChild(image);
  } else {
    const placeholder = document.createElement('div');
    placeholder.className = 'holodex-stream-thumb-placeholder';
    thumb.appendChild(placeholder);
  }

  if (stream.topic) {
    const topic = document.createElement('div');
    topic.className = 'holodex-stream-thumb-top';
    const label = document.createElement('span');
    label.className = 'holodex-stream-topic';
    label.textContent = stream.topic;
    topic.appendChild(label);
    thumb.appendChild(topic);
  }

  return thumb;
}

/// Same shape as the dashboard's action icons, so the two pages read alike.
function createStreamIcon(pathData) {
  const svg = createSvgIcon('0 0 24 24', pathData);
  svg.setAttribute('width', '14');
  svg.setAttribute('height', '14');
  svg.setAttribute('fill', 'none');
  svg.setAttribute('stroke', 'currentColor');
  svg.setAttribute('stroke-width', '2');
  svg.setAttribute('stroke-linecap', 'round');
  svg.setAttribute('stroke-linejoin', 'round');
  return svg;
}

function createSwitchButton(stream) {
  const button = document.createElement('button');
  button.type = 'button';
  button.className = 'holodex-stream-btn holodex-stream-btn-switch';

  const icon = createStreamIcon([
    { d: 'M22 12c0 6-4.39 10-9.806 10C7.792 22 4.24 19.665 3 16m-1-4C2 6 6.39 2 11.807 2C16.208 2 19.758 4.335 21 8' },
    { d: 'm7 17l-4-1l-1 4M17 7l4 1l1-4' },
  ]);
  const label = document.createElement('span');
  label.textContent = '切换';
  button.append(icon, label);

  if (!stream.switchable) {
    button.disabled = true;
    button.setAttribute('aria-disabled', 'true');
    button.title = REASON_LABELS[stream.reason] || '该直播不可点播';
    return button;
  }

  button.addEventListener('click', () => startSwitch(stream));
  return button;
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

function createStreamCard(stream) {
  const card = document.createElement('div');
  card.className = 'holodex-stream-card';
  card.append(createThumb(stream));

  const body = document.createElement('div');
  body.className = 'holodex-stream-body';

  const title = document.createElement('div');
  title.className = 'holodex-stream-title';
  title.textContent = stream.title || '-';

  const channelRow = document.createElement('div');
  channelRow.className = 'holodex-stream-channel-row';
  const channel = document.createElement('span');
  channel.className = 'holodex-stream-channel';
  channel.textContent = stream.channel_name || '-';
  channelRow.appendChild(channel);

  body.append(title, channelRow);

  if (stream.status !== 'live' && stream.start_scheduled) {
    const scheduled = document.createElement('div');
    scheduled.className = 'holodex-stream-note';
    scheduled.textContent = formatScheduledStart(stream.start_scheduled);
    body.appendChild(scheduled);
  }

  if (!stream.switchable) {
    const note = document.createElement('div');
    note.className = 'holodex-stream-note';
    note.textContent = REASON_LABELS[stream.reason] || '该直播不可点播';
    body.appendChild(note);
  }

  const actions = document.createElement('div');
  actions.className = 'holodex-stream-actions';
  if (stream.link) {
    const watch = document.createElement('a');
    watch.className = 'holodex-stream-watch';
    watch.href = stream.link;
    watch.target = '_blank';
    watch.rel = 'noopener noreferrer';
    watch.textContent = '观看';
    actions.appendChild(watch);
  }
  actions.appendChild(createSwitchButton(stream));
  body.appendChild(actions);

  card.appendChild(body);
  return card;
}

export function renderStreams(streams) {
  const container = document.getElementById('holodex-streams');
  const status = document.getElementById('holodex-status');
  if (!container) {
    return;
  }

  if (!Array.isArray(streams) || streams.length === 0) {
    container.replaceChildren();
    setStatus(status, '当前没有正在直播或即将开播的频道');
    return;
  }

  setStatus(status, null);
  const fragment = document.createDocumentFragment();

  // Live first, upcoming after, with a divider between the two groups the way
  // the dashboard separates them.
  const live = streams.filter((stream) => stream.status === 'live');
  const upcoming = streams.filter((stream) => stream.status !== 'live');

  for (const stream of live) {
    fragment.appendChild(createStreamCard(stream));
  }
  if (upcoming.length && live.length) {
    fragment.appendChild(createScheduleDivider());
  }
  for (const stream of upcoming) {
    fragment.appendChild(createStreamCard(stream));
  }

  container.replaceChildren(fragment);
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

function startSwitch(stream) {
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
  const select = document.getElementById('area-modal-select');
  const label = document.getElementById('area-modal-stream');
  if (!modal || !select) {
    return;
  }

  if (label) {
    label.textContent = `${stream.channel_name || ''} · ${stream.title || ''}`.trim();
  }

  const options = [option('', '选择分区...')];
  for (const area of areas.filter(areaIsUsable)) {
    options.push(option(String(area.id), area.name));
  }
  select.replaceChildren(...options);
  select.value = '';

  modal.classList.remove('hidden');
  select.focus();
}

function option(value, label) {
  const element = document.createElement('option');
  element.value = value;
  element.textContent = label;
  return element;
}

export function closeAreaModal() {
  document.getElementById('area-modal')?.classList.add('hidden');
}

export function confirmArea() {
  const select = document.getElementById('area-modal-select');
  const chosen = areaById(Number(select?.value));
  if (!chosen || !areaIsUsable(chosen) || !pendingStream) {
    return;
  }
  closeAreaModal();
  showCommand(pendingStream, chosen);
}

/// The command to show, plus a shorter one when the formal names do not fit a
/// regular account's danmaku.
export function commandForms(stream, area) {
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
  const modal = document.getElementById('command-modal');
  const input = document.getElementById('command-text');
  if (!modal || !input) {
    return;
  }

  const forms = commandForms(stream, area);
  if (!forms) {
    return;
  }

  input.value = forms.primary;
  renderShortForm(forms.short);
  renderAlternatives(forms.alternatives);
  setFeedback('');
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
}

function setFeedback(message) {
  const feedback = document.getElementById('command-feedback');
  if (feedback) {
    feedback.textContent = message;
  }
}

export async function copyCommand(inputId = 'command-text') {
  const input = document.getElementById(inputId);
  if (!input) {
    return;
  }

  try {
    // Clipboard access needs a secure context; select-and-copy is the
    // fallback when the page is opened over plain http.
    await navigator.clipboard.writeText(input.value);
    setFeedback('已复制，去直播间发送即可。');
  } catch (error) {
    input.select();
    setFeedback('复制失败，请手动选中并复制。');
  }
}
