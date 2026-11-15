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
    clients: HashMap<u64, Client>,
    hooks: HashMap<String, Hook>,
    next_client: u64,
    alone_since: Option<Instant>,
}

type Shared = Arc<Mutex<Server>>;

/// A window: where its frames go, and how many bytes of them wait to be
/// written to its socket.
struct Client {
    tx: mpsc::Sender<Vec<u8>>,
    queued: Arc<std::sync::atomic::AtomicUsize>,
    /// Its socket, to let go of a window that takes nothing.
    stream: UnixStream,
}

/// With this much waiting for a window, a session's output stays in its
/// pty until the window catches up: the program is held back, as by a
/// terminal that cannot keep up, instead of the daemon's memory filling.
const CLIENT_QUEUE_BYTES: usize = 4 << 20;
/// A window that takes nothing for this long (stopped, hung) is let go:
/// its programs run on, and it attaches again, to what they printed last,
/// when it is back. Queueing for it instead would fill the daemon.
const CLIENT_STALL: Duration = Duration::from_secs(if cfg!(test) { 2 } else { 5 });

impl Server {
    fn send(&self, client: u64, frame: Vec<u8>) {
        if let Some(client) = self.clients.get(&client) {
            client.queue(frame);
        }
    }

    fn broadcast(&self, frame: &[u8]) {
        for client in self.clients.values() {
            client.queue(frame.to_vec());
        }
    }

    /// Whether one of these windows has too much waiting for it.
    fn behind(&self, clients: &HashSet<u64>) -> bool {
        clients.iter().filter_map(|c| self.clients.get(c)).any(Client::behind)
    }

    /// Closes the connections of those of these windows that are behind:
    /// what is queued for them goes, and they are sent nothing more.
    fn let_go(&mut self, clients: &HashSet<u64>) {
        for id in clients {
            if self.clients.get(id).is_some_and(Client::behind) {
                if let Some(client) = self.clients.remove(id) {
                    let _ = client.stream.shutdown(std::net::Shutdown::Both);
                }
            }
        }
    }
}

/// The most that ever waited for one window.
#[cfg(test)]
static MOST_QUEUED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

impl Client {
    fn behind(&self) -> bool {
        self.queued.load(std::sync::atomic::Ordering::Relaxed) > CLIENT_QUEUE_BYTES
    }

