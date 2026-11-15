//! Agent status from Claude Code hooks, without touching any Claude config.
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

/// Hook events we listen to.
const EVENTS: &[&str] = &[
    "SessionStart",
    "UserPromptSubmit",
    "PreToolUse",
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
    /// The daemon's record of an earlier event, handed over on connect: it
    /// sets the status but notifies nobody again.
    pub replayed: bool,
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
            "UserPromptSubmit" | "PreToolUse" => AgentStatus::Working,
            // A minute after a turn ends Claude reminds that it waits; that is
            // no question, the turn is just done.
            "Notification" if self.is_idle_reminder() => AgentStatus::Done,
            "Notification" if self.kind.as_deref() == Some("auth_success") => return None,
            "Notification" => AgentStatus::NeedsInput(
                self.message.clone().unwrap_or_else(|| "Claude needs your input".into()),
            ),
            "Stop" => AgentStatus::Done,
            "SessionEnd" => AgentStatus::Exited,
            _ => return None,
        })
    }
}

/// The socket and the hooks file of this app instance.
pub struct Bridge {
    /// What `IDEAE_SOCK` holds: the socket's path, or `127.0.0.1:port/token`.
    pub socket: String,
    socket_file: Option<PathBuf>,
    pub settings: PathBuf,
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
        let settings = if cfg!(unix) { dir.join("hooks.json") } else { dir.join(format!("hooks-{pid}.json")) };
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

        let exe = std::env::current_exe()?;
        std::fs::write(&settings, serde_json::to_vec_pretty(&hooks_settings(&exe))?)?;
        Ok(Self { socket, socket_file, settings })
    }

    /// The shell command for the agent tab: continue the last conversation in
    /// this worktree, or start one when there is none. On Windows the shell
    /// is PowerShell, whose 5.1 has no `||`.
    pub fn claude_command(&self, claude: &str) -> String {
        let settings = shell_quote(&self.settings.display().to_string());
        // The user's own choice of conversation stands.
        let picks_conversation = claude
            .split_whitespace()
            .skip(1)
            .any(|a| matches!(a, "-c" | "--continue" | "-r" | "--resume" | "--session-id") || a.starts_with("--resume=") || a.starts_with("--session-id="));
        if picks_conversation {
            return format!("{claude} --settings {settings}");
        }
        if cfg!(windows) {
            format!("{claude} --settings {settings} --continue; if (-not $?) {{ {claude} --settings {settings} }}")
        } else {
            format!("{claude} --settings {settings} --continue || {claude} --settings {settings}")
        }
    }
}

/// Whether an agent command runs Claude Code (`claude`, a path to it, with
/// arguments or not): it gets the hooks and continues its conversation.
pub fn is_claude(command: &str) -> bool {
    command.split_whitespace().next().and_then(|program| {
        let name = Path::new(program).file_name()?.to_string_lossy().to_lowercase();
        Some(matches!(name.trim_end_matches(".exe").trim_end_matches(".cmd"), "claude"))
    }) == Some(true)
}

impl Drop for Bridge {
    fn drop(&mut self) {
        if let Some(file) = &self.socket_file {
            let _ = std::fs::remove_file(file);
        }
        if cfg!(windows) {
            let _ = std::fs::remove_file(&self.settings);
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

fn hooks_settings(exe: &Path) -> Value {
    let mut hooks = serde_json::Map::new();
    for event in EVENTS {
        let command = hook_command(exe, event);
        let mut matcher = json!({ "hooks": [{ "type": "command", "command": command, "timeout": 5 }] });
        if *event == "PreToolUse" {
            matcher["matcher"] = json!("*");
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
        replayed: false,
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

/// `ideae --hook <Event>`: run by Claude Code, forwards the event and exits.
/// It must never block or fail the agent, so every error is swallowed.
pub fn run_hook(event: &str) {
    let mut input = String::new();
    let _ = std::io::stdin().read_to_string(&mut input);
    let payload: Value = serde_json::from_str(&input).unwrap_or(Value::Null);
    let (Some(socket), Some(task)) = (std::env::var(SOCK_ENV).ok(), std::env::var(TASK_ENV).ok()) else {
        return;
    };
    // Before a tool runs the message says what the agent is doing.
    let message = match event {
        "PreToolUse" => activity(&payload),
        _ => payload["message"].as_str().map(str::to_string),
    };
    let line = json!({
        "task": task,
        "event": event,
        "message": message,
        "kind": payload["notification_type"].as_str(),
    });
    #[cfg(unix)]
    if Path::new(&socket).file_name().is_some_and(|name| name.to_string_lossy().starts_with("daemon")) {
        crate::daemon::client::send_hook(&socket, crate::daemon::proto::Hook {
            task,
            event: event.to_string(),
            message: line["message"].as_str().map(str::to_string),
            kind: line["kind"].as_str().map(str::to_string),
        });
        return;
    }
    send(&socket, &line.to_string());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn note(message: &str, kind: Option<&str>) -> Option<AgentStatus> {
        HookEvent { task: PathBuf::new(), event: "Notification".into(), message: Some(message.into()), kind: kind.map(Into::into), replayed: false }
            .status()
    }

    #[test]
    fn events_reach_the_bridge() {
        let (tx, rx) = std::sync::mpsc::channel();
        let bridge = Bridge::start(move |event| {
            let _ = tx.send(event);
        })
        .unwrap();
        send(&bridge.socket, r#"{"task":"/w/t","event":"Stop"}"#);
        let event = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!((event.task, event.event), (PathBuf::from("/w/t"), "Stop".to_string()));
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
        assert_eq!(a(json!({"tool_name":"Bash","tool_input":{"command":"cargo test --all","description":""}})).as_deref(), Some("Running cargo test --all"));
        assert_eq!(a(json!({"tool_name":"Bash","tool_input":{"command":"x","description":"Build the app"}})).as_deref(), Some("Build the app"));
        assert_eq!(a(json!({"tool_name":"Grep","tool_input":{"pattern":"fn main"}})).as_deref(), Some("Searching \"fn main\""));
        assert_eq!(a(json!({"tool_name":"mcp__github__get_issue","tool_input":{}})).as_deref(), Some("Using github get_issue"));
    }

    #[test]
    fn claude_commands() {
        assert!(is_claude("claude"));
        assert!(is_claude("claude --model opus"));
        assert!(is_claude("/home/u/.local/bin/claude"));
        assert!(!is_claude("aider"));
        assert!(!is_claude(""));
        let bridge = Bridge { socket: String::new(), socket_file: None, settings: PathBuf::from("/run/h.json") };
        let with_args = bridge.claude_command("claude --model opus");
        assert!(with_args.starts_with("claude --model opus --settings '/run/h.json' --continue ||"), "{with_args}");
        assert_eq!(bridge.claude_command("claude --resume abc"), "claude --resume abc --settings '/run/h.json'");
        std::mem::forget(bridge);
    }

    #[test]
    fn idle_reminder_is_not_a_question() {
        assert_eq!(note("Claude is waiting for your input", Some("idle_prompt")), Some(AgentStatus::Done));
        assert_eq!(note("Claude is waiting for your input", None), Some(AgentStatus::Done));
        assert!(matches!(note("Claude needs your permission to use Bash", Some("permission_prompt")), Some(AgentStatus::NeedsInput(_))));
        assert!(matches!(note("Claude needs your permission to use Bash", None), Some(AgentStatus::NeedsInput(_))));
    }
}
