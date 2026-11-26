//! A port of this machine's loopback that reaches a socket of its user's
//! alone: a task's Chrome of a window elsewhere, carried here by ssh to a
//! socket in `~/.ssh`. Its agents here ask the port (IDEAE_BROWSER_CDP);
//! a connection from another user's process is closed unanswered, as
//! DevTools ask nobody who they are.

use std::collections::HashMap;
use std::net::{TcpListener, TcpStream};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// A port held for a window (the daemon's client) to the socket of its run.
struct Relay {
    owner: u64,
    to: PathBuf,
    stop: Arc<AtomicBool>,
    accepting: std::thread::JoinHandle<()>,
    /// Its connections joined to the socket, until they end: closed with
    /// it, as its forward is gone.
    conns: Arc<Mutex<HashMap<u64, TcpStream>>>,
}

static RELAYS: Mutex<Option<HashMap<u16, Relay>>> = Mutex::new(None);

/// Port `port` reaches `socket` for window `owner` from now on; None lets
/// go of it. A port another window at work holds is not taken from it (two
/// laptops on one task: the second does without, as its own ssh forward
/// would); one whose window is no longer at work (`at_work`: a laptop
/// asleep, its link gone unnoticed; a run before this one) is. The same
/// socket asked by another connection is the same window's run, its link
/// made anew: it is that window's now. A window gone by the time it is
/// asked gets none.
pub fn set(owner: u64, port: u16, socket: Option<PathBuf>, at_work: &dyn Fn(u64) -> bool) -> Result<(), String> {
    let mut relays = RELAYS.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let relays = relays.get_or_insert_with(HashMap::new);
    let Some(socket) = socket else {
        if relays.get(&port).is_some_and(|relay| relay.owner == owner) {
            if let Some(relay) = relays.remove(&port) {
                end(relay, port);
            }
        }
        return Ok(());
    };
    if !at_work(owner) {
        return Err("its window has gone".into());
    }
    if let Some(relay) = relays.get_mut(&port) {
        if relay.to == socket {
            relay.owner = owner;
            return Ok(());
        }
        if at_work(relay.owner) {
            return Err(format!("port {port} here is another window's (the same task open on two computers): its Chrome is theirs"));
        }
        if let Some(relay) = relays.remove(&port) {
            end(relay, port);
        }
    }
    let listener = bind(port)?;
    let (stop, conns) = (Arc::new(AtomicBool::new(false)), Arc::new(Mutex::new(HashMap::<u64, TcpStream>::new())));
    let (to, stopping, held) = (socket.clone(), stop.clone(), conns.clone());
    let accepting = std::thread::spawn(move || {
        for conn in listener.incoming() {
            if stopping.load(Ordering::SeqCst) {
                break;
            }
            let conn = match conn {
                Ok(conn) => conn,
                // Out of descriptors, say: not again at once.
                Err(_) => {
                    std::thread::sleep(std::time::Duration::from_millis(100));
                    continue;
                }
            };
            let (to, held) = (to.clone(), held.clone());
            // Refused or not joined, it is closed as its thread ends: kept only while joined.
            std::thread::spawn(move || {
                if !ours(&conn) {
                    return;
                }
                let Ok(there) = UnixStream::connect(&to) else { return };
                static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
                let n = NEXT.fetch_add(1, Ordering::Relaxed);
                if let Ok(kept) = conn.try_clone() {
                    held.lock().unwrap_or_else(std::sync::PoisonError::into_inner).insert(n, kept);
                }
                join(conn, there);
                held.lock().unwrap_or_else(std::sync::PoisonError::into_inner).remove(&n);
            });
        }
    });
    relays.insert(port, Relay { owner, to: socket, stop, accepting, conns });
    Ok(())
}

/// The port bound; one let go of a moment ago may still be the system's
/// for its last connections' closing: waited for a while.
fn bind(port: u16) -> Result<TcpListener, String> {
    let until = std::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        match TcpListener::bind(("127.0.0.1", port)) {
            Ok(listener) => return Ok(listener),
            Err(e) if e.kind() == std::io::ErrorKind::AddrInUse && std::time::Instant::now() < until => {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            Err(e) => return Err(format!("port {port} here: {e}")),
        }
    }
}

/// The ports window `owner` held: it has gone (quit, its link dropped; a
/// link made anew asks again).
pub fn drop_owner(owner: u64) {
    let mut relays = RELAYS.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(relays) = relays.as_mut() else { return };
    let gone: Vec<u16> = relays.iter().filter(|(_, relay)| relay.owner == owner).map(|(&port, _)| port).collect();
    for port in gone {
        if let Some(relay) = relays.remove(&port) {
            end(relay, port);
        }
    }
}

/// Stops its accepting and waits for the port to be free: it may be asked
/// for again at once (a task's view closed and opened).
fn end(relay: Relay, port: u16) {
    relay.stop.store(true, Ordering::SeqCst);
    // Its accept returns, and sees it is to stop.
    let _ = TcpStream::connect(("127.0.0.1", port));
    let _ = relay.accepting.join();
    for (_, conn) in relay.conns.lock().unwrap_or_else(std::sync::PoisonError::into_inner).drain() {
        let _ = conn.shutdown(std::net::Shutdown::Both);
    }
}

