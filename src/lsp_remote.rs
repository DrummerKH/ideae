//! A language server of a remote task ([`crate::hosts`]): run there in a
//! pipe session of its host's daemon, spoken to through it. Its messages
//! name files as they are there (`file:///home/…`); the window names them
//! by their keys (`file://ssh:/box/home/…`): each message is rewritten at
//! this edge, both ways, and nothing else of the client knows.

use std::io::{self, Read, Write};
use std::path::Path;
use std::sync::Arc;

use serde_json::Value;

use crate::daemon::client::{Daemon, PipeEvent};

/// The URI prefixes of a host: as the window writes them, as its servers do.
#[derive(Clone)]
pub struct Remap {
    here: String,
}

const THERE: &str = "file://";

impl Remap {
    pub fn of(host: &str) -> Self {
        Remap { here: crate::lsp::path_to_uri(Path::new(&format!("ssh:/{host}"))) }
    }

    /// `file://ssh:/box/a` → `file:///a`.
    fn outgoing(&self, text: &str) -> Option<String> {
        let rest = text.strip_prefix(&self.here).filter(|rest| rest.starts_with('/'))?;
        Some(format!("{THERE}{rest}"))
    }

    /// `file:///a` → `file://ssh:/box/a`.
    fn incoming(&self, text: &str) -> Option<String> {
        let rest = text.strip_prefix(THERE).filter(|rest| rest.starts_with('/'))?;
        Some(format!("{}{rest}", self.here))
    }

    /// Every string and every key of `value` that is a URI of the other side, made one of this.
    fn rewrite(value: &mut Value, how: &dyn Fn(&str) -> Option<String>) {
        match value {
            Value::String(s) => {
                if let Some(new) = how(s) {
                    *s = new;
                }
            }
            Value::Array(items) => items.iter_mut().for_each(|item| Self::rewrite(item, how)),
            Value::Object(map) => {
                // Keyed by URI: a workspace edit's changes.
                if map.keys().any(|k| how(k).is_some()) {
                    let old = std::mem::take(map);
                    for (k, v) in old {
                        map.insert(how(&k).unwrap_or(k), v);
                    }
                }
                map.values_mut().for_each(|v| Self::rewrite(v, how));
            }
            _ => {}
        }
    }

    fn to_server(&self, mut message: Value) -> Value {
        Self::rewrite(&mut message, &|s| self.outgoing(s));
        // This process is not one of that machine's: a server that watches it would end.
        if message["method"] == "initialize" {
            message["params"]["processId"] = Value::Null;
        }
        message
    }

    fn from_server(&self, mut message: Value) -> Value {
        Self::rewrite(&mut message, &|s| self.incoming(s));
        message
    }
}

/// `Content-Length` frames out of `buf`, as they are whole: each message.
fn frames(buf: &mut Vec<u8>) -> Vec<Value> {
    let mut found = Vec::new();
    loop {
        let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") else { break };
        let head = String::from_utf8_lossy(&buf[..end]).into_owned();
        // Anywhere in it: what a server printed before its first message is no header.
        let length = head.rfind("Content-Length:").and_then(|at| head.get(at + "Content-Length:".len()..)).and_then(|v| v.lines().next()).and_then(|v| v.trim().parse::<usize>().ok());
        let Some(length) = length else {
            // Not a header: what came before the next one is dropped.
            buf.drain(..end + 4);
            continue;
        };
        if buf.len() < end + 4 + length {
            break;
        }
        let body: Vec<u8> = buf.drain(..end + 4 + length).skip(end + 4).collect();
        if let Ok(message) = serde_json::from_slice(&body) {
            found.push(message);
        }
    }
    found
}

fn framed(message: &Value) -> Vec<u8> {
    let body = serde_json::to_vec(message).unwrap_or_default();
    let mut out = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
    out.extend_from_slice(&body);
    out
}

/// What the client writes, rewritten and sent to the session there.
pub struct Writer {
    daemon: Arc<Daemon>,
    id: String,
    remap: Remap,
    buf: Vec<u8>,
}

impl Write for Writer {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.buf.extend_from_slice(bytes);
        for message in frames(&mut self.buf) {
            self.daemon.write(&self.id, &framed(&self.remap.to_server(message)));
        }
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// What the session there prints, rewritten for the client. Its end, or
/// the link coming back (messages may have been lost meanwhile), is the
/// server's end: the client is started again, and that session ended.
pub struct Reader {
    events: async_channel::Receiver<PipeEvent>,
    daemon: Arc<Daemon>,
    id: String,
    remap: Remap,
    raw: Vec<u8>,
    ready: Vec<u8>,
}

impl Read for Reader {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        while self.ready.is_empty() {
            match self.events.recv_blocking() {
                Ok(PipeEvent::Bytes(bytes)) => {
                    self.raw.extend_from_slice(&bytes);
                    for message in frames(&mut self.raw) {
                        self.ready.extend(framed(&self.remap.from_server(message)));
                    }
                }
                Ok(PipeEvent::Reattached) => {
                    self.daemon.kill(&self.id);
                    return Ok(0);
                }
                Ok(PipeEvent::Exited) | Err(_) => return Ok(0),
            }
        }
        let n = out.len().min(self.ready.len());
        out[..n].copy_from_slice(&self.ready[..n]);
        self.ready.drain(..n);
        Ok(n)
    }
}

