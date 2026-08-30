// format.js — pure formatters shared by the dashboard, the cluster panel and
// the status cards. Nothing here touches the DOM.

export function formatNetworkRate(kbps) {
  if (!Number.isFinite(kbps) || kbps <= 0) {
    return '-';
  }
  if (kbps >= 1000) {
    return `${(kbps / 1000).toFixed(2)} Mb/s`;
  }
  return `${Math.round(kbps)} Kb/s`;
}

export function formatSpeedRatio(value) {
  return Number.isFinite(value) && value > 0 ? `${value.toFixed(2)}x` : '-';
}

export function formatStreamTime(secs) {
  if (!Number.isFinite(secs) || secs < 0) {
    return '-';
  }
  const totalSec = Math.floor(secs);
  const h = Math.floor(totalSec / 3600);
  const m = Math.floor((totalSec % 3600) / 60);
  const s = totalSec % 60;
  if (h > 0) {
    return `${h}:${String(m).padStart(2, '0')}:${String(s).padStart(2, '0')}`;
  }
  return `${m}:${String(s).padStart(2, '0')}`;
}

export function formatFps(value) {
  if (!Number.isFinite(value) || value < 0) {
    return '-';
  }
  return value >= 100 ? `${Math.round(value)}` : value.toFixed(1);
}

export function formatFrameCount(value) {
  return Number.isFinite(value) && value >= 0 ? Math.round(value).toLocaleString() : '-';
}

export function formatHlsCacheStatus(enabled, latencySecs) {
  return enabled ? `${latencySecs || 8}秒` : '关闭';
}

export function timestampMs(value) {
  if (!value) {
    return null;
  }
  const ms = new Date(value).getTime();
  return Number.isNaN(ms) ? null : ms;
}

export function formatClock(value) {
  const date = value instanceof Date ? value : new Date(value);
  if (Number.isNaN(date.getTime())) {
    return '';
  }
  return `${String(date.getHours()).padStart(2, '0')}:${String(date.getMinutes()).padStart(2, '0')}`;
}

// Elapsed live time, same H:MM:SS / M:SS the dashboard paints on thumbnails.
export function formatDuration(ms) {
  const totalSec = Math.max(0, Math.floor(ms / 1000));
  const h = Math.floor(totalSec / 3600);
  const m = Math.floor((totalSec % 3600) / 60);
  const s = totalSec % 60;
  if (h > 0) {
    return `${h}:${String(m).padStart(2, '0')}:${String(s).padStart(2, '0')}`;
  }
  return `${m}:${String(s).padStart(2, '0')}`;
}

export function formatScheduledStart(startScheduled) {
  const start = new Date(startScheduled);
  if (Number.isNaN(start.getTime())) {
    return '预告';
  }

  const now = Date.now();
  const diffMs = start.getTime() - now;
  const clock = formatClock(start);

  if (diffMs <= 0) {
    return `即将开播 (${clock})`;
  }

  const diffMinutes = diffMs / (1000 * 60);
  if (diffMinutes < 60) {
    const minutes = Math.max(1, Math.ceil(diffMinutes));
    return `将在 ${minutes} 分钟内开播 (${clock})`;
  }

  const diffHours = diffMs / (1000 * 60 * 60);
  if (diffHours < 24) {
    const hours = Math.ceil(diffHours);
    return `将在 ${hours} 小时内开播 (${clock})`;
  }

  const y = start.getFullYear();
  const m = start.getMonth() + 1;
  const d = start.getDate();
  return `将在 ${y}/${m}/${d}开播 (${clock})`;
}

export function getQualityDisplayText(technicalValue, platform = 'youtube') {
  const qualityMappings = {
    youtube: {
      'best': '最佳质量',
      'best[height<=1080]': '超清 (1080p)',
      'best[height<=720]': '高清 (720p)',
      'best[height<=480]': '标清 (480p)',
      'best[height<=360]': '流畅 (360p)',
      'worst': '最低质量'
    },
    twitch: {
      'best': '原画质量',
      'high': '高质量 (720p)',
      'medium': '中等质量 (540p)',
      'low': '低质量 (360p)',
      'audio_only': '仅音频',
      'worst': '最低质量'
    },
    niconico: {
      'best': '最佳质量',
      '1080p60': '1080p60',
      '720p60': '720p60',
      '450p': '450p',
      '288p': '288p'
    }
  };

  return qualityMappings[platform]?.[technicalValue] || technicalValue;
}

