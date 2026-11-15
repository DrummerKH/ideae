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
    #[cfg(target_os = "linux")]
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
    // macOS has no parent-death signal: these end with their task or on
    // quit, not when the app is killed.
    #[cfg(all(unix, not(target_os = "linux")))]
    {
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

#[cfg(target_os = "linux")]
fn pids() -> Vec<i32> {
    std::fs::read_dir("/proc")
        .map(|dir| dir.flatten().filter_map(|e| e.file_name().to_str()?.parse().ok()).collect())
        .unwrap_or_default()
}

#[cfg(target_os = "linux")]
fn ppid(pid: i32) -> Option<i32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let (_, rest) = stat.rsplit_once(')')?;
    rest.split_whitespace().nth(1)?.parse().ok()
}

#[cfg(target_os = "macos")]
fn pids() -> Vec<i32> {
    // SAFETY: a null buffer asks for the count; the second call fills one
    // of the size given.
    unsafe {
        let n = libc::proc_listallpids(std::ptr::null_mut(), 0).max(0) as usize;
        let mut pids = vec![0i32; n + 64];
        let size = (pids.len() * std::mem::size_of::<i32>()) as libc::c_int;
        let got = libc::proc_listallpids(pids.as_mut_ptr().cast(), size).max(0) as usize;
        pids.truncate(got);
        pids
    }
}

#[cfg(target_os = "macos")]
fn bsd_info(pid: i32) -> Option<libc::proc_bsdinfo> {
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    // SAFETY: proc_pidinfo writes at most `size` bytes into `info`.
    let got = unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDTBSDINFO, 0, (&raw mut info).cast(), size) };
    (got == size).then_some(info)
}

#[cfg(target_os = "macos")]
fn ppid(pid: i32) -> Option<i32> {
    bsd_info(pid).map(|i| i.pbi_ppid as i32)
}

/// A process's name, such as `bash`.
#[cfg(target_os = "linux")]
pub fn name(pid: u32) -> Option<String> {
    std::fs::read_to_string(format!("/proc/{pid}/comm")).ok().map(|n| n.trim().to_string())
}

#[cfg(target_os = "macos")]
pub fn name(pid: u32) -> Option<String> {
    let mut buf = [0u8; 256];
    // SAFETY: proc_name writes at most the buffer's size.
    let n = unsafe { libc::proc_name(pid as i32, buf.as_mut_ptr().cast(), buf.len() as u32) };
    (n > 0).then(|| String::from_utf8_lossy(&buf[..n as usize]).into_owned())
}

#[cfg(unix)]
fn sid(pid: i32) -> i32 {
    // SAFETY: getsid only reads the process table.
    unsafe { libc::getsid(pid) }
}

#[cfg(target_os = "linux")]
fn running(pid: i32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/stat"))
        .ok()
        .and_then(|s| s.rsplit_once(')').map(|(_, rest)| !rest.trim_start().starts_with('Z')))
        .unwrap_or(false)
}

/// Running and no zombie (status SZOMB is 5).
#[cfg(target_os = "macos")]
fn running(pid: i32) -> bool {
    bsd_info(pid).is_some_and(|i| i.pbi_status != 5)
}

/// Every session led by a child of this process (the terminals' shells):
/// the shell, the agent, and whatever they started, nohup included. On
/// Windows each terminal's tree is in a job that dies with the app.
#[cfg(not(unix))]
pub fn end_child_sessions() {}

/// A process running `ideae --daemon`.
#[cfg(target_os = "linux")]
fn is_daemon(pid: i32) -> bool {
    std::fs::read(format!("/proc/{pid}/cmdline")).is_ok_and(|c| c.split(|&b| b == 0).any(|arg| arg == b"--daemon"))
}

/// The window knows its daemon's pid from the socket.
#[cfg(target_os = "macos")]
fn is_daemon(pid: i32) -> bool {
    crate::daemon::client::get().and_then(|d| d.pid()) == Some(pid)
}