/// Starts `command` with `args` in `cwd` on the host of `root`: the pipes
/// of the session there, and what ends it.
pub fn start(program: &str, command: &Path, args: &[&str], root: &Path, cwd: &Path) -> io::Result<(Writer, Reader, Box<dyn FnOnce() + Send>)> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let (host, _) = crate::hosts::split(root).ok_or_else(|| io::Error::other("not a remote task"))?;
    let daemon = crate::daemon::client::for_path(root).ok_or_else(|| io::Error::new(io::ErrorKind::NotConnected, "its host is not connected"))?;
    let id = format!("{}#lsp-{program}-{}", root.display(), NEXT.fetch_add(1, Ordering::Relaxed));
    let mut line = vec![command.display().to_string()];
    line.extend(args.iter().map(|a| a.to_string()));
    // As a local one is started: jdtls with an index of the project's own,
    // in the home there; a Java server with its options.
    if program == "jdtls" {
        use sha2::Digest as _;
        let hash: String = sha2::Sha256::digest(crate::hosts::there(root).to_string_lossy().as_bytes()).iter().take(8).map(|b| format!("{b:02x}")).collect();
        let mut shell = vec!["sh".to_string(), "-c".into(), format!(r#"exec "$@" -data "$HOME/.local/share/ideae/jdtls/{hash}""#), "sh".into()];
        shell.extend(line);
        line = shell;
    }
    let env = crate::lsp::java_options(program, None).map(|o| ("JAVA_TOOL_OPTIONS".to_string(), o.to_string_lossy().into_owned())).into_iter().collect();
    let spec = crate::daemon::proto::Spec { cwd: crate::hosts::there(cwd), env, input: None, program: line };
    let events = daemon.pipe(id.clone(), spec);
    let remap = Remap::of(&host);
    let writer = Writer { daemon: daemon.clone(), id: id.clone(), remap: remap.clone(), buf: Vec::new() };
    let reader = Reader { events, daemon: daemon.clone(), id: id.clone(), remap, raw: Vec::new(), ready: Vec::new() };
    let stop = Box::new(move || daemon.kill(&id));
    Ok((writer, reader, stop))
}

/// Where `program` is on the host of `path`, as its login shell finds it
/// (its PATH: `~/.cargo/bin` and the like). Asked once a run.
pub fn locate(path: &Path, program: &str) -> Option<std::path::PathBuf> {
    use std::{collections::HashMap, sync::Mutex};
    static KNOWN: Mutex<Option<HashMap<(String, String), Option<std::path::PathBuf>>>> = Mutex::new(None);
    let (host, _) = crate::hosts::split(path)?;
    let key = (host, program.to_string());
    if let Some(known) = KNOWN.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert_with(HashMap::new).get(&key) {
        return known.clone();
    }
    let at = path.parent().unwrap_or(path);
    let asked = crate::hostfs::run(at, "sh", &["-c", r#"exec "${SHELL:-/bin/sh}" -lc "command -v $1" 2>/dev/null"#, "sh", program], &[], None);
    let found = asked.ok().filter(|out| out.success()).and_then(|out| {
        let printed = String::from_utf8_lossy(&out.stdout).lines().last().unwrap_or_default().trim().to_string();
        printed.starts_with('/').then(|| std::path::PathBuf::from(printed))
    });
    // Found is kept; not found is asked again next time (it may be installed meanwhile).
    if found.is_some() {
        KNOWN.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert_with(HashMap::new).insert(key, found.clone());
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_message_names_files_as_each_side_does() {
        let remap = Remap::of("me@box:2222");
        let here = crate::lsp::path_to_uri(&crate::hosts::join("me@box:2222", Path::new("/home/me/a b.rs")));
        let asked = json!({"method": "initialize", "params": {"processId": 7, "rootUri": here, "other": "file://ssh:/me@box:22220/x"}});
        let sent = remap.to_server(asked);
        assert_eq!(sent["params"]["rootUri"], "file:///home/me/a%20b.rs");
        assert_eq!(sent["params"]["other"], "file://ssh:/me@box:22220/x", "another host's is no file of this one");
        assert!(sent["params"]["processId"].is_null());
        let told = json!({"result": {"changes": {"file:///home/me/a%20b.rs": [{"newText": "x"}]}, "uri": "file:///home/me/a%20b.rs", "text": "file:///"}});
        let got = remap.from_server(told);
        assert_eq!(got["result"]["uri"], here.as_str());
        assert!(got["result"]["changes"].get(here.as_str()).is_some(), "keys too");
        assert_eq!(crate::lsp::uri_to_path(got["result"]["uri"].as_str().unwrap()), Some(crate::hosts::join("me@box:2222", Path::new("/home/me/a b.rs"))));
    }

    #[test]
    fn frames_are_read_whole_and_written_again() {
        let one = framed(&json!({"id": 1}));
        let mut buf = one.clone();
        buf.extend_from_slice(&one[..one.len() - 3]);
        assert_eq!(frames(&mut buf), [json!({"id": 1})]);
        buf.extend_from_slice(&one[one.len() - 3..]);
        assert_eq!(frames(&mut buf), [json!({"id": 1})]);
        assert!(buf.is_empty());
        // What a server printed before its first message is left behind.
        let mut buf = b"starting up... ".to_vec();
        buf.extend_from_slice(&one);
        assert_eq!(frames(&mut buf), [json!({"id": 1})]);
    }
}
