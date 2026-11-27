//! Where things are: the app's name on the system, the folder of its
//! sockets, the running binary.

use std::path::PathBuf;

/// A debug build is another app (`ideae-dev`): its own sockets and state.
pub const APP_ID: &str = if cfg!(debug_assertions) { "ideae-dev" } else { "ideae" };

/// The app this process serves: [`APP_ID`], or what `IDEAE_APP` says. A
/// window of `ideae-dev` runs the same `ideae-server` as one of `ideae` on
/// a server, telling it which it serves, so that their daemons stay apart;
/// each is the one an ideae of that machine runs for its own window.
pub fn app() -> String {
    std::env::var("IDEAE_APP")
        .ok()
        .filter(|app| app.starts_with("ideae") && app.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'))
        .unwrap_or_else(|| APP_ID.to_string())
}

/// The folder of this app's sockets: the same in a desktop session and in
/// one ssh starts, so that a window over ssh finds the daemon the one of
/// that machine's desktop runs (and is not started a second one beside it).
pub fn runtime_dir() -> PathBuf {
    let app = app();
    match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(dir) => PathBuf::from(dir).join(app),
        None => no_runtime_var(&app),
    }
}

/// An ssh session without a login session's folder: the user's own one,
/// where the desktop's daemon is; /tmp, every user's, a folder of one's own there.
#[cfg(target_os = "linux")]
fn no_runtime_var(app: &str) -> PathBuf {
    use std::os::unix::fs::MetadataExt as _;
    // SAFETY: getuid cannot fail.
    let uid = unsafe { libc::getuid() };
    let run = PathBuf::from(format!("/run/user/{uid}"));
    if std::fs::metadata(&run).is_ok_and(|m| m.is_dir() && m.uid() == uid) {
        return run.join(app);
    }
    std::env::temp_dir().join(format!("{app}-{uid}"))
}

/// The user's own temporary folder, as the system says: the desktop's
/// TMPDIR, which an ssh session may not have.
#[cfg(target_os = "macos")]
fn no_runtime_var(app: &str) -> PathBuf {
    let mut buf = vec![0 as libc::c_char; 1024];
    // SAFETY: confstr writes at most `len` bytes, nul-ended, into `buf`.
    let n = unsafe { libc::confstr(libc::_CS_DARWIN_USER_TEMP_DIR, buf.as_mut_ptr(), buf.len()) };
    if n > 0 && n <= buf.len() {
        // SAFETY: nul-ended by confstr.
        let dir = unsafe { std::ffi::CStr::from_ptr(buf.as_ptr()) }.to_string_lossy().into_owned();
        if !dir.is_empty() {
            return PathBuf::from(dir).join(app);
        }
    }
    std::env::temp_dir().join(app)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn no_runtime_var(app: &str) -> PathBuf {
    std::env::temp_dir().join(app)
}

/// Where apps keep their data: `~/.local/share`, `~/Library/Application
/// Support` on macOS, `%LOCALAPPDATA%` on Windows.
pub fn data_home() -> PathBuf {
    let var = |name| std::env::var_os(name).filter(|v| !v.is_empty()).map(PathBuf::from);
    if cfg!(windows) {
        return var("LOCALAPPDATA").unwrap_or_else(std::env::temp_dir);
    }
    if cfg!(target_os = "macos") {
        if let Some(home) = var("HOME") {
            return home.join("Library/Application Support");
        }
    }
    var("XDG_DATA_HOME").or_else(|| var("HOME").map(|h| h.join(".local/share"))).unwrap_or_else(std::env::temp_dir)
}

/// This app's state.json, of the app this process serves ([`app`]): as a
/// window here keeps it (IDEAE_STATE for a run of its own).
pub fn state_file() -> PathBuf {
    match std::env::var_os("IDEAE_STATE") {
        Some(path) => PathBuf::from(path),
        None => data_home().join(app()).join("state.json"),
    }
}

/// [`runtime_dir`], made if missing, refused unless it is this user's and
/// closed to others: its sockets take a terminal's input.
#[cfg(unix)]
pub fn own_runtime_dir() -> std::io::Result<PathBuf> {
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
    let dir = runtime_dir();
    let _ = std::fs::create_dir_all(&dir);
    let meta = std::fs::symlink_metadata(&dir)?;
    // SAFETY: getuid cannot fail.
    if !meta.is_dir() || meta.uid() != unsafe { libc::getuid() } {
        return Err(std::io::Error::other(format!("{} is not this user's", dir.display())));
    }
    if meta.mode() & 0o077 != 0 {
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(dir)
}

/// The running binary, also once a newer one has replaced its file.
pub fn exe() -> std::io::Result<PathBuf> {
    let exe = std::env::current_exe()?;
    let s = exe.to_string_lossy();
    Ok(match s.strip_suffix(" (deleted)") {
        Some(plain) => PathBuf::from(plain),
        None => exe,
    })
}

pub fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

#[cfg(test)]
mod tests {
    /// Without the session's variables (ssh), the same folder as with them.
    #[cfg(target_os = "macos")]
    #[test]
    fn the_sockets_folder_is_the_desktop_s_without_its_variables() {
        let Some(tmp) = std::env::var_os(crate::test_tmp::WAS).or_else(|| std::env::var_os("TMPDIR")) else { return };
        let with = std::path::PathBuf::from(tmp).join("x");
        let canon = |p: &std::path::Path| std::fs::canonicalize(p.parent().unwrap()).unwrap();
        assert_eq!(canon(&super::no_runtime_var("x")), canon(&with));
    }
}
