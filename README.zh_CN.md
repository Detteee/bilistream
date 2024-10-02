# Bilistream

[English](README.md) | [中文](README.zh_CN.md)

本项目受 [limitcool/bilistream](https://github.com/limitcool/bilistream) 启发，但使用 [Cursor](https://www.cursor.com/) 进行了重大重新设计和增强。虽然它们共享相同的核心概念，但此实现提供了独特的功能和改进，包括一个全面的 `stream_manager.sh` 脚本，以便于管理。
 

## 特性

- 自动将 Twitch 和 YouTube 流重新广播到 Bilibili 直播（支持监听+自动开播）
- 支持 YouTube 上的预告窗
- 可配置的Bilibili直播间设置（标题、分区等）
- 使用管理脚本（`stream_manager.sh`）用于轻松配置和控制

## 依赖

- ffmpeg
- yt-dlp
- streamlink（安装了 [2bc4/streamlink-ttvlol](https://github.com/2bc4/streamlink-ttvlol) 插件）
- pm2（用于 `stream_manager.sh`）

## 安装

1. 克隆仓库：
   ```
   git clone https://github.com/your-username/bilistream.git
   cd bilistream
   ```

2. 安装所需依赖（以 Debian 系统为例）：
   ```
   sudo apt update
   sudo apt install ffmpeg yt-dlp nodejs npm
   sudo npm install -g pm2
   pip install streamlink
   ```

3. 安装 streamlink-ttvlol 插件：
   查看[2bc4/streamlink-ttvlol](https://github.com/2bc4/streamlink-ttvlol)


4. 构建项目

   适用于 Debian 12 及 glibc >= 2.36 的linux:
   ```
   cargo zigbuild --target x86_64-unknown-linux-gnu.2.36 --release
   ```
   Windows（未经测试）：
   ```
   cargo build --target x86_64-pc-windows-gnu --release
   ```

## 配置

1. 复制 `config.yaml.example` 文件到 `config.yaml`：
   ```
   cp config.yaml.example config.yaml
   ```

2. 编辑 `config.yaml` 以设置您的特定设置：
   - 设置您的 Bilibili 账户详细信息（SESSDATA、bili_jct 等）
   - 配置所需转播的平台（Twitch 或 YouTube）
   - 设置频道 ID 和其他相关信息

## 使用方法

### 基本用法（直接运行）

运行 bilistream 应用程序：

```
./bilistream -c ./config.yaml
```

### 使用 stream_manager.sh管理

`stream_manager.sh` 脚本提供了一个交互式界面来管理您的流：

1. 设置目录结构：
   ```
   mkdir YT 
   mkdir TW
   cp config.yaml YT/config.yaml
   cp config.yaml TW/config.yaml

   # 目录树状结构(tree .)：
   .
   ├── bili_stop_live
   ├── bilistream
   ├── stream_manager.sh
   ├── TW
   │   └── config.yaml
   └── YT
       └── config.yaml
   ```

2. 分别编辑 `YT/config.yaml` 和 `TW/config.yaml`，设置适当的 YouTube 和 Twitch 设置。

3. 运行管理脚本：
   ```
   ./stream_manager.sh
   ```

4. 使用交互式菜单来启动、停止或管理您的转播任务。

#### Bilistream 管理器主菜单
![主菜单](./assets/Main_menu.png)

