#!/bin/sh
# Build Yonder.app from this checkout and install it into /Applications.
set -eu
cd "$(dirname "$0")/.."
scripts/build-agents.sh
cd app
npm ci
npx tauri build --bundles app
cd ..
# Yonder asks before quitting, so wait until it has really gone: replacing
# the bundle under a running copy breaks the relaunch below.
if pgrep -xq yonder-app; then
  echo "Quitting Yonder; answer its prompt if it asks…"
  osascript -e 'quit app "Yonder"' 2>/dev/null || true
  while pgrep -xq yonder-app; do sleep 0.5; done
fi
rm -rf /Applications/Yonder.app
cp -R target/release/bundle/macos/Yonder.app /Applications/
echo "Installed /Applications/Yonder.app"
# `yonder .` opens the current folder.
mkdir -p ~/.local/bin
ln -sf /Applications/Yonder.app/Contents/Resources/yonder ~/.local/bin/yonder
open -a /Applications/Yonder.app
# A zip to copy to other Macs (ditto keeps the bundle's symlinks and signature).
rm -f target/release/bundle/Yonder.zip
ditto -c -k --keepParent target/release/bundle/macos/Yonder.app target/release/bundle/Yonder.zip
echo "Zipped target/release/bundle/Yonder.zip"
