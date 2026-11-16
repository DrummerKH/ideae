#!/bin/sh
# Installs the latest release of ideae:
#   Linux x86_64             ~/.local/bin/ideae, listed with the applications
#   macOS on Apple Silicon   ~/Applications/ideae.app
#
#   curl -fsSL https://github.com/scillarium/ideae/releases/latest/download/install.sh | sh
#
# Run again to update; the app also updates itself. `ideae --uninstall` removes it.
set -eu

repo=scillarium/ideae
# The latest release answers with a redirect to its tag.
latest=$(curl -fsSLI -o /dev/null -w '%{url_effective}' "https://github.com/$repo/releases/latest")
tag=${latest##*/}
case "$tag" in
  "" | latest) echo "ideae: no release found" >&2; exit 1 ;;
esac
from="https://github.com/$repo/releases/download/$tag"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

case "$(uname -s)-$(uname -m)" in
  Linux-x86_64)
    dir="${XDG_BIN_HOME:-$HOME/.local/bin}"
    mkdir -p "$dir"
    curl -fL --progress-bar -o "$tmp/ideae" "$from/ideae-$tag-linux-x86_64"
    chmod +x "$tmp/ideae"
    # A running ideae keeps the file it started from and offers a restart.
    mv -f "$tmp/ideae" "$dir/ideae"
    "$dir/ideae" --install
    case ":$PATH:" in
      *":$dir:"*) ;;
      *) echo "To run it from a terminal too, add $dir to PATH." ;;
    esac
    ;;
  Darwin-arm64)
    apps="$HOME/Applications"
    mkdir -p "$apps"
    curl -fL --progress-bar -o "$tmp/ideae.zip" "$from/ideae-$tag-macos-arm64.app.zip"
    ditto -x -k "$tmp/ideae.zip" "$tmp/app"
    rm -rf "$apps/ideae.app"
    mv "$tmp/app/ideae.app" "$apps/ideae.app"
    # Not signed with a Developer ID: without this, the first start asks for right click → Open.
    xattr -dr com.apple.quarantine "$apps/ideae.app" 2>/dev/null || true
    echo "In $apps: open it from Launchpad or Spotlight, and keep it in the Dock."
    ;;
  *)
    echo "ideae: no build for $(uname -s) $(uname -m) yet" >&2
    exit 1
    ;;
esac
echo "ideae $tag installed."
