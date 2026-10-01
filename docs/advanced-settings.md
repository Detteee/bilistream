# Advanced settings

[Back to the main guide](../README.md) · [中文](advanced-settings.zh_CN.md)

The main guide covers ordinary YouTube/Twitch rebroadcasting. Enable these features as needed.

## Control panel

The YouTube and Twitch cards let you configure title, area, quality, crop and HLS cache by platform. Stream controls include Bilibili start/stop, rebroadcast restart, live bitrate/cache meters, title/area/cover updates and collision avoidance. A Holodex API key adds live and upcoming streams, target switching and suggested areas; optional Holodex login adds favourites.

The priority monitor and multi-server mode are off by default. Display preferences do not change monitor state. Existing configuration keys remain compatible.

## Command-line options

Launch the console Web UI or system tray, or change an individual setting:

```bash
./bilistream --webui
./bilistream --tray
./bilistream --port 3150
./bilistream --bind 127.0.0.1
./bilistream --ffmpeg-log-level error
```

The tray is the default on Windows; the console Web UI is the default on Linux/macOS. These commands use `./bilistream`; on Windows use `bilistream.exe` instead.

| Option | Environment variable | Purpose |
| --- | --- | --- |
| `--bind ADDR` | `BILISTREAM_BIND` | Admin listen address; default `127.0.0.1`. |
| `-p`, `--port PORT` | `BILISTREAM_PORT` | Admin port; default `3150`. |
| `--password PASSWORD`, `--password-file PATH` | `BILISTREAM_PASSWORD` | Initial password import only; saved or explicitly cleared state takes precedence. See [remote access and password](remote-access.md). |
| `--cluster-token-file PATH` | `BILISTREAM_CLUSTER_TOKEN_FILE` | Shared node key; see [multi-server mode](#multi-server-mode). |
| `--ffmpeg-log-level LEVEL` | `BILISTREAM_FFMPEG_LOG_LEVEL` | `error`, `info` or `debug`; default `error`. |
| `--reset-panel-password` | — | Clear the panel password offline with the service stopped, then exit; see [recovery](remote-access.md#forgotten-password). |
| `--export-legacy DIR` | — | Export plaintext for an older binary and exit; see [downgrade](data-and-upgrades.md#downgrade). |
| `-h`, `--help` / `-V`, `--version` | — | Print help or version and exit. |
| — | `BILISTREAM_DATA_DIR`, `BILISTREAM_KEY_FILE` | Data directory and protected key location; see [data and backups](data-and-upgrades.md). |

## Priority channel

**Priority channel:** an optional card for preferred YouTube/Twitch channels. Basic Settings → **显示优先频道** only shows/hides the card. Its monitor switch enables priority selection; **自动重启流** allows an active rebroadcast to switch when the priority channel becomes playable. Hiding the card does not stop its monitor.

1. In System Settings → Basic Settings, enable **显示优先频道** and save.
2. Choose the channel and default area on its dashboard card, then enable its monitor.
3. Enable **自动重启流** if it should interrupt another rebroadcast after priority playback is confirmed.

Visibility and monitoring are independent. Hiding the card preserves an enabled monitor; turn off the card's monitor switch to stop priority scheduling.

## YouTube discovery settings

| Setting/source | Purpose |
| --- | --- |
| YouTube Data API keys | Confirm live/upcoming status and fund uploads-playlist polling. Multiple keys from different Google projects can provide separate quotas. |
| **YouTube RSS** in Basic Settings | Read channel feeds; on by default. Turning it off leaves WebSub and budgeted uploads-playlist discovery running. |
| WebSub callback URL and port | Receive push hints. Works on a standalone server; requires a YouTube key and a reachable callback. Empty URL disables it. |
| Holodex API key | Supplement the list with metadata, categories, schedules and external streams. Optional Holodex login enables favourites. |
| **yt-dlp 兜底** in Basic Settings | Probe YouTube directly every monitor cycle. With it off, index mode still has a periodic yt-dlp safety probe. |

RSS/WebSub discover video IDs; neither proves that a stream is playable. The Data API classifies them, and playback still needs a usable stream URL. Failed/disabled RSS falls back to uploads polling within the available budget. WebSub failure restores normal polling speed. See [discovery and fallback details](youtube-discovery.md) and [WebSub tunnel setup](websub-tunnel.md).

In a cluster with a usable shared index, the **YouTube index node** handles discovery. Set its keys, RSS and callback there. The **active restream node** may be a different machine. These names describe separate responsibilities.

## Public status page

One computer or server is enough. In **System Settings → 公开状态页**, select **本机** and save; leave multi-server mode off. The default viewer URL is `http://127.0.0.1:23234`. [Single-server setup and sharing](public-status.md).

## Multi-server mode

**Optional multi-server mode:** active/standby control, health checks, configuration sync and automatic failover. The public status page runs on a selected node, with a separate listener.

- Off by default. Each node needs a unique ID, reachable admin API address and consistent membership. Nodes use a shared communication key; their Web UI passwords can differ.
- The active restream node publishes; standby nodes wait for handoff. Automatic failover relies on heartbeats, quorum agreement and source-shutdown checks, and remains fenced when safe takeover cannot be confirmed.
- Select the public-status node, separate listener port and public URL. The viewer listener does not expose admin routes.
- A public-status node with usable YouTube keys also acts as the YouTube index node; it may differ from the active restream node.
- Keys, RSS, WebSub callback and card visibility are node-local settings. In shared-index mode configure discovery on the index node.

See [discovery fallbacks](youtube-discovery.md) and [WebSub tunnel setup](websub-tunnel.md).

First set each node’s password locally in **System Settings → 安全**; headless servers can use initial bootstrap from the [remote-access guide](remote-access.md).

Create the communication key **once**, then copy the same file to each node over SSH:

```bash
umask 077
mkdir -p ~/.config/bilistream
openssl rand -hex 32 > ~/.config/bilistream/cluster-token
chmod 600 ~/.config/bilistream/cluster-token
./bilistream --bind 0.0.0.0 \
  --cluster-token-file ~/.config/bilistream/cluster-token
```

`BILISTREAM_CLUSTER_TOKEN_FILE` can also specify the file. Keep this key separate from the Web UI password; it is plaintext protected by file permissions. Use HTTPS or a private network for node API addresses. A missing or mismatched key prevents peer authentication.

When upgrading from shared Web UI password authentication, stop the cluster, configure this key and upgrade **all** nodes before resuming. Sign in to the Web UI again after the first upgrade. Later restarts preserve browser sessions; logout and expiry (30 days) invalidate them. Changing or clearing a password immediately revokes all sessions. Backups exclude panel passwords and browser sessions. Rotate the communication key on all nodes together and restart them.

## Niconico Live

Configure the Niconico channel and install streamlink, then enter its `user_session` value in System Settings. A full cookie export is unnecessary; legacy Netscape paths remain supported. Daily checks report validity without renewing the session, and distinguish network uncertainty from invalid credentials. [Session checks](niconico-session.md).

Basic Settings has independent Twitch, Niconico and priority-card visibility switches. Hiding a card leaves monitoring unchanged. The multi-server panel is hidden while multi-server mode is off. Niconico uses piped ingest; priority channels currently support YouTube/Twitch.

## Danmaku, area rules and LoL checks

Danmaku target changes use channel names saved in channel management, for example:

```text
%转播%YT%示例频道1%英雄联盟
%转播%TW%示例频道1%无畏契约
%查询
```

Area keyword rules may adjust the requested area. LoL player-name checking requires a Riot key; `lol_monitor_interval` is in minutes. Player-name filter keywords appear below the Riot API Key when LoL player ID monitoring is enabled.

## Settings preview

![Advanced settings with simulated data](images/settings.png)

Rendered from the real UI using simulated data.
