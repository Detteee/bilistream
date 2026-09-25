// format.js — pure formatters shared by the dashboard and the status cards.
// Nothing here touches the DOM.

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

export function formatScheduledStart(startScheduled) {
  const start = new Date(startScheduled);
  if (Number.isNaN(start.getTime())) {
    return '预告';
  }

  const now = Date.now();
  const diffMs = start.getTime() - now;
  const clock = `${String(start.getHours()).padStart(2, '0')}:${String(start.getMinutes()).padStart(2, '0')}`;

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
  const stretched = playlist.stretch > 1 ? `（配额偏快，间隔 ×${playlist.stretch}）` : '';
  if (playlist.on) {
    return playlist.rss_down
      ? `RSS 故障，上传列表每频道 ${playlist.interval_secs}s${stretched}`
      : `上传列表轮询：每频道 ${playlist.interval_secs}s${stretched}`;
  }
  if (playlist.interval_secs == null) return '上传列表轮询：关闭（没有可用 key）';
  if (playlist.stretch > 1) return `上传列表轮询：关闭（配额偏快，间隔 ×${playlist.stretch} 后慢于 RSS）`;
  if (playlist.keys_needed) {
    return `上传列表轮询：关闭（RSS 正常；${playlist.keys_needed} 个可用 key 起常规轮询）`;
  }
  return '上传列表轮询：关闭';
}
