# Project settings: setup, teardown, check, services

What a project does for each of its tasks, besides starting the agent, is
set in **Project settings…** (right click on the project in the sidebar, or
the Run panel, Alt+4) and kept in `.ideae.toml` at the repository root:

```toml
[task]
setup = ["npm ci"]                   # in a new task's worktree, next to the agent
teardown = ["docker compose down"]   # when the task is paused or archived
check = "npm test"                   # when the agent finishes a turn
copy = [".env", "config/*.local"]    # ignored files a new worktree gets

[[service]]
name = "web"
command = "npm run dev -- --port {port}"
port = 3000
url = "http://localhost:{port}"
env = { PORT = "{port}" }
```

The window suggests values from the files the project has (a lock file
gives the install command, `Cargo.toml` or `go.mod` the tests, an
`.env.example` the `.env` to copy); they are shown greyed and taken only with
**Use suggestions**. **Ask the agent to fill this in** has the main
checkout's agent write the file, for you to review as any change.

## Where the settings are

- **`.ideae.toml`**, committed, is the team's. A task reads the one in its own
  worktree, and the main checkout's when its worktree has none (a file not
  committed yet).
- **Only for me** keeps the same settings in the app's state instead,
  nothing written to the repository. When there are settings of your own,
  they are used in place of the file, all of them, services included; turn
  the switch off to go back to the file.

## Setup

Runs in a terminal of the new task, `⚙ setup`, while its agent starts: the
agent is not held up. The commands run in turn, the first that fails ends
it. A failed setup marks the task (`✗ setup` in the sidebar and the branch
bar) and says so in a notification; its terminal keeps the output, and the
Run panel runs it again. A setup that went well closes its terminal.

## Teardown

Runs when a task is paused or archived, in its worktree, for a minute at
most; Archive removes the worktree after it. Quitting the app (Exit, Ctrl+Q)
runs the teardown of every running task at once, for ten seconds at most.
Closing the window with × leaves the tasks running, and runs none.

## Check

Runs in the task's worktree when its agent finishes a turn, in the
background: one at a time for each task (a new turn stops the run before),
and not again when nothing changed in the worktree since the last one. The
sidebar shows `✓ check` or `✗ check`; the Run panel has the output, and on a
failure **Send to agent** gives the agent what it needs to fix it: the
errors and failures found in the output (Rust, Go, Jest, pytest, a
compiler's `file:line: error`), each once with the lines that go with it,
and the output's end; with none found, its last 80 lines. The Run panel's
**Run** runs it whenever you like. A failed CI job's log goes to the agent
the same way.

With the box under Check in Project settings (kept on this machine, not in
the file: the agent's turns are yours to spend), a check that fails after the
agent's turn goes back to it by itself, marked as sent by ideae, as **Send to agent**
would, that many times in a row; then a notification says it still fails and
it is your turn. A turn you start, or a check that passes, starts the count
again. Nothing is sent while you are typing to the agent or while it asks
you something, and a check run from **Run** or by the agent itself is never
sent.

When a task's pull request has a failed CI run, **CI ✗ → agent** in the
branch bar sends the end of the failed job's log (`gh run view --log-failed`)
to the agent.

## Trust

`.ideae.toml` comes with the repository, and whoever can commit to it can
change it. The first time its setup, teardown or check would run, the app
shows the commands and asks; the answer is kept for the project and that
version of them, and a changed command asks again. Your own settings, and a
file saved from the window, are not asked about. Services start only on a
click that shows their command, so they are not asked about either.

## Services

Each task has a port slot (0 for the main checkout, 1, 2… for worktrees) and
`{port}` is `port + 100 × slot`, so the same service runs in several tasks at
once, provided the program takes its port from the environment or its
arguments. A task's terminals, the agent's included, get `IDEAE_PORT_<NAME>`.
`{port.NAME}` is another service's port.

Start them from Run ▾ in the branch bar or the Run panel. A service runs in
its own terminal tab and survives a window restart; a port that is taken is
reported with the process holding it.

## Files to copy

Without a `copy` list, a new worktree gets the main checkout's ignored
`.env*` files, as Settings say. A list names the ignored files to copy
instead, as git globs from the repository root (`.env`, `**/.env.local`).
