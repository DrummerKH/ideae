//! Agent status from the agents' own reports, without touching any of their
//! config: Claude Code's hooks, Codex's `notify`, OpenCode's plugin events
//! (agents/), all as Claude Code's hook events.
//!
//! The agent tab starts `claude --settings <file>`, where the file holds hooks
//! that run `ideae --hook <Event>`. That subcommand forwards the event over a
//! unix socket (on Windows a loopback TCP port and a token), with the task
//! taken from `IDEAE_TASK` in its environment.
//! Settings passed this way add to the user's own; nothing is written to
//! `~/.claude` or the repository.

use std::{
    io::{BufRead, BufReader, Read, Write},
    path::{Path, PathBuf},
    time::Duration,
};

use serde_json::{Value, json};

pub const SOCK_ENV: &str = "IDEAE_SOCK";
pub const TASK_ENV: &str = "IDEAE_TASK";
/// Set to `agent` in the agent tab's terminal, for the process monitor.
pub const ROLE_ENV: &str = "IDEAE_ROLE";

/// The `kind` of a PreToolUse whose tool waits for the user's answer.
pub const ASKS_USER: &str = "asks_user";
/// Tools that wait for the user: a question, a plan to approve. Some
/// versions of Claude Code send no Notification for them.
const ASKING_TOOLS: &str = "AskUserQuestion|ExitPlanMode";

/// Hook events we listen to.
const EVENTS: &[&str] = &[
    "SessionStart",
    "UserPromptSubmit",
    "PreToolUse",
    "PostToolUse",
    "Notification",
    "Stop",
    "SessionEnd",
];

#[derive(Clone, Debug, PartialEq)]
pub enum AgentStatus {
    /// No hook seen yet: the agent is starting or is not Claude.
    Unknown,
    /// Session open, waiting for a first prompt.
    Idle,
    Working,
    /// A permission prompt or a question: the user has to act.
    NeedsInput(String),
    /// Finished its turn.
    Done,
    Exited,
}

impl AgentStatus {
    /// Wants the user's attention.
    pub fn is_attention(&self) -> bool {
        matches!(self, AgentStatus::NeedsInput(_) | AgentStatus::Done)
    }
}

#[derive(Clone, Debug)]
pub struct HookEvent {
    /// The task's worktree root.
    pub task: PathBuf,
    pub event: String,
    pub message: Option<String>,
    /// Of a Notification: permission_prompt, idle_prompt, …
    pub kind: Option<String>,
    /// The file a tool call is about to change.
    pub file: Option<String>,
    /// What the agent said ([`crate::said`]): a turn's last words, or what
    /// a tool call waits for.
    pub said: Option<String>,
    /// The daemon's record of an earlier event, handed over on connect, with
    /// when it heard it (0: not known): it sets the status but notifies
    /// nobody again.
    pub replayed: Option<u64>,
    /// The agent's id of the session it runs, when it says.
    pub session: Option<String>,
    /// Not reported but guessed from the agent's terminal.
    pub guessed: bool,
}

impl HookEvent {
    fn is_idle_reminder(&self) -> bool {
        match self.kind.as_deref() {
            Some(kind) => kind == "idle_prompt",
            // Versions without notification_type.
            None => self.message.as_deref().is_some_and(|m| m.contains("waiting for your input")),
        }
    }

    pub fn status(&self) -> Option<AgentStatus> {
        Some(match self.event.as_str() {
            "SessionStart" => AgentStatus::Idle,
            "PreToolUse" if self.kind.as_deref() == Some(ASKS_USER) => AgentStatus::NeedsInput(
                self.message.clone().unwrap_or_else(|| "Asks you a question".into()),
            ),
            "UserPromptSubmit" | "PreToolUse" | "PostToolUse" => AgentStatus::Working,
            // A minute after a turn ends Claude reminds that it waits; that is
            // no question, the turn is just done.
            "Notification" if self.is_idle_reminder() => AgentStatus::Done,
            "Notification" if self.kind.as_deref() == Some("auth_success") => return None,
            "Notification" => AgentStatus::NeedsInput(
                self.message.clone().unwrap_or_else(|| "The agent needs your input".into()),
            ),
            "Stop" => AgentStatus::Done,
            "SessionEnd" => AgentStatus::Exited,
            _ => return None,
        })
    }
}

