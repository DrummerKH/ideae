//! A daemon on another machine, reached by ssh: `ssh host ideae --daemon
//! --stdio` (see [`super::bridge`]) carries the same frames as the local
//! socket. The link drops (sleep, another network): the agents run on
//! there, and the window dials again until it is back, its terminals
//! attaching anew.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
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
    /// Once put there ([`super::install::ensure`]): the ideae-server there
    /// and the home folder there, for what runs it (an agent's host).
    pub exe: Option<PathBuf>,
    pub home: Option<PathBuf>,
    /// A host reached through this one: as this one's ssh names it
    /// (`-p 2222 me@mac`). It ssh's there with its own keys, config and
    /// known hosts; nothing of here but this one's port need reach it.
    pub hop: Option<Vec<String>>,
}

impl Remote {
    pub fn new(ssh: Vec<String>) -> Self {
        Remote { ssh, ideae: "ideae".into(), ping: Duration::from_secs(5), exe: None, home: None, hop: None }
    }

    /// The host of a key ([`crate::hosts`]), through another one when it names two.
    pub fn for_host(host: &str) -> Self {
        let mut remote = Remote::new(crate::hosts::ssh_for(host));
        remote.hop = crate::hosts::hop(host).1.map(crate::hosts::destination);
        remote
    }

    /// The host, for messages.
    pub fn name(&self) -> String {
        let first = self.ssh.last().map_or("", String::as_str);
        match self.hop.as_ref().and_then(|h| h.last()) {
            Some(then) => format!("{then} (through {first})"),
            None => first.to_string(),
        }
    }

    /// The host this one's ssh reaches, alone.
    pub(super) fn first(&self) -> Remote {
        Remote { hop: None, ..self.clone() }
    }