/// Bytes each way until either end closes.
fn join(tcp: TcpStream, unix: UnixStream) {
    let (Ok(mut tcp_in), Ok(mut unix_out)) = (tcp.try_clone(), unix.try_clone()) else { return };
    std::thread::spawn(move || {
        let _ = std::io::copy(&mut tcp_in, &mut unix_out);
        let _ = unix_out.shutdown(std::net::Shutdown::Write);
    });
    let (mut unix_in, mut tcp_out) = (unix, tcp);
    let _ = std::io::copy(&mut unix_in, &mut tcp_out);
    let _ = tcp_out.shutdown(std::net::Shutdown::Both);
}

/// Whether the process at the other end of `conn`, on this machine's
/// loopback, is this user's.
fn ours(conn: &TcpStream) -> bool {
    let (Ok(peer), Ok(local)) = (conn.peer_addr(), conn.local_addr()) else { return false };
    if !peer.ip().is_loopback() {
        return false;
    }
    owner_is_us(peer.port(), local.port())
}

/// Its socket in the kernel's table: local address the peer's, remote
/// ours, both 127.0.0.1; the eighth column its owner.
#[cfg(target_os = "linux")]
fn owner_is_us(peer: u16, port: u16) -> bool {
    // SAFETY: getuid cannot fail.
    let uid = unsafe { libc::getuid() }.to_string();
    let (from, to) = (format!("0100007F:{peer:04X}"), format!("0100007F:{port:04X}"));
    std::fs::read_to_string("/proc/net/tcp").unwrap_or_default().lines().skip(1).any(|line| {
        let f: Vec<&str> = line.split_whitespace().collect();
        f.len() > 7 && f[1] == from && f[2] == to && f[7] == uid
    })
}

/// Among the sockets of this user's processes (lsof sees no other's
/// without root): one from the peer's port to ours.
#[cfg(target_os = "macos")]
fn owner_is_us(peer: u16, port: u16) -> bool {
    // SAFETY: getuid cannot fail.
    let uid = unsafe { libc::getuid() }.to_string();
    let Ok(out) = std::process::Command::new("lsof").args(["-n", "-P", "-a", "-u", &uid, &format!("-iTCP@127.0.0.1:{peer}"), "-Fn"]).output() else { return false };
    let want = format!("n127.0.0.1:{peer}->127.0.0.1:{port}");
    String::from_utf8_lossy(&out.stdout).lines().any(|line| line == want)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn owner_is_us(_: u16, _: u16) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read as _, Write as _};

    /// This user's connection reaches the socket; once let go of, the port is free.
    #[test]
    fn a_port_reaches_a_socket_of_its_user() {
        let yes = |_| true;
        let dir = std::env::temp_dir().join(format!("ideae-relay-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let socket = dir.join("cdp.sock");
        let server = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        std::thread::spawn(move || {
            for mut conn in server.incoming().flatten() {
                let mut asked = [0u8; 5];
                let _ = conn.read_exact(&mut asked);
                let _ = conn.write_all(b"from Chrome");
            }
        });
        // Below the ephemeral range, as a task's Chrome's is: one of there,
        // once let go of, may be any connection's own before it is bound again.
        let port = (0..100u16)
            .find_map(|n| {
                let port = 31000 - (std::process::id() as u16 % 4000) - n;
                set(1, port, Some(socket.clone()), &yes).ok().map(|()| port)
            })
            .expect("a free port");
        set(2, port, Some(socket.clone()), &yes).expect("asked again by its window with its link made anew");
        let theirs = socket.with_file_name("theirs.sock");
        assert!(set(3, port, Some(theirs.clone()), &yes).is_err(), "another window's at work is not taken");
        assert!(set(4, port, Some(theirs.clone()), &|w| w != 4).is_err(), "a window gone by then gets none");
        set(3, port, None, &yes).unwrap();
        let mut conn = TcpStream::connect(("127.0.0.1", port)).unwrap();
        conn.set_read_timeout(Some(std::time::Duration::from_secs(5))).unwrap();
        conn.write_all(b"hello").unwrap();
        let mut said = String::new();
        let _ = conn.read_to_string(&mut said);
        assert_eq!(said, "from Chrome");
        assert!(!owner_is_us(1, port), "no such connection is no one's");
        assert!(TcpStream::connect(("127.0.0.1", port)).is_ok(), "another window letting go of it leaves it");
        set(2, port, None, &yes).unwrap();
        set(3, port, Some(theirs.clone()), &yes).expect("let go of, and taken again at once");
        // One whose window is no longer at work (a run before, a laptop asleep): taken from it.
        set(5, port, Some(socket.clone()), &|w| w != 3).expect("taken from a window no longer at work");
        drop_owner(5);
        set(3, port, Some(theirs), &yes).unwrap();
        drop_owner(3);
        assert!(bind(port).is_ok(), "let go of with its window");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
