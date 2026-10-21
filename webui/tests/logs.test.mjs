import test from 'node:test';
import assert from 'node:assert/strict';

// state.js initializes the browser config object; severity itself is pure.
globalThis.window = {};
globalThis.document = { addEventListener() {} };
const { logLineLevel, parseLogLine } = await import('../src/js/logs.js');

test('log severity comes from the real prefix before title words or emoji', () => {
  for (const prefix of [
    '2026-10-10 18:12:03+0800 ',
    '2026-10-11 00:00:00-0400 ',
    '2026-10-10T18:12:03+08:00 ',
    '18:12:03 ',
    '18:12:03.123 ',
    '',
  ]) {
    assert.equal(logLineLevel(`${prefix}INFO [runtime] 示例频道001 ERROR WARN ❌ ⚠️`), 'info');
    assert.equal(logLineLevel(`${prefix}DEBUG [websub] 示例频道001 ERROR ❌`), 'debug');
    assert.equal(logLineLevel(`${prefix}WARN [rss] 探测失败 INFO ✅`), 'warn');
    assert.equal(logLineLevel(`${prefix}ERROR [ffmpeg] Broken pipe INFO ✅`), 'error');
    assert.equal(logLineLevel(`${prefix}TRACE [transport] ERROR details`), 'trace');
  }
});

test('unprefixed legacy fallback only recognizes a leading severity emoji', () => {
  assert.equal(logLineLevel('⚠️ 连接暂时中断'), 'warn');
  assert.equal(logLineLevel('❌ 进程启动失败'), 'error');
  assert.equal(logLineLevel('✅ 配置已更新'), 'info');
  assert.equal(logLineLevel('18:12:03 🚀 服务已启动'), 'info');
  assert.equal(logLineLevel('🔄 后台检查'), 'debug');
  assert.equal(logLineLevel('示例频道001 的 ERROR 片段 ⚠️'), '');
  assert.equal(logLineLevel('INFORMATION 不是日志级别'), '');
  assert.equal(logLineLevel(''), '');
});

test('a record splits into the table columns and keeps the full timestamp', () => {
  assert.deepEqual(
    parseLogLine('2026-10-21 15:43:08+0800 INFO [webui] [jp] [YT] 胡桃のあ 监听目标配置已更新；area=APEX英雄 (240)'),
    {
      time: '15:43:08',
      fullTime: '2026-10-21 15:43:08+0800',
      level: 'info',
      component: 'webui',
      node: 'jp',
      platform: 'YT',
      label: '',
      message: '胡桃のあ 监听目标配置已更新；area=APEX英雄 (240)',
    },
  );
});

test('a danmaku command tag stays with the identity, not the message body', () => {
  const parsed = parseLogLine(
    '2026-10-21 07:41:09+0800 INFO [danmaku] [jp] [弹幕 转播] 切台请求 [YT] 紫宮るな；实际转播 [TW] 示例频道002；原因：本轮排序更高',
  );
  assert.equal(parsed.node, 'jp');
  assert.equal(parsed.platform, '');
  assert.equal(parsed.label, '弹幕 转播');
  assert.equal(parsed.message.startsWith('切台请求 [YT]'), true);
});

test('a platform-first record does not mistake the platform for a node', () => {
  const parsed = parseLogLine('2026-10-21 10:10:00+0800 WARN [runtime] [YT] 胡桃のあ 状态检查失败；超时');
  assert.equal(parsed.node, '');
  assert.equal(parsed.platform, 'YT');
  assert.equal(parsed.message, '胡桃のあ 状态检查失败；超时');
});

test('unparsed and UI-only lines keep their full text in the message cell', () => {
  assert.equal(parseLogLine('日志已清空').message, '日志已清空');
  assert.equal(parseLogLine('⚠️ 连接暂时中断').message, '⚠️ 连接暂时中断');
  const bare = parseLogLine('2026-10-21 10:10:00+0800 ERROR');
  assert.equal(bare.level, 'error');
  assert.equal(bare.component, '');
  assert.equal(bare.message, '');
});
