<div align="center">

<h1><img src="assets/icons/icon.png" alt="Bilistream" width="48" height="48"> Bilistream</h1>

[English](README.md) | [中文](README.zh_CN.md)

</div>

Rebroadcast YouTube and Twitch to Bilibili Live. Open the browser control panel and follow the setup wizard.

![Control panel with simulated data](docs/images/screenshot_of_webui.png)

## Quick start

1. On Linux or Windows, download and extract the build from [GitHub Releases](https://github.com/Detteee/bilistream/releases). Run `./bilistream` on Linux, or `bilistream.exe` on Windows.
2. On macOS, [build from source](docs/build.md), then run `./target/release/bilistream`. Releases do not include a macOS build.
3. Open [http://localhost:3150](http://localhost:3150).
4. Follow the wizard to sign in to Bilibili, set your room, and choose channels and areas. Select a target for each platform you want to monitor; **不转播** leaves it off.

This README describes the current source; published releases may contain an earlier feature set. Need help with the wizard? See the [first-run guide](docs/first-run.md).

## What your system needs

- **Windows:** ffmpeg and yt-dlp download automatically; Deno installation is attempted. Install streamlink separately for Twitch/Niconico, plus ttvlol for Twitch. [Installation steps](docs/dependencies.md#windows).
- **Linux:** install ffmpeg, yt-dlp and Deno; add streamlink for Twitch/Niconico and ttvlol for Twitch. [Installation steps](docs/dependencies.md#linux).
- **macOS:** install the same tools, then build from source. [Installation steps](docs/dependencies.md#macos) · [Build from source](docs/build.md).

## Guides

- [First-run setup](docs/first-run.md)
- [Remote access and password](docs/remote-access.md)
- [Advanced settings and command-line options](docs/advanced-settings.md)
- [Multi-server mode](docs/advanced-settings.md#before-you-start)
- [Data, backups and upgrades](docs/data-and-upgrades.md)
- [Build from source and desktop app](docs/build.md)
- [All documentation](docs/index.md)

## License and acknowledgements

[Unlicense](LICENSE). Based on [limitcool/bilistream](https://github.com/limitcool/bilistream), with danmaku support informed by [Isoheptane/bilibili-live-danmaku-cli](https://github.com/Isoheptane/bilibili-live-danmaku-cli). Contributions through issues and pull requests are welcome.
