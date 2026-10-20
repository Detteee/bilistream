// Forward-fix refusal copy (Designer spec rev 6, backend Scout 77fb9aed with
// the 460e59a1 status/outcome table). `outcome` decides the ending of every
// forward-failure code; only outcome === 'not_sent' may say 未保存任何更改 and
// get the AppSecret line; holder_bad_reply always takes the unknown ending.
import test from 'node:test';
import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import {
  notifyRefusal, liveRefusal, secretLostRefusal, revealRefusal, refusalNodeName, nodeAtStart, nodeInline, bodyCarriesCredentials, saveBody, clearSecretBody,
  HOLDER_FALLBACK_NAME, SECRET_NOT_SAVED, HOLDER_DOWN_TEXT, OUTCOME_UNKNOWN_TEXT,
} from '../src/js/notify-state.js';
import { createNotifyState } from '../mock/notify-fixtures.mjs';

// Synthetic ids/names: the id must never show up in any rendered line.
const HOLDER_ID = 'node-7f3a-holder';
const cluster = {
  enabled: true, node_id: 'node-local-01', node_name: '纽约节点',
  peers: [{ node_id: HOLDER_ID, name: 'jp', api_url: 'https://synthetic-holder.invalid' }, { node_id: 'node-ca-02', name: '加州节点' }],
};
const err = (status, data) => ({ status, message: `server message（节点 ${data?.node ?? ''}）`, body: { success: false, message: `server message（节点 ${data?.node ?? ''}）`, data } });
const lines = r => [r.banner.lead, r.banner.text, r.banner.note || '', r.banner.action?.label || ''].join('\n');
const REFRESH = { label: '刷新', target: 'refresh' };
const UNKNOWN = '请刷新查看当前设置，确认后再决定是否重新保存。';
const VERSION = '通常是节点版本不一致，请确认所有节点已更新到同一版本后重试（状态 401）。';

// Statuses as Scout's 460e59a1 table sends them (the mapping ignores them).
// code, status, tone, clause(name), not_sent kind, unknown kind, not_sent hint(name) | null (never not-sent), unknown hint(name), not_sent 刷新
const S = n => nodeAtStart(n), I = n => nodeInline(n);
const FORWARD = [
  ['credentials_holder_unreachable', 503, 'warn', n => `${S(n)}暂时无法连接`, 'holder_down', 'holder_unknown', () => '请稍后重试。', () => '', false],
  ['credentials_membership_changing', 409, 'warn', () => '节点列表正在变更', 'membership_changing', 'membership_changing', () => '请稍后刷新后重试。', () => '', true],
  ['credentials_holder_refused', 409, 'danger', n => `${S(n)}拒绝了这次保存`, 'holder_refused', 'holder_refused', () => VERSION, () => VERSION, false],
  ['credentials_holder_bad_reply', 422, 'warn', n => `${S(n)}的回复无法识别`, null, 'holder_bad_reply', null, () => '', false],
  ['credentials_forward_refused', 409, 'danger', n => `本节点无法向${I(n)}转发保存`, 'forward_refused', 'forward_refused', n => `请检查本节点与${I(n)}的集群配置后重试。`, n => `请检查本节点与${I(n)}的集群配置后重试。`, false],
  ['credentials_store', 500, 'danger', () => '发送节点写入失败', 'holder_store', 'holder_store', () => '请稍后重试；如持续出现，请查看发送节点日志。', () => '', false],
];
// Missing, unknown and anything that is not exactly 'not_sent' are all "unknown".
const UNKNOWN_OUTCOMES = [['missing'], ['unknown', 'unknown'], ['junk', 'maybe'], ['case', 'NOT_SENT'], ['padded', ' not_sent'], ['null', null], ['bool', true], ['number', 0], ['object', { outcome: 'not_sent' }]];
const withOutcome = (data, entry) => (entry.length > 1 ? { ...data, outcome: entry[1] } : data);

function assertUnknown(r, [code, , tone, clause, , unknownKind, , unknownHint], at) {
  assert.equal(r.kind, unknownKind, at);
  assert.equal(r.outcome, 'unknown', at);
  assert.equal(r.banner.tone, tone, `${at}: tone does not depend on outcome`);
  assert.equal(r.banner.lead, `${clause('jp')}，保存结果未知。`, at);
  assert.equal(r.banner.text, `${UNKNOWN}${unknownHint('jp')}`, at);
  assert.deepEqual(r.banner.action, REFRESH, at);
  assert.ok(!lines(r).includes('未保存任何更改'), `${at}: never claims nothing was saved`);
  if (code === 'credentials_store') assert.ok(!lines(r).includes('请稍后重试'), 'store: retry hint is not-sent only');
  const typed = secretLostRefusal(r, true);
  assert.equal(typed, r, `${at}: no AppSecret line`);
  assert.ok(!typed.banner.note && !typed.focus, at);
}

