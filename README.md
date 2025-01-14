# Bilistream

[English](README.md) | [中文](README.zh_CN.md)

This project is inspired by [limitcool/bilistream](https://github.com/limitcool/bilistream) but has been significantly redesigned and enhanced using [Cursor](https://www.cursor.com/). While it shares the same core concept, this implementation offers distinct features and improvements, including a comprehensive `stream_manager.sh` script for easier management.

## Features

- Automated rebroadcasting of Twitch and YouTube streams to Bilibili Live
- Support for scheduled streams on YouTube
- Configurable Bilibili live settings (title, area, etc.)
- Comprehensive management script (`stream_manager.sh`) for easy configuration and control
- Danmaku command feature for changing the listening target channel when Bilibili live is off
- Monitor League of Legends Live game and stops Bilibili live if blacklisted words are found in player names

## Dependencies

- ffmpeg
- yt-dlp
- streamlink (with [2bc4/streamlink-ttvlol](https://github.com/2bc4/streamlink-ttvlol) plugin)
- [Isoheptane/bilibili-danmaku-client](https://github.com/Isoheptane/bilibili-live-danmaku-cli) (for danmaku command feature)
- [biliup/biliup-rs](https://github.com/biliup/biliup-rs) (auto update bilibili cookies)

## Installation

1. Clone the repository:

   ```bash
   git clone https://github.com/your-username/bilistream.git
   cd bilistream
   ```
2. Install the required dependencies (example for Debian-based systems):

   ```bash
   sudo apt update
   sudo apt install ffmpeg yt-dlp nodejs npm
   pip install streamlink
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

## Configuration

1. Copy the example configuration file:

   ```bash
   cp config.yaml.example config.yaml
   ```
2. Edit `config.yaml` with your specific settings:

   - Configure the desired streaming platform (Twitch or YouTube)
   - Set the channel ID and other relevant information
   - Select a proxy region for twitch config
3. For the danmaku feature, configure `config.json` according to the [bilibili-danmaku-client documentation](https://github.com/Isoheptane/bilibili-live-danmaku-cli)
4. Create channel list files:
   In the YT and TW folders, create `YT_channels.txt` and `TW_channels.txt` respectively, with each line in the format:

   ```txt
   (channel name) [channel id]
   ```
5. [Isoheptane/bilibili-danmaku-client](https://github.com/Isoheptane/bilibili-live-danmaku-cli) (if you need danmaku command feature)
6. (Optional) Create `invalid_words.txt` to monitor League of Legends in-game IDs:

   - Create a file named `invalid_words.txt` with one word per line
   - Configure `RiotApiKey` and `LolMonitorInterval` in config.yaml:

     ```yaml
     RiotApiKey: "YOUR-RIOT-API-KEY"    # Get from https://developer.riotgames.com/
     LolMonitorInterval: 1               # Check interval in seconds
     ```
   - The program will monitor in-game players and stop streaming if any blacklisted words are found

## Usage

### Basic Usage

Run the Bilistream application:

```bash
./bilistream -c YT/config.yaml
./bilistream -c TW/config.yaml
```

### Command-line Interface

Bilistream supports the following commands:

1. Start a live stream:

   ```bash
   ./bilistream start-live
   ```
2. Stop a live stream:

   ```bash
   ./bilistream stop-live
   ```
3. Change live stream title:

   ```bash
   ./bilistream change-live-title <new_title>
   ```
4. Get live status:

   ```bash
   ./bilistream get-live-status YT/TW/bilibili <Channel_ID/Room_ID>
   ```
5. Get YouTube live topic:

   ```bash
   ./bilistream get-live-topic YT <Channel_ID>
   ```

### Using stream_manager.sh

The `stream_manager.sh` script provides an interactive interface for managing your streams:

1. Set up the directory structure:

   ```bash
   mkdir YT TW
   cp config.yaml YT/config.yaml
   cp config.yaml TW/config.yaml
   ```

   Rename bilibup to login-biliup

   Rename bilibili-live-danmaku-cli to live-danmaku-cli
   Resulting tree structure:

   ```txt
   .
   ├── bilistream
   ├── config.json
   ├── stream_manager.sh
   ├── login-biliup
   ├── live-danmaku-cli
   ├── invalid_words.txt
   ├── puuid.txt
   ├── TW
   │   ├── config.yaml
   │   └── TW_channels.txt
   └── YT
       ├── config.yaml
       └── YT_channels.txt
   ```
2. Edit `YT/config.yaml` and `TW/config.yaml` with the appropriate settings for YouTube and Twitch, respectively.
3. Run the management script:

   ```bash
   ./stream_manager.sh
   ```
4. Use the interactive menu to start, stop, or manage your rebroadcasting tasks.

### Danmaku Command Feature

When the Bilibili live stream is off, you can use danmaku commands in the Bilibili live chat to change the listening target channel. This allows for dynamic control of the rebroadcasting target without restarting the application.

To use this feature:

1. Ensure the Bilibili live stream is off.
2. Send a specific danmaku command in the Bilibili live chat.
3. The system will process the command and change the listening target channel accordingly.

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
- All users of this project
