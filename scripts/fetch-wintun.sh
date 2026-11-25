#!/bin/sh
# Downloads wintun.dll (amd64) for the Windows build.
set -eu
cd "$(dirname "$0")/.."
tmp=$(mktemp -d)
curl -fsSL -o "$tmp/wintun.zip" https://www.wintun.net/builds/wintun-0.14.1.zip
echo "07c256185d6ee3652e09fa55c0b673e2624b565e02c4b9091c79ca7d2f24ef51  $tmp/wintun.zip" | shasum -a 256 -c -
unzip -q -o "$tmp/wintun.zip" -d "$tmp"
mkdir -p src-tauri/resources
cp "$tmp/wintun/bin/amd64/wintun.dll" src-tauri/resources/wintun.dll
rm -rf "$tmp"
echo "✓ src-tauri/resources/wintun.dll"
