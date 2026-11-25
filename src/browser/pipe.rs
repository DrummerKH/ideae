//! A Chrome spoken to over `--remote-debugging-pipe`: no port that any
//! process of this user (an agent's Playwright) could call, only the pipe
//! the app holds. One connection for the whole browser, messages parted by
//! `\0`: a reply goes to whoever sent its id, a page's event to whoever
//! listens to its session, the browser's own events to every channel.

use std::{
    collections::HashMap,
    io::{Read, Write},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
        mpsc,
    },
    time::Duration,
};

use anyhow::{Result, anyhow};
use serde_json::Value;

/// Where a channel's messages go; `Value::Null` says the channel is ended.
type Back = mpsc::Sender<Value>;

#[derive(Default)]
struct Routes {
    waiting: HashMap<u64, Back>,
    sessions: HashMap<String, Back>,
    channels: HashMap<u64, Back>,
    ended: bool,
}

pub struct Pipe {
    write: Mutex<Box<dyn Write + Send>>,
    next_id: AtomicU64,
    next_channel: AtomicU64,
    routes: Mutex<Routes>,
}

impl Pipe {
    /// Reads what Chrome writes on a thread of its own, until it ends.
    pub fn start(read: impl Read + Send + 'static, write: impl Write + Send + 'static) -> Arc<Pipe> {
        let pipe = Arc::new(Pipe { write: Mutex::new(Box::new(write)), next_id: AtomicU64::new(0), next_channel: AtomicU64::new(0), routes: Default::default() });
        let reader = pipe.clone();
        let _ = std::thread::Builder::new().name("cdp-pipe".into()).spawn(move || reader.read_all(read));
        pipe
    }

    fn read_all(&self, mut read: impl Read) {
        let mut split = Splitter::default();
        let mut chunk = vec![0u8; 1 << 16];
        loop {
            match read.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => split.feed(&chunk[..n], |bytes| {
                    if let Ok(msg) = serde_json::from_slice::<Value>(bytes) {
                        self.route(msg);
                    }
                }),
            }
        }
        self.end();
    }

    fn route(&self, msg: Value) {
        let mut routes = self.routes.lock().unwrap();
        if let Some(id) = msg["id"].as_u64() {
            if let Some(back) = routes.waiting.remove(&id) {
                let _ = back.send(msg);
            }
        } else if let Some(session) = msg["sessionId"].as_str() {
            if let Some(back) = routes.sessions.get(session) {
                let _ = back.send(msg);
            }
        } else {
            if msg["method"] == "Target.detachedFromTarget" {
                if let Some(session) = msg["params"]["sessionId"].as_str() {
                    routes.sessions.remove(session);
                }
            }
            for back in routes.channels.values() {
                let _ = back.send(msg.clone());
            }
        }
    }

    /// Chrome gone: everyone waiting is told.
    fn end(&self) {
        let mut guard = self.routes.lock().unwrap();
        let routes = &mut *guard;
        routes.ended = true;
        let all = routes.waiting.drain().map(|(_, b)| b).chain(routes.sessions.drain().map(|(_, b)| b)).chain(routes.channels.drain().map(|(_, b)| b));
        for back in all {
            let _ = back.send(Value::Null);
        }
    }

    fn next_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed) + 1
    }

    /// Writes a message whose reply goes to `back`. Whole, under the lock: a
    /// page's writer and another's never interleave.
    fn write(&self, msg: &Value, back: &Back) -> Result<()> {
        let id = msg["id"].as_u64().ok_or_else(|| anyhow!("a message with no id"))?;
        {
            let mut routes = self.routes.lock().unwrap();
            if routes.ended {
                return Err(anyhow!("Chrome ended"));
            }
            routes.waiting.insert(id, back.clone());
        }
        let mut bytes = serde_json::to_vec(msg)?;
        bytes.push(0);
        let written = self.write.lock().unwrap().write_all(&bytes);
        if let Err(e) = written {
            self.routes.lock().unwrap().waiting.remove(&id);
            return Err(anyhow!("writing to Chrome: {e}"));
        }
        Ok(())
    }

    /// A connection of its own over the pipe: its replies, the sessions it
    /// listens to, the browser's events.
    pub fn channel(self: &Arc<Self>) -> Result<Channel> {
        let (tx, rx) = mpsc::channel();
        let id = self.next_channel.fetch_add(1, Ordering::Relaxed);
        let mut routes = self.routes.lock().unwrap();
        if routes.ended {
            return Err(anyhow!("Chrome ended"));
        }
        routes.channels.insert(id, tx.clone());
        Ok(Channel { pipe: self.clone(), id, tx, rx, sessions: Vec::new() })
    }
}

