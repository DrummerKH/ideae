//! ideae as an MCP server for a chat agent (`ideae --mcp`): what the app
//! knows of the agent's own task, and nothing of another's. The agent runs
//! it on its standard streams (ACP's `mcpServers`); it asks the window over
//! a socket of the app, naming its worktree with a token only that worktree
//! is given. With no window open it says so.
//!
//! Tools: the files open in the task's editor (the one in front, its
//! cursor and selection), the language servers' problems in its files, the
//! project's check run in the task, as the window runs it, and questions
//! for the task's language servers (definition, references, hover,
//! symbols), which start them when asleep: the agent need run none of its own.

use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
#[cfg(windows)]
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

const ROOT_ENV: &str = "IDEAE_MCP_ROOT";
const TOKEN_ENV: &str = "IDEAE_MCP_TOKEN";
const SOCK_ENV: &str = "IDEAE_MCP_SOCK";

/// A question from an agent's server to the window.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Request {
    pub root: PathBuf,
    pub token: String,
    pub tool: String,
    #[serde(default)]
    pub args: Value,
}

/// The window's answer: text for the agent, or why there is none.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Answer {
    pub text: String,
    #[serde(default)]
    pub error: bool,
}

impl Answer {
    pub fn ok(text: impl Into<String>) -> Self {
        Self { text: text.into(), error: false }
    }

    pub fn error(text: impl Into<String>) -> Self {
        Self { text: text.into(), error: true }
    }
}

// ------------------------------------------------------------- the tokens

static KEY: OnceLock<Vec<u8>> = OnceLock::new();

/// The key the worktrees' tokens are made with: kept next to the state,
/// for agents that outlive this window. Read at the start, off the frames.
pub fn preload() {
    let _ = key();
}

fn key() -> &'static [u8] {
    KEY.get_or_init(|| {
        // Tests keep theirs in memory: the user's state is not touched.
        if cfg!(test) {
            let mut key = vec![0u8; 32];
            let _ = ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut key);
            return key;
        }
        let path = crate::state::state_dir().join("mcp.key");
        if let Ok(key) = std::fs::read(&path) {
            if key.len() == 32 {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt as _;
                    let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
                }
                return key;
            }
        }
        let mut key = vec![0u8; 32];
        let _ = ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut key);
        let _ = std::fs::create_dir_all(crate::state::state_dir());
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        if let Ok(mut file) = options.open(&path) {
            let _ = file.write_all(&key);
        }
        key
    })
}

/// The token of a worktree: what proves a question comes from its agent.
pub fn token(root: &Path) -> String {
    let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, key());
    let tag = ring::hmac::sign(&key, root.to_string_lossy().as_bytes());
    tag.as_ref().iter().map(|b| format!("{b:02x}")).collect()
}

pub fn token_ok(root: &Path, token: &str) -> bool {
    let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, key());
    let Some(bytes) = (0..token.len()).step_by(2).map(|i| token.get(i..i + 2).and_then(|h| u8::from_str_radix(h, 16).ok())).collect::<Option<Vec<u8>>>() else { return false };
    ring::hmac::verify(&key, root.to_string_lossy().as_bytes(), &bytes).is_ok()
}

// ------------------------------------------------------------ the socket

/// Where the window listens: a socket every run of this state shares, so an
/// agent that outlives a window finds the next one.
fn socket_path() -> PathBuf {
    let dir = crate::state::runtime_dir();
    match std::env::var_os("IDEAE_STATE") {
        Some(state) => {
            use std::hash::{Hash as _, Hasher as _};
            let mut h = std::collections::hash_map::DefaultHasher::new();
            state.hash(&mut h);
            dir.join(format!("mcp-{:016x}.sock", h.finish()))
        }
        None => dir.join("mcp.sock"),
    }
}

/// What an agent's server is started with, for the worktree `root`.
pub fn server_for(root: &Path) -> Option<(String, Vec<String>, Vec<(String, String)>)> {
    let exe = std::env::current_exe().ok()?;
    let env = vec![(ROOT_ENV.to_string(), root.display().to_string()), (TOKEN_ENV.to_string(), token(root)), (SOCK_ENV.to_string(), address())];
    Some((exe.display().to_string(), vec!["--mcp".to_string()], env))
}

#[cfg(unix)]
fn address() -> String {
    socket_path().display().to_string()
}

/// On Windows a loopback port, which changes with each window: written to
/// a file the server reads at each question.
#[cfg(windows)]
fn address() -> String {
    socket_path().with_extension("addr").display().to_string()
}

