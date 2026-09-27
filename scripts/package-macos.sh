#!/bin/bash
set -euo pipefail
exe="$1"
package='dist/Paired-Trader-Mac-arm64'
mkdir -p "$package/licenses"
cp "$exe" "$package/双标的模拟"
cp scripts/start-paper-macos.command "$package/启动双标的模拟.command"
chmod +x "$package/双标的模拟" "$package/启动双标的模拟.command"
cp README.md "$package/使用说明.md"
cp -R config "$package/config-templates"
cp -R vendor/lighter-signing/licenses/. "$package/licenses/"
cp vendor/lighter-signing/LICENSE* "$package/licenses/"
cp vendor/hyperliquid_rust_sdk/LICENSE "$package/licenses/Hyperliquid-MIT.txt"
# This diagnostic exits before creating data or connecting to any venue.
"$package/双标的模拟" --build-info > "$package/build-info.json"
node -e 'const i=require("./"+process.argv[1]);if(i.dirty||i.source_commit!==process.env.GITHUB_SHA||i.runtime!=="paper")process.exit(1)' "$package/build-info.json"
ditto -c -k --keepParent "$package" dist/Paired-Trader-Mac-arm64.zip
shasum -a 256 dist/Paired-Trader-Mac-arm64.zip > dist/SHA256SUMS.txt
