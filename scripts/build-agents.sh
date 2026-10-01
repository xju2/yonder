#!/bin/sh
# Build the static Linux agents and put them where the app bundles them.
# Works on macOS and Linux: Rust ships the musl libc and the linker.
set -eu
cd "$(dirname "$0")/.."
out=app/src-tauri/agents
for arch in x86_64 aarch64; do
  target="$arch-unknown-linux-musl"
  rustup target add "$target"
  cargo build --release -p yonder-agent --target "$target"
  cp "target/$target/release/yonder-agent" "$out/yonder-agent-$arch-linux"
done
ls -l "$out"
