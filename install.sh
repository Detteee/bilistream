#!/bin/bash
set -e
set -o pipefail

echo "Starting bilistream deployment on Debian 12..."

# System update and upgrade
echo -e "\n\033[1;32m[1/5] Updating system packages\033[0m"
apt update -y && apt upgrade -y

# Install required packages
echo -e "\n\033[1;32m[2/5] Installing dependencies\033[0m"
apt install -y ffmpeg python3-pip screen curl
## Install Python packages
pip install streamlink --break-system-packages
pip install yt-dlp --break-system-packages

# Install Twitch plugin for streamlink
echo -e "\n\033[1;32m[4/5] Setting up Streamlink plugins\033[0m"
INSTALL_DIR="${XDG_DATA_HOME:-${HOME}/.local/share}/streamlink/plugins"
mkdir -p "$INSTALL_DIR"
curl -L -o "$INSTALL_DIR/twitch.py" \
    'https://github.com/2bc4/streamlink-ttvlol/releases/latest/download/twitch.py'

echo -e "\n\033[1;32m[5/5] Installation completed successfully!\033[0m"
echo "You can now proceed with bilistream setup and configuration."