test('rev 6 matrix: every forward code × not_sent / unknown / missing / junk', () => {
  assert.equal(OUTCOME_UNKNOWN_TEXT, UNKNOWN);
  for (const row of FORWARD) {
    const [code, status, tone, clause, sentKind, , sentHint, , sentRefresh] = row;
    const data = { code, node: HOLDER_ID, holder_status: 401 };
    const sent = notifyRefusal(err(status, { ...data, outcome: 'not_sent' }), null, { route: 'config', cluster });
    if (sentHint === null) {
      assertUnknown(sent, row, `${code} not_sent`);
    } else {
      assert.equal(sent.kind, sentKind, code);
      assert.equal(sent.outcome, 'not_sent', code);
      assert.equal(sent.banner.tone, tone, code);
      assert.equal(sent.banner.lead, `${clause('jp')}，未保存任何更改。`, code);
      assert.equal(sent.banner.text, sentHint('jp'), code);
      assert.deepEqual(sent.banner.action ?? null, sentRefresh ? REFRESH : null, code);
      const typedSent = secretLostRefusal(sent, true);
      assert.equal(typedSent.banner.note, SECRET_NOT_SAVED, `${code} not_sent: AppSecret line`);
      assert.equal(typedSent.focus, 'notify-app-secret', code);
      assert.equal(secretLostRefusal(sent, false), sent, code);
    }
    for (const entry of UNKNOWN_OUTCOMES) {
      assertUnknown(notifyRefusal(err(status, withOutcome(data, entry)), null, { route: 'config', cluster }), row, `${code} outcome ${entry[0]}`);
    }
  }
});

test('rev 5: holder_bad_reply with outcome not_sent still gets the unknown ending, no AppSecret line', () => {
  const r = notifyRefusal(err(422, { code: 'credentials_holder_bad_reply', node: HOLDER_ID, outcome: 'not_sent' }), null, { route: 'config', cluster });
  assert.equal(r.kind, 'holder_bad_reply');
  assert.equal(r.banner.tone, 'warn');
  assert.equal(r.banner.lead, 'jp 的回复无法识别，保存结果未知。');
  assert.equal(r.banner.text, UNKNOWN);
  assert.deepEqual(r.banner.action, REFRESH);
  const typed = secretLostRefusal(r, true);
  assert.equal(typed, r);
  assert.ok(!typed.banner.note && !typed.focus);
});

test('rev 3 spacing: a real name has a space on each side (none at sentence start); 发送节点 has none', () => {
  const real = { node: HOLDER_ID };
  const fallback = { node: 'node-unknown-99' };
  const r = (code, status, data, outcome) => notifyRefusal(err(status, { code, ...data, outcome }), null, { route: 'config', cluster }).banner;
  assert.equal(r('credentials_holder_unreachable', 503, real, 'not_sent').lead, 'jp 暂时无法连接，未保存任何更改。');
  assert.equal(r('credentials_holder_unreachable', 503, fallback, 'not_sent').lead, '发送节点暂时无法连接，未保存任何更改。');
  assert.equal(r('credentials_holder_unreachable', 503, real).lead, 'jp 暂时无法连接，保存结果未知。');
  assert.equal(r('credentials_holder_unreachable', 503, fallback).lead, '发送节点暂时无法连接，保存结果未知。');
  assert.equal(r('credentials_holder_refused', 409, real, 'not_sent').lead, 'jp 拒绝了这次保存，未保存任何更改。');
  assert.equal(r('credentials_holder_refused', 409, fallback, 'not_sent').lead, '发送节点拒绝了这次保存，未保存任何更改。');
  assert.equal(r('credentials_holder_bad_reply', 422, real).lead, 'jp 的回复无法识别，保存结果未知。');
  assert.equal(r('credentials_holder_bad_reply', 422, fallback).lead, '发送节点的回复无法识别，保存结果未知。');
  const fwd = r('credentials_forward_refused', 409, real, 'not_sent');
  assert.equal(fwd.lead, '本节点无法向 jp 转发保存，未保存任何更改。');
  assert.equal(fwd.text, '请检查本节点与 jp 的集群配置后重试。');
  const fwdFallback = r('credentials_forward_refused', 409, fallback, 'not_sent');
  assert.equal(fwdFallback.lead, '本节点无法向发送节点转发保存，未保存任何更改。');
  assert.equal(fwdFallback.text, '请检查本节点与发送节点的集群配置后重试。');
  assert.equal(r('credentials_forward_refused', 409, { node: 'node-ca-02' }, 'not_sent').lead, '本节点无法向 加州节点 转发保存，未保存任何更改。');
  for (const code of FORWARD.map(f => f[0])) {
    for (const data of [real, fallback]) {
      for (const outcome of ['not_sent', undefined]) {
        const b = r(code, 409, { ...data, holder_status: 401 }, outcome);
        for (const line of [b.lead, b.text]) {
          assert.ok(!/^\s/.test(line) && !/\s$/.test(line), `${code}: no leading/trailing space`);
          assert.ok(!/\s{2}/.test(line), `${code}: no double space`);
          assert.ok(!/发送节点 |\s发送节点/.test(line), `${code}: fallback is never spaced`);
        }
      }
    }
  }
});

