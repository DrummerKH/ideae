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
//! And other agents of its project: a question to one, a task proposed to
//! the user, waiting for what they say (workspace/crew.rs).

use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
#[cfg(windows)]
use std::time::Duration;

pub use ideae_core::mcp::{Answer, Request, run_server};
use ideae_core::mcp::{CREW_ENV, ROOT_ENV, SOCK_ENV, Stream, TICKET_ENV, TOKEN_ENV};
#[cfg(all(test, unix))]
use ideae_core::mcp::ask;

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

/// Where the window listens, for a host's agents to reach it through ssh.
#[cfg(unix)]
pub fn socket() -> PathBuf {
    socket_path()
}

/// What an agent's server is started with, for the worktree `root`. On
/// another machine, ideae-server there, asking through the window's
/// socket reached there; none until it is.
pub fn server_for(root: &Path) -> Option<(String, Vec<String>, Vec<(String, String)>)> {
    let (exe, address) = match crate::hosts::is_remote(root) {
        #[cfg(unix)]
        true => {
            let daemon = crate::daemon::client::for_path(root)?;
            (daemon.there()?.0, daemon.window_there()?.display().to_string())
        }
        #[cfg(not(unix))]
        true => return None,
        false => (std::env::current_exe().ok()?, address()),
    };
    let mut env = vec![(ROOT_ENV.to_string(), root.display().to_string()), (TOKEN_ENV.to_string(), token(root)), (SOCK_ENV.to_string(), address)];
    if crate::state::with_settings(|s| s.agents_start_agents) {
        env.push((CREW_ENV.to_string(), "1".to_string()));
    }
    if crate::state::with_settings(|s| s.tickets) && crate::integrations::of_root(root).iter().any(|i| i.agent_read) {
        env.push((TICKET_ENV.to_string(), "1".to_string()));
    }
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
        Ok(mut request) if token_ok(&request.root, &request.token) => {
            // An agent on another machine names files as they are there; the window, by their keys.
            let host = crate::hosts::split(&request.root).map(|(host, _)| host);
            if host.is_some() {
                keyed_args(&mut request.args, &request.root);
            }
            let (tx, rx) = async_channel::bounded(1);
            on_request(request, tx);
            let answer = rx.recv_blocking().unwrap_or_else(|_| Answer::error("ideae's window closed before it answered"));
            match &host {
                Some(host) => Answer { text: unkeyed(&answer.text, host), ..answer },
                None => answer,
            }
        }
        Ok(_) => Answer::error("not this worktree's token"),
        Err(e) => Answer::error(format!("not a question: {e}")),
    };
    let _ = stream.write_all(&serde_json::to_vec(&answer).unwrap_or_default());
    let _ = stream.write_all(b"\n");
}

/// A path an agent of the remote task at `root` gives (absolute, as there),
/// as the window takes it: in its worktree, relative to it; else keyed.
fn keyed_args(args: &mut serde_json::Value, root: &Path) {
    let Some((host, there)) = crate::hosts::split(root) else { return };
    let Some(path) = args.get("path").and_then(|p| p.as_str()).filter(|p| p.starts_with('/')).map(PathBuf::from) else { return };
    let taken = match path.strip_prefix(&there) {
        // The worktree itself: its folder, not no path.
        Ok(rel) if rel.as_os_str().is_empty() => PathBuf::from("."),
        Ok(rel) => rel.to_path_buf(),
        Err(_) => crate::hosts::join(&host, &path),
    };
    args["path"] = serde_json::Value::String(taken.display().to_string());
}

/// The window's answer, its keys of the host's files named as they are there.
/// The whole text: a file's own words that hold such a key (`ssh:/box/…`,
/// rare) are told so too, as there is nothing else they could name there.
fn unkeyed(text: &str, host: &str) -> String {
    text.replace(&format!("ssh:/{host}/"), "/")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What an agent there asks names its files as they are there, and so
    /// does what it is told; the window keys them.
    #[test]
    fn an_agent_there_names_files_as_they_are_there() {
        let root = crate::hosts::join("hub>mac", Path::new("/home/me/wt"));
        let mut args = serde_json::json!({ "path": "/home/me/wt/src/a.rs", "line": 3 });
        keyed_args(&mut args, &root);
        assert_eq!(args["path"], "src/a.rs", "in its worktree: relative to it");
        let mut outside = serde_json::json!({ "path": "/usr/include/a.h" });
        keyed_args(&mut outside, &root);
        assert_eq!(outside["path"], "ssh:/hub>mac/usr/include/a.h");
        let mut itself = serde_json::json!({ "path": "/home/me/wt" });
        keyed_args(&mut itself, &root);
        assert_eq!(itself["path"], ".", "the worktree itself");
        let mut relative = serde_json::json!({ "path": "src/a.rs" });
        keyed_args(&mut relative, &root);
        assert_eq!(relative["path"], "src/a.rs", "relative to its worktree: as it is");
        assert_eq!(unkeyed("ssh:/box/home/me/wt/src/a.rs:3: error\nssh:/boxer/x", "box"), "/home/me/wt/src/a.rs:3: error\nssh:/boxer/x");
    }

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
        let asked = Request { root: root.clone(), token: token(&root), tool: "open_files".into(), args: serde_json::Value::Null, agent: None };
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
