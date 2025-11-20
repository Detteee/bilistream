# Bilistream

[English](README.md) | [中文](README.zh_CN.md)

## Features

- Automated rebroadcasting of Twitch and YouTube streams to Bilibili Live
- Support for scheduled streams on YouTube
- Configurable and auto update Bilibili live settings (title, area and thumbnail)
- **Modern Web UI** - Beautiful control panel for monitoring and managing streams
- Interactive setup wizard for easy first-time configuration
- Comprehensive management script (`stream_manager.sh`) for easy configuration and control
- Danmaku command feature for changing the listening target channel when Bilibili live is off
- Monitor League of Legends Live game and stops Bilibili live if blacklisted words are found in player names
- Detects and avoids rebroadcasting content already being streamed by monitored Bilibili live rooms

## Dependencies

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

5. **Setup Web UI (Optional but Recommended):**

   The Web UI files are already included in the `webui/dist/` directory. No additional setup is required!
   
   The web interface will be automatically served when you run:
   ```bash
   ./bilistream webui
   ```
   
   **Note:** The Web UI is a single-page application with no external dependencies. All assets are bundled in the `webui/dist/index.html` file.

6. **Quick Setup (Recommended):**

   **Automatic Setup:**
   - Simply run `./bilistream` (or double-click on Windows)
   - If config files are missing, the setup wizard starts automatically
   - No need to remember the `setup` command!

   **Manual Setup:**
   Run the interactive setup wizard anytime:

   ```bash
   ./bilistream setup
   ```

   The setup wizard will guide you through:
   - Logging into Bilibili via QR code
   - Configuring proxy settings (if needed for YouTube/Twitch access)
   - Configuring your Bilibili live room
   - Setting up YouTube channel with area ID (optional)
   - Setting up Twitch channel with OAuth token and proxy region (optional)
   - Enabling auto cover replacement
   - Enabling danmaku commands
   - Setting detection interval
   - Configuring anti-collision monitoring with multiple rooms (optional)
   - Advanced options: Holodex API Key, Riot API Key (optional)
   - Automatically retrieving RTMP stream address

   **OR Manual Setup:**

   Configure `config.yaml` manually:

   ```yaml
   # Copy and edit the example config
   cp config.yaml.example config.yaml
   ```

   See `config.yaml.example` for detailed configuration options:

   - Bilibili live room settings
   - YouTube/Twitch channel settings
   - Area IDs for different game categories
   - Proxy settings
   - API keys for various services
   - Anti collision settings

7. Create channels configuration file:
   Create `channels.json` in the root directory with the following structure:

```json
{
  "channels": [
    {
      "name": "Channel Name",
      "platforms": {
        "youtube": "YouTube Channel ID",
        "twitch": "Twitch Channel ID"
      },
      "riot_puuid": "League of Legends PUUID"  // Optional
    }
  ]
}
```

8. (Optional) Create `invalid_words.txt` to monitor League of Legends in-game IDs:

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

### Configuration Files

#### areas.json
Contains area (game category) definitions and banned keywords:
```json
{
  "banned_keywords": [
    "gta", "watchalong", "just chatting", ...
  ],
  "areas": [
    { "id": 86, "name": "英雄联盟" },
    { "id": 329, "name": "无畏契约" },
    ...
  ]
}
```

#### channels.json
Defines available channels for monitoring:
```json
{
  "channels": [
    {
      "name": "Channel Name",
      "platforms": {
        "youtube": "YouTube Channel ID",
        "twitch": "Twitch Channel ID"
      },
      "riot_puuid": "League of Legends PUUID"
    }
  ]
}
```

## Usage

### Quick Start

**First Time Users:**
- Just run the program! If config files are missing, the setup wizard will start automatically
- Follow the interactive prompts to configure everything

**Linux/Mac:**
```bash
./bilistream 
```
- Missing config? Setup wizard starts automatically
- Ready to go? Starts monitoring streams

**Windows:**
- **Double-click `bilistream.exe`** - Automatically starts Web UI with notification
  - Shows all access URLs (localhost and LAN IP)
  - Perfect for easy management
- **CLI mode**: `bilistream.exe --cli` - Runs in command-line mode
  - Missing config? Setup wizard starts automatically
