//! Scratch files: `.sql` files of no project, kept with the app's data and
//! readable by their owner only.
//!
//! When one was last opened is kept here, as many systems do not record a
//! file's access time. One left unopened for [`KEPT_DAYS`] goes to the trash,
//! where it can be got back; an empty one simply goes.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{Mutex, RwLock},
    time::Duration,
};

/// Days without being opened or changed before a scratch file is put away.
pub const KEPT_DAYS: u64 = 90;
const DAY: u64 = 24 * 60 * 60;
/// Past the start, when the tabs of the last run have said what they opened.
const SWEEP_AFTER: Duration = Duration::from_secs(60);
const INDEX: &str = "opened.json";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Scratch {
    pub path: PathBuf,
    pub name: String,
    /// When it was last opened or changed, whichever is later.
    pub used: u64,
    pub size: u64,
}

static LIST: RwLock<Vec<Scratch>> = RwLock::new(Vec::new());
/// One at a time: the index is read, changed and written back.
static CHANGING: Mutex<()> = Mutex::new(());
/// What the last sweep put in the trash, until someone is told.
static SWEPT: Mutex<Vec<String>> = Mutex::new(Vec::new());

pub fn dir() -> PathBuf {
    crate::state::state_dir().join("consoles")
}

pub fn is_scratch(path: &Path) -> bool {
    path.parent().is_some_and(|parent| parent == dir()) && path.extension().is_some_and(|e| e == "sql")
}

fn make_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// Files made before they were kept private become so.
fn tighten(dir: &Path) {
    #[cfg(unix)]
    if dir.is_dir() {
        use std::os::unix::fs::PermissionsExt as _;
        let _ = make_dir(dir);
        for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            let _ = std::fs::set_permissions(entry.path(), std::fs::Permissions::from_mode(0o600));
        }
    }
    #[cfg(not(unix))]
    let _ = dir;
}

fn read_index(dir: &Path) -> BTreeMap<String, u64> {
    std::fs::read(dir.join(INDEX)).ok().and_then(|bytes| serde_json::from_slice(&bytes).ok()).unwrap_or_default()
}

fn write_index(dir: &Path, index: &BTreeMap<String, u64>) {
    if let Ok(json) = serde_json::to_vec(index) {
        let _ = super::config::write_private(&dir.join(INDEX), &json);
    }
}

/// What is in the folder, the latest used first.
fn scan(dir: &Path) -> Vec<Scratch> {
    let index = read_index(dir);
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut out: Vec<Scratch> = entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            if path.extension().is_none_or(|e| e != "sql") {
                return None;
            }
            let meta = entry.metadata().ok().filter(std::fs::Metadata::is_file)?;
            let name = path.file_name()?.to_string_lossy().into_owned();
            let changed = meta.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map_or(0, |d| d.as_secs());
            Some(Scratch { used: changed.max(index.get(&name).copied().unwrap_or(0)), size: meta.len(), name, path })
        })
        .collect();
    out.sort_by(|a, b| b.used.cmp(&a.used).then_with(|| a.name.cmp(&b.name)));
    out
}

fn refresh() {
    *LIST.write().unwrap_or_else(|e| e.into_inner()) = scan(&dir());
}

/// As last read, for a menu: no disk.
pub fn list() -> Vec<Scratch> {
    LIST.read().unwrap_or_else(|e| e.into_inner()).clone()
}

/// Reads the list, and a minute later puts away what has been left too long.
pub fn start() {
    std::thread::spawn(|| {
        tighten(&dir());
        refresh();
        std::thread::sleep(SWEEP_AFTER);
        sweep();
    });
}

fn create_in(dir: &Path) -> std::io::Result<PathBuf> {
    make_dir(dir)?;
    for n in 1..100_000 {
        let path = dir.join(format!("scratch_{n}.sql"));
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        match options.open(&path) {
            Ok(_) => return Ok(path),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        }
    }
    Err(std::io::Error::other("too many scratch files"))
}

