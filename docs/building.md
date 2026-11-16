# Building

Requirements:

- Rust (stable, edition 2024)
- Zig 0.17.x (builds libghostty-vt)
- On Linux, system libraries:
  `libxkbcommon-dev libxkbcommon-x11-dev libxcb-xkb-dev libx11-xcb-dev libwayland-dev libvulkan-dev libfontconfig-dev libfreetype-dev libasound2-dev`

```bash
cargo build --release
./target/release/ideae [REPO...]
```

Options: `--agent CMD` or `--no-agent`, `--theme auto|light|dark`,
`--install` / `--uninstall` (see [platforms](platforms.md)).

A debug build is a separate app (`ideae-dev`): its own state, sockets and
launcher entry, so it can run next to a release build.

## Portable Linux binary

To run on older glibc, build against glibc 2.31 with
[cargo-zigbuild](https://github.com/rust-cross/cargo-zigbuild):

```bash
CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUSTFLAGS="-C link-arg=-Wl,--allow-shlib-undefined" \
  cargo zigbuild --release --target x86_64-unknown-linux-gnu.2.31
```

## macOS

A build of your own becomes an app the Dock can keep with
`scripts/macos-app.sh --link target/release/ideae ~/Applications/ideae.app`:
the bundle links to the binary, so the app offers a restart after each
`cargo build --release`. Run `scripts/macos-sign.sh` after each build on a Mac
where the app was given Screen Recording or Accessibility, or they are gone at
the next restart.

## Windows

On Windows: Rust (MSVC), Zig 0.17.x and the Windows SDK (gpui compiles its
shaders with `fxc.exe`; point `GPUI_FXC_PATH` at it when it is not found),
then `cargo build --release`.

A debug build cross-compiles on Linux with
`cargo build --target x86_64-pc-windows-gnu` (needs `mingw-w64`), enough to
check that it compiles and to run the tests under Wine. Such a build reads
its shaders from this checkout at run time, so it cannot be handed out, and a
release build needs a Windows host for `fxc.exe`.

## Releases

A push to `main` whose CI passes on every system becomes a release tagged with
its UTC time (`yyyymmddhhmmss`): a Linux binary, a Windows `.exe`, macOS
builds and the install scripts (`.github/workflows/release.yml`).
`ideae --version` names the release.
