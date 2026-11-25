//! The window's side: terminals whose programs run in the daemon, shown
//! through a gpui-cn [`StreamSource`].

use std::collections::HashMap;
use std::io::Write;
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use gpui_cn::terminal::{
    Engine, EngineOptions, ExitStatus, ForegroundProcess, StreamEvent, StreamPeer, StreamSource, TerminalConfig,
    TerminalState, frame::Viewport,
};
use gpui_kit::*;

use super::proto::{self, Frame, Hook, KIND_BYTES, KIND_REPLAY, Msg, Size, Spec};

/// Replies the terminal gives while it parses replayed output (to queries
/// the program made long ago) must not reach the program as typing.
const REPLAY_MUTE: Duration = Duration::from_millis(800);

/// What a pipe session (an agent's host) gives the window.
pub enum PipeEvent {
    Bytes(Vec<u8>),
    /// The window attached again, to another daemon or after losing this
    /// one for a moment: what it holds may be behind.
    Reattached,
    Exited,
}

enum Link {
    Term { peer: StreamPeer, mute_until: Option<Instant> },
    Pipe(async_channel::Sender<PipeEvent>),
}

struct Peer {
    link: Link,
    /// To open the session again on another daemon: how to start it, and
    /// the grid it last had.
    spec: Spec,
    size: Option<Size>,
    /// The daemon was asked for the session, and when the size last changed.
    opened: bool,
    sized: Instant,
    /// Started again when its daemon has it no longer (died, or it ended
    /// while the link was down): a chat's host, which finds its thread in
    /// its journal. Others are told they ended.
    again: bool,
    /// The link it was opened on ([`Daemon::epoch`]): one opened on a link
    /// since dropped had its Open lost, and is opened again, not ended.
    epoch: u64,
    /// The daemon said it took the Open (`Opened`).
    acked: bool,
}

/// How long a terminal's size holds before it attaches to a session that
/// runs: a window that maximizes right after it opens would show the
/// program a size for a moment, and its output replayed into that one.
const SIZE_SETTLES: Duration = Duration::from_millis(300);

/// How many ended sessions are remembered for a link that comes back.
const KILLED_KEPT: usize = 256;

pub struct Daemon {
    /// The daemon process, as the socket's peer: changes with a handover.
    pid: std::sync::atomic::AtomicI32,
    writer: Mutex<Box<dyn Write + Send>>,
    peers: Mutex<HashMap<String, Peer>>,
    /// The app told it to end: it is not looked for again, nor started anew.
    quitting: std::sync::atomic::AtomicBool,
    /// It went away and did not come back: terminals run in the window.
    lost: std::sync::atomic::AtomicBool,
    /// The sessions it had when this window came: attached once their
    /// terminal's size holds.
    running: Mutex<std::collections::HashSet<String>>,
    /// A daemon on another machine, over ssh: dialled again whenever the
    /// link drops, for as long as the window wants it.
    wire: Option<super::remote::Wire>,
    /// Calls waiting for their reply, by id.
    calls: Mutex<HashMap<String, std::sync::mpsc::Sender<(proto::Reply, Vec<u8>)>>>,
    /// What tasks have watched there, sent again when the link is back,
    /// and whom a change of each is told to.
    watches: Mutex<HashMap<String, (Vec<std::path::PathBuf>, Vec<std::path::PathBuf>)>>,
    changes: Mutex<HashMap<String, async_channel::Sender<()>>>,
    /// Which link to the daemon this is: one more each time it drops.
    epoch: std::sync::atomic::AtomicU64,
    /// Sessions of another machine ended, till the daemon is known to have
    /// taken their Kill (a Pong to a Ping sent after it: it answers in
    /// order): one sent on a link that had died unnoticed is lost, and is
    /// sent again once a link is back (for one still running there and not
    /// opened again here). By the number of the Kill. Never one it took: by
    /// then another machine's window may run a session of that name.
    killed: Mutex<std::collections::VecDeque<(String, u64)>>,
    kills: std::sync::atomic::AtomicU64,
    /// The Pings on their way, by the number of the last Kill before each.
    pings: Mutex<std::collections::VecDeque<u64>>,
    /// It says `Opened` (one of an earlier build there may not): what it
    /// took is known, not guessed from the link it was sent on.
    acks: std::sync::atomic::AtomicBool,
}