test('{node} falls back to 发送节点 for an unknown id, a missing node, a blank name or no node list', () => {
  assert.equal(HOLDER_FALLBACK_NAME, '发送节点');
  const blank = { ...cluster, peers: [{ node_id: HOLDER_ID, name: '  ' }] };
  for (const [data, list] of [
    [{ node: 'node-unknown-99' }, cluster], [{}, cluster], [{ node: 42 }, cluster],
    [{ node: HOLDER_ID }, blank], [{ node: HOLDER_ID }, null], [{ node: HOLDER_ID }, {}],
  ]) {
    assert.equal(refusalNodeName(data, list), '', 'unresolved');
    assert.equal(nodeAtStart(refusalNodeName(data, list)), '发送节点');
  }
  assert.equal(refusalNodeName({ node: 'node-local-01' }, cluster), '纽约节点', 'local node resolves too');
  assert.equal(refusalNodeName({ node: HOLDER_ID }, cluster), 'jp');
  assert.equal(notifyRefusal(err(503, { code: 'credentials_holder_unreachable', node: HOLDER_ID, outcome: 'not_sent' }), null, { route: 'config' }).banner.lead, '发送节点暂时无法连接，未保存任何更改。');
});

test('the node id, its address and the server message never appear in the rendered text', () => {
  for (const [code, status] of FORWARD) {
    for (const list of [cluster, null]) {
      for (const outcome of ['not_sent', 'unknown', undefined]) {
        const data = { code, node: HOLDER_ID, holder_status: 401, outcome };
        const r = secretLostRefusal(notifyRefusal(err(status, data), null, { route: 'config', cluster: list }), true);
        const out = lines(r);
        assert.ok(!out.includes(HOLDER_ID) && !out.includes('7f3a'), `${code}: id rendered`);
        assert.ok(!out.includes('synthetic-holder.invalid'), `${code}: address rendered`);
        assert.ok(!out.includes('server message'), `${code}: server message rendered`);
        if (code !== 'credentials_holder_refused') assert.ok(!out.includes('401'), `${code}: holder_status outside its one place`);
      }
    }
  }
  const live = liveRefusal(err(422, { code: 'credentials_holder_bad_reply', node: HOLDER_ID }), 'config', null, true, { cluster });
  assert.equal(live.banner.lead, 'jp 的回复无法识别，保存结果未知。');
});

test('rev 6 holder_status: version hint only for 4xx, 「（状态 N）」 alone for 5xx, nothing otherwise', () => {
  for (const outcome of ['not_sent', 'unknown']) {
    const prefix = outcome === 'not_sent' ? '' : UNKNOWN;
    const text = v => notifyRefusal(err(409, { code: 'credentials_holder_refused', node: HOLDER_ID, outcome, ...(v === undefined ? {} : { holder_status: v }) }), null, { route: 'config', cluster }).banner.text;
    for (const v of [400, 401, 403, 413, 499]) assert.equal(text(v), `${prefix}通常是节点版本不一致，请确认所有节点已更新到同一版本后重试（状态 ${v}）。`, `${v} ${outcome}`);
    for (const v of [500, 503, 599]) {
      assert.equal(text(v), `${prefix}（状态 ${v}）`, `${v} ${outcome}`);
      assert.ok(!text(v).includes('版本'), `${v}: no version hint`);
    }
    for (const v of [undefined, null, '401', '503', 401.5, 99, 100, 200, 302, 399, 600, -401, NaN, Infinity, true, { s: 401 }, [401], 'x<b>']) {
      assert.equal(text(v), prefix, `holder_status ${String(v)} (${outcome}): no hint`);
    }
  }
});

