//! A daemon on another machine, reached by ssh: `ssh host ideae --daemon
//! --stdio` (see [`super::bridge`]) carries the same frames as the local
//! socket. The link drops (sleep, another network): the agents run on
//! there, and the window dials again until it is back, its terminals
//! attaching anew.

use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use super::client::Daemon;
use super::proto::{self, Frame, Hook, Msg};

/// Where a daemon runs and how it is reached.
#[derive(Clone, Debug)]
pub struct Remote {
    /// ssh and its arguments up to the host, as typed: `ssh -p 2222 me@box`.
    pub ssh: Vec<String>,
    /// The ideae there, as its shell finds it.
    pub ideae: String,
    /// How often the daemon is asked whether it is there; three times this
    /// without a word, and the link is dropped and dialled again.
    pub ping: Duration,
}

impl Remote {
    pub fn new(ssh: Vec<String>) -> Self {
        Remote { ssh, ideae: "ideae".into(), ping: Duration::from_secs(5) }
    }

    /// The host, for messages.
    pub fn name(&self) -> &str {
        self.ssh.last().map_or("", String::as_str)
    }
}

/// The link of a remote [`Daemon`]: the ssh process of the moment and
/// when the daemon was last heard.
pub struct Wire {
    remote: Remote,
    child: Mutex<Option<Child>>,
    heard: Mutex<Instant>,
    /// The daemon answers pings (it said so on welcome).
    pings: AtomicBool,
    up: AtomicBool,
    /// Where `ideae --hook` reports on that machine.
    socket: Mutex<Option<String>>,
}

impl Wire {
    pub(super) fn heard(&self) {
        *self.heard.lock().unwrap() = Instant::now();
    }

