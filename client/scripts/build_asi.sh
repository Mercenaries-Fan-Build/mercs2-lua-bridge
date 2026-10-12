#!/usr/bin/env bash
# Build lua_bridge.asi and place it at ../src/ (the Shipment's plugin path).
# Requires: rustup target i686-pc-windows-gnu, i686-w64-mingw32-gcc on PATH, and
# scripts/fetch_dw2_libgcc.sh run once per rustc version.
set -euo pipefail
client="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$client"
cargo build --release --target i686-pc-windows-gnu
dll="target/i686-pc-windows-gnu/release/lua_bridge.dll"
asi="../src/lua_bridge.asi"
if [[ -f "$asi" ]]; then
  echo "before: $(shasum -a 256 "$asi")"
fi
cp "$dll" "$asi"
echo "after:  $(shasum -a 256 "$asi")"
