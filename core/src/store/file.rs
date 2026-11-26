//! The store on disk: `machine.json` beside the state, written whole into a
//! file renamed into place, the one before kept as `machine.json.bak`; and
//! `machine.lock`, held by the one process that keeps the store (a second
//! opens nothing of it to write).

use std::fs::File;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use super::{Machine, VERSION};

pub fn path(dir: &Path) -> PathBuf {
    dir.join("machine.json")
}

fn backup(dir: &Path) -> PathBuf {
    dir.join("machine.json.bak")
}

/// The store's lock, held while the returned file is: none when another
/// process holds it. Who holds it is written in it, for a look.
pub fn lock(dir: &Path) -> Result<Option<File>, String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let at = dir.join("machine.lock");
    let mut file = std::fs::OpenOptions::new().create(true).truncate(false).read(true).write(true).open(&at).map_err(|e| format!("{}: {e}", at.display()))?;
    match file.try_lock() {
        Ok(()) => {
            let _ = file.set_len(0);
            let _ = writeln!(file, "{} {}", std::process::id(), crate::paths::APP_ID);
            Ok(Some(file))
        }
        Err(std::fs::TryLockError::WouldBlock) => Ok(None),
        // A file system that cannot lock (some network homes): not a store to write.
        Err(std::fs::TryLockError::Error(e)) => Err(format!("{} cannot be locked: {e}", at.display())),
    }
}

/// The store in `dir`; none when there is none yet. One a crash left
/// between its renames, or left unreadable (written, not yet on disk, when
/// the power went), is the one before it; neither readable, they are kept
/// aside and there is none (state.json, written whole beside it, is what
/// it is made of again). One of a newer format is not read (and so not
/// written over).
pub fn load(dir: &Path) -> Result<Option<Machine>, String> {
    let read = |at: &Path| -> Result<Option<Machine>, String> {
        match std::fs::read(at) {
            Ok(bytes) => serde_json::from_slice::<Machine>(&bytes).map(Some).map_err(|e| format!("{}: {e}", at.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(format!("{}: {e}", at.display())),
        }
    };
    let machine = match read(&path(dir)) {
        Ok(Some(machine)) => Some(machine),
        first => match (first, read(&backup(dir))) {
            (_, Ok(Some(machine))) => Some(machine),
            (Ok(None), Ok(None)) => None,
            (first, second) => {
                eprintln!("ideae: the store unreadable ({}; {}): made again of state.json", first.err().unwrap_or_default(), second.err().unwrap_or_default());
                let at = crate::paths::now();
                let _ = std::fs::rename(path(dir), dir.join(format!("machine.json.unreadable-{at}")));
                let _ = std::fs::rename(backup(dir), dir.join(format!("machine.json.bak.unreadable-{at}")));
                None
            }
        },
    };
    match machine {
        Some(machine) if machine.version > VERSION => Err(format!("{} is of a newer ideae (format {}): open it with that one", path(dir).display(), machine.version)),
        found => Ok(found),
    }
}

/// Writes `machine`: to its file whole, the one before kept as the backup.
/// Not waited for to be on disk (as state.json is not): a save is often on
/// the UI thread, and what a power cut loses state.json gives back.
pub fn save(dir: &Path, machine: &Machine) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let json = serde_json::to_vec_pretty(machine).map_err(std::io::Error::other)?;
    let tmp = dir.join(format!("machine.json.tmp{}", std::process::id()));
    {
        let mut file = File::create(&tmp)?;
        file.write_all(&json)?;
    }
    let at = path(dir);
    if at.exists() {
        std::fs::rename(&at, backup(dir))?;
    }
    std::fs::rename(&tmp, &at)

}
