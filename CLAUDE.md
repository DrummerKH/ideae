# CLAUDE.md

Context for AI coding agents working on ideae. Human docs: [README.md](README.md).

## Layout

One Rust binary on GPUI (`gpui-kit`), Linux first, Windows too.

```
src/main.rs        CLI, key bindings, title bar, window
src/workspace.rs   projects and tasks: load, switch, create/close/delete, state, hooks
src/sidebar.rs     left sidebar (inbox, projects, tasks, drag and drop)
src/task.rs        one task: tabs, panes, editor, terminals, changes, git UI, session save/restore
src/state.rs       state.json; APP_ID (ideae vs ideae-dev)
src/agent.rs       Claude Code hooks bridge (unix socket, `ideae --hook`)
src/git.rs         git CLI wrappers, worktrees
src/lsp.rs         language server client
src/lsp_install.rs language servers installed on request, ~/.local/share/ideae/lsp
src/procs.rs       starting helper programs (no console on Windows), ending child sessions on quit
src/daemon/        terminals in a daemon (`ideae --daemon`, unix): they survive a window restart; Exit ends them
src/desktop.rs     launcher entry, dash pin, restart
src/naming.rs      branch names from task titles
vendor/            patched gpui-component, gpui-base, gpui-cn; changes marked `ideae patch`
scripts/           Xvfb driver for UI checks
```

## Build and test

- Zig 0.17.x must be on PATH (libghostty-vt). System -dev libraries: see README.
- `cargo build`, `cargo test`. LSP tests are `#[ignore]` and need gopls / rust-analyzer.
- A debug build is the separate app `ideae-dev`: own state, sockets, window class, launcher. Keep that split for anything new that has an identity on the system.

## Checking UI changes

Run the real app headless and look at screenshots. Other sessions use the
driver too: pick your own display with `IDEAE_DISPLAY=<n>` (default 77).

```bash
IDEAE_STATE=/tmp/x/state.json scripts/drive.sh /tmp/x/run.log 7 \
  "move:73,57 click wait:0.7 shot:/tmp/x/menu.png" -- --no-agent /path/to/test/repo
```

- Always set `IDEAE_STATE` to a scratch file; the real state belongs to the user's running app. It also gives the run its own daemon (socket named after the state file), so a test never attaches to or prunes the user's agents. `IDEAE_NO_DAEMON=1` keeps terminals in the window.
- Stop test processes by PID, never `pkill -f <pattern>`: patterns match other sessions' shells and the user's own app.
- The driver gives the app no D-Bus session. Never run the app with the user's session bus in tests: it can change their GNOME dash or send notifications.
- Use a throwaway git repo for task create/delete tests, never a real project.
- Debug aids: `IDEAE_LSP_DIR` (where servers get installed), `IDEAE_LSP_IDLE_SECS` (how long a task out of view keeps its language servers, default 900), `IDEAE_OPEN=path:line`, `IDEAE_URL=url`, `IDEAE_TAB=n`, `IDEAE_DEBUG`.
- Before finishing, make sure no Xvfb, test app, language server or test agent you started is still running. The user's own `ideae` instance (DISPLAY=:0) is not yours.

## Rules

- Windows: start helper programs with `procs::command` (it hides the console
  window and finds `npm.cmd` and the like), and programs that must die with
  the app with `procs::spawn_tied`. Check the Windows side compiles with
  `cargo check --target x86_64-pc-windows-gnu --target-dir target/win`.
- The repository is public: no personal paths, hostnames, emails or private project names in code, tests or commits.
- Agents and terminals belong to tasks; whatever a task starts must end on Close, Delete and quit.
- No comments unless the why is non-obvious.
