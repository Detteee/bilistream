#!/bin/bash
set -e
set -o pipefail

echo "Starting bilistream deployment on Debian 12..."

# System update and upgrade
echo -e "\n\033[1;32m[1/5] Updating system packages\033[0m"
apt update -y && apt upgrade -y

# Install required packages
echo -e "\n\033[1;32m[2/5] Installing dependencies\033[0m"
apt install -y ffmpeg python3-pip screen curl unzip
## Install Python packages
pip install streamlink --break-system-packages
pip install 'yt-dlp[default]' --break-system-packages

# yt-dlp uses Deno to solve YouTube JavaScript challenges.
echo -e "\n\033[1;32m[3/5] Installing Deno\033[0m"
if ! command -v deno >/dev/null 2>&1; then
    export DENO_INSTALL="${DENO_INSTALL:-/usr/local/lib/deno}"
    curl -fsSL https://deno.land/install.sh | sh -s -- --yes --no-modify-path
    ln -sfn "$DENO_INSTALL/bin/deno" /usr/local/bin/deno
fi
deno --version

# Install Twitch plugin for streamlink
echo -e "\n\033[1;32m[4/5] Setting up Streamlink plugins\033[0m"
INSTALL_DIR="${XDG_DATA_HOME:-${HOME}/.local/share}/streamlink/plugins"
mkdir -p "$INSTALL_DIR"
curl -L -o "$INSTALL_DIR/twitch.py" \
    'https://github.com/2bc4/streamlink-ttvlol/releases/latest/download/twitch.py'

echo -e "\n\033[1;32m[5/5] Installation completed successfully!\033[0m"
echo "You can now run bilistream and finish setup in the browser."
