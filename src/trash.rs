//! Files put in the system's trash, and taken out again for Undo. On
//! Linux the freedesktop trash in the home folder (and `gio trash` for a
//! file on another disk), on macOS ~/.Trash. Windows has none here: the
//! caller asks and deletes.

use std::path::{Path, PathBuf};

/// Where a file went: moved back for Undo.
#[derive(Clone, Debug)]
pub struct Trashed {
    pub from: PathBuf,
    to: PathBuf,
    /// The freedesktop trash's note of where it came from.
    info: Option<PathBuf>,
}

/// Moves `path` to the trash. `Ok(None)`: trashed, with no way back from
/// here. `Err`: there is no trash it can go to, and it is where it was.
pub fn trash(path: &Path) -> Result<Option<Trashed>, String> {
    #[cfg(windows)]
    {
        let _ = path;
        Err("there is no trash to move it to".into())
    }
    #[cfg(target_os = "macos")]
    {
        let home = std::env::home_dir().ok_or("no home folder")?;
        let bin = home.join(".Trash");
        let name = path.file_name().ok_or("no file name")?.to_string_lossy().into_owned();
        for n in 1..1000 {
            let to = bin.join(if n == 1 { name.clone() } else { numbered(&name, n, " ") });
            if to.symlink_metadata().is_ok() {
                continue;
            }
            std::fs::rename(path, &to).map_err(|e| e.to_string())?;
            return Ok(Some(Trashed { from: path.to_path_buf(), to, info: None }));
        }
        Err("the trash has too many of that name".into())
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        match home_trash().ok_or_else(|| "no home folder".to_string()).and_then(|bin| into_freedesktop(&bin, path, now())) {
            Ok(trashed) => Ok(Some(trashed)),
            // Another disk than the home folder's: gio knows that disk's trash.
            Err(first) => match crate::procs::command("gio").arg("trash").arg("--").arg(path).stdin(std::process::Stdio::null()).output() {
                Ok(out) if out.status.success() => Ok(None),
                _ => Err(first),
            },
        }
    }
}

/// Puts a trashed file back where it was.
pub fn restore(trashed: &Trashed) -> Result<(), String> {
    if trashed.from.symlink_metadata().is_ok() {
        return Err(format!("{} is there again", trashed.from.display()));
    }
    std::fs::rename(&trashed.to, &trashed.from).map_err(|e| e.to_string())?;
    if let Some(info) = &trashed.info {
        let _ = std::fs::remove_file(info);
    }
    Ok(())
}

/// "a.txt" → "a 2.txt" (macOS) or "a.2.txt": the name a second one of it takes.
#[cfg_attr(not(unix), allow(dead_code))]
fn numbered(name: &str, n: usize, sep: &str) -> String {
    match name.rsplit_once('.').filter(|(stem, _)| !stem.is_empty()) {
        Some((stem, ext)) => format!("{stem}{sep}{n}.{ext}"),
        None => format!("{name}{sep}{n}"),
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
fn home_trash() -> Option<PathBuf> {
    let data = std::env::var_os("XDG_DATA_HOME").map(PathBuf::from).filter(|p| p.is_absolute()).or_else(|| std::env::home_dir().map(|home| home.join(".local/share")))?;
    Some(data.join("Trash"))
}

/// Seconds since 1970, as the clock says.
#[cfg(all(unix, not(target_os = "macos")))]
fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
}

/// Into a freedesktop trash folder: a note of where it was (`info/`),
/// written first to claim the name, then the file itself (`files/`).
#[cfg(all(unix, not(target_os = "macos")))]
fn into_freedesktop(bin: &Path, path: &Path, time: i64) -> Result<Trashed, String> {
    use std::io::Write as _;
    let (files, infos) = (bin.join("files"), bin.join("info"));
    std::fs::create_dir_all(&files).and_then(|_| std::fs::create_dir_all(&infos)).map_err(|e| e.to_string())?;
    let absolute = std::path::absolute(path).map_err(|e| e.to_string())?;
    let name = path.file_name().ok_or("no file name")?.to_string_lossy().into_owned();
    for n in 1..1000 {
        let taken = if n == 1 { name.clone() } else { numbered(&name, n, ".") };
        let info = infos.join(format!("{taken}.trashinfo"));
        let mut note = match std::fs::OpenOptions::new().write(true).create_new(true).open(&info) {
            Ok(note) => note,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e.to_string()),
        };
        let to = files.join(&taken);
        if to.symlink_metadata().is_ok() {
            let _ = std::fs::remove_file(&info);
            continue;
        }
        let written = write!(note, "[Trash Info]\nPath={}\nDeletionDate={}\n", escape(&absolute), date(time));
        if let Err(e) = written.map_err(|e| e.to_string()).and_then(|_| std::fs::rename(path, &to).map_err(|e| e.to_string())) {
            let _ = std::fs::remove_file(&info);
            return Err(e);
        }
        return Ok(Trashed { from: absolute, to, info: Some(info) });
    }
    Err("the trash has too many of that name".into())
}