export function asBitrateHistory(values) {
  if (!Array.isArray(values)) {
    return [];
  }
  return values.map((value) => (Number.isFinite(value) && value > 0 ? value : 0));
}

export function formatAreaText(areaName, areaId) {
  return areaName ? `${areaName} (${areaId})` : (areaId || '-');
}

// YouTube Data API key pool (/api/youtube/keys) for the settings view.
function formatCount(value) {
  return (Number.isFinite(value) ? value : 0).toLocaleString('en-US');
}

function formatClock(iso) {
  const at = new Date(iso);
  if (Number.isNaN(at.getTime())) return '';
  return `${String(at.getHours()).padStart(2, '0')}:${String(at.getMinutes()).padStart(2, '0')}`;
}

// WebSub subscriptions and the last push; '' while WebSub is off.
export function formatWebSubStatus(websub) {
  if (!websub) return '';
  if (websub.error) return websub.error;
  const parts = [`WebSub 订阅：已验证 ${websub.verified} · 等待 ${websub.pending} · 失败 ${websub.failed}`];
  const last = websub.last_push ? formatClock(websub.last_push) : '';
  parts.push(last ? `上次推送 ${last}` : '尚未收到推送');
  return parts.join(' · ');
}

// Rows for the key meters: share of the daily budget used.
export function keyMeterRows(data) {
  const budget = data?.budget_per_key || 0;
  return (Array.isArray(data?.keys) ? data.keys : []).map(key => {
    const used = Number.isFinite(key.used) ? key.used : 0;
    return {
      label: key.fingerprint,
      state: key.state,
      fraction: key.state === 'exhausted' ? 1 : budget ? Math.min(1, used / budget) : 0,
      value: key.state === 'usable' ? `${formatCount(used)} / ${formatCount(budget)}` : formatKeyState(key).split(' · ')[1],
    };
  });
}

// Status tiles: { name, tone: 'ok' | 'warn' | 'bad' | 'off', label, detail }.
export function discoveryTiles(data) {
  const playlist = data?.playlist;
  const websub = data?.websub;
  const tiles = [
    {
      name: 'RSS',
      tone: playlist?.rss_down ? 'bad' : 'ok',
      label: playlist?.rss_down ? '故障' : '正常',
      detail: playlist?.rss_down ? '上传列表接替' : '每 3 分钟',
    },
    {
      name: '上传列表',
      tone: !playlist ? 'off' : playlist.on ? (playlist.paused || playlist.stretch > 1 ? 'warn' : 'ok') : 'off',
      label: !playlist ? '—' : !playlist.on ? '关闭' : playlist.paused ? '暂停' : `每频道 ${playlist.interval_secs}s`,
      detail: formatPlaylistPolling(playlist),
    },
  ];
  if (websub) {
    tiles.push({
      name: 'WebSub',
      tone: websub.error || websub.failed ? 'bad' : websub.healthy ? 'ok' : 'warn',
      label: websub.error ? '无法监听' : `${websub.verified} 已验证`,
      detail: formatWebSubStatus(websub),
    });
  }
  return tiles;
}

export function formatKeyPoolSummary(data) {
  const keys = Array.isArray(data?.keys) ? data.keys : [];
  const used = keys.reduce((sum, key) => sum + (Number.isFinite(key.used) ? key.used : 0), 0);
  const total = keys.length * (data?.budget_per_key || 0);
  const left = Math.round((data?.remaining_fraction || 0) * 100);
  const usable = keys.filter(key => key.state === 'usable').length;
  const reset = formatClock(data?.resets_at);
  const parts = [
    `今日已用 ${formatCount(used)} / ${formatCount(total)} 单位`,
    `剩余 ${left}%`,
    `${usable}/${keys.length} 个 key 可用`,
  ];
  if (reset) parts.push(`${reset} 重置`);
  return parts.join(' · ');
}

