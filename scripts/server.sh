#!/usr/bin/env bash
# Builds ideae-server for the systems a server may run: static Linux
# binaries (musl, through cargo-zigbuild) for x86_64 and aarch64, and on a
# Mac the macOS ones too. A debug build of the app puts these on a server
# (src/daemon/install.rs); a release puts its own.
#
#   rustup target add x86_64-unknown-linux-musl aarch64-unknown-linux-musl
#   cargo install cargo-zigbuild    (and Zig on PATH)
set -euo pipefail
cd "$(dirname "$0")/.."
for target in x86_64-unknown-linux-musl aarch64-unknown-linux-musl; do
  cargo zigbuild -p ideae-core --bin ideae-server --release --target "$target" --target-dir target/server
done
if [ "$(uname -s)" = Darwin ]; then
  for target in aarch64-apple-darwin x86_64-apple-darwin; do
    if rustup target list --installed | grep -qx "$target"; then
      cargo build -p ideae-core --bin ideae-server --release --target "$target" --target-dir target/server
    fi
  done
fi
ls -l target/server/*/release/ideae-server
