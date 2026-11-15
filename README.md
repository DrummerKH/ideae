# ideae

An IDE for the agentic age: one window for running AI coding agents
(Claude Code by default), one task per git worktree.

Every task gets the agent in a terminal, more terminals, an editor, the
diff of its changes, git history, and a headless browser tab, and keeps
all of it while you switch between tasks.

Status: early, Linux only (Ubuntu 24.04 or newer is what it is tested on).

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
  search-based fallback), file structure, blame of the cursor
  line, syntax-aware selection, autosave when you leave a file.
- **Git.** Changes with stage/unstage/discard, commit, push, PR (`gh`), the
  branch against its base, history, safe pull.
- **Terminals.** Built on libghostty-vt. Ctrl+click on paths, `file:line`,
  class and function names opens them in the editor.
- **Browser tabs.** Headless Chrome, streamed into a tab.

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
