import assert from 'node:assert/strict';

// Exercise the compiled page, including delegated clicks and the crop hook.
// Do not import source translators: these assertions are the HTTP boundary.
export async function checkStreamActions({ base, context, command, evaluate, waitFor }) {
  const mode = async value => {
    const response = await fetch(`${base}/mock/stream-actions?mode=${value}`);
    assert.equal(response.ok, true, `stream fixture ${value}`);
  };
  const reload = () => command('browsingContext.reload', { context, wait: 'complete' });
  const writes = async () => (await fetch(`${base}/mock/writes`)).json();
  const card = number => `[...document.querySelectorAll('#streams-list .stream-card')].find(card => card.querySelector('.stream-title').textContent === '示例直播 ${number}')`;
  const click = (number, selector) => evaluate(`${card(number)}.querySelector(${JSON.stringify(selector)}).click()`);
  const refresh = async () => {
    await evaluate("document.getElementById('refreshStreamsBtn').click()");
    await waitFor("document.getElementById('streams-list').getAttribute('aria-busy') === 'false'");
  };
  const nextWrite = async (path, before) => {
    await waitFor(`(async () => (await (await fetch('/mock/writes')).json()).slice(${before}).some(row => row.path === ${JSON.stringify(path)}))()`);
    return (await writes()).slice(before).find(row => row.path === path);
  };
  const expectedSwitch = (number, platform, link, area = 235) => ({
    channel_id: `UC${String(number).padStart(22, '0')}`,
    area_id: area,
    title: `示例直播 ${number}`,
    topic_id: number === 105 ? null : `Example topic ${number}`,
    status: 'live', platform,
    twitch_channel_id: platform === 'twitch' ? `example_channel_${number}` : null,
    external_link: link,
  });
  const switchAndCheck = async (number, expected, label) => {
    const before = (await writes()).length;
    await click(number, '.switch-button');
    assert.deepEqual((await nextWrite('/api/holodex/switch', before)).patch, expected, label);
  };

  try {
    for (const vocabulary of ['current', 'legacy']) {
      await mode(vocabulary);
      await reload();
      await waitFor("document.documentElement.classList.contains('app-ready') && document.querySelectorAll('#streams-list .stream-card').length === 8");
      const initialWriteCount = (await writes()).length;
      assert.equal(await evaluate(`${card(101)}.querySelector('.switch-button').checkVisibility()`), true);
      // Both same-ID rows must survive rendering and viewer-only reconciliation.
      assert.equal(await evaluate(`${card(106)} !== ${card(107)}`), true, `${vocabulary}: same ID retains two cards`);
      await evaluate(`window.streamActionCards = [${card(106)}, ${card(107)}]`);
      await mode('viewers');
      await refresh();
      assert.equal(await evaluate(`window.streamActionCards[0] === ${card(106)} && window.streamActionCards[1] === ${card(107)}`), true, `${vocabulary}: viewer updates retain each card`);
      assert.equal(await evaluate(`${card(106)}.querySelector('.stream-viewers').textContent.includes('102')`), true);

      await switchAndCheck(101, expectedSwitch(101, 'twitch', 'https://www.twitch.tv/example_channel_101?view=live#now'), `${vocabulary}: direct Twitch switch`);
      await switchAndCheck(105, expectedSwitch(105, 'youtube', null), `${vocabulary}: YouTube switch and null optionals`);
      await switchAndCheck(106, expectedSwitch(106, 'twitch', 'https://twitch.tv/example_channel_106'), `${vocabulary}: first same-ID action`);
      await switchAndCheck(107, expectedSwitch(107, 'twitch', 'https://twitch.tv/example_channel_107'), `${vocabulary}: second same-ID action`);

      let before = (await writes()).length;
      await click(102, '.switch-button');
      await waitFor("document.getElementById('area-modal').checkVisibility()");
      assert.equal(await evaluate("document.getElementById('confirm-area-selection-btn').disabled"), true);
      assert.equal((await writes()).slice(before).some(row => row.path === '/api/holodex/switch'), false, `${vocabulary}: missing area waits for choice`);
      await evaluate("document.querySelector('#modal-area-list [data-id=\"329\"]').click(); document.getElementById('confirm-area-selection-btn').click()");
      assert.deepEqual((await nextWrite('/api/holodex/switch', before)).patch, expectedSwitch(102, 'twitch', 'https://twitch.tv/example_channel_102/videos', 329), `${vocabulary}: area completion retains stream data`);

      before = (await writes()).length;
      await click(103, '.crop-switch-button');
      await waitFor("document.getElementById('cropModal').classList.contains('active') && !document.getElementById('cropCanvasContainer').classList.contains('hidden') && document.getElementById('cropCanvas').width === 640");
      const capture = await nextWrite('/api/crop/capture/twitch', before);
      assert.deepEqual(capture.query, { channel_id: 'example_channel_103' }, `${vocabulary}: crop targets Twitch login`);
      assert.equal((await writes()).slice(before).some(row => row.path === '/api/holodex/switch'), false, `${vocabulary}: crop waits for apply`);
      await evaluate("document.getElementById('cropX').value='16'; document.getElementById('cropY').value='8'; document.getElementById('cropWidth').value='320'; document.getElementById('cropHeight').value='180'; document.getElementById('crop-apply-btn').click()");
      assert.deepEqual((await nextWrite('/api/crop/update', before)).patch, { platform: 'twitch', enabled: true, x: 16, y: 8, width: 320, height: 180 }, `${vocabulary}: crop rectangle`);
      assert.deepEqual((await nextWrite('/api/holodex/switch', before)).patch, expectedSwitch(103, 'twitch', 'http://twitch.tv/example_channel_103?crop=1'), `${vocabulary}: crop completion retains topic/link/IDs`);
      await waitFor("!document.getElementById('cropModal').classList.contains('active')");

      assert.equal(await evaluate(`${card(104)}.querySelector('.crop-switch-button') === null`), true, `${vocabulary}: Niconico has no crop`);
      await switchAndCheck(104, expectedSwitch(104, 'niconico', 'https://live.nicovideo.jp/watch/lv100000104'), `${vocabulary}: Niconico switch`);

      assert.equal(await evaluate(`${card(108)}.querySelector('.holodex-add-channel-start') === null`), true, `${vocabulary}: configured Twitch channel suppresses add`);
      before = (await writes()).length;
      await click(103, '.holodex-add-channel-start');
      assert.equal(await evaluate(`${card(103)}.querySelector('.holodex-add-channel-confirm').checkVisibility()`), true);
      await click(103, '.holodex-add-channel-confirm');
      assert.deepEqual((await nextWrite('/api/manage/channels', before)).patch, {
        name: '示例频道 103', aliases: [],
        platforms: { youtube: 'UC0000000000000000000103', twitch: 'example_channel_103' },
        riot_puuid: null,
      }, `${vocabulary}: favorites add includes both channel IDs`);
      await waitFor(`${card(103)}.querySelector('.holodex-channel-added-icon') !== null`);
      await refresh();
      assert.equal(await evaluate(`${card(103)}.querySelector('.holodex-add-channel-start') === null`), true, `${vocabulary}: saved favorite stays suppressed after refresh`);
      assert.deepEqual((await writes()).slice(initialWriteCount).map(row => row.path), [
        ...Array(5).fill('/api/holodex/switch'),
        '/api/crop/capture/twitch', '/api/crop/update', '/api/holodex/switch',
        '/api/holodex/switch', '/api/manage/channels',
      ], `${vocabulary}: each click writes once; crop saves before switching`);
    }
    console.log('Stream action checks passed: current/legacy YT/TW/NC switch bodies, area selection, crop completion, favorites add/suppression, duplicate-ID cards and viewer-only retention.');
  } finally {
    // Fixture state is scoped to this helper; leave the existing harness intact.
    await mode('reset');
    await evaluate('delete window.streamActionCards');
    await reload();
    await waitFor("document.documentElement.classList.contains('app-ready') && document.querySelectorAll('#streams-list .stream-card').length === 2");
  }
}
