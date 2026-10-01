<div align="center">

<h1><img src="assets/icons/icon.png" alt="Bilistream" width="48" height="48"> Bilistream</h1>

[English](README.md) | [中文](README.zh_CN.md)

</div>

将 YouTube 和 Twitch 转播到哔哩哔哩直播。打开浏览器控制面板，跟随向导完成设置。

![使用模拟数据的控制面板](docs/images/screenshot_of_webui.png)

## 快速开始

1. Linux 或 Windows：从 [GitHub Releases](https://github.com/Detteee/bilistream/releases) 下载并解压。Linux 运行 `./bilistream`，Windows 运行 `bilistream.exe`。
2. macOS：先[从源码编译](docs/build.zh_CN.md)，再运行 `./target/release/bilistream`。发布包不包含 macOS 版本。
3. 打开 [http://localhost:3150](http://localhost:3150)。
4. 跟随向导登录 Bilibili、设置直播间、选择频道和分区。为需要监控的平台选择转播目标，**不转播**则保持关闭。

本 README 描述当前源码，已发布版本的功能可能较早。向导的详细说明见[首次设置](docs/first-run.md)。

## 系统依赖

- **Windows：**自动下载 ffmpeg、yt-dlp，并尝试安装 Deno。Twitch/Niconico 需另行安装 streamlink，Twitch 还需 ttvlol 插件。[安装步骤](docs/dependencies.zh_CN.md#windows)。
- **Linux：**安装 ffmpeg、yt-dlp 和 Deno；Twitch/Niconico 另需 streamlink，Twitch 还需 ttvlol 插件。[安装步骤](docs/dependencies.zh_CN.md#linux)。
- **macOS：**安装同样的工具，然后从源码编译。[安装步骤](docs/dependencies.zh_CN.md#macos) · [源码编译](docs/build.zh_CN.md)。

## 使用指南

- [首次设置](docs/first-run.md)
- [远程访问与密码](docs/remote-access.zh_CN.md)
- [高级设置与命令行选项](docs/advanced-settings.zh_CN.md)
- [多服务器](docs/advanced-settings.zh_CN.md#准备工作)
- [数据、备份与升级](docs/data-and-upgrades.zh_CN.md)
- [源码编译与桌面应用](docs/build.zh_CN.md)
- [全部文档](docs/index.md)

## 许可证与致谢

采用 [Unlicense](LICENSE)。基于 [limitcool/bilistream](https://github.com/limitcool/bilistream)，弹幕功能参考 [Isoheptane/bilibili-live-danmaku-cli](https://github.com/Isoheptane/bilibili-live-danmaku-cli)。欢迎通过 Issue 和 Pull Request 贡献。
