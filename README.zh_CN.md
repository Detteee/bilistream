<div align="center">

<h1><img src="assets/icons/icon.png" alt="Bilistream" width="48" height="48"> Bilistream</h1>

[English](README.md) | [中文](README.zh_CN.md)

</div>

将 YouTube 和 Twitch 转播到哔哩哔哩直播，提供浏览器控制面板和可选的 Tauri 桌面应用。

## 快速开始

1. 从 [GitHub Releases](https://github.com/Detteee/bilistream/releases) 下载。本 README 描述当前源码，已发布版本的功能可能较早。
2. Windows 双击 `bilistream.exe`；Linux/macOS 运行 `./bilistream`，打开 `http://localhost:3150`。
3. 完成浏览器设置向导：Bilibili 扫码登录、直播间设置、添加自己的频道并选择官方分区。YouTube 可粘贴频道主页或 @handle，也可使用 Holodex API Key 和 JWT 勾选导入收藏频道。
4. 为需要监控的平台选择转播目标；选择**不转播**则关闭该平台监控。也可以先导入频道，稍后再启用。默认按单机方式运行。

新安装使用空频道表和最小分区模板（其他单机）；向导会保存你选择的频道和分区，已有数据不会被覆盖。界面从程序内置资源安装，Windows 的 ffmpeg、yt-dlp 按需下载。

## 控制面板

![使用模拟数据的仪表盘](docs/images/screenshot_of_webui.png)

- **来源监控**：YouTube、Twitch，按平台配置标题、分区、画质、裁剪与 HLS 缓存。
- **直播与预告**：配置 Holodex API Key 后浏览直播并切换转播目标，自动推荐分区；可选登录以使用收藏夹。
- **转播控制**：Bilibili 开关播、重启转播、码率与缓存仪表、标题/分区/封面更新、弹幕指令、关键词过滤、英雄联盟玩家名称检查和防撞车。

向导与配置管理均支持官方分区选择，自动填写 ID 和名称。[首次设置说明](docs/first-run.md)。

## 高级设置

优先频道、RSS、WebSub、YouTube Data API、公开状态页、多服务器和 Niconico 的说明见 [高级设置](docs/advanced-settings.zh_CN.md)。

## 依赖与编译

- **ffmpeg、yt-dlp**：Windows 自动安装二进制，Linux/macOS 自行安装。
- **Deno**：用于 YouTube 的 JavaScript 验证；`install.sh` 已包含安装，其他系统参考 [官方说明](https://docs.deno.com/runtime/getting_started/installation/)。
- **streamlink**：Twitch 和 Niconico 需要；Twitch 还需 [streamlink-ttvlol 插件](https://github.com/2bc4/streamlink-ttvlol)。
- 部分来源需要 Cookie。YouTube Cookie 与 Niconico user_session 在 Web UI 设置，Bilibili 登录由向导完成。

安装 Rust 工具链后编译：

```bash
git clone https://github.com/Detteee/bilistream.git
cd bilistream
cargo build --locked --release --bin bilistream
./target/release/bilistream
```

可选桌面包位于 `src-tauri`（`bilistream-tauri`），共用 Rust 后端，需要 [Tauri 平台依赖](https://v2.tauri.app/start/prerequisites/)。安装 cargo-zigbuild 和 Zig 后，可用 `cargo zigbuild --locked --target x86_64-unknown-linux-gnu.2.36 --release` 进行 Linux 交叉编译。

仓库已包含编译后的 Web UI。修改 `webui/src` 或 `webui/public-src` 后，先用 Node.js 22+ 重新生成，再编译 Rust：

```bash
npm ci --prefix webui --ignore-scripts
npm --prefix webui run build
```

## 启动与配置

```bash
./bilistream --webui
./bilistream --tray
./bilistream --port 3150
./bilistream --bind 127.0.0.1
./bilistream --password-file ~/.config/bilistream/webui-password
./bilistream --ffmpeg-log-level error
```

监听地址、端口和密码也可通过 `BILISTREAM_BIND`、`BILISTREAM_PORT`、`BILISTREAM_PASSWORD` 设置。管理端默认只监听本机；非本机监听必须设置访问密码，远程访问建议使用 HTTPS 反向代理。同一连接 IP 登录失败 5 次后，最多限制 1 分钟。

首次设置密码（Linux/bash），输入不回显，也不记入 shell 历史：

```bash
umask 077
mkdir -p ~/.config/bilistream
read -rsp 'Web UI 密码: ' bilistream_password
printf '\n'
printf '%s' "$bilistream_password" > ~/.config/bilistream/webui-password
unset bilistream_password
chmod 600 ~/.config/bilistream/webui-password
./bilistream --bind 0.0.0.0 --password-file ~/.config/bilistream/webui-password
```

该文件以明文保存密码，由账号权限保护。请保留供后续启动使用；命令行只显示路径。自动重启使用单独的临时凭据，读取后删除。原有 `--password` 仍兼容。多服务器还需配置[节点共享密钥](docs/advanced-settings.zh_CN.md#多服务器)。

设置、频道、规则及发现统计保存在程序旁的 `data/bilistream.db`。配置与登录信息加密保存，在本机重启时自动解锁；日常修改通过 Web UI 完成。可用 `BILISTREAM_DATA_DIR` 指定数据目录，用 `BILISTREAM_KEY_FILE` 指定目录之外的受限密钥文件。

切换新版前先停止旧进程。升级时自动导入原有 JSON 和 Cookie 文件，缺少新设置项的旧配置也可直接升级。验证加密恢复副本后才移除程序目录内的旧数据；外部 Cookie 文件不会删除。请保留原密钥，单独复制数据库不能用于换机恢复。

在「系统设置 → 数据与备份」下载密码保护的备份，新安装可在设置向导中恢复。请另行保管备份密码，不要删除数据库的 `-wal`、`-shm` 文件。图片继续保存在缓存目录，`webui/` 为页面资源。

需要回退时，先停止服务，再用新版运行 `./bilistream --export-legacy ./downgrade-data`。新目录内是供旧版使用的明文配置和凭据，用完请删除。集群各节点升级后才能恢复配置同步和切换。

优先监控和多服务器默认关闭；显示设置不改变监控状态。已有配置键保持兼容。

弹幕切换使用频道管理中保存的频道名，例如：

```text
%转播%YT%示例频道1%英雄联盟
%转播%TW%示例频道1%无畏契约
%查询
```

分区关键词规则可能调整请求的分区。英雄联盟检查需要 Riot key，`lol_monitor_interval` 的单位是分钟。

## 许可证与致谢

采用 [Unlicense](LICENSE)。基于 [limitcool/bilistream](https://github.com/limitcool/bilistream)，弹幕功能参考 [Isoheptane/bilibili-live-danmaku-cli](https://github.com/Isoheptane/bilibili-live-danmaku-cli)。欢迎通过 Issue 和 Pull Request 贡献。