pub struct Channel {
    pipe: Arc<Pipe>,
    id: u64,
    tx: Back,
    rx: mpsc::Receiver<Value>,
    sessions: Vec<String>,
}

impl Channel {
    /// Gives the message an id of the whole browser's and sends it.
    pub fn send(&self, msg: &mut Value) -> Result<u64> {
        let id = self.pipe.next_id();
        msg["id"] = id.into();
        self.pipe.write(msg, &self.tx)?;
        Ok(id)
    }

    /// The next message, none when `timeout` passes first; an error once
    /// Chrome or the channel ended.
    pub fn recv(&self, timeout: Option<Duration>) -> Result<Option<Value>> {
        let msg = match timeout {
            Some(timeout) => match self.rx.recv_timeout(timeout) {
                Ok(msg) => msg,
                Err(mpsc::RecvTimeoutError::Timeout) => return Ok(None),
                Err(mpsc::RecvTimeoutError::Disconnected) => Value::Null,
            },
            None => self.rx.recv().unwrap_or(Value::Null),
        };
        if msg.is_null() { Err(anyhow!("Chrome ended")) } else { Ok(Some(msg)) }
    }

    /// The events of a session it attached go to it.
    pub fn listen(&mut self, session: &str) {
        self.pipe.routes.lock().unwrap().sessions.insert(session.to_string(), self.tx.clone());
        self.sessions.push(session.to_string());
    }

    /// Writes for this channel from another thread; its replies come here.
    pub fn writer(&self) -> Writer {
        Writer { pipe: self.pipe.clone(), back: self.tx.clone() }
    }
}

impl Drop for Channel {
    fn drop(&mut self) {
        let mut routes = self.pipe.routes.lock().unwrap();
        routes.channels.remove(&self.id);
        for session in &self.sessions {
            routes.sessions.remove(session);
        }
    }
}

pub struct Writer {
    pipe: Arc<Pipe>,
    back: Back,
}

impl Writer {
    pub fn next_id(&self) -> u64 {
        self.pipe.next_id()
    }

    pub fn write(&self, msg: &Value) -> bool {
        self.pipe.write(msg, &self.back).is_ok()
    }

    /// Ends its channel's wait for messages, as a page closes.
    pub fn end(&self) {
        let _ = self.back.send(Value::Null);
    }
}

/// The messages of a `\0`-parted stream, as reads cut it.
#[derive(Default)]
struct Splitter {
    buf: Vec<u8>,
    /// How far no `\0` was found: a large message is not looked through again for each read.
    scanned: usize,
}

impl Splitter {
    fn feed(&mut self, chunk: &[u8], mut each: impl FnMut(&[u8])) {
        self.buf.extend_from_slice(chunk);
        let mut start = 0;
        while let Some(at) = self.buf[self.scanned..].iter().position(|b| *b == 0) {
            let end = self.scanned + at;
            each(&self.buf[start..end]);
            start = end + 1;
            self.scanned = start;
        }
        if start > 0 {
            self.buf.drain(..start);
            self.scanned -= start;
        }
        self.scanned = self.buf.len();
    }
}