test('mapping is by code only: forward codes render the same banner at 409, 422, 502 and any other status', () => {
  for (const [code] of FORWARD) {
    for (const outcome of ['not_sent', 'unknown', undefined]) {
      const data = { code, node: HOLDER_ID, holder_status: 401, outcome };
      const base = notifyRefusal(err(409, data), null, { route: 'config', cluster });
      for (const status of [422, 502, 400, 500, 503, undefined]) {
        const other = notifyRefusal(err(status, data), null, { route: 'config', cluster });
        assert.equal(other.kind, base.kind, `${code} at ${status}`);
        assert.deepEqual(other.banner, base.banner, `${code} at ${status}`);
      }
      assert.deepEqual(notifyRefusal(err(422, data), null, { route: 'openid', cluster }).banner, base.banner, code);
    }
  }
});

test('rev 6 credentials_invalid: 凭据无效 when the save carried AppID/AppSecret, 保存内容未通过校验 when it did not', () => {
  const bad = notifyRefusal(err(400, { code: 'bad_credentials' }), null, { route: 'config', cluster });
  for (const outcome of ['not_sent', undefined]) {
    const e = err(400, { code: 'credentials_invalid', outcome });
    for (const opts of [{ credentialsSent: true }, {}]) {
      const r = notifyRefusal(e, null, { route: 'config', cluster, ...opts });
      assert.equal(r.kind, 'bad_credentials');
      assert.deepEqual(r.banner, bad.banner);
      assert.equal(r.banner.lead, '凭据无效。');
      assert.deepEqual(r.banner.action, { label: '重新填写', target: 'notify-app-secret' });
    }
    const plain = notifyRefusal(e, null, { route: 'config', cluster, credentialsSent: false });
    assert.equal(plain.kind, 'invalid_save');
    assert.deepEqual(plain.banner, { lead: '保存内容未通过校验，未保存任何更改。', text: '请刷新后重新编辑。', tone: 'danger' });
    assert.ok(!/AppID|AppSecret/.test(lines(plain)), 'never points at the credentials');
    assert.equal(secretLostRefusal(plain, true), plain, 'no AppSecret line');
    // Re-render after a status poll keeps the same choice.
    assert.equal(liveRefusal(e, 'config', null, true, { cluster, credentialsSent: false }).kind, 'invalid_save');
    assert.equal(liveRefusal(e, 'config', null, true, { cluster }).kind, 'bad_credentials');
  }
  // bad_credentials (the worker's code) is unchanged even for a plain save.
  assert.equal(notifyRefusal(err(400, { code: 'bad_credentials' }), null, { route: 'config', credentialsSent: false }).kind, 'bad_credentials');
});

test('bodyCarriesCredentials: typed AppSecret, an AppID key or 清除 AppSecret; watch or settings only is false', () => {
  const settings = { revision: 3, credentials_generation: 7, secret_revision: 2, openid_revision: 1, watch_revision: 4 };
  assert.equal(bodyCarriesCredentials(saveBody(settings, { watch: { channels: [] } })), false, 'watch only');
  assert.equal(bodyCarriesCredentials(saveBody(settings, { enabled: false })), false, 'settings only');
  assert.equal(bodyCarriesCredentials(saveBody(settings, { qq: { sandbox: true } })), false, 'qq without app_id');
  assert.equal(bodyCarriesCredentials(saveBody(settings, { enabled: true }, { openid: 'synthetic-openid' })), false, 'openid only');
  assert.equal(bodyCarriesCredentials(saveBody(settings, { qq: { app_id: '1020' } })), true, 'AppID');
  assert.equal(bodyCarriesCredentials(saveBody(settings, { qq: { app_id: '' } })), true, 'blank AppID is still an AppID change');
  assert.equal(bodyCarriesCredentials(saveBody(settings, { watch: { channels: [] } }, { secret: 'synthetic-secret' })), true, 'AppSecret');
  assert.equal(bodyCarriesCredentials(clearSecretBody(settings)), true, '清除 AppSecret');
  for (const v of [null, undefined, 'x', 1, {}]) assert.equal(bodyCarriesCredentials(v), false);
});