/// A new empty one. Blocks: not for the UI thread.
pub fn create() -> std::io::Result<PathBuf> {
    let _changing = CHANGING.lock().unwrap_or_else(|e| e.into_inner());
    let path = create_in(&dir())?;
    note_opened(&dir(), &path, crate::state::now());
    refresh();
    Ok(path)
}

fn note_opened(dir: &Path, path: &Path, now: u64) {
    let Some(name) = path.file_name().map(|n| n.to_string_lossy().into_owned()) else { return };
    let mut index = read_index(dir);
    index.insert(name, now);
    write_index(dir, &index);
}

/// It was opened in the editor: off the UI thread.
pub fn opened(path: PathBuf) {
    std::thread::spawn(move || {
        let _changing = CHANGING.lock().unwrap_or_else(|e| e.into_inner());
        note_opened(&dir(), &path, crate::state::now());
        refresh();
    });
}

fn forget_in(dir: &Path, path: &Path) {
    let Some(name) = path.file_name().map(|n| n.to_string_lossy().into_owned()) else { return };
    let mut index = read_index(dir);
    if index.remove(&name).is_some() {
        write_index(dir, &index);
    }
}

/// Its tab closed with nothing written in it: nothing to keep.
pub fn drop_if_empty(path: PathBuf) {
    std::thread::spawn(move || {
        let _changing = CHANGING.lock().unwrap_or_else(|e| e.into_inner());
        if std::fs::metadata(&path).is_ok_and(|m| m.is_file() && m.len() == 0) && std::fs::remove_file(&path).is_ok() {
            forget_in(&dir(), &path);
        }
        refresh();
    });
}

/// A name a scratch file can have: one file in its folder, ending in `.sql`.
pub fn clean_name(typed: &str) -> Result<String, String> {
    let name = typed.trim();
    let stem = name.strip_suffix(".sql").unwrap_or(name).trim();
    if stem.is_empty() {
        return Err("A name is needed".into());
    }
    if stem.starts_with('.') || stem.contains(['/', '\\', ':', '\0', '*', '?', '"', '<', '>', '|']) {
        return Err("Not a name for a file".into());
    }
    Ok(format!("{stem}.sql"))
}

fn rename_in(dir: &Path, from: &Path, typed: &str) -> Result<PathBuf, String> {
    let name = clean_name(typed)?;
    let to = dir.join(&name);
    if to == from {
        return Ok(to);
    }
    if to.symlink_metadata().is_ok() {
        return Err(format!("{name} is there already"));
    }
    std::fs::rename(from, &to).map_err(|e| e.to_string())?;
    let mut index = read_index(dir);
    if let Some(when) = from.file_name().and_then(|n| index.remove(n.to_string_lossy().as_ref())) {
        index.insert(name, when);
        write_index(dir, &index);
    }
    Ok(to)
}

/// Blocks: not for the UI thread.
pub fn rename(from: &Path, typed: &str) -> Result<PathBuf, String> {
    let _changing = CHANGING.lock().unwrap_or_else(|e| e.into_inner());
    let to = rename_in(&dir(), from, typed)?;
    refresh();
    Ok(to)
}

/// Into a project: written with the project's usual rights, and gone from here.
pub fn move_out(from: &Path, to: &Path) -> Result<(), String> {
    let _changing = CHANGING.lock().unwrap_or_else(|e| e.into_inner());
    let bytes = std::fs::read(from).map_err(|e| e.to_string())?;
    std::fs::write(to, bytes).map_err(|e| e.to_string())?;
    std::fs::remove_file(from).map_err(|e| e.to_string())?;
    forget_in(&dir(), from);
    refresh();
    Ok(())
}

/// To the trash, or where there is none, for good: it was asked for.
pub fn delete(path: &Path) -> Result<(), String> {
    let _changing = CHANGING.lock().unwrap_or_else(|e| e.into_inner());
    if crate::trash::trash(path).is_err() {
        std::fs::remove_file(path).map_err(|e| e.to_string())?;
    }
    forget_in(&dir(), path);
    refresh();
    Ok(())
}

/// What a sweep does with each file.
#[derive(Debug, PartialEq, Eq)]
enum Fate {
    Keep,
    Remove,
    Trash,
}