/// Two pipes for Chrome's fds 3 (what it reads) and 4 (what it writes),
/// made so the command puts them there: what the app keeps, and Chrome's
/// ends, to be closed here once it started.
#[cfg(unix)]
pub fn make(command: &mut std::process::Command) -> Result<((std::io::PipeReader, std::io::PipeWriter), (std::io::PipeReader, std::io::PipeWriter))> {
    use std::os::fd::AsRawFd as _;
    use std::os::unix::process::CommandExt as _;
    let (chrome_reads, app_writes) = std::io::pipe()?;
    let (app_reads, chrome_writes) = std::io::pipe()?;
    let (r, w) = (chrome_reads.as_raw_fd(), chrome_writes.as_raw_fd());
    // SAFETY: only fcntl and dup2, safe between fork and exec. Both are moved
    // above 4 first, so one already at 3 or 4 is not overwritten by the other;
    // dup2 leaves 3 and 4 open across exec, the copies above close there.
    unsafe {
        command.pre_exec(move || {
            let r = libc::fcntl(r, libc::F_DUPFD_CLOEXEC, 10);
            let w = libc::fcntl(w, libc::F_DUPFD_CLOEXEC, 10);
            if r < 0 || w < 0 || libc::dup2(r, 3) < 0 || libc::dup2(w, 4) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    Ok(((app_reads, app_writes), (chrome_reads, chrome_writes)))
}

/// The same on Windows, where Chrome takes its pipes as handles
/// (`--remote-debugging-io-pipes=<read>,<write>`, Chrome 117 on, as
/// ChromeDriver starts it): two anonymous pipes, Chrome's ends inherited.
/// Not tried on a live Windows yet.
#[cfg(windows)]
pub fn make(command: &mut std::process::Command) -> Result<((std::fs::File, std::fs::File), (std::os::windows::io::OwnedHandle, std::os::windows::io::OwnedHandle))> {
    use std::os::windows::io::{FromRawHandle as _, OwnedHandle};
    use windows_sys::Win32::Foundation::{HANDLE, HANDLE_FLAG_INHERIT, SetHandleInformation};
    use windows_sys::Win32::System::Pipes::CreatePipe;
    let pipe = || -> Result<(OwnedHandle, OwnedHandle)> {
        let (mut read, mut write): (HANDLE, HANDLE) = (std::ptr::null_mut(), std::ptr::null_mut());
        // SAFETY: two handles made here, owned from now on.
        unsafe {
            if CreatePipe(&mut read, &mut write, std::ptr::null(), 0) == 0 {
                return Err(anyhow!("CreatePipe: {}", std::io::Error::last_os_error()));
            }
            Ok((OwnedHandle::from_raw_handle(read), OwnedHandle::from_raw_handle(write)))
        }
    };
    let (chrome_reads, app_writes) = pipe()?;
    let (app_reads, chrome_writes) = pipe()?;
    for end in [&chrome_reads, &chrome_writes] {
        use std::os::windows::io::AsRawHandle as _;
        // SAFETY: a handle owned here; only whether a child inherits it changes.
        if unsafe { SetHandleInformation(end.as_raw_handle(), HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT) } == 0 {
            return Err(anyhow!("SetHandleInformation: {}", std::io::Error::last_os_error()));
        }
    }
    {
        use std::os::windows::io::AsRawHandle as _;
        command.arg(format!("--remote-debugging-io-pipes={},{}", chrome_reads.as_raw_handle() as usize, chrome_writes.as_raw_handle() as usize));
    }
    Ok(((std::fs::File::from(app_reads), std::fs::File::from(app_writes)), (chrome_reads, chrome_writes)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn split(chunks: &[&[u8]]) -> Vec<String> {
        let mut split = Splitter::default();
        let mut out = Vec::new();
        for chunk in chunks {
            split.feed(chunk, |m| out.push(String::from_utf8(m.to_vec()).unwrap()));
        }
        out
    }

    #[test]
    fn messages_however_reads_cut_them() {
        assert_eq!(split(&[b"{\"a\":1}\0{\"b\":2}\0"]), ["{\"a\":1}", "{\"b\":2}"]);
        assert_eq!(split(&[b"{\"a\"", b":1}\0{\"b", b"\":2}", b"\0"]), ["{\"a\":1}", "{\"b\":2}"]);
        assert_eq!(split(&[b"{}\0", b"", b"\0{}"]), ["{}", ""]);
        // A screenshot's worth, a byte short of its end in each read.
        let big = format!("{{\"data\":\"{}\"}}", "x".repeat(3 << 20));
        let bytes = [big.as_bytes(), b"\0"].concat();
        let parts: Vec<&[u8]> = bytes.chunks(65536).collect();
        assert_eq!(split(&parts), [big]);
    }

    /// A stand-in Chrome: answers each command with its method, and an
    /// attach with a session that then sends an event of its own.
    fn fake() -> (Arc<Pipe>, std::io::PipeWriter, std::thread::JoinHandle<()>) {
        let (chrome_reads, app_writes) = std::io::pipe().unwrap();
        let (app_reads, chrome_writes) = std::io::pipe().unwrap();
        let events = chrome_writes.try_clone().unwrap();
        let chrome = std::thread::spawn(move || {
            let mut out = chrome_writes;
            let mut read = chrome_reads;
            let mut split = Splitter::default();
            let mut chunk = [0u8; 4096];
            let say = |out: &mut std::io::PipeWriter, v: Value| out.write_all(&[serde_json::to_vec(&v).unwrap(), vec![0]].concat()).unwrap();
            while let Ok(n) = read.read(&mut chunk) {
                if n == 0 {
                    break;
                }
                let mut got = Vec::new();
                split.feed(&chunk[..n], |m| got.push(serde_json::from_slice::<Value>(m).unwrap()));
                for msg in got {
                    let method = msg["method"].as_str().unwrap().to_string();
                    if method == "Target.attachToTarget" {
                        let session = format!("S-{}", msg["params"]["targetId"].as_str().unwrap());
                        say(&mut out, json!({ "id": msg["id"], "result": { "sessionId": session } }));
                    } else if method == "Ping" {
                        say(&mut out, json!({ "method": "Page.loadEventFired", "sessionId": msg["sessionId"], "params": {} }));
                        say(&mut out, json!({ "id": msg["id"], "result": { "method": method } }));
                    } else {
                        say(&mut out, json!({ "id": msg["id"], "result": { "method": method } }));
                    }
                }
            }
        });
        (Pipe::start(app_reads, app_writes), events, chrome)
    }

    fn call(channel: &Channel, msg: Value) -> Value {
        let mut msg = msg;
        let id = channel.send(&mut msg).unwrap();
        loop {
            let got = channel.recv(Some(Duration::from_secs(5))).unwrap().expect("a reply in time");
            if got["id"].as_u64() == Some(id) {
                return got;
            }
        }
    }

    #[test]
    fn each_channel_has_its_replies_and_its_sessions() {
        let (pipe, mut events, chrome) = fake();
        let mut a = pipe.channel().unwrap();
        let b = pipe.channel().unwrap();
        let session = call(&a, json!({ "method": "Target.attachToTarget", "params": { "targetId": "A" } }))["result"]["sessionId"].as_str().unwrap().to_string();
        a.listen(&session);
        assert_eq!(call(&b, json!({ "method": "Browser.getVersion" }))["result"]["method"], "Browser.getVersion");
        // A's session's event comes to A, before its reply; none of it to B.
        let mut ping = json!({ "method": "Ping", "sessionId": session });
        a.send(&mut ping).unwrap();
        let first = a.recv(Some(Duration::from_secs(5))).unwrap().unwrap();
        assert_eq!(first["method"], "Page.loadEventFired");
        assert_eq!(a.recv(Some(Duration::from_secs(5))).unwrap().unwrap()["result"]["method"], "Ping");
        assert!(b.recv(Some(Duration::from_millis(100))).unwrap().is_none());
        // The browser's own events go to both.
        events.write_all(b"{\"method\":\"Target.targetCreated\",\"params\":{}}\0").unwrap();
        assert_eq!(a.recv(Some(Duration::from_secs(5))).unwrap().unwrap()["method"], "Target.targetCreated");
        assert_eq!(b.recv(Some(Duration::from_secs(5))).unwrap().unwrap()["method"], "Target.targetCreated");
        // A writer's replies come to its channel's reader.
        let writer = b.writer();
        let id = writer.next_id();
        assert!(writer.write(&json!({ "id": id, "method": "Page.enable" })));
        assert_eq!(b.recv(Some(Duration::from_secs(5))).unwrap().unwrap()["id"], id);
        writer.end();
        assert!(b.recv(None).is_err());
        drop((events, chrome));
    }

    #[test]
    fn chrome_s_end_ends_every_wait() {
        let (chrome_reads, app_writes) = std::io::pipe().unwrap();
        let (app_reads, chrome_writes) = std::io::pipe().unwrap();
        let pipe = Pipe::start(app_reads, app_writes);
        let a = pipe.channel().unwrap();
        let mut msg = json!({ "method": "Browser.getVersion" });
        a.send(&mut msg).unwrap();
        drop((chrome_reads, chrome_writes));
        assert!(a.recv(Some(Duration::from_secs(5))).is_err());
        assert!(a.send(&mut json!({ "method": "x" })).is_err());
        assert!(pipe.channel().is_err());
    }
}
