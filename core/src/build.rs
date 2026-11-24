//! Which build this is, for the daemon and the window to tell each other.

use std::sync::OnceLock;
use std::time::SystemTime;

static VERSION: OnceLock<String> = OnceLock::new();

/// The version the binary that links this tells, before [`id`] is first
/// asked: the app's own, with its commit.
pub fn set_version(version: String) {
    let _ = VERSION.set(version);
}

/// This crate's version and commit, for a binary that tells none.
pub fn version() -> String {
    VERSION.get().cloned().unwrap_or_else(|| {
        let base = option_env!("IDEAE_VERSION").unwrap_or(concat!(env!("CARGO_PKG_VERSION"), "-local"));
        match env!("IDEAE_COMMIT") {
            "" => base.to_string(),
            commit => format!("{base} ({commit})"),
        }
    })
}

/// This binary as it was when the process started: the version and the
/// file's time. A daemon and a window with different ids run different
/// builds, even of one commit.
pub fn id() -> String {
    static ID: OnceLock<String> = OnceLock::new();
    ID.get_or_init(|| {
        let built = crate::paths::exe()
            .ok()
            .and_then(|exe| std::fs::metadata(exe).ok()?.modified().ok())
            .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_secs());
        format!("{} {built}", version())
    })
    .clone()
}
