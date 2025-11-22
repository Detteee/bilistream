# Bilistream

[English](README.md) | [中文](README.zh_CN.md)

## 下载

**最新版本：v0.3.5**

从 [GitHub Releases](https://github.com/Detteee/bilistream/releases) 下载最新版本

**Windows 发行包包含：**

- `bilistream.exe` - 主程序
- `webui/dist/index.html` - Web UI（已打包，无需外部依赖）
- `channels.json` - 频道配置模板
- `areas.json` - 分区定义和禁用关键词

**快速开始：**

1. 下载并解压发行包
2. **Windows:** 双击 `bilistream.exe` - Web UI 自动启动！
3. **Linux/Mac:** 终端运行 `./bilistream`
4. 在浏览器中打开 `http://localhost:3150`
5. **首次运行:** 在浏览器中完成设置向导（二维码登录、配置）
6. **后续运行:** 直接访问控制面板
7. 依赖项首次运行时自动下载（Windows）或按下方说明安装

## 功能特点

- **Web UI** - 现代化控制面板，用于监控和管理直播
- **网页版设置向导** - 通过浏览器完成首次配置（无需命令行！）
  - 浏览器中显示二维码登录
  - 分步引导式设置
  - 实时状态更新
- **自动转播** - Twitch 和 YouTube 直播到哔哩哔哩
- **预定直播** - 支持 YouTube 预定直播
- **自动设置** - 自动更新哔哩哔哩直播标题、分区和封面
- **弹幕命令** - 离线时通过聊天更改监控目标
- **英雄联盟监控** - 玩家名称发现黑名单词汇时停止直播
- **防撞车** - 避免转播已被转播的内容

## 依赖

**Windows:**

- ✨ **自动下载！** 核心依赖项会在首次运行时自动下载：
  - ffmpeg.exe
  - yt-dlp.exe
- **Twitch 支持**（可选）：
  - 安装 streamlink: [下载](https://github.com/streamlink/windows-builds/releases) 或 `pip install streamlink`
  - 安装 ttvlol 插件: [streamlink-ttvlol](https://github.com/2bc4/streamlink-ttvlol)

**Linux/Mac:**

- ffmpeg
- yt-dlp
- streamlink (需安装 [2bc4/streamlink-ttvlol](https://github.com/2bc4/streamlink-ttvlol) 插件)

## 安装步骤

1. 克隆仓库：

   ```bash
   git clone https://github.com/your-username/bilistream.git
   cd bilistream
   ```
2. 安装所需依赖（以 Debian 系统为例）：

   ```bash
   sudo apt update
   sudo apt install ffmpeg python3-pip
   pip install yt-dlp streamlink
   ```
3. 安装 streamlink-ttvlol 插件：
   按照 [2bc4/streamlink-ttvlol](https://github.com/2bc4/streamlink-ttvlol) 的说明进行操作
4. 构建项目：

   对于 Debian 12 和其他使用 glibc 2.36 或更新版本的 Linux 发行版：

   ```bash
   cargo zigbuild --target x86_64-unknown-linux-gnu.2.36 --release
   ```

   对于 Windows：

   ```bash
   cargo build --target x86_64-pc-windows-gnu --release
   ```
5. **配置：**

   **网页版设置（推荐）:**

   - 直接运行 `./bilistream`（或双击 `bilistream.exe`）
   - 在浏览器中打开 `http://localhost:3150`
   - 如果缺少配置文件，网页设置向导会自动显示
   - 通过浏览器界面完成所有配置：
     - **步骤 1**: 哔哩哔哩登录（二维码显示在浏览器中）
     - **步骤 2**: 基础设置（直播间号、检测间隔、功能开关）
     - **步骤 3**: 平台配置（YouTube、Twitch、API 密钥）

   **命令行设置（备选）:**

   运行命令行设置向导：

   ```bash
   ./bilistream setup
   ```

   命令行向导将引导你完成：

   - 哔哩哔哩登录（终端显示二维码）
   - 代理设置（可选）
   - 直播间配置
   - YouTube/Twitch 频道（可选）
   - API 密钥（Holodex、Riot Games - 可选）
   - 防撞车监控（可选）
   - 流质量设置（适用于网络受限用户）
6. **流质量配置：**

   对于网络带宽受限的用户，可以配置较低质量的流：

   **YouTube (yt-dlp):**

   - `best` - 最佳质量（推荐）
   - `worst` - 最低质量
   - `720p`、`480p`、`360p` - 指定分辨率
   - 或使用任何 yt-dlp 格式字符串

   **Twitch (streamlink):**

   - `best` - 最佳质量（推荐）
   - `worst` - 最低质量
   - `720p`、`480p` - 指定分辨率

   编辑 `config.yaml`：

   ```yaml
   Youtube:
     Quality: "720p"  # 网络受限时使用较低质量
   Twitch:
     Quality: "480p"  # 网络受限时使用较低质量
   ```
7. （可选）创建 `invalid_words.txt` 以监控英雄联盟游戏内 ID：

   - 创建名为 `invalid_words.txt` 的文件，每行一个词
   - 在 config.yaml 中配置 `RiotApiKey` 和 `LolMonitorInterval`：

     ```yaml
     RiotApiKey: "YOUR-RIOT-API-KEY"    # 从 https://developer.riotgames.com/ 获取
     LolMonitorInterval: 1               # 检查间隔（秒）
     ```
   - 程序将监控游戏内玩家，如发现黑名单词汇则停止直播

## 文件结构

```txt
.
├── bilistream           # 主程序可执行文件
├── areas.json           # 分区（游戏类别）和禁用关键词配置
├── channels.json        # YouTube、Twitch 和 PUUID 的频道配置
├── config.yaml          # 主配置文件
├── cookies.json         # 哔哩哔哩登录 cookies（./bilistream login）
├── invalid_words.txt    # 英雄联盟玩家 ID 过滤词 (可选)
└── stream_manager.sh    # 管理脚本
```

## 使用方法

### 快速开始

**运行程序：**

```bash
./bilistream                    # 默认：Web UI 访问 http://localhost:3150
./bilistream --cli              # CLI 监控模式（无 Web UI）
./bilistream webui --port 8080  # 自定义端口
```

**Windows:** 双击 `bilistream.exe` - Web UI 启动并显示桌面通知（含访问地址）

**首次运行行为：**

- **默认模式：** Web UI 启动，设置向导在浏览器中显示
- **CLI 模式 (`--cli`)：** 终端设置向导先运行，然后开始监控

### Web UI 功能

- 🚀 **网态页版设置向导**
  - 在浏览器中完成首次配置
  - 二维码直接显示在网页中
  - 无需终端/命令行知识
  - 分步引导式流程
- 📊  实时状态仪表板（Bilibili、YouTube、Twitch）
- 🎮 一键直播控制
- 💬 发送弹幕消息
- 📺 频道管理
- 🎯 分区下拉选择
- 📱 移动端友好界面

### 命令

```bash
./bilistream setup                              # 设置向导
./bilistream login                              # 登录哔哩哔哩
./bilistream                                    # 启动（Web UI 模式）
./bilistream --cli                              # 启动（CLI 模式）
./bilistream webui --port 3150                  # 自定义端口的 Web UI
./bilistream send-danmaku <弹幕内容>             # 发送弹幕
./bilistream replace-cover <图片路径>            # 更新直播封面
./bilistream update-area <分区ID>               # 更新直播分区
./bilistream renew                              # 更新哔哩哔哩令牌
./bilistream get-live-status <平台>             # 获取状态（YT/TW/bilibili/all）
./bilistream completion <shell>                 # 生成补全脚本（bash/zsh/fish）
```

### 弹幕命令功能

弹幕命令格式：

```txt
%转播%YT/TW%频道名称%分区名称
频道名称必须在 YT/TW_channels.txt 中
```

示例：

```txt
%转播%YT%kamito%英雄联盟
%转播%TW%kamito%无畏契约
```

系统会检查直播标题并根据需要调整分区 ID。例如，如果直播标题包含 "Valorant"，无论指定的分区名称是什么，都会将分区 ID 设置为 329（无畏契约）。查看 [https://api.live.bilibili.com/room/v1/Area/getList](https://api.live.bilibili.com/room/v1/Area/getList) 获取更多分区名称和 ID。

## 贡献

欢迎贡献！请随时提交 Pull Request。

## 许可证

本项目采用 [unlicense](LICENSE) 许可证。

## 致谢

- [limitcool/bilistream](https://github.com/limitcool/bilistream)
- [Isoheptane/bilibili-live-danmaku-cli](https://github.com/Isoheptane/bilibili-live-danmaku-cli)
