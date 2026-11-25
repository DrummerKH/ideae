//! Starting programs, and the process table, without a window.

use std::ffi::OsStr;
use std::path::PathBuf;
use std::process::{Child, Command};

/// Windows: what puts a child in the app's job, set by the app.
#[cfg(windows)]
pub static TIE: std::sync::OnceLock<fn(&Child)> = std::sync::OnceLock::new();

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
        // The signal comes when the thread that started the child ends, not
        // the app: started from a page's or a picture's thread, Chrome died
        // with it. A thread that lasts as long as the app starts them all.
        struct Job(*mut Command, std::sync::mpsc::Sender<std::io::Result<Child>>);
        // SAFETY: the command is used only while its owner waits for the reply.
        unsafe impl Send for Job {}
        static SPAWNER: std::sync::OnceLock<std::sync::Mutex<std::sync::mpsc::Sender<Job>>> = std::sync::OnceLock::new();
        let spawner = SPAWNER.get_or_init(|| {
            let (tx, rx) = std::sync::mpsc::channel::<Job>();
            std::thread::Builder::new()
                .name("spawner".into())
                .spawn(move || {
                    for Job(command, reply) in rx {
                        // SAFETY: see `Job`.
                        let _ = reply.send(unsafe { (*command).spawn() });
                    }
                })
                .expect("spawn the spawner thread");
            std::sync::Mutex::new(tx)
        });
        let (reply, child) = std::sync::mpsc::channel();
        spawner.lock().unwrap_or_else(|e| e.into_inner()).send(Job(command, reply)).map_err(|_| std::io::Error::other("the spawner thread ended"))?;
        child.recv().map_err(std::io::Error::other)?
    }
    // macOS has no parent-death signal: these end with their task or on
    // quit, not when the app is killed.
    #[cfg(all(unix, not(target_os = "linux")))]
    {
        command.spawn()
    }
    // In a job that ends with the app, given by the app (TIE).
    #[cfg(windows)]
    {
        let child = command.spawn()?;
        if let Some(tie) = TIE.get() {
            tie(&child);
        }
        Ok(child)
    }
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

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn name(_pid: u32) -> Option<String> {
    None
}

#[derive(Clone, Debug, PartialEq)]
pub struct Listening {
    pub port: u16,
    pub pid: i32,
    pub name: String,
}

/// The ports `pids` listen on, lowest first.
#[cfg(target_os = "linux")]
pub fn listening(pids: &[i32]) -> Vec<Listening> {
    // inode → port, from the listening sockets of the whole machine.
    let mut ports: std::collections::HashMap<String, u16> = std::collections::HashMap::new();
    for table in ["/proc/net/tcp", "/proc/net/tcp6"] {
        let Ok(text) = std::fs::read_to_string(table) else { continue };
        for line in text.lines().skip(1) {
            let f: Vec<&str> = line.split_whitespace().collect();
            if f.len() > 9 && f[3] == "0A" {
                if let Some(port) = f[1].rsplit(':').next().and_then(|h| u16::from_str_radix(h, 16).ok()) {
                    ports.insert(format!("socket:[{}]", f[9]), port);
                }
            }
        }
    }
    let mut out: Vec<Listening> = Vec::new();
    for &pid in pids {
        let Ok(fds) = std::fs::read_dir(format!("/proc/{pid}/fd")) else { continue };
        for fd in fds.flatten() {
            let Ok(link) = std::fs::read_link(fd.path()) else { continue };
            if let Some(&port) = ports.get(link.to_string_lossy().as_ref()) {
                if !out.iter().any(|l| l.port == port) {
                    let name = name(pid as u32).unwrap_or_default();
                    out.push(Listening { port, pid, name });
                }
            }
        }
    }
    out.sort_by_key(|l| l.port);
    out
}

/// `lsof` for the ports of `pids`.
#[cfg(target_os = "macos")]
pub fn listening(pids: &[i32]) -> Vec<Listening> {
    if pids.is_empty() {
        return Vec::new();
    }
    let list: Vec<String> = pids.iter().map(i32::to_string).collect();
    let Ok(out) = std::process::Command::new("lsof")
        .args(["-a", "-p", &list.join(","), "-iTCP", "-sTCP:LISTEN", "-P", "-n", "-Fpcn"])
        .output()
    else {
        return Vec::new();
    };
    let mut found: Vec<Listening> = Vec::new();
    let (mut pid, mut name) = (0, String::new());
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        match line.split_at_checked(1) {
            Some(("p", v)) => pid = v.parse().unwrap_or(0),
            Some(("c", v)) => name = v.to_string(),
            // "n*:3000" or "n127.0.0.1:3000".
            Some(("n", v)) => {
                if let Some(port) = v.rsplit(':').next().and_then(|p| p.parse().ok()) {
                    if !found.iter().any(|l| l.port == port) {
                        found.push(Listening { port, pid, name: name.clone() });
                    }
                }
            }
            _ => {}
        }
    }
    found.sort_by_key(|l| l.port);
    found
}

#[cfg(not(unix))]
pub fn listening(_: &[i32]) -> Vec<Listening> {
    Vec::new()
}


