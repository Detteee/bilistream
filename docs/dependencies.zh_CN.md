# 安装依赖

[返回快速开始](../README.zh_CN.md) · [English](dependencies.md)

Bilistream 使用 ffmpeg 转播视频。YouTube 使用 yt-dlp 和 Deno 获取播放地址、处理 JavaScript 验证。Twitch 和 Niconico 使用 streamlink，Twitch 还需 streamlink-ttvlol 插件。请在运行 Bilistream 的电脑上安装依赖。

## Windows

将程序解压到可写的文件夹，再运行 `bilistream.exe`。Bilistream 会将缺少的 `ffmpeg.exe` 和 `yt-dlp.exe` 下载到该文件夹，并尝试安装 Deno。如果 Deno 安装失败，请按 [Deno 官方安装说明](https://docs.deno.com/runtime/getting_started/installation/)操作。安装 Deno 后重启 Bilistream，使程序能够找到它。

使用 Twitch 或 Niconico 时，通过 [Windows 安装程序](https://github.com/streamlink/windows-builds/releases)另行安装 streamlink。确保 `streamlink` 在 `PATH` 中，打开新终端运行 `streamlink --version` 检查。Twitch 还需按 [streamlink-ttvlol 安装说明](https://github.com/2bc4/streamlink-ttvlol#installation)安装插件。安装后重启 Bilistream。

## Linux

使用发行版的包管理器安装 ffmpeg。Debian/Ubuntu 可运行以下命令，同时安装后续所需工具：

```bash
sudo apt update
sudo apt install ffmpeg pipx curl unzip
pipx ensurepath
pipx install 'yt-dlp[default]'
```

使用 [Deno 官方安装程序](https://docs.deno.com/runtime/getting_started/installation/)安装 Deno：

```bash
curl -fsSL https://deno.land/install.sh | sh
```

使用 Twitch 或 Niconico 时，另行运行 `pipx install streamlink`。Twitch 还需按 [streamlink-ttvlol 安装说明](https://github.com/2bc4/streamlink-ttvlol#installation)安装插件。

安装后打开新终端，使 `PATH` 的修改生效。如果仍找不到 Deno，在 shell 启动文件中加入 `export PATH="$HOME/.deno/bin:$PATH"`。以服务方式运行 Bilistream 时，也要确保服务账号能找到这些程序。

其他发行版请使用其 ffmpeg 软件包，并参考 [yt-dlp](https://github.com/yt-dlp/yt-dlp#installation) 和 [streamlink](https://streamlink.github.io/install.html) 的安装说明。仓库中的 Debian `install.sh` 也会安装 Deno 及其他依赖。

下载的 Bilistream 文件没有执行权限时，先运行 `chmod +x bilistream`，再运行 `./bilistream`。

## macOS

安装 [Homebrew](https://brew.sh/) 后运行：

```bash
brew install ffmpeg yt-dlp deno
```

使用 Twitch 或 Niconico 时，另行运行 `brew install streamlink`。Twitch 还需按 [streamlink-ttvlol 安装说明](https://github.com/2bc4/streamlink-ttvlol#installation)安装插件。安装后打开新终端；如无执行权限，先运行 `chmod +x bilistream`，再运行 `./bilistream`。

## 检查安装

Linux/macOS 下，使用运行 Bilistream 的同一账号检查：

```bash
ffmpeg -version
yt-dlp --version
deno --version
```

Twitch/Niconico 还需检查 `streamlink --version`。Windows 的 ffmpeg 和 yt-dlp 位于 Bilistream 程序旁；Deno 和 streamlink 需要能通过 `PATH` 找到。

部分来源需要 Cookie。YouTube Cookie 与 Niconico `user_session` 在 Web UI 设置，Bilibili 登录由向导完成。接下来按[首次设置](first-run.md)操作。
