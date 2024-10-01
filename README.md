# Bilistream

[English](README.md) | [中文](README.zh_CN.md)

This project is inspired by [limitcool/bilistream](https://github.com/limitcool/bilistream) but has been significantly redesigned and enhanced using [Cursor](https://www.cursor.com/). While it shares the same core concept, this implementation offers distinct features and improvements, including a comprehensive `stream_manager.sh` script for easier management.

## Features

- Automated rebroadcasting of Twitch and YouTube streams to Bilibili Live
- Support for scheduled streams on YouTube
- Configurable bilibili live settings (title, area, etc.)
- Comprehensive management script (`stream_manager.sh`) for easy configuration and control

## Dependencies

- ffmpeg
- yt-dlp
- streamlink (with [2bc4/streamlink-ttvlol](https://github.com/2bc4/streamlink-ttvlol) plugin installed)
- pm2 (for `stream_manager.sh`)

## Installation

1. Clone the repository:
   ```
   git clone https://github.com/your-username/bilistream.git
   cd bilistream
   ```

2. Install the required dependencies (example for Debian-based systems):
   ```
   sudo apt update
   sudo apt install ffmpeg yt-dlp nodejs npm
   sudo npm install -g pm2
   pip install streamlink
   ```

3. Install the streamlink-ttvlol plugin:
   Refer to [2bc4/streamlink-ttvlol](https://github.com/2bc4/streamlink-ttvlol)

4. Build the project:
   For Debian 12 and  other Linux distributions using glibc 2.36 or newer.
   ```
   cargo zigbuild --target x86_64-unknown-linux-gnu.2.36 --release
   ```
   For Windows (have not been tested)
   ```
   cargo build --target x86_64-pc-windows-gnu --release
   ```

## Configuration

1. Copy the `config.yaml.example` file to `config.yaml` and:
   ```
   cp config.yaml.example config.yaml
   ```

2. Edit `config.yaml` with your specific settings:
   - Set your Bilibili account details (SESSDATA, bili_jct, etc.)
   - Configure the desired streaming platform (Twitch or YouTube)
   - Set the channel ID and other relevant information

## Usage

### Basic Usage

Run the bilistream application:

```
./bilistream -c ./config.yaml
```

### Using stream_manager.sh

The `stream_manager.sh` script provides an interactive interface for managing your streams:

1. Set up the directory structure:
   ```
   mkdir YT TW
   cp config.yaml YT/config.yaml
   cp config.yaml TW/config.yaml
   ```

2. Edit `YT/config.yaml` and `TW/config.yaml` with the appropriate settings for YouTube and Twitch, respectively.

3. Run the management script:
   ```
   ./stream_manager.sh
   ```

4. Use the interactive menu to start, stop, or manage your rebroadcasting tasks.

#### Bilistream manager main menu
![Main Menu](./assets/Main_menu.png)
## Contributing

Contributions are welcome! Please feel free to submit a Pull Request.

## License

This project is licensed under the [GPL-3.0 license](LICENSE).