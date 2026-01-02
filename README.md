<div align="center">

<h1>
  <img src="icon.svg" alt="Bilistream" width="48" height="48" style="vertical-align: middle;">
  Bilistream
</h1>

[English](README.md) | [中文](README.zh_CN.md)

</div>

## Download

**Latest Release: v0.4.4**

Download from [GitHub Releases](https://github.com/Detteee/bilistream/releases)

**Quick Start:**

1. **Windows:** Double-click `bilistream.exe` - Runs in background, browser opens webui automatically!
2. **Linux/Mac:** Run `./bilistream` in terminal
3. **Auto-download:** Required files download automatically on first run:
   - `webui/dist/index.html` - Web interface
   - `areas.json` - Bilibili categories and banned keywords
   - `channels.json` - Preset channel list
   - **Windows only:** `yt-dlp.exe` and `ffmpeg.exe`
4. Open browser to `http://localhost:3150`
5. **First run:** Complete setup wizard in browser (QR code login, configuration)
6. **Subsequent runs:** Access control panel directly

## Features

- **Auto-Update** - One-click updates from Web UI
  - Automatic update detection on startup
  - Safe installation with backup
  - Preserves all configuration and user data
  - Auto-restart after update
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

## Web UI Preview

![Web UI Screenshot](screenshot_of_webui.png)

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

   For users with limited network bandwidth, you can configure stream quality settings:

   **YouTube (yt-dlp) Quality Options:**

   - `best` - Best available quality (recommended)
   - `best[height<=1080]` - Best quality up to 1080p
   - `best[height<=720]` - Best quality up to 720p  
   - `best[height<=480]` - Best quality up to 480p
   - `best[height<=360]` - Best quality up to 360p
   - `worst` - Lowest available quality

   **Twitch (streamlink) Quality Options:**

   - `source` - Original broadcaster quality
   - `high` - High quality (≤720p30fps, filters out higher resolutions)
   - `medium` - Medium quality (≤540p30fps, filters out higher resolutions)
   - `low` - Low quality (≤360p30fps, filters out higher resolutions)
   - `audio_only` - Audio stream only (no video)
   - `worst` - Lowest available quality

   Edit `config.json`:

   ```json
   {
     "youtube": {
       "quality": "best[height<=720]"
     },
     "twitch": {
       "quality": "high"
     }
   }
   ```
7. (Optional) Create `invalid_words.txt` to monitor League of Legends in-game IDs:

- Create a file named `invalid_words.txt` with one word per line
- Configure `RiotApiKey` and `LolMonitorInterval` in config.json:

  ```json
  {
    "riot_api_key": "YOUR-RIOT-API-KEY",
    "lol_monitor_interval": 1
  }
  ```
- The program will monitor in-game players and stop streaming if any blacklisted words are found

## File Structure

```txt
.
├── bilistream           # Main executable
├── areas.json           # Area (game categories) and banned keywords configuration
├── channels.json        # Channel configuration for YouTube, Twitch, and PUUID
├── config.json          # Main configuration file
├── cookies.json         # Bilibili login cookies (./bilistream login)
├── invalid_words.txt    # Filtered words for LOL players ID
└── stream_manager.sh    # Management script
```

## Usage

### Quick Start

**Easiest way - just run it:**

```bash
./bilistream
```

**What happens:**

- **Windows:** Runs in background, browser opens webui automatically, tray icon appears
- **Linux/Mac:** Starts web server, open `http://localhost:3150` in browser

**Advanced options:**

```bash
./bilistream tray               # Force background mode (with system tray)
./bilistream webui              # Force web mode (shows console logs)
./bilistream cli                # Command-line only (no web interface)
```

**First run:**

- Setup wizard appears in your browser
- Follow the steps to login and configure
- That's it!

### Web UI Features

- 🚀 **Web-Based Setup Wizard**
  - Complete first-run configuration in browser
  - QR code login displayed directly in web page
  - No terminal/CLI knowledge required
  - Step-by-step guided process
- ⚙️ **Configuration Management**
  - Edit all settings through web interface
  - Real-time configuration updates
  - No need to manually edit JSON files
  - Manage channels, areas, and platform settings
- 📊 Real-time status dashboard (Bilibili, YouTube, Twitch)
- 🎮 One-click stream controls
- 💬 Send danmaku messages
- 📺 Channel management
- 🎯 Area selection dropdown
- 📱 Mobile-friendly interface

### Commands

```bash
# Running modes
./bilistream                                    # Default (tray on Windows, webui on Linux)
./bilistream tray                               # System tray mode
./bilistream webui                              # Web UI mode
./bilistream cli                                # CLI only mode

# Setup and configuration
./bilistream setup                              # Setup wizard
./bilistream login                              # Login to Bilibili
./bilistream renew                              # Renew Bilibili tokens

# Stream control
./bilistream start-live                         # Start streaming
./bilistream stop-live                          # Stop streaming
./bilistream change-live-title <title>          # Change stream title
./bilistream update-area <area_id>              # Update stream area
./bilistream replace-cover <image_path>         # Update stream cover

# Status and utilities
./bilistream get-live-status <platform>         # Get status (YT/TW/bilibili/all)
./bilistream send-danmaku <message>             # Send chat message
./bilistream completion <shell>                 # Generate completions (bash/zsh/fish)

# Custom ports
./bilistream webui --port 8080                  # Web UI with custom port
./bilistream tray --port 8080                   # Tray mode with custom port
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
