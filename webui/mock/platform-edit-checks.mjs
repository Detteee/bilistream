import assert from 'node:assert/strict';

// Drive the nine inline editors on the compiled page — channel, area and
// quality for each platform — through the real click handlers: open, fill,
// select, save, close, and cancel. The assertions are the HTTP boundary plus
// the row's visibility, never the source that renders them.
export async function checkPlatformEditors({ base, evaluate, waitFor }) {
  const writes = async () => (await fetch(`${base}/mock/writes`)).json();
  const rows = [];
  for (const prefix of ['yt', 'tw', 'nc']) {
    for (const kind of ['channel', 'area', 'quality']) rows.push({ prefix, kind });
  }

  const platforms = { yt: 'youtube', tw: 'twitch', nc: 'niconico' };
  const fields = { channel: 'channel', area: 'area', quality: 'quality' };

  for (const { prefix, kind } of rows) {
    const container = `document.getElementById('${prefix}-${kind}-edit-container')`;
    const select = `document.getElementById('${prefix}-${kind}-edit-select')`;
    const isOpen = `!${container}.classList.contains('hidden')`;
    const isClosed = `${container}.classList.contains('hidden')`;

    // Open: the editor row replaces the value row and fills its options.
    await evaluate(`document.getElementById('${prefix}-${kind}-edit-btn').click()`);
    await waitFor(isOpen);
    await waitFor(`${select}.options.length > 1`);

    const value = await evaluate(`${select}.options[1].value`);
    assert.ok(value, `${prefix}/${kind} has a selectable option`);

    const before = (await writes()).length;
    await evaluate(`${select}.value = ${JSON.stringify(value)}; document.getElementById('${prefix}-${kind}-save-btn').click()`);
    await waitFor(`(async () => (await (await fetch('/mock/writes')).json()).length > ${before})()`);

    const write = (await writes()).slice(before).find((row) => row.path === '/api/channel');
    assert.ok(write, `${prefix}/${kind} posts to /api/channel`);
    assert.equal(write.patch.platform, platforms[prefix], `${prefix}/${kind} platform`);
    if (kind === 'channel') {
      const option = JSON.parse(value);
      assert.equal(write.patch.channel_id, option.id, `${prefix}/channel id`);
      assert.equal(write.patch.channel_name, option.name, `${prefix}/channel name`);
    } else if (kind === 'area') {
      assert.equal(write.patch.area_id, Number(value), `${prefix}/area id`);
    } else {
      assert.equal(write.patch.quality, value, `${prefix}/quality`);
    }

    // The row closes itself on success.
    await waitFor(isClosed);

    // Cancel leaves the value row in place.
    await evaluate(`document.getElementById('${prefix}-${kind}-edit-btn').click()`);
    await waitFor(isOpen);
    await evaluate(`document.getElementById('${prefix}-${kind}-cancel-btn').click()`);
    await waitFor(isClosed);
  }

  // The empty-selection guard still refuses a save without touching the API.
  const before = (await writes()).length;
  await evaluate("document.getElementById('yt-area-edit-btn').click()");
  await waitFor("!document.getElementById('yt-area-edit-container').classList.contains('hidden')");
  await evaluate("document.getElementById('yt-area-edit-select').value = ''; document.getElementById('yt-area-save-btn').click()");
  await new Promise((resolve) => setTimeout(resolve, 300));
  assert.equal((await writes()).length, before, 'an empty area selection posts nothing');
  await evaluate("document.getElementById('yt-area-cancel-btn').click()");

  console.log('Platform editor checks passed: nine rows open, fill, save and cancel, and an empty area selection posts nothing.');
}