/// The processes of the sessions `leaders` lead (a terminal's shell, a
/// program on pipes, all they started there): theirs, not another session's.
#[cfg(target_os = "linux")]
pub fn in_sessions(leaders: &[i32]) -> Vec<i32> {
    let mut found = Vec::new();
    for entry in std::fs::read_dir("/proc").into_iter().flatten().flatten() {
        let Some(pid) = entry.file_name().to_str().and_then(|n| n.parse::<i32>().ok()) else { continue };
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else { continue };
        // After the name in parentheses: state, parent, group, session.
        let session = stat.rsplit_once(')').and_then(|(_, rest)| rest.split_whitespace().nth(3)?.parse::<i32>().ok());
        if session.is_some_and(|s| leaders.contains(&s)) {
            found.push(pid);
        }
    }
    found
}

#[cfg(target_os = "macos")]
pub fn in_sessions(leaders: &[i32]) -> Vec<i32> {
    let mut pids = vec![0i32; 8192];
    // SAFETY: proc_listallpids writes at most the buffer's size in bytes.
    let n = unsafe { libc::proc_listallpids(pids.as_mut_ptr().cast(), (pids.len() * std::mem::size_of::<i32>()) as i32) };
    pids.truncate(n.max(0) as usize);
    // SAFETY: getsid only reads the process table.
    pids.into_iter().filter(|&pid| pid > 0 && leaders.contains(&unsafe { libc::getsid(pid) })).collect()
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn in_sessions(_: &[i32]) -> Vec<i32> {
    Vec::new()
}

/// The program a process runs, by its path: unlike its name, it can be read
/// of a process of another user, root's `login` among them.
#[cfg(target_os = "macos")]
pub fn path(pid: u32) -> Option<std::path::PathBuf> {
    let mut buf = vec![0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    // SAFETY: proc_pidpath writes at most the buffer's size.
    let n = unsafe { libc::proc_pidpath(pid as libc::c_int, buf.as_mut_ptr().cast(), buf.len() as u32) };
    let n = usize::try_from(n).ok().filter(|&n| n > 0)?;
    buf.truncate(n);
    Some(std::path::PathBuf::from(<std::ffi::OsStr as std::os::unix::ffi::OsStrExt>::from_bytes(&buf)))
}

/// The name of the shell a terminal's session runs, `pid` being the
/// session's first process. On macOS that process is login(1), root's, whose
/// name cannot be read, and the shell is its child: a session handed over by
/// another daemon has nothing else to tell its shell by.
pub fn shell_of_session(pid: u32) -> Option<String> {
    #[cfg(target_os = "macos")]
    if path(pid).is_some_and(|path| path.file_name().is_some_and(|name| name == "login")) {
        let mut children = [0 as libc::pid_t; 8];
        // SAFETY: proc_listchildpids writes at most the buffer's size, and returns how many pids it wrote.
        let n = unsafe { libc::proc_listchildpids(pid as libc::pid_t, children.as_mut_ptr().cast(), std::mem::size_of_val(&children) as libc::c_int) };
        let count = usize::try_from(n).unwrap_or(0).min(children.len());
        return children.get(..count).and_then(|found| found.first()).and_then(|&child| name(child as u32));
    }
    name(pid)
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use std::os::unix::process::CommandExt as _;

    /// A session whose first process is a login (by its path): its shell
    /// is its child.
    #[test]
    fn a_session_under_login_is_told_its_shell() {
        let dir = std::env::temp_dir().join(format!("ideae-login-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // A stand-in for login(1): this test binary under that name, with sleep as its child.
        // Not a copy of /bin/bash: macOS kills a copy of one of its own programs as it starts.
        let login = dir.join("login");
        std::fs::copy(std::env::current_exe().unwrap(), &login).unwrap();
        // A group of its own: the sleep goes with it at the end.
        let mut child = std::process::Command::new(&login)
            .args(["--exact", "procs::tests::login_stand_in", "--ignored", "--nocapture", "--test-threads=1"])
            .env("IDEAE_TEST_LOGIN", "1")
            .stdout(std::process::Stdio::null())
            .process_group(0)
            .spawn()
            .unwrap();
        let mut told = None;
        for _ in 0..100 {
            told = super::shell_of_session(child.id());
            if told.as_deref() == Some("sleep") {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        // SAFETY: a signal to the group this test started.
        unsafe { libc::kill(-(child.id() as libc::pid_t), libc::SIGKILL) };
        let _ = child.wait();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(told.as_deref(), Some("sleep"));
        assert_eq!(super::shell_of_session(std::process::id()), super::name(std::process::id()), "no login: its own name");
    }

    /// The login of the test above: a process of that name whose child is the shell (sleep here).
    #[test]
    #[ignore]
    fn login_stand_in() {
        if std::env::var_os("IDEAE_TEST_LOGIN").is_some() {
            let _ = std::process::Command::new("sleep").arg("30").status();
        }
    }

    /// The real login is root's, whose name a user's process cannot read:
    /// its path can be. launchd, root's too, stands for it.
    #[test]
    fn a_root_process_is_told_by_its_path() {
        assert_eq!(super::path(1).as_deref(), Some(std::path::Path::new("/sbin/launchd")));
    }
}
