import test from 'node:test';
import assert from 'node:assert/strict';

// state.js initializes the browser config object; severity itself is pure.
globalThis.window = {};
globalThis.document = { addEventListener() {} };
const { logLineLevel } = await import('../src/js/logs.js');

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
