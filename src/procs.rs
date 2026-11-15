//! Ending what the app started when it quits. Closing a terminal ends its
//! session on a background thread, which a quitting app does not wait for.

use std::time::{Duration, Instant};

fn pids() -> Vec<i32> {
    std::fs::read_dir("/proc")
        .map(|dir| dir.flatten().filter_map(|e| e.file_name().to_str()?.parse().ok()).collect())
        .unwrap_or_default()
}

fn ppid(pid: i32) -> Option<i32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let (_, rest) = stat.rsplit_once(')')?;
    rest.split_whitespace().nth(1)?.parse().ok()
}

fn sid(pid: i32) -> i32 {
    // SAFETY: getsid only reads the process table.
    unsafe { libc::getsid(pid) }
}

fn running(pid: i32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/stat"))
        .ok()
        .and_then(|s| s.rsplit_once(')').map(|(_, rest)| !rest.trim_start().starts_with('Z')))
        .unwrap_or(false)
}

/// Every session led by a child of this process (the terminals' shells):
/// the shell, the agent, and whatever they started, nohup included.
pub fn end_child_sessions() {
    let me = std::process::id() as i32;
    let leaders: Vec<i32> = pids().into_iter().filter(|&p| ppid(p) == Some(me) && sid(p) == p).collect();
    if leaders.is_empty() {
        return;
    }
    let members = || -> Vec<i32> {
        pids().into_iter().filter(|&p| leaders.contains(&sid(p)) && running(p)).collect()
    };
    for (signal, grace) in [(libc::SIGHUP, 300), (libc::SIGTERM, 300), (libc::SIGKILL, 0)] {
        let alive = members();
        if alive.is_empty() {
            return;
        }
        for pid in alive {
            // SAFETY: kill only sends a signal.
            unsafe { libc::kill(pid, signal) };
        }
        let deadline = Instant::now() + Duration::from_millis(grace);
        while Instant::now() < deadline && !members().is_empty() {
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

static QUIT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

extern "C" fn on_quit_signal(_: libc::c_int) {
    QUIT.store(true, std::sync::atomic::Ordering::Relaxed);
}

/// SIGTERM, SIGHUP and SIGINT ask the app to quit instead of killing it,
/// so it saves and ends its terminals' sessions first.
pub fn catch_quit_signals() {
    for signal in [libc::SIGTERM, libc::SIGHUP, libc::SIGINT] {
        // SAFETY: the handler only stores to an atomic.
        unsafe { libc::signal(signal, on_quit_signal as *const () as libc::sighandler_t) };
    }
}

pub fn quit_requested() -> bool {
    QUIT.load(std::sync::atomic::Ordering::Relaxed)
}
