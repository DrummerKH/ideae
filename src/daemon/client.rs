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
}

pub struct Daemon {
    writer: Mutex<UnixStream>,
    peers: Mutex<HashMap<String, Peer>>,
}

static DAEMON: OnceLock<Option<Arc<Daemon>>> = OnceLock::new();

/// The connected daemon, when terminals run there.
pub fn get() -> Option<Arc<Daemon>> {
    DAEMON.get().cloned().flatten()
}

/// Connects, starting the daemon when none runs. Hook reports and the
/// latest ones the daemon kept go to `on_hook`.
pub fn connect(on_hook: impl Fn(Hook) + Send + 'static) -> Option<(Arc<Daemon>, Vec<Hook>)> {
    let path = proto::socket_path();
    let stream = match UnixStream::connect(&path) {
        Ok(stream) => stream,
        Err(_) => {
            start_daemon().ok()?;
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                if let Ok(stream) = UnixStream::connect(&path) {
                    break stream;
                }
                if Instant::now() > deadline {
                    return None;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    };
    let mut reader = stream.try_clone().ok()?;
    let mut writer = stream;
    proto::write_msg(&mut writer, &Msg::Hello { protocol: proto::PROTOCOL, version: crate::build_watch::version() }).ok()?;
    let _ = reader.set_read_timeout(Some(Duration::from_secs(5)));
    let Ok(Frame::Msg(Msg::Welcome { protocol, hooks, .. })) = proto::read_frame(&mut reader) else {
        return None;
    };
    let _ = reader.set_read_timeout(None);
    if protocol != proto::PROTOCOL {
        // An older daemon speaks other frames: replace it. Its agents end and
        // continue their conversations when started again.
        let _ = proto::write_msg(&mut writer, &Msg::Shutdown);
        drop(writer);
        std::thread::sleep(Duration::from_millis(1500));
        return connect(on_hook);
    }
    let daemon = Arc::new(Daemon { writer: Mutex::new(writer), peers: Mutex::default() });
    let reading = daemon.clone();
    std::thread::Builder::new()
        .name("daemon-reader".into())
        .spawn(move || {
            while let Ok(frame) = proto::read_frame(&mut reader) {
                reading.on_frame(frame, &on_hook);
            }
            // The daemon went away: terminals show it.
            for (_, p) in reading.peers.lock().unwrap().drain() {
                p.peer.output(b"\r\n[ideae: lost the daemon]\r\n");
                p.peer.exit(ExitStatus::new(None, Some("daemon gone")));
            }
        })
        .ok()?;
    let _ = DAEMON.set(Some(daemon.clone()));
    Some((daemon, hooks))
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

impl Daemon {
    fn send(&self, frame: Vec<u8>) {
        let _ = self.writer.lock().unwrap().write_all(&frame);
    }

    fn on_frame(&self, frame: Frame, on_hook: &impl Fn(Hook)) {
        match frame {
            Frame::Bytes { kind, id, bytes } => {
                let mut peers = self.peers.lock().unwrap();
                let Some(p) = peers.get_mut(&id) else { return };
                if kind == KIND_REPLAY {
                    p.mute_until = Some(Instant::now() + REPLAY_MUTE);
                }
                if kind == KIND_BYTES || kind == KIND_REPLAY {
                    p.peer.output(&bytes);
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
        self.peers.lock().unwrap().insert(id.clone(), Peer { peer, mute_until: None });
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
