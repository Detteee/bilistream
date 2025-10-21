# Bilistream

[English](README.md) | [中文](README.zh_CN.md)

## 功能特点

- 自动将 Twitch 和 YouTube 直播转播到哔哩哔哩直播
- 支持 YouTube 的预定直播
- 可配置/自动更新的哔哩哔哩直播设置（标题、分区和封面）
- 管理脚本（`stream_manager.sh`）用于更改配置
- 当哔哩哔哩未转播时可通过弹幕命令更改监听目标频道
- 监控英雄联盟游戏内玩家名称，如发现黑名单词汇则停止直播
- 自动检测并避免转播已被转播的频道

## 依赖

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
5. 配置 `config.yaml`：

   ```yaml
   # 复制并编辑示例配置
   cp config.yaml.example config.yaml
   ```

   查看 `config.yaml.example` 了解详细配置选项：

   - 哔哩哔哩直播间设置
   - YouTube/Twitch 频道设置
   - 不同游戏分类的分区 ID
   - 代理设置
   - 各种服务的 API 密钥
   - 防撞车设置

6. 创建频道配置文件：
   在根目录创建 `channels.json`，使用以下结构：

```json
{
  "channels": [
    {
      "name": "频道名称",
      "platforms": {
        "youtube": "YouTube频道ID",
        "twitch": "Twitch频道ID"
      },
      "riot_puuid": "英雄联盟PUUID"  // 可选
    }
  ]
}
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
├── channels.json        # YouTube、Twitch 和 PUUID 的频道配置
├── config.yaml          # 主配置文件
├── cookies.json         # 哔哩哔哩登录 cookies（./bilistream login）
├── invalid_words.txt    # 英雄联盟玩家 ID 过滤词
└── stream_manager.sh    # 管理脚本
```

## 使用方法

运行 Bilistream 应用：

```bash
./bilistream 
```

### 子命令

Bilistream 支持以下命令：

1. 开始直播：

   ```bash
   ./bilistream
   ```
2. 登录哔哩哔哩：

   ```bash
   ./bilistream login
   ```
3. 发送弹幕：

   ```bash
   ./bilistream send-danmaku <弹幕内容>
   ```
4. 更换直播间封面：

   ```bash
   ./bilistream replace-cover <图片路径>
   ```
5. 更新直播间分区：

   ```bash
   ./bilistream update-area <分区ID>
   ```
6. 更新哔哩哔哩令牌：

   ```bash
   ./bilistream renew
   ```
7. 获取直播状态：

   ```bash
   ./bilistream get-live-status <平台> [频道ID]
   # 平台: YT, TW, bilibili, all
   ```
8. 生成命令补全脚本：

   ```bash
   ./bilistream completion <shell>
   # shell: bash, zsh, fish
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