/// Listens for the agents' questions; `on_request` answers each, on the
/// window's time, through the sender it is given.
pub fn listen(on_request: impl Fn(Request, async_channel::Sender<Answer>) + Send + Sync + 'static) -> std::io::Result<()> {
    let path = socket_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let on_request = std::sync::Arc::new(on_request);
    #[cfg(unix)]
    let accept = {
        let _ = std::fs::remove_file(&path);
        let listener = std::os::unix::net::UnixListener::bind(&path)?;
        move || {
            listener.accept().ok().map(|(s, _)| {
                let _ = s.set_read_timeout(Some(std::time::Duration::from_secs(10)));
                Box::new(s) as Box<dyn Stream>
            })
        }
    };
    #[cfg(windows)]
    let accept = {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0))?;
        std::fs::write(path.with_extension("addr"), listener.local_addr()?.to_string())?;
        move || {
            listener.accept().ok().map(|(s, _)| {
                let _ = s.set_read_timeout(Some(Duration::from_secs(10)));
                Box::new(s) as Box<dyn Stream>
            })
        }
    };
    std::thread::Builder::new().name("mcp-listener".into()).spawn(move || {
        while let Some(stream) = accept() {
            let on_request = on_request.clone();
            std::thread::spawn(move || serve_one(stream, &*on_request));
        }
    })?;
    Ok(())
}

trait Stream: std::io::Read + std::io::Write + Send {}
impl<T: std::io::Read + std::io::Write + Send> Stream for T {}

fn serve_one(mut stream: Box<dyn Stream>, on_request: &(dyn Fn(Request, async_channel::Sender<Answer>) + Send + Sync)) {
    // One line, soon and not huge: a client that sends none holds nothing long.
    let mut line = String::new();
    {
        let mut reader = BufReader::new((&mut stream).take(1 << 20));
        if reader.read_line(&mut line).is_err() || !line.ends_with('\n') {
            return;
        }
    }
    let answer = match serde_json::from_str::<Request>(&line) {
        Ok(request) if token_ok(&request.root, &request.token) => {
            let (tx, rx) = async_channel::bounded(1);
            on_request(request, tx);
            rx.recv_blocking().unwrap_or_else(|_| Answer::error("ideae's window closed before it answered"))
        }
        Ok(_) => Answer::error("not this worktree's token"),
        Err(e) => Answer::error(format!("not a question: {e}")),
    };
    let _ = stream.write_all(&serde_json::to_vec(&answer).unwrap_or_default());
    let _ = stream.write_all(b"\n");
}

/// Asks the window; a check can take as long as it runs.
fn ask(address: &str, request: &Request) -> Answer {
    #[cfg(unix)]
    let stream = std::os::unix::net::UnixStream::connect(address).map(|s| Box::new(s) as Box<dyn Stream>);
    #[cfg(windows)]
    let stream = std::fs::read_to_string(address)
        .map_err(std::io::Error::other)
        .and_then(|a| a.trim().parse::<std::net::SocketAddr>().map_err(std::io::Error::other))
        .and_then(|a| std::net::TcpStream::connect_timeout(&a, Duration::from_secs(2)))
        .map(|s| Box::new(s) as Box<dyn Stream>);
    let Ok(mut stream) = stream else {
        return Answer::error("ideae's window is not open: what it shows (open files, problems, the check) and its language servers are not at hand now");
    };
    let _ = stream.write_all(&serde_json::to_vec(request).unwrap_or_default());
    let _ = stream.write_all(b"\n");
    let mut reply = String::new();
    let _ = stream.read_to_string(&mut reply);
    serde_json::from_str(&reply).unwrap_or_else(|_| Answer::error("ideae's window gave no answer"))
}

// ------------------------------------------------------------ the server

fn tools() -> Value {
    json!([
        {
            "name": "open_files",
            "description": "The files open in ideae's editor for this task (its worktree only): which one is in front, its cursor line, and the text selected in it, and which have edits not saved yet.",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "diagnostics",
            "description": "Errors and warnings the language servers report for this task's files, as ideae's editor shows them: for one file (a path in the worktree), or for all files open in the editor. Faster and fresher than running a build for a quick look; only files the editor has open are analysed.",
            "inputSchema": { "type": "object", "properties": { "path": { "type": "string", "description": "A file of the worktree, relative to it or absolute; all open files when left out." } } }
        },
        {
            "name": "definition",
            "description": "Where a name used in this task's code is declared, from the language server ideae runs for the task (rust-analyzer, gopls, clangd, the TypeScript server…): file, line and the line's text. Exact where a text search guesses; it knows imports, methods, overloads and the libraries. Give the file, the line and the name as written on it.",
            "inputSchema": { "type": "object", "properties": {
                "path": { "type": "string", "description": "A file of the worktree, relative to it or absolute." },
                "line": { "type": "integer", "description": "The line the name is on, counted from 1." },
                "symbol": { "type": "string", "description": "The name as written on that line; its first occurrence there is asked about." },
                "column": { "type": "integer", "description": "Instead of symbol: the column, counted from 1." }
            }, "required": ["path", "line"] }
        },
        {
            "name": "references",
            "description": "Every place in this task's code that uses a name, from the language server: what a change of it would touch. Its own declaration is left out. Same arguments as definition.",
            "inputSchema": { "type": "object", "properties": {
                "path": { "type": "string" }, "line": { "type": "integer" }, "symbol": { "type": "string" }, "column": { "type": "integer" }
            }, "required": ["path", "line"] }
        },
        {
            "name": "hover",
            "description": "What a name is, from the language server: its type or signature and its documentation. Same arguments as definition.",
            "inputSchema": { "type": "object", "properties": {
                "path": { "type": "string" }, "line": { "type": "integer" }, "symbol": { "type": "string" }, "column": { "type": "integer" }
            }, "required": ["path", "line"] }
        },
        {
            "name": "symbols",
            "description": "Types, functions and other names this task's project and its libraries declare that match a query, with where each is: to find a declaration by name without knowing its file.",
            "inputSchema": { "type": "object", "properties": {
                "query": { "type": "string", "description": "A name or the start of one." },
                "path": { "type": "string", "description": "A file of the language to search, when no language server runs for the task yet." }
            }, "required": ["query"] }
        },
        {
            "name": "run_check",
            "description": "Runs the project's check (its tests or build, as set up in ideae for this project) in this task's worktree, as ideae does after a turn, and gives its result and the end of its output. Can take minutes.",
            "inputSchema": { "type": "object", "properties": {} }
        }
    ])
}