static DAEMON: OnceLock<Option<Arc<Daemon>>> = OnceLock::new();

/// The connected daemon, when terminals run there.
pub fn get() -> Option<Arc<Daemon>> {
    DAEMON.get().cloned().flatten().filter(|daemon| !daemon.lost.load(std::sync::atomic::Ordering::Relaxed))
}

/// The daemons of other machines, by host ([`crate::hosts`]).
static REMOTES: Mutex<std::collections::BTreeMap<String, Arc<Daemon>>> = Mutex::new(std::collections::BTreeMap::new());

/// The daemon that runs `path`'s terminals: its host's for a remote task,
/// this machine's for the rest.
pub fn for_path(path: &std::path::Path) -> Option<Arc<Daemon>> {
    match crate::hosts::split(path) {
        Some((host, _)) => REMOTES.lock().unwrap().get(&host).cloned(),
        None => get(),
    }
}

/// The daemon of session `id` (`<task>#<name>`).
pub fn for_session(id: &str) -> Option<Arc<Daemon>> {
    for_path(std::path::Path::new(id.rsplit_once('#').map_or(id, |(task, _)| task)))
}

pub fn add_remote(host: String, daemon: Arc<Daemon>) {
    REMOTES.lock().unwrap().insert(host, daemon);
}

pub fn remotes() -> Vec<Arc<Daemon>> {
    REMOTES.lock().unwrap().values().cloned().collect()
}

/// Connects, starting the daemon when none runs, and moving its sessions
/// to a daemon of this build when it runs another. Hook reports and the
/// latest ones the daemon kept go to `on_hook`.
pub fn connect(on_hook: impl Fn(Hook) + Send + Sync + 'static) -> Option<(Arc<Daemon>, Vec<Hook>)> {
    let (writer, reader, hooks, sessions) = dial(true)?;
    let daemon = Arc::new(Daemon {
        pid: peer_pid(&writer).into(),
        writer: Mutex::new(Box::new(writer)),
        peers: Mutex::default(),
        quitting: Default::default(),
        lost: Default::default(),
        running: Mutex::new(sessions.into_iter().collect()),
        wire: None,
        calls: Mutex::default(),
        watches: Mutex::default(),
        changes: Mutex::default(),
        epoch: Default::default(),
        killed: Mutex::default(),
        kills: Default::default(),
        pings: Mutex::default(),
        acks: true.into(),
    });
    let reading = daemon.clone();
    std::thread::Builder::new()
        .name("daemon-reader".into())
        .spawn(move || {
            let mut reader = reader;
            loop {
                while let Ok(frame) = proto::read_frame(&mut reader) {
                    reading.on_frame(frame, &on_hook);
                }
                // Told to end by this window: looking for it again would start
                // another, and the agents just ended with it.
                if reading.quitting.load(std::sync::atomic::Ordering::Relaxed) {
                    return;
                }
                // The daemon went away: handed over to a newer one, or died.
                // Attach again where it serves now.
                reading.link_lost();
                match dial(false) {
                    Some((writer, next, kept, running)) => {
                        reading.pid.store(peer_pid(&writer), std::sync::atomic::Ordering::Relaxed);
                        *reading.writer.lock().unwrap() = Box::new(writer);
                        reader = next;
                        reading.reopen(&running);
                        // What was reported while nobody listened: a turn's
                        // end among them, or the task stays "working".
                        for hook in kept {
                            on_hook(hook);
                        }
                    }
                    None => break,
                }
            }
            reading.lost.store(true, std::sync::atomic::Ordering::Relaxed);
            for (_, p) in reading.peers.lock().unwrap().drain() {
                match p.link {
                    Link::Term { peer, .. } => {
                        peer.output(b"\r\n[ideae: lost the daemon]\r\n");
                        peer.exit(ExitStatus::new(None, Some("daemon gone")));
                    }
                    Link::Pipe(tx) => {
                        let _ = tx.try_send(PipeEvent::Exited);
                    }
                }
            }
        })
        .ok()?;
    let _ = DAEMON.set(Some(daemon.clone()));
    Some((daemon, hooks))
}