#[cfg(unix)]
pub fn end_child_sessions() {
    let me = std::process::id() as i32;
    // The daemon leads a session too, and its terminals outlive the window
    // by design; it ends them itself when told to.
    let daemon = |p: i32| {
        is_daemon(p)
    };
    let leaders: Vec<i32> =
        pids().into_iter().filter(|&p| ppid(p) == Some(me) && sid(p) == p && !daemon(p)).collect();
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

/// A process's environment, NUL-separated.
#[cfg(target_os = "linux")]
fn environ(pid: i32) -> Option<Vec<u8>> {
    std::fs::read(format!("/proc/{pid}/environ")).ok()
}

/// From KERN_PROCARGS2: argc, the executable's path, NUL padding, the
/// arguments, then the environment.
#[cfg(target_os = "macos")]
fn environ(pid: i32) -> Option<Vec<u8>> {
    let mut size: libc::size_t = 0;
    let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid];
    // SAFETY: a null buffer asks for the size; the second call fills one of it.
    unsafe {
        if libc::sysctl(mib.as_mut_ptr(), 3, std::ptr::null_mut(), &mut size, std::ptr::null_mut(), 0) != 0 {
            return None;
        }
    }
    let mut buf = vec![0u8; size];
    // SAFETY: as above, with the buffer `size` long.
    unsafe {
        if libc::sysctl(mib.as_mut_ptr(), 3, buf.as_mut_ptr().cast(), &mut size, std::ptr::null_mut(), 0) != 0 {
            return None;
        }
    }
    buf.truncate(size);
    let argc = i32::from_ne_bytes(buf.get(..4)?.try_into().ok()?) as usize;
    let mut at = 4 + buf[4..].iter().position(|&b| b == 0)?;
    while buf.get(at) == Some(&0) {
        at += 1;
    }
    for _ in 0..argc {
        at += buf.get(at..)?.iter().position(|&b| b == 0)? + 1;
    }
    let env = &buf[at.min(buf.len())..];
    let end = env.windows(2).position(|w| w == [0, 0]).map_or(env.len(), |i| i + 1);
    Some(env[..end].to_vec())
}

/// Every process started for a task: with this app's `socket` and, if
/// given, the task's root in its environment, wherever it went (a session
/// of its own included). Another ideae started from a task's terminal
/// inherits the same, so the trees of ideae processes are left out.
#[cfg(unix)]
pub fn task_processes(task: Option<&str>, socket: &str) -> Vec<i32> {
    let me = std::process::id() as i32;
    let daemon = crate::daemon::client::get().and_then(|d| d.pid()).map(|p| p as i32);
    let sock = format!("{}={socket}", crate::agent::SOCK_ENV);
    let task = task.map(|t| format!("{}={t}", crate::agent::TASK_ENV));
    let tagged = |pid: i32| {
        let Some(env) = environ(pid) else { return false };
        let vars: Vec<&[u8]> = env.split(|&b| b == 0).collect();
        vars.contains(&sock.as_bytes()) && task.as_ref().is_none_or(|t| vars.contains(&t.as_bytes()))
    };
    let ideae = |pid: i32| name(pid as u32).is_some_and(|n| n.starts_with("ideae"));
    let in_other_app = |pid: i32| {
        let mut at = Some(pid);
        for _ in 0..64 {
            match at {
                Some(p) if p == me || Some(p) == daemon || p <= 1 => return false,
                Some(p) if ideae(p) => return true,
                Some(p) => at = ppid(p),
                None => return false,
            }
        }
        false
    };
    pids().into_iter().filter(|&p| p != me && Some(p) != daemon && running(p) && tagged(p) && !in_other_app(p)).collect()
}

#[cfg(not(unix))]
pub fn task_processes(_: Option<&str>, _: &str) -> Vec<i32> {
    Vec::new()
}

/// Ends what a task started that left its terminals' sessions, such as a
/// shell an agent runs in the background with a session of its own.
#[cfg(unix)]
pub fn end_task_strays(task: Option<&str>, socket: &str) {
    for (signal, grace) in [(libc::SIGTERM, 500), (libc::SIGKILL, 0)] {
        let alive = task_processes(task, socket);
        if alive.is_empty() {
            return;
        }
        for pid in alive {
            // SAFETY: kill only sends a signal.
            unsafe { libc::kill(pid, signal) };
        }
        let deadline = Instant::now() + Duration::from_millis(grace);
        while Instant::now() < deadline && !task_processes(task, socket).is_empty() {
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

/// On Windows a terminal's tree is in a job that dies with it.
#[cfg(not(unix))]
pub fn end_task_strays(_: Option<&str>, _: &str) {}

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

#[cfg(all(test, unix))]
mod stray_tests {
    use std::os::unix::process::CommandExt as _;
    use std::process::{Child, Command};

    fn tagged_sleep(task: &str, socket: &str) -> Child {
        let mut command = Command::new("sleep");
        command.arg("30").env(crate::agent::TASK_ENV, task).env(crate::agent::SOCK_ENV, socket);
        // SAFETY: setsid in the child before exec, as a background shell does.
        unsafe { command.pre_exec(|| { libc::setsid(); Ok(()) }) };
        command.spawn().unwrap()
    }

    #[test]
    fn ends_the_tasks_strays_and_only_them() {
        let socket = format!("/tmp/ideae-test-{}.sock", std::process::id());
        let (mut ours, mut other_task, mut other_app) =
            (tagged_sleep("/t/a", &socket), tagged_sleep("/t/b", &socket), tagged_sleep("/t/a", "elsewhere"));
        super::end_task_strays(Some("/t/a"), &socket);
        assert!(ours.try_wait().unwrap().is_some(), "the task's stray is ended");
        assert!(other_task.try_wait().unwrap().is_none(), "another task's process lives on");
        assert!(other_app.try_wait().unwrap().is_none(), "another app's process lives on");
        let _ = other_task.kill();
        let _ = other_app.kill();
        let _ = (other_task.wait(), other_app.wait());
    }
}