    fn queue(&self, frame: Vec<u8>) {
        let len = frame.len();
        let queued = self.queued.fetch_add(len, std::sync::atomic::Ordering::Relaxed) + len;
        #[cfg(test)]
        MOST_QUEUED.fetch_max(queued, std::sync::atomic::Ordering::Relaxed);
        let _ = queued;
        if self.tx.send(frame).is_err() {
            self.queued.fetch_sub(len, std::sync::atomic::Ordering::Relaxed);
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

/// `adopt`: started by the daemon this one replaces, which waits on fd 3
/// to hand over its sessions.
pub fn run(adopt: bool) {
    let path = proto::socket_path();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let shared: Shared = Arc::new(Mutex::new(Server { alone_since: Some(Instant::now()), ..Default::default() }));
    let mut handover = None;
    if adopt {
        match handover::receive(&shared) {
            Ok(channel) => handover = Some(channel),
            Err(e) => {
                eprintln!("ideae daemon: taking over failed: {e}");
                return;
            }
        }
    } else if UnixStream::connect(&path).is_ok() {
        return; // another daemon serves
    }
    // The old daemon's socket file goes; windows connect here from now on.
    let _ = std::fs::remove_file(&path);
    let Ok(listener) = UnixListener::bind(&path) else { return };
    if let Some(mut channel) = handover {
        let _ = channel.write_all(b"k");
    }

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
    let queued: Arc<std::sync::atomic::AtomicUsize> = Arc::default();
    let written = queued.clone();
    let writer_thread = std::thread::spawn(move || {
        while let Ok(frame) = rx.recv() {
            let sent = writer.write_all(&frame);
            written.fetch_sub(frame.len(), std::sync::atomic::Ordering::Relaxed);
            if sent.is_err() {
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
                let Ok(stream) = reader.try_clone() else { break };
                let mut s = shared.lock().unwrap();
                s.clients.insert(client, Client { tx: tx.clone(), queued: queued.clone(), stream });
                s.alone_since = None;
                registered = true;
                let welcome = Msg::Welcome {
                    protocol: proto::PROTOCOL,
                    version: crate::build_watch::build_id(),
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
            Frame::Msg(Msg::Upgrade { exe }) => handover::give(&shared, &exe),
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
    let mut behind_since: Option<Instant> = None;
    loop {
        {
            let mut s = shared.lock().unwrap();
            let Some(session) = s.sessions.get(&id) else { return };
            if !Arc::ptr_eq(&session.handle, &handle) {
                return;
            }
            // Not taken from the pty while a window has megabytes to read yet.
            if !s.behind(&session.attached) {
                behind_since = None;
            } else if behind_since.get_or_insert_with(Instant::now).elapsed() < CLIENT_STALL {
                drop(s);
                std::thread::sleep(Duration::from_millis(5));
                continue;
            } else {
                let attached = session.attached.clone();
                s.let_go(&attached);
                behind_since = None;
            }
        }
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
    // The window may have changed it since this daemon started.
    if !crate::state::load().settings.notifications {
        return;
    }
    let event = crate::agent::HookEvent {
        task: hook.task.clone().into(),
        event: hook.event.clone(),
        message: hook.message.clone(),
        kind: hook.kind.clone(),
        file: hook.file.clone(),
        replayed: false,
    };
    let Some(status) = event.status().filter(|s| s.is_attention()) else { return };
    let body = match status {
        crate::agent::AgentStatus::NeedsInput(message) => message,
        _ => "Finished".into(),
    };
    let task = std::path::Path::new(&hook.task).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let path = std::path::PathBuf::from(&hook.task);
    crate::desktop::notify_with(&task, &body, move || crate::desktop::open_window_on(&path));
}

/// Moving the sessions to a daemon of a newer build without stopping their
/// programs: the pty masters pass over a socket pair (SCM_RIGHTS), the
/// programs never notice.
mod handover {
    use std::io::{Read, Write};
    use std::os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd, RawFd};
    use std::os::unix::net::UnixStream;
    use std::path::Path;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use gpui_cn::terminal::ByteSink;

    use super::{Session, Shared, pump};
    use crate::daemon::proto::{Handover, Handovers};

    const FD: RawFd = 3;

    /// Old side: sends everything to a new daemon from `exe` and exits once
    /// it took over; on any failure it keeps serving.
    pub(super) fn give(shared: &Shared, exe: &Path) {
        let s = shared.lock().unwrap();
        let mut state = Handovers { hooks: s.hooks.values().cloned().collect(), ..Default::default() };
        let mut fds: Vec<OwnedFd> = Vec::new();
        for (id, session) in &s.sessions {
            let Some((fd, pid)) = session.handle.lock().unwrap().pty() else { continue };
            // SAFETY: dup returns a new descriptor this side owns.
            let copy = unsafe { libc::dup(fd) };
            if copy < 0 {
                continue;
            }
            fds.push(unsafe { OwnedFd::from_raw_fd(copy) });
            state.sessions.push(Handover {
                id: id.clone(),
                pid,
                size: session.size,
                fg: session.fg.clone(),
                replay: session.replay.iter().copied().collect(),
            });
        }
        let Ok(result) = send(exe, &state, &fds) else { return };
        if result {
            // The successor holds the ptys now: leave without ending anything.
            std::process::exit(0);
        }
    }

    fn send(exe: &Path, state: &Handovers, fds: &[OwnedFd]) -> std::io::Result<bool> {
        use std::os::unix::process::CommandExt as _;
        let (mut ours, theirs) = UnixStream::pair()?;
        let their_fd = theirs.as_raw_fd();
        let mut command = std::process::Command::new(exe);
        command
            .args(["--daemon", "--adopt"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        // SAFETY: dup2 and setsid only rearrange the child's descriptors and session.
        unsafe {
            command.pre_exec(move || {
                if their_fd == FD {
                    // dup2 onto itself keeps close-on-exec; clear it instead.
                    libc::fcntl(FD, libc::F_SETFD, 0);
                } else if libc::dup2(their_fd, FD) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                libc::setsid();
                Ok(())
            });
        }
        let mut child = command.spawn()?;
        drop(theirs);
        let body = serde_json::to_vec(state).map_err(std::io::Error::other)?;
        ours.write_all(&(body.len() as u32).to_be_bytes())?;
        ours.write_all(&body)?;
        send_fds(&ours, fds)?;
        ours.set_read_timeout(Some(Duration::from_secs(15)))?;
        let mut ack = [0u8; 1];
        let taken = ours.read_exact(&mut ack).is_ok() && ack[0] == b'k';
        if !taken {
            let _ = child.kill();
        }
        std::thread::spawn(move || child.wait());
        Ok(taken)
    }

    fn send_fds(stream: &UnixStream, fds: &[OwnedFd]) -> std::io::Result<()> {
        let raw: Vec<RawFd> = fds.iter().map(|f| f.as_raw_fd()).collect();
        let mut byte = [b'f'];
        let mut iov = libc::iovec { iov_base: byte.as_mut_ptr().cast(), iov_len: 1 };
        let bytes = std::mem::size_of_val(raw.as_slice()) as u32;
        // SAFETY: the control buffer is sized by CMSG_SPACE for `raw`, and the
        // header is filled within it.
        unsafe {
            let space = libc::CMSG_SPACE(bytes) as usize;
            let mut control = vec![0u8; space.max(1)];
            let mut msg: libc::msghdr = std::mem::zeroed();
            msg.msg_iov = &mut iov;
            msg.msg_iovlen = 1;
            if !raw.is_empty() {
                msg.msg_control = control.as_mut_ptr().cast();
                msg.msg_controllen = space as _;
                let cmsg = libc::CMSG_FIRSTHDR(&msg);
                (*cmsg).cmsg_level = libc::SOL_SOCKET;
                (*cmsg).cmsg_type = libc::SCM_RIGHTS;
                (*cmsg).cmsg_len = libc::CMSG_LEN(bytes) as _;
                std::ptr::copy_nonoverlapping(raw.as_ptr(), libc::CMSG_DATA(cmsg).cast::<RawFd>(), raw.len());
            }
            if libc::sendmsg(stream.as_raw_fd(), &msg, 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
        }
        Ok(())
    }

    fn receive_fds(stream: &UnixStream, count: usize) -> std::io::Result<Vec<OwnedFd>> {
        let mut byte = [0u8; 1];
        let mut iov = libc::iovec { iov_base: byte.as_mut_ptr().cast(), iov_len: 1 };
        let bytes = (count.max(1) * std::mem::size_of::<RawFd>()) as u32;
        let mut fds = Vec::new();
        // SAFETY: as in send_fds; descriptors the kernel installed are taken
        // into OwnedFd once each.
        unsafe {
            let space = libc::CMSG_SPACE(bytes) as usize;
            let mut control = vec![0u8; space];
            let mut msg: libc::msghdr = std::mem::zeroed();
            msg.msg_iov = &mut iov;
            msg.msg_iovlen = 1;
            msg.msg_control = control.as_mut_ptr().cast();
            msg.msg_controllen = space as _;
            #[cfg(target_os = "linux")]
            let flags = libc::MSG_CMSG_CLOEXEC;
            #[cfg(not(target_os = "linux"))]
            let flags = 0;
            if libc::recvmsg(stream.as_raw_fd(), &mut msg, flags) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            let mut cmsg = libc::CMSG_FIRSTHDR(&msg);
            while !cmsg.is_null() {
                if (*cmsg).cmsg_level == libc::SOL_SOCKET && (*cmsg).cmsg_type == libc::SCM_RIGHTS {
                    let n = ((*cmsg).cmsg_len as usize - libc::CMSG_LEN(0) as usize) / std::mem::size_of::<RawFd>();
                    let data = libc::CMSG_DATA(cmsg).cast::<RawFd>();
                    for i in 0..n {
                        let fd = *data.add(i);
                        // Without MSG_CMSG_CLOEXEC (macOS) it is set here.
                        libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
                        fds.push(OwnedFd::from_raw_fd(fd));
                    }
                }
                cmsg = libc::CMSG_NXTHDR(&msg, cmsg);
            }
        }
        Ok(fds)
    }

    /// New side: takes the sessions from fd 3 and returns the channel for
    /// the acknowledgement, sent once the socket is ours.
    pub(super) fn receive(shared: &Shared) -> std::io::Result<UnixStream> {
        // SAFETY: the old daemon put the socket pair's other end at fd 3.
        let mut channel = unsafe { UnixStream::from_raw_fd(FD) };
        let mut len = [0u8; 4];
        channel.read_exact(&mut len)?;
        let mut body = vec![0u8; u32::from_be_bytes(len) as usize];
        channel.read_exact(&mut body)?;
        let state: Handovers = serde_json::from_slice(&body).map_err(std::io::Error::other)?;
        let fds = receive_fds(&channel, state.sessions.len())?;
        let mut s = shared.lock().unwrap();
        for hook in state.hooks {
            s.hooks.insert(hook.task.clone(), hook);
        }
        for (session, fd) in state.sessions.into_iter().zip(fds) {
            let program = crate::procs::name(session.pid);
            let (sink, detached) = ByteSink::detached();
            let Ok(handle) = gpui_cn::terminal::adopt(fd, session.pid, program, sink) else { continue };
            let handle = Arc::new(Mutex::new(handle));
            let detached = Arc::new(detached);
            s.sessions.insert(
                session.id.clone(),
                Session {
                    handle: handle.clone(),
                    sink: detached.clone(),
                    replay: session.replay.into_iter().collect(),
                    attached: Default::default(),
                    pending: Vec::new(),
                    fg: session.fg,
                    size: session.size,
                },
            );
            let shared = shared.clone();
            let id = session.id;
            std::thread::spawn(move || pump(shared, id, handle, detached));
        }
        Ok(channel)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read as _;

    /// A program that prints without end, into a window that reads nothing:
    /// what waits for the window stays a few megabytes, the program held
    /// back through its pty.
    #[test]
    #[ignore]
    fn a_window_that_does_not_read_holds_the_program_back() {
        let _one = crate::workspace::variants::tests::ONE_WINDOW.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("ideae-daemon-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // SAFETY: set before anything of the daemon reads it.
        unsafe { std::env::set_var("IDEAE_STATE", dir.join("state.json")) };
        std::thread::spawn(|| run(false));
        let path = proto::socket_path();
        let mut stream = (0..200)
            .find_map(|_| {
                std::thread::sleep(Duration::from_millis(20));
                UnixStream::connect(&path).ok()
            })
            .expect("the daemon listens");
        proto::write_msg(&mut stream, &Msg::Hello { protocol: proto::PROTOCOL, version: crate::build_watch::build_id() }).unwrap();
        assert!(matches!(proto::read_frame(&mut stream), Ok(Frame::Msg(Msg::Welcome { .. }))));
        let spec = Spec { cwd: dir.clone(), env: Vec::new(), input: Some("yes 0123456789012345678901234567890123456789\n".into()) };
        let size = Size { cols: 80, rows: 24, cell_w: 8, cell_h: 16 };
        proto::write_msg(&mut stream, &Msg::Open { id: "flood".into(), spec, size }).unwrap();

        stream.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        // A second of `yes` is hundreds of megabytes when nothing holds it back.
        std::thread::sleep(Duration::from_secs(1));
        // And a window that goes on taking nothing is let go of.
        std::thread::sleep(CLIENT_STALL + Duration::from_secs(2));
        let (mut waited, mut chunk, mut closed) = (0usize, vec![0u8; 1 << 16], false);
        loop {
            match stream.read(&mut chunk) {
                Ok(0) => {
                    closed = true;
                    break;
                }
                // Without end when nothing holds the program back: fail, not hang.
                Ok(n) if waited > 16 * CLIENT_QUEUE_BYTES => panic!("{} bytes and more to come", waited + n),
                Ok(n) => waited += n,
                Err(_) => break,
            }
        }
        let most = MOST_QUEUED.load(std::sync::atomic::Ordering::Relaxed);
        assert!(most > 100_000 && most < 3 * CLIENT_QUEUE_BYTES, "{most} bytes waited for a window that read none");
        let _ = waited;
        assert!(closed, "the daemon closed the connection of a window that took nothing");

        // The program ran on; the window, back, finds it and what it printed last.
        let mut stream = UnixStream::connect(&path).unwrap();
        proto::write_msg(&mut stream, &Msg::Hello { protocol: proto::PROTOCOL, version: crate::build_watch::build_id() }).unwrap();
        let Ok(Frame::Msg(Msg::Welcome { sessions, .. })) = proto::read_frame(&mut stream) else { panic!("no welcome") };
        assert_eq!(sessions, ["flood".to_string()]);
        let spec = Spec { cwd: dir.clone(), env: Vec::new(), input: None };
        proto::write_msg(&mut stream, &Msg::Open { id: "flood".into(), spec, size }).unwrap();
        let replay = (0..50).find_map(|_| match proto::read_frame(&mut stream) {
            Ok(Frame::Bytes { kind: KIND_REPLAY, bytes, .. }) => Some(bytes),
            _ => None,
        });
        assert!(replay.is_some_and(|bytes| bytes.windows(10).any(|w| w == b"0123456789")), "the replay has what it printed");
        proto::write_msg(&mut stream, &Msg::Kill { id: "flood".into() }).unwrap();
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
