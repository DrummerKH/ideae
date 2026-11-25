//! A ticket asked for from outside the window: `ideae ticket DEV-123` on
//! the command line, `ideae://ticket/DEV-123` from a browser's link or a
//! bookmarklet. The window that runs gets it (a socket of its own, per
//! state, so a test run's never takes the user's); none running, the app
//! starts and takes it then. The task of that ticket comes to the front,
//! or New task opens with its key.

use std::io::{BufRead as _, BufReader, Write as _};
use std::path::PathBuf;
use std::sync::Mutex;

/// The scheme a link to the app has: the dev build is an app of its own.
pub const SCHEME: &str = if cfg!(debug_assertions) { "ideae-dev" } else { "ideae" };

/// What a line from outside asks for.
#[derive(Clone, Debug, PartialEq)]
pub enum Asked {
    Ticket(String),
}

/// `ticket DEV-123`, or `ideae://ticket/DEV-123` (`ideae-dev://` for the dev
/// build), `ideae://ticket?key=DEV-123`.
pub fn parse(line: &str) -> Option<Asked> {
    let line = line.trim();
    let key = if let Some(rest) = line.strip_prefix("ticket ") {
        rest.trim().to_string()
    } else {
        let rest = line.strip_prefix(SCHEME).and_then(|r| r.strip_prefix("://"))?;
        let rest = rest.strip_prefix("ticket")?;
        let key = match (rest.strip_prefix('/'), rest.strip_prefix("?key=")) {
            (Some(path), _) => path.split(['?', '#']).next().unwrap_or_default(),
            (_, Some(query)) => query.split('&').next().unwrap_or_default(),
            _ => "",
        };
        key.trim_end_matches('/').replace("%23", "#").replace("%2F", "/")
    };
    // A key, or `group/project#4` read as written; nothing longer or stranger.
    let ok = !key.is_empty() && key.chars().count() <= 120 && key.chars().all(|c| c.is_ascii_alphanumeric() || "-_#./".contains(c));
    ok.then_some(Asked::Ticket(key))
}

/// The path of a `file://` URL a launcher passes (`%U`), its escapes
/// decoded as UTF-8 bytes: `file:///home/u/%D0%BF%D1%80`, `file://localhost/x`.
/// Another machine's (`file://host/x`) is none of ours.
pub fn file_url_path(arg: &str) -> Option<std::path::PathBuf> {
    let rest = arg.strip_prefix("file://")?;
    let path = match rest.find('/') {
        Some(0) => rest,
        Some(at) if matches!(rest.get(..at), Some("localhost" | "127.0.0.1")) => rest.get(at..)?,
        _ => return None,
    };
    let bytes = path.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = |b: u8| (b as char).to_digit(16);
        match (bytes[i], bytes.get(i + 1).copied().and_then(hex), bytes.get(i + 2).copied().and_then(hex)) {
            (b'%', Some(high), Some(low)) => {
                out.push((high * 16 + low) as u8);
                i += 3;
            }
            (b, _, _) => {
                out.push(b);
                i += 1;
            }
        }
    }
    Some(std::path::PathBuf::from(String::from_utf8(out).ok()?))
}

/// Lines that came before the window was there to take them.
static INBOX: Mutex<(Vec<String>, Option<async_channel::Sender<String>>)> = Mutex::new((Vec::new(), None));

/// Hands a line to the window: now, or when it is there.
pub fn deliver(line: String) {
    let mut inbox = INBOX.lock().unwrap_or_else(|e| e.into_inner());
    match &inbox.1 {
        Some(tx) => {
            let _ = tx.try_send(line);
        }
        None => inbox.0.push(line),
    }
}

/// The window takes the lines: those waiting, then each as it comes.
pub fn connect(tx: async_channel::Sender<String>) {
    let mut inbox = INBOX.lock().unwrap_or_else(|e| e.into_inner());
    for line in inbox.0.drain(..) {
        let _ = tx.try_send(line);
    }
    inbox.1 = Some(tx);
}

/// Where the running window listens: one per state file, so that a run on
/// a scratch state (`IDEAE_STATE`) has its own.
fn address() -> PathBuf {
    use std::hash::{Hash as _, Hasher as _};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    crate::state::state_path().hash(&mut h);
    // A test's own, never the user's window.
    if cfg!(test) {
        std::process::id().hash(&mut h);
    }
    crate::state::runtime_dir().join(format!("open-{:016x}", h.finish()))
}

