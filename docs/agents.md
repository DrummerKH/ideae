# Agents

Each task runs an agent in its agent tab: a program in a terminal, as you
would run it yourself. Which program, with which flags and environment, is
an agent **profile** (Settings → Agents). A fresh install has one, Claude
Code (`claude`); with one profile nothing below shows and everything works
as it always did.

## Installed agents

On the first start the app looks for Claude Code (`claude`), Codex
(`codex`), OpenCode (`opencode`) and Gemini CLI (`gemini`, as any other
program) on PATH (the login shell's) and where their installers put them:
`~/.local/bin`, `~/.claude/local`, `~/.npm-global/bin`, `~/.bun/bin`,
`~/.opencode/bin`, `~/.volta/bin`, nvm's and pnpm's folders, `/usr/local/bin`,
`/opt/homebrew/bin`; on Windows `%APPDATA%\npm`, `%USERPROFILE%\.local\bin`,
pnpm, Scoop, WinGet, Volta. Claude Code alone, and the default: nothing is
said. Others found: a card offers to add them as profiles (the default stays
unless you tick it). None: a card says where to get one, and that tasks open
with a shell until then. "Don't show again", or Add, ends it; it never shows
while presenting. Settings → Agents → **Find installed agents** looks again
and adds what it finds to the list.

## Profiles

A profile has a name, a kind (Claude Code, Codex, OpenCode, or any other
program), a command (the program and its flags: `claude --model opus`) and
variables for the agent alone. Two accounts of one agent are two profiles
with their own config folder:

| Agent | Variable for a second account |
| --- | --- |
| Claude Code | `CLAUDE_CONFIG_DIR=~/.claude-work` |
| Codex | `CODEX_HOME=~/.codex-work` |
| OpenCode | `XDG_DATA_HOME=~/.local/share/opencode-work` |

One profile is the default. A project can have its own (its menu in the
sidebar: "New tasks: …"); it is kept in the app's state, not in
`.ideae.toml`, since whose subscription runs is yours alone. The new-task
dialog picks the project's, and lets you choose another; each variant of a
task can have its own, next to its own line of the prompt.

With more than one profile, a task's row and its agent tab show the agent's
letter (C, X, O, or the first letter of the profile's name). A profile can
have a mark of its own instead (a letter or a few: W, Me) and a faint color
(the profile editor's Mark; a click on it changes the color), so two
accounts of one agent tell apart at a glance.

## Sessions

The app keeps each task's agent sessions: which profile ran, its session id,
when it started. A new task's Claude Code starts under an id the app chooses
(`--session-id`), and a restart, or the next run of the app, resumes exactly
that session (`--resume`). An id the agent reports itself (after `/clear`, or
Codex's and OpenCode's own) replaces the one known. A task from before ids
were kept continues the latest conversation in its folder, as it did, and
its id is learned then. A `--continue`, `--resume` or `--session-id` in your
own command still wins.

**Continue with another agent…** (a task's menu, with more than one profile)
stops the task's agent and starts another in the same worktree, in a new
session. It is first told the task, what the agent before it said last, and
that `git log` and `git diff` show what was done. The earlier session stays
recorded with the task.

An archived task keeps its sessions' files (Claude Code's transcript and its
subagents', Codex's rollout); a restore puts them back for the new worktree
and resumes the latest session by its id, else starts a new one told a
summary (see [features](features.md#pause-archive-restore)).

## Chat agents (ACP)

A profile can run as a **chat** instead of a terminal (the profile editor's
"Runs as"): its command starts the agent's Agent Client Protocol server,
`opencode acp`, `gemini --acp`, `kimi acp`, `qwen --acp`, or an adapter such
as `npx -y @agentclientprotocol/codex-acp`. The agent tab then shows the
conversation: its messages (markdown), its thinking folded, each tool call
with its diff and output, its plan, and a prompt (Enter sends, Shift+Enter a
new line; prompts sent while it works wait their turn, and one can be taken
back to change, or all dropped).

**Adding one.** Settings → Agents → **Browse ACP agents…** lists the
agents of the ACP registry by how they would run here: installed already,
through `npx` or `uvx`, or downloaded (checked against the registry's
sha256 and unpacked into the app's data folder). The one chosen opens in
the profile editor, filled in. **Find installed agents** adds Gemini CLI,
Kimi Code, Qwen Code, Goose, Cursor, Copilot and Mistral Vibe as chats.

**Choices.** What the agent lets be chosen (model, effort, mode, and
whatever else it offers) is a row of menus under the prompt; a long list of
models is searched, grouped by provider. What you choose is what the
agent's new sessions start with: the profile editor shows it ("Starts
with") and forgets it. The new-task dialog, and each variant, can start a
task with a model of its own.

**Permissions.** A tool call that asks shows the agent's answers on its
card, with the change it would make when the agent sends it first; the
task's row and the notification say what is asked. An agent with an "auto"
mode of its own (Claude Code, Codex, Qwen, Kimi) has it in its mode menu.
One without gets **Permissions** under the chat:

- *Ask me*: every question waits for you;
- *ideae decides*: reading in the worktree, changes inside it and commands
  that only look, build or test (git status/diff/log, cargo/npm/go/make
  test and build, grep, ls…) are allowed; the network, deleting, pushing,
  paths outside (a link out of the worktree too), a read that names no
  file, and commands it does not know are asked. So is a known command
  given a way to write elsewhere or run another (`&`, `env cmd`,
  `sort -o`, `sed w`, `rg --pre`, `git diff --output`, `cargo --config`,
  `npx` of a package that is not the tool asked for…), and so is any word
  the shell would expand (`$HOME`, `~`). It is no sandbox: a build or a
  test runs the project's own code (`build.rs`, a `Makefile`, `conftest.py`),
  which the agent may have changed;
- *Allow all*.

Each card says who allowed it. OpenCode, which asks nothing by itself, is
made to ask, and the rules answer it. A variant left to work answers yes.

**Commands** the agent runs through the app (ACP's terminals; Claude Code's
adapter runs its Bash this way) show their output live on their card, and
end with the agent (killed when one ignores being asked to).

**Around the chat.** **@** mentions a file of the task: it goes to the agent
as a link to it. **Follow** opens the files the agent read or changed
beside the chat, at their line. An image pasted (Ctrl+V) goes with the next
prompt, for agents that take them. The plan of the turn stays above the
prompt until it is done. `/` lists the agent's own commands (Tab
completes). A message has **Copy** on hover.

**Checkpoints.** Before each prompt the task's files are kept as a git
tree (untracked files too, not ignored ones nor any over 16 MB; the index
is not touched). Hovering a prompt shows **↺ Files before this**: what
would change is listed first, then the files go back, and a note in the
chat has **Undo**. Only the files change: the conversation and what is
staged stay, and the agent is not told. They are kept by refs under
`refs/ideae/checkpoints/`, the last 50 of a chat, and dropped with its
worktree or when another agent takes the task.

**Review in Changes** on a tool call's diff opens that change in the
Changes tab beside the chat, where it is staged or reverted whole or in
parts.

**ideae's tools.** A chat agent is given ideae as an MCP server
(`ideae --mcp`), for its own task alone: `open_files` (the files open in
the editor, the one in front with its cursor and selection, those not
saved), `diagnostics` (what the language servers report for a file of
the worktree or for the open ones) and `run_check` (the project's check,
run as after a turn, the user's trust of a `.ideae.toml` asked first). It
asks the window over a socket of the app, with a token made for its
worktree: another task's agent cannot ask about this one. With no window
open it says so.

**Keys.** When it asks, with the prompt empty: **Y** allows, **A** allows
always, **N** declines (the letters are on the buttons). **Esc** stops its
turn. **↑** in an empty prompt brings back the last one. **Ctrl+F** looks
through the conversation (Enter, Shift+Enter go through what it found).

**Status** is what the agent says, not guessed: working, asking, done,
ended, as Claude Code's hooks tell it for a terminal agent. The agent reads
a file open with unsaved edits as it is in the editor.

**Continue with another agent** between a chat and a terminal of the same
agent (Claude Code, OpenCode, with the same config folder) goes on with
the same session: the conversation is the agent's own, shown again in the
chat. If it cannot be loaded, the new session is told where the work
stands, as for another agent.

**Sessions.** The app keeps the conversation (in `acp/` next to its state),
so a task shows it again after a restart without the agent's replay, and
resumes the agent's session when it can load one; an archive keeps it too.
A chat agent with nothing to do, in a task out of view for a while
(Settings → Agents → Chat agents; 30 minutes unless set, 0 for never), is
stopped to free its memory, and starts again with its conversation when the
task is shown.

**Away from the window.** A chat agent goes on working when the window
closes; a question it asks then is a desktop notification, and waits for
you, or is declined after a time set (Settings → Agents → Chat agents).
Its **Log** (under the prompt) is what it wrote on its error stream. A
context nearly full shows in the warning's color. An agent left running
through an update of the app may not answer what came with it: the chat
says so, with a Restart.

**Cost.** A chat profile can have a **daily limit** in dollars: once its
sessions cost that much today (UTC), as the agent reports its cost
(Claude Code's adapter does), the turn stops and no prompt is sent until
the limit is raised or the day ends. It is counted in `spend.json` next to
the state, by what each session grew, so a run with no window open counts.

**When it goes wrong.** An agent that ends in the middle of its turn is
started again once a minute at most, going on with its session; the chat
says why it ended. One whose program is not there says what it needs
(Node.js for `npx`, uv for `uvx`); one that does not answer in 3 minutes is
stopped (the first start through `npx` or `uvx` downloads it, and says
so). The kept conversation is replaced whole on disk, never half-written,
and written anew when it grows.

The agent runs under a small host process of the app (`ideae --acp-host`),
which speaks ACP with it and keeps the thread; with the daemon, the host is
one of its sessions, so the agent goes on working when the window closes,
and a daemon of a newer build takes it over without stopping it. Without
the daemon (Windows) the host is a thread of the window.

**Claude Code as a chat** runs Anthropic's adapter with
`--hide-claude-auth`: an app may not offer a claude.ai plan's login to an
agent built on the Agent SDK, so it runs on an API key (the profile
editor's **API key**, kept sealed as `ANTHROPIC_API_KEY`, or a gateway).
Codex's adapter and Gemini get the same field (`OPENAI_API_KEY`,
`GEMINI_API_KEY`). A Claude plan is used
by the terminal profile, as before.

## Keys

A profile's variables whose names say they are secrets (`…_KEY`,
`…_TOKEN`, `…SECRET…`, `…PASSWORD…`) are not kept in the state: their
values go to `secrets.json` next to it, readable by you alone, each sealed
(ChaCha20-Poly1305) with a key made from this machine's id, your user and a
random salt. A copy of the file elsewhere opens nothing; on this machine it
is no vault, which no store is against an agent handed the key anyway. No
keyring is asked, so no password dialog shows. The profile editor shows
them masked; leave the mask to keep one. A value `$(pass show anthropic)`
is a command run when the agent starts, for a password manager.

## Quick questions

Commit messages ("✦ Write") and the variants' judge ask one question and
want one answer. Settings → Agents → "Agent for quick questions" says which
profile answers (the default one unless chosen): `claude -p`, `codex exec`
or `opencode run`. Any other program cannot be asked that way.

## What each kind can do

| | Claude Code | Codex | OpenCode | Any other |
| --- | --- | --- | --- | --- |
| Status | hooks (`--settings`) | turn end by `notify`; approvals as terminal notifications; work guessed | plugin events | guessed |
| Session id chosen by the app | yes | no, learned | no, learned | — |
| Resume | `--resume <id>` | `codex resume <id>` | `--session <id>` | — |
| Variants run unasked | `--permission-mode auto` | `--sandbox workspace-write --ask-for-approval never` | `--auto` | as typed |
| Tokens and context | transcript | rollout file (`$CODEX_HOME/sessions`) | not read (SQLite) | — |
| One question | `claude -p` | `codex exec -` | `opencode run` | — |
| Archived sessions | transcript and subagents | rollout file | its database, left as it is | — |

Nothing of an agent's own configuration is written. Claude Code gets its
hooks from a `--settings` file the app writes in its runtime folder; Codex
its `notify` through `-c` for the session; OpenCode a plugin file in the
same folder, added with `OPENCODE_CONFIG_CONTENT` (a profile that sets that
variable itself keeps it, and then reports nothing). The one thing put in
an agent's own folder is a restored task's session files, where it keeps
its sessions (a new file next to the others, none replaced).

**Codex and OpenCode have not been tried live**: their adapters follow their
source code (openai/codex `codex-rs/`, anomalyco/opencode `packages/`), and
the tests run stand-ins that behave as that code says. The profile editor
says so.

### Guessed status

An agent that reports nothing (or, like Codex, only a turn's end) is watched
in its terminal: writing for two seconds with nobody typing is work, five
quiet seconds after that is done, a bell or a desktop notification it asks
for (OSC 9 / 777) is done, or a question when its words say so. The row's
tooltip says when a status is guessed. Text for such an agent's prompt goes
in once its terminal has been quiet a moment after it started.