    /// Ends the ssh process: its output ends, and the reader dials again
    /// (or stops, when the window is done with the daemon).
    pub(super) fn drop_link(&self) {
        self.up.store(false, Ordering::SeqCst);
        if let Some(mut child) = self.child.lock().unwrap().take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    pub fn is_up(&self) -> bool {
        self.up.load(Ordering::SeqCst)
    }

    pub fn socket(&self) -> Option<String> {
        self.socket.lock().unwrap().clone()
    }

    /// Cuts the link as a network would: the window dials again.
    #[cfg(test)]
    pub(super) fn drop_link_for_test(&self) {
        if let Some(child) = self.child.lock().unwrap().as_mut() {
            let _ = child.kill();
        }
    }

    #[cfg(test)]
    fn pid(&self) -> Option<i32> {
        self.child.lock().unwrap().as_ref().map(|c| c.id() as i32)
    }
}

/// One dialled link, said hello on.
struct Dialled {
    child: Child,
    writer: Box<dyn Write + Send>,
    reader: Box<dyn Read + Send>,
    hooks: Vec<Hook>,
    sessions: Vec<String>,
    ping: bool,
    socket: Option<String>,
}

/// How long ssh gets to log in and the daemon to say hello.
const HANDSHAKE: Duration = Duration::from_secs(20);
/// The most a dropped link waits before it is dialled again.
const MOST_BACKOFF: Duration = Duration::from_secs(30);

/// Connects to the daemon on `remote`, starting it there when none runs.
/// The first dial's failure is returned (a wrong host, a refused key);
/// once connected, a dropped link is dialled again without end.
pub fn connect(remote: Remote, on_hook: impl Fn(Hook) + Send + Sync + 'static) -> Result<(Arc<Daemon>, Vec<Hook>), String> {
    let first = dial(&remote)?;
    let wire = Wire {
        remote,
        child: Mutex::new(Some(first.child)),
        heard: Mutex::new(Instant::now()),
        pings: AtomicBool::new(first.ping),
        up: AtomicBool::new(true),
        socket: Mutex::new(first.socket),
    };
    let daemon = Arc::new(Daemon::remote(wire, queued(first.writer), first.sessions));
    let reading = daemon.clone();
    let mut reader = first.reader;
    std::thread::Builder::new()
        .name("remote-daemon-reader".into())
        .spawn(move || {
            let wire = reading.wire().expect("a remote daemon has a wire");
            loop {
                while let Ok(frame) = proto::read_frame(&mut reader) {
                    wire.heard();
                    reading.on_frame(frame, &on_hook);
                }
                wire.drop_link();
                reading.drop_calls();
                if reading.quitting() {
                    return;
                }
                reading.tell_terminals(&format!("\r\n[ideae: lost {}; dialling again]\r\n", wire.remote.name()));
                let mut backoff = Duration::from_millis(500);
                let next = loop {
                    if reading.quitting() {
                        return;
                    }
                    match dial(&wire.remote) {
                        Ok(next) => break next,
                        Err(_) => {
                            std::thread::sleep(backoff);
                            backoff = (backoff * 2).min(MOST_BACKOFF);
                        }
                    }
                };
                *wire.child.lock().unwrap() = Some(next.child);
                wire.pings.store(next.ping, Ordering::Relaxed);
                *wire.socket.lock().unwrap() = next.socket;
                wire.heard();
                // The new writer first: a call made once the link is up goes through it.
                reading.set_writer(queued(next.writer));
                wire.up.store(true, Ordering::SeqCst);
                reader = next.reader;
                // A window that let go meanwhile (`disconnect`) does not keep this one.
                if reading.quitting() {
                    wire.drop_link();
                    return;
                }
                reading.reopen();
                for hook in next.hooks {
                    on_hook(hook);
                }
            }
        })
        .map_err(|e| e.to_string())?;
    let pinging = Arc::downgrade(&daemon);
    std::thread::Builder::new()
        .name("remote-daemon-ping".into())
        .spawn(move || {
            loop {
                let Some(daemon) = pinging.upgrade() else { return };
                let wire = daemon.wire().expect("a remote daemon has a wire");
                let every = wire.remote.ping;
                if daemon.quitting() {
                    return;
                }
                if wire.is_up() && wire.pings.load(Ordering::Relaxed) {
                    // Quiet for three rounds: the link hangs (a laptop that
                    // slept, a network gone without a word). Dropped, it
                    // is dialled again.
                    if wire.heard.lock().unwrap().elapsed() > every * 3 {
                        wire.drop_link();
                    } else {
                        daemon.send(proto::encode_msg(&Msg::Ping));
                    }
                }
                drop(daemon);
                std::thread::sleep(every);
            }
        })
        .map_err(|e| e.to_string())?;
    Ok((daemon.clone(), first.hooks))
}

/// Writes go through a thread: a link that hangs holds that thread, not
/// whoever sends (the UI thread, typing into a terminal).
fn queued(mut writer: Box<dyn Write + Send>) -> Box<dyn Write + Send> {
    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    let waiting: Arc<std::sync::atomic::AtomicUsize> = Arc::default();
    let written = waiting.clone();
    std::thread::spawn(move || {
        while let Ok(frame) = rx.recv() {
            let sent = writer.write_all(&frame).and_then(|()| writer.flush());
            written.fetch_sub(1, Ordering::Relaxed);
            if sent.is_err() {
                // What is still queued never goes.
                written.store(0, Ordering::Relaxed);
                return;
            }
        }
    });
    Box::new(Queued { tx, waiting })
}

struct Queued {
    tx: mpsc::Sender<Vec<u8>>,
    waiting: Arc<std::sync::atomic::AtomicUsize>,
}

impl Write for Queued {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.waiting.fetch_add(1, Ordering::Relaxed);
        if self.tx.send(buf.to_vec()).is_err() {
            self.waiting.fetch_sub(1, Ordering::Relaxed);
            return Err(std::io::Error::from(std::io::ErrorKind::BrokenPipe));
        }
        Ok(buf.len())
    }