/// `in_use`: some task, running or paused, has it among its tabs.
fn fate(file: &Scratch, in_use: bool, now: u64) -> Fate {
    let age = now.saturating_sub(file.used);
    if in_use {
        Fate::Keep
    } else if file.size == 0 && age >= DAY {
        Fate::Remove
    } else if file.size > 0 && age >= KEPT_DAYS * DAY {
        Fate::Trash
    } else {
        Fate::Keep
    }
}

fn sweep() {
    let _changing = CHANGING.lock().unwrap_or_else(|e| e.into_inner());
    let dir = dir();
    // Named anywhere in the saved tabs: left alone, whichever task it is of.
    let Ok(state) = std::fs::read_to_string(crate::state::state_dir().join("state.json")) else { return };
    let now = crate::state::now();
    let mut swept = Vec::new();
    for file in scan(&dir) {
        match fate(&file, state.contains(&file.name), now) {
            Fate::Keep => {}
            Fate::Remove => {
                if std::fs::remove_file(&file.path).is_ok() {
                    forget_in(&dir, &file.path);
                }
            }
            // With no trash to go to, it stays: nothing written is deleted unasked.
            Fate::Trash => {
                if crate::trash::trash(&file.path).is_ok() {
                    forget_in(&dir, &file.path);
                    swept.push(file.name);
                }
            }
        }
    }
    refresh();
    if !swept.is_empty() {
        SWEPT.lock().unwrap_or_else(|e| e.into_inner()).extend(swept);
    }
}

/// What was put in the trash since last asked.
pub fn take_swept() -> Vec<String> {
    std::mem::take(&mut *SWEPT.lock().unwrap_or_else(|e| e.into_inner()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp() -> PathBuf {
        std::env::temp_dir().join(format!("ideae-scratch-{}", super::super::config::new_id()))
    }

    fn file(size: u64, used: u64) -> Scratch {
        Scratch { path: "x.sql".into(), name: "x.sql".into(), used, size }
    }

    #[test]
    fn only_what_was_left_long_enough_is_put_away() {
        let now = 1_000 * DAY;
        assert_eq!(fate(&file(10, now - 89 * DAY), false, now), Fate::Keep);
        assert_eq!(fate(&file(10, now - 90 * DAY), false, now), Fate::Trash);
        assert_eq!(fate(&file(10, now - 400 * DAY), true, now), Fate::Keep, "a task has it among its tabs");
        assert_eq!(fate(&file(0, now - 2 * DAY), false, now), Fate::Remove);
        assert_eq!(fate(&file(0, now - 60), false, now), Fate::Keep, "just made");
        assert_eq!(fate(&file(0, now - 2 * DAY), true, now), Fate::Keep);
    }

    #[test]
    fn a_new_file_is_its_owner_s_and_being_opened_counts_as_use() {
        let dir = temp();
        let path = create_in(&dir).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
            assert_eq!(std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777, 0o700);
        }
        let later = crate::state::now() + 10_000;
        note_opened(&dir, &path, later);
        assert_eq!(scan(&dir)[0].used, later, "no need of the system's access time");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_rename_keeps_its_date_and_never_overwrites() {
        let dir = temp();
        let (a, b) = (create_in(&dir).unwrap(), create_in(&dir).unwrap());
        let later = crate::state::now() + 10_000;
        note_opened(&dir, &a, later);
        assert!(rename_in(&dir, &a, b.file_name().unwrap().to_str().unwrap()).is_err());
        let to = rename_in(&dir, &a, "monthly report").unwrap();
        assert_eq!(to.file_name().unwrap(), "monthly report.sql");
        assert_eq!(read_index(&dir).get("monthly report.sql"), Some(&later));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_name_cannot_leave_the_folder() {
        assert_eq!(clean_name(" report.sql ").unwrap(), "report.sql");
        for bad in ["", ".sql", "../x", "a/b", "a\\b", ".hidden"] {
            assert!(clean_name(bad).is_err(), "{bad}");
        }
    }
}
