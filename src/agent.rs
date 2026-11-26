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

pub use ideae_core::hook::{AGENT_ENV, SOCK_ENV, TASK_ENV};
/// Set to `agent` in the agent tab's terminal, for the process monitor.
pub const ROLE_ENV: &str = "IDEAE_ROLE";

use ideae_core::hook::ASKING_TOOLS;

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

pub use ideae_core::status::{AgentStatus, HookEvent, asked_again};

/// The socket and the hooks file of this app instance.
pub struct Bridge {
    /// What `IDEAE_SOCK` holds: the socket's path, or `127.0.0.1:port/token`.
    pub socket: String,
    socket_file: Option<PathBuf>,
    pub settings: PathBuf,
    /// OpenCode's plugin, next to the settings.
    plugin: PathBuf,
    /// Claude Code's `--mcp-config`, next to them too.
    mcp: PathBuf,
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
        let mcp = settings.with_extension("mcp.json");
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

        let bridge = Self { socket, socket_file, settings, plugin, mcp };
        bridge.write_settings()?;
        let exe = std::env::current_exe().ok();
        // The script hooks in agents' own files run, and which files have them.
        // Read before tasks are restored (a few small files, of these agents' profiles alone):
        // an agent started with them reports, one without is watched.
        if let Some(exe) = exe.clone().filter(|_| cfg!(unix) && !cfg!(test)) {
            crate::agents::own_config::refresh(&crate::state::with_settings(|s| s.agents.clone()));
            let _ = std::thread::Builder::new().name("agent-hooks".into()).spawn(move || {
                let _ = crate::agents::own_config::write_script(&exe);
            });
        }
        let mut claude_mcp = None;
        if let Some(exe) = &exe {
            let _ = std::fs::write(&bridge.plugin, crate::agents::opencode::plugin_source(exe));
            // No token here: `ideae --mcp` reads the agent's terminal's variables.
            let config = json!({ "mcpServers": { "ideae": { "command": exe, "args": ["--mcp"] } } });
            if std::fs::write(&bridge.mcp, serde_json::to_vec_pretty(&config).unwrap_or_default()).is_ok() {
                claude_mcp = Some(bridge.mcp.clone());
            }
        }
        crate::agents::set_hooks(crate::agents::Hooks {
            claude_settings: Some(bridge.settings.clone()),
            exe,
            opencode_plugin: Some(bridge.plugin.clone()),
            claude_mcp,
        });
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
            let _ = std::fs::remove_file(&self.mcp);
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

pub(crate) fn hooks_settings(exe: &Path) -> Value {
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
        agent: v["agent"].as_str().filter(|a| !a.is_empty()).map(str::to_string),
        event: v["event"].as_str()?.to_string(),
        message: v["message"].as_str().map(str::to_string),
        kind: v["kind"].as_str().map(str::to_string),
        file: v["file"].as_str().map(str::to_string),
        said: v["said"].as_str().map(str::to_string),
        replayed: None,
        session: v["session"].as_str().map(str::to_string),
        guessed: false,
        worktree: v["worktree"].as_str().map(str::to_string),
        call: v["call"].as_str().map(str::to_string),
    })
}

/// Single quotes, for sh and PowerShell alike; each escapes a quote its own way.
fn shell_quote(s: &str) -> String {
    if cfg!(windows) { format!("'{}'", s.replace('\'', "''")) } else { format!("'{}'", s.replace('\'', r"'\''")) }
}

/// `ideae --hook <Event>` ([`ideae_core::hook::run`]); a report for this
/// window's own socket (no daemon) goes there.
pub fn run_hook(args: &[String]) {
    let log = cfg!(debug_assertions).then(|| crate::state::state_dir().join("hooks.log"));
    if let Some((socket, line)) = ideae_core::hook::run(args, log.as_deref()) {
        send(&socket, &line);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ideae_core::hook::{activity, as_git_lists, asking, asks_user, edited_file, worktree_added};
    use ideae_core::status::ASKS_USER;

    /// The worktree a command makes, wherever it is said to run.
    #[test]
    fn a_worktree_added_by_a_command() {
        // As paths, not text: Windows parts them with backslashes.
        let added = |command: &str| worktree_added(&json!({ "tool_name": "Bash", "cwd": "/w/repo", "tool_input": { "command": command } })).map(PathBuf::from);
        let at = |path: &str| Some(PathBuf::from(path));
        let made = "git -C /w/repo fetch -q origin && git -C /w/repo worktree add /w/repo-batch -b support_batch origin/main 2>&1 | tail -3";
        assert_eq!(added(made), at("/w/repo-batch"));
        assert_eq!(added("git worktree add ../repo-x"), at("/w/repo-x"));
        assert_eq!(added("cd sub && git worktree add -B 'my branch' \"../with space\" HEAD"), at("/w/repo/with space"));
        assert_eq!(added("git -C .. worktree add --detach --reason why x"), at("/w/x"));
        assert_eq!(added("git worktree add \"$DIR\""), None, "a variable is not known");
        assert_eq!(added("git worktree list"), None);
        // An existing folder's links are followed, as git lists the worktree.
        let tmp = std::env::temp_dir();
        assert_eq!(as_git_lists(&tmp.join("x")), tmp.canonicalize().unwrap().join("x"));
        assert_eq!(added("echo git worktree add x"), None);
        assert_eq!(worktree_added(&json!({ "tool_name": "Read", "cwd": "/w", "tool_input": { "command": "git worktree add x" } })), None);
    }

    fn note(message: &str, kind: Option<&str>) -> Option<AgentStatus> {
        HookEvent { task: PathBuf::new(), agent: None, event: "Notification".into(), message: Some(message.into()), kind: kind.map(Into::into), file: None, said: None, replayed: None, session: None, guessed: false, worktree: None, call: None }
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
            agent: None,
            event: name.into(),
            message: message.map(Into::into),
            kind: kind.map(Into::into),
            file: None,
            said: None,
            replayed: None,
            session: None,
            guessed: false,
            worktree: None,
            call: None,
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
