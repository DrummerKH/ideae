//! The system kept from sleeping while agents work: the screen still dims
//! and locks, only the machine stays up. A thread of its own holds it, as
//! taking and letting go asks the system (a process on Linux, IPC on macOS)
//! and Windows ties it to the thread that asked.

use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::time::Duration;

#[derive(Default)]
pub struct Awake {
    to: Option<Sender<(bool, bool)>>,
    told: Option<(bool, bool)>,
}

impl Awake {
    /// Whether something works that the machine should stay up for, and
    /// whether only while plugged in. Cheap when nothing changed.
    pub fn want(&mut self, on: bool, plugged_only: bool) {
        if cfg!(test) {
            return;
        }
        let wish = (on, plugged_only && on);
        if self.told == Some(wish) || (self.told.is_none() && !on) {
            return;
        }
        self.told = Some(wish);
        if self.to.is_none() {
            let (to, from) = std::sync::mpsc::channel();
            if std::thread::Builder::new().name("awake".into()).spawn(move || keep(from)).is_err() {
                return;
            }
            self.to = Some(to);
        }
        if let Some(to) = &self.to {
            let _ = to.send(wish);
        }
    }
}

fn keep(from: Receiver<(bool, bool)>) {
    let (mut on, mut plugged_only) = (false, false);
    let mut hold: Option<Hold> = None;
    loop {
        // Plugged in or out is looked at again now and then while it matters.
        let wait = if on && plugged_only { Duration::from_secs(60) } else { Duration::from_secs(3600) };
        match from.recv_timeout(wait) {
            Ok(wish) => (on, plugged_only) = wish,
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
        let up = on && !(plugged_only && crate::battery::read().is_some_and(|(_, used)| used));
        if up != hold.is_some() {
            hold = if up { Hold::take() } else { None };
        }
    }
}

#[cfg(target_os = "macos")]
struct Hold(u32);

#[cfg(target_os = "macos")]
impl Hold {
    fn take() -> Option<Self> {
        use std::ffi::{c_char, c_void};
        #[link(name = "CoreFoundation", kind = "framework")]
        unsafe extern "C" {
            fn CFStringCreateWithCString(alloc: *const c_void, text: *const c_char, encoding: u32) -> *const c_void;
            fn CFRelease(object: *const c_void);
        }
        #[link(name = "IOKit", kind = "framework")]
        unsafe extern "C" {
            fn IOPMAssertionCreateWithName(kind: *const c_void, level: u32, name: *const c_void, id: *mut u32) -> i32;
        }
        const UTF8: u32 = 0x0800_0100;
        const ON: u32 = 255;
        // SAFETY: both strings are made here, given to the call and released after it.
        unsafe {
            let kind = CFStringCreateWithCString(std::ptr::null(), c"PreventUserIdleSystemSleep".as_ptr(), UTF8);
            let name = CFStringCreateWithCString(std::ptr::null(), c"Agents at work in ideae".as_ptr(), UTF8);
            if kind.is_null() || name.is_null() {
                return None;
            }
            let mut id = 0;
            let done = IOPMAssertionCreateWithName(kind, ON, name, &mut id);
            CFRelease(kind);
            CFRelease(name);
            (done == 0).then_some(Hold(id))
        }
    }
}

#[cfg(target_os = "macos")]
impl Drop for Hold {
    fn drop(&mut self) {
        #[link(name = "IOKit", kind = "framework")]
        unsafe extern "C" {
            fn IOPMAssertionRelease(id: u32) -> i32;
        }
        // SAFETY: the id is one the system gave and not released yet.
        unsafe { IOPMAssertionRelease(self.0) };
    }
}

/// logind's inhibitor lasts as long as `systemd-inhibit` runs; it dies with
/// the app (`spawn_tied`), and so does what it holds.
#[cfg(target_os = "linux")]
struct Hold(std::process::Child);

#[cfg(target_os = "linux")]
impl Hold {
    fn take() -> Option<Self> {
        let mut command = crate::procs::command("systemd-inhibit");
        command
            .args(["--what=idle:sleep", "--who=ideae", "--why=Agents at work", "--mode=block", "sleep", "infinity"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        crate::procs::spawn_tied(&mut command).ok().map(Hold)
    }
}

#[cfg(target_os = "linux")]
impl Drop for Hold {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Windows ties the state to the thread that set it: this one, as long as it holds.
#[cfg(windows)]
struct Hold;

#[cfg(windows)]
#[link(name = "kernel32")]
unsafe extern "system" {
    fn SetThreadExecutionState(flags: u32) -> u32;
}

#[cfg(windows)]
const CONTINUOUS: u32 = 0x8000_0000;

#[cfg(windows)]
impl Hold {
    fn take() -> Option<Self> {
        const SYSTEM_REQUIRED: u32 = 0x0000_0001;
        // SAFETY: a flag of the calling thread.
        (unsafe { SetThreadExecutionState(CONTINUOUS | SYSTEM_REQUIRED) } != 0).then_some(Hold)
    }
}

#[cfg(windows)]
impl Drop for Hold {
    fn drop(&mut self) {
        // SAFETY: as above.
        unsafe { SetThreadExecutionState(CONTINUOUS) };
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
struct Hold;

#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
impl Hold {
    fn take() -> Option<Self> {
        None
    }
}

#[cfg(test)]
mod tests {
    /// Run by hand after `cargo build`: a real daemon (target/debug/ideae)
    /// holds this machine up while an agent's turn runs in it, and lets go
    /// when it ends.
    #[cfg(target_os = "macos")]
    #[test]
    #[ignore]
    fn a_daemon_holds_its_machine_up_while_an_agent_works() {
        use crate::daemon::proto::{self, Frame, Hook, Msg, Size, Spec};
        use std::time::{Duration, Instant};
        let dir = std::env::temp_dir().join(format!("ideae-awake-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let state = dir.join("state.json");
        let exe = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../target/debug/ideae");
        let mut daemon = std::process::Command::new(&exe).arg("--daemon").env("IDEAE_STATE", &state).spawn().expect("target/debug/ideae: cargo build first");
        let socket = proto::socket_for(Some(state.as_os_str()));
        let mut w = (0..200)
            .find_map(|_| {
                std::thread::sleep(Duration::from_millis(25));
                std::os::unix::net::UnixStream::connect(&socket).ok()
            })
            .expect("the daemon listens");
        proto::write_msg(&mut w, &Msg::Hello { protocol: proto::PROTOCOL, version: crate::build::id() }).unwrap();
        let spec = Spec { cwd: dir.clone(), env: Vec::new(), input: None, program: vec!["sleep".into(), "60".into()] };
        proto::write_msg(&mut w, &Msg::Open { id: "/w/a#agent".into(), spec, size: Size { cols: 80, rows: 24, cell_w: 8, cell_h: 16 } }).unwrap();
        let hook = |event: &str| Msg::Hook(Hook { task: "/w/a".into(), event: event.into(), message: None, kind: None, file: None, said: None, session: None, worktree: None, agent: None, call: None, at: 0 });
        let held = || {
            let out = String::from_utf8_lossy(&std::process::Command::new("pmset").args(["-g", "assertions"]).output().unwrap().stdout).into_owned();
            out.lines().any(|l| l.contains(&format!("pid {}(", daemon.id())) && l.contains("Agents at work in ideae"))
        };
        let until = |want: bool| {
            let start = Instant::now();
            while held() != want {
                assert!(start.elapsed() < Duration::from_secs(15), "held: {}", !want);
                std::thread::sleep(Duration::from_millis(250));
            }
        };
        proto::write_msg(&mut w, &hook("UserPromptSubmit")).unwrap();
        until(true);
        proto::write_msg(&mut w, &hook("Stop")).unwrap();
        until(false);
        proto::write_msg(&mut w, &Msg::Shutdown).unwrap();
        while !matches!(proto::read_frame(&mut w), Err(_) | Ok(Frame::Msg(Msg::Stays))) {}
        let _ = daemon.kill();
        let _ = daemon.wait();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Run by hand: it holds the real machine up for a moment.
    #[test]
    #[ignore]
    fn the_system_is_held_and_let_go() {
        let hold = super::Hold::take().expect("held");
        #[cfg(target_os = "macos")]
        {
            let held = || String::from_utf8_lossy(&std::process::Command::new("pmset").args(["-g", "assertions"]).output().unwrap().stdout).contains("Agents at work in ideae");
            assert!(held());
            drop(hold);
            assert!(!held());
        }
        #[cfg(not(target_os = "macos"))]
        drop(hold);
    }
}
