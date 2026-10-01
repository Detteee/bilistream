// Synthetic API only: never reads runtime config, credentials or provider APIs.
import { createServer } from 'node:http';
import { readFile } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import { resolve, extname, sep } from 'node:path';

const root = resolve(fileURLToPath(new URL('../dist/', import.meta.url)));
const ok = data => ({ success: true, data });
export function createMockServer() {
  let needsSetup = false;
  let panelPassword = null, panelGeneration = 1, remoteListener = false, setupResponseLoss = false;
  let passwordAttempts = 0, setupAttempts = 0;
  let sessionSaved = false;
  let cookieRevision = 1, cookieCount = 0, filterRevision = 1, playerFilter = '';
  const proxyValues = {};
  const proxyMask = value => value.replace(/:([^/@]*)@/, (_, password) => ':' + '•'.repeat([...decodeURIComponent(password)].length) + '@');
  let favoriteMode = 'ok';
  const favorites = Array.from({ length: 160 }, (_, i) => ({ id: 'UC' + String(i).padStart(22, '0'), name: i === 0 ? '示例收藏频道 001' : i === 1 ? '示例收藏频道 002 / Example Channel 002 / サンプルチャンネル 002' : `示例收藏频道 ${String(i + 1).padStart(3, '0')}` }));
  const catalog = [{id:235,name:'其他单机',parent_name:'单机游戏'},{id:329,name:'无畏契约',parent_name:'网游'}, ...Array.from({ length: 300 }, (_, i) => ({ id: 1000 + i, name: i === 0 ? '开放世界探索与多人合作冒险 · Open World and Multiplayer Adventures' : `演示分区 ${i + 1}`, parent_name: `演示分类 ${Math.floor(i / 30) + 1}` }))];
  const config = {
    interval: 30, auto_cover: true, show_priority_channel: false, show_twitch: true, show_niconico: false, youtube_rss_enabled: true,
    holodex_monitor_gate: true, enable_lol_monitor: false, enable_anti_collision: false,
    anti_collision_list: {}, holodex_api_key: '', holodex_api_key_configured: true, holodex_api_key_mask: '•'.repeat(24), holodex_jwt_configured: false, secret_revision: 1,
    youtube_api_key: '', youtube_api_key_configured: true, youtube_api_key_mask: '•'.repeat(39) + '\n' + '•'.repeat(39), riot_api_key: '', riot_api_key_configured: false, riot_api_key_mask: '',
    youtube_websub_callback_url: 'https://yt.example.com/websub/youtube', youtube_websub_port: 3151,
    bilibili: { room: 10000, enable_danmaku_command: true },
    youtube: { enable_monitor: true, channel_name: '示例频道 001', channel_id: 'UC1111111111111111111111', area_v2: 235, quality: 'best', proxy: '', ffmpeg_cache: { enabled: true, latency_secs: 8 } },
    twitch: { enable_monitor: true, channel_name: '示例频道 002', channel_id: 'demo_games', area_v2: 235, quality: 'best', proxy_region: 'asl', proxy: '', ffmpeg_cache: { enabled: false, latency_secs: 8 } },
    niconico: { user_session_configured: false, session_check_enabled: true, enable_monitor: false, channel_name: '', channel_id: '', cookies_file: '', proxy: '' },
    priority_channel: { enabled: true, auto_restart: true, channel_name: '示例频道 003', default_area: 235 },
    cluster: { enabled: false, node_id: 'local', node_name: 'Demo Computer', public_api_url: '', peers: [], priority: 0, heartbeat_interval_secs: 5, failover_timeout_secs: 20, lease_ttl_secs: 30, auto_failover: true, sync_monitored_channels: true, thresholds: { max_failed_restarts: 3, max_external_api_failures: 3, window_secs: 300 }, public_status: { node_id: '', base_url: '', bind: '127.0.0.1', port: 23234, holodex_refresh_secs: 60 } },
  };
  const network = { ffmpeg_running: true, stream_speed: 1, stream_cache_speed: 1.04, stream_bitrate_kbps: 6040, stream_cache_bitrate_kbps: 6230, stream_fps: 60, stream_frame: 148200, stream_time_secs: 2470, stream_cache_time_secs: 2478, hls_cache_active: true, stream_bitrate_history: [5900,6050,6000,6250,6100,6080,6040], stream_cache_bitrate_history: [6100,6240,6300,6150,6400,6230,6230] };
  const channel = (platform, live) => ({ ...config[platform], is_live: live, title: live ? '示例直播 · Live preview' : '-', topic: 'Gaming', game: 'Just Chatting', area_id: 235, area_name: '其他单机', crop_enabled: false, ffmpeg_cache_enabled: platform === 'youtube', ffmpeg_cache_latency_secs: 8 });
  const events = new Set();
  const writes = [];
  // Synthetic managed membership. Identities are placeholders, not keys.
  const memberId = n => String(n).padStart(64, '0');
  const fingerprint = id => id.slice(0, 16).match(/.{4}/g).join(':');
  const membership = { lifecycle: 'standalone', local_revision: 0, revision: 0, cluster_id: null, digest: null, members: [], public_member_id: null, local_member_id: null, next: 2 };
  const operations = new Map();
  let membershipMode = 'ok', operationPolls = 0;
  const TARGET_PASSWORD = 'synthetic-target-password';
  const projectCluster = () => {
    const local = membership.members.find(m => m.member_id === membership.local_member_id);
    const managed = membership.lifecycle === 'managed' && local;
    Object.assign(config.cluster, managed
      ? { enabled: true, node_id: local.node_id, node_name: local.name, public_api_url: local.api_url, priority: local.priority, peers: membership.members.filter(m => m !== local).map(({ node_id, name, api_url, priority }) => ({ node_id, name, api_url, priority })) }
      : { enabled: false, peers: [] });
    config.cluster.public_status.node_id = membership.members.find(m => m.member_id === membership.public_member_id)?.node_id || (managed ? '' : config.cluster.public_status.node_id);
    if (membership.lifecycle === 'left') config.cluster.public_status.node_id = '';
    for (const event of events) event.write('event: config\ndata: changed\n\n');
  };
  const operationStatus = op => op.status;
  const membershipView = () => ({
    protocol: 1, lifecycle: membership.lifecycle, local_revision: membership.local_revision, password_required: !!panelPassword,
    local: { member_id: membership.local_member_id, node_id: config.cluster.node_id, name: config.cluster.node_name, api_url: config.cluster.public_api_url, priority: config.cluster.priority },
    cluster_id: membership.lifecycle === 'managed' ? membership.cluster_id : null,
    revision: membership.lifecycle === 'managed' ? membership.revision : 0,
    digest: membership.lifecycle === 'managed' ? membership.digest : null,
    members: membership.lifecycle === 'managed' ? membership.members.map(m => ({ ...m, fingerprint: fingerprint(m.member_id) })) : [],
    public_member_id: membership.lifecycle === 'managed' ? membership.public_member_id : null,
    operation: [...operations.values()].map(operationStatus).find(s => !s.terminal || s.pending_node_ids.length) || null,
  });
  const commit = op => {
    const { change } = op;
    if (change.kind === 'add') {
      const n = membership.next++;
      membership.members.push({ member_id: memberId(n), node_id: `node-${String(n).padStart(3, '0')}`, name: `Example Node ${String(n).padStart(3, '0')}`, api_url: change.target_url, priority: 0 });
    } else if (change.kind === 'remove') {
      membership.members = membership.members.filter(m => m.member_id !== change.target_member_id);
      if (membership.public_member_id === change.target_member_id) membership.public_member_id = change.replacement_public_member_id ?? null;
      if (change.target_member_id === membership.local_member_id) { membership.lifecycle = 'left'; membership.local_revision++; }
    } else if (change.kind === 'update_node') {
      Object.assign(membership.members.find(m => m.member_id === change.target_member_id), { name: change.name, api_url: change.api_url, priority: change.priority });
    } else if (change.kind === 'set_public_node') membership.public_member_id = change.public_member_id;
    membership.revision++;
    membership.digest = `example-digest-${membership.revision}`;
    projectCluster();
  };
  // Each status read moves an operation one phase, like a slow real cluster.
  const advance = op => {
    const s = op.status;
    if (s.terminal || op.held) return;
    const others = op.base.filter(m => m.member_id !== membership.local_member_id).map(m => m.node_id);
    if (s.phase === 'preparing') Object.assign(s, { phase: 'committing', pending_node_ids: others.slice(0, 1) });
    else if (s.phase === 'committing') { commit(op); Object.assign(s, { phase: 'completed', terminal: true, retryable: false, pending_node_ids: [], message: null }); }
  };
  const newOperation = (body, extra = {}) => {
    const { operation_id, expected_revision, target_password, ...change } = body;
    const coordinator = membership.members.find(m => m.member_id === membership.local_member_id)?.node_id || 'local';
    const op = {
      change, base: structuredClone(membership.members), held: membershipMode === 'needs-attention', hiddenPolls: 0,
      status: { operation_id, kind: change.kind, phase: 'preparing', coordinator_node_id: coordinator, message: null, pending_node_ids: membership.members.map(m => m.node_id), terminal: false, retryable: true, ...extra },
    };
    if (op.held) Object.assign(op.status, { phase: 'needs_attention', message: '以下服务器尚未确认: node-002' , pending_node_ids: ['node-002'] });
    operations.set(operation_id, op);
    return op;
  };
  const server = createServer(async (req, res) => {
    const path = new URL(req.url, 'http://localhost').pathname;
    const send = (body, status = 200) => { res.writeHead(status, { 'Content-Type': 'application/json', 'Cache-Control': 'no-store' }); res.end(JSON.stringify(body)); };
    try {
      if (path === '/mock/favorites-mode') { favoriteMode = new URL(req.url, 'http://localhost').searchParams.get('mode') || 'ok'; return send(ok(null)); }
      if (path === '/mock/setup-mode') { needsSetup = true; return send(ok(null)); }
      if (path === '/mock/password-mode') {
        const mode = new URL(req.url, 'http://localhost').searchParams.get('mode');
        if (mode === 'missing') { panelPassword = null; remoteListener = false; panelGeneration++; }
        if (mode === 'configured') { panelPassword = 'synthetic-panel-password'; panelGeneration++; }
        if (mode === 'remote') remoteListener = true;
        if (mode === 'local') remoteListener = false;
        if (mode === 'expire') panelGeneration++;
        if (mode === 'lose-setup-response') setupResponseLoss = true;
        return send(ok(null));
      }
      if (path === '/mock/auth-stats') return send({ passwordAttempts, setupAttempts });
      if (path === '/mock/membership-mode') {
        membershipMode = new URL(req.url, 'http://localhost').searchParams.get('mode') || 'ok';
        if (membershipMode === 'restore-managed') {
          membership.lifecycle = 'managed';
          if (!membership.members.some(m => m.member_id === membership.local_member_id)) {
            membership.members.unshift({ member_id: membership.local_member_id, node_id: config.cluster.node_id, name: config.cluster.node_name, api_url: config.cluster.public_api_url, priority: config.cluster.priority });
          }
          membership.cluster_id ||= 'example-cluster';
          membership.revision ||= 1;
          membership.digest ||= 'example-digest-restored';
          projectCluster();
          membershipMode = 'ok';
        }
        return send(ok(null));
      }
      if (path === '/mock/membership-stats') return send({ membership: membershipView(), operations: [...operations.keys()], operationPolls });
      const authenticated = !panelPassword || req.headers.cookie?.split('; ').includes(`mock_panel=${panelGeneration}`);
      if (path === '/api/auth') return send({ required: !!panelPassword, authenticated: !!authenticated, can_create_password: !panelPassword && !remoteListener, can_clear_password: !!panelPassword && !remoteListener });
      if (path === '/api/auth/password') passwordAttempts++;
      if (path === '/api/setup/save-config') setupAttempts++;
      if (path.startsWith('/api/') && !['/api/login', '/api/logout'].includes(path) && !authenticated) return send({ success: false, message: '需要登录' }, 401);
      if (path === '/api/events') {
        res.writeHead(200, { 'Content-Type': 'text/event-stream', 'Cache-Control': 'no-store' });
        res.write(': mock connected\n\n'); events.add(res); req.on('close', () => events.delete(res)); return;
      }
      if (req.method === 'POST' || req.method === 'DELETE') {
        let text = ''; for await (const chunk of req) text += chunk;
        const patch = JSON.parse(text || '{}');
        if (path === '/api/login') {
          if (panelPassword && patch.password !== panelPassword) return send({ success: false, message: '密码错误' }, 403);
          res.setHeader('Set-Cookie', `mock_panel=${panelGeneration}; HttpOnly; SameSite=Strict; Path=/`);
          return send({ success: true });
        }
        if (path === '/api/logout') { panelGeneration++; return send({ success: true }); }
        if (path === '/api/setup/holodex-favorites') {
          if (favoriteMode === 'error') return send({ success: false, message: 'Holodex 凭据无效，请重新登录' });
          if (favoriteMode === 'slow') await new Promise(resolve => setTimeout(resolve, 400));
          return send(ok(favoriteMode === 'empty' ? [] : favorites));
        }
        const recorded = structuredClone(patch);
        if ('target_password' in recorded) recorded.target_password = recorded.target_password ? '<sent>' : '';
        writes.push({ path, patch: recorded });
        if (path === '/api/auth/password') {
          if (panelPassword && patch.current_password !== panelPassword) return send({ success: false, message: '当前密码错误' }, 403);
          if (patch.action === 'clear' && remoteListener) return send({ success: false, message: '远程监听不能清除密码' }, 403);
          panelPassword = patch.action === 'clear' ? null : patch.new_password;
          panelGeneration++;
          for (const event of events) event.end();
          return send({ success: true });
        }
        if (path === '/api/youtube/cookies') {
          if (patch.expected_revision !== cookieRevision) return send({ success: false, message: 'Cookie 已更新' }, 409);
          cookieCount = req.method === 'DELETE' ? 0 : (patch.content || '').split('\n').filter(line => line && !line.startsWith('#')).length;
          cookieRevision += 1;
          return send({ success: true, message: cookieCount ? 'Cookie 已加密保存' : 'Cookie 已清除', data: { configured: cookieCount > 0, count: cookieCount, revision: cookieRevision, updated_at: 1 } });
        }
        if (path === '/api/player-filter') { playerFilter = patch.content; filterRevision += 1; return send({ success: true, data: filterRevision, message: '过滤词已保存' }); }

        if (path === '/api/cluster/create' || path === '/api/cluster/prepare-join') {
          if (!panelPassword) return send({ success: false, message: '请先在 系统设置 → 安全 中设置面板密码' }, 409);
          if (patch.expected_local_revision !== membership.local_revision) return send({ success: false, message: '本地状态已改变，请刷新' }, 409);
          if (['managed', 'pairing'].includes(membership.lifecycle)) return send({ success: false, message: '已有成员操作，不能覆盖' }, 409);
          Object.assign(config.cluster, { node_id: patch.node_id, node_name: patch.name, public_api_url: patch.api_url, priority: patch.priority });
          membership.local_member_id = memberId(1);
          membership.local_revision++;
          if (path === '/api/cluster/create') {
            Object.assign(membership, { lifecycle: 'managed', cluster_id: patch.operation_id, revision: 1, digest: 'example-digest-1', public_member_id: null, members: [{ member_id: memberId(1), node_id: patch.node_id, name: patch.name, api_url: patch.api_url, priority: patch.priority }] });
          } else membership.lifecycle = 'join_ready';
          projectCluster();
          return send({ success: true, data: membershipView(), message: path === '/api/cluster/create' ? '已创建集群，本服务器是唯一成员' : '本服务器已准备加入，请在集群中任一服务器的面板添加它' });
        }
        if (path === '/api/cluster/membership/leave') {
          if (membership.lifecycle !== 'managed') return send({ success: false, message: '只有已加入集群的服务器可以这样退出' }, 409);
          if ([...operations.values()].some(op => !op.status.terminal || op.status.pending_node_ids.length)) return send({ success: false, message: '上一项成员操作尚未完成，不能退出集群' }, 409);
          const others = membership.members.filter(m => m.member_id !== membership.local_member_id);
          const name = others[0]?.name || others[0]?.node_id || 'Example Node 002';
          if (membershipMode === 'leave-unreachable') return send({ success: false, message: `服务器 ${name} 没有确认本节点已不在集群中` }, 409);
          if (membershipMode === 'leave-present') return send({ success: false, message: `服务器 ${name} 仍承认本节点，请使用「移除」` }, 409);
          if (membershipMode !== 'leave-absent') return send({ success: false, message: '退出集群尚未准备' }, 409);
          membership.lifecycle = 'left';
          membership.local_revision++;
          config.enable_youtube_monitor = false;
          config.enable_twitch_monitor = false;
          projectCluster();
          return send({ success: true, data: membershipView(), message: '本服务器已离开集群，监控已全部关闭' });
        }
        if (path === '/api/cluster/membership/operations') {
          if (membership.lifecycle !== 'managed') return send({ success: false, message: '本服务器尚未加入受管集群' }, 409);
          const existing = operations.get(patch.operation_id);
          if (existing) { advance(existing); return send({ success: true, data: existing.status, message: '操作已在进行，正在继续' }); }
          if (patch.expected_revision !== membership.revision) return send({ success: false, message: '成员版本已改变，请刷新后重试' }, 409);
          if (patch.kind === 'add' && patch.target_password !== TARGET_PASSWORD) {
            newOperation(patch, { phase: 'aborted', terminal: true, retryable: false, pending_node_ids: [], message: '目标服务器面板密码错误或未设置' });
            return send({ success: false, message: '目标服务器面板密码错误或未设置' }, 403);
          }
          const selfRemoval = patch.kind === 'remove' && patch.target_member_id === membership.local_member_id;
          const op = newOperation(patch);
          if (selfRemoval) op.hiddenPolls = 2;
          if (membershipMode === 'lose-response') {
            membershipMode = 'ok';
            res.writeHead(200, { 'Content-Type': 'application/json' });
            res.end('{"success":');
            return;
          }
          return send({ success: true, data: op.status, message: selfRemoval ? '已交由 node-002 协调移除本服务器' : '成员操作已开始' });
        }
        const retry = path.match(/^\/api\/cluster\/membership\/operations\/([^/]+)\/retry$/);
        if (retry) {
          const op = operations.get(retry[1]);
          if (!op) return send({ success: false, message: '操作尚未到达本服务器，请稍后刷新或在协调服务器查看' }, 404);
          if (op.held) { op.held = false; Object.assign(op.status, { phase: 'preparing', message: null }); }
          return send({ success: true, data: op.status });
        }
        if (path === '/api/cluster/public-status') {
          if (membership.lifecycle !== 'standalone' && patch.config.node_id !== config.cluster.public_status.node_id) return send({ success: false, message: '集群成员、节点身份和状态页节点只能通过成员操作更改，请使用多服务器设置中的成员操作' });
          config.cluster.public_status = patch.config;
          for (const event of events) event.write('event: config\ndata: changed\n\n');
          return send({ success: true, message: '公开页设置已保存', data: { enabled: false, nodes: [], local_node_id: 'local', public_status: patch.config } });
        }
        if (path === '/api/channels/resolve-youtube') return send(ok({ channel_id: 'UC4444444444444444444444' }));
        if (path === '/api/niconico/session/check') { await new Promise(resolve => setTimeout(resolve, 300)); return send(ok({ state: sessionSaved ? 'valid' : 'unconfigured', message: sessionSaved ? '会话仍被接受；本次检测不会续期' : '未配置 user_session' })); }
        if (path === '/api/setup/save-config') {
          if (patch.panel_password) {
            if (panelPassword) return send({ success: false, message: '密码已经配置' }, 409);
            panelPassword = patch.panel_password; panelGeneration++;
            res.setHeader('Set-Cookie', `mock_panel=${panelGeneration}; HttpOnly; SameSite=Strict; Path=/`);
            for (const event of events) event.end();
          }
          needsSetup = false;
          if (setupResponseLoss) {
            setupResponseLoss = false;
            // Deterministic incomplete response, without a cookie. Closing a
            // pooled socket can make Firefox retry below the application layer.
            res.removeHeader('Set-Cookie');
            res.writeHead(200, { 'Content-Type': 'application/json' });
            res.end('{"success":');
            return;
          }
          return send({ success: true });
        }
        if (path === '/api/config') {
          if (patch.expected_secret_revision != null && patch.expected_secret_revision !== config.secret_revision) return send({ success: false, message: '密钥配置已更新' }, 409);
          for (const key of ['holodex_api_key', 'youtube_api_key', 'riot_api_key']) {
            if (patch[`clear_${key}`]) { config[`${key}_configured`] = false; config[`${key}_mask`] = ''; }
            else if (patch[key]) { config[`${key}_configured`] = true; config[`${key}_mask`] = [...patch[key]].map(c => c === '\n' ? c : '•').join(''); }
          }
          for (const platform of ['youtube', 'twitch', 'niconico']) {
            if (patch[`clear_${platform}_proxy`]) { config[platform].proxy_configured = false; config[platform].proxy = ''; proxyValues[platform] = ''; }
            else if (patch[`${platform}_proxy`]) {
              let value = patch[`${platform}_proxy`];
              if (patch[`${platform}_proxy_keep_password`]) {
                const password = proxyValues[platform]?.match(/:([^/@]*)@/)?.[1];
                if (!password || !value.includes(':@')) return send({success:false,message:'代理密码无法保留'},400);
                value = value.replace(':@', ':' + password + '@');
              }
              proxyValues[platform] = value;
              config[platform].proxy_configured = true; config[platform].proxy = proxyMask(value);
            }
            delete patch[`${platform}_proxy`];
            delete patch[`${platform}_proxy_keep_password`];
          }
          config.secret_revision += 1;

          for (const [key, before] of Object.entries(patch.expected || {})) {
            if (key in config && JSON.stringify(config[key]) !== JSON.stringify(before)) return send({ success: false, message: '配置冲突' }, 409);
          }
          if (patch.cluster) {
            const c = config.cluster, r = patch.cluster;
            const identity = ['node_id', 'node_name', 'public_api_url', 'priority'].every(k => c[k] === r[k]) && c.public_status.node_id === r.public_status?.node_id;
            const localEdit = membership.lifecycle === 'standalone' && !c.enabled && !r.enabled;
            if (c.enabled !== r.enabled || JSON.stringify(c.peers) !== JSON.stringify(r.peers) || (!identity && !localEdit)) {
              return send({ success: false, message: '集群成员、节点身份和状态页节点只能通过成员操作更改，请使用多服务器设置中的成员操作' });
            }
          }
          const { expected, niconico_user_session, clear_niconico_user_session, niconico_session_check_enabled, ...changes } = patch;
          if (niconico_user_session) { sessionSaved = true; config.niconico.user_session_mask = '•'.repeat([...niconico_user_session].length); }
          if (clear_niconico_user_session) { sessionSaved = false; config.niconico.user_session_mask = ''; }
          config.niconico.user_session_configured = sessionSaved;
          if (niconico_session_check_enabled != null) config.niconico.session_check_enabled = niconico_session_check_enabled;
          for (const key of Object.keys(changes)) if (key.startsWith('clear_') || key==='expected_secret_revision') delete changes[key];
          Object.assign(config, changes); for (const key of ['holodex_api_key', 'youtube_api_key', 'riot_api_key']) config[key] = '';
          for (const event of events) event.write('event: config\ndata: changed\n\n');
        } else if (path === '/api/priority-channel') Object.assign(config.priority_channel, patch);
        else if (path !== '/api/banned-keywords') return send({ success: false, message: 'Unsupported mock action' }, 404);
        return send({ success: true, message: '配置已保存' });
      }
      if (path === '/mock/writes') return send(writes);
      if (path === '/api/storage') return send({ ready: true, configured: true, schema: 1, sqlite_version: '3.53.2' });
      if (path === '/api/youtube/cookies') return send(ok({ configured: cookieCount > 0, count: cookieCount, revision: cookieRevision, updated_at: 1 }));
      if (path === '/api/player-filter') return send(ok({ content: playerFilter, revision: filterRevision }));
      if (path === '/api/setup-status') return send({ needs_setup: needsSetup });
      if (path === '/api/setup/login-status') return send({ logged_in: true });
      if (path === '/api/niconico/session') return send(ok({ state: sessionSaved ? 'unchecked' : 'unconfigured', message: sessionSaved ? '尚未检测；检测不会延长会话有效期' : '未配置 user_session' }));
      if (path === '/api/areas/catalog') return send(ok(catalog));
      if (path === '/api/manage/areas') return send(ok({areas:[{id:235,name:'其他单机',aliases:[],title_keywords:[]}]}));
      if (path === '/api/manage/channels') return send(ok({channels:[]}));
      if (path === '/api/config') return send(config);
      if (path === '/api/version') return send(ok({ version: '0.7.0', is_tauri: false }));
      if (path === '/api/update/check') return send(ok({ has_update: false, current_version: '0.7.0', latest_version: '0.7.0' }));
      if (path === '/api/status') return send(ok({ bilibili: { ...network, is_live: true, title: '示例频道 001 | 示例直播', area_id: 235, area_name: '其他单机', stream_quality: '1080p60', enable_danmaku_command: true, live_start_ts: Math.floor(Date.now() / 1000) - 2470 }, youtube: channel('youtube', true), twitch: channel('twitch', false), niconico: null, priority_channel: { ...config.priority_channel, default_area_name: "其他单机", is_live: false, platform: null, title: null } }));
      if (path.startsWith('/api/refresh/')) return send(ok(null));
      if (path === '/api/network-status') return send(ok(network));
      if (path === '/api/logs') return send({ success: true, logs: '12:00:00 INFO 模拟数据 · Web UI preview\n12:00:01 INFO 转播运行中' });
      if (path === '/api/cluster/membership') return send(ok(membershipView()));
      const operation = path.match(/^\/api\/cluster\/membership\/operations\/([^/]+)$/);
      if (operation) {
        operationPolls++;
        const op = operations.get(operation[1]);
        if (!op || op.hiddenPolls-- > 0) return send({ success: false, message: '操作尚未到达本服务器，请稍后刷新或在协调服务器查看' }, 404);
        advance(op);
        return send(ok(op.status));
      }
      if (path === '/api/cluster/status') {
        if (membership.lifecycle !== 'managed') return send(ok({ enabled: false, nodes: [], local_can_enable_monitor_toggles: true }));
        const local = membership.members.find(m => m.member_id === membership.local_member_id);
        const active = [...membership.members].sort((a, b) => b.priority - a.priority)[0];
        return send(ok({ enabled: true, auto_failover: true, local_node_id: local?.node_id, active_owner: active?.node_id, config_version: 'example', public_status: config.cluster.public_status,
          nodes: membership.members.map(m => ({ node_id: m.node_id, name: m.name, api_url: m.api_url, is_local: m === local, role: m === active ? 'active' : 'standby', health: { healthy: true, stale: false }, last_seen: Math.floor(Date.now() / 1000), config_version: 'example', ffmpeg_running: false })) }));
      }
      if (path === '/api/banned-keywords') return send({ streaming_banned_keywords: [], danmaku_banned_keywords: [] });
      if (path === '/api/areas') return send({ areas: [{ id: 235, name: '其他单机' }, { id: 329, name: '无畏契约' }] });
      if (path === '/api/channels') return send({ channels: [{ name: '示例频道 001', platforms: { youtube: 'UC1111111111111111111111' } }, { name: '示例频道 002', platforms: { twitch: 'demo_games' } }, { name: '示例频道 003', platforms: { youtube: 'UC3333333333333333333333' } }] });
      if (path === '/api/youtube/keys') return send(ok({ configured: true, budget_per_key: 9000, remaining_fraction: .78, resets_at: '2026-09-30T07:00:00Z', keys: [{ fingerprint: 'demo…a', used: 2480, state: 'usable' }, { fingerprint: 'demo…b', used: 1480, state: 'usable' }], playlist: { on: true, interval_secs: 180, rss_enabled: config.youtube_rss_enabled, rss_down: false, stretch: 1, websub_slowed: true }, websub: { verified: 24, pending: 0, failed: 0, healthy: true, listening: 3151, last_push: '2026-09-29T04:00:00Z' } }));
      if (path === '/api/holodex/streams') return send({ success: true, source: 'channels', data: [
        { id: 'demoLive', title: '示例直播', channel_name: '示例频道 001', channel_id: 'UC1111111111111111111111', status: 'live', start_actual: new Date(Date.now()-2470000).toISOString(), suggested_area_id: 235, suggested_area_name: '其他单机', thumbnail: '/mock-thumbnail.svg', channel_photo: '/icon-blue.png', live_viewers: 1280 },
        { id: 'demoNext', title: '示例直播预告', channel_name: '示例频道 003', channel_id: 'UC3333333333333333333333', status: 'upcoming', start_scheduled: new Date(Date.now()+3600000).toISOString(), suggested_area_id: 235, suggested_area_name: '其他单机', thumbnail: '/mock-thumbnail.svg', channel_photo: '/icon-blue.png' },
      ] });
      if (path === '/mock-thumbnail.svg') {
        res.writeHead(200, { 'Content-Type': 'image/svg+xml' });
        return res.end('<svg xmlns="http://www.w3.org/2000/svg" width="640" height="360"><defs><linearGradient id="g"><stop stop-color="#82afa5"/><stop offset="1" stop-color="#d9c7ad"/></linearGradient></defs><rect width="640" height="360" fill="url(#g)"/><circle cx="480" cy="60" r="140" fill="#ffffff" opacity=".16"/><text x="50" y="170" font-size="48" font-family="sans-serif" fill="#fff">EXAMPLE CHANNEL</text><text x="52" y="216" font-size="20" font-family="sans-serif" fill="#fff">BILISTREAM · LIVE PREVIEW</text></svg>');
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