test('credentials_invalid and unchanged conflict codes; the uncoded 503 keeps its copy', () => {
  for (const [code, status] of [['credentials_conflict', 409], ['watch_conflict', 409], ['credentials_holder_moved', 409], ['base_watch_revision_required', 400]]) {
    const r = notifyRefusal(err(status, { code, current_generation: 9, current_revision: 9, outcome: 'not_sent' }), null, { route: 'config', cluster });
    assert.equal(r.kind, 'credentials_conflict', code);
    assert.equal(r.banner.lead, '配置已在其他节点更新，请刷新后重试。', code);
    assert.deepEqual(r.banner.action, REFRESH, code);
  }
  const uncoded = notifyRefusal({ status: 503, body: {} }, null, { route: 'config', cluster });
  assert.equal(uncoded.kind, 'holder_down');
  assert.equal(uncoded.banner.lead, `${HOLDER_DOWN_TEXT}。`);
  assert.equal(uncoded.banner.lead, '发送节点暂时无法连接，未保存任何更改，请稍后重试。');
  assert.equal(uncoded.banner.text, '');
  assert.equal(notifyRefusal({ status: 503, body: {} }, null, { route: 'openid', cluster }), null);
  assert.equal(notifyRefusal(err(409, { code: 'credentials_something_new', node: HOLDER_ID }), null, { route: 'config', cluster }), null);
});

