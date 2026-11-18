//! The files of a task as they were before a prompt: a git tree of the
//! worktree (untracked files too, ignored ones not), made with an index of
//! its own so the task's staging is not touched, and kept from git's
//! cleanup by a ref under `refs/ideae/checkpoints/<thread>/`. Putting the
//! files back writes the worktree only: the index and HEAD stay.

use std::path::Path;
use std::process::Stdio;

const REFS: &str = "refs/ideae/checkpoints";
/// An untracked file larger is left out of checkpoints (and so never
/// removed by putting files back).
const UNTRACKED_MOST: u64 = 16 << 20;

fn git(cwd: &Path) -> std::process::Command {
    let mut c = crate::procs::command("git");
    // A file goes back byte for byte as it was kept: not with the line ends
    // git would give it where it is set to change them (Windows' default).
    c.args(["-c", "core.autocrlf=false", "-c", "core.eol=lf"]).current_dir(cwd).stdin(Stdio::null()).stderr(Stdio::null());
    c
}

fn output(cmd: &mut std::process::Command) -> Option<String> {
    let out = cmd.output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// The worktree now, as a tree: its id, None when it is not a repository.
pub fn take(cwd: &Path, thread: &str) -> Option<String> {
    let index = output(git(cwd).args(["rev-parse", "--path-format=absolute", "--git-path", "index"]))?;
    let own = format!("{index}.ideae-checkpoint");
    // A copy of the real index: only what changed since is hashed again.
    // Its time kept too: git trusts a file's cached stat only when the file
    // is older than the index, and a copy made now would vouch for changes
    // of this very second (same size) as unchanged.
    let copied = std::fs::copy(&index, &own).and_then(|_| {
        let modified = std::fs::metadata(&index)?.modified()?;
        std::fs::OpenOptions::new().write(true).open(&own)?.set_modified(modified)
    });
    if copied.is_err() {
        let _ = std::fs::remove_file(&own);
    }
    let made = (|| {
        // What git has: changed and deleted files.
        let updated = git(cwd).env("GIT_INDEX_FILE", &own).args(["add", "-u"]).stdout(Stdio::null()).status().ok()?;
        if !updated.success() {
            return None;
        }
        // Untracked files, not ignored and not huge: a dataset or a build
        // left unignored is not copied into the repository at every prompt.
        let others = git(cwd).args(["ls-files", "-o", "--exclude-standard", "-z"]).output().ok()?;
        let small: Vec<u8> = others
            .stdout
            .split(|&b| b == 0)
            .filter(|p| !p.is_empty())
            .filter(|p| std::fs::metadata(cwd.join(String::from_utf8_lossy(p).as_ref())).is_ok_and(|m| m.len() <= UNTRACKED_MOST))
            .flat_map(|p| p.iter().copied().chain([0]))
            .collect();
        if !small.is_empty() {
            let mut add = git(cwd).env("GIT_INDEX_FILE", &own).args(["add", "--pathspec-from-file=-", "--pathspec-file-nul"]).stdin(Stdio::piped()).stdout(Stdio::null()).spawn().ok()?;
            if let Some(mut stdin) = add.stdin.take() {
                use std::io::Write as _;
                let _ = stdin.write_all(&small);
            }
            add.wait().ok()?;
        }
        output(git(cwd).env("GIT_INDEX_FILE", &own).arg("write-tree"))
    })();
    let _ = std::fs::remove_file(&own);
    let tree = made.filter(|t| !t.is_empty())?;
    let _ = git(cwd).args(["update-ref", &format!("{REFS}/{thread}/{tree}"), &tree]).status();
    Some(tree)
}

/// What differs from `from` to `to` (trees): 'A' added since, 'M'
/// changed, 'D' deleted since, with the path.
pub fn changes(cwd: &Path, from: &str, to: &str) -> Vec<(char, String)> {
    let Some(out) = git(cwd).args(["diff-tree", "-r", "--no-renames", "--name-status", "-z", from, to]).output().ok().filter(|o| o.status.success()) else {
        return Vec::new();
    };
    let words: Vec<&[u8]> = out.stdout.split(|&b| b == 0).filter(|w| !w.is_empty()).collect();
    words
        .chunks(2)
        .filter_map(|pair| match pair {
            [status, path] => Some((status.first().map_or('M', |&c| c as char), String::from_utf8_lossy(path).into_owned())),
            _ => None,
        })
        .collect()
}

/// The worktree's files back as in `tree`, given what changed since.
pub fn restore(cwd: &Path, tree: &str, changed: &[(char, String)]) -> Result<(), String> {
    let back: Vec<&str> = changed.iter().filter(|(s, _)| *s != 'A').map(|(_, p)| p.as_str()).collect();
    for paths in back.chunks(200) {
        let done = git(cwd)
            .args(["restore", &format!("--source={tree}"), "--worktree", "--"])
            .args(paths)
            .stderr(Stdio::piped())
            .output()
            .map_err(|e| e.to_string())?;
        if !done.status.success() {
            return Err(String::from_utf8_lossy(&done.stderr).trim().to_string());
        }
    }
    // Made since, unless ignored as the files are now (a .gitignore put
    // back may hide again what was there all along, untracked).
    let made: Vec<&str> = changed.iter().filter(|(s, _)| *s == 'A').map(|(_, p)| p.as_str()).collect();
    let ignored = ignored(cwd, &made);
    for path in made.into_iter().filter(|p| !ignored.contains(*p)) {
        let path = cwd.join(path);
        // A folder of it made a link out of the worktree since: not followed.
        if !crate::acp::policy::inside(cwd, path.parent().unwrap_or(cwd)) && path.parent() != Some(cwd) {
            continue;
        }
        let _ = std::fs::remove_file(&path);
        // Folders it left empty go too, up to the worktree.
        let mut dir = path.parent();
        while let Some(d) = dir.filter(|d| d.starts_with(cwd) && *d != cwd) {
            if std::fs::remove_dir(d).is_err() {
                break;
            }
            dir = d.parent();
        }
    }
    Ok(())
}

fn ignored(cwd: &Path, paths: &[&str]) -> std::collections::HashSet<String> {
    if paths.is_empty() {
        return Default::default();
    }
    let Ok(mut child) = git(cwd).args(["check-ignore", "--no-index", "--stdin", "-z"]).stdin(Stdio::piped()).stdout(Stdio::piped()).spawn() else { return Default::default() };
    if let Some(mut stdin) = child.stdin.take() {
        use std::io::Write as _;
        let input: Vec<u8> = paths.iter().flat_map(|p| p.bytes().chain([0])).collect();
        let _ = stdin.write_all(&input);
    }
    let Ok(out) = child.wait_with_output() else { return Default::default() };
    out.stdout.split(|&b| b == 0).filter(|w| !w.is_empty()).map(|w| String::from_utf8_lossy(w).into_owned()).collect()
}

/// The refs that keep a thread's checkpoints: dropped with its worktree.
pub fn forget(repo: &Path, thread: &str) {
    let Some(refs) = output(git(repo).args(["for-each-ref", "--format=%(refname)", &format!("{REFS}/{thread}/")])) else { return };
    let input: String = refs.lines().filter(|l| !l.is_empty()).map(|r| format!("delete {r}\n")).collect();
    if input.is_empty() {
        return;
    }
    if let Ok(mut child) = git(repo).args(["update-ref", "--stdin"]).stdin(Stdio::piped()).spawn() {
        use std::io::Write as _;
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(input.as_bytes());
        }
        let _ = child.wait();
    }
}

