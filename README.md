# ideae

An IDE for the agentic age: one window for running AI coding agents
(Claude Code by default), one task per git worktree.

Every task gets the agent in a terminal, more terminals, an editor, the
diff of its changes, git history, and a headless browser tab, and keeps
all of it while you switch between tasks.

Status: early. Linux (Ubuntu 24.04 or newer is what it is tested on); a
Windows build exists and is being tried out (see [Windows](#windows)).

## What it does

- **Projects and tasks.** Add a repository, start a task from a title. The
  title becomes a branch (`DEV-123 Fix login` gives `DEV-123_fixLogin`, any
  other text a slug with a random suffix). The task gets a worktree in
  `<repo>.worktrees/<branch>`, branched from the remote's default branch,
  with the main checkout's ignored `.env*` files copied in. Worktrees made
  outside the app show up as tasks too.
- **Agent status.** Claude Code runs with hooks passed through
  `--settings`. Nothing is written to `~/.claude` or the repository. The
  sidebar shows working / needs input / done, and a desktop notification
  fires when a task out of view wants you.
- **State.** Projects, tasks, open tabs, cursor lines and open folders
  survive a restart (`~/.local/share/ideae/state.json`).
- **Editor.** Tree-sitter highlighting, go to definition, usages and
  implementations (LSP when the language server is installed, else a
  search-based fallback; a missing server can be installed from the editor
  header into `~/.local/share/ideae/lsp`), file structure, blame of the cursor
  line, syntax-aware selection, autosave when you leave a file.
- **Git.** Changes with stage/unstage/discard, commit, push, PR (`gh`), the
  branch against its base, history, safe pull.
- **Terminals.** Built on libghostty-vt. Ctrl+click on paths, `file:line`,
  class and function names opens them in the editor.
- **Browser tabs.** Headless Chrome, streamed into a tab.

## Services

Describe what a project runs while you work in `.ideae.toml` at its root,
and start it per task from Run ▾ in the branch bar:

```toml
[[service]]
name = "web"
command = "npm run dev -- --port {port}"
port = 3000
url = "http://localhost:{port}"
env = { PORT = "{port}" }
```

Each task has a port slot (0 for the main checkout, 1, 2… for worktrees)
and `{port}` is `port + 100 × slot`, so the same service runs in several
tasks at once, provided the program takes its port from the environment or
its arguments. A task's terminals, the agent's included, get
`IDEAE_PORT_<NAME>`. A service runs in its own terminal tab and survives a
window restart; a taken port is reported with the process holding it.

## macOS

Releases carry an Apple Silicon build: `ideae-<tag>-macos-arm64.app.zip`.
It is signed ad hoc, without an Apple Developer ID, so the first start
needs right click → Open (or `xattr -d com.apple.quarantine ideae.app`).
Shortcuts use Cmd where Linux uses Ctrl. Not there yet on macOS: the
process monitor in the title bar, clicks on notifications, the Kotlin
language server install.

## Updates

Release builds ask the GitHub API for this repository's latest release at
start and every six hours (Settings → Check for updates turns that off).
A newer one shows "↑ update" in the title bar: it downloads that release's
binary next to the running one, puts it in its place and restarts; on
Linux the agents keep running through the restart.

## Build

Requirements:

- Rust (stable, edition 2024)
- Zig 0.17.x (builds libghostty-vt)
- System libraries:
  `libxkbcommon-dev libxkbcommon-x11-dev libxcb-xkb-dev libx11-xcb-dev libwayland-dev libvulkan-dev libfontconfig-dev libfreetype-dev libasound2-dev`

```bash
cargo build --release
./target/release/ideae [REPO...]
```

Options: `--agent CMD` or `--no-agent`, `--theme auto|light|dark`.

A debug build is a separate app (`ideae-dev`): its own state, sockets and
launcher entry, so it can run next to a release build.

### Portable binary

To run on older glibc, build against glibc 2.31 with
[cargo-zigbuild](https://github.com/rust-cross/cargo-zigbuild):

```bash
CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUSTFLAGS="-C link-arg=-Wl,--allow-shlib-undefined" \
  cargo zigbuild --release --target x86_64-unknown-linux-gnu.2.31
```

### Releases

Every push to `main` builds a release tagged with its UTC time
(`yyyymmddhhmmss`): a Linux binary (`chmod +x` it after download) and a
Windows `.exe` (`.github/workflows/release.yml`). `ideae --version` names the release.

### Windows

Windows 10 1903 or newer, x64. The `.exe` is self-contained: it needs only
system DLLs. Terminals run PowerShell (`pwsh` when installed). Git must be
on PATH; Claude Code on Windows brings Git Bash, which also runs the agent
hooks. The headless browser tab uses Chrome, or Edge when there is no
Chrome.

State and language servers live in `%LOCALAPPDATA%\ideae`.

Not there yet on Windows: the CPU/RAM readout in the title bar, pinning
to a launcher, the Kotlin language server install.

To build on Windows: Rust (MSVC), Zig 0.17.x and the Windows SDK (gpui
compiles its shaders with `fxc.exe`; point `GPUI_FXC_PATH` at it when it is
not found), then `cargo build --release`.

A debug build cross-compiles on Linux with
`cargo build --target x86_64-pc-windows-gnu` (needs `mingw-w64`), enough to
check that it compiles and to run the tests under Wine. Such a build reads
its shaders from this checkout at run time, so it cannot be handed out, and
a release build needs a Windows host for `fxc.exe`.

## Keys

| Key | Action |
|---|---|
| Ctrl+Shift+T | New task |
| Ctrl+1 / Ctrl+2 / Ctrl+3 / Ctrl+\` | Agent / Changes / History / shell |
| Ctrl+Shift+N, Ctrl+Shift+F | Go to file, find in files |
| Ctrl+B / Ctrl+click | Go to definition; on a declaration, its usages |
| Ctrl+Alt+B | Implementations, then usages |
| Ctrl+F12 | File structure |
| Alt+1 / Alt+2 / Alt+3 | Changes / Files / Structure panel |
| Shift+Esc | Focus mode |

## Third-party code

`vendor/` holds patched copies of `gpui-component`, `gpui-base` and
`gpui-cn` (Apache-2.0, licenses kept in each directory). Changed places are
marked `ideae patch`.

`vendor/tree-sitter-twig` is the generated parser of
[tree-sitter-twig](https://github.com/gbprod/tree-sitter-twig) (MIT, license
kept beside it). `queries/julia-highlights.scm` and
`queries/solidity-highlights.scm` come from tree-sitter-julia and
tree-sitter-solidity (MIT); the Solidity one has a pattern rewritten for this
tree-sitter version.
