//! Notices a rebuilt binary: the file the app was started from changes on
//! disk, and the title bar offers a restart into it.

use std::{path::PathBuf, time::SystemTime};

pub fn version() -> String {
    let base = option_env!("IDEAE_VERSION").unwrap_or(concat!(env!("CARGO_PKG_VERSION"), "-local"));
    match env!("IDEAE_COMMIT") {
        "" => base.to_string(),
        commit => format!("{base} ({commit})"),
    }
}

/// This binary as it was when the process started: the version and the
/// file's time. A daemon and a window with different ids run different
/// builds, even of one commit.
pub fn build_id() -> String {
    static ID: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    ID.get_or_init(|| {
        let built = crate::desktop::exe()
            .ok()
            .and_then(|exe| std::fs::metadata(exe).ok()?.modified().ok())
            .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_secs());
        format!("{} {built}", version())
    })
    .clone()
}

#[derive(Clone, PartialEq)]
struct Stamp {
    modified: SystemTime,
    inode: u64,
}

fn stamp(path: &PathBuf) -> Option<Stamp> {
    let meta = std::fs::metadata(path).ok()?;
    #[cfg(unix)]
    let inode = std::os::unix::fs::MetadataExt::ino(&meta);
    #[cfg(not(unix))]
    let inode = meta.len();
    Some(Stamp { modified: meta.modified().ok()?, inode })
}

pub struct NewBuild {
    pub version: String,
    pub built: SystemTime,
}

pub struct BuildWatch {
    exe: Option<PathBuf>,
    running: Option<Stamp>,
    /// What the last look found, to wait until the linker is done writing.
    last: Option<Stamp>,
    probed: Option<Stamp>,
    pub new: Option<NewBuild>,
}

impl BuildWatch {
    pub fn new() -> Self {
        let exe = crate::desktop::exe().ok();
        let running = exe.as_ref().and_then(stamp);
        Self { exe, last: running.clone(), running, probed: None, new: None }
    }

    pub fn running_built(&self) -> Option<SystemTime> {
        self.running.as_ref().map(|s| s.modified)
    }

    /// A file that changed and then stayed the same for one look: the
    /// program to ask for its version, off the UI thread.
    pub fn check(&mut self) -> Option<PathBuf> {
        let exe = self.exe.clone()?;
        let now = stamp(&exe);
        let settled = now.is_some() && now == self.last;
        self.last = now.clone();
        if !settled || now == self.running || now == self.probed {
            if now == self.running {
                self.new = None;
            }
            return None;
        }
        self.probed = now;
        Some(exe)
    }

    pub fn found(&mut self, version: Option<String>) {
        let Some(stamp) = self.probed.clone() else { return };
        // A binary that does not answer --version is still being written.
        match version {
            Some(version) => self.new = Some(NewBuild { version, built: stamp.modified }),
            None => self.probed = None,
        }
    }
}

/// `path --version`, run off the UI thread.
pub fn probe(path: PathBuf) -> Option<String> {
    let out = crate::procs::command(&path).arg("--version").output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (out.status.success() && !text.is_empty()).then(|| text.trim_start_matches("ideae ").to_string())
}

/// "12:24", today's builds; the date for older ones.
pub fn clock(time: SystemTime) -> String {
    let secs = time.duration_since(SystemTime::UNIX_EPOCH).map_or(0, |d| d.as_secs()) as i64;
    let (day, month, hour, minute) = local(secs);
    let age = SystemTime::now().duration_since(time).map_or(0, |d| d.as_secs());
    if age < 20 * 3600 {
        format!("{hour:02}:{minute:02}")
    } else {
        format!("{day}.{month:02} {hour:02}:{minute:02}")
    }
}

#[cfg(unix)]
fn local(secs: i64) -> (i32, i32, i32, i32) {
    let t = secs as libc::time_t;
    // SAFETY: localtime_r only writes into the struct it is given.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe { libc::localtime_r(&t, &mut tm) };
    (tm.tm_mday, tm.tm_mon + 1, tm.tm_hour, tm.tm_min)
}

/// UTC; good enough for telling builds apart.
#[cfg(not(unix))]
fn local(secs: i64) -> (i32, i32, i32, i32) {
    let days = secs.div_euclid(86400);
    let rest = secs.rem_euclid(86400);
    // Civil date from days since 1970 (Howard Hinnant's algorithm).
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    (day as i32, month as i32, (rest / 3600) as i32, (rest % 3600 / 60) as i32)
}