/// A Notification about what already waits for the user: the question
/// its tool call (AskUserQuestion) told of before.
pub fn asked_again(was: &AgentStatus, event: &HookEvent) -> bool {
    event.event == "Notification" && matches!(was, AgentStatus::NeedsInput(_)) && matches!(event.status(), Some(AgentStatus::NeedsInput(_)))
}

/// The socket and the hooks file of this app instance.
pub struct Bridge {
    /// What `IDEAE_SOCK` holds: the socket's path, or `127.0.0.1:port/token`.
    pub socket: String,
    socket_file: Option<PathBuf>,
    pub settings: PathBuf,
    /// OpenCode's plugin, next to the settings.
    plugin: PathBuf,
}

#[cfg(unix)]
fn listen(dir: &Path, pid: u32) -> std::io::Result<(String, Option<PathBuf>, impl Iterator<Item = impl Read + use<>> + Send + use<>)> {
    let socket = dir.join(format!("{pid}.sock"));
    let _ = std::fs::remove_file(&socket);
    let listener = std::os::unix::net::UnixListener::bind(&socket)?;
    let incoming = std::iter::from_fn(move || listener.accept().ok().map(|(stream, _)| stream));
    Ok((socket.display().to_string(), Some(socket), incoming))
}

/// Any local program can reach a loopback port, so each line must carry the
/// token, which only the app's terminals get.
#[cfg(windows)]
fn listen(_: &Path, _: u32) -> std::io::Result<(String, Option<PathBuf>, impl Iterator<Item = impl Read + use<>> + Send + use<>)> {
    use std::hash::{BuildHasher as _, Hasher as _};
    let random = || std::collections::hash_map::RandomState::new().build_hasher().finish();
    let token = format!("{:016x}{:016x}", random(), random());
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0))?;
    let address = format!("{}/{token}", listener.local_addr()?);
    let incoming = std::iter::from_fn(move || loop {
        let (mut stream, _) = listener.accept().ok()?;
        let mut first = [0u8; 33];
        let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
        if stream.read_exact(&mut first).is_ok() && first[..32] == *token.as_bytes() && first[32] == b'\n' {
            let _ = stream.set_read_timeout(None);
            return Some(stream);
        }
    });
    Ok((address, None, incoming))
}

#[cfg(unix)]
fn send(address: &str, line: &str) {
    if let Ok(mut stream) = std::os::unix::net::UnixStream::connect(address) {
        let _ = stream.set_write_timeout(Some(Duration::from_secs(1)));
        let _ = writeln!(stream, "{line}");
    }
}

#[cfg(windows)]
fn send(address: &str, line: &str) {
    let Some((addr, token)) = address.split_once('/') else { return };
    let Ok(addr) = addr.parse::<std::net::SocketAddr>() else { return };
    if let Ok(mut stream) = std::net::TcpStream::connect_timeout(&addr, Duration::from_secs(1)) {
        let _ = stream.set_write_timeout(Some(Duration::from_secs(1)));
        let _ = write!(stream, "{token}\n{line}\n");
    }
}

impl Bridge {
    /// Listens on a fresh socket and writes the hooks settings file. Events
    /// go to `on_event` from a background thread.
    pub fn start(on_event: impl Fn(HookEvent) + Send + 'static) -> std::io::Result<Self> {
        let dir = crate::state::runtime_dir();
        std::fs::create_dir_all(&dir)?;
        remove_stale(&dir);
        let pid = std::process::id();
        // Agents in the daemon outlive this window, and Claude reads the
        // file again later: on unix it stays, at a path every run shares.
        let settings = if cfg!(test) {
            std::env::temp_dir().join(format!("ideae-test-hooks-{pid}.json"))
        } else if let Some(state) = std::env::var_os("IDEAE_STATE") {
            // A run with its own state (tests, experiments) must not rewrite
            // the file the user's agents read.
            use std::hash::{Hash as _, Hasher as _};
            let mut h = std::collections::hash_map::DefaultHasher::new();
            state.hash(&mut h);
            dir.join(format!("hooks-{:016x}.json", h.finish()))
        } else if cfg!(unix) {
            dir.join("hooks.json")
        } else {
            dir.join(format!("hooks-{pid}.json"))
        };
        let plugin = settings.with_extension("opencode.js");
        let (socket, socket_file, incoming) = listen(&dir, pid)?;

        std::thread::Builder::new().name("hook-listener".into()).spawn(move || {
            for stream in incoming {
                let reader = BufReader::new(stream);
                for line in reader.lines().map_while(Result::ok) {
                    if let Some(event) = parse_line(&line) {
                        on_event(event);
                    }
                }
            }
        })?;

        let bridge = Self { socket, socket_file, settings, plugin };
        bridge.write_settings()?;
        let exe = std::env::current_exe().ok();
        if let Some(exe) = &exe {
            let _ = std::fs::write(&bridge.plugin, crate::agents::opencode::plugin_source(exe));
        }
        crate::agents::set_hooks(crate::agents::Hooks { claude_settings: Some(bridge.settings.clone()), exe, opencode_plugin: Some(bridge.plugin.clone()) });
        Ok(bridge)
    }