test('panel wiring: node list and credentialsSent reach both refusal paths; no innerHTML', async () => {
  const js = await readFile(new URL('../src/js/notify.js', import.meta.url), 'utf8');
  assert.match(js, /notifyRefusal\(error, status, \{ route, cluster: clusterNodes\(\), credentialsSent \}\)/);
  assert.match(js, /liveRefusal\(refusalSrc\.error, refusalSrc\.route, status, statusSeq > refusalSrc\.seq, \{ cluster: clusterNodes\(\), credentialsSent: refusalSrc\.credentialsSent \}\)/);
  assert.match(js, /handleRefusal\(error, 'config', \{ credentialsSent: bodyCarriesCredentials\(body\) \}\)/);
  assert.match(js, /function clusterNodes\(\) \{\s*return window\.configData\?\.cluster \|\| null;/);
  const state = await readFile(new URL('../src/js/notify-state.js', import.meta.url), 'utf8');
  assert.ok(!/innerHTML/.test(js + state));
});

test('mock: each forward-fix mode refuses the save at its 460e59a1 status, applies nothing and renders the expected banner', () => {
  const mockCluster = { node_id: 'local', node_name: 'Demo Computer', peers: [] };
  for (const [mode, status, code, lead, text] of [
    ['cred-holder-down', 503, 'credentials_holder_unreachable', '发送节点暂时无法连接，未保存任何更改。', '请稍后重试。'],
    ['cred-holder-unknown', 503, 'credentials_holder_unreachable', '发送节点暂时无法连接，保存结果未知。', UNKNOWN],
    ['cred-holder-refused', 409, 'credentials_holder_refused', '发送节点拒绝了这次保存，未保存任何更改。', VERSION],
    ['cred-holder-refused-5xx', 409, 'credentials_holder_refused', '发送节点拒绝了这次保存，保存结果未知。', `${UNKNOWN}（状态 503）`],
    ['cred-holder-bad-reply', 422, 'credentials_holder_bad_reply', '发送节点的回复无法识别，保存结果未知。', UNKNOWN],
    ['cred-forward-refused', 409, 'credentials_forward_refused', '本节点无法向发送节点转发保存，未保存任何更改。', '请检查本节点与发送节点的集群配置后重试。'],
    ['cred-membership-changing', 409, 'credentials_membership_changing', '节点列表正在变更，未保存任何更改。', '请稍后刷新后重试。'],
    ['cred-holder-store', 500, 'credentials_store', '发送节点写入失败，保存结果未知。', UNKNOWN],
    ['cred-invalid', 400, 'credentials_invalid', '保存内容未通过校验，未保存任何更改。', '请刷新后重新编辑。'],
    ['cred-holder-unknown-named', 503, 'credentials_holder_unreachable', 'Demo Computer 暂时无法连接，保存结果未知。', UNKNOWN],
    ['cred-forward-refused-named', 409, 'credentials_forward_refused', '本节点无法向 Demo Computer 转发保存，未保存任何更改。', '请检查本节点与 Demo Computer 的集群配置后重试。'],
  ]) {
    const m = createNotifyState(() => 1_790_000_000);
    assert.ok(m.setMode(mode), mode);
    const settings = m.route('GET', '/api/notify')[1].data;
    const before = JSON.stringify(settings);
    const body = { expected_revision: settings.revision, base_generation: 7, config: { enabled: false } };
    const res = m.route('PUT', '/api/notify', body);
    assert.equal(res[0], status, mode);
    assert.equal(res[1].data.code, code, mode);
    assert.equal(JSON.stringify(m.route('GET', '/api/notify')[1].data), before, `${mode}: nothing applied`);
    const r = notifyRefusal({ status: res[0], body: res[1] }, null, { route: 'config', cluster: mockCluster, credentialsSent: bodyCarriesCredentials(body) });
    assert.equal(r.banner.lead, lead, mode);
    assert.equal(r.banner.text, text, mode);
    assert.ok(!lines(r).includes('node-holder') && !/\blocal\b/.test(lines(r)), `${mode}: id rendered`);
  }
});

test('Clerk: the AppSecret line needs outcome === not_sent for every kind, conflicts included; banner copy unchanged', () => {
  for (const [code, status] of [['watch_conflict', 409], ['credentials_conflict', 409], ['credentials_holder_moved', 409], ['base_watch_revision_required', 400], ['base_generation_required', 400]]) {
    const missing = notifyRefusal(err(status, { code, current_revision: 9, current_generation: 9 }), null, { route: 'config', cluster });
    assert.equal(missing.outcome, 'unknown', code);
    assert.equal(missing.banner.lead, '配置已在其他节点更新，请刷新后重试。', `${code}: copy unchanged`);
    assert.deepEqual(missing.banner.action, REFRESH, code);
    const typedMissing = secretLostRefusal(missing, true);
    assert.equal(typedMissing, missing, `${code} without outcome: no AppSecret line`);
    assert.ok(!typedMissing.banner.note && !typedMissing.focus, code);
    for (const junk of ['unknown', 'NOT_SENT', null, true]) {
      const r = notifyRefusal(err(status, { code, outcome: junk }), null, { route: 'config', cluster });
      assert.equal(r.outcome, 'unknown', `${code} ${String(junk)}`);
      assert.equal(secretLostRefusal(r, true), r, `${code} ${String(junk)}: no AppSecret line`);
    }
    const sent = notifyRefusal(err(status, { code, outcome: 'not_sent' }), null, { route: 'config', cluster });
    assert.equal(sent.outcome, 'not_sent', code);
    assert.deepEqual(sent.banner, missing.banner, `${code}: same banner either way`);
    const typedSent = secretLostRefusal(sent, true);
    assert.equal(typedSent.banner.note, SECRET_NOT_SAVED, `${code} not_sent: AppSecret line`);
    assert.equal(typedSent.focus, 'notify-app-secret', code);
    assert.equal(typedSent.banner.lead, sent.banner.lead, code);
  }
  // The uncoded 503 keeps its one-line copy but has no outcome, so no AppSecret line.
  const uncoded = notifyRefusal({ status: 503, body: {} }, null, { route: 'config', cluster });
  assert.equal(uncoded.outcome, 'unknown');
  assert.equal(secretLostRefusal(uncoded, true), uncoded);
  // Every coded refusal carries an outcome.
  for (const code of ['not_owner', 'revision_conflict', 'no_writer', 'already_bound', 'credentials_missing', 'credentials_invalid', 'bad_credentials']) {
    assert.equal(notifyRefusal(err(409, { code }), null, { route: 'config', cluster }).outcome, 'unknown', code);
    assert.equal(notifyRefusal(err(409, { code, outcome: 'not_sent' }), null, { route: 'config', cluster }).outcome, 'not_sent', code);
  }
});

test('rev 6 (file 318a2264): a not-sent holder_refused without a usable 4xx/5xx status has no second line', () => {
  for (const v of [undefined, 302, 'x']) {
    const r = notifyRefusal(err(409, { code: 'credentials_holder_refused', node: HOLDER_ID, outcome: 'not_sent', ...(v === undefined ? {} : { holder_status: v }) }), null, { route: 'config', cluster });
    assert.equal(r.banner.lead, 'jp 拒绝了这次保存，未保存任何更改。');
    assert.equal(r.banner.text, '');
  }
});

// Designer B1: the refusal banner is scrolled into view (below the sticky bars)
// and stays there; focus goes to the AppSecret field only with the AppSecret line.
function stubEl({ hidden = false, disabled = false } = {}) {
  const calls = [];
  const attrs = new Map();
  return {
    calls, attrs, hidden, disabled,
    scrollIntoView(opts) { calls.push(['scrollIntoView', opts]); },
    focus(opts) { calls.push(['focus', opts]); },
    setAttribute(k, v) { calls.push(['setAttribute', k, v]); attrs.set(k, String(v)); },
    getAttribute(k) { return attrs.has(k) ? attrs.get(k) : null; },
  };
}

const resolver = map => key => (Object.hasOwn(map, key) ? map[key] : null);

test('B1: a refusal with the AppSecret line scrolls the banner into view, then focuses the AppSecret field with preventScroll', () => {
  const sent = notifyRefusal(err(409, { code: 'credentials_holder_refused', node: HOLDER_ID, holder_status: 401, outcome: 'not_sent' }), null, { route: 'config', cluster });
  const r = secretLostRefusal(sent, true);
  assert.equal(r.banner.note, SECRET_NOT_SAVED);
  const banner = stubEl(), field = stubEl(), remove = stubEl();
  assert.equal(revealRefusal(r, { banner, resolve: resolver({ 'notify-app-secret': field, 'watch-remove': remove }) }), field);
  assert.deepEqual(banner.calls, [['scrollIntoView', { block: 'start', behavior: 'smooth' }]], 'banner is scrolled, not focused');
  assert.deepEqual(field.calls, [['focus', { preventScroll: true }]], 'focus never scrolls the banner away');
  assert.deepEqual(remove.calls, []);
  assert.equal(banner.getAttribute('tabindex'), null, 'banner stays out of the tab order');
});

test('B1 (Designer ruling): watch_too_many scrolls the banner in, then focuses the first 移除 with preventScroll; none → the banner', () => {
  const r = notifyRefusal(err(400, { code: 'watch_too_many' }), null, { route: 'config', cluster });
  assert.equal(r.focus, 'watch-remove');
  const banner = stubEl(), remove = stubEl(), field = stubEl();
  assert.equal(revealRefusal(secretLostRefusal(r, true), { banner, resolve: resolver({ 'watch-remove': remove, 'notify-app-secret': field }) }), remove);
  assert.deepEqual(banner.calls, [['scrollIntoView', { block: 'start', behavior: 'smooth' }]]);
  assert.deepEqual(remove.calls, [['focus', { preventScroll: true }]]);
  assert.deepEqual(field.calls, [], 'no AppSecret line on a watch refusal');
  assert.equal(banner.getAttribute('tabindex'), null, 'no banner tabindex when 移除 takes focus');
  // No 移除 button (resolver gives null) or a disabled one: the banner takes focus.
  for (const [at, target] of [['no 移除', null], ['disabled 移除', stubEl({ disabled: true })]]) {
    const b = stubEl();
    assert.equal(revealRefusal(r, { banner: b, resolve: resolver({ 'watch-remove': target }) }), b, at);
    assert.deepEqual(b.calls, [
      ['scrollIntoView', { block: 'start', behavior: 'smooth' }],
      ['setAttribute', 'tabindex', '-1'],
      ['focus', { preventScroll: true }],
    ], at);
    assert.deepEqual(target?.calls || [], [], at);
  }
  // The other watch refusals name no target.
  for (const code of ['watch_duplicate', 'watch_unknown_channel']) {
    assert.equal(notifyRefusal(err(400, { code }), null, { route: 'config', cluster }).focus, undefined, code);
  }
  assert.equal(notifyRefusal(err(400, { code: 'invalid_field', field: 'watch.channels' }), null, { route: 'config', cluster }).focus, undefined);
});

test('B1: every other refusal (not_sent without a typed AppSecret, unknown, conflict, other watch errors, bind, 清除) focuses the banner itself (tabindex -1)', () => {
  const cases = [
    ['not_sent, no AppSecret typed', secretLostRefusal(notifyRefusal(err(409, { code: 'credentials_conflict', outcome: 'not_sent' }), null, { route: 'config', cluster }), false)],
    ['unknown ending', secretLostRefusal(notifyRefusal(err(503, { code: 'credentials_holder_unreachable', node: HOLDER_ID }), null, { route: 'config', cluster }), true)],
    ['M1 watch_conflict unknown', secretLostRefusal(notifyRefusal(err(409, { code: 'watch_conflict', outcome: 'unknown' }), null, { route: 'config', cluster }), true)],
    ['watch_duplicate', notifyRefusal(err(400, { code: 'watch_duplicate' }), null, { route: 'config', cluster })],
    ['watch_unknown_channel', notifyRefusal(err(400, { code: 'watch_unknown_channel' }), null, { route: 'config', cluster })],
    ['revision', notifyRefusal(err(409, { code: 'revision_conflict' }), null, { route: 'config', cluster })],
    ['bind already_bound', notifyRefusal(err(409, { code: 'already_bound' }), null, { route: 'openid', cluster })],
    ['uncoded 503 on 清除', secretLostRefusal(notifyRefusal({ status: 503, body: {} }, null, { route: 'config', cluster }), true)],
  ];
  for (const [at, r] of cases) {
    assert.ok(r, at);
    assert.ok(!r.banner.note, at);
    const banner = stubEl(), field = stubEl(), remove = stubEl();
    assert.equal(revealRefusal(r, { banner, resolve: resolver({ 'notify-app-secret': field, 'watch-remove': remove }) }), banner, at);
    assert.deepEqual(banner.calls, [
      ['scrollIntoView', { block: 'start', behavior: 'smooth' }],
      ['setAttribute', 'tabindex', '-1'],
      ['focus', { preventScroll: true }],
    ], at);
    assert.deepEqual(field.calls, [], `${at}: AppSecret field untouched`);
    assert.deepEqual(remove.calls, [], `${at}: 移除 untouched`);
  }
  // tabindex is set once, as an attribute.
  const banner = stubEl();
  const r = notifyRefusal(err(409, { code: 'revision_conflict' }), null, { route: 'config', cluster });
  revealRefusal(r, { banner }); revealRefusal(r, { banner });
  assert.equal(banner.calls.filter(c => c[0] === 'setAttribute').length, 1);
});

test('B1: reduced motion scrolls instantly; nothing happens without a refusal or visible banner; a disabled field falls back to the banner', () => {
  const r = secretLostRefusal(notifyRefusal(err(409, { code: 'credentials_conflict', outcome: 'not_sent' }), null, { route: 'config', cluster }), true);
  const banner = stubEl(), field = stubEl();
  revealRefusal(r, { banner, resolve: resolver({ 'notify-app-secret': field }), reducedMotion: true });
  assert.deepEqual(banner.calls[0], ['scrollIntoView', { block: 'start', behavior: 'auto' }]);
  for (const [at, args] of [['no refusal', [null, { banner: stubEl(), field: stubEl() }]], ['hidden banner', [r, { banner: stubEl({ hidden: true }), field: stubEl() }]], ['no banner', [r, { field: stubEl() }]]]) {
    const f = args[1].field;
    args[1].resolve = resolver({ 'notify-app-secret': f });
    assert.equal(revealRefusal(...args), null, at);
    assert.deepEqual(args[1].banner?.calls || [], [], at);
    assert.deepEqual(f.calls, [], at);
  }
  const b2 = stubEl(), disabled = stubEl({ disabled: true });
  assert.equal(revealRefusal(r, { banner: b2, resolve: resolver({ 'notify-app-secret': disabled }) }), b2);
  assert.deepEqual(disabled.calls, []);
});

test('B1 wiring: every refusal route reveals the banner after the render; banner clears the sticky bars', async () => {
  const js = await readFile(new URL('../src/js/notify.js', import.meta.url), 'utf8');
  const handler = js.slice(js.indexOf('function handleRefusal('), js.indexOf('// Unbind on all nodes'));
  assert.match(handler, /queueMicrotask\(\(\) => \{[\s\S]*revealRefusal\(refusal, \{[\s\S]*banner: \$\('notify-alert'\),[\s\S]*resolve: refusalFocusTarget,[\s\S]*prefers-reduced-motion: reduce/);
  // Recovery targets: the AppSecret field, or the first 移除 with no fallback to another control.
  const resolveFn = js.slice(js.indexOf('function refusalFocusTarget('), js.indexOf('function setResult('));
  assert.match(resolveFn, /key === 'watch-remove'\) return \$\('notify-watch-list'\)\?\.querySelector\('\.notify-watch-remove'\) \|\| null;/);
  assert.match(resolveFn, /key === 'notify-app-secret' \? \$\('notify-app-secret'\) : null/);
  assert.ok(!/picker-toggle/.test(resolveFn), 'no fallback to the picker toggle');
  assert.ok(!/\.focus\(\)/.test(handler), 'no unconditional focus() that scrolls the banner away');
  // Settings save, 清除 AppSecret, unbind and bind all go through handleRefusal.
  assert.match(js, /handleRefusal\(error, 'config', \{ credentialsSent: bodyCarriesCredentials\(body\) \}\)/);
  assert.match(js, /onError: error => \{\s*handleRefusal\(error, 'config'\);/);
  assert.equal((js.match(/handleRefusal\(error\);/g) || []).length, 2, 'unbind + bind');
  const css = await readFile(new URL('../src/styles.css', import.meta.url), 'utf8');
  assert.match(css, /\.notify-settings \.notify-alert \{[^}]*scroll-margin-top: calc\(var\(--appbar-h\) \+ var\(--tabbar-h\) \+ var\(--sp-4\)\);/);
  assert.match(css, /\.tabbar \{[^}]*top: var\(--appbar-h\);/);
  assert.match(css, /\.tabbar__inner \{[^}]*height: var\(--tabbar-h\);/);
});
