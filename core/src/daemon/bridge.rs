//! `ideae --daemon --stdio`: what ssh runs on another machine. It joins
//! its stdin and stdout to the daemon there, starting one when none runs.
//! The daemon is not this process: when the link drops, ssh ends it, and
//! the agents go on in the daemon.

use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::FromRawFd as _;
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

use super::proto::{self, Msg};

/// Printed before the first frame: what a login shell prints before it
/// (a greeting in `.bashrc`, a notice of the system) is skipped up to it.
pub const MARK: &[u8] = b"\nideae-daemon-stdio-1\n";

pub fn run() {
    let Some(stream) = connect() else {
        eprintln!("ideae: no daemon at {}", proto::socket_path().display());
        std::process::exit(1);
    };
    // Unbuffered: the standard library's stdout holds a frame's tail until
    // a newline.
    // SAFETY: descriptors 0 and 1 are this process's and nothing else here
    // uses them.
    let (mut stdin, mut stdout) = unsafe { (File::from_raw_fd(0), File::from_raw_fd(1)) };
    if stdout.write_all(MARK).is_err() {
        return;
    }
    let Ok(mut to_daemon) = stream.try_clone() else { return };
    std::thread::spawn(move || {
        let _ = std::io::copy(&mut stdin, &mut to_daemon);
        // ssh is gone: so is this window's connection.
        std::process::exit(0);
    });
    let mut from_daemon = stream;
    let mut chunk = vec![0u8; 1 << 16];
    loop {
        match from_daemon.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if stdout.write_all(&chunk[..n]).is_err() {
                    break;
                }
            }
        }
    }
}

fn connect() -> Option<UnixStream> {
    // Another user's folder could hold a socket of theirs, to be given
    // this one's typing.
    if let Err(e) = crate::paths::own_runtime_dir() {
        eprintln!("ideae: {e}");
        return None;
    }
    let stream = reach()?;
    // A daemon outlives every window: one of an earlier build would not
    // answer what this one asks (calls). It hands its sessions to one of
    // this build, the programs going on; one that speaks other frames is
    // replaced.
    match welcome(stream) {
        // Handed over forward only: to a build that can do what it cannot,
        // or to a newer one. Two windows of two builds then settle on one
        // daemon instead of handing it back and forth at each dial.
        Some((protocol, version, calls)) if protocol == proto::PROTOCOL && calls && !newer(&crate::build::id(), &version) => {}
        Some((protocol, _, _)) => {
            let mut old = reach()?;
            let ask = if protocol == proto::PROTOCOL { Msg::Upgrade { exe: crate::paths::exe().ok()? } } else { Msg::Shutdown };
            proto::write_msg(&mut old, &Msg::Hello { protocol: proto::PROTOCOL, version: crate::build::id() }).ok()?;
            let _ = old.set_read_timeout(Some(Duration::from_secs(20)));
            let _ = proto::read_frame(&mut old);
            proto::write_msg(&mut old, &ask).ok()?;
            // It goes once done, or says it stays (and is used as it is).
            while matches!(proto::read_frame(&mut old), Ok(frame) if !matches!(frame, proto::Frame::Msg(Msg::Stays))) {}
            std::thread::sleep(Duration::from_millis(200));
        }
        None => {}
    }
    reach()
}

/// Whether build `ours` is newer than `theirs`: by the time of the file
/// each runs from (the last word of a build's id), put on the server when
/// its window first needed it.
fn newer(ours: &str, theirs: &str) -> bool {
    let time = |id: &str| id.rsplit(' ').next().and_then(|t| t.parse::<u64>().ok()).unwrap_or(0);
    time(ours) > time(theirs)
}

/// What the daemon says of itself: its protocol, build and whether it answers calls.
fn welcome(mut stream: UnixStream) -> Option<(u32, String, bool)> {
    proto::write_msg(&mut stream, &Msg::Hello { protocol: proto::PROTOCOL, version: crate::build::id() }).ok()?;
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    match proto::read_frame(&mut stream) {
        Ok(proto::Frame::Msg(Msg::Welcome { protocol, version, calls, .. })) => Some((protocol, version, calls)),
        _ => None,
    }
}

/// A connection to the daemon, started when none listens (or none yet,
/// right after a handover).
fn reach() -> Option<UnixStream> {
    let path = proto::socket_path();
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut started = false;
    loop {
        if let Ok(stream) = UnixStream::connect(&path) {
            return Some(stream);
        }
        if Instant::now() > deadline {
            return None;
        }
        // A successor binds within moments: given those before one is started.
        if !started && deadline - Instant::now() < Duration::from_secs(9) {
            started = true;
            super::start().ok()?;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_daemon_is_handed_forward_only() {
        assert!(super::newer("0.1.0 (abc) 1700000100", "0.1.0 (abc) 1700000000"));
        assert!(!super::newer("0.1.0 (abc) 1700000000", "0.1.0 (def) 1700000100"), "an older build leaves a newer daemon be");
        assert!(!super::newer("0.1.0 (abc) 1700000000", "0.1.0 (abc) 1700000000"));
    }
}
