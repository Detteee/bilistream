# Bilistream

[English](README.md) | [中文](README.zh_CN.md)

## Features

- Automated rebroadcasting of Twitch and YouTube streams to Bilibili Live
- Support for scheduled streams on YouTube
- Configurable and auto update Bilibili live settings (title, area and thumbnail)
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

5. **Quick Setup (Recommended):**

   Run the interactive setup wizard to configure everything:

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

6. Create channels configuration file:
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
├── channels.json        # Channel configuration for YouTube, Twitch, and PUUID
├── config.yaml          # Main configuration file
├── cookies.json         # Bilibili login cookies (./bilistream login)
├── invalid_words.txt    # Filtered words for LOL players ID
└── stream_manager.sh    # Management script
```

## Usage

Run the Bilistream application:

```bash
./bilistream 
```

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

9. Generate shell completions:

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
