//! A task's files and programs wherever its machine is: a remote task's
//! ([`crate::hosts`]) through its host's daemon, the rest here. Blocking:
//! off the UI thread, as any file or process is.

use std::io;
use std::path::{Path, PathBuf};

/// What a program run to its end gave.
#[derive(Debug, Default)]
pub struct Out {
    /// None: ended by a signal.
    pub code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: String,
}

impl Out {
    pub fn success(&self) -> bool {
        self.code == Some(0)
    }
}

/// A file as it is, its links not followed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stat {
    pub dir: bool,
    pub file: bool,
    pub link: bool,
    pub len: u64,
    /// Nanoseconds since 1970.
    pub modified: u128,
    pub executable: bool,
}

#[cfg(unix)]
fn remote(path: &Path) -> Option<io::Result<(std::sync::Arc<crate::daemon::client::Daemon>, PathBuf)>> {
    let (_, there) = crate::hosts::split(path)?;
    Some(crate::daemon::client::for_path(path).map(|d| (d, there)).ok_or_else(|| io::Error::new(io::ErrorKind::NotConnected, "its host is not connected")))
}

#[cfg(not(unix))]
fn remote(path: &Path) -> Option<io::Result<((), PathBuf)>> {
    crate::hosts::is_remote(path).then(|| Err(io::Error::new(io::ErrorKind::Unsupported, "projects on other machines need Linux or macOS")))
}

#[cfg(unix)]
fn called(daemon: &crate::daemon::client::Daemon, call: crate::daemon::proto::Call, body: &[u8]) -> io::Result<(crate::daemon::proto::Reply, Vec<u8>)> {
    let (reply, out) = daemon.call(&call, body)?;
    match &reply.error {
        Some(error) if error.contains("No such file") || error.contains("not found") => Err(io::Error::new(io::ErrorKind::NotFound, error.clone())),
        Some(error) => Err(io::Error::other(error.clone())),
        None => Ok((reply, out)),
    }
}

/// Runs `program` in `cwd` to its end, `input` its stdin.
pub fn run(cwd: &Path, program: &str, args: &[&str], env: &[(&str, &str)], input: Option<&[u8]>) -> io::Result<Out> {
    if let Some(remote) = remote(cwd) {
        #[cfg(unix)]
        {
            let (daemon, there) = remote?;
            let call = crate::daemon::proto::Call::Exec {
                cwd: there,
                program: program.to_string(),
                args: args.iter().map(|a| a.to_string()).collect(),
                env: env.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
            };
            let (reply, stdout) = called(&daemon, call, input.unwrap_or_default())?;
            return Ok(Out { code: reply.code, stdout, stderr: reply.stderr });
        }
        #[cfg(not(unix))]
        return remote.map(|_| Out::default());
    }
    use std::io::Write as _;
    use std::process::Stdio;
    let mut command = crate::procs::command(program);
    command.current_dir(cwd).args(args).stdout(Stdio::piped()).stderr(Stdio::piped());
    for (k, v) in env {
        command.env(k, v);
    }
    command.stdin(if input.is_some() { Stdio::piped() } else { Stdio::null() });
    let mut child = command.spawn()?;
    if let (Some(input), Some(mut stdin)) = (input, child.stdin.take()) {
        let input = input.to_vec();
        std::thread::spawn(move || {
            let _ = stdin.write_all(&input);
        });
    }
    let out = child.wait_with_output()?;
    Ok(Out { code: out.status.code(), stdout: out.stdout, stderr: String::from_utf8_lossy(&out.stderr).into_owned() })
}

pub fn read(path: &Path) -> io::Result<Vec<u8>> {
    match remote(path) {
        #[cfg(unix)]
        Some(remote) => {
            let (daemon, there) = remote?;
            called(&daemon, crate::daemon::proto::Call::Read { path: there }, b"").map(|(_, bytes)| bytes)
        }
        #[cfg(not(unix))]
        Some(remote) => remote.map(|_| Vec::new()),
        None => std::fs::read(path),
    }
}

pub fn read_to_string(path: &Path) -> io::Result<String> {
    String::from_utf8(read(path)?).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

/// Writes `bytes` to `path`. There, into a file beside it renamed into
/// place, its mode kept; here as `std::fs::write`.
pub fn write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    match remote(path) {
        #[cfg(unix)]
        Some(remote) => {
            let (daemon, there) = remote?;
            called(&daemon, crate::daemon::proto::Call::Write { path: there }, bytes).map(|_| ())
        }
        #[cfg(not(unix))]
        Some(remote) => remote.map(|_| ()),
        None => std::fs::write(path, bytes),
    }
}

