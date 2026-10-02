#!/bin/sh
# Build Yonder.app from this checkout and install it into /Applications.
set -eu
cd "$(dirname "$0")/.."
scripts/build-agents.sh
cd app
npm ci
npx tauri build --bundles app
cd ..
osascript -e 'quit app "Yonder"' 2>/dev/null || true
rm -rf /Applications/Yonder.app
cp -R target/release/bundle/macos/Yonder.app /Applications/
echo "Installed /Applications/Yonder.app"
# A zip to copy to other Macs (ditto keeps the bundle's symlinks and signature).
rm -f target/release/bundle/Yonder.zip
ditto -c -k --keepParent target/release/bundle/macos/Yonder.app target/release/bundle/Yonder.zip
echo "Zipped target/release/bundle/Yonder.zip"