/// A connection that said hello: the writer, the reader, the kept hooks,
/// the sessions that run.
fn dial(upgrade: bool) -> Option<(UnixStream, UnixStream, Vec<Hook>, Vec<String>)> {
    let path = proto::socket_path();
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut started = false;
    let mut upgraded = !upgrade;
    loop {
        let stream = match UnixStream::connect(&path) {
            Ok(stream) => stream,
            Err(_) if Instant::now() > deadline => return None,
            Err(_) => {
                // After a handover the successor binds within moments: give
                // it those before starting one.
                if !upgrade && !started && Instant::now() + Duration::from_secs(8) < deadline {
                    std::thread::sleep(Duration::from_millis(100));
                    continue;
                }
                if !started {
                    started = true;
                    ideae_core::daemon::start().ok()?;
                }
                std::thread::sleep(Duration::from_millis(50));
                continue;
            }
        };
        let mut reader = stream.try_clone().ok()?;
        let mut writer = stream;
        proto::write_msg(&mut writer, &Msg::Hello { protocol: proto::PROTOCOL, version: crate::build_watch::build_id() }).ok()?;
        let _ = reader.set_read_timeout(Some(Duration::from_secs(5)));
        let welcome = proto::read_frame(&mut reader);
        let _ = reader.set_read_timeout(None);
        match welcome {
            Ok(Frame::Msg(Msg::Welcome { protocol, version, hooks, sessions, .. })) => {
                if protocol == proto::PROTOCOL && (version == crate::build_watch::build_id() || upgraded) {
                    return Some((writer, reader, hooks, sessions));
                }
                upgraded = true;
                // A window that only attaches again ends nobody's daemon.
                if !upgrade {
                    return None;
                }
                if protocol == proto::PROTOCOL {
                    // Another build: it hands its sessions to one of ours.
                    let exe = crate::desktop::exe().ok()?;
                    proto::write_msg(&mut writer, &Msg::Upgrade { exe }).ok()?;
                } else {
                    // It speaks other frames and cannot hand over: replace it.
                    // Its agents end and continue their conversations when
                    // started again.
                    let _ = proto::write_msg(&mut writer, &Msg::Shutdown);
                }
                // It exits once done; wait for that, then connect again. One
                // that could not hand over says so, or goes on sending what
                // its agents report: then it is attached to as it is.
                let _ = reader.set_read_timeout(Some(Duration::from_secs(15)));
                let patience = Instant::now() + Duration::from_secs(20);
                while Instant::now() < patience && matches!(proto::read_frame(&mut reader), Ok(frame) if !matches!(frame, Frame::Msg(Msg::Stays))) {}
                std::thread::sleep(Duration::from_millis(200));
            }
            // An old daemon that does not understand: replace it.
            _ if !upgraded => {
                upgraded = true;
                let _ = proto::write_msg(&mut writer, &Msg::Shutdown);
                std::thread::sleep(Duration::from_millis(1500));
            }
            _ => return None,
        }
    }
}

fn size_of(viewport: Viewport) -> Size {
    Size { cols: viewport.columns(), rows: viewport.rows(), cell_w: viewport.cell_width(), cell_h: viewport.cell_height() }
}

/// The pid of the process at the other end, 0 when unknown.
#[cfg(target_os = "macos")]
fn peer_pid(stream: &UnixStream) -> i32 {
    use std::os::fd::AsRawFd as _;
    let mut pid: libc::pid_t = 0;
    let mut len = std::mem::size_of::<libc::pid_t>() as libc::socklen_t;
    // SAFETY: LOCAL_PEERPID writes a pid_t of the given length.
    let ok = unsafe {
        libc::getsockopt(stream.as_raw_fd(), libc::SOL_LOCAL, libc::LOCAL_PEERPID, (&raw mut pid).cast(), &mut len)
    } == 0;
    if ok { pid } else { 0 }
}

