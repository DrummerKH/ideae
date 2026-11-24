//! Where things are: the app's name on the system, the folder of its
//! sockets, the running binary.

use std::path::PathBuf;

/// A debug build is another app (`ideae-dev`): its own sockets and state.
pub const APP_ID: &str = if cfg!(debug_assertions) { "ideae-dev" } else { "ideae" };

/// The app this process serves: [`APP_ID`], or what `IDEAE_APP` says. A
/// window of `ideae-dev` runs the same `ideae-server` as one of `ideae` on
/// a server, telling it which it serves (`ideae-remote`, `ideae-dev-remote`),
/// so that their daemons stay apart, and apart from the one an ideae of
/// that machine runs for its own window.
pub fn app() -> String {
    std::env::var("IDEAE_APP")
        .ok()
        .filter(|app| app.starts_with("ideae") && app.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'))
        .unwrap_or_else(|| APP_ID.to_string())
}

pub fn runtime_dir() -> PathBuf {
    let app = app();
    match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(dir) => PathBuf::from(dir).join(app),
        // /tmp is every user's (a server reached by ssh, with no login
        // session): a folder of one's own there. macOS's is per user already,
        // and its daemons' sockets stay where they were.
        #[cfg(target_os = "linux")]
        // SAFETY: getuid cannot fail.
        None => std::env::temp_dir().join(format!("{app}-{}", unsafe { libc::getuid() })),
        #[cfg(not(target_os = "linux"))]
        None => std::env::temp_dir().join(app),
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
