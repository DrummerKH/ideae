//! `ideae --hook <Event>`: what an agent's hook tells, read the same by
//! the app and by `ideae-server` on another machine, and sent to the daemon
//! there (or to the window's own socket).

use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::status::ASKS_USER;

pub const SOCK_ENV: &str = "IDEAE_SOCK";
pub const TASK_ENV: &str = "IDEAE_TASK";
pub const AGENT_ENV: &str = "IDEAE_AGENT";

/// Tools that wait for the user: a question, a plan to approve. Some
/// versions of Claude Code send no Notification for them.
pub const ASKING_TOOLS: &str = "AskUserQuestion|ExitPlanMode";

/// "Editing task.rs", "Running cargo test": a tool call in a few words.
pub fn activity(payload: &Value) -> Option<String> {
    let tool = payload["tool_name"].as_str()?;
    let input = &payload["tool_input"];
    let file = |key: &str| {
        input[key].as_str().map(|p| Path::new(p).file_name().map_or(p.to_string(), |n| n.to_string_lossy().into_owned()))
    };
    let short = |s: &str, n: usize| {
        let s = s.lines().next().unwrap_or_default().trim();
        if s.chars().count() > n { format!("{}…", s.chars().take(n).collect::<String>()) } else { s.to_string() }
    };
    Some(match tool {
        "Edit" | "MultiEdit" | "Write" | "NotebookEdit" => format!("Editing {}", file("file_path").or_else(|| file("notebook_path"))?),
        "Read" => format!("Reading {}", file("file_path")?),
        "Bash" => match input["description"].as_str().filter(|d| !d.is_empty()) {
            Some(description) => short(description, 60),
            None => format!("Running {}", short(input["command"].as_str()?, 50)),
        },
        "Grep" => format!("Searching \"{}\"", short(input["pattern"].as_str()?, 40)),
        "Glob" => format!("Finding {}", short(input["pattern"].as_str()?, 40)),
        "WebFetch" | "WebSearch" => "Browsing the web".into(),
        "Task" | "Agent" => format!("Delegating: {}", short(input["description"].as_str().unwrap_or("a subtask"), 50)),
        "TodoWrite" => "Planning".into(),
        other if other.starts_with("mcp__") => format!("Using {}", other.trim_start_matches("mcp__").replace("__", " ")),
        other => format!("Using {other}"),
    })
}

/// The folder a shell command makes a worktree in with `git worktree add`,
/// made absolute from where it runs (`cd`, `git -C`, the agent's folder).
/// Only Claude Code's Bash tool: a path in a variable or made by a script
/// is not known, and nothing is said.
pub fn worktree_added(payload: &Value) -> Option<String> {
    if payload["tool_name"].as_str()? != "Bash" {
        return None;
    }
    let command = payload["tool_input"]["command"].as_str()?;
    let mut here = PathBuf::from(payload["cwd"].as_str()?);
    for part in command.split(|c| matches!(c, '\n' | ';' | '|' | '&')) {
        let words = shell_words(part);
        let words: Vec<&str> = words.iter().map(String::as_str).collect();
        match words.as_slice() {
            ["cd", dir] => here = here.join(dir),
            ["git", rest @ ..] => {
                let mut at = here.clone();
                let mut rest = rest;
                while let ["-C", dir, more @ ..] = rest {
                    at = at.join(dir);
                    rest = more;
                }
                let ["worktree", "add", options @ ..] = rest else { continue };
                let mut options = options.iter();
                while let Some(word) = options.next() {
                    match *word {
                        "-b" | "-B" | "--reason" => {
                            options.next();
                        }
                        option if option.starts_with('-') => {}
                        path if path.contains('$') || path.contains('`') => return None,
                        path => return Some(as_git_lists(&normal(&at.join(path))).display().to_string()),
                    }
                }
            }
            _ => {}
        }
    }
    None
}

/// A folder about to be made, as `git worktree list` will name it: its parent
/// with links followed (`/var` is `/private/var` on macOS), else as it is.
pub fn as_git_lists(path: &Path) -> PathBuf {
    match (path.parent().and_then(|p| p.canonicalize().ok()), path.file_name()) {
        (Some(parent), Some(name)) => parent.join(name),
        _ => path.to_path_buf(),
    }
}

/// Words as a shell splits them, quotes taken off; enough for a command line.
fn shell_words(line: &str) -> Vec<String> {
    let (mut words, mut word, mut quote, mut any) = (Vec::new(), String::new(), None, false);
    for c in line.chars() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), c) => word.push(c),
            (None, '\'' | '"') => {
                quote = Some(c);
                any = true;
            }
            (None, c) if c.is_whitespace() => {
                if any || !word.is_empty() {
                    words.push(std::mem::take(&mut word));
                }
                any = false;
            }
            (None, c) => word.push(c),
        }
    }
    if any || !word.is_empty() {
        words.push(word);
    }
    words
}

