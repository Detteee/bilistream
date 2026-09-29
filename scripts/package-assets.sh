#!/usr/bin/env bash
# Stage neutral first-run data, matching UI and reader-facing guides.
set -euo pipefail
stage=${1:?Usage: scripts/package-assets.sh STAGING_DIRECTORY}
mkdir -p "$stage/assets/icons" "$stage/assets/defaults" "$stage/webui" "$stage/webui/mock"
cp assets/defaults/*.json "$stage/assets/defaults/"
cp README.md README.zh_CN.md LICENSE "$stage/"
cp assets/icons/icon.png assets/icons/icon.svg "$stage/assets/icons/"
cp -R webui/dist webui/public-dist "$stage/webui/"
cp webui/mock/*.mjs "$stage/webui/mock/"
mkdir -p "$stage/docs/images"
cp docs/index.md docs/first-run.md docs/advanced-settings.md docs/advanced-settings.zh_CN.md docs/niconico-session.md docs/public-status.md docs/websub-tunnel.md docs/youtube-discovery.md "$stage/docs/"
cp docs/images/*.png "$stage/docs/images/"
