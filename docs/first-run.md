# First-run data / 首次配置

Release packages and automatic initialization use `assets/defaults/`: an empty channel list and area 235 (其他单机), with empty keywords/aliases. They never copy the maintainer's root channel/area lists. Existing files are preserved, including custom fields and rules.

## Add your channels / 添加频道

The wizard accepts a display name and a platform identity. Blank names use the normalized ID; you can rename them later in configuration management.

- **YouTube:** paste a `UC…` channel ID, `@handle` or official channel homepage. **识别频道** resolves it without an API key. Saving also resolves URLs. Related-channel IDs in the page are ignored; only the channel's own metadata is used.
- **Twitch:** use the account login name (the name in its URL), not a numeric user ID.
- **Niconico:** use the channel slug / `ch…` ID or `ch.nicovideo.jp` channel URL, not an individual `lv…` broadcast.

Successful setup adds these entries to your `channels.json` as well as setting the monitor targets. Existing names, aliases and platform IDs are preserved; conflicting IDs for an existing name require an explicit edit in configuration management. Saving again does not duplicate entries. New Niconico setup shows its card; daily login checks are configured separately.

### Import Holodex favourites / 导入 Holodex 收藏

In step 3, optionally enter your Holodex API key and JWT to load your favourite channels, including channels that are offline. Search the list and select the channels you want to add. The selected channels become choices for the YouTube restream target. Loading the list only previews it; **完成设置** saves your selected channels and credentials. Existing channel names, aliases and other platform mappings are kept.

第 3 步可选填 Holodex API Key 和 JWT，加载收藏频道后搜索并勾选需要导入的频道。收藏列表有独立滚动区域，不会随频道数量不断撑长页面。加载失败或收藏为空时，可以重试，也可跳过并手动添加频道。JWT 是登录凭据；界面使用密码输入框，不会在收藏结果中展示它。

![Holodex favourites with mock data](images/holodex-favorites.png)

### Choose what to monitor / 选择是否转播

Each platform offers **不转播**, a saved/imported channel, or manual entry. Choosing **不转播** disables that platform's monitor and skips its channel fields; previous target and quality preferences remain available. Choosing a valid target enables its monitor when setup is saved. Importing favourite channels alone does not enable monitoring, and completing setup with all three platforms off is allowed.

每个平台都可选「不转播」；它会关闭监控，不会删除频道或已有画质等设置。选择频道后才需要填写分区等信息，代理和画质等进阶选项可按需展开。可以只导入收藏频道，暂不开启任何转播。基础设置中的「显示卡片」只影响界面，与这里的监控选择独立。

![Mock setup](images/setup.png)

## Choose areas / 选择分区

Click **加载官方分区** in setup or area management. The backend reads Bilibili's [official catalog](https://api.live.bilibili.com/room/v1/Area/getList), and the picker groups areas by category. The results stay inside a dropdown; loading the full catalog does not add a row for every area to the page.

In setup, selected source areas are appended to `areas.json` with `id`, `name`, empty `title_keywords` and `aliases`. Existing IDs keep their names and custom rules. In area management, selecting an item fills the ID and name; you can add keywords/aliases before saving. It does not replace an existing area's ID while editing it.

![Mock area picker](images/area-picker.png)

If the API is unavailable, local areas remain usable and manual area entry still works. The minimal 235 template allows first-run setup offline. No database migration is needed: JSON reads are cached and writes use atomic files/conflict handling. The three documents are not one database transaction; a reported partial setup save can be retried without replacing existing entries.

## Packaging

From the source checkout, `bash scripts/package-assets.sh <stage-directory>` stages neutral data, matching UI files and documentation. Release builders should call this helper after copying the compiled binary. Runtime configuration and login sessions are not part of the package. Root curated JSON files remain available to the developer but are not downloaded or included in new releases.
