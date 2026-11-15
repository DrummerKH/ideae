//! `ideae --daemon`: owns the terminals (shells, agents) so they outlive the
//! window. Each terminal is a session: a pty running the user's shell set up
//! the way gpui-cn sets it up, its recent output kept for the next window
//! that attaches. Claude Code's hooks report here too, and the latest per
//! task is handed to every window.

use std::collections::{HashMap, HashSet, VecDeque};
use std::io::Write;
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use gpui_cn::terminal::{
    ByteHandle, ByteSink, ByteSource as _, DetachedSink, LocalPty, LocalTerminalOptions, WorkingDirectory,
    frame::Viewport,
};

use super::proto::{self, Fg, Frame, Hook, KIND_BYTES, KIND_REPLAY, Msg, Size, Spec};

/// Output kept per session for a window that attaches later.
const REPLAY_BYTES: usize = 2 << 20;
/// How long a daemon with no sessions and no window stays. With sessions it
/// stays: a closed window leaves the agents working.
const EMPTY: Duration = Duration::from_secs(60);

struct Session {
    handle: Arc<Mutex<Box<dyn ByteHandle>>>,
    sink: Arc<DetachedSink>,
    replay: VecDeque<u8>,
    attached: HashSet<u64>,
    /// Input the pty could not take yet.
    pending: Vec<u8>,
    fg: Option<Fg>,
    /// The grid the windows last asked for.
    size: Size,
}

#[derive(Default)]
struct Server {
    sessions: HashMap<String, Session>,
    clients: HashMap<u64, mpsc::Sender<Vec<u8>>>,
    hooks: HashMap<String, Hook>,
    next_client: u64,
    alone_since: Option<Instant>,
}

type Shared = Arc<Mutex<Server>>;

impl Server {
    fn send(&self, client: u64, frame: Vec<u8>) {
        if let Some(tx) = self.clients.get(&client) {
            let _ = tx.send(frame);
        }
    }

    fn broadcast(&self, frame: &[u8]) {
        for tx in self.clients.values() {
            let _ = tx.send(frame.to_vec());
        }
    }
}

fn viewport(size: Size) -> Viewport {
    Viewport::new(size.cols.max(2), size.rows.max(2), size.cell_w.max(1), size.cell_h.max(1))
        .or_else(|| Viewport::new(80, 24, 8, 16))
        .expect("80x24 is a valid grid")
}

fn fg_of(handle: &Mutex<Box<dyn ByteHandle>>) -> Option<Fg> {
    let fg = handle.lock().ok()?.foreground()?;
    Some(Fg {
        pid: fg.pid(),
        name: fg.name().to_string(),
        argv: fg.argv().to_vec(),
        cwd: fg.cwd().map(|p| p.to_path_buf()),
        shell: fg.is_shell(),
    })
}

pub fn run() {
    let path = proto::socket_path();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if UnixStream::connect(&path).is_ok() {
        return; // another daemon serves
    }
    let _ = std::fs::remove_file(&path);
    let Ok(listener) = UnixListener::bind(&path) else { return };
    let shared: Shared = Arc::new(Mutex::new(Server { alone_since: Some(Instant::now()), ..Default::default() }));

    let house = shared.clone();
    std::thread::spawn(move || housekeeping(house));

    for stream in listener.incoming().flatten() {
        let shared = shared.clone();
        std::thread::spawn(move || serve(shared, stream));
    }
}

/// Foreground processes for the windows, and the end of an unattended daemon.
fn housekeeping(shared: Shared) {
    loop {
        std::thread::sleep(Duration::from_secs(1));
        let unattended = {
            let s = shared.lock().unwrap();
            s.alone_since.is_some_and(|since| s.sessions.is_empty() && since.elapsed() > EMPTY)
        };
        if unattended {
            shutdown(&shared);
        }
        let handles: Vec<(String, Arc<Mutex<Box<dyn ByteHandle>>>)> = {
            let s = shared.lock().unwrap();
            s.sessions.iter().filter(|(_, x)| !x.attached.is_empty()).map(|(id, x)| (id.clone(), x.handle.clone())).collect()
        };
        for (id, handle) in handles {
            let fg = fg_of(&handle);
            let mut s = shared.lock().unwrap();
            let Some(session) = s.sessions.get_mut(&id) else { continue };
            if session.fg != fg {
                session.fg = fg.clone();
                let clients: Vec<u64> = session.attached.iter().copied().collect();
                let frame = proto::encode_msg(&Msg::Foreground { id, fg });
                for c in clients {
                    s.send(c, frame.clone());
                }
            }
        }
    }
}

