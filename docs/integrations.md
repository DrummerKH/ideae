# Settings in layers, trackers and code hosts

## Layers

A setting is set in Settings (Global), in a space, in a repository's
`.ideae.toml` (the team's), or in a project on this machine; the most
specific layer that sets it wins. Settings has a switch at the top,
**Settings for: Global / Space: … / Project: …**: on a space or a project it
shows only what that layer may set, each value with where it comes from
("from space Work", "from Settings") and ↺ where it is set there. On Global,
a setting a space or a project sets otherwise says who does.

| Setting | Global | Space | Team (`.ideae.toml`) | Project |
|---|---|---|---|---|
| Default agent | ✓ | ✓ | | ✓ |
| Worktrees folder (one per project in it) | ✓ | ✓ | | |
| Worktrees in (this project's own folder) | | | | ✓ |
| New tasks from (base branch) | | | ✓ | ✓ |
| Pull request status | ✓ | ✓ | | ✓ |
| Language servers | | ✓ | | ✓ |
| Failed check back to the agent | | ✓ | | ✓ |
| Branch of a ticket's task (`{key}_{summary}`) | ✓ | ✓ | ✓ | ✓ |
| Pull request's title (`{key} {title}`) | ✓ | ✓ | ✓ | ✓ |
| Ticket in a pull request (fixes, refs, none) | ✓ | ✓ | ✓ | ✓ |
| Integrations (a list, by id) | ✓ | ✓ | ✓ | ✓ |

Whose agent runs, where worktrees go and anything secret are never the
team's: a `.ideae.toml` that sets them is not listened to there.

A list of integrations is put together from all the layers: a layer adds an
entry by a new id (with its kind and address), changes one by its id, or
turns it off (`disabled`). A project in no space has Settings and its own
alone.

Spaces are found by an id: renaming one keeps its projects and settings.
Removing one that has settings of its own asks whether they move to
Settings or go with it. A project moved to another space is told what
changes for it.

`state.json` keeps the fields older versions read (a project's `agent`,
`base`, `pr_off`…, a space by its name): an older version reads what a newer
one wrote, and what it changes there is taken back.

## Integrations

**Settings → Integrations** (and a space's or a project's page): each is a
kind (Jira, Linear, YouTrack, GitHub, GitLab), an address and the prefixes
of keys it owns (`DEV`, `OPS`), or for GitHub and GitLab issues the
repositories (`owner/repo`, `group/sub/project`). A key's tracker is the one
that owns its prefix, the most specific layer's when two do: nothing is
guessed from what was pasted before (one `DEV` may be two companies' Jira).

A team can write one into its repository:

```toml
[project]
base = "develop"

[integrations.acme]
kind = "jira"
url = "https://example.atlassian.net"
keys = ["DEV", "OPS"]
```

ideae reads these from the main checkout, not a task's worktree, where an
agent writes; and writes the file only when you choose "In the team's
.ideae.toml". An integration a team's file names alone only makes links: it
never decides which host ideae asks anything of.

Without any integration, links of Jira Cloud (`…atlassian.net/browse/DEV-1`,
`selectedIssue=`), Linear, and GitHub and GitLab issues are still known. A
bare key no integration owns only looks like a ticket: its mark is grey,
without a link.

A task made from a tracker's link that no integration knows offers, once a
host, to add one: for this project, its space, everywhere, or in the team's
file. It never stands in the way of the task.

**Settings → Integrations → Tickets** turns all of it off: no marks, no
questions, no `IDEAE_TICKET` variables; a team's integrations stay silent.

## Reading tickets with your token

An integration's address is all it takes to make links. To read a ticket's
title and status, give it a token of yours in its dialog (Settings →
Integrations → Edit…):

| Tracker | Token |
|---|---|
| Jira Cloud | your email as the account, an API token (id.atlassian.com → Security → API tokens; no admin) |
| Jira Server/Data Center | a personal access token, no account |
| Linear | a personal API key |
| YouTrack | a permanent token |
| GitHub, GitLab | none: gh and glab, with your sign-in there |

A token is kept in the system's keyring (the macOS Keychain, Windows
Credential Manager, the Secret Service on Linux) by host and account: two
spaces of one Jira share it, two companies' Jiras never do. The keyring is
asked when the token is first used, not at the start. With no keyring (Linux
without a desktop session) it is sealed in the app's own store instead, and
you are told that this is weaker. A debug build (`ideae-dev`) keeps its own.

A token is for the address it was given for. An integration of a team's
`.ideae.toml` reads with your token only when you connect it (**Change
here…** on it, a token given): its address changed in the file since, it
pauses until you give the token again. One not connected asks nothing of
the network: no check, no icon, no server information.

With one connected:

- **New task**: a key typed reads its ticket once typing pauses: its title
  names the task and its branch at once, and its words go into the prompt,
  framed as the ticket's (you see it, and change it, before it is sent);
- a task made from a ticket gets its title and status on its mark (in its
  tooltip); a ticket moved (DEV-5 is OPS-9 now) is followed; one not found
  is marked **not found** (a mistyped key);
- a request merged while its ticket is still open says so.

A token refused is told once a run; a tracker that asks to slow down is
left alone for five minutes. Nothing of it runs on the window's thread, and
tickets are not polled: read when a task is made and when its request is
merged.

### An agent reading its task's tickets

An integration with a token can let agents read (its dialog: **A task's
agent may read its tickets**; off unless you turn it on, never by a team's
file). A task's agent then has ideae's MCP tool `ticket`: its own task's
tickets only (no other key, no search, no other project), read with your
token by the window (the token never reaches the agent or a remote host),
its text framed as the ticket's and not instructions, bounded, without
attachments. Nothing written to the tracker: no transitions, no comments.
Agents of tasks on another machine have no ideae MCP server, so no `ticket`
there; their terminals still get `IDEAE_TICKET*`.

## A task's links

A task keeps its tickets, its pull request and every address its title and
prompt had (an error's page, a thread). The first ticket is its own:

- its key is a mark on the task's row; a click opens it in the system's
  browser (never the task's own Chrome, whose sessions are its agent's);
  `DEV-1 +1` when it has more;
- the task's menu and the palette: **Open DEV-1**, **Copy Ticket Link**,
  **Copy Ticket Key**, **Add a Link…**;
- the task list's search finds a task by its ticket's key;
- its terminals and agent get `IDEAE_TICKET` (`DEV-1`), `IDEAE_TICKET_URL`,
  `IDEAE_TICKET_URLS`, `IDEAE_TICKET_KIND` (`jira`) and `IDEAE_PR_URL`: ids
  and addresses, never a ticket's words.

A key typed or pasted in **New task** that a task has already offers
**Switch to it**, or **Restore…** for an archived one.

In a chat agent's messages, the keys an integration owns are links to
their tickets (not in code, nor a link already), made once as the text comes.

**Copy today's tasks** (the palette): the tasks worked on today, as
Markdown for a stand-up, by ticket, each with its request's state: from what
ideae knows, nothing asked of a tracker.

The ticket's mark tells, in its tooltip, why its link goes where it does:
the integration that owns its key, and the layer that set it.

Tasks of before links get theirs once: the addresses of their prompt and
the keys of their branch an integration owns.

## The key where trackers look

A tracker links a ticket to a branch, a commit or a pull request by its key
in them, when its admin connected the code host (GitHub for Atlassian,
GitLab for Jira, Linear's integration). ideae puts the key there:

- **the branch**: a title that starts with a key names it by the template
  (`{key}`, `{summary}` camelCase words, `{slug}` dashed words, `{rand}`);
  the task's title, named later from its agent's first turn, leaves the
  branch as it is;
- **the request's title** by its template, `{key} {title}`, unless the
  commits' title has the key already;
- **its description**: before it is made, **Fixes** (the tracker may close
  the ticket when it is merged), **Refs** or nothing, for each of its tickets;
- **commits**, where the project's recent commits carry keys (most of the
  last fifty): the commit box starts with the key, a written message gets
  it, and the agent of a ticket's task is told to start each subject with it.
  No hook: nothing changes how git commits from a terminal.

A task's menu has **New Task for This Ticket…**: the ticket's key and links,
its agent told the earlier branch and request.

A request's link pasted as a title (`…/pull/12`, `…/-/merge_requests/12`)
makes a task on its branch, fetched from where requests are; one from a
fork has nothing to push to. It is marked **review**, and its agent asked to
review it, not to commit.

The poll of requests tells, as it happens: a task's request was merged
(**Archive task**, **Open ticket**), its CI failed or new review comments
came while the task was out of view.

### From outside the window

`ideae ticket DEV-123`, or a link `ideae://ticket/DEV-123`
(`ideae://ticket?key=group/app%2345` for an issue; `ideae-dev://` for a
debug build): the ticket's task comes to the front, or **New task** opens
with its key in the project whose integration owns it. The running window
takes it; none running, the app starts with it. `ideae --install` registers
the scheme (a launcher entry on Linux, the user's classes on Windows; the
.app's Info.plist on macOS). A bookmarklet makes one from a ticket's page:

```js
javascript:location.href='ideae://ticket/'+location.pathname.split('/').pop()
```

**Copy a ticket's branch name…** in the palette gives the branch a task
would have, without making one. A ticket's link copied before **New task**
is offered there: **Create from DEV-…**.

## Code hosts

A task's pull request (GitHub) or merge request (GitLab) is asked of its
code host from this machine, with your sign-in there: `gh auth login`, or
`glab auth login --hostname gitlab.example.com` for a server of your own.
Which one is told by the remote's host: an integration of yours naming it,
else `gitlab.com` and `gitlab.*` are GitLab, the rest GitHub (gh knows the
Enterprise hosts it is signed in to). A remote project's repository is read
there and asked of here; nothing of your sign-in goes to its host.

A fork: requests and issues are asked of `upstream` when there is one, else
`origin`; a request is made from origin's branch into upstream.

GitLab does what GitHub does: a merge request's state, draft, pipeline and
unresolved discussions on the task's row; its review comments to the agent;
a failed pipeline's job logs to the agent (CI ✗ → agent); creating it;
merged branches to delete; `#12` in New task reads the issue. Projects in
groups of groups (`group/sub/project`) are found by their whole path.