/// Listens for lines from another run of the app, each handed on.
#[cfg(unix)]
pub fn listen() {
    let socket = address().with_extension("sock");
    if let Some(dir) = socket.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    // Another window of this state listening already: it gets them.
    if std::os::unix::net::UnixStream::connect(&socket).is_ok() {
        return;
    }
    let _ = std::fs::remove_file(&socket);
    let Ok(listener) = std::os::unix::net::UnixListener::bind(&socket) else { return };
    let _ = std::thread::Builder::new().name("deep-link".into()).spawn(move || {
        for stream in listener.incoming().flatten() {
            for line in BufReader::new(stream).lines().map_while(Result::ok).take(4) {
                deliver(line);
            }
        }
    });
}

/// On Windows a loopback port, its address and a token in a file only this
/// user reads: any program can reach the port, only the token's holder is heard.
#[cfg(windows)]
pub fn listen() {
    use std::hash::{BuildHasher as _, Hasher as _};
    let file = address().with_extension("addr");
    if send_to(&file, "") {
        return;
    }
    let Ok(listener) = std::net::TcpListener::bind(("127.0.0.1", 0)) else { return };
    let Ok(at) = listener.local_addr() else { return };
    let random = || std::collections::hash_map::RandomState::new().build_hasher().finish();
    let token = format!("{:016x}{:016x}", random(), random());
    if let Some(dir) = file.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if std::fs::write(&file, format!("{at}/{token}")).is_err() {
        return;
    }
    let _ = std::thread::Builder::new().name("deep-link".into()).spawn(move || {
        for stream in listener.incoming().flatten() {
            let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(2)));
            let mut lines = BufReader::new(stream).lines().map_while(Result::ok);
            if lines.next().as_deref() != Some(token.as_str()) {
                continue;
            }
            for line in lines.take(4) {
                deliver(line);
            }
        }
    });
}

#[cfg(windows)]
fn send_to(file: &std::path::Path, line: &str) -> bool {
    let Ok(text) = std::fs::read_to_string(file) else { return false };
    let Some((at, token)) = text.trim().split_once('/') else { return false };
    let Ok(at) = at.parse::<std::net::SocketAddr>() else { return false };
    let Ok(mut stream) = std::net::TcpStream::connect_timeout(&at, std::time::Duration::from_secs(1)) else { return false };
    writeln!(stream, "{token}\n{line}").is_ok()
}

/// Hands a line to the window that runs; false when none does.
pub fn send(line: &str) -> bool {
    #[cfg(unix)]
    {
        let Ok(mut stream) = std::os::unix::net::UnixStream::connect(address().with_extension("sock")) else { return false };
        let _ = stream.set_write_timeout(Some(std::time::Duration::from_secs(1)));
        writeln!(stream, "{line}").is_ok()
    }
    #[cfg(windows)]
    {
        send_to(&address().with_extension("addr"), line)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn what_a_link_asks_for() {
        assert_eq!(parse("ticket DEV-123"), Some(Asked::Ticket("DEV-123".into())));
        assert_eq!(parse(&format!("{SCHEME}://ticket/DEV-123")), Some(Asked::Ticket("DEV-123".into())));
        assert_eq!(parse(&format!("{SCHEME}://ticket?key=OPS-7&x=1")), Some(Asked::Ticket("OPS-7".into())));
        assert_eq!(parse(&format!("{SCHEME}://ticket/group/app%2345")), Some(Asked::Ticket("group/app#45".into())));
        assert_eq!(parse(&format!("{SCHEME}://ticket?key=group/app%2345")), Some(Asked::Ticket("group/app#45".into())));
        assert_eq!(parse("ticket $(rm -rf ~)"), None);
        assert_eq!(parse("other://ticket/DEV-1"), None);
        assert_eq!(parse(&format!("{SCHEME}://task/x")), None);
    }

    #[test]
    fn a_launcher_s_file_url() {
        let path = |s: &str| file_url_path(s).map(|p| p.to_string_lossy().into_owned());
        assert_eq!(path("file:///home/u/%D0%BF%D1%80%D0%BE%D0%B5%D0%BA%D1%82%D1%8B/my%20app").as_deref(), Some("/home/u/проекты/my app"));
        assert_eq!(path("file://localhost/a/100%25%23x").as_deref(), Some("/a/100%#x"));
        assert_eq!(path("file:///a/50%").as_deref(), Some("/a/50%"));
        assert_eq!(path("file://elsewhere/a"), None);
        assert_eq!(path("/home/u/app"), None);
    }

    #[cfg(unix)]
    #[test]
    fn a_line_reaches_the_window_that_listens() {
        let (tx, rx) = async_channel::unbounded();
        connect(tx);
        listen();
        assert!(send("ticket DEV-9"));
        let got = rx.recv_blocking().unwrap();
        assert_eq!(got, "ticket DEV-9");
    }
}