fn serve(shared: Shared, stream: UnixStream) {
    let Ok(mut writer) = stream.try_clone() else { return };
    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    let client = {
        let mut s = shared.lock().unwrap();
        s.next_client += 1;
        s.next_client
    };
    // Registered on Hello: `ideae --hook` connects only to report.
    let writer_thread = std::thread::spawn(move || {
        while let Ok(frame) = rx.recv() {
            if writer.write_all(&frame).is_err() {
                break;
            }
        }
    });
    let mut reader = stream;
    let mut registered = false;
    while let Ok(frame) = proto::read_frame(&mut reader) {
        match frame {
            Frame::Bytes { kind: KIND_BYTES, id, bytes } => input(&shared, &id, &bytes),
            Frame::Bytes { .. } => {}
            Frame::Msg(Msg::Hello { .. }) => {
                let mut s = shared.lock().unwrap();
                s.clients.insert(client, tx.clone());
                s.alone_since = None;
                registered = true;
                let welcome = Msg::Welcome {
                    protocol: proto::PROTOCOL,
                    version: crate::build_watch::version(),
                    sessions: s.sessions.keys().cloned().collect(),
                    hooks: s.hooks.values().cloned().collect(),
                };
                s.send(client, proto::encode_msg(&welcome));
            }
            Frame::Msg(Msg::Open { id, spec, size }) => open(&shared, client, id, spec, size),
            Frame::Msg(Msg::Resize { id, size }) => {
                let mut s = shared.lock().unwrap();
                if let Some(session) = s.sessions.get_mut(&id) {
                    session.size = size;
                    let _ = session.handle.lock().unwrap().resize(viewport(size));
                }
            }
            Frame::Msg(Msg::Kill { id }) => kill(&shared, &id),
            Frame::Msg(Msg::Prune { keep }) => {
                let gone: Vec<String> = shared.lock().unwrap().sessions.keys().filter(|id| !keep.contains(id)).cloned().collect();
                for id in gone {
                    kill(&shared, &id);
                }
            }
            Frame::Msg(Msg::Shutdown) => shutdown(&shared),
            Frame::Msg(Msg::Hook(hook)) => {
                let mut s = shared.lock().unwrap();
                if s.clients.is_empty() {
                    notify(&hook);
                }
                s.broadcast(&proto::encode_msg(&Msg::Hook(hook.clone())));
                s.hooks.insert(hook.task.clone(), hook);
            }
            Frame::Msg(_) => {}
        }
    }
    let mut s = shared.lock().unwrap();
    s.clients.remove(&client);
    for session in s.sessions.values_mut() {
        session.attached.remove(&client);
    }
    if registered && s.clients.is_empty() {
        s.alone_since = Some(Instant::now());
    }
    drop(s);
    drop(tx);
    let _ = writer_thread.join();
}

fn input(shared: &Shared, id: &str, bytes: &[u8]) {
    let mut s = shared.lock().unwrap();
    let Some(session) = s.sessions.get_mut(id) else { return };
    if !session.pending.is_empty() {
        session.pending.extend_from_slice(bytes);
        return;
    }
    let written = session.handle.lock().unwrap().write(bytes).unwrap_or(bytes.len());
    session.pending.extend_from_slice(&bytes[written.min(bytes.len())..]);
}

fn open(shared: &Shared, client: u64, id: String, spec: Spec, size: Size) {
    let mut s = shared.lock().unwrap();
    if let Some(session) = s.sessions.get_mut(&id) {
        // Attach: what it printed so far, then live output.
        session.attached.insert(client);
        session.size = size;
        let replay: Vec<u8> = session.replay.iter().copied().collect();
        let fg = session.fg.clone();
        let handle = session.handle.clone();
        s.send(client, proto::encode_bytes(KIND_REPLAY, &id, &replay));
        s.send(client, proto::encode_msg(&Msg::Foreground { id: id.clone(), fg }));
        drop(s);
        // A size change makes full-screen programs (the agent) draw again,
        // over whatever the replay could not restore. It ends on the size
        // the window has by then: a window that maximizes right after it
        // opens sends a new one meanwhile.
        let shared = shared.clone();
        std::thread::spawn(move || {
            let shrunk = Size { rows: size.rows.saturating_sub(1).max(2), ..size };
            let _ = handle.lock().unwrap().resize(viewport(shrunk));
            std::thread::sleep(Duration::from_millis(60));
            let s = shared.lock().unwrap();
            if let Some(session) = s.sessions.get(&id).filter(|x| Arc::ptr_eq(&x.handle, &handle)) {
                let _ = handle.lock().unwrap().resize(viewport(session.size));
            }
        });
        return;
    }
    let mut options = LocalTerminalOptions::default().with_cwd(WorkingDirectory::Path(spec.cwd.clone()));
    for (name, value) in &spec.env {
        options = options.with_env(name.clone(), value.clone());
    }
    let (sink, detached) = ByteSink::detached();
    let handle = match Box::new(LocalPty::new(options)).start(sink, viewport(size)) {
        Ok(handle) => handle,
        Err(e) => {
            let msg = format!("\r\nideae: could not start the shell: {e}\r\n");
            s.send(client, proto::encode_bytes(KIND_BYTES, &id, msg.as_bytes()));
            s.send(client, proto::encode_msg(&Msg::Exited { id, code: Some(127), signal: None }));
            return;
        }
    };
    if let Some(input) = &spec.input {
        let _ = handle.write(input.as_bytes());
    }
    let handle = Arc::new(Mutex::new(handle));
    let detached = Arc::new(detached);
    s.sessions.insert(
        id.clone(),
        Session {
            handle: handle.clone(),
            sink: detached.clone(),
            replay: VecDeque::new(),
            attached: HashSet::from([client]),
            pending: Vec::new(),
            fg: None,
            size,
        },
    );
    drop(s);
    let shared = shared.clone();
    std::thread::spawn(move || pump(shared, id, handle, detached));
}