    /// What tells this one apart from another of the same first host.
    fn label(&self) -> String {
        let mut label = self.ssh.join(" ");
        if let Some(hop) = &self.hop {
            label.push_str(" > ");
            label.push_str(&hop.join(" "));
        }
        label
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
    /// The daemon watches folders (it said so on welcome).
    watches: AtomicBool,
    up: AtomicBool,
    /// Where `ideae --hook` reports on that machine.
    socket: Mutex<Option<String>>,
    /// Ports of here reached from there (`-R`): asked again of a master made anew.
    /// Its holders, and whether the forward is made (a failed one is asked again by the next holder).
    reverses: Mutex<HashMap<u16, (i64, bool)>>,
    /// The window's MCP socket, as reached there (a socket of its user's
    /// there): its agents there ask the window through it.
    window_socket: Mutex<Option<(PathBuf, PathBuf)>>,
    /// The window's socket asked to be reached: tried again with a link made anew while it is not.
    window_wanted: Mutex<Option<PathBuf>>,
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

    /// Whether the daemon there watches folders for the window.
    pub fn watches(&self) -> bool {
        self.watches.load(Ordering::Relaxed)
    }

    /// Waits, `most` at most, for the ssh of the link to end.
    pub(super) fn wait_gone(&self, most: Duration) {
        let until = Instant::now() + most;
        while Instant::now() < until {
            let gone = self.child.lock().unwrap().as_mut().is_none_or(|child| child.try_wait().ok().flatten().is_some());
            if gone {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Port `port` of here reached at the same port there (a task's Chrome,
    /// for its agent there), now and after the link is made anew.
    /// The socket there it is reached at; the host's daemon makes a port of
    /// it ([`super::client::Daemon::reach_here`]).
    pub(super) fn reach_here(&self, port: u16) -> Result<PathBuf, String> {
        // Counted, and asked of ssh under the same lock: a task's view let go of
        // while a new one of it takes the port again leaves the port reached;
        // one let go of before its taking was done (below zero) leaves it not.
        let mut reverses = self.reverses.lock().unwrap();
        let (holders, made) = reverses.entry(port).or_insert((0, false));
        *holders += 1;
        match *holders {
            0 => {
                reverses.remove(&port);
                Err("let go of meanwhile".into())
            }
            // The first, or one after a forward that failed: made now. Not reached, it is
            // still counted: its holder lets go of it as of one that was (once for each asking).
            _ if !*made => {
                let done = reverse(&self.remote, port, "forward");
                *made = done.is_ok();
                done
            }
            _ => {
                let home = self.remote.home.clone().ok_or("its home folder there is not known")?;
                Ok(home.join(".ssh").join(format!("ideae-cdp-{}-{port}.sock", run_token())))
            }
        }
    }

    /// The window's socket `here` reached at a socket there, for the agents
    /// there (their MCP server): once a link, made again with it.
    pub fn reach_window(&self, here: &Path) -> Result<PathBuf, String> {
        *self.window_wanted.lock().unwrap() = Some(here.to_path_buf());
        let mut held = self.window_socket.lock().unwrap();
        if let Some((_, there)) = held.as_ref() {
            return Ok(there.clone());
        }
        let there = forward_socket(&self.remote, here)?;
        *held = Some((here.to_path_buf(), there.clone()));
        Ok(there)
    }

    /// Where the window's socket is reached there, once it is.
    pub fn window_there(&self) -> Option<PathBuf> {
        self.window_socket.lock().unwrap().as_ref().map(|(_, there)| there.clone())
    }

    /// No longer, once its last holder has gone.
    /// Whether that was its last holder: the port there is let go of too.
    pub(super) fn release_here(&self, port: u16) -> bool {
        let mut reverses = self.reverses.lock().unwrap();
        let (holders, _) = reverses.entry(port).or_insert((0, false));
        *holders -= 1;
        if *holders == 0 {
            reverses.remove(&port);
            let _ = reverse(&self.remote, port, "cancel");
            return true;
        }
        false
    }

    pub(super) fn remote(&self) -> &Remote {
        &self.remote
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
    watches: bool,
    opened: bool,
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
        watches: AtomicBool::new(first.watches),
        up: AtomicBool::new(true),
        socket: Mutex::new(first.socket),
        reverses: Mutex::default(),
        window_socket: Mutex::default(),
        window_wanted: Mutex::default(),
    };
    let daemon = Arc::new(Daemon::remote(wire, queued(first.writer), first.sessions, first.opened));
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
                reading.link_lost();
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
                wire.watches.store(next.watches, Ordering::Relaxed);
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
                reading.set_acks(next.opened);
                reading.reopen(&next.sessions);
                // A master made anew has none of the old one's forwards.
                let mut reverses = wire.reverses.lock().unwrap();
                let mut relays: Vec<(u16, PathBuf)> = Vec::new();
                for (&port, (_, made)) in reverses.iter_mut().filter(|(_, (held, _))| *held > 0) {
                    let done = reverse(&wire.remote, port, "forward");
                    *made = done.is_ok();
                    relays.extend(done.ok().map(|socket| (port, socket)));
                }
                drop(reverses);
                // Its daemon may be another (upgraded): asked again, once the link reads its answers.
                let asking = reading.clone();
                std::thread::spawn(move || {
                    for (port, socket) in relays {
                        let _ = asking.relay(port, Some(socket));
                    }
                });
                let held = wire.window_socket.lock().unwrap().clone();
                match held {
                    Some((here, _)) => {
                        let _ = forward_socket(&wire.remote, &here);
                    }
                    // Not reached the time before: tried again.
                    None => {
                        let wanted = wire.window_wanted.lock().unwrap().clone();
                        if let Some(here) = wanted {
                            let _ = wire.reach_window(&here);
                        }
                    }
                }
                // Beyond a first host, its own link may have been made anew, its forwards gone with it.
                if wire.remote.hop.is_some() {
                    let label = wire.remote.label();
                    FORWARDS.lock().unwrap().get_or_insert_with(HashMap::new).retain(|(of, _), _| *of != label);
                }
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
                        daemon.ping();
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
        let mut deadline = deadline;
        while Instant::now() < deadline {
            if waiting.load(Ordering::Relaxed) {
                return;
            }
            // Someone is typing a password: their time is not the host's.
            if crate::askpass::asking() {
                deadline = Instant::now() + HANDSHAKE;
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
        // Ended by now when it was ssh that failed (its output closed with it).
        let until = Instant::now() + Duration::from_millis(300);
        let mut code = None;
        while Instant::now() < until {
            if let Ok(Some(status)) = child.try_wait() {
                code = status.code();
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let _ = child.kill();
        let _ = child.wait();
        let said = String::from_utf8_lossy(&said.lock().unwrap()).trim().to_string();
        ssh_said(code, if said.is_empty() { what.to_string() } else { format!("{what}: {said}") })
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
        Ok(Frame::Msg(Msg::Welcome { protocol, hooks, sessions, ping, socket, watches, opened, owners, .. })) if protocol == proto::PROTOCOL => {
            greeted.store(true, Ordering::Relaxed);
            if owners && proto::write_msg(&mut writer, &Msg::Owner { owner: super::client::window_id() }).is_err() {
                return Err(fail(&mut child, "the link closed"));
            }
            Ok(Dialled { child, writer: Box::new(writer), reader: Box::new(reader), hooks, sessions, ping, socket, watches, opened })
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
        let said = if said.is_empty() { format!("ssh {} failed", remote.name()) } else { said };
        return Err(ssh_said(out.status.code(), said));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// ssh to the host, running `line` in its shell there.
pub(super) fn ssh(remote: &Remote, line: &str) -> Result<std::process::Command, String> {
    let mut command = ssh_to(remote)?;
    match &remote.hop {
        Some(hop) => command.arg("--").arg(hop_line(hop, &[], Some(line))),
        None => command.arg("--").arg(line),
    };
    Ok(command)
}

/// What names this run's files on a first host: its process and when it
/// began, as another machine's window could have the same process id.
pub(super) fn run_token() -> &'static str {
    static TOKEN: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    TOKEN.get_or_init(|| {
        let at = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_micros());
        format!("{}x{:x}", std::process::id(), at % 0x1000_0000)
    })
}

/// What ssh said, marked as ssh's own failure when it was (its 255),
/// not a failure of a command it ran ([`crate::hosts::explain_ssh`]).
fn ssh_said(code: Option<i32>, said: String) -> String {
    if code == Some(255) { format!("{said}\n{}", crate::hosts::SSH_FAILED) } else { said }
}

fn quote(word: &str) -> String {
    format!("'{}'", word.replace('\'', r"'\''"))
}

/// The line the first host runs to reach the one beyond it: its own ssh,
/// asking nothing (no one is there to answer), with a master connection
/// of its own that forwards are added to. `extra` before the host, then
/// `line` for the host beyond.
fn hop_line(hop: &[String], extra: &[&str], line: Option<&str>) -> String {
    // This run's alone: Exit here ends no link of another window's through the same host.
    let control = format!("ControlPath=~/.ssh/ideae-{}-%C", run_token());
    let mut words: Vec<String> = ["ssh", "-T", "-o", "BatchMode=yes", "-o", "ServerAliveInterval=5", "-o", "ServerAliveCountMax=3", "-o", "ControlMaster=auto", "-o", &control, "-o", "ControlPersist=600", "-o", "StreamLocalBindUnlink=yes"]
        .iter()
        .map(|w| w.to_string())
        .collect();
    words.extend(extra.iter().map(|w| w.to_string()));
    words.extend(hop.iter().cloned());
    let mut ssh = words.iter().map(|w| quote(w)).collect::<Vec<_>>().join(" ");
    if let Some(line) = line {
        ssh.push_str(" -- ");
        ssh.push_str(&quote(line));
    }
    // ssh's own failure (255), not the command's: told as the leg beyond failing.
    format!("mkdir -p ~/.ssh; {ssh}; c=$?; [ $c -eq 255 ] && printf '\\n%s\\n' {} >&2; exit $c", quote(crate::hosts::BEYOND_FAILED))
}

/// What runs of the app that did not Exit (killed, the machine gone) left on
/// a first host: the sockets of their forwards beyond it, once the link
/// they were made on is gone (its ssh removes its control socket as it ends).
pub(super) fn clear_first_host(remote: &Remote) {
    const CLEAR: &str = r#"cd ~/.ssh 2>/dev/null || exit 0
for s in ideae-*-*.sock; do
  [ -e "$s" ] || continue
  # A window's own socket there (MCP), reached directly: its host's daemon sees to those.
  case $s in ideae-mcp-*|ideae-cdp-*) continue ;; esac
  t=${s#ideae-}; t=${t%%-*}; live=
  for m in ideae-"$t"-*; do
    case $m in *.sock) ;; *) [ -S "$m" ] && live=1 ;; esac
  done
  [ -n "$live" ] || rm -f "$s"
done"#;
    if remote.hop.is_some() {
        let _ = run(&remote.first(), CLEAR, None);
    }
}

/// Runs `extra` (`-O forward …`) on the first host's master connection to
/// the one beyond it.
fn on_hop(remote: &Remote, hop: &[String], extra: &[&str]) -> Result<(), String> {
    run(&remote.first(), &hop_line(hop, extra, None), None).map(|_| ())
}

/// ssh to the host, its options and the host given, no command yet. What
/// it asks (a password, a key's passphrase, a host not met) is asked in
/// the window, once a link: later ones go through the first's master
/// connection. With no window to ask, it asks nothing.
fn ssh_to(remote: &Remote) -> Result<std::process::Command, String> {
    ssh_with(remote, &[])
}

/// The same, with `extra` before the host (`-O forward …`).
fn ssh_with(remote: &Remote, extra: &[&str]) -> Result<std::process::Command, String> {
    let (program, rest) = remote.ssh.split_first().ok_or("no ssh command")?;
    let (host, options) = rest.split_last().ok_or("no host")?;
    let mut command = crate::procs::command(program);
    command.args(options).args(["-T", "-o", "ServerAliveInterval=5", "-o", "ServerAliveCountMax=3"]);
    // IDEAE_ASKPASS_PROGRAM: the app's binary, for a test whose own is not.
    let askpass = std::env::var_os("IDEAE_ASKPASS_PROGRAM").map(PathBuf::from).map_or_else(crate::desktop::exe, Ok);
    match (crate::askpass::socket(), askpass) {
        (Some(socket), Ok(exe)) => {
            command.env("SSH_ASKPASS", exe).env("SSH_ASKPASS_REQUIRE", "force").env(crate::askpass::SOCK_ENV, socket).env(crate::askpass::MODE_ENV, "1");
        }
        _ => {
            command.args(["-o", "BatchMode=yes"]);
        }
    }
    if let Some(dir) = masters() {
        command.args(["-o", "ControlMaster=auto", "-o"]).arg(format!("ControlPath={}/%C", dir.display())).args(["-o", "ControlPersist=600"]);
    }
    command.args(extra).arg(host);
    Ok(command)
}

/// Where ssh keeps this run's master connections: a short path (a
/// socket's is 104 bytes at most on macOS), a folder of this user's alone,
/// one of this process's in it, so that Exit in one window ends no link
/// of another. Those of runs gone are cleared the first time.
fn masters() -> Option<PathBuf> {
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
    static MADE: std::sync::Once = std::sync::Once::new();
    // SAFETY: getuid cannot fail.
    let uid = unsafe { libc::getuid() };
    let dir = PathBuf::from(format!("/tmp/{}-ssh-{uid}", crate::state::APP_ID));
    let _ = std::fs::create_dir(&dir);
    let meta = std::fs::symlink_metadata(&dir).ok()?;
    if !meta.is_dir() || meta.uid() != uid {
        return None;
    }
    if meta.mode() & 0o077 != 0 {
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).ok()?;
    }
    MADE.call_once(|| {
        for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            let pid = entry.file_name().to_str().and_then(|n| n.parse::<i32>().ok());
            // SAFETY: kill with 0 only asks whether the process is there.
            if pid.is_some_and(|pid| unsafe { libc::kill(pid, 0) } != 0) {
                let _ = std::fs::remove_dir_all(entry.path());
            }
        }
    });
    let own = dir.join(std::process::id().to_string());
    let _ = std::fs::create_dir(&own);
    own.is_dir().then_some(own)
}

/// Ends ssh's master connection to the host: Exit leaves none behind.
pub fn close_master(remote: &Remote) {
    if masters().is_none() {
        return;
    }
    if let Ok(mut exit) = ssh_with(remote, &["-O", "exit"]) {
        let _ = exit.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn();
    }
}

/// Beyond a first host: the first host's sockets of this run's forwards
/// beyond it, and its link beyond, ended through the link to it (so before
/// [`close_master`] of any project through it), asking nothing, waited for
/// a moment at most (this is a quit).
pub fn close_beyond(remote: &Remote) {
    if masters().is_none() {
        return;
    }
    if let Some(hop) = &remote.hop {
        let line = format!("rm -f ~/.ssh/ideae-{}-*.sock; {}", run_token(), hop_line(hop, &["-O", "exit"], None));
        if let Ok(mut command) = ssh_with(&remote.first(), &["-o", "BatchMode=yes"]) {
            if let Ok(mut child) = command.arg("--").arg(line).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn() {
                let until = Instant::now() + Duration::from_secs(2);
                while Instant::now() < until && matches!(child.try_wait(), Ok(None)) {
                    std::thread::sleep(Duration::from_millis(20));
                }
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }
}

/// Port `port` of here (a task's Chrome), reached at a socket of the
/// host's user alone (as sshd makes it): `~/.ssh/ideae-cdp-<run>-<port>.sock`
/// there, a name the clearing of a first host's sockets passes by; the
/// host's daemon makes the same port there of it, for that user's processes
/// alone ([`Call::Relay`]). Beyond a first host, through a socket of this
/// run's on it. What a link before left there is let go of first.
/// `how`: "forward", or "cancel" to end it.
fn reverse(remote: &Remote, port: u16, how: &str) -> Result<PathBuf, String> {
    masters().ok_or("no folder for ssh's master connections")?;
    let home = remote.home.clone().ok_or("its home folder there is not known")?;
    let there = home.join(".ssh").join(format!("ideae-cdp-{}-{port}.sock", run_token()));
    let forwarding = how == "forward";
    let clear = |at: &Remote, socket: &str| run(at, &format!("mkdir -p ~/.ssh && rm -f {}", quote(socket)), None);
    let master = |how: &str, spec: &str| -> Result<(), String> {
        let out = ssh_with(remote, &["-O", how, "-R", spec])?.stdin(Stdio::null()).output().map_err(|e| format!("ssh: {e}"))?;
        if out.status.success() { Ok(()) } else { Err(ssh_said(Some(255), String::from_utf8_lossy(&out.stderr).trim().to_string())) }
    };
    match &remote.hop {
        None => {
            let spec = format!("{}:127.0.0.1:{port}", there.display());
            if forwarding {
                let _ = master("cancel", &spec);
                clear(remote, &there.display().to_string())?;
            }
            master(how, &spec)?;
        }
        Some(hop) => {
            let on_first = format!("{}/.ssh/ideae-{}-r{port}.sock", first_home(remote)?, run_token());
            let to_first = format!("{on_first}:127.0.0.1:{port}");
            let on_beyond = format!("{}:{on_first}", there.display());
            if forwarding {
                let _ = master("cancel", &to_first);
                clear(&remote.first(), &on_first)?;
            }
            master(how, &to_first)?;
            if forwarding {
                let _ = on_hop(remote, hop, &["-O", "cancel", "-R", &on_beyond]);
                clear(remote, &there.display().to_string())?;
            }
            let done = on_hop(remote, hop, &["-O", how, "-R", &on_beyond]);
            if !forwarding {
                let _ = run(&remote.first(), &format!("rm -f {}", quote(&on_first)), None);
            }
            done?;
        }
    }
    if !forwarding {
        let _ = run(remote, &format!("rm -f {}", quote(&there.display().to_string())), None);
    }
    Ok(there)
}

/// The socket `here` reached at a socket on the host, its user's alone (as
/// sshd makes it): `~/.ssh/ideae-mcp-<run>.sock` there, a name the clearing
/// of a first host's sockets passes by. Beyond a first host, through one of
/// its own on it, named by the host beyond (two of them through one first
/// host have one each). What a link before left (a socket, a forward its
/// master still holds, which a new one of the same would not bind again)
/// is let go of first.
fn forward_socket(remote: &Remote, here: &Path) -> Result<PathBuf, String> {
    masters().ok_or("no folder for ssh's master connections")?;
    let home = remote.home.clone().ok_or("its home folder there is not known")?;
    let there = home.join(".ssh").join(format!("ideae-mcp-{}.sock", run_token()));
    let clear = |at: &Remote, socket: &str| run(at, &format!("mkdir -p ~/.ssh && rm -f {}", quote(socket)), None);
    let master = |how: &str, spec: &str| -> Result<(), String> {
        let out = ssh_with(remote, &["-O", how, "-R", spec])?.stdin(Stdio::null()).output().map_err(|e| format!("ssh: {e}"))?;
        if out.status.success() { Ok(()) } else { Err(ssh_said(Some(255), String::from_utf8_lossy(&out.stderr).trim().to_string())) }
    };
    match &remote.hop {
        None => {
            let spec = format!("{}:{}", there.display(), here.display());
            let _ = master("cancel", &spec);
            clear(remote, &there.display().to_string())?;
            master("forward", &spec)?;
        }
        Some(hop) => {
            // A name of its own: the first host may be the one beyond too (a hop to itself).
            let beyond = {
                use std::hash::{Hash as _, Hasher as _};
                let mut h = std::collections::hash_map::DefaultHasher::new();
                hop.hash(&mut h);
                h.finish() as u32
            };
            let on_first = format!("{}/.ssh/ideae-{}-{beyond:08x}-mcp-via.sock", first_home(remote)?, run_token());
            let to_first = format!("{on_first}:{}", here.display());
            let _ = master("cancel", &to_first);
            clear(&remote.first(), &on_first)?;
            master("forward", &to_first)?;
            let on_beyond = format!("{}:{on_first}", there.display());
            let _ = on_hop(remote, hop, &["-O", "cancel", "-R", &on_beyond]);
            clear(remote, &there.display().to_string())?;
            on_hop(remote, hop, &["-O", "forward", "-R", &on_beyond])?;
        }
    }
    Ok(there)
}

/// The home folder of a first host, asked once.
fn first_home(remote: &Remote) -> Result<String, String> {
    static HOMES: Mutex<Option<HashMap<String, String>>> = Mutex::new(None);
    let key = remote.first().label();
    if let Some(home) = HOMES.lock().unwrap().get_or_insert_with(HashMap::new).get(&key) {
        return Ok(home.clone());
    }
    let home = run(&remote.first(), r#"printf %s "$HOME""#, None)?;
    if !home.starts_with('/') {
        return Err(format!("{}: no home folder there", remote.first().name()));
    }
    HOMES.lock().unwrap().get_or_insert_with(HashMap::new).insert(key, home.clone());
    Ok(home)
}

/// Port `there` of the host, reached here through ssh's master connection:
/// at the same port when it is free here, else at one that is. Kept while
/// it answers; forwarded again when the master was made anew.
/// Ports of hosts forwarded here: by the host's label and the port there.
static FORWARDS: Mutex<Option<HashMap<(String, u16), u16>>> = Mutex::new(None);

pub fn forward(remote: &Remote, there: u16) -> Result<u16, String> {
    let key = (remote.label(), there);
    let known = FORWARDS.lock().unwrap().get_or_insert_with(HashMap::new).get(&key).copied();
    if let Some(here) = known.filter(|here| std::net::TcpStream::connect(("127.0.0.1", *here)).is_ok()) {
        return Ok(here);
    }
    masters().ok_or("no folder for ssh's master connections")?;
    let here = known
        .filter(|here| std::net::TcpListener::bind(("127.0.0.1", *here)).is_ok())
        .or_else(|| std::net::TcpListener::bind(("127.0.0.1", there)).ok().map(|_| there))
        .or_else(|| std::net::TcpListener::bind(("127.0.0.1", 0)).ok().and_then(|l| l.local_addr().ok()).map(|a| a.port()))
        .ok_or("no port free here")?;
    // Beyond the first host: a socket of its own reaches the port there, and here reaches that socket.
    let spec = match &remote.hop {
        Some(hop) => {
            let home = first_home(remote)?;
            let socket = format!("{home}/.ssh/ideae-{}-{there}-{:08x}.sock", run_token(), {
                use std::hash::{Hash as _, Hasher as _};
                let mut h = std::collections::hash_map::DefaultHasher::new();
                hop.hash(&mut h);
                h.finish() as u32
            });
            on_hop(remote, hop, &["-O", "forward", "-L", &format!("{socket}:127.0.0.1:{there}")])?;
            format!("{here}:{socket}")
        }
        None => format!("{here}:127.0.0.1:{there}"),
    };
    let out = ssh_with(remote, &["-O", "forward", "-L", &spec])?.stdin(Stdio::null()).output().map_err(|e| format!("ssh: {e}"))?;
    if !out.status.success() {
        return Err(ssh_said(Some(255), String::from_utf8_lossy(&out.stderr).trim().to_string()));
    }
    FORWARDS.lock().unwrap().get_or_insert_with(HashMap::new).insert(key, here);
    Ok(here)
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
    /// Once what was written to the daemon on `stream` is taken: a Ping after it answered.
    fn settle(stream: &mut std::os::unix::net::UnixStream) {
        proto::write_msg(stream, &Msg::Ping).unwrap();
        while !matches!(proto::read_frame(stream), Ok(Frame::Msg(Msg::Pong)) | Err(_)) {}
    }

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
        let _daemon = crate::daemon::tests::start();

        // ssh to "box": a greeting, as a login shell may print, then the bridge.
        let ssh = dir.join("ssh");
        let exe = std::env::current_exe().unwrap();
        std::fs::write(
            &ssh,
            format!(
                "#!/bin/sh\ncase \" $* \" in *\" -O \"*) exit 0;; esac\necho \"$*\" >> '{args}'\necho 'Welcome to box'\nexec env IDEAE_TEST_BRIDGE=1 '{exe}' --exact daemon::remote::tests::bridge_process --ignored --nocapture --test-threads=1\n",
                args = dir.join("args").display(),
                exe = exe.display()
            ),
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&ssh, std::fs::Permissions::from_mode(0o755)).unwrap();
        let remote = Remote { ssh: vec![ssh.display().to_string(), "box".into()], ideae: "ideae".into(), ping: Duration::from_millis(300), exe: None, home: None, hop: None };
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
        // Meanwhile a session ends there: told so once the link is back, not run again.
        let once = Spec { cwd: dir.clone(), env: Vec::new(), input: None, program: vec!["sh".into(), "-c".into(), "if [ -f once.log ]; then exec sleep 60; fi; echo up; while [ ! -f gate ]; do sleep 0.02; done; echo ran >> once.log".into()] };
        let once_events = daemon.pipe("task#once".into(), once);
        let mut once_back = false;
        assert!(until(&once_events, "up", &mut once_back).contains("up"));
        // One that finds what it was doing on its own (a chat's host) is started again instead.
        let again = Spec { cwd: dir.clone(), env: Vec::new(), input: None, program: vec!["sh".into(), "-c".into(), "echo up; while [ ! -f gate ]; do sleep 0.02; done; echo ran >> again.log".into()] };
        let again_events = daemon.pipe_again("task#again".into(), again);
        assert!(until(&again_events, "up", &mut once_back).contains("up"));
        let doomed = Spec { cwd: dir.clone(), env: Vec::new(), input: None, program: vec!["sh".into(), "-c".into(), "echo up; exec sleep 60".into()] };
        let doomed_events = daemon.pipe("task#doomed".into(), doomed);
        assert!(until(&doomed_events, "up", &mut once_back).contains("up"));
        reattached = false;
        let second = wire.pid().unwrap();
        unsafe { libc::kill(second, libc::SIGSTOP) };
        // Those two end now, while the link is dead and before it is found to be.
        std::fs::write(dir.join("gate"), "").unwrap();
        // The link is dead and not yet known to be: what is opened and ended now is lost on the way.
        let late = Spec { cwd: dir.clone(), env: Vec::new(), input: None, program: vec!["sh".into(), "-c".into(), "echo late-up; exec sleep 60".into()] };
        let late_events = daemon.pipe("task#late".into(), late);
        daemon.kill("task#doomed");
        let _ = until(&events, "", &mut reattached);
        assert!(reattached, "a link that went quiet was dropped and dialled again");
        let deadline = Instant::now() + Duration::from_secs(10);
        let ended = loop {
            match once_events.try_recv() {
                Ok(PipeEvent::Exited) => break true,
                Ok(PipeEvent::Reattached) => break false,
                _ if Instant::now() > deadline => break false,
                _ => std::thread::sleep(Duration::from_millis(20)),
            }
        };
        assert!(ended, "a session that ended while the link was down is told as ended");
        // Opened on the dead link: opened on the new one, not taken as ended.
        let mut late_back = false;
        assert!(until(&late_events, "late-up", &mut late_back).contains("late-up"), "a session opened as the link died runs");
        daemon.kill("task#late");

        // Ended here with the link up: its Kill is taken (a Pong says so), and not sent
        // again later, when another machine's window may run a session of that name.
        let shared = Spec { cwd: dir.clone(), env: Vec::new(), input: None, program: vec!["sh".into(), "-c".into(), "echo up; exec sleep 60".into()] };
        let shared_events = daemon.pipe("task#shared".into(), shared.clone());
        assert!(until(&shared_events, "up", &mut once_back).contains("up"));
        daemon.kill("task#shared");
        let deadline = Instant::now() + Duration::from_secs(10);
        while daemon.kills_unconfirmed() > 0 {
            assert!(Instant::now() < deadline, "the Kill taken, as a Pong says");
            std::thread::sleep(Duration::from_millis(20));
        }
        let mut other = std::os::unix::net::UnixStream::connect(proto::socket_path()).unwrap();
        proto::write_msg(&mut other, &Msg::Hello { protocol: proto::PROTOCOL, version: crate::build_watch::build_id() }).unwrap();
        assert!(matches!(proto::read_frame(&mut other), Ok(Frame::Msg(Msg::Welcome { .. }))));
        let size = proto::Size { cols: 80, rows: 24, cell_w: 8, cell_h: 16 };
        proto::write_msg(&mut other, &Msg::Open { id: "task#shared".into(), spec: shared, size }).unwrap();
        while !matches!(proto::read_frame(&mut other), Ok(Frame::Msg(Msg::Opened { .. }))) {}
        reattached = false;
        let third = wire.pid().unwrap();
        unsafe { libc::kill(third, libc::SIGKILL) };
        let _ = until(&events, "", &mut reattached);
        assert!(reattached);
        // What the window sent as the link came back (an Open, a Kill) is taken once a call
        // after it is answered: the daemon takes a window's messages in order.
        daemon.call(&proto::Call::Listening { task: "none".into() }, b"").unwrap();
        let mut look = std::os::unix::net::UnixStream::connect(proto::socket_path()).unwrap();
        proto::write_msg(&mut look, &Msg::Hello { protocol: proto::PROTOCOL, version: crate::build_watch::build_id() }).unwrap();
        let Ok(Frame::Msg(Msg::Welcome { sessions: now, .. })) = proto::read_frame(&mut look) else { panic!("no welcome") };
        assert!(now.iter().any(|s| s == "task#shared"), "another window's session of that name runs on: {now:?}");
        // Started again, it would run on (its log there): not among them.
        assert!(!now.iter().any(|s| s == "task#once"), "an ended session is not started again: {now:?}");
        assert_eq!(std::fs::read_to_string(dir.join("once.log")).unwrap(), "ran\n");
        proto::write_msg(&mut other, &Msg::Kill { id: "task#shared".into() }).unwrap();
        settle(&mut other);
        let mut again_back = false;
        let _ = until(&again_events, "", &mut again_back);
        assert!(again_back, "one to start again is told it is attached again");
        let deadline = Instant::now() + Duration::from_secs(10);
        while std::fs::read_to_string(dir.join("again.log")).unwrap_or_default() != "ran\nran\n" {
            assert!(Instant::now() < deadline, "and runs again");
            std::thread::sleep(Duration::from_millis(50));
        }
        daemon.kill("task#again");
        daemon.write(&id, b"three\n");
        assert!(until(&events, "got:three", &mut reattached).contains("got:three"));

        // The window lets go; the session stays for the next one.
        daemon.disconnect();
        let mut stream = std::os::unix::net::UnixStream::connect(proto::socket_path()).unwrap();
        proto::write_msg(&mut stream, &Msg::Hello { protocol: proto::PROTOCOL, version: crate::build_watch::build_id() }).unwrap();
        let Ok(Frame::Msg(Msg::Welcome { sessions, ping, .. })) = proto::read_frame(&mut stream) else { panic!("no welcome") };
        assert!(ping);
        assert_eq!(sessions, [id.clone()], "the one ended as the link died ended there too");
        proto::write_msg(&mut stream, &Msg::Kill { id }).unwrap();
        settle(&mut stream);
        // Shut down while its socket is there to be asked on.
        drop(_daemon);
        let _ = std::fs::remove_file(proto::socket_path());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The first host's own ssh failing to reach the next is told apart
    /// from the command failing there: run here, against no such machine.
    #[test]
    fn a_failure_beyond_the_first_host_is_marked() {
        let line = hop_line(&["-o".into(), "ConnectTimeout=2".into(), "no-such-machine.invalid".into()], &[], Some("true"));
        let out = std::process::Command::new("sh").arg("-c").arg(&line).output().unwrap();
        assert_eq!(out.status.code(), Some(255));
        let said = String::from_utf8_lossy(&out.stderr);
        assert!(said.contains(crate::hosts::BEYOND_FAILED), "{said}");
        assert!(crate::hosts::explain_ssh("hub>no-such-machine.invalid", &said).starts_with("no machine named no-such-machine.invalid is known"), "{}", crate::hosts::explain_ssh("hub>no-such-machine.invalid", &said));
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
