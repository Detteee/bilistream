import test from 'node:test';
import assert from 'node:assert/strict';

globalThis.window = {};
globalThis.document = { addEventListener() {} };
const { parseLogLine } = await import('../src/js/logs.js');
const { LOG_SHAPES } = await import('../mock/capture.mjs');

// Log wording ratchet (owner-requested): the mock capture fixtures and the
// wording that Rust emits must stay aligned, so any Rust wording regression
// that reintroduces `会话`/`s=N` numbering, ASCII parens, `，` chains or `=`
// prose fails here even though only the browser suites render records live.

const prefix = i => `2026-10-21 18:00:${String(i).padStart(2, '0')}+0800 `;

test('the capture fixture exports the owner shape list', () => {
  assert.equal(Array.isArray(LOG_SHAPES), true);
  assert.equal(LOG_SHAPES.length >= 5, true);
  for (const shape of LOG_SHAPES) {
    assert.equal(typeof shape, 'string');
    assert.equal(/^INFO \[/.test(shape), true, shape);
  }
});

test('every shape keeps fused `_N` numbering and no banned prose marks', () => {
  LOG_SHAPES.forEach((shape, i) => {
    const parsed = parseLogLine(`${prefix(i)}${shape}`);
    const line = parsed.message;
    // (i) zero ASCII parens — full-width `（）` are the sanctioned bracket.
    assert.equal(line.includes('('), false, shape);
    assert.equal(line.includes(')'), false, shape);
    // (ii) zero `，` clause chains — reasons follow the first `：`/`；`.
    assert.equal(line.includes('，'), false, shape);
    // (iii) zero `=` — key=value prose reads like a raw field dump.
    assert.equal(line.includes('='), false, shape);
    // ASCII-space tokens also never carry the banned 会话/s=1/#N numbering.
    for (const banned of ['会话', 's=', '#']) {
      assert.equal(line.includes(banned), false, shape);
    }
  });
});

test('shapes 0 and 3 fuse the session number into the first token', () => {
  const first = parseLogLine(`${prefix(0)}${LOG_SHAPES[0]}`);
  assert.equal(
    first.messageHtml,
    '示例频道001<span class="log-sub">1</span> 推流 已观察到推流输出',
  );
  const second = parseLogLine(`${prefix(3)}${LOG_SHAPES[3]}`);
  assert.equal(second.messageHtml.includes('示例频道002<span class="log-sub">2</span>'), true);
  assert.equal(second.messageHtml.includes('<span class="log-detail">'), true);
});

test('the danmaku shape renders exactly two inline platform chips', () => {
  const parsed = parseLogLine(`${prefix(1)}${LOG_SHAPES[1]}`);
  const chips = [...parsed.messageHtml.matchAll(/<span class="log-tag">(YT|TW|NC)<\/span>/g)].map(m => m[1]);
  assert.deepEqual(chips, ['YT', 'TW']);
});