/// The session's output to its windows, until the program ends.
fn pump(shared: Shared, id: String, handle: Arc<Mutex<Box<dyn ByteHandle>>>, sink: Arc<DetachedSink>) {
    let mut finished_at: Option<Instant> = None;
    loop {
        let batch = sink.next(Duration::from_millis(200));
        let mut s = shared.lock().unwrap();
        let Some(session) = s.sessions.get_mut(&id) else { return };
        if !Arc::ptr_eq(&session.handle, &handle) {
            return;
        }
        let mut frames = Vec::new();
        for chunk in &batch.output {
            session.replay.extend(chunk.iter().copied());
            let excess = session.replay.len().saturating_sub(REPLAY_BYTES);
            session.replay.drain(..excess);
            frames.push(proto::encode_bytes(KIND_BYTES, &id, chunk));
        }
        if batch.writable && !session.pending.is_empty() {
            let pending = std::mem::take(&mut session.pending);
            let written = handle.lock().unwrap().write(&pending).unwrap_or(pending.len());
            session.pending = pending[written.min(pending.len())..].to_vec();
        }
        let clients: Vec<u64> = session.attached.iter().copied().collect();
        for frame in frames {
            for &c in &clients {
                s.send(c, frame.clone());
            }
        }
        if batch.finished {
            finished_at.get_or_insert_with(Instant::now);
        }
        let status = handle.lock().unwrap().try_wait().ok().flatten();
        let gave_up = finished_at.is_some_and(|at| at.elapsed() > Duration::from_secs(2));
        if status.is_some() || gave_up {
            let (code, signal) = status.map_or((None, None), |st| (st.code(), st.signal().map(str::to_string)));
            s.sessions.remove(&id);
            let frame = proto::encode_msg(&Msg::Exited { id: id.clone(), code, signal });
            for &c in &clients {
                s.send(c, frame.clone());
            }
            return;
        }
    }
}

/// Ends a session's program and everything in its session.
fn kill(shared: &Shared, id: &str) {
    let Some(session) = shared.lock().unwrap().sessions.remove(id) else { return };
    session.sink.close();
    session.handle.lock().unwrap().close();
}

fn shutdown(shared: &Shared) -> ! {
    let ids: Vec<String> = shared.lock().unwrap().sessions.keys().cloned().collect();
    let handles: Vec<_> = ids
        .iter()
        .filter_map(|id| shared.lock().unwrap().sessions.remove(id))
        .map(|session| {
            session.sink.close();
            std::thread::spawn(move || session.handle.lock().unwrap().close())
        })
        .collect();
    for h in handles {
        let _ = h.join();
    }
    let _ = std::fs::remove_file(proto::socket_path());
    std::process::exit(0);
}

/// No window to show it: an agent that wants the user says so on the desktop.
fn notify(hook: &Hook) {
    let event = crate::agent::HookEvent {
        task: hook.task.clone().into(),
        event: hook.event.clone(),
        message: hook.message.clone(),
        kind: hook.kind.clone(),
        replayed: false,
    };
    let Some(status) = event.status().filter(|s| s.is_attention()) else { return };
    let body = match status {
        crate::agent::AgentStatus::NeedsInput(message) => message,
        _ => "Finished".into(),
    };
    let task = std::path::Path::new(&hook.task).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    if let Ok(mut child) = std::process::Command::new("notify-send").args(["-a", crate::state::APP_ID, &task, &body]).spawn() {
        std::thread::spawn(move || child.wait());
    }
}
