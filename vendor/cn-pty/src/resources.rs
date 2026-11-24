//! Ghostty's `xterm-ghostty` terminfo and its shell integration scripts,
//! embedded (copied from the ghostty-vt crate, whose pinned Ghostty they
//! come from; see LICENSE-GHOSTTY) and written to a cache directory once,
//! where a program finds them through `TERMINFO` and
//! `GHOSTTY_RESOURCES_DIR`.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// The `TERM` value for Ghostty's terminfo entry.
pub const TERM: &str = "xterm-ghostty";
/// The Ghostty these come from, for `TERM_PROGRAM_VERSION`.
pub const GHOSTTY_VERSION: &str = "1.0.1";
const GHOSTTY_COMMIT: &str = "108cacb";

struct File {
    path: &'static str,
    bytes: &'static [u8],
}

const XTERM_GHOSTTY: &[u8] = include_bytes!("../resources/terminfo/78/xterm-ghostty");
const GHOSTTY: &[u8] = include_bytes!("../resources/terminfo/67/ghostty");

// ncurses looks the entry up under a single-letter directory on most
// systems and under a hex directory on macOS, so both are written.
const TERMINFO: &[File] = &[
    File { path: "ghostty.terminfo", bytes: include_bytes!("../resources/terminfo/ghostty.terminfo") },
    File { path: "x/xterm-ghostty", bytes: XTERM_GHOSTTY },
    File { path: "78/xterm-ghostty", bytes: XTERM_GHOSTTY },
    File { path: "g/ghostty", bytes: GHOSTTY },
    File { path: "67/ghostty", bytes: GHOSTTY },
];

macro_rules! script {
    ($path:literal) => {
        File {
            path: concat!("shell-integration/", $path),
            bytes: include_bytes!(concat!("../resources/shell-integration/", $path)),
        }
    };
}

const SCRIPTS: &[File] = &[
    script!("bash/bash-preexec.sh"),
    script!("bash/ghostty.bash"),
    script!("elvish/lib/ghostty-integration.elv"),
    script!("fish/vendor_conf.d/ghostty-shell-integration.fish"),
    script!("nushell/vendor/autoload/ghostty.nu"),
    script!("zsh/.zshenv"),
    script!("zsh/ghostty-integration"),
];

/// The directory holding the compiled `xterm-ghostty` terminfo database.
pub fn terminfo() -> io::Result<PathBuf> {
    install("terminfo", TERMINFO)
}

/// The directory to use as `GHOSTTY_RESOURCES_DIR`.
pub fn shell_integration() -> io::Result<PathBuf> {
    install("resources", SCRIPTS)
}

/// The cache root for this Ghostty pin: `GHOSTTY_VT_CACHE_DIR`, else the
/// platform cache directory.
fn root() -> io::Result<PathBuf> {
    let base = match std::env::var_os("GHOSTTY_VT_CACHE_DIR") {
        Some(dir) => PathBuf::from(dir),
        None => dirs::cache_dir().ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no cache directory"))?.join("cn-pty"),
    };
    Ok(base.join(format!("{GHOSTTY_VERSION}-{GHOSTTY_COMMIT}")))
}

fn install(subdir: &str, files: &[File]) -> io::Result<PathBuf> {
    install_in(&root()?, subdir, files)
}

/// Written to a sibling and renamed into place, so a concurrent process
/// sees either nothing or the whole set; a marker records completion.
fn install_in(root: &Path, subdir: &str, files: &[File]) -> io::Result<PathBuf> {
    let target = root.join(subdir);
    if target.join(".complete").exists() {
        return Ok(target);
    }
    fs::create_dir_all(root)?;
    static STAGING: AtomicU64 = AtomicU64::new(0);
    let staging = root.join(format!(".{subdir}-{}-{}", std::process::id(), STAGING.fetch_add(1, Ordering::Relaxed)));
    if staging.exists() {
        fs::remove_dir_all(&staging)?;
    }
    for file in files {
        let path = staging.join(file.path);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&path, file.bytes)?;
    }
    fs::write(staging.join(".complete"), b"")?;
    if let Err(error) = fs::rename(&staging, &target) {
        let _ = fs::remove_dir_all(&staging);
        if !target.join(".complete").exists() {
            return Err(error);
        }
    }
    Ok(target)
}

#[cfg(test)]
mod tests {
    #[test]
    fn written_once_and_whole() {
        let root = std::env::temp_dir().join(format!("cn-pty-res-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let dir = super::install_in(&root, "terminfo", super::TERMINFO).unwrap();
        assert!(dir.join("x/xterm-ghostty").is_file() && dir.join(".complete").is_file());
        assert_eq!(super::install_in(&root, "terminfo", super::TERMINFO).unwrap(), dir);
        let _ = std::fs::remove_dir_all(&root);
    }
}
