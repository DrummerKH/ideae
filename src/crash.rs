//! What a panic leaves behind. A panic ends the app at once (on macOS
//! whatever thread it is on), and started from a launcher nothing of it is
//! seen: what it said and where goes to files next to the state, and the
//! next start tells of it.

use std::{io::Write as _, path::PathBuf};

fn dir() -> PathBuf {
    crate::state::state_dir()
}

const LOG: &str = "crashes.log";
const LAST: &str = "last-crash.txt";

thread_local! {
    static CATCHING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Runs code of a library that may panic on input it does not expect (a
/// diagram's layout): its panic is an error of that input, not a crash, so
/// it is neither logged nor ends the app.
pub fn caught<R>(run: impl FnOnce() -> R) -> Result<R, String> {
    let before = CATCHING.replace(true);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(run));
    CATCHING.set(before);
    result.map_err(|payload| {
        payload.downcast_ref::<&str>().map(|s| s.to_string()).or_else(|| payload.downcast_ref::<String>().cloned()).unwrap_or_else(|| "it failed".into())
    })
}

/// `what` is the process: the window, or the daemon of the terminals.
pub fn install(what: &'static str) {
    let before = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if CATCHING.get() {
            return;
        }
        let said = info.payload().downcast_ref::<&str>().map(|s| s.to_string()).or_else(|| info.payload().downcast_ref::<String>().cloned()).unwrap_or_default();
        let at = info.location().map(|at| format!("{}:{}", at.file(), at.line())).unwrap_or_default();
        let thread = std::thread::current();
        let first = format!("{what} {}: {said} at {at}", crate::build_watch::version());
        let report = format!(
            "{first}\nunix time {}, thread {}\n{}\n\n",
            crate::state::now(),
            thread.name().unwrap_or("unnamed"),
            std::backtrace::Backtrace::force_capture()
        );
        let dir = dir();
        let _ = std::fs::create_dir_all(&dir);
        if let Ok(mut log) = std::fs::OpenOptions::new().create(true).append(true).open(dir.join(LOG)) {
            let _ = log.write_all(report.as_bytes());
        }
        let _ = std::fs::write(dir.join(LAST), &first);
        before(info);
    }));
}

/// The panic of the run before, told once: its first line, and where the
/// whole of it is. Not for the UI thread.
pub fn take_last() -> Option<(String, PathBuf)> {
    let last = dir().join(LAST);
    let first = std::fs::read_to_string(&last).ok()?;
    let _ = std::fs::remove_file(&last);
    Some((first.lines().next()?.to_string(), dir().join(LOG)))
}
