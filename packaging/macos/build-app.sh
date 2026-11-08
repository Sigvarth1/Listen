#!/usr/bin/env bash
# Builds Listen.app for macOS (the architecture of this Mac) into target/release/Listen.app.
#
#   packaging/macos/build-app.sh            # build the bundle
#   packaging/macos/build-app.sh --install  # also copy it to /Applications (or ~/Applications)
#
# Listen plays through mpv; YouTube Music also needs yt-dlp and Deno. Install them with
# `brew install mpv yt-dlp deno` (or from Settings → Advanced → Helper programs in the app).
set -euo pipefail

root="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$root"

version="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n 1)"
cargo build --release -p listen

app="target/release/Listen.app"
rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp target/release/listen "$app/Contents/MacOS/Listen"
cp packaging/macos/Listen.icns "$app/Contents/Resources/Listen.icns"
sed "s/@VERSION@/$version/g" packaging/macos/Info.plist > "$app/Contents/Info.plist"

# An ad-hoc signature over the whole bundle: macOS then knows the app by its bundle id
# (Keychain and privacy permissions stay granted across rebuilds of the same bundle id).
codesign --force --sign - --identifier io.github.leks2205.Listen "$app"

echo "Built $app ($version)"

if [[ "${1:-}" == "--install" ]]; then
  dest="/Applications"
  [[ -w "$dest" ]] || dest="$HOME/Applications"
  mkdir -p "$dest"
  rm -rf "$dest/Listen.app"
  cp -R "$app" "$dest/"
  echo "Installed to $dest/Listen.app"
fi

for tool in mpv yt-dlp deno; do
  if ! command -v "$tool" >/dev/null && [[ ! -x "/opt/homebrew/bin/$tool" && ! -x "/usr/local/bin/$tool" ]]; then
    echo "note: $tool is not installed (brew install $tool)"
  fi
done