/// `a/./b/../c` as `a/c`, without asking the disk: the folder may not be there yet.
fn normal(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for part in path.components() {
        match part {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

/// `Bash:1f…`: the tool and a print of its input, to tell the same call
/// made again.
fn call_print(payload: &Value) -> Option<String> {
    use std::hash::{Hash as _, Hasher as _};
    let tool = payload["tool_name"].as_str()?;
    // Waiting on purpose: output of a shell in the background read again, a pause.
    let polls = matches!(tool, "BashOutput" | "TaskOutput") || payload["tool_input"]["command"].as_str().is_some_and(|c| c.contains("sleep "));
    if polls {
        return None;
    }
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    payload["tool_input"].to_string().hash(&mut hasher);
    Some(format!("{tool}:{:016x}", hasher.finish()))
}

/// Whether a tool call waits for the user to answer.
pub fn asks_user(payload: &Value) -> bool {
    payload["tool_name"].as_str().is_some_and(|tool| ASKING_TOOLS.split('|').any(|t| t == tool))
}

/// What a tool that waits for the user asks: its question, or that a plan
/// waits for approval.
pub fn asking(payload: &Value) -> Option<String> {
    Some(match payload["tool_name"].as_str()? {
        "ExitPlanMode" => "Waits for you to approve its plan".into(),
        _ => {
            let question = payload["tool_input"]["questions"][0]["question"].as_str().map(str::trim).filter(|q| !q.is_empty());
            crate::said::cap(question.unwrap_or("Asks you a question"), 200)
        }
    })
}

/// The file a tool call writes to, as the agent names it.
pub fn edited_file(payload: &Value) -> Option<String> {
    let input = &payload["tool_input"];
    match payload["tool_name"].as_str()? {
        "Edit" | "MultiEdit" | "Write" => input["file_path"].as_str(),
        "NotebookEdit" => input["notebook_path"].as_str(),
        _ => None,
    }
    .map(str::to_string)
}

/// `ideae --hook <Event>`: run by Claude Code (the payload on stdin) or by
/// OpenCode's plugin, as `ideae --hook Codex --task=… --sock=… <json>` by
/// Codex's notify; forwards the event and exits. It must never block or
/// fail the agent, so every error is swallowed.
/// What is not for a daemon's socket (the window's own, with no daemon)
/// is returned: that socket and the line to send it.
pub fn run(args: &[String], log: Option<&Path>) -> Option<(String, String)> {
    let event = args.first().map(String::as_str).unwrap_or_default();
    let given = |name: &str| args.iter().find_map(|a| a.strip_prefix(&format!("--{name}="))).filter(|v| !v.is_empty()).map(str::to_string);
    let (event, payload) = if event == "Codex" {
        // Codex gives the JSON as the last argument, and only at a turn's end.
        match args.last().and_then(|json| codex_payload(json)) {
            Some(payload) => ("Stop", payload),
            None => return None,
        }
    } else {
        let mut input = String::new();
        let _ = std::io::stdin().read_to_string(&mut input);
        let payload = serde_json::from_str(&input).unwrap_or(Value::Null);
        match event.split_once(':') {
            Some((agent, theirs)) => foreign(agent, theirs, payload)?,
            None => (event, payload),
        }
    };
    let (Some(socket), Some(task)) = (given("sock").or_else(|| std::env::var(SOCK_ENV).ok()), given("task").or_else(|| std::env::var(TASK_ENV).ok())) else {
        return None;
    };
    let agent = given("agent").or_else(|| std::env::var(AGENT_ENV).ok()).filter(|a| !a.is_empty());
    let asks_user = event == "PreToolUse" && asks_user(&payload);
    // Before a tool runs the message says what the agent is doing.
    let message = match event {
        "PreToolUse" if asks_user => asking(&payload),
        "PreToolUse" => activity(&payload),
        _ => payload["message"].as_str().map(str::to_string),
    };
    let said = match event {
        "Stop" => crate::said::last_words(&payload),
        "PreToolUse" => crate::said::asks(&payload),
        _ => None,
    };
    if let Some(log) = log {
        log_hook(log, &task, event, &payload);
    }
    let line = json!({
        "task": task,
        "agent": agent,
        "event": event,
        "message": message,
        "kind": if asks_user { Some(ASKS_USER) } else { payload["notification_type"].as_str() },
        "file": if event == "PreToolUse" { edited_file(&payload) } else { None },
        "said": said,
        "session": payload["session_id"].as_str(),
        "worktree": if event == "PreToolUse" { worktree_added(&payload) } else { None },
        "call": if event == "PreToolUse" && !asks_user { call_print(&payload) } else { None },
    });
    #[cfg(unix)]
    if Path::new(&socket).file_name().is_some_and(|name| name.to_string_lossy().starts_with("daemon")) {
        crate::daemon::send_hook(&socket, crate::daemon::proto::Hook {
            task,
            event: event.to_string(),
            message: line["message"].as_str().map(str::to_string),
            kind: line["kind"].as_str().map(str::to_string),
            file: line["file"].as_str().map(str::to_string),
            said,
            session: line["session"].as_str().map(str::to_string),
            worktree: line["worktree"].as_str().map(str::to_string),
            call: line["call"].as_str().map(str::to_string),
            agent,
            at: 0,
        });
        return None;
    }
    Some((socket, line.to_string()))
}

/// Debug builds: every hook as it came, to hooks.log next to the state, so
/// which process of an agent told which session can be seen afterwards
/// (Claude Code may move a session to a process of its own, under another
/// id). Kept to a few megabytes: the log before is hooks.log.old.
fn log_hook(path: &Path, task: &str, event: &str, payload: &Value) {
    if std::fs::metadata(path).is_ok_and(|m| m.len() > 4 << 20) {
        let _ = std::fs::rename(&path, path.with_extension("log.old"));
    }
    let at = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0., |d| d.as_secs_f64());
    let line = json!({
        "at": at,
        "task": task,
        "event": event,
        "session": payload["session_id"],
        "source": payload["source"],
        "reason": payload["reason"],
        "transcript": payload["transcript_path"],
        "cwd": payload["cwd"],
        "by": ancestors(),
    });
    if let Ok(mut log) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(log, "{line}");
    }
}

/// The processes this hook was run by, nearest first: pid and command line.
#[cfg(target_os = "linux")]
fn ancestors() -> Vec<String> {
    let mut chain = Vec::new();
    let mut pid = std::os::unix::process::parent_id();
    for _ in 0..4 {
        if pid <= 1 {
            break;
        }
        let args = std::fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default();
        let args: String = String::from_utf8_lossy(&args).replace('\0', " ").trim().chars().take(160).collect();
        chain.push(format!("{pid} {args}"));
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap_or_default();
        // After the command's name in parentheses: state, then the parent.
        pid = stat.rsplit_once(')').and_then(|(_, rest)| rest.split_whitespace().nth(1)?.parse().ok()).unwrap_or(0);
    }
    chain
}

#[cfg(unix)]
#[cfg(not(target_os = "linux"))]
fn ancestors() -> Vec<String> {
    vec![std::os::unix::process::parent_id().to_string()]
}

#[cfg(windows)]
fn ancestors() -> Vec<String> {
    Vec::new()
}


/// A hook of an agent's own configuration (`Kimi:Stop`, `Antigravity:Stop`,
/// written there by ideae when asked) as Claude Code's of the same meaning;
/// what means nothing to ideae is dropped.
pub fn foreign(agent: &str, event: &str, payload: Value) -> Option<(&'static str, Value)> {
    let mut payload = if payload.is_object() { payload } else { json!({}) };
    let event = match (agent, event) {
        ("Kimi", "SessionStart") => "SessionStart",
        ("Kimi", "UserPromptSubmit" | "PermissionResult") => "UserPromptSubmit",
        ("Kimi", "Stop" | "StopFailure" | "Interrupt") => "Stop",
        ("Kimi", "PermissionRequest") => {
            let what = payload["tool_name"].as_str().map(|t| format!(": {t}")).unwrap_or_default();
            payload["notification_type"] = json!("permission_prompt");
            payload["message"] = json!(format!("Kimi Code needs your permission{what}"));
            "Notification"
        }
        ("Antigravity", "SessionStart" | "PreInvocation" | "Stop") => {
            if let Some(id) = payload["conversationId"].as_str().map(str::to_string) {
                payload["session_id"] = json!(id);
            }
            match event {
                "PreInvocation" => "UserPromptSubmit",
                "Stop" => "Stop",
                _ => "SessionStart",
            }
        }
        _ => return None,
    };
    Some((event, payload))
}

/// Codex's notify payload as a Stop's: only a turn's end says anything.
pub fn codex_payload(json: &str) -> Option<serde_json::Value> {
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    (v["type"] == "agent-turn-complete").then(|| {
        serde_json::json!({
            "session_id": v["thread-id"],
            "last_assistant_message": v["last-assistant-message"],
            "cwd": v["cwd"],
        })
    })
}

#[cfg(test)]
#[test]
fn an_agent_s_own_hooks_as_claude_s() {
    let (event, payload) = foreign("Kimi", "PermissionRequest", json!({ "session_id": "s1", "tool_name": "Shell" })).unwrap();
    assert_eq!((event, payload["notification_type"].as_str(), payload["message"].as_str()), ("Notification", Some("permission_prompt"), Some("Kimi Code needs your permission: Shell")));
    assert_eq!(foreign("Kimi", "Interrupt", Value::Null).unwrap().0, "Stop");
    assert!(foreign("Kimi", "PreToolUse", json!({})).is_none());
    let (event, payload) = foreign("Antigravity", "PreInvocation", json!({ "conversationId": "c1" })).unwrap();
    assert_eq!((event, payload["session_id"].as_str()), ("UserPromptSubmit", Some("c1")));
    assert_eq!(foreign("Antigravity", "Stop", json!({ "session_id": "kept" })).unwrap().1["session_id"], "kept");
    assert!(foreign("Other", "Stop", json!({})).is_none());
}