    /// Waits a moment for what is queued to be written: the app quits
    /// right after telling a daemon to end.
    fn flush(&mut self) -> std::io::Result<()> {
        let until = Instant::now() + Duration::from_secs(1);
        while self.waiting.load(Ordering::Relaxed) > 0 && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(10));
        }
        Ok(())
    }
}

fn dial(remote: &Remote) -> Result<Dialled, String> {
    let program = remote.ssh.first().ok_or("no ssh command")?;
    let mut command = ssh(remote, &format!("{} --daemon --stdio", remote.ideae))?;
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().map_err(|e| format!("{program}: {e}"))?;
    let stdin = child.stdin.take().ok_or("no stdin")?;
    let stdout = child.stdout.take().ok_or("no stdout")?;
    let stderr = child.stderr.take().ok_or("no stderr")?;
    let said = Arc::new(Mutex::new(Vec::<u8>::new()));
    let keeping = said.clone();
    std::thread::spawn(move || {
        let mut stderr = stderr;
        let mut chunk = [0u8; 4096];
        while let Ok(n @ 1..) = stderr.read(&mut chunk) {
            let mut said = keeping.lock().unwrap();
            said.extend_from_slice(&chunk[..n]);
            let over = said.len().saturating_sub(8192);
            said.drain(..over);
        }
    });

    // A login that hangs (a host that does not answer) ends at the deadline.
    let pid = child.id();
    let greeted = Arc::new(AtomicBool::new(false));
    let waiting = greeted.clone();
    std::thread::spawn(move || {
        let deadline = Instant::now() + HANDSHAKE;
        while Instant::now() < deadline {
            if waiting.load(Ordering::Relaxed) {
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        if !waiting.load(Ordering::Relaxed) {
            // SAFETY: kill only sends a signal; the child is not waited for
            // yet, so the pid is still its.
            unsafe { libc::kill(pid as i32, libc::SIGKILL) };
        }
    });
    let fail = |child: &mut Child, what: &str| {
        greeted.store(true, Ordering::Relaxed);
        let _ = child.kill();
        let _ = child.wait();
        let said = String::from_utf8_lossy(&said.lock().unwrap()).trim().to_string();
        if said.is_empty() { what.to_string() } else { format!("{what}: {said}") }
    };

    let mut reader = BufReader::new(stdout);
    if !skip_to_mark(&mut reader) {
        return Err(fail(&mut child, "no ideae daemon answered"));
    }
    let mut writer = stdin;
    if proto::write_msg(&mut writer, &Msg::Hello { protocol: proto::PROTOCOL, version: crate::build_watch::build_id() }).is_err() {
        return Err(fail(&mut child, "the link closed"));
    }
    match proto::read_frame(&mut reader) {
        Ok(Frame::Msg(Msg::Welcome { protocol, hooks, sessions, ping, socket, .. })) if protocol == proto::PROTOCOL => {
            greeted.store(true, Ordering::Relaxed);
            Ok(Dialled { child, writer: Box::new(writer), reader: Box::new(reader), hooks, sessions, ping, socket })
        }
        Ok(Frame::Msg(Msg::Welcome { protocol, .. })) => {
            Err(fail(&mut child, &format!("the daemon there speaks protocol {protocol}, this window {}", proto::PROTOCOL)))
        }
        _ => Err(fail(&mut child, "the daemon there did not say hello")),
    }
}

/// What `line` printed, run on the host; its error output on failure.
pub fn run(remote: &Remote, line: &str, input: Option<&[u8]>) -> Result<String, String> {
    let mut command = ssh(remote, line)?;
    command.stdin(if input.is_some() { Stdio::piped() } else { Stdio::null() }).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = command.spawn().map_err(|e| format!("ssh: {e}"))?;
    if let (Some(input), Some(mut stdin)) = (input, child.stdin.take()) {
        // The host closes it early when the file is there already.
        let _ = stdin.write_all(input);
    }
    let out = child.wait_with_output().map_err(|e| format!("ssh: {e}"))?;
    if !out.status.success() {
        let said = String::from_utf8_lossy(&out.stderr).trim().to_string();
        return Err(if said.is_empty() { format!("ssh {} failed", remote.name()) } else { said });
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// ssh to the host, running `line` in its shell there.
pub(super) fn ssh(remote: &Remote, line: &str) -> Result<std::process::Command, String> {
    let (program, rest) = remote.ssh.split_first().ok_or("no ssh command")?;
    let (host, options) = rest.split_last().ok_or("no host")?;
    let mut command = crate::procs::command(program);
    command
        .args(options)
        // No questions nobody can answer, and a link that hangs is told
        // by ssh too.
        .args(["-T", "-o", "BatchMode=yes", "-o", "ServerAliveInterval=5", "-o", "ServerAliveCountMax=3"])
        .arg(host)
        .arg("--")
        .arg(line);
    Ok(command)
}

/// Reads up to the bridge's mark; what a login shell printed before it goes.
fn skip_to_mark(reader: &mut impl BufRead) -> bool {
    let mark = super::bridge::MARK;
    let mut tail: Vec<u8> = Vec::with_capacity(mark.len());
    let mut seen = 0usize;
    let mut byte = [0u8; 1];
    while seen < 1 << 20 {
        match reader.read(&mut byte) {
            Ok(1) => {}
            _ => return false,
        }
        seen += 1;
        if tail.len() == mark.len() {
            tail.remove(0);
        }
        tail.push(byte[0]);
        if tail == mark {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::client::PipeEvent;
    use crate::daemon::proto::Spec;

    /// The bridge, as ssh runs it on the other machine: the test binary run
    /// again by the stand-in ssh of the test below.
    #[test]
    #[ignore]
    fn bridge_process() {
        if std::env::var_os("IDEAE_TEST_BRIDGE").is_some() {
            super::super::bridge::run();
        }
    }

    /// Text a pipe session printed, until `want` shows (with no `want`,
    /// until it is attached again) or time is up.
    fn until(events: &async_channel::Receiver<PipeEvent>, want: &str, reattached: &mut bool) -> String {
        let deadline = Instant::now() + Duration::from_secs(20);
        let mut got = String::new();
        while Instant::now() < deadline && !(if want.is_empty() { *reattached } else { got.contains(want) }) {
            match events.try_recv() {
                Ok(PipeEvent::Bytes(bytes)) => got.push_str(&String::from_utf8_lossy(&bytes)),
                Ok(PipeEvent::Reattached) => *reattached = true,
                Ok(PipeEvent::Exited) => panic!("the session ended: {got}"),
                Err(_) => std::thread::sleep(Duration::from_millis(20)),
            }
        }
        got
    }

    /// A daemon on "another machine" (a stand-in ssh running the bridge):
    /// a session runs there; the link is cut, then hangs without a word;
    /// each time the window dials again and finds the session running. A
    /// window that lets go leaves it running.
    ///
    /// `cargo test -- --ignored a_remote_daemon` after changing the bridge or the remote link.
    #[test]
    #[ignore]
    fn a_remote_daemon_outlives_a_dropped_link() {
        let _one = crate::workspace::variants::tests::ONE_WINDOW.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("ideae-remote-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // SAFETY: set before anything of the daemon reads it.
        unsafe { std::env::set_var("IDEAE_STATE", dir.join("state.json")) };
        std::thread::spawn(|| crate::daemon::server::run(false));

        // ssh to "box": a greeting, as a login shell may print, then the bridge.
        let ssh = dir.join("ssh");
        let exe = std::env::current_exe().unwrap();
        std::fs::write(
            &ssh,
            format!(
                "#!/bin/sh\necho \"$*\" >> '{args}'\necho 'Welcome to box'\nexec env IDEAE_TEST_BRIDGE=1 '{exe}' --exact daemon::remote::tests::bridge_process --ignored --nocapture --test-threads=1\n",
                args = dir.join("args").display(),
                exe = exe.display()
            ),
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&ssh, std::fs::Permissions::from_mode(0o755)).unwrap();
        let remote = Remote { ssh: vec![ssh.display().to_string(), "box".into()], ideae: "ideae".into(), ping: Duration::from_millis(300) };
        let (daemon, _) = connect(remote, |_| {}).expect("connected");
        let args = std::fs::read_to_string(dir.join("args")).unwrap();
        assert!(args.contains("BatchMode=yes") && args.ends_with("box -- ideae --daemon --stdio\n"), "{args}");

        let id = "task#echo".to_string();
        let spec = Spec { cwd: dir.clone(), env: Vec::new(), input: None, program: vec!["sh".into(), "-c".into(), "while read l; do echo got:$l; done".into()] };
        let events = daemon.pipe(id.clone(), spec);
        let mut reattached = false;
        daemon.write(&id, b"one\n");
        assert!(until(&events, "got:one", &mut reattached).contains("got:one"));

        // Cut: ssh dies.
        let wire = daemon.wire().unwrap();
        let first = wire.pid().unwrap();
        unsafe { libc::kill(first, libc::SIGKILL) };
        let _ = until(&events, "", &mut reattached);
        assert!(reattached, "attached again after the link was cut");
        assert_ne!(wire.pid(), Some(first));
        daemon.write(&id, b"two\n");
        assert!(until(&events, "got:two", &mut reattached).contains("got:two"), "the session ran on");

        // Hangs: the bridge stops, the link stays open and says nothing.
        reattached = false;
        let second = wire.pid().unwrap();
        unsafe { libc::kill(second, libc::SIGSTOP) };
        let _ = until(&events, "", &mut reattached);
        assert!(reattached, "a link that went quiet was dropped and dialled again");
        daemon.write(&id, b"three\n");
        assert!(until(&events, "got:three", &mut reattached).contains("got:three"));

        // The window lets go; the session stays for the next one.
        daemon.disconnect();
        let mut stream = std::os::unix::net::UnixStream::connect(proto::socket_path()).unwrap();
        proto::write_msg(&mut stream, &Msg::Hello { protocol: proto::PROTOCOL, version: crate::build_watch::build_id() }).unwrap();
        let Ok(Frame::Msg(Msg::Welcome { sessions, ping, .. })) = proto::read_frame(&mut stream) else { panic!("no welcome") };
        assert!(ping);
        assert_eq!(sessions, [id.clone()]);
        proto::write_msg(&mut stream, &Msg::Kill { id }).unwrap();
        std::thread::sleep(Duration::from_millis(200));
        let _ = std::fs::remove_file(proto::socket_path());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_greeting_before_the_mark_is_skipped() {
        let mut wire = b"Welcome to box!\nLast login: today\n".to_vec();
        wire.extend_from_slice(super::super::bridge::MARK);
        wire.extend(proto::encode_msg(&Msg::Ping));
        let mut reader = BufReader::new(std::io::Cursor::new(wire));
        assert!(skip_to_mark(&mut reader));
        assert!(matches!(proto::read_frame(&mut reader), Ok(Frame::Msg(Msg::Ping))));
        assert!(!skip_to_mark(&mut BufReader::new(std::io::Cursor::new(b"no mark here".to_vec()))));
    }

    #[test]
    fn a_message_of_a_newer_side_is_left_alone() {
        let body = br#"{"t":"SomethingNew","x":1}"#;
        let mut wire = ((body.len() + 1) as u32).to_be_bytes().to_vec();
        wire.push(proto::KIND_MSG);
        wire.extend_from_slice(body);
        assert!(matches!(proto::read_frame(&mut std::io::Cursor::new(wire)), Ok(Frame::Msg(Msg::Unknown))));
    }
}
