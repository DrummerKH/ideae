//! What a window on another machine asks of this one ([`proto::Call`]):
//! its git run here, its files read and written here.

use std::io::Write as _;
use std::path::Path;
use std::process::Stdio;

use super::proto::{self, Call, Reply};

/// A frame holds at most 64 MB: a file or an output larger is refused,
/// not cut. Error output is kept to its end, which says what went wrong.
const MOST: usize = 60 << 20;
const MOST_STDERR: usize = 1 << 20;

fn failed(error: impl ToString) -> (Reply, Vec<u8>) {
    (Reply { error: Some(error.to_string()), ..Default::default() }, Vec::new())
}

pub fn answer(call: Call, body: &[u8]) -> (Reply, Vec<u8>) {
    match call {
        Call::Exec { cwd, program, args, env } => {
            let mut command = std::process::Command::new(&program);
            command.current_dir(&cwd).args(&args).envs(env).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
            let mut child = match command.spawn() {
                Ok(child) => child,
                Err(e) => return failed(format!("{program}: {e}")),
            };
            if let Some(mut stdin) = child.stdin.take() {
                let body = body.to_vec();
                // Fed from its own thread: a program that writes before it
                // has read all would wait on us, and we on it.
                std::thread::spawn(move || {
                    let _ = stdin.write_all(&body);
                });
            }
            match child.wait_with_output() {
                Ok(out) if out.stdout.len() > MOST => failed(format!("{program} printed more than {} MB", MOST >> 20)),
                Ok(out) => {
                    let tail = &out.stderr[out.stderr.len().saturating_sub(MOST_STDERR)..];
                    (Reply { code: out.status.code(), stderr: String::from_utf8_lossy(tail).into_owned(), ..Default::default() }, out.stdout)
                }
                Err(e) => failed(format!("{program}: {e}")),
            }
        }
        Call::Read { path } => match std::fs::metadata(&path) {
            Ok(meta) if meta.len() as usize > MOST => failed(format!("{} is larger than {} MB", path.display(), MOST >> 20)),
            _ => match std::fs::read(&path) {
                Ok(bytes) => (Reply::default(), bytes),
                Err(e) => failed(e),
            },
        },
        Call::Write { path } => match write(&path, body) {
            Ok(()) => (Reply::default(), Vec::new()),
            Err(e) => failed(e),
        },
        // Answered by the server, which knows the sessions.
        Call::Listening { .. } => failed("not here"),
        Call::Stat { path } => match std::fs::symlink_metadata(&path) {
            Ok(meta) => (Reply { stat: Some(stat_of(&meta)), ..Default::default() }, Vec::new()),
            Err(e) => failed(e),
        },
    }
}

/// Into a file beside it, renamed into place: a reader never sees half of
/// it. It keeps the mode of the one it replaces.
fn write(path: &Path, body: &[u8]) -> std::io::Result<()> {
    // Through a link, as a plain write goes: the file it points at is written.
    let path = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let path = path.as_path();
    let name = path.file_name().ok_or_else(|| std::io::Error::other("no file name"))?;
    // Each write a part of its own: two saves at once never share one.
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let part = path.with_file_name(format!(".{}.ideae-{}-{n}", name.to_string_lossy(), std::process::id()));
    std::fs::write(&part, body)?;
    if let Ok(meta) = std::fs::metadata(path) {
        let _ = std::fs::set_permissions(&part, meta.permissions());
    }
    std::fs::rename(&part, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&part);
    })
}

pub fn stat_of(meta: &std::fs::Metadata) -> proto::Stat {
    use std::os::unix::fs::PermissionsExt as _;
    let modified = meta.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map_or(0, |d| d.as_nanos());
    proto::Stat { dir: meta.is_dir(), file: meta.is_file(), link: meta.file_type().is_symlink(), len: meta.len(), modified, mode: meta.permissions().mode() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_program_runs_and_files_are_read_written_and_looked_at() {
        let dir = std::env::temp_dir().join(format!("ideae-calls-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (reply, out) = answer(Call::Exec { cwd: dir.clone(), program: "sh".into(), args: vec!["-c".into(), "cat; echo err >&2; exit 3".into()], env: Vec::new() }, b"in");
        assert_eq!((reply.code, reply.stderr.as_str(), out.as_slice()), (Some(3), "err\n", &b"in"[..]));
        assert!(answer(Call::Exec { cwd: dir.clone(), program: "no-such-program-here".into(), args: Vec::new(), env: Vec::new() }, b"").0.error.is_some());
        let file = dir.join("a.txt");
        assert!(answer(Call::Write { path: file.clone() }, "é\n".as_bytes()).0.error.is_none());
        assert_eq!(answer(Call::Read { path: file.clone() }, b"").1, "é\n".as_bytes());
        let stat = answer(Call::Stat { path: file.clone() }, b"").0.stat.unwrap();
        assert!(stat.file && !stat.dir && stat.len == 3 && stat.modified > 0);
        assert!(answer(Call::Read { path: dir.join("missing") }, b"").0.error.is_some());
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1, "no part file left");
        // Through a link: the file it points at, the link kept.
        std::os::unix::fs::symlink(&file, dir.join("link")).unwrap();
        assert!(answer(Call::Write { path: dir.join("link") }, b"via\n").0.error.is_none());
        assert!(std::fs::symlink_metadata(dir.join("link")).unwrap().file_type().is_symlink());
        assert_eq!(std::fs::read(&file).unwrap(), b"via\n");
        // Saves at once: each whole, none lost to another's part file.
        let writes: Vec<_> = (0..8u8)
            .map(|i| {
                let file = file.clone();
                std::thread::spawn(move || answer(Call::Write { path: file }, &vec![b'a' + i; 100_000]).0.error)
            })
            .collect();
        assert!(writes.into_iter().all(|w| w.join().unwrap().is_none()));
        let after = std::fs::read(&file).unwrap();
        assert!(after.len() == 100_000 && after.iter().all(|b| *b == after[0]), "one write whole");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