/// A path as the trash spec writes it: URL-escaped, `/` kept.
#[cfg(all(unix, not(target_os = "macos")))]
fn escape(path: &Path) -> String {
    use std::os::unix::ffi::OsStrExt as _;
    path.as_os_str()
        .as_bytes()
        .iter()
        .map(|&b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'/' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// `2026-11-17T09:05:00` of seconds since 1970, in UTC: the spec asks for
/// local time, which no file manager holds against a trash entry.
#[cfg(all(unix, not(target_os = "macos")))]
fn date(secs: i64) -> String {
    let (days, rest) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // Days to a civil date (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}", rest / 3600, rest % 3600 / 60, rest % 60)
}

#[cfg(all(test, unix, not(target_os = "macos")))]
mod tests {
    use super::*;

    #[test]
    fn dates_and_names() {
        assert_eq!(date(0), "1970-01-01T00:00:00");
        assert_eq!(date(1_795_000_000), "2026-11-18T11:06:40");
        assert_eq!(date(951_782_400), "2000-02-29T00:00:00");
        assert_eq!(numbered("a.txt", 2, "."), "a.2.txt");
        assert_eq!(numbered(".env", 3, " "), ".env 3");
        assert_eq!(escape(Path::new("/tmp/a b%.rs")), "/tmp/a%20b%25.rs");
    }

    #[test]
    fn trashed_and_put_back() {
        let dir = std::env::temp_dir().join(format!("ideae-trash-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (bin, work) = (dir.join("Trash"), dir.join("work"));
        std::fs::create_dir_all(work.join("sub")).unwrap();
        std::fs::write(work.join("sub/a.txt"), "one").unwrap();
        std::fs::write(work.join("a.txt"), "two").unwrap();

        let first = into_freedesktop(&bin, &work.join("a.txt"), 0).unwrap();
        let second = into_freedesktop(&bin, &work.join("sub/a.txt"), 0).unwrap();
        let folder = into_freedesktop(&bin, &work.join("sub"), 0).unwrap();
        assert!(!work.join("a.txt").exists() && !work.join("sub").exists());
        assert_eq!(std::fs::read_to_string(bin.join("files/a.txt")).unwrap(), "two");
        assert_eq!(std::fs::read_to_string(bin.join("files/a.2.txt")).unwrap(), "one");
        let note = std::fs::read_to_string(bin.join("info/a.2.txt.trashinfo")).unwrap();
        assert!(note.starts_with("[Trash Info]\nPath=/") && note.contains("/work/sub/a.txt\n") && note.ends_with("DeletionDate=1970-01-01T00:00:00\n"), "{note}");

        restore(&folder).unwrap();
        restore(&second).unwrap();
        restore(&first).unwrap();
        assert_eq!(std::fs::read_to_string(work.join("sub/a.txt")).unwrap(), "one");
        assert_eq!(std::fs::read_to_string(work.join("a.txt")).unwrap(), "two");
        assert_eq!(std::fs::read_dir(bin.join("info")).unwrap().count(), 0, "the notes go with them");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
