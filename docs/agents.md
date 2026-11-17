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
letter (C, X, O, or the first letter of the profile's name).

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