/// `ideae --mcp`: an MCP server on standard input and output.
pub fn run_server() {
    let root = PathBuf::from(std::env::var(ROOT_ENV).unwrap_or_default());
    let token = std::env::var(TOKEN_ENV).unwrap_or_default();
    let address = std::env::var(SOCK_ENV).unwrap_or_default();
    let stdin = std::io::stdin();
    let mut out = std::io::stdout();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let Ok(msg) = serde_json::from_str::<Value>(&line) else { continue };
        let Some(id) = msg.get("id").cloned() else { continue };
        let method = msg.get("method").and_then(Value::as_str).unwrap_or_default();
        let result = match method {
            "initialize" => Ok(json!({
                "protocolVersion": msg.pointer("/params/protocolVersion").cloned().unwrap_or(json!("2025-06-18")),
                "capabilities": { "tools": {} },
                "serverInfo": { "name": "ideae", "version": env!("CARGO_PKG_VERSION") },
                "instructions": "ideae is the IDE this task runs in. Its tools tell what the user sees of this task (open files and selection, the language servers' problems, the project's check) and ask its language servers: where a name is declared, where it is used, what it is, which names the project declares.",
            })),
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({ "tools": tools() })),
            "tools/call" => {
                let tool = msg.pointer("/params/name").and_then(Value::as_str).unwrap_or_default().to_string();
                let args = msg.pointer("/params/arguments").cloned().unwrap_or(Value::Null);
                let answer = ask(&address, &Request { root: root.clone(), token: token.clone(), tool, args });
                Ok(json!({ "content": [{ "type": "text", "text": answer.text }], "isError": answer.error }))
            }
            _ => Err(json!({ "code": -32601, "message": format!("{method} is not supported") })),
        };
        let reply = match result {
            Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
            Err(error) => json!({ "jsonrpc": "2.0", "id": id, "error": error }),
        };
        let _ = writeln!(out, "{reply}");
        let _ = out.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_token_is_its_worktree_s_alone() {
        let a = token(Path::new("/w/a"));
        assert!(token_ok(Path::new("/w/a"), &a));
        assert!(!token_ok(Path::new("/w/b"), &a), "another worktree's token");
        assert!(!token_ok(Path::new("/w/a"), "00"), "made up");
        assert!(!token_ok(Path::new("/w/a"), "zz"));
    }

    /// A question through the socket, answered for its worktree only; with
    /// no window listening, the server says so.
    #[cfg(unix)]
    #[test]
    fn a_question_reaches_the_window_with_its_token() {
        let _one = crate::workspace::variants::tests::ONE_WINDOW.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("ideae-mcp-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // SAFETY: under the lock the tests that set it share.
        unsafe { std::env::set_var("IDEAE_STATE", dir.join("state.json")) };
        let address = address();
        let root = PathBuf::from("/w/task");
        let asked = Request { root: root.clone(), token: token(&root), tool: "open_files".into(), args: Value::Null };
        assert!(ask(&address, &asked).text.contains("not open"), "no window");

        listen(|request, answer| {
            let _ = answer.try_send(Answer::ok(format!("{} of {}", request.tool, request.root.display())));
        })
        .unwrap();
        let heard = ask(&address, &asked);
        assert_eq!((heard.text.as_str(), heard.error), ("open_files of /w/task", false));
        let other = ask(&address, &Request { root: PathBuf::from("/w/other"), ..asked });
        assert!(other.error && other.text.contains("token"), "another worktree, its token not had: {}", other.text);
        let _ = std::fs::remove_file(&address);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
