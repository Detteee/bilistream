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
    parseLogLine('2026-10-21 15:43:08+0800 INFO [ffmpeg] [jp] [YT] 示例频道001_2 推流 已观察到推流输出'),
    {
      time: '15:43:08',
      fullTime: '2026-10-21 15:43:08+0800',
      level: 'info',
      component: 'ffmpeg',
      node: 'jp',
      platform: 'YT',
      label: '',
      message: '示例频道001_2 推流 已观察到推流输出',
      messageHtml: '示例频道001<span class="log-sub">2</span> 推流 已观察到推流输出',
    },
  );
});

test('a danmaku command tag stays with the identity, not the message body', () => {
  const parsed = parseLogLine(
    '2026-10-21 07:41:09+0800 INFO [danmaku] [jp] [弹幕 转播] 切台请求 [YT] 示例频道001；实际转播 [TW] 示例频道002；原因：本轮排序更高',
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

test('the message cell splits into tone and muted detail at the first separator', () => {
  // Owner wording (m03353/m03490): no `，` chains, ASCII parens or `=` in the
  // prose. The tail from the first `：`/`；` reads muted, so the outcome stays
  // primary while reasons and diagnostics keep their full verbatim text.
  const parsed = parseLogLine(
    '2026-10-21 18:09:34+0800 INFO [ffmpeg] [jp] [TW] 示例频道002_2 转播：已合并 1 条重复诊断；ERROR new decoder failure',
  );
  assert.equal(parsed.message, '示例频道002_2 转播：已合并 1 条重复诊断；ERROR new decoder failure');
  assert.equal(
    parsed.messageHtml,
    '示例频道002<span class="log-sub">2</span> 转播<span class="log-detail">：已合并 1 条重复诊断；ERROR new decoder failure</span>',
  );
  assert.equal(parsed.messageHtml.includes('<span class="log-detail">：'), true);
  // Only the message's FIRST whitespace token fuses a `_digits` session number.
  assert.equal(parsed.messageHtml.includes('<span class="log-sub">2</span>'), true);
});

test('inline platform codes become chips without fusing any session digits', () => {
  const parsed = parseLogLine(
    '2026-10-21 18:09:41+0800 INFO [danmaku] [jp] [弹幕 转播] 切台请求 [YT] 示例频道001；实际转播 [TW] 示例频道002；原因：本轮排序更高',
  );
  const chips = [...parsed.messageHtml.matchAll(/<span class="log-tag">([A-Z]{2})<\/span>/g)].map(m => m[1]);
  assert.deepEqual(chips, ['YT', 'TW']);
  assert.equal(parsed.messageHtml.includes('log-sub'), false);
  assert.equal(
    parsed.messageHtml.startsWith('切台请求 <span class="log-tag">YT</span> 示例频道001'),
    true,
  );
  assert.equal(parsed.messageHtml.includes('<span class="log-detail">；实际转播'), true);
});

test('unparsed and UI-only lines keep their full text in the message cell', () => {
  assert.equal(parseLogLine('日志已清空').message, '日志已清空');
  assert.equal(parseLogLine('⚠️ 连接暂时中断').message, '⚠️ 连接暂时中断');
  // All span wrapping happens after the text is escaped, so it stays XML-safe.
  assert.equal(parseLogLine('示例 & 合作 <脚本>').messageHtml, '示例 &amp; 合作 &lt;脚本&gt;');
  const bare = parseLogLine('2026-10-21 10:10:00+0800 ERROR');
  assert.equal(bare.level, 'error');
  assert.equal(bare.component, '');
  assert.equal(bare.message, '');
  assert.equal(bare.messageHtml, '');
});
