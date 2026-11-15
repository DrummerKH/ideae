//! Agent status from Claude Code hooks, without touching any Claude config.
//!
//! The agent tab starts `claude --settings <file>`, where the file holds hooks
//! that run `ideae --hook <Event>`. That subcommand forwards the event over a
//! unix socket, with the task taken from `IDEAE_TASK` in its environment.
//! Settings passed this way add to the user's own; nothing is written to
//! `~/.claude` or the repository.

use std::{
    io::{BufRead, BufReader, Read, Write},
    os::unix::net::{UnixListener, UnixStream},
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
}

impl HookEvent {
    pub fn status(&self) -> Option<AgentStatus> {
        Some(match self.event.as_str() {
            "SessionStart" => AgentStatus::Idle,
            "UserPromptSubmit" | "PreToolUse" => AgentStatus::Working,
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
    pub socket: PathBuf,
    pub settings: PathBuf,
}

impl Bridge {
    /// Listens on a fresh socket and writes the hooks settings file. Events
    /// go to `on_event` from a background thread.
    pub fn start(on_event: impl Fn(HookEvent) + Send + 'static) -> std::io::Result<Self> {
        let dir = std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir)
            .join(crate::state::APP_ID);
        std::fs::create_dir_all(&dir)?;
        remove_stale(&dir);
        let pid = std::process::id();
        let socket = dir.join(format!("{pid}.sock"));
        let settings = dir.join(format!("hooks-{pid}.json"));
        let _ = std::fs::remove_file(&socket);

        let listener = UnixListener::bind(&socket)?;
        std::thread::Builder::new().name("hook-listener".into()).spawn(move || {
            for stream in listener.incoming().flatten() {
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
        Ok(Self { socket, settings })
    }

    /// The shell command for the agent tab: continue the last conversation in
    /// this worktree, or start one when there is none.
    pub fn claude_command(&self, claude: &str) -> String {
        let settings = shell_quote(&self.settings.display().to_string());
        format!("{claude} --settings {settings} --continue || {claude} --settings {settings}")
    }
}

impl Drop for Bridge {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.socket);
        let _ = std::fs::remove_file(&self.settings);
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
            if !Path::new(&format!("/proc/{pid}")).exists() {
                // Sockets and hook files, and Chrome profiles of the browser tabs.
                let _ = std::fs::remove_file(entry.path());
                let _ = std::fs::remove_dir_all(entry.path());
            }
        }
    }
}

fn hooks_settings(exe: &Path) -> Value {
    let mut hooks = serde_json::Map::new();
    for event in EVENTS {
        let command = format!("{} --hook {event}", shell_quote(&exe.display().to_string()));
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
    })
}

fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// `ideae --hook <Event>`: run by Claude Code, forwards the event and exits.
/// It must never block or fail the agent, so every error is swallowed.
pub fn run_hook(event: &str) {
    let mut input = String::new();
    let _ = std::io::stdin().read_to_string(&mut input);
    let payload: Value = serde_json::from_str(&input).unwrap_or(Value::Null);
    let (Some(socket), Some(task)) = (std::env::var_os(SOCK_ENV), std::env::var(TASK_ENV).ok()) else {
        return;
    };
    let line = json!({
        "task": task,
        "event": event,
        "message": payload["message"].as_str(),
    });
    if let Ok(mut stream) = UnixStream::connect(socket) {
        let _ = stream.set_write_timeout(Some(Duration::from_secs(1)));
        let _ = writeln!(stream, "{line}");
    }
}