#[cfg(target_os = "linux")]
fn peer_pid(stream: &UnixStream) -> i32 {
    use std::os::fd::AsRawFd as _;
    let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: SO_PEERCRED writes a ucred of the given length.
    let ok = unsafe {
        libc::getsockopt(stream.as_raw_fd(), libc::SOL_SOCKET, libc::SO_PEERCRED, (&raw mut cred).cast(), &mut len)
    } == 0;
    if ok { cred.pid } else { 0 }
}

impl Daemon {
    pub fn pid(&self) -> Option<i32> {
        Some(self.pid.load(std::sync::atomic::Ordering::Relaxed)).filter(|&p| p > 0)
    }

    fn epoch(&self) -> u64 {
        self.epoch.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Whether the daemon there, as dialled again, says `Opened`.
    pub(super) fn set_acks(&self, acks: bool) {
        self.acks.store(acks, std::sync::atomic::Ordering::SeqCst);
    }

    /// The link to the daemon dropped: what is opened from now on is opened on the next.
    pub(super) fn link_lost(&self) {
        self.epoch.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        // Their Pongs do not come.
        self.pings.lock().unwrap().clear();
    }

    /// Kills not yet known to be taken.
    #[cfg(test)]
    pub(super) fn kills_unconfirmed(&self) -> usize {
        self.killed.lock().unwrap().len()
    }

    /// Asks whether the daemon is there, and so whether it took what was sent before.
    pub(super) fn ping(&self) {
        let mut pings = self.pings.lock().unwrap();
        pings.push_back(self.kills.load(std::sync::atomic::Ordering::SeqCst));
        self.send(proto::encode_msg(&Msg::Ping));
    }

    pub(super) fn send(&self, frame: Vec<u8>) {
        let _ = self.writer.lock().unwrap().write_all(&frame);
    }

    /// After a reconnect: every terminal attaches again, its screen cleared
    /// for the replay that follows.
    /// `running`: the sessions the daemon has (its welcome's). One of this
    /// window's not among them ended while nobody listened: it is told so,
    /// not started again (a check run twice, a shell that exited back).
    pub(super) fn reopen(&self, running: &[String]) {
        // Ended while the link was down: ended there too, unless opened again since.
        let killed: Vec<String> = self.killed.lock().unwrap().iter().map(|(id, _)| id.clone()).collect();
        for id in killed {
            if running.contains(&id) && !self.peers.lock().unwrap().contains_key(&id) {
                self.send(proto::encode_msg(&Msg::Kill { id }));
            }
        }
        let now = self.epoch();
        // What was watched there: watched again, and looked at once, as what changed meanwhile was not told.
        // One of an earlier build may not (the bridge could not hand it over).
        let watches = self.watches_folders();
        for (id, (flat, deep)) in self.watches.lock().unwrap().iter().filter(|_| watches) {
            self.send(proto::encode_msg(&Msg::Watch { id: id.clone(), flat: flat.clone(), deep: deep.clone() }));
            if let Some(tx) = self.changes.lock().unwrap().get(id) {
                let _ = tx.try_send(());
            }
        }
        let mut peers = self.peers.lock().unwrap();
        // Not there, though opened on a link before: it ended meanwhile. One opened
        // since the link dropped is opened now (if the daemon has it after all, it attaches).
        let acks = self.acks.load(std::sync::atomic::Ordering::SeqCst);
        let ended: Vec<String> = peers.iter().filter(|(id, p)| p.opened && !p.again && (if acks { p.acked } else { p.epoch < now }) && !running.contains(id)).map(|(id, _)| id.clone()).collect();
        for id in ended {
            match peers.remove(&id).map(|p| p.link) {
                Some(Link::Term { peer, .. }) => peer.exit(ExitStatus::new(None, None::<String>)),
                Some(Link::Pipe(tx)) => {
                    let _ = tx.try_send(PipeEvent::Exited);
                }
                None => {}
            }
        }
        for (id, p) in peers.iter_mut() {
            let Some(size) = p.size.filter(|_| p.opened) else { continue };
            p.acked = false;
            if let Link::Term { peer, .. } = &p.link {
                peer.output(b"\x1b[H\x1b[2J\x1b[3J\x1bc");
            }
            self.send(proto::encode_msg(&Msg::Open { id: id.clone(), spec: p.spec.clone(), size }));
            if let Link::Pipe(tx) = &p.link {
                let _ = tx.try_send(PipeEvent::Reattached);
            }
        }
    }

    pub(super) fn on_frame(&self, frame: Frame, on_hook: &impl Fn(Hook)) {
        match frame {
            Frame::Bytes { kind: proto::KIND_REPLY, id, bytes } => {
                if let (Some(tx), Some((reply, body))) = (self.calls.lock().unwrap().remove(&id), proto::unpack::<proto::Reply>(&bytes)) {
                    let _ = tx.send((reply, body.to_vec()));
                }
            }
            Frame::Bytes { kind, id, bytes } => {
                let peer = {
                    let mut peers = self.peers.lock().unwrap();
                    let Some(p) = peers.get_mut(&id) else { return };
                    match &mut p.link {
                        Link::Term { peer, mute_until } => {
                            if kind == KIND_REPLAY {
                                *mute_until = Some(Instant::now() + REPLAY_MUTE);
                            }
                            peer.clone()
                        }
                        Link::Pipe(tx) => {
                            let tx = tx.clone();
                            drop(peers);
                            // Waits while the window is behind: the daemon then
                            // holds the host back, as for a terminal.
                            let _ = tx.send_blocking(PipeEvent::Bytes(bytes));
                            return;
                        }
                    }
                };
                if kind == KIND_BYTES || kind == KIND_REPLAY {
                    // Not under the lock, which the UI thread takes too. While
                    // the terminal is behind, nothing more is read from the
                    // daemon, which then holds the program back.
                    peer.wait_for_room();
                    peer.output(&bytes);
                }
            }
            Frame::Msg(Msg::Opened { id }) => {
                if let Some(p) = self.peers.lock().unwrap().get_mut(&id) {
                    p.acked = true;
                }
            }
            Frame::Msg(Msg::Exited { id, code, signal }) => {
                if let Some(p) = self.peers.lock().unwrap().remove(&id) {
                    match p.link {
                        Link::Term { peer, .. } => peer.exit(ExitStatus::new(code, signal)),
                        Link::Pipe(tx) => {
                            let _ = tx.try_send(PipeEvent::Exited);
                        }
                    }
                }
            }
            Frame::Msg(Msg::Foreground { id, fg }) => {
                if let Some(Peer { link: Link::Term { peer, .. }, .. }) = self.peers.lock().unwrap().get(&id) {
                    peer.set_foreground(fg.map(|f| {
                        let mut process = ForegroundProcess::new(f.pid, f.name).with_argv(f.argv).with_shell(f.shell);
                        if let Some(cwd) = f.cwd {
                            process = process.with_cwd(cwd);
                        }
                        process
                    }));
                }
            }
            Frame::Msg(Msg::Hook(hook)) => on_hook(hook),
            Frame::Msg(Msg::Changed { id }) => {
                if let Some(tx) = self.changes.lock().unwrap().get(&id) {
                    let _ = tx.try_send(());
                }
            }
            Frame::Msg(Msg::Pong) => {
                if let Some(wire) = &self.wire {
                    wire.heard();
                }
                // Every Kill sent before its Ping was taken.
                if let Some(before) = self.pings.lock().unwrap().pop_front() {
                    self.killed.lock().unwrap().retain(|(_, n)| *n > before);
                }
            }
            Frame::Msg(_) => {}
        }
    }

    /// A terminal for session `id`: attached when it runs, else started
    /// from `spec` once the terminal knows its size.
    pub fn terminal(
        self: &Arc<Self>,
        id: String,
        spec: Spec,
        config: TerminalConfig,
        window: &mut Window,
        cx: &mut Context<TerminalState>,
    ) -> TerminalState {
        let (source, peer) = StreamSource::new();
        let events = peer.clone();
        self.peers.lock().unwrap().insert(id.clone(), Peer { link: Link::Term { peer, mute_until: None }, spec: spec.clone(), size: None, opened: false, sized: Instant::now(), again: false, epoch: self.epoch(), acked: false });
        let attach = self.running.lock().unwrap().remove(&id);
        let daemon = self.clone();
        cx.background_spawn(async move {
            let mut waiting = false;
            while let Ok(event) = events.events().recv().await {
                match event {
                    StreamEvent::Input(bytes) => {
                        let muted = daemon
                            .peers
                            .lock()
                            .unwrap()
                            .get(&id)
                            .and_then(|p| match p.link {
                                Link::Term { mute_until, .. } => mute_until,
                                Link::Pipe(_) => None,
                            })
                            .is_some_and(|until| Instant::now() < until);
                        if !muted {
                            daemon.send(proto::encode_bytes(KIND_BYTES, &id, &bytes));
                        }
                    }
                    StreamEvent::Resize(viewport) => {
                        let size = size_of(viewport);
                        let mut peers = daemon.peers.lock().unwrap();
                        let Some(p) = peers.get_mut(&id) else { continue };
                        p.size = Some(size);
                        p.sized = Instant::now();
                        if p.opened {
                            daemon.send(proto::encode_msg(&Msg::Resize { id: id.clone(), size }));
                        } else if !attach {
                            p.opened = true;
                            // Sent now, on the link of now.
                            p.epoch = daemon.epoch();
                            p.acked = false;
                            daemon.send(proto::encode_msg(&Msg::Open { id: id.clone(), spec: spec.clone(), size }));
                        } else if !waiting {
                            waiting = true;
                            let (daemon, id, spec) = (daemon.clone(), id.clone(), spec.clone());
                            std::thread::spawn(move || daemon.attach_when_sized(&id, &spec));
                        }
                    }
                    // The window let go of the terminal; whether the program
                    // ends is the task's call (`kill`).
                    StreamEvent::Closed => break,
                    _ => {}
                }
            }
            daemon.peers.lock().unwrap().remove(&id);
        })
        .detach();
        TerminalState::new(Engine::new(source, EngineOptions::default()), config, window, cx)
    }

    /// Asks for a session that runs once its terminal's size has held, or
    /// after a few seconds of changes. Under the lock a resize also takes:
    /// the size sent is the last one.
    fn attach_when_sized(&self, id: &str, spec: &Spec) {
        let since = Instant::now();
        loop {
            std::thread::sleep(Duration::from_millis(50));
            let mut peers = self.peers.lock().unwrap();
            let Some(p) = peers.get_mut(id) else { return };
            let Some(size) = p.size else { return };
            if p.sized.elapsed() >= SIZE_SETTLES || since.elapsed() >= SIZE_SETTLES * 10 {
                p.opened = true;
                // Sent now, on the link of now.
                p.epoch = self.epoch();
                p.acked = false;
                self.send(proto::encode_msg(&Msg::Open { id: id.to_string(), spec: spec.clone(), size }));
                return;
            }
        }
    }

    /// A pipe session `id` (an agent's host): attached when it runs, else
    /// started from `spec`. What it prints comes as events; [`Self::write`]
    /// gives it input.
    pub fn pipe(&self, id: String, spec: Spec) -> async_channel::Receiver<PipeEvent> {
        self.open_pipe(id, spec, false)
    }

    /// A pipe session started again whenever its daemon has it no longer
    /// (see [`Peer::again`]): it tells `Reattached` then, as when found.
    pub fn pipe_again(&self, id: String, spec: Spec) -> async_channel::Receiver<PipeEvent> {
        self.open_pipe(id, spec, true)
    }

    fn open_pipe(&self, id: String, spec: Spec, again: bool) -> async_channel::Receiver<PipeEvent> {
        let (tx, rx) = async_channel::unbounded();
        // Pipes have no grid; the size only marks the session as opened.
        let size = Size { cols: 80, rows: 24, cell_w: 8, cell_h: 16 };
        self.peers.lock().unwrap().insert(id.clone(), Peer { link: Link::Pipe(tx), spec: spec.clone(), size: Some(size), opened: true, sized: Instant::now(), again, epoch: self.epoch(), acked: false });
        self.send(proto::encode_msg(&Msg::Open { id, spec, size }));
        rx
    }

    pub fn write(&self, id: &str, bytes: &[u8]) {
        self.send(proto::encode_bytes(KIND_BYTES, id, bytes));
    }

    /// Ends session `id`. It is forgotten here at once: a link made anew
    /// opens again only what is still meant to run.
    pub fn kill(&self, id: &str) {
        let peer = self.peers.lock().unwrap().remove(id);
        // Numbered and sent under the lock Pings are sent under: in the order they go.
        let pings = self.pings.lock().unwrap();
        if self.wire.is_some() {
            let n = self.kills.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
            let mut killed = self.killed.lock().unwrap();
            killed.retain(|(k, _)| k != id);
            if killed.len() >= KILLED_KEPT {
                killed.pop_front();
            }
            killed.push_back((id.to_string(), n));
        }
        self.send(proto::encode_msg(&Msg::Kill { id: id.to_string() }));
        drop(pings);
        match peer.map(|p| p.link) {
            Some(Link::Term { peer, .. }) => peer.exit(ExitStatus::new(None, None::<String>)),
            Some(Link::Pipe(tx)) => {
                let _ = tx.try_send(PipeEvent::Exited);
            }
            None => {}
        }
    }

    pub fn prune(&self, keep: Vec<String>) {
        self.send(proto::encode_msg(&Msg::Prune { keep }));
    }

    /// Builds one for a daemon on another machine; [`super::remote`] keeps
    /// its link.
    pub(super) fn remote(wire: super::remote::Wire, writer: Box<dyn Write + Send>, sessions: Vec<String>, acks: bool) -> Self {
        Daemon {
            pid: 0.into(),
            writer: Mutex::new(writer),
            peers: Mutex::default(),
            quitting: Default::default(),
            lost: Default::default(),
            running: Mutex::new(sessions.into_iter().collect()),
            wire: Some(wire),
            calls: Mutex::default(),
            watches: Mutex::default(),
            changes: Mutex::default(),
            epoch: Default::default(),
            killed: Mutex::default(),
            kills: Default::default(),
            pings: Mutex::default(),
            acks: acks.into(),
        }
    }

    pub fn wire(&self) -> Option<&super::remote::Wire> {
        self.wire.as_ref()
    }

    pub(super) fn set_writer(&self, writer: Box<dyn Write + Send>) {
        *self.writer.lock().unwrap() = writer;
    }

    pub(super) fn quitting(&self) -> bool {
        self.quitting.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Asks the daemon's machine to run a program or read, write or look
    /// at a file ([`proto::Call`]), and waits for the answer: off the UI
    /// thread. A link that drops meanwhile fails it.
    pub fn call(&self, call: &proto::Call, body: &[u8]) -> std::io::Result<(proto::Reply, Vec<u8>)> {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let id = format!("call-{}", NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed));
        let (tx, rx) = std::sync::mpsc::channel();
        // Waiting first, then asked whether the link is up: one that drops
        // after this is told by `drop_calls`, which runs after it is down.
        self.calls.lock().unwrap().insert(id.clone(), tx);
        if self.wire.as_ref().is_some_and(|wire| !wire.is_up()) {
            self.calls.lock().unwrap().remove(&id);
            return Err(std::io::Error::new(std::io::ErrorKind::NotConnected, "the host is not connected now"));
        }
        self.send(proto::encode_bytes(proto::KIND_CALL, &id, &proto::pack(call, body)));
        let answer = rx.recv_timeout(Duration::from_secs(120));
        self.calls.lock().unwrap().remove(&id);
        answer.map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "no answer from the host"))
    }

    /// Whom a change of what `id` watches is told to.
    pub fn on_change(&self, id: &str, tx: async_channel::Sender<()>) {
        self.changes.lock().unwrap().insert(id.to_string(), tx);
    }

    /// Watches `flat` (each on its own) and `deep` (with all in them) on
    /// the daemon's machine as `id`, in place of what it watched.
    pub fn watch(&self, id: &str, flat: Vec<std::path::PathBuf>, deep: Vec<std::path::PathBuf>) {
        if !self.watches_folders() {
            return;
        }
        self.watches.lock().unwrap().insert(id.to_string(), (flat.clone(), deep.clone()));
        self.send(proto::encode_msg(&Msg::Watch { id: id.to_string(), flat, deep }));
    }

    pub fn unwatch(&self, id: &str) {
        let watched = self.watches.lock().unwrap().remove(id).is_some();
        self.changes.lock().unwrap().remove(id);
        if watched {
            self.send(proto::encode_msg(&Msg::Unwatch { id: id.to_string() }));
        }
    }

    /// Whether it watches folders for this window: this machine's, which
    /// the window watches itself, never asked; another's, when it said so.
    pub fn watches_folders(&self) -> bool {
        self.wire.as_ref().is_some_and(|wire| wire.watches())
    }

    /// The link dropped: what waits for a reply is told now.
    pub(super) fn drop_calls(&self) {
        self.calls.lock().unwrap().clear();
    }

    /// Where `ideae --hook` reports to this daemon, on its machine.
    pub fn hook_socket(&self) -> Option<String> {
        match &self.wire {
            Some(wire) => wire.socket(),
            None => Some(super::proto::socket_path().display().to_string()),
        }
    }

    /// The ideae-server and the home folder of a daemon's machine, another one.
    pub fn there(&self) -> Option<(std::path::PathBuf, std::path::PathBuf)> {
        let remote = self.wire.as_ref()?.remote();
        Some((remote.exe.clone()?, remote.home.clone()?))
    }

    /// Port `there` of a daemon's machine, reached at the port here this gives.
    pub fn forward(&self, there: u16) -> Result<u16, String> {
        match &self.wire {
            Some(wire) => super::remote::forward(wire.remote(), there),
            None => Ok(there),
        }
    }

    /// Ends ssh's master connection to a daemon on another machine.
    pub fn close_master(&self) {
        if let Some(wire) = &self.wire {
            super::remote::close_master(wire.remote());
        }
    }

    /// What a link through another host left on that host, ended: before any close_master.
    pub fn close_beyond(&self) {
        if let Some(wire) = &self.wire {
            super::remote::close_beyond(wire.remote());
        }
    }

    /// Lets go of a daemon on another machine: its sessions run on there,
    /// for this window or another to attach to later.
    pub fn disconnect(&self) {
        self.quitting.store(true, std::sync::atomic::Ordering::Relaxed);
        if let Some(wire) = &self.wire {
            wire.drop_link();
        }
    }

    /// Says on every terminal that the link is down, once per drop.
    pub(super) fn tell_terminals(&self, line: &str) {
        for p in self.peers.lock().unwrap().values() {
            if let Link::Term { peer, .. } = &p.link {
                peer.output(line.as_bytes());
            }
        }
    }

    /// Ends every session and the daemon: the app quits for good.
    pub fn shutdown(&self) {
        self.quitting.store(true, std::sync::atomic::Ordering::Relaxed);
        self.send(proto::encode_msg(&Msg::Shutdown));
        let _ = self.writer.lock().unwrap().flush();
        // Told before its link goes: the daemon ends, and with it the ssh of the link.
        if let Some(wire) = &self.wire {
            wire.wait_gone(Duration::from_secs(2));
        }
    }
}

