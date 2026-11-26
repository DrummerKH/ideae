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

/// Said when ideae on that machine runs a daemon of a version this window
/// cannot speak with: it is that machine's to update.
pub const APP_OLDER: &str = "ideae on that machine is a version this window cannot work with: update it there (its daemon is its desktop's, not replaced from here)";

/// Said when the daemon there is of another version and another window is
/// at work on it: replacing it would end that window's agents.
pub const OTHER_VERSION: &str = "ideae there is another version, and another window is at work on it: it is replaced when that window has gone (Exit there, or its link closed)";

pub fn run() {
    let stream = match connect() {
        Ok(stream) => stream,
        Err(said) => {
            eprintln!("ideae: {said}");
            std::process::exit(1);
        }
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

fn connect() -> Result<UnixStream, String> {
    let none = || format!("no daemon at {}", proto::socket_path().display());
    // Another user's folder could hold a socket of theirs, to be given
    // this one's typing.
    crate::paths::own_runtime_dir().map_err(|e| e.to_string())?;
    let stream = reach().ok_or_else(none)?;
    // A daemon outlives every window: one of an earlier build would not
    // answer what this one asks (calls). It hands its sessions to one of
    // this build, the programs going on; one that speaks other frames is
    // replaced.
    match welcome(stream).map(|(protocol, version, can, app)| (protocol, decide(protocol, &version, can, app))) {
        None | Some((_, Found::Use)) => {}
        Some((_, Found::AppOlder)) => return Err(APP_OLDER.into()),
        Some((protocol, Found::Replace)) => {
            let mut old = reach().ok_or_else(none)?;
            let ask = if protocol == proto::PROTOCOL { Msg::Upgrade { exe: crate::paths::exe().map_err(|e| e.to_string())? } } else { Msg::Shutdown };
            proto::write_msg(&mut old, &Msg::Hello { protocol: proto::PROTOCOL, version: crate::build::id() }).map_err(|e| e.to_string())?;
            let _ = old.set_read_timeout(Some(Duration::from_secs(20)));
            let _ = proto::read_frame(&mut old);
            proto::write_msg(&mut old, &ask).map_err(|e| e.to_string())?;
            // It goes once done, or says it stays (and is used as it is).
            let mut stays = false;
            while let Ok(frame) = proto::read_frame(&mut old) {
                if matches!(frame, proto::Frame::Msg(Msg::Stays)) {
                    stays = true;
                    break;
                }
            }
            // One that speaks other frames and stays for another window's sake cannot be used.
            if stays && protocol != proto::PROTOCOL {
                return Err(OTHER_VERSION.into());
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    }
    reach().ok_or_else(none)
}

#[derive(Debug, PartialEq)]
enum Found {
    Use,
    Replace,
    /// The app of that machine runs it, and it cannot do what this asks.
    AppOlder,
}

/// What a bridge of this build does with the daemon it found.
fn decide(protocol: u32, version: &str, can: bool, app: bool) -> Found {
    let same = protocol == proto::PROTOCOL;
    match () {
        // Handed over forward only: to a build that can do what it cannot,
        // or to a newer one. Two windows of two builds then settle on one
        // daemon instead of handing it back and forth at each dial.
        _ if same && can && !newer(&crate::build::id(), version) => Found::Use,
        // The app of that machine runs it (its desktop is told of agents
        // when no window is there): not replaced by this one, which cannot.
        _ if app && same && can => Found::Use,
        _ if app => Found::AppOlder,
        _ => Found::Replace,
    }
}

/// Whether build `ours` is newer than `theirs`: by the time of the file
/// each runs from (the last word of a build's id), put on the server when
/// its window first needed it.
fn newer(ours: &str, theirs: &str) -> bool {
    let time = |id: &str| id.rsplit(' ').next().and_then(|t| t.parse::<u64>().ok()).unwrap_or(0);
    time(ours) > time(theirs)
}

/// What the daemon says of itself: its protocol, build, whether it does
/// all a window asks of it (calls, watching), and whether the app of this
/// machine runs it.
fn welcome(mut stream: UnixStream) -> Option<(u32, String, bool, bool)> {
    proto::write_msg(&mut stream, &Msg::Hello { protocol: proto::PROTOCOL, version: crate::build::id() }).ok()?;
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    match proto::read_frame(&mut stream) {
        Ok(proto::Frame::Msg(Msg::Welcome { protocol, version, calls, watches, app, .. })) => Some((protocol, version, calls && watches, app)),
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

    #[test]
    fn a_bridge_keeps_its_machine_s_app_daemon() {
        use super::{Found, decide, proto};
        let ours = crate::build::id();
        let older = "build 1";
        let same = proto::PROTOCOL;
        assert_eq!(decide(same, &ours, true, false), Found::Use);
        assert_eq!(decide(same, older, true, false), Found::Replace, "an older ideae-server's: this build's from now on");
        assert_eq!(decide(same, older, true, true), Found::Use, "the desktop's, older: kept");
        assert_eq!(decide(same, older, false, true), Found::AppOlder, "the desktop's, unable: told to update there");
        assert_eq!(decide(same + 1, older, true, true), Found::AppOlder);
        assert_eq!(decide(same + 1, older, true, false), Found::Replace);
    }
}