/// Removes the file at `path`; one not there is no error.
pub fn remove(path: &Path) -> io::Result<()> {
    if let Some((host, there)) = crate::hosts::split(path) {
        let out = run(&crate::hosts::join(&host, Path::new("/")), "rm", &["-f", &there.to_string_lossy()], &[], None)?;
        return if out.success() { Ok(()) } else { Err(io::Error::other(out.stderr)) };
    }
    match std::fs::remove_file(path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        done => done,
    }
}

pub fn stat(path: &Path) -> io::Result<Stat> {
    match remote(path) {
        #[cfg(unix)]
        Some(remote) => {
            let (daemon, there) = remote?;
            let (reply, _) = called(&daemon, crate::daemon::proto::Call::Stat { path: there }, b"")?;
            let s = reply.stat.ok_or_else(|| io::Error::other("no answer"))?;
            Ok(Stat { dir: s.dir, file: s.file, link: s.link, len: s.len, modified: s.modified, executable: s.mode & 0o111 != 0 })
        }
        #[cfg(not(unix))]
        Some(remote) => remote.map(|_| Stat { dir: false, file: false, link: false, len: 0, modified: 0, executable: false }),
        None => {
            let meta = std::fs::symlink_metadata(path)?;
            #[cfg(unix)]
            let executable = std::os::unix::fs::PermissionsExt::mode(&meta.permissions()) & 0o111 != 0;
            #[cfg(not(unix))]
            let executable = false;
            let modified = meta.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map_or(0, |d| d.as_nanos());
            Ok(Stat { dir: meta.is_dir(), file: meta.is_file(), link: meta.file_type().is_symlink(), len: meta.len(), modified, executable })
        }
    }
}

/// Whether `path`'s machine can be asked now: this one, or a host that is connected.
pub fn reachable(path: &Path) -> bool {
    match remote(path) {
        #[cfg(unix)]
        Some(remote) => remote.is_ok_and(|(daemon, _)| daemon.wire().is_none_or(|w| w.is_up())),
        #[cfg(not(unix))]
        Some(_) => false,
        None => true,
    }
}

/// An id no other run of the app gives: for sessions a daemon keeps
/// after this window has gone.
#[cfg_attr(not(unix), allow(dead_code))]
pub fn unique() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let at = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis());
    format!("{}-{at}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed))
}

/// The ports the task at `root` listens on: here, of its processes (by
/// its terminals' variables); a remote task's, asked of its daemon there.
pub fn listening(root: &Path, socket: Option<&str>) -> Vec<crate::services::Listening> {
    #[cfg(unix)]
    if let Some(remote) = remote(root) {
        let Ok((daemon, _)) = remote else { return Vec::new() };
        let asked = daemon.call(&crate::daemon::proto::Call::Listening { task: root.display().to_string() }, b"");
        let Ok((reply, body)) = asked else { return Vec::new() };
        if reply.error.is_some() {
            return Vec::new();
        }
        let found: Vec<serde_json::Value> = serde_json::from_slice(&body).unwrap_or_default();
        return found
            .iter()
            .filter_map(|l| Some(crate::services::Listening { port: l["port"].as_u64()? as u16, pid: l["pid"].as_i64()? as i32, name: l["name"].as_str()?.to_string() }))
            .collect();
    }
    socket.map(|socket| crate::services::listening(&crate::procs::task_processes(Some(&root.display().to_string()), socket))).unwrap_or_default()
}

/// `url` as it is reached from here: a remote task's address of its host's
/// own (`localhost:3000` there) at the port here its host's is forwarded
/// to. Others as they are. Off the UI thread: it runs ssh.
pub fn reach(root: &Path, url: &str) -> Result<String, String> {
    #[cfg(unix)]
    if crate::hosts::is_remote(root) {
        let Some((port, at, written)) = crate::browser::port_place(url) else { return Ok(url.to_string()) };
        let daemon = crate::daemon::client::for_path(root).ok_or("its host is not connected")?;
        let here = daemon.forward(port)?;
        let port = if written { here.to_string() } else { format!(":{here}") };
        return Ok(format!("{}{port}{}", url.get(..at.start).unwrap_or_default(), url.get(at.end..).unwrap_or_default()));
    }
    let _ = root;
    Ok(url.to_string())
}

/// Whether `path`'s host watches its folders for the window (its daemon said so).
pub fn watched_there(path: &Path) -> bool {
    #[cfg(unix)]
    return crate::daemon::client::for_path(path).is_some_and(|daemon| daemon.watches_folders());
    #[cfg(not(unix))]
    {
        let _ = path;
        false
    }
}

/// A path git or a program on `root`'s machine printed, as the window
/// keys it: on a remote task's host, its key.
pub fn rekey(root: &Path, printed: PathBuf) -> PathBuf {
    match crate::hosts::split(root) {
        Some((host, _)) => crate::hosts::join(&host, &printed),
        None => printed,
    }
}
