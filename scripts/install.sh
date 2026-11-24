#!/bin/sh
# Installs the newest release of ideae that has a build for this system:
#   Linux x86_64             ~/.local/bin/ideae, listed with the applications
#   macOS                    ~/Applications/ideae.app
#
#   curl -fsSL https://github.com/scillarium/ideae/releases/latest/download/install.sh | sh
#
# Run again to update; the app also updates itself. `ideae --uninstall` removes it.
set -eu

repo=scillarium/ideae
system="$(uname -s)-$(uname -m)"
case "$system" in
  Linux-x86_64) build=linux-x86_64 ;;
  Darwin-arm64 | Darwin-x86_64)
    arch=$(uname -m)
    # A shell under Rosetta says x86_64 on Apple Silicon too.
    if [ "$(sysctl -n sysctl.proc_translated 2>/dev/null)" = 1 ]; then
      arch=arm64
    fi
    build="macos-$arch.app.zip"
    ;;
  *)
    echo "ideae: no build for $(uname -s) $(uname -m) yet" >&2
    exit 1
    ;;
esac
# The newest release with this system's build: one whose macOS builds
# failed has only the others. Releases come newest first.
pattern=$(printf '%s' "$build" | sed 's/\./\\./g')
tag=$(curl -fsSL "https://api.github.com/repos/$repo/releases?per_page=20" |
  grep -o "\"ideae-[0-9]\{14\}-$pattern\"" | head -n 1 | sed 's/^"ideae-\([0-9]*\)-.*/\1/')
if [ -z "$tag" ]; then
  echo "ideae: no release with a build for $(uname -s) $(uname -m) found" >&2
  exit 1
fi
from="https://github.com/$repo/releases/download/$tag"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

case "$system" in
  Linux-x86_64)
    dir="${XDG_BIN_HOME:-$HOME/.local/bin}"
    mkdir -p "$dir"
    curl -fL --progress-bar -o "$tmp/ideae" "$from/ideae-$tag-$build"
    chmod +x "$tmp/ideae"
    # A running ideae keeps the file it started from and offers a restart.
    mv -f "$tmp/ideae" "$dir/ideae"
    "$dir/ideae" --install
    case ":$PATH:" in
      *":$dir:"*) ;;
      *) echo "To run it from a terminal too, add $dir to PATH." ;;
    esac
    ;;
  Darwin-*)
    apps="$HOME/Applications"
    mkdir -p "$apps"
    curl -fL --progress-bar -o "$tmp/ideae.zip" "$from/ideae-$tag-$build"
    ditto -x -k "$tmp/ideae.zip" "$tmp/app"
    rm -rf "$apps/ideae.app"
    mv "$tmp/app/ideae.app" "$apps/ideae.app"
    # Not signed with a Developer ID: without this, the first start asks for right click → Open.
    xattr -dr com.apple.quarantine "$apps/ideae.app" 2>/dev/null || true
    echo "In $apps: open it from Launchpad or Spotlight, and keep it in the Dock."
    ;;
esac
echo "ideae $tag installed."
