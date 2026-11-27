//! A test process's temporary files in a folder of its own, gone when it
//! ends: the tests make theirs in `temp_dir()`, and what a test that fails
//! or is killed leaves would stay there until the machine starts again.
//! Each crate's tests call [`own`] before main; a run that was killed has its
//! folder taken away by the next.

use std::path::{Path, PathBuf};

/// Set in what a test starts (the test binary run again as an agent, a
/// daemon): it keeps the folder of the run that started it.
const MARK: &str = "IDEAE_TEST_TMP";
const PREFIX: &str = "ideae-t";

static mut OWN: Option<PathBuf> = None;

/// Before main, while no other thread runs.
///
/// # Safety
/// Only from a constructor: it sets the process's environment.
pub unsafe fn own() {
    if std::env::var_os(MARK).is_some() {
        return;
    }
    let system = std::env::temp_dir();
    sweep(&system);
    let dir = system.join(format!("{PREFIX}{}", std::process::id()));
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    // SAFETY: before main, one thread (the caller's promise).
    unsafe {
        for var in ["TMPDIR", "TMP", "TEMP"] {
            std::env::set_var(var, &dir);
        }
        std::env::set_var(MARK, &dir);
        OWN = Some(dir);
        atexit(remove);
    }
}

unsafe extern "C" {
    fn atexit(f: extern "C" fn()) -> std::ffi::c_int;
}

extern "C" fn remove() {
    // SAFETY: written once before main, read here at exit.
    if let Some(dir) = unsafe { (*std::ptr::addr_of!(OWN)).as_ref() } {
        let _ = std::fs::remove_dir_all(dir);
    }
}

/// The folders of runs that ended without taking theirs away.
fn sweep(system: &Path) {
    let Ok(entries) = std::fs::read_dir(system) else { return };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(pid) = name.to_str().and_then(|n| n.strip_prefix(PREFIX)).and_then(|p| p.parse::<u32>().ok()) else { continue };
        if !alive(pid, &entry.path()) {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

#[cfg(unix)]
fn alive(pid: u32, _: &Path) -> bool {
    // SAFETY: signal 0 only asks whether the process is there.
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM) }
}

#[cfg(not(unix))]
fn alive(_: u32, dir: &Path) -> bool {
    // No cheap test of a process here: a day is longer than any run.
    dir.metadata().and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok()).is_none_or(|age| age.as_secs() < 86_400)
}
