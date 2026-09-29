// Synthetic API only: never reads runtime config, credentials or provider APIs.
import { createServer } from 'node:http';
import { readFile } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import { resolve, extname, sep } from 'node:path';

const root = resolve(fileURLToPath(new URL('../dist/', import.meta.url)));
const ok = data => ({ success: true, data });
export function createMockServer() {
  let needsSetup = false;
  let sessionSaved = false;
  let favoriteMode = 'ok';
  const favorites = Array.from({ length: 160 }, (_, i) => ({ id: 'UC' + String(i).padStart(22, '0'), name: i === 0 ? 'Demo Favourite' : i === 1 ? '星海放送局 · International Gaming, Music and Weekend Collaboration' : `Demo Channel ${i + 1}` }));
  const catalog = [{id:235,name:'其他单机',parent_name:'单机游戏'},{id:329,name:'无畏契约',parent_name:'网游'}, ...Array.from({ length: 300 }, (_, i) => ({ id: 1000 + i, name: i === 0 ? '开放世界探索与多人合作冒险 · Open World and Multiplayer Adventures' : `演示分区 ${i + 1}`, parent_name: `演示分类 ${Math.floor(i / 30) + 1}` }))];
  const config = {
    interval: 30, auto_cover: true, show_priority_channel: false, show_twitch: true, show_niconico: false, youtube_rss_enabled: true,
    holodex_monitor_gate: true, enable_lol_monitor: false, enable_anti_collision: false,
    anti_collision_list: {}, holodex_api_key: 'demo-holodex-key', holodex_jwt_configured: false,
    youtube_api_key: 'demo-key-project-a\ndemo-key-project-b',
    youtube_websub_callback_url: 'https://yt.example.com/websub/youtube', youtube_websub_port: 3151,
    bilibili: { room: 10000, enable_danmaku_command: true },
    youtube: { enable_monitor: true, channel_name: 'Demo Studio', channel_id: 'UCdemo', area_v2: 235, quality: 'best', proxy: '', ffmpeg_cache: { enabled: true, latency_secs: 8 } },
    twitch: { enable_monitor: true, channel_name: 'Demo Games', channel_id: 'demo_games', area_v2: 235, quality: 'best', proxy_region: 'asl', proxy: '', ffmpeg_cache: { enabled: false, latency_secs: 8 } },
    niconico: { user_session_configured: false, session_check_enabled: true, enable_monitor: false, channel_name: '', channel_id: '', cookies_file: '', proxy: '' },
    priority_channel: { enabled: true, auto_restart: true, channel_name: 'Demo Music', default_area: 235 },
    cluster: { enabled: false, node_id: 'local', node_name: 'Demo Computer', public_api_url: '', peers: [], priority: 0, heartbeat_interval_secs: 5, failover_timeout_secs: 20, lease_ttl_secs: 30, auto_failover: true, sync_monitored_channels: true, thresholds: { max_failed_restarts: 3, max_external_api_failures: 3, window_secs: 300 }, public_status: { node_id: '', base_url: '', bind: '127.0.0.1', port: 23234, holodex_refresh_secs: 60 } },
  };
  const network = { ffmpeg_running: true, stream_speed: 1, stream_cache_speed: 1.04, stream_bitrate_kbps: 6040, stream_cache_bitrate_kbps: 6230, stream_fps: 60, stream_frame: 148200, stream_time_secs: 2470, stream_cache_time_secs: 2478, hls_cache_active: true, stream_bitrate_history: [5900,6050,6000,6250,6100,6080,6040], stream_cache_bitrate_history: [6100,6240,6300,6150,6400,6230,6230] };
  const channel = (platform, live) => ({ ...config[platform], is_live: live, title: live ? '一起探索新的世界 · Demo live' : '-', topic: 'Gaming', game: 'Just Chatting', area_id: 235, area_name: '其他单机', crop_enabled: false, ffmpeg_cache_enabled: platform === 'youtube', ffmpeg_cache_latency_secs: 8 });
  const events = new Set();
  const writes = [];
  const server = createServer(async (req, res) => {
    const path = new URL(req.url, 'http://localhost').pathname;
    const send = (body, status = 200) => { res.writeHead(status, { 'Content-Type': 'application/json', 'Cache-Control': 'no-store' }); res.end(JSON.stringify(body)); };
    try {
      if (path === '/mock/favorites-mode') { favoriteMode = new URL(req.url, 'http://localhost').searchParams.get('mode') || 'ok'; return send(ok(null)); }
      if (path === '/mock/setup-mode') { needsSetup = true; return send(ok(null)); }
      if (path === '/api/events') {
        res.writeHead(200, { 'Content-Type': 'text/event-stream', 'Cache-Control': 'no-store' });
        res.write(': mock connected\n\n'); events.add(res); req.on('close', () => events.delete(res)); return;
      }
      if (req.method === 'POST') {
        let text = ''; for await (const chunk of req) text += chunk;
        const patch = JSON.parse(text || '{}');
        if (path === '/api/setup/holodex-favorites') {
          if (favoriteMode === 'error') return send({ success: false, message: 'Holodex 凭据无效，请重新登录' });
          if (favoriteMode === 'slow') await new Promise(resolve => setTimeout(resolve, 400));
          return send(ok(favoriteMode === 'empty' ? [] : favorites));
        }
        writes.push({ path, patch });
        if (path === '/api/cluster/public-status') {
          config.cluster.public_status = patch.config;
          for (const event of events) event.write('event: config\ndata: changed\n\n');
          return send({ success: true, message: '公开页设置已保存', data: { enabled: false, nodes: [], local_node_id: 'local', public_status: patch.config } });
        }
        if (path === '/api/channels/resolve-youtube') return send(ok({ channel_id: 'UCvUc0m317LWTTPZoBQV479A' }));
        if (path === '/api/niconico/session/check') return send(ok({ state: sessionSaved ? 'valid' : 'unconfigured', message: sessionSaved ? '会话仍被接受；本次检查不会续期' : '未配置 user_session' }));
        if (path === '/api/setup/save-config') { needsSetup = false; return send({ success: true }); }
        if (path === '/api/config') {
          for (const [key, before] of Object.entries(patch.expected || {})) {
            if (key in config && JSON.stringify(config[key]) !== JSON.stringify(before)) return send({ success: false, message: '配置冲突' }, 409);
          }
          const { expected, niconico_user_session, clear_niconico_user_session, niconico_session_check_enabled, ...changes } = patch;
          if (niconico_user_session) sessionSaved = true;
          if (clear_niconico_user_session) sessionSaved = false;
          config.niconico.user_session_configured = sessionSaved;
          if (niconico_session_check_enabled != null) config.niconico.session_check_enabled = niconico_session_check_enabled;
          Object.assign(config, changes);
          for (const event of events) event.write('event: config\ndata: changed\n\n');
        } else if (path === '/api/priority-channel') Object.assign(config.priority_channel, patch);
        else if (path !== '/api/banned-keywords') return send({ success: false, message: 'Unsupported mock action' }, 404);
        return send({ success: true, message: '配置已保存' });
      }
      if (path === '/mock/writes') return send(writes);
      if (path === '/api/auth') return send({ required: false, authenticated: true });
      if (path === '/api/setup-status') return send({ needs_setup: needsSetup });
      if (path === '/api/setup/login-status') return send({ logged_in: true });
      if (path === '/api/niconico/session') return send(ok({ state: sessionSaved ? 'unchecked' : 'unconfigured', message: sessionSaved ? '尚未检查；检查不会延长会话有效期' : '未配置 user_session' }));
      if (path === '/api/areas/catalog') return send(ok(catalog));
      if (path === '/api/manage/areas') return send(ok({areas:[{id:235,name:'其他单机',aliases:[],title_keywords:[]}]}));
      if (path === '/api/manage/channels') return send(ok({channels:[]}));
      if (path === '/api/config') return send(config);
      if (path === '/api/version') return send(ok({ version: '0.6.2', is_tauri: false }));
      if (path === '/api/update/check') return send(ok({ has_update: false, current_version: '0.6.2', latest_version: '0.6.2' }));
      if (path === '/api/status') return send(ok({ bilibili: { ...network, is_live: true, title: 'Demo Studio | 一起探索新的世界', area_id: 235, area_name: '其他单机', stream_quality: '1080p60', enable_danmaku_command: true, live_start_ts: Math.floor(Date.now() / 1000) - 2470 }, youtube: channel('youtube', true), twitch: channel('twitch', false), niconico: null, priority_channel: { ...config.priority_channel, is_live: false, platform: null, title: null } }));
      if (path.startsWith('/api/refresh/')) return send(ok(null));
      if (path === '/api/network-status') return send(ok(network));
      if (path === '/api/logs') return send({ success: true, logs: '12:00:00 INFO 模拟数据 · Web UI preview\n12:00:01 INFO 转播运行中' });
      if (path === '/api/cluster/status') return send(ok({ enabled: false, nodes: [], local_can_enable_monitor_toggles: true }));
      if (path === '/api/banned-keywords') return send({ streaming_banned_keywords: [], danmaku_banned_keywords: [] });
      if (path === '/api/areas') return send({ areas: [{ id: 235, name: '其他单机' }, { id: 329, name: '无畏契约' }] });
      if (path === '/api/channels') return send({ channels: [{ name: 'Demo Studio', platforms: { youtube: 'UCdemo' } }, { name: 'Demo Games', platforms: { twitch: 'demo_games' } }, { name: 'Demo Music', platforms: { youtube: 'UCmusic' } }] });
      if (path === '/api/youtube/keys') return send(ok({ configured: true, budget_per_key: 9000, remaining_fraction: .78, resets_at: '2026-09-30T07:00:00Z', keys: [{ fingerprint: 'demo…a', used: 2480, state: 'usable' }, { fingerprint: 'demo…b', used: 1480, state: 'usable' }], playlist: { on: true, interval_secs: 180, rss_enabled: config.youtube_rss_enabled, rss_down: false, stretch: 1, websub_slowed: true }, websub: { verified: 24, pending: 0, failed: 0, healthy: true, listening: 3151, last_push: '2026-09-29T04:00:00Z' } }));
      if (path === '/api/holodex/streams') return send({ success: true, source: 'channels', data: [
        { id: 'demoLive', title: '一起探索新的世界', channel_name: 'Demo Studio', channel_id: 'UCdemo', status: 'live', start_actual: new Date(Date.now()-2470000).toISOString(), suggested_area_id: 235, suggested_area_name: '其他单机', thumbnail: '/mock-thumbnail.svg', channel_photo: '/icon-blue.png', live_viewers: 1280 },
        { id: 'demoNext', title: '晚间音乐时光', channel_name: 'Demo Music', channel_id: 'UCmusic', status: 'upcoming', start_scheduled: new Date(Date.now()+3600000).toISOString(), suggested_area_id: 235, suggested_area_name: '其他单机', thumbnail: '/mock-thumbnail.svg', channel_photo: '/icon-blue.png' },
      ] });
      if (path === '/mock-thumbnail.svg') {
        res.writeHead(200, { 'Content-Type': 'image/svg+xml' });
        return res.end('<svg xmlns="http://www.w3.org/2000/svg" width="640" height="360"><defs><linearGradient id="g"><stop stop-color="#82afa5"/><stop offset="1" stop-color="#d9c7ad"/></linearGradient></defs><rect width="640" height="360" fill="url(#g)"/><circle cx="480" cy="60" r="140" fill="#ffffff" opacity=".16"/><text x="50" y="170" font-size="48" font-family="sans-serif" fill="#fff">DEMO STUDIO</text><text x="52" y="216" font-size="20" font-family="sans-serif" fill="#fff">BILISTREAM · LIVE PREVIEW</text></svg>');
      }
      if (path.startsWith('/api/')) return send({ success: false, message: `Unknown mock route: ${path}` }, 404);
      const file = resolve(root, path === '/' ? 'index.html' : `.${path}`);
      if (!file.startsWith(root + sep) && file !== resolve(root, 'index.html')) return send({}, 403);
      const body = await readFile(file);
      const mime = { '.html': 'text/html', '.js': 'text/javascript', '.css': 'text/css', '.png': 'image/png' }[extname(file)] || 'application/octet-stream';
      res.writeHead(200, { 'Content-Type': mime, 'Cache-Control': 'no-store' }); res.end(body);
    } catch (error) { send({ success: false, message: error.message }, 500); }
  });
  return server;
}
if (process.argv[1] === fileURLToPath(import.meta.url)) {
  createMockServer().listen(8765, '127.0.0.1', () => console.log('Mock UI: http://127.0.0.1:8765'));
}
