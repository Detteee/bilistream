# Bilistream

[English](README.md) | [中文](README.zh_CN.md)

本项目受 [limitcool/bilistream](https://github.com/limitcool/bilistream) 启发，但使用 [Cursor](https://www.cursor.com/) 进行了重大重新设计和增强。虽然它们共享相同的核心概念，但此实现提供了独特的功能和改进，包括一个全面的 `stream_manager.sh` 脚本，以便于管理。

## 特性

- 自动将 Twitch 和 YouTube 流重新广播到 Bilibili 直播（支持监听+自动开播）
- 支持 YouTube 上的预告窗
- 可配置的Bilibili直播间设置（标题、分区等）
- 使用管理脚本（`stream_manager.sh`）用于轻松配置和控制
- 当 Bilibili 直播关闭时，使用弹幕命令功能更改监听目标频道
- 监控英雄联盟游戏内玩家名称并在检测到违禁词时停止直播

## 依赖

- ffmpeg
- yt-dlp
- streamlink（安装了 [2bc4/streamlink-ttvlol](https://github.com/2bc4/streamlink-ttvlol) 插件）
- [Isoheptane/bilibili-danmaku-client](https://github.com/Isoheptane/bilibili-live-danmaku-cli)（用于弹幕命令功能）
- [biliup/biliup-rs](https://github.com/biliup/biliup-rs)（用于登录B站账号）
- (可选项)[riotwatcher](https://riot-watcher.readthedocs.io/en/latest/)（用于英雄联盟游戏内玩家名称违禁词检测）

## 安装

1. 克隆仓库：

   ```bash
   git clone https://github.com/your-username/bilistream.git
   cd bilistream
   ```

2. 安装所需依赖（以 Debian 系统为例）：

   ```bash
   sudo apt update
   sudo apt install ffmpeg yt-dlp nodejs npm
   pip install streamlink
   pip install riotwatcher
   ```

3. 安装 streamlink-ttvlol 插件：
   查看[2bc4/streamlink-ttvlol](https://github.com/2bc4/streamlink-ttvlol)
4. 构建项目：

   适用于 Debian 12 及 glibc >= 2.36 的linux:

   ```bash
   cargo zigbuild --target x86_64-unknown-linux-gnu.2.36 --release
   ```

   Windows：

   ```bash
   cargo build --target x86_64-pc-windows-gnu --release
   ```

## 配置

1. 复制示例配置文件：

   ```bash
   cp config.yaml.example config.yaml
   ```

2. 编辑 `config.yaml` 以设置您的特定设置：

   - 配置所需转播的平台（Twitch 或 YouTube）
   - 设置频道 ID 和其他相关信息
   - 为T台选择一个代理地区
3. 对于弹幕功能，根据 [bilibili-danmaku-client 文档](https://github.com/Isoheptane/bilibili-live-danmaku-cli) 配置 `config.json`
4. 创建频道列表文件：
   在 YT 和 TW 文件夹中，分别创建 `YT_channels.txt` 和 `TW_channels.txt`，每行的格式为：

   ```txt
   (频道名称) [频道 ID]
   ```

5. [Isoheptane/bilibili-danmaku-client](https://github.com/Isoheptane/bilibili-live-danmaku-cli) (如果需要弹幕命令功能)

## 使用方法

### 基本用法

运行 Bilistream 应用程序：

```bash
./bilistream -c YT/config.yaml
./bilistream -c TW/config.yaml
```

### 命令行界面

Bilistream 支持以下命令：

1. 开始直播：

   ```bash
   ./bilistream start-live
   ```

2. 停止直播：

   ```bash
   ./bilistream stop-live
   ```

3. 更改直播标题：

   ```bash
   ./bilistream change-live-title <新标题>
   ```

4. 获取直播状态：

   ```bash
   ./bilistream get-live-status
   ```

### 使用 stream_manager.sh

`stream_manager.sh` 脚本提供了一个交互式界面来管理您的流：

1. 设置目录结构：

   ```bash
   mkdir YT TW
   cp config.yaml YT/config.yaml
   cp config.yaml TW/config.yaml
   ```

   文件目录结构：

   ```bash
   .
   ├── bilistream
   ├── config.json
   ├── stream_manager.sh
   ├── login-biliup
   ├── live-danmaku-cli
   ├── get_lol_id.py
   ├── invalid_words.txt
   ├── puuid.txt
   ├── TW
   │   ├── config.yaml
   │   └── TW_channels.txt
   └── YT
       ├── config.yaml
       └── YT_channels.txt
   ```

2. 分别编辑 `YT/config.yaml` 和 `TW/config.yaml`，设置适当的 YouTube 和 Twitch 设置。
3. 运行管理脚本：

   ```bash
   ./stream_manager.sh
   ```

4. 使用交互式菜单来启动、停止或管理您的转播任务。

### 弹幕命令功能

当 Bilibili 直播关闭时，您可以在 Bilibili 直播聊天中使用弹幕命令来更改监听目标频道。这允许在不重启应用程序的情况下动态控制转播目标。

使用此功能：

1. 确保 Bilibili 直播已关闭。
2. 在 Bilibili 直播聊天中发送特定的弹幕命令。
3. 系统将处理命令并相应地更改监听目标频道。

弹幕命令格式：

```txt
%转播%YT/TW%频道名称%分区名称
频道名称必须在 YT/TW_channels.txt 中
```

例如：

```txt
%转播%YT%kamito%英雄联盟
%转播%TW%kamito%无畏契约
```

系统将检查直播标题并在必要时调整分区ID。例如，如果直播标题包含"Valorant"，它将设置分区ID为329（无畏契约），无论指定的分区名称是什么。查看 <https://api.live.bilibili.com/room/v1/Area/getList> 获取更多分区名称和ID。

### 英雄联盟游戏内玩家名称检测

如果您想使用英雄联盟游戏内玩家名称检测功能：

1. 从 [Riot Developer Portal](https://developer.riotgames.com/) 获取 API Key
2. 创建 `puuid.txt` 文件，每行格式为：

   ```txt
   (频道名称) [PUUID]
   ```

3. 创建 `invalid_words.txt` 文件，每行一个需要检测的屏蔽词
4. 在配置文件中设置：

   ```yaml
   bililive:
     area_v2: 86  # 英雄联盟分区
   riot_api_key: "YOUR-RIOT-API-KEY"
   ```

当检测到游戏中有包含屏蔽词的玩家名称时，直播会自动停止。

## 贡献

欢迎贡献！请随时提交 Pull Request。

## 许可证

本项目采用 [Unlicense许可证](LICENSE)。

## 致谢

- [limitcool/bilistream](https://github.com/limitcool/bilistream)
- [Cursor](https://www.cursor.com/)
- 本项目的所有用户
