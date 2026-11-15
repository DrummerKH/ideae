#!/bin/bash
# Type-checks the macOS build from Linux: cargo-zigbuild compiles the C parts
# with Zig's macOS headers. gpui's Metal shaders are stitched by a build
# script that runs on macOS only, so empty placeholders stand in for them;
# this checks the Rust, it builds nothing to run.
#   rustup target add aarch64-apple-darwin; cargo install cargo-zigbuild
set -e
cd "$(dirname "$0")/.."
target=aarch64-apple-darwin
dir=target/mac
for attempt in 1 2 3; do
  if cargo-zigbuild check --target "$target" --target-dir "$dir" "$@"; then
    exit 0
  fi
  # A fresh target dir has no out dirs yet: the first run makes them.
  created=0
  for out in "$dir/$target"/debug/build/gpui-*-apple-*/out; do
    [ -d "$out" ] || continue
    [ -f "$out/stitched_shaders.metal" ] && continue
    touch "$out/stitched_shaders.metal"
    created=1
  done
  [ "$created" = 1 ] || exit 1
done
exit 1
