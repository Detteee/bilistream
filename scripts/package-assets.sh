#!/usr/bin/env bash
# Stage icons, the Web UI and reader-facing guides. First-run channel and area
# records are written by the setup wizard, not copied from a defaults directory.
set -euo pipefail
stage=${1:?Usage: scripts/package-assets.sh STAGING_DIRECTORY}
mkdir -p "$stage/assets/icons" "$stage/webui" "$stage/webui/mock"
cp README.md README.zh_CN.md LICENSE "$stage/"
cp assets/icons/icon.png assets/icons/icon.svg "$stage/assets/icons/"
cp -R webui/dist webui/public-dist "$stage/webui/"
cp webui/mock/*.mjs "$stage/webui/mock/"
mkdir -p "$stage/docs/images"
cp docs/index.md docs/first-run.md \
  docs/dependencies.md docs/dependencies.zh_CN.md \
  docs/remote-access.md docs/remote-access.zh_CN.md \
  docs/data-and-upgrades.md docs/data-and-upgrades.zh_CN.md \
  docs/build.md docs/build.zh_CN.md \
  docs/advanced-settings.md docs/advanced-settings.zh_CN.md \
  docs/niconico-session.md docs/public-status.md docs/websub-tunnel.md docs/youtube-discovery.md \
  "$stage/docs/"
cp docs/images/*.png "$stage/docs/images/"