    /// The hooks, and with Settings' "Agents' language servers" off, the
    /// user's LSP plugins turned off. Agents started later read it.
    pub fn write_settings(&self) -> std::io::Result<()> {
        let exe = std::env::current_exe()?;
        let mut value = hooks_settings(&exe);
        if !crate::state::settings().agent_lsp {
            let off: serde_json::Map<String, Value> = lsp_plugins().into_iter().map(|p| (p, Value::Bool(false))).collect();
            value["enabledPlugins"] = Value::Object(off);
        }
        std::fs::write(&self.settings, serde_json::to_vec_pretty(&value)?)
    }
}

impl Drop for Bridge {
    fn drop(&mut self) {
        if let Some(file) = &self.socket_file {
            let _ = std::fs::remove_file(file);
        }
        if cfg!(windows) {
            let _ = std::fs::remove_file(&self.settings);
            let _ = std::fs::remove_file(&self.plugin);
        }
    }
}

/// Files of instances that exited without cleaning up (killed, or quit
/// without running destructors): their pid is no longer alive.
fn remove_stale(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let pid = name
            .trim_start_matches("hooks-")
            .trim_start_matches("chrome-")
            .split('.')
            .next()
            .and_then(|p| p.parse::<u32>().ok());
        if let Some(pid) = pid {
            if !crate::procs::alive(pid) {
                // Sockets and hook files, and Chrome profiles of the browser tabs.
                let _ = std::fs::remove_file(entry.path());
                let _ = std::fs::remove_dir_all(entry.path());
            }
        }
    }
}

/// The hook's command line. Claude Code on Windows runs hooks with Git Bash,
/// where a double-quoted path with forward slashes works as in cmd.
fn hook_command(exe: &Path, event: &str) -> String {
    if cfg!(windows) {
        format!("\"{}\" --hook {event}", exe.display().to_string().replace('\\', "/"))
    } else {
        format!("{} --hook {event}", shell_quote(&exe.display().to_string()))
    }
}

/// The LSP plugins the user's Claude Code settings enable
/// ("rust-analyzer-lsp@claude-plugins-official" and the like).
fn lsp_plugins() -> Vec<String> {
    let home = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).map(std::path::PathBuf::from);
    let config = std::env::var_os("CLAUDE_CONFIG_DIR").map(std::path::PathBuf::from).or_else(|| home.map(|h| h.join(".claude")));
    let Some(config) = config else { return Vec::new() };
    let mut out = Vec::new();
    for file in ["settings.json", "settings.local.json"] {
        let Ok(text) = std::fs::read_to_string(config.join(file)) else { continue };
        let Ok(v) = serde_json::from_str::<Value>(&text) else { continue };
        if let Some(plugins) = v["enabledPlugins"].as_object() {
            for (name, on) in plugins {
                if on.as_bool() == Some(true) && name.split('@').next().is_some_and(|n| n.ends_with("-lsp")) && !out.contains(name) {
                    out.push(name.clone());
                }
            }
        }
    }
    out
}

fn hooks_settings(exe: &Path) -> Value {
    let mut hooks = serde_json::Map::new();
    for event in EVENTS {
        let command = hook_command(exe, event);
        let mut matcher = json!({ "hooks": [{ "type": "command", "command": command, "timeout": 5 }] });
        match *event {
            "PreToolUse" => matcher["matcher"] = json!("*"),
            // Only to hear that a question was answered.
            "PostToolUse" => matcher["matcher"] = json!(ASKING_TOOLS),
            _ => {}
        }
        hooks.insert(event.to_string(), json!([matcher]));
    }
    json!({ "hooks": hooks })
}