export function formatKeyState(key) {
  switch (key?.state) {
    case 'exhausted':
      return `${key.fingerprint} · 今日额度已用完`;
    case 'rejected':
      return `${key.fingerprint} · 被拒绝（检查 key 或 API 是否启用）`;
    default:
      return `${key?.fingerprint} · ${formatCount(key?.used)}`;
  }
}

export function formatPlaylistPolling(playlist) {
  if (!playlist) return '';
  const byHour = playlist.by_hour ? '（按开播时段）' : '';
  const stretched = playlist.stretch > 1 ? `（配额留给索引，间隔 ×${playlist.stretch}）` : '';
  const slowed = playlist.websub_slowed ? '（WebSub 正常，放慢一倍）' : '';
  if (playlist.on && playlist.paused) return '上传列表轮询：暂停（今日剩余配额留给索引与转播目标）';
  if (playlist.on) {
    return playlist.rss_down
      ? `RSS 故障，上传列表每频道 ${playlist.interval_secs}s${byHour}${stretched}${slowed}`
      : `上传列表轮询：每频道 ${playlist.interval_secs}s${byHour}${stretched}${slowed}`;
  }
  if (playlist.interval_secs == null) return '上传列表轮询：关闭（没有可用 key）';
  if (playlist.keys_needed) {
    return `上传列表轮询：关闭（RSS 正常；${playlist.keys_needed} 个可用 key 起常规轮询）`;
  }
  return '上传列表轮询：关闭';
}

// Browser UTC offset in whole hours, e.g. 8 for UTC+8.
export function localHourOffset(date = new Date()) {
  return Math.round(-date.getTimezoneOffset() / 60);
}

// The 24 UTC-ordered hours of `playlist.hours`, rotated to local hours 0-23.
// Rows: { hour, share, intervalSecs, pollsPerHour, current }.
export function goliveHourRows(playlist, offset = localHourOffset(), now = new Date()) {
  const hours = Array.isArray(playlist?.hours) && playlist.hours.length === 24 ? playlist.hours : null;
  if (!hours) return [];
  const currentUtc = now.getUTCHours();
  return Array.from({ length: 24 }, (_, hour) => {
    const utc = (((hour - offset) % 24) + 24) % 24;
    const { golive_share: share = 0, interval_secs: intervalSecs = null } = hours[utc] || {};
    return {
      hour,
      share: Number.isFinite(share) ? share : 0,
      intervalSecs,
      pollsPerHour: intervalSecs ? 3600 / intervalSecs : 0,
      current: utc === currentUtc,
    };
  });
}

// One line under the chart, so no value needs a hover to be read.
export function formatGoliveSummary(playlist, rows) {
  if (!playlist || !rows.length) return '';
  if (!playlist.by_hour) {
    const need = Math.max(0, (playlist.golives_needed || 0) - (playlist.golives || 0));
    const pace = playlist.interval_secs ? `，目前均匀每频道 ${playlist.interval_secs}s` : '';
    return `已记录 ${formatCount(playlist.golives || 0)} 次开播，还需 ${formatCount(need)} 次后按开播时段轮询${pace}`;
  }
  const timed = rows.filter(row => row.intervalSecs);
  if (!timed.length) return '上传列表轮询暂停';
  const fastest = timed.reduce((best, row) => (row.intervalSecs < best.intervalSecs ? row : best));
  const slowest = Math.max(...timed.map(row => row.intervalSecs));
  const busiest = rows.reduce((best, row) => (row.share > best.share ? row : best));
  return `开播最多 ${busiest.hour} 点（${Math.round(busiest.share * 100)}%）· 最快 ${fastest.hour} 点每 ${fastest.intervalSecs}s · 最慢每 ${slowest}s`;
}
