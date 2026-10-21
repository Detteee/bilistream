/// Live-room links for the platform cards on 总览.
///
/// Each card names one platform and shows what that platform is doing right
/// now, so the natural next click is "go look at it". The URL is derived from
/// the `/api/status` payload rather than stored in markup: the channel ids are
/// user configuration and change without a reload.
///
/// A card whose payload carries no id renders as plain text — the anchor keeps
/// no `href`, so it is not focusable and not announced as a link.

const LINK_BUILDERS = {
  // The room is the push target: the room itself is the live page.
  bilibili: data => (data?.room > 0 ? `https://live.bilibili.com/${data.room}` : ''),
  // `/live` resolves to the current stream when the channel is on air and to
  // the channel page when it is not, so the link never goes stale.
  youtube: data => (data?.channel_id ? `https://www.youtube.com/channel/${data.channel_id}/live` : ''),
  // Twitch's channel id is the login, which is also the URL segment.
  twitch: data => (data?.channel_id ? `https://www.twitch.tv/${data.channel_id}` : ''),
  // Prefer the 放送 page while live; fall back to the channel page otherwise.
  niconico: data => {
    if (data?.live_id) return `https://live.nicovideo.jp/watch/${data.live_id}`;
    return data?.channel_id ? `https://ch.nicovideo.jp/${data.channel_id}` : '';
  },
};

const LINK_LABELS = {
  bilibili: '在 Bilibili 打开直播间',
  youtube: '在 YouTube 打开直播间',
  twitch: '在 Twitch 打开直播间',
  niconico: '在 Niconico 打开直播间',
};

export function platformRoomUrl(platform, data) {
  const build = LINK_BUILDERS[platform];
  return build ? build(data) : '';
}

/// Points one card's title link at `url`, or strips it back to plain text.
export function applyPlatformLink(platform, url) {
  const link = document.getElementById(`${platform}-card-link`);
  const header = link?.closest('.dashboard-card-header');
  if (!link) {
    return;
  }
  if (url) {
    link.href = url;
    link.target = '_blank';
    link.rel = 'noopener noreferrer';
    link.title = LINK_LABELS[platform] || '打开直播间';
    header?.classList.add('is-linkable');
  } else {
    link.removeAttribute('href');
    link.removeAttribute('target');
    link.removeAttribute('rel');
    link.removeAttribute('title');
    header?.classList.remove('is-linkable');
  }
}

/// Convenience for the card renderers: derive the URL and paint it in one go.
export function renderPlatformLink(platform, data) {
  applyPlatformLink(platform, platformRoomUrl(platform, data));
}

/// Paints every card from one `/api/status` payload. Called next to
/// `renderStatusCards` so a card's link always matches the ids it just showed.
export function renderPlatformLinks(status) {
  renderPlatformLink('bilibili', status?.bilibili);
  renderPlatformLink('youtube', status?.youtube);
  renderPlatformLink('twitch', status?.twitch);
  renderPlatformLink('niconico', status?.niconico);
}

/// The title link is the accessible affordance; the header around it is the
/// generous click target. Clicks on the header's own controls, on text the
/// user is selecting, and on anything interactive inside the card are left
/// alone so editing never navigates away.
export function initPlatformCardLinks() {
  for (const card of document.querySelectorAll('.card[data-platform]')) {
    const header = card.querySelector('.dashboard-card-header');
    if (!header || header.dataset.linkBound === 'true') {
      continue;
    }
    header.dataset.linkBound = 'true';
    header.addEventListener('click', event => {
      if (event.defaultPrevented || event.button !== 0) return;
      if (event.metaKey || event.ctrlKey || event.shiftKey || event.altKey) return;
      if (event.target.closest('a, button, input, select, textarea, label, summary, [role="button"]')) return;
      if (!window.getSelection()?.isCollapsed) return;
      card.querySelector('.dashboard-card-link[href]')?.click();
    });
  }
}
