#!/bin/bash
# Wraps a built binary in a macOS .app, which is what the Dock can keep.
#   scripts/macos-app.sh [--link] [--dev] BINARY APP [VERSION]
# --link puts a symlink to BINARY in the bundle instead of a copy: the app
# then notices each `cargo build` and offers to restart into it.
# --dev is for a debug build, which is the separate app ideae-dev.
#   cargo build --release
#   scripts/macos-app.sh --link target/release/ideae ~/Applications/ideae.app
set -e
link= name=ideae id=io.github.scillarium.ideae
while :; do
  case $1 in
    --link) link=1 ;;
    --dev) name="ideae (dev)" id=io.github.scillarium.ideae-dev ;;
    *) break ;;
  esac
  shift
done
bin=$1 app=$2 version=${3:-0.0.0}
if [ ! -f "$bin" ] || [ -z "$app" ]; then
  echo "usage: $0 [--link] [--dev] BINARY APP [VERSION]" >&2
  exit 1
fi
root=$(cd "$(dirname "$0")/.." && pwd)
bin=$(cd "$(dirname "$bin")" && pwd -P)/$(basename "$bin")

rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
if [ -n "$link" ]; then
  ln -s "$bin" "$app/Contents/MacOS/ideae"
else
  cp "$bin" "$app/Contents/MacOS/ideae"
fi
cp "$root/assets/icon/ideae.icns" "$app/Contents/Resources/ideae.icns"
cat > "$app/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>CFBundleName</key><string>$name</string>
  <key>CFBundleDisplayName</key><string>$name</string>
  <key>CFBundleIdentifier</key><string>$id</string>
  <key>CFBundleExecutable</key><string>ideae</string>
  <key>CFBundleIconFile</key><string>ideae</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>$version</string>
  <key>CFBundleVersion</key><string>$version</string>
  <key>LSMinimumSystemVersion</key><string>11.0</string>
  <key>NSHighResolutionCapable</key><true/>
</dict></plist>
PLIST
# A linked binary carries the linker's own ad-hoc signature, and a bundle
# around a symlink cannot be signed.
[ -n "$link" ] || codesign --force --sign - "$app"