- **Any subcommand**: `bilistream.exe setup`, `bilistream.exe webui`, etc.

### Web UI (Recommended)

For easier management, use the Web UI:

```bash
./bilistream webui
```

Then open your browser and navigate to http://localhost:3150

**Windows Users:**
- Double-click `bilistream.exe` to auto-start Web UI
- A notification will pop up showing all access URLs:
  - Local: http://localhost:3150
  - LAN: http://your-ip:3150
- Click any URL to open in your browser

The Web UI provides:
- 📊 Real-time status dashboard showing Bilibili, YouTube, and Twitch status
- 🎮 One-click stream controls (start/stop)
- 💬 Send danmaku messages directly from the browser
- 📺 Channel management - easily switch monitoring targets
- 🎯 Area selection with dropdown (no need to remember area IDs)
- ⚙️ Update stream settings on the fly
- 📱 Mobile-friendly responsive interface
- 🔄 Auto-refresh status every 60 seconds

### Subcommands

Bilistream supports the following commands:

1. **Setup wizard (Recommended for first-time users):**

   ```bash
   ./bilistream setup
   ```
   
   Interactive setup wizard that helps you:
   - Login to Bilibili via QR code (or reuse existing credentials)
   - Configure proxy for YouTube/Twitch access (optional)
   - Configure config.yaml with all necessary settings
   - Set up YouTube channel with Holodex API support (optional)
   - Set up Twitch channel with OAuth token (optional)
     - Get OAuth token: https://streamlink.github.io/cli/plugins/twitch.html#authentication
   - Configure anti-collision monitoring rooms (iteratively add multiple rooms)
   - Advanced API keys: Holodex (https://holodex.net/login), Riot Games (https://developer.riotgames.com/)
   - Automatically retrieve and update RTMP stream address

2. Start streaming:

   ```bash
   ./bilistream
   ```

3. Login to Bilibili:

   ```bash
   ./bilistream login
   ```

4. Send danmaku (chat message):

   ```bash
   ./bilistream send-danmaku <message>
   ```

5. Replace stream cover:

   ```bash
   ./bilistream replace-cover <image_path>
   ```

6. Update stream area:

   ```bash
   ./bilistream update-area <area_id>
   ```

7. Renew Bilibili tokens:

   ```bash
   ./bilistream renew
   ```

8. Get live status:

   ```bash
   ./bilistream get-live-status <platform> [channel_id]
   # platform: YT, TW, bilibili, all
   ```

9. **Web UI (Control Panel):**

   ```bash
   ./bilistream webui
   # Or specify custom port
   ./bilistream webui --port 3150
   ```
   
   Launch a modern web-based control panel:
   - Real-time status monitoring for Bilibili, YouTube, and Twitch
   - Start/stop live streaming with one click
   - Send danmaku messages
   - Update stream area
   - Auto-refresh status every 10 seconds
   - Responsive design for mobile and desktop
   - Default access at http://localhost:3150

10. Generate shell completions:

   ```bash
   ./bilistream completion <shell>
   # shell: bash, zsh, fish
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

### Shell Completions

Bilistream supports command completion for bash, zsh, and fish shells. To enable completions:

#### Bash

```bash
# Generate completions
./bilistream completion bash > ~/.local/share/bash-completion/completions/bilistream
# Reload completions
source ~/.bashrc
```

#### Zsh

```bash
# Generate completions
./bilistream completion zsh > ~/.zsh/completion/_bilistream
# Reload completions
source ~/.zshrc
```

#### Fish

```bash
# Generate completions
mkdir -p ~/.config/fish/completions
./bilistream completion fish > ~/.config/fish/completions/bilistream.fish
# Reload completions
source ~/.config/fish/completions/bilistream.fish
```

## Contributing

Contributions are welcome! Please feel free to submit a Pull Request.

## License

This project is licensed under the [unlicense](LICENSE).

## Acknowledgements

- [limitcool/bilistream](https://github.com/limitcool/bilistream) 
- [Isoheptane/bilibili-live-danmaku-cli](https://github.com/Isoheptane/bilibili-live-danmaku-cli)
- All users of this project