fn parse_line(line: &str) -> Option<HookEvent> {
    let v: Value = serde_json::from_str(line).ok()?;
    Some(HookEvent {
        task: PathBuf::from(v["task"].as_str()?),
        event: v["event"].as_str()?.to_string(),
        message: v["message"].as_str().map(str::to_string),
        kind: v["kind"].as_str().map(str::to_string),
        file: v["file"].as_str().map(str::to_string),
        said: v["said"].as_str().map(str::to_string),
        replayed: None,
        session: v["session"].as_str().map(str::to_string),
        guessed: false,
    })
}

/// Single quotes, for sh and PowerShell alike; each escapes a quote its own way.
fn shell_quote(s: &str) -> String {
    if cfg!(windows) { format!("'{}'", s.replace('\'', "''")) } else { format!("'{}'", s.replace('\'', r"'\''")) }
}

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

/// Whether a tool call waits for the user to answer.
fn asks_user(payload: &Value) -> bool {
    payload["tool_name"].as_str().is_some_and(|tool| ASKING_TOOLS.split('|').any(|t| t == tool))
}

/// What a tool that waits for the user asks: its question, or that a plan
/// waits for approval.
fn asking(payload: &Value) -> Option<String> {
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
pub fn run_hook(args: &[String]) {
    let event = args.first().map(String::as_str).unwrap_or_default();
    let given = |name: &str| args.iter().find_map(|a| a.strip_prefix(&format!("--{name}="))).filter(|v| !v.is_empty()).map(str::to_string);
    let (event, payload) = if event == "Codex" {
        // Codex gives the JSON as the last argument, and only at a turn's end.
        match args.last().and_then(|json| crate::agents::codex::notify_payload(json)) {
            Some(payload) => ("Stop", payload),
            None => return,
        }
    } else {
        let mut input = String::new();
        let _ = std::io::stdin().read_to_string(&mut input);
        (event, serde_json::from_str(&input).unwrap_or(Value::Null))
    };
    let (Some(socket), Some(task)) = (given("sock").or_else(|| std::env::var(SOCK_ENV).ok()), given("task").or_else(|| std::env::var(TASK_ENV).ok())) else {
        return;
    };
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
    if cfg!(debug_assertions) {
        log_hook(&task, event, &payload);
    }
    let line = json!({
        "task": task,
        "event": event,
        "message": message,
        "kind": if asks_user { Some(ASKS_USER) } else { payload["notification_type"].as_str() },
        "file": if event == "PreToolUse" { edited_file(&payload) } else { None },
        "said": said,
        "session": payload["session_id"].as_str(),
    });
    #[cfg(unix)]
    if Path::new(&socket).file_name().is_some_and(|name| name.to_string_lossy().starts_with("daemon")) {
        crate::daemon::client::send_hook(&socket, crate::daemon::proto::Hook {
            task,
            event: event.to_string(),
            message: line["message"].as_str().map(str::to_string),
            kind: line["kind"].as_str().map(str::to_string),
            file: line["file"].as_str().map(str::to_string),
            said,
            session: line["session"].as_str().map(str::to_string),
            at: 0,
        });
        return;
    }
    send(&socket, &line.to_string());
}

/// Debug builds: every hook as it came, to hooks.log next to the state, so
/// which process of an agent told which session can be seen afterwards
/// (Claude Code may move a session to a process of its own, under another
/// id). Kept to a few megabytes: the log before is hooks.log.old.
fn log_hook(task: &str, event: &str, payload: &Value) {
    let path = crate::state::state_dir().join("hooks.log");
    if std::fs::metadata(&path).is_ok_and(|m| m.len() > 4 << 20) {
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
    if let Ok(mut log) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn note(message: &str, kind: Option<&str>) -> Option<AgentStatus> {
        HookEvent { task: PathBuf::new(), event: "Notification".into(), message: Some(message.into()), kind: kind.map(Into::into), file: None, said: None, replayed: None, session: None, guessed: false }
            .status()
    }

    #[test]
    fn events_reach_the_bridge() {
        let (tx, rx) = std::sync::mpsc::channel();
        let bridge = Bridge::start(move |event| {
            let _ = tx.send(event);
        })
        .unwrap();
        send(&bridge.socket, r#"{"task":"/w/t","event":"Stop","session":"u-1"}"#);
        let event = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!((event.task, event.event, event.session.as_deref()), (PathBuf::from("/w/t"), "Stop".to_string(), Some("u-1")));
        #[cfg(windows)]
        {
            let (addr, _) = bridge.socket.split_once('/').unwrap();
            send(&format!("{addr}/{}", "0".repeat(32)), r#"{"task":"/w/t","event":"Stop"}"#);
            assert!(rx.recv_timeout(Duration::from_millis(500)).is_err(), "a wrong token got through");
        }
    }

    #[test]
    fn activities() {
        let a = |v: Value| activity(&v);
        assert_eq!(a(json!({"tool_name":"Edit","tool_input":{"file_path":"/x/src/task.rs"}})).as_deref(), Some("Editing task.rs"));
        assert_eq!(edited_file(&json!({"tool_name":"Write","tool_input":{"file_path":"/x/src/task.rs"}})).as_deref(), Some("/x/src/task.rs"));
        assert_eq!(edited_file(&json!({"tool_name":"Read","tool_input":{"file_path":"/x/src/task.rs"}})), None);
        assert_eq!(a(json!({"tool_name":"Bash","tool_input":{"command":"cargo test --all","description":""}})).as_deref(), Some("Running cargo test --all"));
        assert_eq!(a(json!({"tool_name":"Bash","tool_input":{"command":"x","description":"Build the app"}})).as_deref(), Some("Build the app"));
        assert_eq!(a(json!({"tool_name":"Grep","tool_input":{"pattern":"fn main"}})).as_deref(), Some("Searching \"fn main\""));
        assert_eq!(a(json!({"tool_name":"mcp__github__get_issue","tool_input":{}})).as_deref(), Some("Using github get_issue"));
    }

    #[test]
    fn a_question_tool_needs_the_user_until_answered() {
        let ask = json!({"tool_name":"AskUserQuestion","tool_input":{"questions":[{"question":"Which database?","header":"DB","options":[{"label":"Postgres"}],"multiSelect":false}]}});
        let plan = json!({"tool_name":"ExitPlanMode","tool_input":{"plan":"1. Do it"}});
        assert!(asks_user(&ask) && asks_user(&plan) && !asks_user(&json!({"tool_name":"Bash"})));
        assert_eq!(asking(&ask).as_deref(), Some("Which database?"));
        assert_eq!(asking(&plan).as_deref(), Some("Waits for you to approve its plan"));
        let event = |name: &str, kind: Option<&str>, message: Option<&str>| HookEvent {
            task: PathBuf::new(),
            event: name.into(),
            message: message.map(Into::into),
            kind: kind.map(Into::into),
            file: None,
            said: None,
            replayed: None,
            session: None,
            guessed: false,
        };
        let asked = event("PreToolUse", Some(ASKS_USER), Some("Which database?")).status().unwrap();
        assert_eq!(asked, AgentStatus::NeedsInput("Which database?".into()));
        assert_eq!(event("PreToolUse", None, Some("Running ls")).status(), Some(AgentStatus::Working));
        assert_eq!(event("PostToolUse", None, None).status(), Some(AgentStatus::Working));
        // Versions that do send a Notification for it: no second notice.
        assert!(asked_again(&asked, &event("Notification", None, Some("Claude needs your input"))));
        assert!(!asked_again(&AgentStatus::Working, &event("Notification", None, Some("Claude needs your input"))));
        assert!(!asked_again(&asked, &event("Stop", None, None)));
        let hooks = hooks_settings(Path::new("/x/ideae"));
        assert_eq!(hooks["hooks"]["PostToolUse"][0]["matcher"], json!("AskUserQuestion|ExitPlanMode"));
    }

    #[test]
    fn idle_reminder_is_not_a_question() {
        assert_eq!(note("Claude is waiting for your input", Some("idle_prompt")), Some(AgentStatus::Done));
        assert_eq!(note("Claude is waiting for your input", None), Some(AgentStatus::Done));
        assert!(matches!(note("Claude needs your permission to use Bash", Some("permission_prompt")), Some(AgentStatus::NeedsInput(_))));
        assert!(matches!(note("Claude needs your permission to use Bash", None), Some(AgentStatus::NeedsInput(_))));
    }
}
