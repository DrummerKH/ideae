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

struct Peer {
    peer: StreamPeer,
    mute_until: Option<Instant>,
    /// To open the session again on another daemon: how to start it, and
    /// the grid it last had.
    spec: Spec,
    size: Option<Size>,
}

pub struct Daemon {
    /// The daemon process, as the socket's peer: changes with a handover.
    pid: std::sync::atomic::AtomicI32,
    writer: Mutex<UnixStream>,
    peers: Mutex<HashMap<String, Peer>>,
}

static DAEMON: OnceLock<Option<Arc<Daemon>>> = OnceLock::new();

/// The connected daemon, when terminals run there.
pub fn get() -> Option<Arc<Daemon>> {
    DAEMON.get().cloned().flatten()
}

/// Connects, starting the daemon when none runs, and moving its sessions
/// to a daemon of this build when it runs another. Hook reports and the
/// latest ones the daemon kept go to `on_hook`.
pub fn connect(on_hook: impl Fn(Hook) + Send + Sync + 'static) -> Option<(Arc<Daemon>, Vec<Hook>)> {
    let (writer, reader, hooks) = dial(true)?;
    let daemon = Arc::new(Daemon { pid: peer_pid(&writer).into(), writer: Mutex::new(writer), peers: Mutex::default() });
    let reading = daemon.clone();
    std::thread::Builder::new()
        .name("daemon-reader".into())
        .spawn(move || {
            let mut reader = reader;
            loop {
                while let Ok(frame) = proto::read_frame(&mut reader) {
                    reading.on_frame(frame, &on_hook);
                }
                // The daemon went away: handed over to a newer one, or died.
                // Attach again where it serves now.
                match dial(false) {
                    Some((writer, next, _)) => {
                        reading.pid.store(peer_pid(&writer), std::sync::atomic::Ordering::Relaxed);
                        *reading.writer.lock().unwrap() = writer;
                        reader = next;
                        reading.reopen();
                    }
                    None => break,
                }
            }
            for (_, p) in reading.peers.lock().unwrap().drain() {
                p.peer.output(b"\r\n[ideae: lost the daemon]\r\n");
                p.peer.exit(ExitStatus::new(None, Some("daemon gone")));
            }
        })
        .ok()?;
    let _ = DAEMON.set(Some(daemon.clone()));
    Some((daemon, hooks))
}

/// A connection that said hello: the writer, the reader, the kept hooks.
fn dial(upgrade: bool) -> Option<(UnixStream, UnixStream, Vec<Hook>)> {
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
                    start_daemon().ok()?;
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
            Ok(Frame::Msg(Msg::Welcome { protocol, version, hooks, .. })) => {
                if protocol == proto::PROTOCOL && (version == crate::build_watch::build_id() || upgraded) {
                    return Some((writer, reader, hooks));
                }
                upgraded = true;
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
                // It exits once done; wait for that, then connect again.
                let _ = reader.set_read_timeout(Some(Duration::from_secs(15)));
                while proto::read_frame(&mut reader).is_ok() {}
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

/// Starts `ideae --daemon` in a session of its own, so it stays when the
/// window goes.
fn start_daemon() -> std::io::Result<()> {
    use std::os::unix::process::CommandExt as _;
    let exe = crate::desktop::exe()?;
    let mut command = std::process::Command::new(exe);
    command.arg("--daemon").stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    // SAFETY: setsid only changes the child's session.
    unsafe {
        command.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    command.spawn().map(|_| ())
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

    fn send(&self, frame: Vec<u8>) {
        let _ = self.writer.lock().unwrap().write_all(&frame);
    }

    /// After a reconnect: every terminal attaches again, its screen cleared
    /// for the replay that follows.
    fn reopen(&self) {
        let peers = self.peers.lock().unwrap();
        for (id, p) in peers.iter() {
            let Some(size) = p.size else { continue };
            p.peer.output(b"\x1b[H\x1b[2J\x1b[3J\x1bc");
            self.send(proto::encode_msg(&Msg::Open { id: id.clone(), spec: p.spec.clone(), size }));
        }
    }

    fn on_frame(&self, frame: Frame, on_hook: &impl Fn(Hook)) {
        match frame {
            Frame::Bytes { kind, id, bytes } => {
                let peer = {
                    let mut peers = self.peers.lock().unwrap();
                    let Some(p) = peers.get_mut(&id) else { return };
                    if kind == KIND_REPLAY {
                        p.mute_until = Some(Instant::now() + REPLAY_MUTE);
                    }
                    p.peer.clone()
                };
                if kind == KIND_BYTES || kind == KIND_REPLAY {
                    // Not under the lock, which the UI thread takes too. While
                    // the terminal is behind, nothing more is read from the
                    // daemon, which then holds the program back.
                    peer.wait_for_room();
                    peer.output(&bytes);
                }
            }
            Frame::Msg(Msg::Exited { id, code, signal }) => {
                if let Some(p) = self.peers.lock().unwrap().remove(&id) {
                    p.peer.exit(ExitStatus::new(code, signal));
                }
            }
            Frame::Msg(Msg::Foreground { id, fg }) => {
                if let Some(p) = self.peers.lock().unwrap().get(&id) {
                    p.peer.set_foreground(fg.map(|f| {
                        let mut process = ForegroundProcess::new(f.pid, f.name).with_argv(f.argv).with_shell(f.shell);
                        if let Some(cwd) = f.cwd {
                            process = process.with_cwd(cwd);
                        }
                        process
                    }));
                }
            }
            Frame::Msg(Msg::Hook(hook)) => on_hook(hook),
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
        self.peers.lock().unwrap().insert(id.clone(), Peer { peer, mute_until: None, spec: spec.clone(), size: None });
        let daemon = self.clone();
        cx.background_spawn(async move {
            let mut opened = false;
            while let Ok(event) = events.events().recv().await {
                match event {
                    StreamEvent::Input(bytes) => {
                        let muted = daemon
                            .peers
                            .lock()
                            .unwrap()
                            .get(&id)
                            .and_then(|p| p.mute_until)
                            .is_some_and(|until| Instant::now() < until);
                        if !muted {
                            daemon.send(proto::encode_bytes(KIND_BYTES, &id, &bytes));
                        }
                    }
                    StreamEvent::Resize(viewport) => {
                        let size = size_of(viewport);
                        if let Some(p) = daemon.peers.lock().unwrap().get_mut(&id) {
                            p.size = Some(size);
                        }
                        let msg = if opened {
                            Msg::Resize { id: id.clone(), size }
                        } else {
                            opened = true;
                            Msg::Open { id: id.clone(), spec: spec.clone(), size }
                        };
                        daemon.send(proto::encode_msg(&msg));
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

    pub fn kill(&self, id: &str) {
        self.send(proto::encode_msg(&Msg::Kill { id: id.to_string() }));
    }

    pub fn prune(&self, keep: Vec<String>) {
        self.send(proto::encode_msg(&Msg::Prune { keep }));
    }

    /// Ends every session and the daemon: the app quits for good.
    pub fn shutdown(&self) {
        self.send(proto::encode_msg(&Msg::Shutdown));
    }
}

/// `ideae --hook` in a terminal of the daemon: reports there.
pub fn send_hook(socket: &str, hook: Hook) {
    if let Ok(mut stream) = UnixStream::connect(socket) {
        let _ = stream.set_write_timeout(Some(Duration::from_secs(1)));
        let _ = proto::write_msg(&mut stream, &Msg::Hook(hook));
    }
}
