//! Starting programs and ending what the app started when it quits. Closing
//! a terminal ends its session on a background thread, which a quitting app
//! does not wait for.

use std::{
    ffi::OsStr,
    path::PathBuf,
    process::{Child, Command},
};
#[cfg(unix)]
use std::time::{Duration, Instant};

/// A command for a helper program (git, gh, curl…). On Windows it opens no
/// console window, and a bare name is looked up on PATH with its extension,
/// so `npm` finds `npm.cmd`, which `Command` alone does not.
pub fn command(program: impl AsRef<OsStr>) -> Command {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let program = program.as_ref();
        let mut command = Command::new(which(program).map_or_else(|| program.to_os_string(), PathBuf::into_os_string));
        command.creation_flags(CREATE_NO_WINDOW);
        command
    }
    #[cfg(not(windows))]
    Command::new(program)
}

/// Where `program` is on PATH; with an extension from PATHEXT on Windows.
pub fn which(program: impl AsRef<OsStr>) -> Option<PathBuf> {
    let program = std::path::Path::new(program.as_ref());
    if program.components().count() > 1 {
        return program.is_file().then(|| program.to_path_buf());
    }
    let path = std::env::var_os("PATH")?;
    #[cfg(windows)]
    let extensions: Vec<String> = {
        let pathext = std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".into());
        std::iter::once(String::new()).chain(pathext.split(';').filter(|e| !e.is_empty()).map(str::to_lowercase)).collect()
    };
    std::env::split_paths(&path).find_map(|dir| {
        #[cfg(windows)]
        {
            extensions.iter().find_map(|ext| {
                let mut name = program.as_os_str().to_os_string();
                name.push(ext);
                let candidate = dir.join(name);
                // A bare name with no extension is no Windows executable (npm's sh script).
                (!ext.is_empty() || program.extension().is_some()).then_some(())?;
                candidate.is_file().then_some(candidate)
            })
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let candidate = dir.join(program);
            std::fs::metadata(&candidate)
                .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
                .then_some(candidate)
        }
    })
}

/// Starts a program that must not outlive the app, even when the app is
/// killed: a language server, the headless browser.
pub fn spawn_tied(command: &mut Command) -> std::io::Result<Child> {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        // SAFETY: prctl only sets a flag of the child.
        unsafe {
            command.pre_exec(|| {
                libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
                Ok(())
            });
        }
        command.spawn()
    }
    #[cfg(windows)]
    {
        let child = command.spawn()?;
        win::tie(&child);
        Ok(child)
    }
}

/// Whether a process with this id runs.
pub fn alive(pid: u32) -> bool {
    #[cfg(unix)]
    return running(pid as i32);
    #[cfg(windows)]
    return win::alive(pid);
}

#[cfg(windows)]
mod win {
    use std::sync::OnceLock;

    use windows_sys::Win32::{
        Foundation::{CloseHandle, HANDLE, STILL_ACTIVE},
        System::{
            JobObjects::{
                AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
                JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation, SetInformationJobObject,
            },
            Threading::{GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION},
        },
    };

    struct Job(HANDLE);
    // SAFETY: a job handle may be used from any thread.
    unsafe impl Send for Job {}
    unsafe impl Sync for Job {}

    /// One job for the app's whole life; its handle closes only when the
    /// process ends, and then everything in it is killed.
    fn job() -> Option<&'static Job> {
        static JOB: OnceLock<Option<Job>> = OnceLock::new();
        JOB.get_or_init(|| {
            // SAFETY: null attributes and name create an anonymous job; the
            // limits are plain data of the size given.
            unsafe {
                let handle = CreateJobObjectW(std::ptr::null(), std::ptr::null());
                if handle.is_null() {
                    return None;
                }
                let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
                limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
                SetInformationJobObject(
                    handle,
                    JobObjectExtendedLimitInformation,
                    std::ptr::from_ref(&limits).cast(),
                    std::mem::size_of_val(&limits) as u32,
                );
                Some(Job(handle))
            }
        })
        .as_ref()
    }

    pub fn tie(child: &std::process::Child) {
        use std::os::windows::io::AsRawHandle as _;
        if let Some(job) = job() {
            // SAFETY: both handles are open.
            unsafe { AssignProcessToJobObject(job.0, child.as_raw_handle() as HANDLE) };
        }
    }

    pub fn alive(pid: u32) -> bool {
        // SAFETY: the handle is checked and closed here.
        unsafe {
            let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if handle.is_null() {
                return false;
            }
            let mut code = 0u32;
            let ok = GetExitCodeProcess(handle, &mut code) != 0;
            CloseHandle(handle);
            ok && code == STILL_ACTIVE as u32
        }
    }
}

#[cfg(unix)]
fn pids() -> Vec<i32> {
    std::fs::read_dir("/proc")
        .map(|dir| dir.flatten().filter_map(|e| e.file_name().to_str()?.parse().ok()).collect())
        .unwrap_or_default()
}

#[cfg(unix)]
fn ppid(pid: i32) -> Option<i32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let (_, rest) = stat.rsplit_once(')')?;
    rest.split_whitespace().nth(1)?.parse().ok()
}

#[cfg(unix)]
fn sid(pid: i32) -> i32 {
    // SAFETY: getsid only reads the process table.
    unsafe { libc::getsid(pid) }
}

#[cfg(unix)]
fn running(pid: i32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/stat"))
        .ok()
        .and_then(|s| s.rsplit_once(')').map(|(_, rest)| !rest.trim_start().starts_with('Z')))
        .unwrap_or(false)
}

/// Every session led by a child of this process (the terminals' shells):
/// the shell, the agent, and whatever they started, nohup included. On
/// Windows each terminal's tree is in a job that dies with the app.
#[cfg(not(unix))]
pub fn end_child_sessions() {}

#[cfg(unix)]
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

#[cfg_attr(not(unix), allow(dead_code))]
static QUIT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

#[cfg(unix)]
extern "C" fn on_quit_signal(_: libc::c_int) {
    QUIT.store(true, std::sync::atomic::Ordering::Relaxed);
}

/// SIGTERM, SIGHUP and SIGINT ask the app to quit instead of killing it,
/// so it saves and ends its terminals' sessions first.
pub fn catch_quit_signals() {
    #[cfg(unix)]
    for signal in [libc::SIGTERM, libc::SIGHUP, libc::SIGINT] {
        // SAFETY: the handler only stores to an atomic.
        unsafe { libc::signal(signal, on_quit_signal as *const () as libc::sighandler_t) };
    }
}

pub fn quit_requested() -> bool {
    QUIT.load(std::sync::atomic::Ordering::Relaxed)
}