/// One checkpoint's ref, when the thread keeps too many.
pub fn drop_one(cwd: &Path, thread: &str, tree: &str) {
    let _ = git(cwd).args(["update-ref", "-d", &format!("{REFS}/{thread}/{tree}")]).status();
}

/// The name a thread's checkpoints go by: its journal's.
pub fn thread_name(journal: Option<&Path>) -> String {
    journal.and_then(|j| j.file_stem()).map_or_else(|| "chat".to_string(), |s| s.to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn files_go_back_as_they_were_and_the_index_stays() {
        let dir = std::env::temp_dir().join(format!("ideae-checkpoint-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let run = |args: &[&str]| assert!(std::process::Command::new("git").current_dir(&dir).args(args).stdout(Stdio::null()).status().unwrap().success(), "{args:?}");
        run(&["init", "-q"]);
        run(&["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-q", "--allow-empty", "-m", "init"]);
        std::fs::write(dir.join("a.txt"), "one\n").unwrap();
        std::fs::write(dir.join(".gitignore"), "target/\n").unwrap();
        std::fs::create_dir_all(dir.join("target")).unwrap();
        std::fs::write(dir.join("target/big"), "ignored\n").unwrap();
        run(&["add", "a.txt"]);
        let before = take(&dir, "t1").expect("a checkpoint");

        // The agent's turn: a change, a new file in a new folder, a file gone.
        std::fs::write(dir.join("a.txt"), "two\n").unwrap();
        std::fs::create_dir_all(dir.join("src/deep")).unwrap();
        std::fs::write(dir.join("src/deep/new.rs"), "fn x() {}\n").unwrap();
        std::fs::remove_file(dir.join(".gitignore")).unwrap();
        // Untracked and huge: neither kept nor removed.
        std::fs::write(dir.join("dump.bin"), vec![0u8; UNTRACKED_MOST as usize + 1]).unwrap();
        let now = take(&dir, "t1").unwrap();
        let changed = changes(&dir, &before, &now);
        // Without its .gitignore, target/ shows: not removed once it is back.
        assert_eq!(changed.len(), 4, "{changed:?}");

        restore(&dir, &before, &changed).unwrap();
        assert_eq!(std::fs::read_to_string(dir.join("a.txt")).unwrap(), "one\n");
        assert!(dir.join(".gitignore").exists());
        assert!(!dir.join("src").exists(), "the new file and its folders gone");
        assert_eq!(std::fs::read_to_string(dir.join("target/big")).unwrap(), "ignored\n");
        assert!(dir.join("dump.bin").exists(), "a huge untracked file is left alone");
        let staged = std::process::Command::new("git").current_dir(&dir).args(["diff", "--cached", "--name-only"]).output().unwrap();
        assert_eq!(String::from_utf8_lossy(&staged.stdout).trim(), "a.txt", "the index as it was");

        // And back again: the undo.
        let restored = take(&dir, "t1").unwrap();
        restore(&dir, &now, &changes(&dir, &now, &restored)).unwrap();
        assert_eq!(std::fs::read_to_string(dir.join("a.txt")).unwrap(), "two\n");
        assert!(dir.join("src/deep/new.rs").exists());

        forget(&dir, "t1");
        let refs = std::process::Command::new("git").current_dir(&dir).args(["for-each-ref", "refs/ideae"]).output().unwrap();
        assert!(refs.stdout.is_empty(), "no refs left");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
