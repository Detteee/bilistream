# Bilistream

[English](README.md) | [中文](README.zh_CN.md)

## Download

**Latest Release: v0.3.5**

Download from [GitHub Releases](https://github.com/Detteee/bilistream/releases)

**Windows Package includes:**

- `bilistream.exe` - Main executable
- `webui/dist/index.html` - Web UI (bundled, no external dependencies)
- `channels.json` - Channel configuration template
- `areas.json` - Area definitions and banned keywords

**Quick Start:**

1. Download and extract the release package
2. **Windows:** Double-click `bilistream.exe` - Web UI launches automatically!
3. **Linux/Mac:** Run `./bilistream` in terminal
4. Open browser to `http://localhost:3150`
5. **First run:** Complete setup wizard in browser (QR code login, configuration)
6. **Subsequent runs:** Access control panel directly
7. Dependencies auto-download on first run (Windows) or follow setup instructions below

## Features

- **Web UI** - Modern control panel for monitoring and managing streams
- **Web-Based Setup Wizard** - Complete first-run configuration through browser (no CLI needed!)
  - QR code login displayed in browser
  - Step-by-step guided setup
  - Real-time status updates
- **Auto Rebroadcast** - Twitch and YouTube streams to Bilibili Live
- **Scheduled Streams** - Support for YouTube scheduled streams
- **Auto Settings** - Update Bilibili live title, area, and thumbnail automatically
- **Danmaku Commands** - Change monitoring target via chat when offline
- **LoL Monitor** - Stop streaming if blacklisted words found in player names
- **Anti-Collision** - Avoid rebroadcasting already-streamed content

## Dependencies

**Windows:**

- ✨ **Auto-downloaded!** Core dependencies are automatically downloaded on first run:
  - ffmpeg.exe
  - yt-dlp.exe
- **For Twitch support** (optional):
  - Install streamlink: [Download](https://github.com/streamlink/windows-builds/releases) or `pip install streamlink`
  - Install ttvlol plugin: [streamlink-ttvlol](https://github.com/2bc4/streamlink-ttvlol)

**Linux/Mac:**

- ffmpeg
- yt-dlp
- streamlink (with [2bc4/streamlink-ttvlol](https://github.com/2bc4/streamlink-ttvlol) plugin)

## Setup

1. Clone the repository:

   ```bash
   git clone https://github.com/your-username/bilistream.git
   cd bilistream
   ```
2. Install the required dependencies (example for Debian-based systems):

   ```bash
   sudo apt update
   sudo apt install ffmpeg python3-pip
   pip install yt-dlp streamlink
   ```
3. Install the streamlink-ttvlol plugin:
   Follow the instructions at [2bc4/streamlink-ttvlol](https://github.com/2bc4/streamlink-ttvlol)
4. Build the project:

   For Debian 12 and other Linux distributions using glibc 2.36 or newer:

   ```bash
   cargo zigbuild --target x86_64-unknown-linux-gnu.2.36 --release
   ```

   For Windows:

   ```bash
   cargo build --target x86_64-pc-windows-gnu --release
   ```
5. **Configuration:**

   **Web-Based Setup (Recommended):**

   - Simply run `./bilistream` (or double-click on Windows)
   - Open your browser to `http://localhost:3150`
   - If config files are missing, the web setup wizard appears automatically
   - Complete all configuration through the browser interface:
     - **Step 1**: Bilibili login with QR code displayed in browser
     - **Step 2**: Basic settings (room number, intervals, features)
     - **Step 3**: Platform configuration (YouTube, Twitch, API keys)

   **CLI Setup (Alternative):**

   Run the command-line setup wizard:

   ```bash
   ./bilistream setup
   ```

   The CLI wizard guides you through:

   - Bilibili login (QR code in terminal)
   - Proxy settings (optional)
   - Live room configuration
   - YouTube/Twitch channels (optional)
   - API keys (Holodex, Riot Games - optional)
   - Anti-collision monitoring (optional)
   - Stream quality settings (for network-limited users)
6. **Stream Quality Configuration:**

   For users with limited network bandwidth, you can configure lower quality streams:

   **YouTube (yt-dlp):**

   - `best` or `source` - Best quality (recommended)
   - `worst` - Lowest quality
   - `720p`, `480p`, `360p` - Specific resolutions
   - Or use any yt-dlp format string

   **Twitch (streamlink):**

   - `best` or `source` - Best quality (recommended)
   - `worst` - Lowest quality
   - `720p`, `480p` - Specific resolutions

   Edit `config.yaml`:

   ```yaml
   Youtube:
     Quality: "720p"  # Lower quality for bandwidth-limited networks
   Twitch:
     Quality: "480p"  # Lower quality for bandwidth-limited networks
   ```
7. (Optional) Create `invalid_words.txt` to monitor League of Legends in-game IDs:

- Create a file named `invalid_words.txt` with one word per line
- Configure `RiotApiKey` and `LolMonitorInterval` in config.yaml:

  ```yaml
  RiotApiKey: "YOUR-RIOT-API-KEY"    # Get from https://developer.riotgames.com/
  LolMonitorInterval: 1               # Check interval in seconds
  ```
- The program will monitor in-game players and stop streaming if any blacklisted words are found

## File Structure

```txt
.
├── bilistream           # Main executable
├── areas.json           # Area (game categories) and banned keywords configuration
├── channels.json        # Channel configuration for YouTube, Twitch, and PUUID
├── config.yaml          # Main configuration file
├── cookies.json         # Bilibili login cookies (./bilistream login)
├── invalid_words.txt    # Filtered words for LOL players ID
└── stream_manager.sh    # Management script
```

## Usage

### Quick Start

**Run the program:**

```bash
./bilistream                    # Default: Web UI at http://localhost:3150
./bilistream --cli              # CLI monitoring mode (no Web UI)
./bilistream webui --port 8080  # Custom port
```

**Windows:** Double-click `bilistream.exe` - Web UI launches with desktop notification showing access URLs

**First run behavior:**

- **Default mode:** Web UI starts, setup wizard appears in browser
- **CLI mode (`--cli`):** Terminal-based setup wizard runs first, then monitoring starts

### Web UI Features

- 🚀 **Web-Based Setup Wizard**
  - Complete first-run configuration in browser
  - QR code login displayed directly in web page
  - No terminal/CLI knowledge required
  - Step-by-step guided process
- 📊 Real-time status dashboard (Bilibili, YouTube, Twitch)
- 🎮 One-click stream controls
- 💬 Send danmaku messages
- 📺 Channel management
- 🎯 Area selection dropdown
- 📱 Mobile-friendly inte

### Commands

```bash
./bilistream setup                              # Setup wizard
./bilistream login                              # Login to Bilibili
./bilistream                                    # Start (Web UI mode)
./bilistream --cli                              # Start (CLI mode)
./bilistream webui --port 3150                  # Web UI with custom port
./bilistream send-danmaku <message>             # Send chat message
./bilistream replace-cover <image_path>         # Update stream cover
./bilistream update-area <area_id>              # Update stream area
./bilistream renew                              # Renew Bilibili tokens
./bilistream get-live-status <platform>         # Get status (YT/TW/bilibili/all)
./bilistream completion <shell>                 # Generate completions (bash/zsh/fish)
```

### Danmaku Command Feature

Danmaku command format:

```txt
%转播%YT/TW%channel_name%area_name
channel_name must in YT/TW_channels.txt
```

Example:

```txt
%转播%YT%kamito%英雄联盟
%转播%TW%kamito%无畏契约
```

The system will check the live title and adjust the area ID if necessary. For example, if the live title contains "Valorant", it will set the area ID to 329 (无畏契约) regardless of the specified area name. Check [https://api.live.bilibili.com/room/v1/Area/getList](https://api.live.bilibili.com/room/v1/Area/getList) for more Area name and ID.

## Contributing

Contributions are welcome! Please feel free to submit a Pull Request.

## License

This project is licensed under the [unlicense](LICENSE).

## Acknowledgements

- [limitcool/bilistream](https://github.com/limitcool/bilistream)
- [Isoheptane/bilibili-live-danmaku-cli](https://github.com/Isoheptane/bilibili-live-danmaku-cli)
- All users of this project
