<div align="center">

<h1><img src="assets/icons/icon.png" alt="Bilistream" width="48" height="48"> Bilistream</h1>

[English](README.md) | [中文](README.zh_CN.md)

</div>

Rebroadcast YouTube and Twitch to Bilibili Live, with a browser control panel and an optional Tauri desktop shell.

## Quick start

1. Download a build from [GitHub Releases](https://github.com/Detteee/bilistream/releases). This README describes the current source; published releases may contain an earlier feature set.
2. On Windows, run `bilistream.exe`. On Linux/macOS, run `./bilistream` and open `http://localhost:3150`.
3. Complete the browser setup wizard: Bilibili QR login, room settings, your own channels and official area selection. Paste a YouTube channel URL or @handle, or optionally import selected Holodex favourites using your API key and JWT.
4. Choose a restream target for each platform you want to monitor; **不转播** leaves that platform off. You can import channels and start monitoring later. The default setup runs on a single server.

New installations start with an empty channel roster and a minimal area template (其他单机). The wizard saves your selections and preserves existing data. UI assets are bundled; Windows ffmpeg/yt-dlp dependencies download when needed.

## Control panel

![Mock dashboard with simulated data](docs/images/screenshot_of_webui.png)

- **Source monitors:** YouTube and Twitch; configure title, area, quality, crop and HLS cache by platform.
- **Live & upcoming list:** configure a Holodex API key to browse streams, switch targets and see suggested areas. Optional Holodex login adds favourites.
- **Stream controls:** Bilibili start/stop, rebroadcast restart, live bitrate/cache meters, title/area/cover updates, danmaku commands, keyword filters, LoL player-name checks and collision avoidance.

Setup and area management offer the official area picker, filling IDs and names automatically. [First-run guide](docs/first-run.md).

## Advanced settings

For priority channels, RSS, WebSub, YouTube Data API, public status, multi-server mode and Niconico, see [Advanced settings](docs/advanced-settings.md).

## Dependencies and build

- **ffmpeg** and **yt-dlp**: Windows installs the binaries automatically; install them yourself on Linux/macOS.
- **Deno**: yt-dlp uses it for YouTube JavaScript challenges. `install.sh` installs it on Debian; other systems can use the [official installer](https://docs.deno.com/runtime/getting_started/installation/).
- **streamlink**: needed for Twitch and Niconico. For Twitch, install the [streamlink-ttvlol plugin](https://github.com/2bc4/streamlink-ttvlol).
- Some sources need cookies. Configure YouTube cookies and the Niconico user_session value in the Web UI; Bilibili login is handled by the wizard.

Build the current source with a Rust toolchain:

```bash
git clone https://github.com/Detteee/bilistream.git
cd bilistream
cargo build --locked --release --bin bilistream
./target/release/bilistream
```

The optional desktop package is `src-tauri` (`bilistream-tauri`); it shares the Rust backend and needs the [Tauri platform prerequisites](https://v2.tauri.app/start/prerequisites/). Linux cross-builds can use `cargo zigbuild --locked --target x86_64-unknown-linux-gnu.2.36 --release` with cargo-zigbuild and Zig installed.

## Launch and configuration

```bash
./bilistream --webui
./bilistream --tray
./bilistream --port 3150
./bilistream --bind 127.0.0.1
./bilistream --password '<password>'
./bilistream --ffmpeg-log-level error
```

Bind, port and password can also come from `BILISTREAM_BIND`, `BILISTREAM_PORT` and `BILISTREAM_PASSWORD`. The admin listener defaults to localhost. Binding another address requires a password; use HTTPS through a reverse proxy for remote access. Five failed logins from one connection IP temporarily block further attempts for up to one minute.

Settings, channels, rules and discovery statistics live in `data/bilistream.db` beside the executable. Configuration and login credentials are encrypted; the application unlocks them automatically on this computer. Change settings through the Web UI. `BILISTREAM_DATA_DIR` selects another data directory; `BILISTREAM_KEY_FILE` can select a protected key file **outside** it.

Stop the old process before starting the new binary. Upgrade imports existing JSON/Cookie files automatically, including older configurations missing newer settings. Original app-owned files are retired only after a verified encrypted recovery copy. External Cookie files are left untouched. Keep the original key: copying the database alone is not a portable backup.

Use **System Settings → 数据与备份** to download a password-protected backup. A new installation can restore it from the setup wizard. Keep its password separately. Do not delete database `-wal` or `-shm` files. Images remain in cache directories, and `webui/` contains page assets.

To downgrade, stop the service and run `./bilistream --export-legacy ./downgrade-data` with the new binary. The new directory contains plaintext settings and credentials; use it with the old binary and remove it when no longer needed. In a cluster, upgrade each node before resuming configuration sync or handoff to it.

The default priority monitor and multi-server mode are off. Display preferences do not change monitor state. Existing configuration keys remain compatible.

Danmaku target changes use channel names saved in channel management, for example:

```text
%转播%YT%示例频道1%英雄联盟
%转播%TW%示例频道1%无畏契约
%查询
```

Area keyword rules may adjust the requested area. LoL checking requires a Riot key; `lol_monitor_interval` is in minutes.

## License and acknowledgements

[Unlicense](LICENSE). Based on [limitcool/bilistream](https://github.com/limitcool/bilistream), with danmaku support informed by [Isoheptane/bilibili-live-danmaku-cli](https://github.com/Isoheptane/bilibili-live-danmaku-cli). Contributions through issues and pull requests are welcome.
