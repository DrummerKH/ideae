use std::{
    path::{Path, PathBuf},
};
use std::sync::Arc;
#[cfg(test)]
use std::process::Command;

pub struct Worktree {
    pub path: PathBuf,
    pub branch: String,
    /// `git worktree lock`, with its reason when given.
    pub locked: Option<String>,
}

impl Worktree {
    /// Locked by a process that still runs, such as a Claude Code subagent
    /// working in it ("claude agent … (pid N start T)"): someone else's
    /// working copy for now.
    pub fn in_use(&self) -> bool {
        self.locked.as_deref().and_then(lock_owner).is_some_and(|(pid, start)| process_alive(pid, start))
    }
}

/// The pid and start time named in a lock reason.
fn lock_owner(reason: &str) -> Option<(u32, Option<u64>)> {
    let rest = &reason[reason.find("pid ")? + 4..];
    let pid = rest.split(|c: char| !c.is_ascii_digit()).next()?.parse().ok()?;
    let start = rest
        .find("start ")
        .and_then(|i| rest[i + 6..].split(|c: char| !c.is_ascii_digit()).next()?.parse().ok());
    Some((pid, start))
}

/// The process exists and, when its start time is known, is the same one
/// and not a later process given the same pid.
fn process_alive(pid: u32, start: Option<u64>) -> bool {
    if cfg!(not(target_os = "linux")) {
        return crate::procs::alive(pid);
    }
    let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else { return false };
    let Some(start) = start else { return true };
    // Field 22, counted after the parenthesized command name.
    let after = stat.rsplit_once(')').map_or("", |(_, rest)| rest);
    after.split_whitespace().nth(19).and_then(|s| s.parse::<u64>().ok()) == Some(start)
}

fn git(root: &Path, args: &[&str]) -> Option<String> {
    // On another machine: run there, the same way.
    if crate::hosts::is_remote(root) {
        let mut all = vec!["-c", "core.quotePath=false"];
        all.extend_from_slice(args);
        let out = crate::hostfs::run(root, "git", &all, &[("GIT_OPTIONAL_LOCKS", "0")], None).ok()?;
        if !out.success() && out.code != Some(1) {
            return None;
        }
        return Some(String::from_utf8(out.stdout).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned()));
    }
    // No optional locks: status and diff must not rewrite the index, or the
    // file watcher would see our own refresh and refresh again forever.
    let output = crate::procs::command("git")
        .env("GIT_OPTIONAL_LOCKS", "0")
        // Names as they are: quoted ("b/\303\251.rs") they are not the
        // names the rest of the app knows the files by.
        .args(["-c", "core.quotePath=false"])
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .ok()?;
    // `diff --no-index` exits 1 when the files differ.
    if !output.status.success() && output.status.code() != Some(1) {
        return None;
    }
    // One file in Latin-1 is no reason to show nothing of all the others:
    // what is not UTF-8 shows as U+FFFD.
    Some(String::from_utf8(output.stdout).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned()))
}

/// What a worktree has that `base` has not: commits, and whether anything
/// is not committed.
pub fn work_beyond(path: &Path, base: &str) -> (u32, bool) {
    let ahead = git(path, &["rev-list", "--count", &format!("{base}..HEAD"), "--"]).and_then(|n| n.trim().parse().ok()).unwrap_or(0);
    let dirty = git(path, &["status", "--porcelain"]).is_some_and(|s| !s.trim().is_empty());
    (ahead, dirty)
}

/// Every worktree of the repository containing `path`, main one first.
/// The branch of a folder git cannot read: `worktrees` gives it alone.
pub const NO_GIT: &str = "(no git)";

pub fn worktrees(path: &Path) -> Vec<Worktree> {
    let Some(out) = git(path, &["worktree", "list", "--porcelain"]) else {
        return vec![Worktree { path: path.to_path_buf(), branch: NO_GIT.into(), locked: None }];
    };
    let mut result = Vec::new();
    for block in out.split("\n\n") {
        let mut wt_path = None;
        let mut branch = String::from("(detached)");
        let mut locked = None;
        for line in block.lines() {
            if let Some(p) = line.strip_prefix("worktree ") {
                // On another machine: keyed as the window knows it.
                wt_path = Some(crate::hostfs::rekey(path, PathBuf::from(p)));
            } else if let Some(b) = line.strip_prefix("branch ") {
                branch = b.trim_start_matches("refs/heads/").to_string();
            } else if line == "locked" {
                locked = Some(String::new());
            } else if let Some(reason) = line.strip_prefix("locked ") {
                locked = Some(reason.trim_matches('"').to_string());
            } else if line == "bare" {
                wt_path = None;
            }
        }
        if let Some(path) = wt_path {
            result.push(Worktree { path, branch, locked });
        }
    }
    result
}

/// Where a task's worktree is when that is not on the branch the task owns:
/// another branch, no branch, or a git operation under way.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Head {
    /// The branch checked out, or being rebased; empty on none.
    pub branch: String,
    /// On no branch: the ref it is at (origin/main), else its short hash.
    pub at: String,
    /// "rebasing", "merging", "cherry-picking", "reverting", "bisecting".
    pub doing: Option<&'static str>,
}

impl Head {
    /// In words, against the task's own branch; None when it is on it.
    pub fn off(&self, own: &str) -> Option<String> {
        match (self.doing, self.branch.as_str()) {
            (Some(doing), b) if b.is_empty() || b == own => Some(doing.to_string()),
            (Some(doing), b) => Some(format!("{doing} {b}")),
            (None, "") => Some(format!("detached at {}", self.at)),
            (None, b) if b != own => Some(format!("on {b}")),
            _ => None,
        }
    }
}

/// The git folder of a worktree: its `.git`, or where its `.git` file points.
fn git_dir_of(worktree: &Path) -> Option<PathBuf> {
    let dot = worktree.join(".git");
    // Where the worktree's machine is (crate::hostfs).
    if crate::hostfs::stat(&dot).is_ok_and(|s| s.dir) {
        return Some(dot);
    }
    let text = crate::hostfs::read_to_string(&dot).ok()?;
    let dir = PathBuf::from(text.trim().strip_prefix("gitdir:")?.trim());
    Some(if dir.is_absolute() { crate::hostfs::rekey(worktree, dir) } else { worktree.join(dir) })
}

/// What a worktree's HEAD is on, `listed` being its branch as `git worktree
/// list` gave it ("(detached)" for none).
pub fn head_of(worktree: &Path, listed: &str) -> Head {
    let dir = git_dir_of(worktree);
    let has = |name: &str| dir.as_ref().is_some_and(|d| crate::hostfs::stat(&d.join(name)).is_ok());
    let rebasing = ["rebase-merge", "rebase-apply"].into_iter().find(|d| has(d));
    let doing = if rebasing.is_some() {
        Some("rebasing")
    } else if has("MERGE_HEAD") {
        Some("merging")
    } else if has("CHERRY_PICK_HEAD") {
        Some("cherry-picking")
    } else if has("REVERT_HEAD") {
        Some("reverting")
    } else if has("BISECT_LOG") {
        Some("bisecting")
    } else {
        None
    };
    let mut branch = if listed.starts_with('(') { String::new() } else { listed.to_string() };
    if let (Some(rebase), Some(dir)) = (rebasing, dir.as_ref()) {
        if let Ok(name) = crate::hostfs::read_to_string(&dir.join(rebase).join("head-name")) {
            branch = name.trim().trim_start_matches("refs/heads/").to_string();
        }
    }
    let at = if branch.is_empty() && doing.is_none() { described_head(worktree) } else { String::new() };
    Head { branch, at, doing }
}

/// A ref HEAD is at, a remote's first (origin/main), else its short hash.
fn described_head(worktree: &Path) -> String {
    let names = git(worktree, &["for-each-ref", "--points-at", "HEAD", "--format=%(refname)", "refs/remotes", "refs/heads", "refs/tags"]).unwrap_or_default();
    ["refs/remotes/", "refs/heads/", "refs/tags/"]
        .iter()
        .find_map(|kind| names.lines().filter(|name| !name.ends_with("/HEAD")).find_map(|name| name.strip_prefix(kind)))
        .map(str::to_string)
        .or_else(|| git(worktree, &["rev-parse", "--short", "HEAD"]).map(|h| h.trim().to_string()))
        .unwrap_or_default()
}

fn is_ancestor(root: &Path, older: &str, newer: &str) -> bool {
    git_status(root, &["merge-base", "--is-ancestor", older, newer]).is_ok()
}

/// The branch `other`, checked out in a task's worktree, is that task's work
/// under another name: made since the task was (`since`, Unix seconds), from
/// its branch `own`, which it has all of unless it is gone (renamed).
pub fn grew_from(worktree: &Path, own: &str, other: &str, since: u64) -> bool {
    if since == 0 || own.is_empty() || other == own {
        return false;
    }
    if branch_exists(worktree, own) && !is_ancestor(worktree, own, other) {
        return false;
    }
    // The first line of its reflog is when it was made.
    let made = git(worktree, &["log", "-g", "--format=%ct", &format!("refs/heads/{other}")])
        .and_then(|out| out.lines().last().and_then(|l| l.trim().parse::<u64>().ok()));
    made.is_some_and(|made| made >= since)
}

/// Puts a task's worktree back on its branch `own`. With all of it here,
/// the branch is moved to where the worktree is; else the worktree goes to
/// it, and commits here on no other branch get a branch first.
pub fn back_on_branch(worktree: &Path, own: &str) -> Result<String, String> {
    if !branch_exists(worktree, own) {
        git_status(worktree, &["switch", "-c", own])?;
        return Ok(format!("On {own} again, made here"));
    }
    if is_ancestor(worktree, own, "HEAD") {
        git_status(worktree, &["switch", "-C", own])?;
        return Ok(format!("On {own} again, moved here"));
    }
    let loose: u32 = git(worktree, &["rev-list", "--count", "HEAD", "--not", "--branches", "--remotes"]).and_then(|n| n.trim().parse().ok()).unwrap_or(0);
    let mut kept = String::new();
    if loose > 0 {
        let name = std::iter::once(format!("{own}-here"))
            .chain((2..).map(|n| format!("{own}-here-{n}")))
            .find(|name| !branch_exists(worktree, name))
            .unwrap_or_else(|| format!("{own}-here"));
        git_status(worktree, &["branch", &name])?;
        kept = format!("; the {loose} commit{} on no branch {} on {name}", if loose == 1 { "" } else { "s" }, if loose == 1 { "is" } else { "are" });
    }
    git_status(worktree, &["switch", own])?;
    Ok(format!("On {own} again{kept}"))
}

const DIFF_ARGS: &[&str] = &[
    "--no-color",
    "--no-ext-diff",
    "--no-textconv",
    "--find-renames",
    "--src-prefix=a/",
    "--dst-prefix=b/",
    // Whatever the user's own git is set to show: the patch is parsed, and
    // its changes are applied back one at a time.
    "--unified=3",
    "--submodule=short",
];

/// Diffs to look at leave out changes of whitespace and blank lines
/// (Settings, the diffs' "Ignore whitespace").
pub fn ignore_whitespace() -> bool {
    #[cfg(test)]
    return IGNORE_WHITESPACE.get();
    #[cfg(not(test))]
    crate::state::with_settings(|s| s.diff_ignore_whitespace)
}

#[cfg(test)]
thread_local! {
    static IGNORE_WHITESPACE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// `DIFF_ARGS`, and what leaves whitespace out when it is.
fn diff_args() -> Vec<&'static str> {
    let mut args = DIFF_ARGS.to_vec();
    if ignore_whitespace() {
        args.extend(["--ignore-all-space", "--ignore-blank-lines"]);
    }
    args
}

/// Uncommitted changes against HEAD, untracked files included, as one patch.
pub fn working_patch(root: &Path) -> String {
    let untracked = git(root, &["ls-files", "--others", "--exclude-standard", "-z"]).unwrap_or_default();
    let untracked: Vec<&str> = untracked.split('\0').filter(|f| !f.is_empty()).collect();
    tracked_patch(root, "HEAD", false) + &new_files_patch(root, &untracked)
}

/// The tracked files of the worktree against `rev`.
fn tracked_patch(root: &Path, rev: &str, light: bool) -> String {
    let mut args = light_args(light, &["diff", rev]);
    args.extend(diff_args());
    without_huge_files(git(root, &args).unwrap_or_default())
}

/// The most untracked files a patch shows, the largest of them shown in
/// full, and the most of their text in all: an agent leaves logs, bundles
/// and data files behind, and the patch is read again on every change.
const NEW_FILES: usize = 200;
const NEW_FILE_BYTES: u64 = 512 * 1024;
const NEW_FILES_BYTES: usize = 4 * 1024 * 1024;

/// Untracked files as the patch `git diff` would give were they added,
/// made here: asking git costs a process a file, on every refresh.
fn new_files_patch(root: &Path, untracked: &[&str]) -> String {
    let mut patch = String::new();
    for file in untracked.iter().take(NEW_FILES) {
        new_file_patch(root, file, patch.len() < NEW_FILES_BYTES, &mut patch);
    }
    patch
}

fn new_file_patch(root: &Path, file: &str, in_full: bool, patch: &mut String) {
    use std::fmt::Write as _;
    let path = root.join(file);
    // Where the task's machine is (crate::hostfs).
    let Ok(meta) = crate::hostfs::stat(&path) else { return };
    let executable = meta.executable;
    let (mode, text) = if meta.link {
        ("120000", std::fs::read_link(&path).ok().map(|to| to.to_string_lossy().into_owned()))
    } else if !meta.file {
        return;
    } else {
        // Too large, or not text (as git tells: a NUL early on): named, not shown.
        let text = (in_full && meta.len <= NEW_FILE_BYTES)
            .then(|| crate::hostfs::read(&path).ok())
            .flatten()
            .filter(|bytes| !bytes[..bytes.len().min(8000)].contains(&0))
            .and_then(|bytes| String::from_utf8(bytes).ok());
        (if executable { "100755" } else { "100644" }, text)
    };
    let _ = write!(patch, "diff --git a/{file} b/{file}\nnew file mode {mode}\n");
    match text {
        None => {
            let _ = writeln!(patch, "Binary files /dev/null and b/{file} differ");
        }
        Some(text) if text.is_empty() => {}
        Some(text) => {
            let lines = text.split_inclusive('\n').count();
            let range = if lines == 1 { "+1".to_string() } else { format!("+1,{lines}") };
            let _ = write!(patch, "--- /dev/null\n+++ b/{file}\n@@ -0,0 {range} @@\n");
            for line in text.split_inclusive('\n') {
                patch.push('+');
                patch.push_str(line);
            }
            if !text.ends_with('\n') {
                patch.push_str("\n\\ No newline at end of file\n");
            }
        }
    }
}

/// One change of a patch, on its own: a run of lines added and removed,
/// enough to take back or to stage without the rest of its file, or of
/// its hunk, where git puts changes a few lines apart together.
#[derive(Clone, Debug, PartialEq)]
pub struct Hunk {
    pub path: String,
    /// The last line it changes: on the new side, or on the old one
    /// (`modified` false) when it only removes.
    pub line: u32,
    pub modified: bool,
    /// Its lines, added and removed: what it is, wherever in the file the
    /// other changes have moved it.
    pub changes: String,
    /// The hunk it is in, kept once for all its changes, and which of the
    /// hunk's lines are this one: its patches are made when asked for. Made
    /// for each change ahead, they were the hunk again for every change in it.
    source: std::sync::Arc<HunkSource>,
    lines: std::ops::Range<usize>,
}

#[derive(Debug, PartialEq)]
struct HunkSource {
    /// The file's header.
    header: String,
    /// The first line on the old side and on the new.
    old: u32,
    new: u32,
    lines: Vec<String>,
}

impl Hunk {
    /// The file's header and this change as `git apply -R` takes it from
    /// the file as it is now: the other changes of its hunk are there.
    pub fn patch(&self) -> String {
        self.alone(true)
    }

    /// The same for `git apply` to the file as it was: they are not.
    pub fn stage(&self) -> String {
        self.alone(false)
    }

    /// With `as_it_is` the lines the other changes added are around this
    /// one, and those they removed are not; without, the reverse.
    fn alone(&self, as_it_is: bool) -> String {
        let HunkSource { header, old, new, lines } = &*self.source;
        let (mut body, mut olds, mut news, mut kept) = (String::new(), 0, 0, true);
        for (i, line) in lines.iter().enumerate() {
            let (sign, rest) = line.split_at(1.min(line.len()));
            let here = self.lines.contains(&i);
            let stays = if as_it_is { "+" } else { "-" };
            kept = match sign {
                "\\" => kept,
                "+" | "-" if !here => sign == stays,
                _ => true,
            };
            if !kept {
                continue;
            }
            let sign = if here || !line.starts_with(['+', '-']) { sign } else { " " };
            olds += u32::from(sign == " " || sign == "-");
            news += u32::from(sign == " " || sign == "+");
            body.push_str(sign);
            body.push_str(rest);
        }
        format!("{header}@@ -{old},{olds} +{new},{news} @@\n{body}")
    }
}

/// A one-hunk patch whose context lines differ from `text` in whitespace
/// only, with those lines as `text` has them: `text` is its new side when
/// `new_side` (to apply in reverse), else its old one. None when the lines
/// are not there.
fn as_written(patch: &str, text: &str, new_side: bool) -> Option<String> {
    let at = patch.find("\n@@ ").map(|at| at + 1).or_else(|| patch.starts_with("@@ ").then_some(0))?;
    let (header, hunk) = patch.split_at(at);
    let (range, body) = hunk.split_once('\n')?;
    let side = range.split_whitespace().find(|part| part.starts_with(if new_side { '+' } else { '-' }))?;
    let first: usize = side[1..].split(',').next()?.parse().ok()?;
    let own = if new_side { '+' } else { '-' };
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    let squeeze = |line: &str| line.chars().filter(|c| !c.is_whitespace()).collect::<String>();
    let mut cursor = first.max(1) - 1;
    let mut out = format!("{header}{range}\n");
    for line in body.split_inclusive('\n') {
        let sign = line.chars().next().unwrap_or(' ');
        if sign == ' ' || sign == own {
            let written = lines.get(cursor)?;
            if squeeze(written) != squeeze(&line[1..]) {
                return None;
            }
            out.push(sign);
            out.push_str(written);
            if !written.ends_with('\n') {
                out.push('\n');
            }
            cursor += 1;
        } else {
            out.push_str(line);
        }
    }
    Some(out)
}

/// The changes of a patch `git diff` made. Not those of files that are new
/// or gone: there the file is the change.
pub fn hunks(patch: &str) -> Vec<Hunk> {
    let mut found = Vec::new();
    let (mut header, mut path, mut whole_file) = (String::new(), String::new(), false);
    // (first old line, first new line, lines)
    let mut open: Option<(u32, u32, Vec<&str>)> = None;
    let mut close = |open: &mut Option<(u32, u32, Vec<&str>)>, header: &str, path: &str, whole_file: bool| {
        let Some((old, new, lines)) = open.take() else { return };
        // Nor what was not UTF-8 and is shown with U+FFFD: it would not apply.
        if whole_file || path.is_empty() || lines.iter().any(|line| line.contains('\u{FFFD}')) {
            return;
        }
        let changed = |line: &str| line.starts_with(['+', '-']);
        let mut source = None;
        let mut at = 0;
        while at < lines.len() {
            if !changed(lines[at]) {
                at += 1;
                continue;
            }
            let end = (at..lines.len()).find(|&i| !changed(lines[i]) && !lines[i].starts_with('\\')).unwrap_or(lines.len());
            let (mut old_line, mut new_line, mut last) = (old, new, None);
            for (i, line) in lines.iter().enumerate().take(end) {
                let here = i >= at;
                match line.as_bytes().first() {
                    Some(b'+') => {
                        if here {
                            last = Some((new_line, true));
                        }
                        new_line += 1;
                    }
                    Some(b'-') => {
                        // A line added after it speaks for the change.
                        if here && !last.is_some_and(|(_, modified)| modified) {
                            last = Some((old_line, false));
                        }
                        old_line += 1;
                    }
                    Some(b' ') => {
                        old_line += 1;
                        new_line += 1;
                    }
                    _ => {}
                }
            }
            if let Some((line, modified)) = last {
                let source = source.get_or_insert_with(|| std::sync::Arc::new(HunkSource { header: header.to_string(), old, new, lines: lines.iter().map(|line| line.to_string()).collect() }));
                found.push(Hunk { path: path.to_string(), line, modified, changes: lines[at..end].concat(), source: source.clone(), lines: at..end });
            }
            at = end;
        }
    };
    for line in patch.split_inclusive('\n') {
        if line.starts_with("diff --git ") {
            close(&mut open, &header, &path, whole_file);
            (header, path, whole_file) = (line.to_string(), String::new(), false);
        } else if let Some(at) = line.strip_prefix("@@ -") {
            close(&mut open, &header, &path, whole_file);
            // "12,3 +14,5 @@ fn name"
            let start = |side: &str| side.split(',').next().and_then(|n| n.parse::<u32>().ok()).unwrap_or(0);
            let mut sides = at.split(' ');
            let (old, new) = (start(sides.next().unwrap_or_default()), start(sides.next().unwrap_or_default().trim_start_matches('+')));
            open = Some((old, new, Vec::new()));
        } else if let Some((_, _, lines)) = open.as_mut() {
            lines.push(line);
        } else {
            // A submodule's commit is no line to take back either.
            let submodule = line.strip_prefix("index ").is_some_and(|index| index.trim_end().ends_with(" 160000"));
            if line.starts_with("--- /dev/null") || line.starts_with("+++ /dev/null") || line.starts_with("Binary files") || submodule {
                whole_file = true;
            }
            if let Some(name) = line.strip_prefix("+++ b/") {
                path = name.trim_end().to_string();
                // The header a change is applied with is of the file as it is
                // named now, and of nothing else: with the one git gave, taking
                // one line back would also take back a rename or a mode.
                header = format!("diff --git a/{path} b/{path}\n--- a/{name}+++ b/{name}");
            } else {
                header.push_str(line);
            }
        }
    }
    close(&mut open, &header, &path, whole_file);
    found
}

/// Stages one change of a file, or takes it out of the index again. The
/// change is looked up in what the index has now, so that others of its
/// file staged before it are no obstacle.
pub fn stage_change(root: &Path, change: &Hunk, stage: bool) -> Result<String, String> {
    // The whole of what is staged, not the file's alone: a renamed file
    // would be a new one there, with no change to take out.
    let mut args = if stage { vec!["diff"] } else { vec!["diff", "--cached"] };
    args.extend(diff_args());
    if stage {
        args.extend(["--", change.path.as_str()]);
    }
    let now = hunks(&git(root, &args).unwrap_or_default())
        .into_iter()
        .find(|hunk| hunk.path == change.path && hunk.changes == change.changes)
        .ok_or("the change is no longer there")?;
    if stage { apply_hunk(root, &now.stage(), false, true) } else { apply_hunk(root, &now.patch(), true, true) }
}

/// Applies one hunk: to the worktree (`reverse` takes the change back),
/// or with `cached` to the index (stages it).
pub fn apply_hunk(root: &Path, patch: &str, reverse: bool, cached: bool) -> Result<String, String> {
    use std::io::Write as _;
    let mut command = crate::procs::command("git");
    command.current_dir(root).args(["apply", "--whitespace=nowarn"]);
    // Shown without whitespace, a change has its context lines as the new
    // side has them: they are made what the side it applies to has.
    let exact;
    let patch = if ignore_whitespace() {
        let path = patch.lines().find_map(|l| l.strip_prefix("+++ b/").or_else(|| l.strip_prefix("--- a/"))).unwrap_or_default().trim_end_matches('\t');
        let text = if cached { git(root, &["show", &format!(":{path}")]) } else { std::fs::read_to_string(root.join(path)).ok() };
        exact = text.and_then(|text| as_written(patch, &text, reverse)).ok_or("the change no longer applies")?;
        exact.as_str()
    } else {
        patch
    };
    if reverse {
        command.arg("-R");
    }
    if cached {
        command.arg("--cached");
    }
    let mut child = command
        .arg("-")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("git: {e}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(patch.as_bytes());
    }
    let done = child.wait_with_output().map_err(|e| format!("git: {e}"))?;
    if done.status.success() {
        Ok(String::new())
    } else {
        Err(String::from_utf8_lossy(&done.stderr).lines().next().unwrap_or("the change no longer applies").to_string())
    }
}

/// First line of each file's first hunk on the new side, keyed by path.
pub fn first_changed_lines(patch: &str) -> std::collections::HashMap<String, u32> {
    let mut lines = std::collections::HashMap::new();
    let mut current: Option<String> = None;
    for line in patch.lines() {
        if let Some(path) = line.strip_prefix("+++ ") {
            // Git ends a name that has a space in it with a tab.
            current = path.strip_prefix("b/").map(|path| path.trim_end_matches('\t').to_string());
        } else if line.starts_with("@@ ") {
            if let Some(path) = current.take() {
                let start = line
                    .split_whitespace()
                    .find_map(|part| part.strip_prefix('+'))
                    .and_then(|range| range.split(',').next())
                    .and_then(|n| n.parse().ok())
                    .unwrap_or(1);
                lines.insert(path, start);
            }
        }
    }
    lines
}

/// What the system leaves in folders for itself (Finder's, Explorer's,
/// Dolphin's): not shown among a task's files unless committed. The Changes
/// panel still lists one not ignored, as what a commit of all would take.
pub fn is_junk(path: &str) -> bool {
    let name = path.trim_end_matches('/').rsplit('/').next().unwrap_or(path);
    matches!(name, ".DS_Store" | ".localized" | ".Spotlight-V100" | ".Trashes" | ".fseventsd" | "Thumbs.db" | "ehthumbs.db" | "desktop.ini" | "Desktop.ini" | "$RECYCLE.BIN" | ".directory")
        || name.starts_with("._")
}

/// Tracked and untracked-but-not-ignored files, relative to `root`.
#[cfg(test)]
pub fn files(root: &Path) -> Vec<String> {
    let out = git(root, &["ls-files", "--cached", "--others", "--exclude-standard", "-z"]).unwrap_or_default();
    let mut files: Vec<String> = out.split('\0').filter(|f| !f.is_empty()).map(str::to_string).collect();
    files.sort();
    files.dedup();
    files
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct BranchInfo {
    pub head: String,
    pub upstream: Option<String>,
    /// Push and PR go to `origin`.
    pub has_origin: bool,
    pub ahead: u32,
    pub behind: u32,
    /// The branch work starts from (origin/main), and the commits there
    /// this branch lacks.
    pub base: Option<String>,
    pub behind_base: u32,
    pub last_hash: String,
    pub last_subject: String,
    /// The last commit is on the remote already: amending it would rewrite
    /// what others may have.
    pub head_pushed: bool,
    /// The files a pull would leave in conflict, with what is committed
    /// here or only written; empty when none, or when it cannot be told.
    pub pull_conflicts: Vec<String>,
    /// The same for the base branch as last fetched: what a rebase onto it
    /// would stop at, known before the PR says so.
    pub base_conflicts: Vec<String>,
    /// The repository's own checkout is on another branch than the default.
    pub off_default: Option<OffDefault>,
    /// A folder without git: nothing here has a branch.
    pub plain: bool,
}

/// The repository's own checkout on a branch other than the default one.
#[derive(Clone, Default, PartialEq, Debug)]
pub struct OffDefault {
    /// The default branch as checked out (main), and the ref it is
    /// compared with (origin/main, or main itself without a remote).
    pub default: String,
    pub base: String,
    /// All the branch changed is in the base already (merged, squashed or
    /// rebased): nothing of it is lost when it goes.
    pub contained: bool,
}

/// Whether `base` holds all that HEAD changed: merged into it, it would
/// stay as it is.
fn holds_all(root: &Path, base: &str) -> bool {
    let merged = git(root, &["merge-tree", "--write-tree", base, "HEAD"]).and_then(|out| out.lines().next().map(str::to_string));
    let tree = git(root, &["rev-parse", &format!("{base}^{{tree}}")]).map(|t| t.trim().to_string());
    merged.is_some() && merged == tree
}

/// The main checkout goes to its default branch, brought up to the base
/// when it only lags behind; the branch it was on is deleted when the base
/// holds all of it.
pub fn switch_to_default(root: &Path, off: &OffDefault, from: &str) -> Result<String, String> {
    let contained = holds_all(root, off.base.as_str());
    let mut said = switch_default(root, off)?;
    if contained {
        git_status(root, &["branch", "-D", from])?;
        said.push_str(&format!("; {from} deleted, {} has all of it", off.base));
    }
    Ok(said)
}

fn switch_default(root: &Path, off: &OffDefault) -> Result<String, String> {
    let local = git(root, &["rev-parse", "--verify", "-q", &format!("refs/heads/{}", off.default)]).is_some_and(|hash| !hash.trim().is_empty());
    if local || off.base == off.default {
        git_status(root, &["switch", &off.default])?;
    } else {
        git_status(root, &["switch", "--track", "-c", &off.default, &off.base])?;
    }
    let mut said = format!("On {}", off.default);
    if off.base != off.default && git_status(root, &["merge", "--ff-only", "-q", &off.base]).is_err() {
        said.push_str(&format!(", which has commits {} lacks: not brought up to it", off.base));
    }
    Ok(said)
}

/// The branch the main checkout is on goes to a worktree of its own, with
/// what is not committed yet, and the checkout to its default branch.
pub fn move_to_task(root: &Path, off: &OffDefault, branch: &str, copy_env: bool) -> Result<PathBuf, String> {
    let dir = task_worktree_dir(root, branch);
    if dir.exists() {
        return Err(format!("{} already exists", dir.display()));
    }
    let dirty = git(root, &["status", "--porcelain"]).is_some_and(|out| !out.trim().is_empty());
    if dirty {
        git_status(root, &["stash", "push", "-u", "-m", &format!("ideae: {branch} to a task of its own")])?;
    }
    if let Err(e) = switch_default(root, off) {
        if dirty {
            let _ = git_status(root, &["stash", "pop", "--index"]);
        }
        return Err(e);
    }
    let dir_s = dir.to_string_lossy().into_owned();
    git_status(root, &["worktree", "add", &dir_s, branch])?;
    if dirty {
        git_status(&dir, &["stash", "pop", "--index"]).map_err(|e| format!("{e}; the changes are in the stash"))?;
    }
    if copy_env {
        copy_env_files(root, &dir);
    }
    Ok(dir)
}

/// `git status` read once for all it tells: the branch, how far it is from
/// its upstream, each changed file's state, the untracked files.
#[derive(Default)]
struct Status {
    info: BranchInfo,
    /// The commit HEAD is at.
    oid: String,
    states: std::collections::HashMap<String, FileState>,
    untracked: Vec<String>,
    /// The tracked files that changed, with where a renamed one was.
    changed: Vec<String>,
    /// What `.gitignore` keeps out: files, and folders as `target/` with
    /// nothing of what is in them.
    ignored: Vec<String>,
}

/// With `light` git reads the worktree on one thread, not ten: the same
/// answer for a sixth of the processor's time and a little more of the
/// clock's, which is the better trade for a task nobody looks at.
fn light_args<'a>(light: bool, args: &[&'a str]) -> Vec<&'a str> {
    let mut all = if light { vec!["-c", "core.preloadIndex=false"] } else { Vec::new() };
    all.extend_from_slice(args);
    all
}

fn parse_status(out: &str) -> Status {
    let mut status = Status::default();
    let mut entries = out.split('\0');
    while let Some(entry) = entries.next() {
        // What comes before the path: the kind, then fields by the kind.
        let changed = |fields: usize| {
            let mut parts = entry.splitn(fields + 1, ' ');
            let xy = parts.nth(1)?.as_bytes();
            Some((*xy.first()?, *xy.get(1)?, parts.nth(fields - 2)?.to_string()))
        };
        if let Some(head) = entry.strip_prefix("# branch.head ") {
            status.info.head = head.to_string();
        } else if let Some(oid) = entry.strip_prefix("# branch.oid ") {
            status.oid = oid.to_string();
        } else if let Some(upstream) = entry.strip_prefix("# branch.upstream ") {
            status.info.upstream = Some(upstream.to_string());
        } else if let Some(ab) = entry.strip_prefix("# branch.ab ") {
            for part in ab.split_whitespace() {
                if let Some(n) = part.strip_prefix('+') {
                    status.info.ahead = n.parse().unwrap_or(0);
                } else if let Some(n) = part.strip_prefix('-') {
                    status.info.behind = n.parse().unwrap_or(0);
                }
            }
        } else if let Some(path) = entry.strip_prefix("? ") {
            // A directory, when it holds a repository of its own: no file of this one.
            if path.ends_with('/') {
                continue;
            }
            status.states.insert(path.to_string(), FileState { staged: false, unstaged: true, change: Change::Untracked });
            status.untracked.push(path.to_string());
        } else if let Some(path) = entry.strip_prefix("! ") {
            status.ignored.push(path.to_string());
        } else {
            let fields = match entry.as_bytes().first() {
                Some(b'1') => 8,
                Some(b'2') => {
                    // The original path of a rename or copy.
                    status.changed.extend(entries.next().map(str::to_string));
                    9
                }
                Some(b'u') => 10,
                _ => continue,
            };
            let kind = entry.as_bytes()[0];
            if let Some((x, y, path)) = changed(fields) {
                let change = match (kind, x, y) {
                    (b'u', ..) => Change::Conflict,
                    (_, b'D', _) | (_, _, b'D') => Change::Deleted,
                    (_, b'A', _) | (_, _, b'A') => Change::Added,
                    _ => Change::Modified,
                };
                status.changed.push(path.clone());
                status.states.insert(path, FileState { staged: x != b'.', unstaged: y != b'.', change });
            }
        }
    }
    status
}

/// A file's time and size: whether it is the one seen before.
type Stamp = Option<(std::time::SystemTime, u64)>;

fn stamp(path: &Path) -> Stamp {
    let meta = std::fs::symlink_metadata(path).ok()?;
    Some((meta.modified().ok()?, meta.len()))
}

/// What a refresh need not ask git every time: where the repository keeps
/// itself, whether it has an origin, the branch work starts from (asked
/// again now and then, since a remote can be added and a default changed),
/// and the answers of the last refresh that still hold, each with what it
/// was read from.
#[derive(Default)]
pub struct Known {
    /// What a pull was last tried on in the mind, and how it came out.
    pull: (String, Vec<String>),
    /// The same for the base branch taken in (a rebase or a merge of it):
    /// HEAD and how far behind, what the worktree held, when, and the files.
    onto_base: (String, u64, Option<std::time::Instant>, Vec<String>),
    age: u32,
    git_dir: Option<PathBuf>,
    refs: Option<PathBuf>,
    /// A remote task's refs, watched there: not stamped here.
    refs_there: Option<PathBuf>,
    has_origin: bool,
    base: Option<String>,
    /// The tracked files, by the index they were listed from.
    tracked: Option<(Stamp, Arc<Vec<String>>)>,
    /// Every file and the directories to watch, as made from the tracked
    /// files and the new ones last: made again only when one of the two is
    /// another, and one list for all who read it.
    listed: Option<Listed>,
    /// The repository's own checkout, not a worktree added to it.
    main_worktree: bool,
    /// What only a new commit or a moved base changes.
    history: Option<(HistoryKey, History)>,
    /// What the patches last given were made of.
    patches: Option<u64>,
    /// The stashes, by the log they were listed from.
    stashes: (Stamp, Vec<Stash>),
}

struct Listed {
    tracked: Arc<Vec<String>>,
    untracked: Vec<String>,
    files: Arc<Vec<String>>,
    flat: Arc<std::collections::BTreeSet<PathBuf>>,
}

/// HEAD, the branch compared with, and the files its ref can be in.
#[derive(PartialEq)]
struct HistoryKey {
    oid: String,
    base: Option<String>,
    compared: Option<String>,
    refs: Vec<Stamp>,
}

#[derive(Clone, Default)]
struct History {
    behind_base: u32,
    ahead_of_base: u32,
    last_hash: String,
    last_subject: String,
    /// Where the branch left the one it is compared with.
    fork: Option<String>,
    off_default: Option<OffDefault>,
}

impl Known {
    fn fresh(&mut self, root: &Path) {
        if self.age % 30 == 0 {
            // Paths on the task's machine: keyed as the window knows it (a
            // remote one's are not looked at here).
            self.git_dir = git(root, &["rev-parse", "--absolute-git-dir"]).map(|dir| crate::hostfs::rekey(root, PathBuf::from(dir.trim())));
            let common = git(root, &["rev-parse", "--path-format=absolute", "--git-common-dir"]).map(|common| crate::hostfs::rekey(root, PathBuf::from(common.trim())));
            self.main_worktree = self.git_dir.is_some() && self.git_dir == common;
            self.refs_there = common.as_ref().filter(|_| crate::hosts::is_remote(root)).map(|common| common.join("refs"));
            self.refs = common.map(|common| common.join("refs")).filter(|refs| refs.is_dir());
            self.has_origin = git(root, &["remote", "get-url", "origin"]).is_some();
            self.base = base_ref(root);
        }
        self.age += 1;
    }

    /// The tracked files: listed again only when the index is another one.
    fn tracked(&mut self, root: &Path) -> Arc<Vec<String>> {
        // Stamped before it is read: an index written meanwhile is read again next time.
        let now = self.git_dir.as_ref().and_then(|dir| stamp(&dir.join("index")));
        if now.is_none() || self.tracked.as_ref().is_none_or(|(seen, _)| *seen != now) {
            let out = git(root, &["ls-files", "--cached", "-z"]).unwrap_or_default();
            self.tracked = Some((now, Arc::new(out.split('\0').filter(|f| !f.is_empty()).map(str::to_string).collect())));
        }
        self.tracked.as_ref().map(|(_, files)| files.clone()).unwrap_or_default()
    }

    /// The files a ref named `name` can be read from, as they are now. None
    /// where refs are not kept in files that tell (reftable): ask every time.
    fn ref_stamps(&self, names: &[Option<&str>]) -> Option<Vec<Stamp>> {
        let common = self.refs.as_ref()?.parent()?;
        if common.join("reftable").exists() {
            return None;
        }
        let mut stamps = vec![stamp(&common.join("packed-refs"))];
        for name in names.iter().flatten() {
            stamps.extend([stamp(&common.join("refs/remotes").join(name)), stamp(&common.join("refs/heads").join(name)), stamp(&common.join("refs").join(name))]);
        }
        Some(stamps)
    }
}

/// All a task shows of its worktree, read in one go.
pub struct Refreshed {
    pub files: Arc<Vec<String>>,
    /// What `.gitignore` keeps out, as the status names it.
    pub ignored: Vec<String>,
    pub watch: (Arc<std::collections::BTreeSet<PathBuf>>, Vec<PathBuf>),
    pub info: BranchInfo,
    pub states: std::collections::HashMap<String, FileState>,
    /// The branch compared with.
    pub base: Option<String>,
    /// None when they are the ones of the refresh before.
    pub patches: Option<Patches>,
    /// Git did not answer (no repository any more, no git): nothing here
    /// is to be believed, and what was read before stays.
    pub failed: bool,
    /// Work put away, the latest first.
    pub stashes: Vec<Stash>,
}

pub struct Patches {
    /// The uncommitted changes, untracked files included.
    pub working: String,
    /// The changes since the branch forked from the one compared with.
    pub branch: String,
    /// What is staged; asked for only when something is.
    pub staged: String,
}

/// Work put away with `git stash`. The repository has one list of it,
/// whichever worktree it was put away in.
#[derive(Clone, Debug, PartialEq)]
pub struct Stash {
    /// `stash@{0}`.
    pub name: String,
    /// "On main: before the pull".
    pub what: String,
    /// "2 hours ago".
    pub when: String,
    /// Its commit: the name is a place in the list, which every worktree
    /// of the repository adds to and takes from.
    pub id: String,
}

/// Where a stash is in the list now: an action is for the one that was
/// shown, not for whatever has its place since.
fn stash_now(root: &Path, stash: &Stash) -> Result<String, String> {
    stashes(root).into_iter().find(|now| now.id == stash.id).map(|now| now.name).ok_or_else(|| format!("{} ({}) is no longer in the list", stash.name, stash.what))
}

pub fn stashes(root: &Path) -> Vec<Stash> {
    let list = git(root, &["stash", "list", "--format=%gd%x1f%gs%x1f%cr%x1f%H"]).unwrap_or_default();
    list.lines()
        .filter_map(|line| {
            let mut parts = line.split('\x1f');
            Some(Stash { name: parts.next()?.to_string(), what: parts.next()?.to_string(), when: parts.next()?.to_string(), id: parts.next()?.to_string() })
        })
        .collect()
}

/// Puts away everything not committed, files git does not know of too.
pub fn stash(root: &Path, message: &str) -> Result<String, String> {
    let mut args = vec!["stash", "push", "--include-untracked"];
    if !message.is_empty() {
        args.extend(["-m", message]);
    }
    run(root, "git", &args).map(|_| "Put away: it is under Stashes".to_string())
}

/// Brings a stash back into the worktree; with `drop` it leaves the list
/// when that went without a conflict. On a conflict it stays.
pub fn stash_apply(root: &Path, stash: &Stash, drop: bool) -> Result<String, String> {
    let name = &stash_now(root, stash)?;
    // Not tried when it would not go in: a stash applied halfway leaves its conflict in the files.
    if let Some(files) = merge_conflicts(root, Some(&format!("{name}^1")), name).filter(|files| !files.is_empty()) {
        return Err(format!("would conflict in {} (the stash is kept)", files.join(", ")));
    }
    run(root, "git", &["stash", "apply", name]).map_err(|error| {
        let first = error.lines().find(|line| line.starts_with("error") || line.contains("CONFLICT")).unwrap_or(error.lines().next().unwrap_or_default());
        format!("{first} (the stash is kept)")
    })?;
    if drop {
        run(root, "git", &["stash", "drop", name])?;
    }
    Ok(format!("Brought back {name}"))
}

pub fn stash_drop(root: &Path, stash: &Stash) -> Result<String, String> {
    let name = &stash_now(root, stash)?;
    run(root, "git", &["stash", "drop", name]).map(|_| format!("Dropped {name}"))
}

/// What a stash holds, as a patch against the commit it was made on.
pub fn stash_patch(root: &Path, name: &str) -> String {
    let mut args = vec!["stash", "show", "-p", "--include-untracked"];
    if ignore_whitespace() {
        args.extend(["--ignore-all-space", "--ignore-blank-lines"]);
    }
    args.push(name);
    git(root, &args).unwrap_or_default()
}

/// Entries of a folder without git that are listed and watched: one opened
/// by mistake (a home directory) must not take the machine's inotify watches.
const PLAIN_FOLDER_LIMIT: usize = 50_000;

/// Where version control keeps itself: not a project's files.
const VCS_DIRS: [&str; 5] = [".git", ".svn", ".hg", ".bzr", "CVS"];
/// What tells a Perforce workspace, which keeps no folder of its own.
const VCS_FILES: [&str; 3] = [".p4config", ".p4ignore", "P4CONFIG"];
/// A build's output, which a folder without a .gitignore does not name.
const OUTPUT_DIRS: [&str; 8] = ["node_modules", "target", "__pycache__", ".venv", "venv", ".gradle", ".cache", ".next"];

pub fn is_repository(path: &Path) -> bool {
    git(path, &["rev-parse", "--git-dir"]).is_some()
}

/// Whether a folder may become a repository unasked: git is there, no other
/// version control has it, and it is not a whole home or disk.
fn may_init(root: &Path) -> bool {
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")).map(PathBuf::from);
    root.parent().is_some()
        && home.as_deref() != Some(root)
        && !root.ancestors().any(|folder| VCS_DIRS.iter().chain(&VCS_FILES).any(|mark| folder.join(mark).exists()))
        && crate::procs::command("git").arg("--version").output().is_ok_and(|out| out.status.success())
}

/// Makes `root` a repository if it may be one. Whether a task can have a
/// worktree of it: that takes a commit, and the first is the user's to make.
pub fn adopt(root: &Path) -> bool {
    if !is_repository(root) && (!may_init(root) || make_repository(root).is_err()) {
        return false;
    }
    git(root, &["rev-parse", "--verify", "-q", "HEAD^{commit}"]).is_some_and(|hash| !hash.trim().is_empty())
}

/// `git init`, nothing committed. The project gets no file of ours: what to
/// leave out is in .git/info.
pub fn make_repository(root: &Path) -> Result<String, String> {
    let out = git_status(root, &["init", "-q"])?;
    let exclude = VCS_DIRS.iter().chain(&OUTPUT_DIRS).map(|dir| format!("{dir}/\n")).collect::<String>();
    let at = root.join(".git/info");
    std::fs::create_dir_all(&at).and_then(|()| std::fs::write(at.join("exclude"), exclude)).map_err(|e| e.to_string())?;
    Ok(out)
}

/// A project that is a plain folder: its files from the disk, no changes.
fn refresh_plain_folder(root: &Path, known: &mut Known) -> Refreshed {
    // With no .gitignore to say so: a build's output is shown as ignored, read
    // when opened, or every build would walk it again.
    fn is_output(name: &str) -> bool {
        OUTPUT_DIRS.contains(&name)
    }
    // A `git init` here is seen at the next refresh, not thirty later.
    known.age = 0;
    let mut files = Vec::new();
    let mut ignored = Vec::new();
    let mut flat = std::collections::BTreeSet::from([root.to_path_buf()]);
    let walk = ignore::WalkBuilder::new(root)
        .hidden(false)
        .require_git(false)
        .filter_entry(|entry| {
            let in_output = entry.depth() > 1 && entry.path().parent().and_then(Path::file_name).and_then(|name| name.to_str()).is_some_and(is_output);
            !in_output && !entry.file_name().to_str().is_some_and(|name| VCS_DIRS.contains(&name) || is_junk(name))
        })
        .build();
    for entry in walk.flatten().skip(1).take(PLAIN_FOLDER_LIMIT) {
        let Some(path) = entry.path().strip_prefix(root).ok().and_then(Path::to_str).map(|path| path.replace('\\', "/")) else { continue };
        if !entry.file_type().is_some_and(|kind| kind.is_dir()) {
            files.push(path);
        } else if entry.file_name().to_str().is_some_and(is_output) {
            ignored.push(path + "/");
        } else {
            flat.insert(entry.into_path());
        }
    }
    files.sort();
    ignored.sort();
    let same = known.listed.as_ref().is_some_and(|listed| *listed.files == files && *listed.flat == flat);
    if !same {
        known.listed = Some(Listed { tracked: Arc::default(), untracked: Vec::new(), files: Arc::new(files), flat: Arc::new(flat) });
    }
    let (files, flat) = known.listed.as_ref().map(|listed| (listed.files.clone(), listed.flat.clone())).unwrap_or_default();
    Refreshed {
        files,
        ignored,
        watch: (flat, Vec::new()),
        info: BranchInfo { plain: true, ..Default::default() },
        states: Default::default(),
        base: None,
        patches: None,
        stashes: Vec::new(),
        failed: false,
    }
}

/// `picked`: the branch to compare with when not the one work starts from.
/// `light`: for a task out of view, see `light_args`.
pub fn refresh(root: &Path, picked: Option<String>, light: bool, known: &mut Known) -> Refreshed {
    use std::hash::{Hash as _, Hasher as _};
    known.fresh(root);
    let raw = git(root, &light_args(light, &["status", "--porcelain=v2", "--branch", "-z", "--untracked-files=all", "--ignored=matching"])).unwrap_or_default();
    // A status has its branch lines at the least.
    let failed = raw.is_empty();
    if failed && known.git_dir.is_none() {
        return refresh_plain_folder(root, known);
    }
    let Status { mut info, oid, states, untracked, changed, mut ignored } = parse_status(&raw);
    ignored.retain(|path| !is_junk(path));

    // Every file: the tracked ones and, from the status, those that are new.
    let tracked = known.tracked(root);
    if !known.listed.as_ref().is_some_and(|listed| Arc::ptr_eq(&listed.tracked, &tracked) && listed.untracked == untracked) {
        // With nothing new the tracked files are all of them, as git lists them: in order.
        let shown = untracked.iter().filter(|path| !is_junk(path));
        let files = if shown.clone().next().is_none() && tracked.is_sorted() {
            tracked.clone()
        } else {
            let mut files: Vec<String> = tracked.iter().chain(shown).cloned().collect();
            files.sort();
            files.dedup();
            Arc::new(files)
        };
        let mut flat: std::collections::BTreeSet<PathBuf> = std::collections::BTreeSet::new();
        let mut last = "";
        for dir in files.iter().filter_map(|file| file.rsplit_once('/')).map(|(dir, _)| dir) {
            // Sorted: a directory's files come together.
            if dir != last {
                flat.insert(root.join(dir));
                last = dir;
            }
        }
        flat.insert(root.to_path_buf());
        flat.extend(known.git_dir.clone());
        known.listed = Some(Listed { tracked, untracked: untracked.clone(), files, flat: Arc::new(flat) });
    }
    let (files, flat) = known.listed.as_ref().map(|listed| (listed.files.clone(), listed.flat.clone())).unwrap_or_default();
    let watch = (flat, known.refs.clone().or_else(|| known.refs_there.clone()).into_iter().collect());

    info.has_origin = known.has_origin;
    // Not for the base branch itself (main, tracking origin/main).
    let base = known.base.clone().filter(|b| b.rsplit('/').next() != Some(info.head.as_str()));
    // On the base branch itself the comparison is the uncommitted diff again.
    let compared = picked.or_else(|| known.base.clone().filter(|base| *base != info.head));
    let key = known.ref_stamps(&[base.as_deref(), compared.as_deref()]).map(|refs| HistoryKey { oid: oid.clone(), base: base.clone(), compared: compared.clone(), refs });
    let history = match (&key, &known.history) {
        (Some(key), Some((seen, history))) if key == seen => history.clone(),
        _ => {
            let count = |range: String| git(root, &["rev-list", "--count", &range]).and_then(|n| n.trim().parse().ok()).unwrap_or(0);
            let mut history = History::default();
            if let Some(base) = &base {
                history.behind_base = count(format!("HEAD..{base}"));
                history.ahead_of_base = count(format!("{base}..HEAD"));
            }
            if let Some(last) = git(root, &["log", "-1", "--format=%h%x1f%s"]) {
                if let Some((hash, subject)) = last.trim_end().split_once('\x1f') {
                    (history.last_hash, history.last_subject) = (hash.to_string(), subject.to_string());
                }
            }
            // The main checkout on another branch than the one work starts from.
            if let Some(base) = base.as_ref().filter(|_| known.main_worktree && info.head != "(detached)" && !info.head.is_empty()) {
                let default = base.split_once('/').filter(|_| base.starts_with("origin/")).map_or(base.as_str(), |(_, name)| name).to_string();
                let contained = history.ahead_of_base == 0 || holds_all(root, base);
                history.off_default = Some(OffDefault { default, base: base.clone(), contained });
            }
            history.fork = compared.as_ref().and_then(|base| git(root, &["merge-base", "HEAD", base])).map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
            known.history = key.map(|key| (key, history.clone()));
            history
        }
    };
    if base.is_some() {
        info.behind_base = history.behind_base;
        // Not published yet: its own commits are the ones ahead.
        if info.upstream.is_none() {
            info.ahead = history.ahead_of_base;
        }
        info.base = base;
    }
    info.off_default = history.off_default.clone();
    (info.last_hash, info.last_subject) = (history.last_hash, history.last_subject);
    info.head_pushed = head_pushed(info.upstream.is_some(), info.ahead, info.base.is_some(), history.ahead_of_base);

    // The patches are of what the status tells, the files it names as they
    // are on disk, and where the branch forked: the same of all three, the
    // same patches, and git is not asked.
    let mut made_of = std::collections::hash_map::DefaultHasher::new();
    // Not what is ignored: a build writing its output changes no patch.
    raw.split('\0').filter(|entry| !entry.starts_with("! ")).for_each(|entry| entry.hash(&mut made_of));
    (&history.fork, &compared, ignore_whitespace()).hash(&mut made_of);
    for path in changed.iter().chain(&untracked) {
        stamp(&root.join(path)).hash(&mut made_of);
    }
    let made_of = made_of.finish();
    // Asked again at once when HEAD or the base has moved (a fetch brings
    // more commits to be behind); for the work written meanwhile, twice a
    // minute at most: each asking writes a tree and runs a merge.
    if let Some(onto) = info.base.clone().filter(|_| info.behind_base > 0) {
        let key = format!("{oid} {}", info.behind_base);
        let (seen, written, at, _) = &known.onto_base;
        let wrote = *written != made_of && at.is_none_or(|at| at.elapsed() >= std::time::Duration::from_secs(30));
        if *seen != key || wrote {
            known.onto_base = (key, made_of, Some(std::time::Instant::now()), merge_conflicts(root, None, &onto).unwrap_or_default());
        }
        info.base_conflicts = known.onto_base.3.clone();
    }
    let base = compared.filter(|_| history.fork.is_some());
    // The log of the stash is there as long as something is stashed, and
    // is written to whenever the list changes.
    let log = stamp(&known.refs.as_ref().and_then(|refs| refs.parent()).map(|common| common.join("logs/refs/stash")).unwrap_or_default());
    if known.stashes.0 != log {
        known.stashes = (log, if log.is_some() { stashes(root) } else { Vec::new() });
    }
    let stashes = known.stashes.1.clone();
    if known.patches == Some(made_of) {
        // As found when the patches were made: nothing it depends on has moved.
        if info.upstream.is_some() && info.behind > 0 {
            info.pull_conflicts = known.pull.1.clone();
        }
        return Refreshed { files, ignored, watch, info, states, base, patches: None, stashes, failed };
    }
    known.patches = Some(made_of);

    let untracked: Vec<&str> = untracked.iter().map(String::as_str).collect();
    let new_files = new_files_patch(root, &untracked);
    let working = changed_patch(root, &oid, &changed, light) + &new_files;
    let branch = match &history.fork {
        // No commit of its own yet: the same patch, not asked for twice.
        Some(fork) if *fork == oid => working.clone(),
        Some(fork) => tracked_patch(root, fork, light) + &new_files,
        None => String::new(),
    };
    if info.upstream.is_some() && info.behind > 0 {
        // Asked again only when a side of it has moved.
        let theirs = git(root, &["rev-parse", "@{u}"]).unwrap_or_default();
        let key = format!("{oid} {} {}", theirs.trim(), working.len() ^ working.bytes().fold(0usize, |sum, byte| sum.wrapping_mul(31).wrapping_add(byte as usize)));
        if known.pull.0 != key {
            known.pull = (key, pull_conflicts(root).unwrap_or_default());
        }
        info.pull_conflicts = known.pull.1.clone();
    }
    let staged = if states.values().any(|state| state.staged) { git(root, &[&["diff", "--cached"][..], &diff_args()].concat()).unwrap_or_default() } else { String::new() };
    Refreshed { files, ignored, watch, info, states, base, patches: Some(Patches { working, branch, staged }), stashes, failed }
}

/// The most changed files `git diff` is told by name. More, and naming
/// them costs more than it saves (and a command line has an end).
const NAMED_PATHS: usize = 50;

/// The tree of no files, which every repository has.
const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

/// The tracked files against HEAD, when the status has told which changed:
/// none, and git is not asked; a few, and it looks at those and not at
/// every file of the worktree.
fn changed_patch(root: &Path, head: &str, changed: &[String], light: bool) -> String {
    if changed.is_empty() {
        return String::new();
    }
    // Before the first commit there is no HEAD to compare with: what is
    // added is new against a tree with nothing in it.
    let head = if head == "(initial)" { EMPTY_TREE } else { "HEAD" };
    if changed.len() > NAMED_PATHS {
        return tracked_patch(root, head, light);
    }
    let mut args = vec!["--literal-pathspecs", "diff", head];
    args.extend(diff_args());
    args.push("--");
    args.extend(changed.iter().map(String::as_str));
    without_huge_files(git(root, &args).unwrap_or_default())
}

/// The most of one tracked file's changes a patch shows. A lock file or a
/// bundle written anew is megabytes that would be parsed, laid out and kept
/// for every task at every change: such a file is named, as a binary one is.
const FILE_PATCH_BYTES: usize = 1024 * 1024;

fn without_huge_files(patch: String) -> String {
    const START: &str = "diff --git ";
    let starts: Vec<usize> = patch.match_indices(START).map(|(at, _)| at).filter(|&at| at == 0 || patch.as_bytes()[at - 1] == b'\n').collect();
    let ends = starts.iter().skip(1).copied().chain([patch.len()]);
    if !starts.iter().zip(ends.clone()).any(|(start, end)| end - start > FILE_PATCH_BYTES) {
        return patch;
    }
    let mut out = String::with_capacity(patch.len().min(4 * FILE_PATCH_BYTES));
    for (&start, end) in starts.iter().zip(ends) {
        let section = &patch[start..end];
        // Named by the file as it is now, or as it was when it is gone.
        let name = section.lines().take_while(|line| !line.starts_with("@@ ")).find_map(|line| line.strip_prefix("+++ b/").or(line.strip_prefix("--- a/")));
        match name.filter(|_| section.len() > FILE_PATCH_BYTES) {
            Some(name) => {
                let name = name.trim_end_matches('\t');
                out.push_str(section.lines().next().unwrap_or_default());
                out.push_str(&format!("\nBinary files a/{name} and b/{name} differ\n"));
            }
            None => out.push_str(section),
        }
    }
    out
}

#[derive(Clone, Debug)]
pub struct Commit {
    pub hash: String,
    pub short: String,
    pub author: String,
    pub date: String,
    pub subject: String,
    /// The branches and tags that point at it: "main, origin/main, tag: v1".
    pub refs: String,
    /// Joins branches: shown dimmer, as there is seldom anything to read in one.
    pub merge: bool,
}

/// Whose history is read and which commits of it: all fields empty, the
/// task's own branch, all of it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct HistoryQuery {
    /// A branch, tag or commit other than HEAD.
    pub rev: String,
    /// In the message, whatever the case.
    pub text: String,
    pub author: String,
    /// Commits that touched this file or folder.
    pub path: String,
}

impl HistoryQuery {
    /// As typed into one field: words are looked for in the message,
    /// `@ann` is the author, `file:src/a.rs` what the commit touched.
    pub fn typed(rev: &str, typed: &str) -> Self {
        let mut query = Self { rev: rev.to_string(), ..Self::default() };
        let mut words = Vec::new();
        for word in typed.split_whitespace() {
            if let Some(author) = word.strip_prefix('@') {
                query.author = author.to_string();
            } else if let Some(path) = word.strip_prefix("file:") {
                query.path = path.to_string();
            } else {
                words.push(word);
            }
        }
        query.text = words.join(" ");
        query
    }
}

/// The newest `limit` commits reachable from HEAD.
pub fn history(root: &Path, limit: usize, query: &HistoryQuery) -> Vec<Commit> {
    let limit = format!("-n{limit}");
    let (text, author) = (format!("--grep={}", query.text), format!("--author={}", query.author));
    let mut args = vec!["log", limit.as_str(), "--format=%H%x1f%h%x1f%an%x1f%ar%x1f%s%x1f%D%x1f%P%x1e"];
    if !query.text.is_empty() {
        args.extend([text.as_str(), "-i", "--fixed-strings"]);
    }
    if !query.author.is_empty() {
        args.push(author.as_str());
    }
    if !query.rev.is_empty() {
        args.push(query.rev.as_str());
    }
    args.push("--");
    if !query.path.is_empty() {
        args.push(query.path.as_str());
    }
    let out = git(root, &args).unwrap_or_default();
    out.split('\x1e')
        .filter_map(|record| {
            let mut fields = record.trim_start_matches('\n').split('\x1f');
            Some(Commit {
                hash: fields.next().filter(|h| !h.is_empty())?.to_string(),
                short: fields.next()?.to_string(),
                author: fields.next()?.to_string(),
                date: fields.next()?.to_string(),
                subject: fields.next().unwrap_or_default().to_string(),
                // HEAD's own arrow says nothing the list does not.
                refs: fields.next().unwrap_or_default().split(", ").map(|name| name.trim_start_matches("HEAD -> ")).filter(|name| !name.is_empty() && *name != "HEAD" && !name.ends_with("/HEAD")).collect::<Vec<_>>().join(", "),
                merge: fields.next().is_some_and(|parents| parents.split_whitespace().count() > 1),
            })
        })
        .collect()
}

/// A commit of the graph of all branches, with its parents.
#[derive(Clone, Debug)]
pub struct GraphCommit {
    pub commit: Commit,
    pub parents: Vec<String>,
}

/// What the graph leaves out, kept in the repository's own config (so its
/// worktrees share it): branches by name pattern (`tenant/*`), and with
/// `own_only` those with no commit of their own but merges.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct GraphFilter {
    pub hide: Vec<String>,
    pub own_only: bool,
}

pub fn graph_filter(root: &Path) -> GraphFilter {
    let hide = git(root, &["config", "--get", "ideae.graphHide"]).unwrap_or_default();
    GraphFilter {
        hide: hide.split([',', ' ']).map(str::trim).filter(|g| !g.is_empty()).map(str::to_string).collect(),
        own_only: git(root, &["config", "--get", "ideae.graphOwnOnly"]).is_some_and(|v| v.trim() == "true"),
    }
}

pub fn set_graph_filter(root: &Path, filter: &GraphFilter) {
    let _ = git(root, &["config", "--local", "ideae.graphHide", &filter.hide.join(", ")]);
    let _ = git(root, &["config", "--local", "ideae.graphOwnOnly", if filter.own_only { "true" } else { "false" }]);
}

/// The branches and where they point, as one line each: what tells that the
/// graph is to be read again.
pub fn refs_stamp(root: &Path) -> u64 {
    use std::hash::{Hash as _, Hasher as _};
    let out = git(root, &["for-each-ref", "--format=%(objectname) %(refname)", "refs/heads", "refs/remotes"]).unwrap_or_default();
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    out.hash(&mut hasher);
    hasher.finish()
}

/// Which branches of the remotes have commits of their own (not merges)
/// that the base has not; `known` keeps the answers by branch and commit, so a reload asks
/// only of the branches that moved.
fn branches_with_work(root: &Path, known: &mut std::collections::HashMap<(String, String), bool>) -> Vec<(String, bool)> {
    let base = git(root, &["rev-parse", "--abbrev-ref", "origin/HEAD"]).map(|s| s.trim().to_string()).filter(|s| !s.is_empty() && s != "origin/HEAD");
    // The remotes' branches only: one's own are never noise.
    let refs = git(root, &["for-each-ref", "--format=%(refname:short)%09%(objectname)%09%(refname)", "refs/remotes"]).unwrap_or_default();
    let mut found = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for line in refs.lines() {
        let mut parts = line.split('\t');
        let (Some(name), Some(oid), Some(full)) = (parts.next(), parts.next(), parts.next()) else { continue };
        if full.ends_with("/HEAD") || Some(name) == base.as_deref() {
            continue;
        }
        let key = (name.to_string(), oid.to_string());
        seen.insert(key.clone());
        let has = *known.entry(key).or_insert_with(|| match &base {
            Some(base) => git(root, &["rev-list", "--count", "--no-merges", &format!("{base}..{oid}")]).is_some_and(|n| n.trim() != "0"),
            None => true,
        });
        found.push((full.to_string(), has));
    }
    known.retain(|key, _| seen.contains(key));
    found
}

/// The branches, local and of the remotes, whose history holds `hash`.
pub fn branches_containing(root: &Path, hash: &str) -> Vec<String> {
    git(root, &["for-each-ref", "--contains", hash, "--format=%(refname:short)%09%(refname)", "refs/heads", "refs/remotes"])
        .unwrap_or_default()
        .lines()
        .filter_map(|line| line.split_once('\t'))
        .filter(|(_, full)| !full.ends_with("/HEAD"))
        .map(|(name, _)| name.to_string())
        .collect()
}

/// Prefixes under which a remote has many branches (`tenant/`, with how
/// many): families of branches the graph may be asked to leave out.
pub fn branch_families(root: &Path, at_least: usize) -> Vec<(String, usize)> {
    let refs = git(root, &["for-each-ref", "--format=%(refname:lstrip=3)", "refs/remotes"]).unwrap_or_default();
    let mut counts: std::collections::BTreeMap<&str, usize> = std::collections::BTreeMap::new();
    for name in refs.lines() {
        if let Some((prefix, _)) = name.split_once('/') {
            *counts.entry(prefix).or_default() += 1;
        }
    }
    let mut families: Vec<(String, usize)> = counts.into_iter().filter(|(_, n)| *n >= at_least).map(|(p, n)| (format!("{p}/*"), n)).collect();
    families.sort_by(|a, b| b.1.cmp(&a.1));
    families
}

/// The newest `limit` commits of every branch, local and of the remotes
/// (tags only as names on them), newest first, with their parents.
pub fn graph(root: &Path, limit: usize, filter: &GraphFilter, known: &mut std::collections::HashMap<(String, String), bool>) -> Vec<GraphCommit> {
    let limit = format!("-n{limit}");
    let mut args: Vec<String> = vec!["log".into(), limit, "--date-order".into(), "--format=%H%x1f%h%x1f%an%x1f%ar%x1f%s%x1f%D%x1f%P%x1e".into()];
    // Branches without work of their own, left out one by one.
    let idle: Vec<String> = if filter.own_only { branches_with_work(root, known).into_iter().filter(|(_, has)| !has).map(|(full, _)| full).collect() } else { Vec::new() };
    // An exclusion holds for the next --branches or --remotes only.
    for (kind, prefix) in [("--branches", "refs/heads/"), ("--remotes", "refs/remotes/")] {
        for glob in &filter.hide {
            args.push(format!("--exclude={}", if kind == "--remotes" { format!("*/{glob}") } else { glob.clone() }));
        }
        args.extend(idle.iter().filter_map(|full| full.strip_prefix(prefix)).map(|name| format!("--exclude={name}")));
        args.push(kind.into());
    }
    // Nor their names on the commits that are listed for other branches.
    for glob in &filter.hide {
        args.push(format!("--decorate-refs-exclude=refs/heads/{glob}"));
        args.push(format!("--decorate-refs-exclude=refs/remotes/*/{glob}"));
    }
    args.extend(idle.iter().map(|full| format!("--decorate-refs-exclude={full}")));
    args.extend(["HEAD".into(), "--".into()]);
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let out = git(root, &args).unwrap_or_default();
    out.split('\x1e')
        .filter_map(|record| {
            let mut fields = record.trim_start_matches('\n').split('\x1f');
            let commit = Commit {
                hash: fields.next().filter(|h| !h.is_empty())?.to_string(),
                short: fields.next()?.to_string(),
                author: fields.next()?.to_string(),
                date: fields.next()?.to_string(),
                subject: fields.next().unwrap_or_default().to_string(),
                refs: fields.next().unwrap_or_default().split(", ").map(|name| name.trim_start_matches("HEAD -> ")).filter(|name| !name.is_empty() && *name != "HEAD" && !name.ends_with("/HEAD")).collect::<Vec<_>>().join(", "),
                merge: false,
            };
            let parents: Vec<String> = fields.next().unwrap_or_default().split_whitespace().map(str::to_string).collect();
            let commit = Commit { merge: parents.len() > 1, ..commit };
            Some(GraphCommit { commit, parents })
        })
        .collect()
}

/// The patch a commit introduced, against its first parent.
/// A commit's message past its subject, trailing blank lines left out.
pub fn commit_body(root: &Path, hash: &str) -> String {
    git(root, &["show", "-s", "--format=%b", hash, "--"]).map(|b| b.trim_end().to_string()).unwrap_or_default()
}

pub fn commit_patch(root: &Path, hash: &str) -> String {
    let mut args = vec!["show", "--format=", "--patch", "--first-parent", "-m"];
    args.extend(diff_args());
    args.extend_from_slice(&[hash, "--"]);
    git(root, &args).unwrap_or_default()
}

/// Index (staged) and worktree (unstaged) state per path.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct FileState {
    pub staged: bool,
    pub unstaged: bool,
    pub change: Change,
}

/// What became of a file since the last commit, as the tree colors it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Change {
    #[default]
    Modified,
    /// New, and added to the index.
    Added,
    /// New, and not added.
    Untracked,
    Conflict,
    /// Gone from disk, git has it still.
    Deleted,
}

/// Runs a command in `root` for a user action: stdout on success, stderr (or
/// stdout) as the error.
pub fn run(root: &Path, program: &str, args: &[&str]) -> Result<String, String> {
    if crate::hosts::is_remote(root) {
        let env: &[(&str, &str)] = if program == "git" { &[("GIT_TERMINAL_PROMPT", "0")] } else { &[] };
        let out = crate::hostfs::run(root, program, args, env, None).map_err(|e| format!("{program}: {e}"))?;
        let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
        let stderr = out.stderr.trim().to_string();
        return if out.success() { Ok(if stdout.is_empty() { stderr } else { stdout }) } else { Err(if stderr.is_empty() { stdout } else { stderr }) };
    }
    let mut command = crate::procs::command(program);
    command.current_dir(root).args(args).stdin(std::process::Stdio::null());
    if program == "git" {
        // Started from a terminal, git or ssh would ask for a password there and
        // wait for ever: git asks no one, and ssh, with no terminal of its own,
        // asks through SSH_ASKPASS when there is one or fails and says so.
        command.env("GIT_TERMINAL_PROMPT", "0");
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt as _;
            // SAFETY: setsid is async-signal-safe and touches nothing of the parent.
            unsafe { command.pre_exec(|| { libc::setsid(); Ok(()) }) };
        }
    }
    let output = command.output().map_err(|e| format!("{program}: {e}"))?;
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if output.status.success() {
        Ok(if stdout.is_empty() { stderr } else { stdout })
    } else {
        Err(if stderr.is_empty() { stdout } else { stderr })
    }
}

/// A commit message for what would be committed (the staged changes, or
/// all of them), written by the agent for quick questions (`claude -p`…)
/// in the style of the latest commits.
pub fn suggest_message(root: &Path, staged_only: bool) -> Result<String, String> {
    use std::io::Write as _;
    let Some(profile) = crate::state::with_settings(|s| s.quick_profile().cloned()) else {
        return Err("No agent to write it: add one in Settings → Agents".into());
    };
    let Some(asked) = profile.adapter().one_shot(&profile, &crate::agents::Ask::Quick) else {
        return Err(format!("{} cannot answer one question: choose another agent for quick questions in Settings → Agents", profile.name));
    };
    const MAX_DIFF: usize = 60_000;
    let mut diff = if staged_only { git(root, &["diff", "--cached"]).unwrap_or_default() } else { working_patch(root) };
    if diff.trim().is_empty() {
        return Err("Nothing to describe".into());
    }
    if diff.len() > MAX_DIFF {
        let mut cut = MAX_DIFF;
        while !diff.is_char_boundary(cut) {
            cut -= 1;
        }
        diff.truncate(cut);
        diff.push_str("\n[… the rest of the diff is left out]\n");
    }
    let log = git(root, &["log", "-12", "--format=%B%x1e"]).unwrap_or_default();
    let examples: Vec<&str> = log.split('\x1e').map(str::trim).filter(|m| !m.is_empty()).collect();
    let prompt = format!(
        "Write the git commit message for the diff below. Match the recent commits of this \
         repository: language, length and form of the subject, whether there is a body and how it \
         reads. Leave out trailers such as Co-Authored-By. Answer with the message only, no quotes, \
         no code fences.\n\nRecent commits, separated by ---:\n{}\n\nThe diff:\n{diff}",
        examples.join("\n---\n")
    );
    let name = profile.name.clone();
    let mut child = asked
        .command()
        .current_dir(root)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("{name}: {e}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(prompt.as_bytes());
    }
    let out = child.wait_with_output().map_err(|e| format!("{name}: {e}"))?;
    let text = asked.read_answer(&String::from_utf8_lossy(&out.stdout)).0.trim_matches('`').trim().to_string();
    if out.status.success() && !text.is_empty() {
        Ok(text)
    } else {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        Err(if err.is_empty() { format!("{name} gave no message") } else { err })
    }
}

pub fn stage(root: &Path, paths: &[String]) -> Result<String, String> {
    let mut args = vec!["add", "--"];
    args.extend(paths.iter().map(String::as_str));
    run(root, "git", &args)
}

pub fn unstage(root: &Path, paths: &[String]) -> Result<String, String> {
    let mut args = vec!["restore", "--staged", "--"];
    args.extend(paths.iter().map(String::as_str));
    run(root, "git", &args)
}

/// Commits what is staged; with nothing staged, everything (`git add -A`
/// first). Asked of git here: what the window last read may be from before
/// a change was staged.
pub fn commit(root: &Path, message: &str, amend: bool) -> Result<String, String> {
    if git(root, &["diff", "--cached", "--name-only"]).is_some_and(|staged| staged.trim().is_empty()) {
        run(root, "git", &["add", "-A"])?;
    }
    if amend {
        run(root, "git", &["commit", "--amend", "-m", message])
    } else {
        run(root, "git", &["commit", "-m", message])
    }
}

/// Whether the last commit is on the remote: in the upstream (nothing
/// ahead of it), or, for a branch not published, in the branch work starts
/// from. With neither it is nowhere but here.
pub fn head_pushed(has_upstream: bool, ahead: u32, has_base: bool, ahead_of_base: u32) -> bool {
    if has_upstream {
        ahead == 0
    } else {
        has_base && ahead_of_base == 0
    }
}

/// Takes the last commit back: its changes are staged again, as they were
/// before it was made, and nothing in the files changes. For a commit that
/// is nowhere but here; `hash` is the one the asker saw, and another in
/// its place is left alone. Gives a line to tell, then the commit's message.
pub fn undo_commit(root: &Path, hash: &str) -> Result<String, String> {
    let head = git(root, &["rev-parse", "HEAD"]).unwrap_or_default();
    if hash.is_empty() || !head.trim().starts_with(hash) {
        return Err("the last commit is another one now".into());
    }
    if git(root, &["rev-parse", "--verify", "--quiet", "HEAD~1"]).is_none_or(|parent| parent.trim().is_empty()) {
        return Err("it is the first commit: there is nothing to go back to".into());
    }
    let message = last_message(root).unwrap_or_default();
    run(root, "git", &["reset", "--soft", "HEAD~1"])?;
    Ok(format!("Undone {hash}: its changes are staged again\n{message}"))
}

/// The whole message of the last commit.
pub fn last_message(root: &Path) -> Option<String> {
    git(root, &["log", "-1", "--format=%B"]).map(|m| m.trim_end().to_string())
}

/// A push the remote turned down because it has commits this branch has
/// not, as git words it; not one that failed for any other reason.
pub fn push_refused(error: &str) -> bool {
    error.lines().any(|line| {
        let line = line.trim();
        (line.starts_with("! [rejected]") && ["(non-fast-forward)", "(fetch first)", "(stale info)"].iter().any(|why| line.ends_with(why)))
            || line.starts_with("Updates were rejected because the tip of your current branch is behind")
            || line.starts_with("Updates were rejected because the remote contains work")
    })
}

/// Where a branch pushes: the remote, and the branch there.
#[derive(Clone, Debug, PartialEq)]
pub struct PushTarget {
    pub remote: String,
    pub branch: String,
}

impl PushTarget {
    /// `origin/feature`.
    pub fn tracking(&self) -> String {
        format!("{}/{}", self.remote, self.branch)
    }
}

/// The branch's upstream, or `origin/<branch>` where a first push would go.
pub fn push_target(root: &Path) -> Option<PushTarget> {
    let head = git(root, &["symbolic-ref", "--short", "HEAD"])?.trim().to_string();
    let config = |key: &str| git(root, &["config", "--get", &format!("branch.{head}.{key}")]).map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
    match (config("remote"), config("merge")) {
        (Some(remote), Some(merge)) if remote != "." => Some(PushTarget { remote, branch: merge.strip_prefix("refs/heads/").unwrap_or(&merge).to_string() }),
        _ => Some(PushTarget { remote: "origin".into(), branch: head }),
    }
}

/// A push turned down: what the remote has that is not here.
#[derive(Clone, Debug)]
pub struct Diverged {
    pub target: PushTarget,
    /// The commit the remote branch is at, as fetched just now: an
    /// overwrite replaces that one and no other.
    pub remote_hash: String,
    /// The remote's commits not here, as "abc1234 subject (author)".
    pub theirs: Vec<String>,
    /// Why it is not to be overwritten, when it is not.
    pub keep: Option<String>,
}

/// Whether a branch is one not to overwrite: the repository's default
/// one, or one others have pushed to (`authors` of the commits that would
/// go; `me` the user's email).
pub fn keep_reason(branch: &str, default: Option<&str>, authors: &[(String, String)], me: Option<&str>) -> Option<String> {
    if default == Some(branch) || ["main", "master"].contains(&branch) {
        return Some(format!("{branch} is the repository's main branch"));
    }
    let others: std::collections::BTreeSet<&str> = authors.iter().filter(|(_, email)| me.is_none_or(|me| !email.eq_ignore_ascii_case(me))).map(|(name, _)| name.as_str()).collect();
    if !others.is_empty() {
        return Some(format!("the remote has commits by {}", others.into_iter().collect::<Vec<_>>().join(", ")));
    }
    None
}

/// Fetches the branch a push was turned down by, and tells what it has.
pub fn diverged(root: &Path) -> Result<Diverged, String> {
    let target = push_target(root).ok_or("not on a branch")?;
    let _ = fetch_quietly(root, &["fetch", "--quiet", "--no-auto-maintenance", &target.remote, &target.branch]);
    let tracking = target.tracking();
    let remote_hash = git(root, &["rev-parse", "--verify", "-q", &format!("refs/remotes/{tracking}")]).map(|h| h.trim().to_string()).filter(|h| !h.is_empty()).ok_or_else(|| format!("{tracking} could not be read"))?;
    let log = git(root, &["log", "--format=%h %s%x1f%an%x1f%ae", &format!("HEAD..{remote_hash}")]).unwrap_or_default();
    let mut theirs = Vec::new();
    let mut authors = Vec::new();
    for line in log.lines() {
        let mut parts = line.split('\x1f');
        let (what, name, email) = (parts.next().unwrap_or_default(), parts.next().unwrap_or_default(), parts.next().unwrap_or_default());
        theirs.push(format!("{what} ({name})"));
        authors.push((name.to_string(), email.to_string()));
    }
    let me = git(root, &["config", "user.email"]).map(|m| m.trim().to_string());
    let keep = keep_reason(&target.branch, remote_default(root).as_deref(), &authors, me.as_deref());
    Ok(Diverged { target, remote_hash, theirs, keep })
}

/// Pushes; a push turned down for what the remote has comes back as what
/// it has, to be pulled first or overwritten.
pub fn push_or_tell(root: &Path, has_upstream: bool) -> Result<String, Result<Diverged, String>> {
    match push(root, has_upstream) {
        Ok(out) => Ok(out),
        Err(error) if push_refused(&error) => Err(diverged(root).map_err(|why| format!("{}: {why}", first_line(&error)))),
        Err(error) => Err(Err(error)),
    }
}

fn first_line(text: &str) -> &str {
    text.lines().find(|line| !line.trim().is_empty()).unwrap_or_default()
}

/// Brings in what the remote branch has (a rebase of the commits here onto
/// it, as `git pull --rebase` does), then pushes. A conflict undoes the
/// rebase: the branch is as it was, and the files that conflict are told.
pub fn pull_and_push(root: &Path, has_upstream: bool) -> Result<String, RebaseError> {
    let target = push_target(root).ok_or_else(|| RebaseError::Failed("not on a branch".into()))?;
    run(root, "git", &["fetch", &target.remote, &target.branch]).map_err(|e| RebaseError::Failed(first_line(&e).to_string()))?;
    let theirs = target.tracking();
    if let Some(files) = merge_conflicts(root, None, &theirs).filter(|files| !files.is_empty()) {
        return Err(RebaseError::Conflicts(files));
    }
    if let Err(e) = run(root, "git", &["rebase", "--autostash", &theirs]) {
        let conflicts: Vec<String> = git(root, &["diff", "--name-only", "--diff-filter=U"]).unwrap_or_default().lines().map(str::to_string).collect();
        let _ = run(root, "git", &["rebase", "--abort"]);
        return Err(if conflicts.is_empty() { RebaseError::Failed(first_line(&e).to_string()) } else { RebaseError::Conflicts(conflicts) });
    }
    push(root, has_upstream).map_err(|e| RebaseError::Failed(first_line(&e).to_string()))?;
    Ok(format!("Rebased onto {theirs} and pushed"))
}

/// Pushes over what the remote branch has, as long as it is still at the
/// commit that was shown (`--force-with-lease` with it): never a bare
/// `--force`.
pub fn push_overwrite(root: &Path, diverged: &Diverged) -> Result<String, String> {
    let Diverged { target, remote_hash, keep, .. } = diverged;
    if let Some(why) = keep {
        return Err(format!("not overwritten: {why}"));
    }
    let lease = format!("--force-with-lease=refs/heads/{}:{remote_hash}", target.branch);
    let refspec = format!("HEAD:refs/heads/{}", target.branch);
    run(root, "git", &["push", "--set-upstream", &lease, &target.remote, &refspec])?;
    Ok(format!("Overwrote {}", target.tracking()))
}

/// Pushes, setting the upstream on the first push of a branch.
pub fn push(root: &Path, has_upstream: bool) -> Result<String, String> {
    if has_upstream {
        run(root, "git", &["push"])
    } else {
        run(root, "git", &["push", "-u", "origin", "HEAD"])
    }
}

/// The branch's pull request (GitHub) or merge request (GitLab), made
/// from its commits if there is none: pushed where the checkout is, asked
/// for here, where gh or glab has the user's sign-in. As `gh pr create
/// --fill` does, one commit gives its subject and body, several the
/// branch's name and their subjects.
/// What a request says of its task's tickets: its key for the title, and
/// a line for each (`Fixes DEV-1`, `Refs DEV-1`), none when `closes` is none.
#[derive(Clone, Debug, Default)]
pub struct RequestText {
    pub title_template: String,
    pub key: String,
    pub tickets: Vec<(String, String)>,
    pub closes: String,
}

impl RequestText {
    pub fn title(&self, title: &str) -> String {
        crate::layers::request_title(&self.title_template, &self.key, title)
    }

    pub fn body(&self, body: &str) -> String {
        let word = match self.closes.as_str() {
            "fixes" => "Fixes",
            "refs" => "Refs",
            _ => return body.to_string(),
        };
        let lines: Vec<String> = self.tickets.iter().map(|(key, url)| if url.is_empty() { format!("{word} {key}") } else { format!("{word} {key} ({url})") }).collect();
        if lines.is_empty() {
            return body.to_string();
        }
        let body = body.trim_end();
        if body.is_empty() { lines.join("\n") } else { format!("{body}\n\n{}", lines.join("\n")) }
    }
}

pub fn pull_request(root: &Path, has_upstream: bool, text: &RequestText) -> Result<String, String> {
    let host = crate::integrations::code_host(root).map_err(|e| e.to_string())?;
    let branch = run(root, "git", &["rev-parse", "--abbrev-ref", "HEAD"])?;
    if let Ok(Some(url)) = host.existing_pr(root, &branch) {
        return Ok(url);
    }
    push(root, has_upstream)?;
    let base = base_ref(root).unwrap_or_else(|| "origin/main".into());
    let log = run(root, "git", &["log", "--reverse", "--format=%s%x1f%b%x1e", &format!("{base}..HEAD")])?;
    let commits: Vec<(&str, &str)> = log.split('\x1e').map(str::trim).filter(|c| !c.is_empty()).filter_map(|c| c.split_once('\x1f')).collect();
    let (title, body) = match commits.as_slice() {
        [(subject, body)] => (subject.to_string(), body.trim().to_string()),
        many => (branch.clone(), many.iter().map(|(subject, _)| format!("- {subject}")).collect::<Vec<_>>().join("\n")),
    };
    let into = base.split_once('/').map_or(base.as_str(), |(_, b)| b);
    host.create_pr(root, &branch, into, &text.title(&title), &text.body(&body)).map_err(|e| e.to_string())
}

/// The directories to watch so that any change to the worktree or its git
/// state is seen: every directory holding a listed file, the worktree root,
/// the git dir (HEAD, index) and the shared refs.
pub fn base_ref(root: &Path) -> Option<String> {
    let nonempty = |out: Option<String>| out.map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    if let Some(r) = nonempty(git(root, &["rev-parse", "--abbrev-ref", "origin/HEAD"])) {
        if r != "origin/HEAD" {
            return Some(r);
        }
    }
    ["origin/main", "origin/master", "main", "master"]
        .into_iter()
        .find(|c| nonempty(git(root, &["rev-parse", "--verify", "-q", c])).is_some())
        .map(str::to_string)
}

/// Everything the task changed: from where it forked off `base` to the
/// worktree as it is now, uncommitted work included.
pub fn touched_files(root: &Path, base: &str) -> Option<std::collections::BTreeSet<String>> {
    let fork = git(root, &["merge-base", "HEAD", base]).map(|s| s.trim().to_string()).filter(|s| !s.is_empty())?;
    let changed = git(root, &["diff", "--name-only", "--no-renames", "-z", &fork])?;
    let untracked = git(root, &["ls-files", "--others", "--exclude-standard", "-z"]).unwrap_or_default();
    Some(
        changed
            .split('\0')
            .chain(untracked.split('\0'))
            .filter(|f| !f.is_empty() && !is_lockfile(f))
            .map(str::to_string)
            .collect(),
    )
}

pub fn is_lockfile(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    name.ends_with(".lock") || matches!(name, "package-lock.json" | "pnpm-lock.yaml" | "go.sum")
}

#[derive(Clone, Debug)]
pub struct BlameLine {
    pub hash: String,
    pub author: String,
    /// Unix seconds.
    pub time: i64,
    pub summary: String,
}

impl BlameLine {
    pub fn uncommitted(&self) -> bool {
        self.hash.bytes().all(|b| b == b'0')
    }
}

/// Who last changed each line (index 0 is line 1), by `git blame --porcelain`.
pub fn blame(root: &Path, file: &Path) -> Vec<Option<std::sync::Arc<BlameLine>>> {
    // Whitespace and line endings are no change of who wrote a line.
    let mut args = vec!["blame", "--porcelain", "-w"];
    // Commits that only reformatted, as GitHub's blame leaves them out.
    if crate::hostfs::stat(&root.join(".git-blame-ignore-revs")).is_ok_and(|s| s.file) {
        args.extend(["--ignore-revs-file", ".git-blame-ignore-revs"]);
    }
    // A remote task's file by its place in the worktree: its key is no path there.
    let file = if crate::hosts::is_remote(root) { file.strip_prefix(root).unwrap_or(file) } else { file };
    let file = file.to_string_lossy();
    args.extend(["--", &file]);
    let Some(out) = git(root, &args) else {
        return Vec::new();
    };
    // A commit is told once and shared by its lines: a file has thousands
    // of lines and a handful of commits.
    let mut index: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    let mut commits: Vec<BlameLine> = Vec::new();
    let mut lines: Vec<Option<usize>> = Vec::new();
    let mut current: Option<(usize, usize)> = None;
    for line in out.lines() {
        if line.starts_with('\t') {
            if let Some((commit, final_line)) = current.take() {
                if final_line > 0 {
                    if lines.len() < final_line {
                        lines.resize(final_line, None);
                    }
                    lines[final_line - 1] = Some(commit);
                }
            }
            continue;
        }
        let mut parts = line.split(' ');
        let first = parts.next().unwrap_or_default();
        if first.len() == 40 && first.bytes().all(|b| b.is_ascii_hexdigit()) {
            let final_line = parts.nth(1).and_then(|n| n.parse().ok()).unwrap_or(0);
            let commit = *index.entry(first).or_insert_with(|| {
                commits.push(BlameLine { hash: first.to_string(), author: String::new(), time: 0, summary: String::new() });
                commits.len() - 1
            });
            current = Some((commit, final_line));
            continue;
        }
        let Some(entry) = current.and_then(|(commit, _)| commits.get_mut(commit)) else { continue };
        let rest = line.split_once(' ').map(|(_, r)| r).unwrap_or_default();
        match first {
            "author" => entry.author = rest.to_string(),
            "author-time" => entry.time = rest.parse().unwrap_or(0),
            "summary" => entry.summary = rest.to_string(),
            _ => {}
        }
    }
    let commits: Vec<std::sync::Arc<BlameLine>> = commits.into_iter().map(std::sync::Arc::new).collect();
    lines.into_iter().map(|commit| commit.map(|commit| commits[commit].clone())).collect()
}

/// One commit's list entry, for a commit outside the loaded history.
pub fn commit_info(root: &Path, hash: &str) -> Option<Commit> {
    let out = git(root, &["log", "-1", "--format=%H%x1f%h%x1f%an%x1f%ar%x1f%s%x1f%P", hash])?;
    let mut fields = out.trim_end().split('\x1f');
    Some(Commit {
        hash: fields.next().filter(|h| !h.is_empty())?.to_string(),
        short: fields.next()?.to_string(),
        author: fields.next()?.to_string(),
        date: fields.next()?.to_string(),
        subject: fields.next().unwrap_or_default().to_string(),
        refs: String::new(),
        merge: fields.next().is_some_and(|parents| parents.split_whitespace().count() > 1),
    })
}

/// "3 days ago" for a unix time.
pub fn ago(time: i64) -> String {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64);
    let secs = (now - time).max(0);
    let (n, unit) = match secs {
        s if s < 60 => return "just now".into(),
        s if s < 3600 => (s / 60, "minute"),
        s if s < 86400 => (s / 3600, "hour"),
        s if s < 86400 * 30 => (s / 86400, "day"),
        s if s < 86400 * 365 => (s / (86400 * 30), "month"),
        s => (s / (86400 * 365), "year"),
    };
    format!("{n} {unit}{} ago", if n == 1 { "" } else { "s" })
}

/// Throws away every change to `path`: back to HEAD, staged or not; a file
/// HEAD does not have is deleted.
pub fn discard(root: &Path, path: &str) -> Result<String, String> {
    let in_head = run(root, "git", &["cat-file", "-e", &format!("HEAD:{path}")]).is_ok();
    if in_head {
        run(root, "git", &["restore", "--source=HEAD", "--staged", "--worktree", "--", path])
    } else {
        let _ = run(root, "git", &["rm", "--cached", "-q", "--ignore-unmatch", "--", path]);
        std::fs::remove_file(root.join(path)).map_err(|e| e.to_string())?;
        Ok(format!("deleted {path}"))
    }
}

/// A file as it was in the worktree and the index, to put back after a
/// discard. The index holds a blob that is in the object store already.
#[derive(Clone, Debug)]
pub struct FileSnapshot {
    pub path: String,
    worktree: Option<Vec<u8>>,
    /// "mode,sha" of its index entry.
    index: Option<String>,
}

pub fn snapshot(root: &Path, path: &str) -> FileSnapshot {
    let index = git(root, &["ls-files", "-s", "--", path]).and_then(|out| {
        let mut f = out.lines().next()?.split_whitespace();
        Some(format!("{},{}", f.next()?, f.next()?))
    });
    FileSnapshot { path: path.to_string(), worktree: std::fs::read(root.join(path)).ok(), index }
}

pub fn restore(root: &Path, snap: &FileSnapshot) -> Result<String, String> {
    let path = &snap.path;
    match &snap.index {
        Some(entry) => run(root, "git", &["update-index", "--add", "--cacheinfo", &format!("{entry},{path}")])?,
        None => run(root, "git", &["rm", "--cached", "-q", "--ignore-unmatch", "--", path])?,
    };
    let file = root.join(path);
    match &snap.worktree {
        Some(bytes) => {
            if let Some(dir) = file.parent() {
                std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
            }
            std::fs::write(&file, bytes).map_err(|e| e.to_string())?;
        }
        None if file.exists() => std::fs::remove_file(&file).map_err(|e| e.to_string())?,
        None => {}
    }
    Ok(format!("Restored {path}"))
}

pub fn fetch(root: &Path) -> Result<String, String> {
    run(root, "git", &["fetch", "--prune"]).map(|out| if out.is_empty() { "Fetched".into() } else { out })
}

/// Brings in the upstream without ever leaving a mess: a fast-forward when
/// there is nothing local, else a rebase that is aborted on any conflict.
/// Uncommitted work is stashed around it either way.
/// Why a rebase onto the base did not happen.
#[derive(Debug)]
pub enum RebaseError {
    /// These files conflict; the rebase was undone.
    Conflicts(Vec<String>),
    Failed(String),
}

/// Takes a commit of elsewhere into this branch (`revert` false), or
/// commits its opposite. On a conflict nothing changes and the files that
/// conflict are told.
pub fn replay_commit(root: &Path, hash: &str, revert: bool) -> Result<String, RebaseError> {
    let (verb, args): (&str, &[&str]) = if revert { ("revert", &["revert", "--no-edit"]) } else { ("cherry-pick", &["cherry-pick"]) };
    let mut all = args.to_vec();
    all.push(hash);
    match run(root, "git", &all) {
        Ok(_) => Ok(if revert { format!("Reverted {}", hash.get(..8).unwrap_or(hash)) } else { format!("Picked {}", hash.get(..8).unwrap_or(hash)) }),
        Err(e) => {
            let conflicts: Vec<String> = git(root, &["diff", "--name-only", "--diff-filter=U"]).unwrap_or_default().lines().map(str::to_string).collect();
            let _ = run(root, "git", &[verb, "--abort"]);
            match conflicts.is_empty() {
                true => Err(RebaseError::Failed(e.lines().find(|line| line.starts_with("error") || line.starts_with("fatal")).unwrap_or(e.lines().next().unwrap_or_default()).to_string())),
                false => Err(RebaseError::Conflicts(conflicts)),
            }
        }
    }
}

/// A worktree with a new branch at a commit, where a new task's would be:
/// the app then shows it as a task.
pub fn commit_worktree(root: &Path, hash: &str, copy_env: bool) -> Result<PathBuf, String> {
    let repo = main_worktree(root).ok_or("not a git repository")?;
    worktree_at(&repo, &format!("from-{}", hash.get(..8).unwrap_or(hash)), hash, copy_env)
}

/// Fetches the base and rebases the branch onto it, uncommitted changes
/// stashed and put back. On a conflict nothing changes.
pub fn rebase_onto(root: &Path, base: &str) -> Result<String, RebaseError> {
    if let Some((remote, branch)) = base.split_once('/') {
        let _ = run(root, "git", &["fetch", remote, branch]);
    }
    // What is not committed is set aside and put back after: where it
    // would not fit back, git leaves its conflict in the files, under an
    // agent that may be writing them. Tried on trees first, and not begun.
    if worktree_commit(root).is_some_and(|commit| commit != "HEAD") {
        if let Some(files) = merge_conflicts(root, None, base).filter(|files| !files.is_empty()) {
            return Err(RebaseError::Conflicts(files));
        }
    }
    match run(root, "git", &["rebase", "--autostash", base]) {
        Ok(_) => Ok(format!("Rebased onto {base}")),
        Err(e) => {
            let conflicts: Vec<String> = git(root, &["diff", "--name-only", "--diff-filter=U"])
                .unwrap_or_default()
                .lines()
                .map(str::to_string)
                .collect();
            let _ = run(root, "git", &["rebase", "--abort"]);
            if conflicts.is_empty() {
                Err(RebaseError::Failed(e.lines().next().unwrap_or_default().to_string()))
            } else {
                Err(RebaseError::Conflicts(conflicts))
            }
        }
    }
}

/// What the worktree `theirs` has that `ours` has not, in the files named:
/// a patch as `git diff` makes one, of two folders as they are on disk,
/// files no commit knows of among them. Files the same in both are left out.
pub fn between(ours: &Path, theirs: &Path, files: &[String]) -> String {
    let mut patch = String::new();
    for file in files {
        let (a, b) = (std::fs::read(ours.join(file)).ok(), std::fs::read(theirs.join(file)).ok());
        if a == b {
            continue;
        }
        let text = |side: &Option<Vec<u8>>| String::from_utf8(side.clone().unwrap_or_default()).ok().filter(|text| !text.contains('\0'));
        let (old, new) = (text(&a), text(&b));
        // Whitespace left out: git tells what else differs, or that nothing does.
        if ignore_whitespace() && a.is_some() && b.is_some() && old.is_some() && new.is_some() {
            let (left, right) = (ours.join(file), theirs.join(file));
            let args = ["diff", "--no-index", "--no-color", "--no-ext-diff", "--ignore-all-space", "--ignore-blank-lines", "--unified=3"];
            let out = git(ours, &[&args[..], &[&*left.to_string_lossy(), &*right.to_string_lossy()]].concat()).unwrap_or_default();
            if let Some(at) = out.find("\n@@") {
                patch.push_str(&format!("diff --git a/{file} b/{file}\n--- a/{file}\n+++ b/{file}\n{}", &out[at + 1..]));
            }
            continue;
        }
        patch.push_str(&format!("diff --git a/{file} b/{file}\n"));
        match (&a, &b) {
            (None, _) => patch.push_str("new file mode 100644\n"),
            (_, None) => patch.push_str("deleted file mode 100644\n"),
            _ => {}
        }
        let (Some(old), Some(new)) = (old, new) else {
            patch.push_str(&format!("Binary files a/{file} and b/{file} differ\n"));
            continue;
        };
        let (from, to) = (if a.is_some() { format!("a/{file}") } else { "/dev/null".into() }, if b.is_some() { format!("b/{file}") } else { "/dev/null".into() });
        let diff = similar::TextDiff::from_lines(&old, &new);
        patch.push_str(&diff.unified_diff().context_radius(3).header(&from, &to).to_string());
    }
    patch
}

/// The files that would be left in conflict by taking in the upstream:
/// the merge is made of trees only, nothing in the worktree or the index
/// moves. What is not committed counts as it is on disk. None when git is
/// too old to tell (before 2.38) or there is no upstream.
pub fn pull_conflicts(root: &Path) -> Option<Vec<String>> {
    merge_conflicts(root, None, "@{u}")
}

/// The same for any commit taken into the worktree; `base` is what both
/// sides are compared with when it is not their common ancestor.
fn merge_conflicts(root: &Path, base: Option<&str>, theirs: &str) -> Option<Vec<String>> {
    let written = worktree_commit(root)?;
    let mine = written.as_str();
    let merge_tree = |extra: &[&str], mine: &str, theirs: &str| {
        let mut merge = crate::procs::command("git");
        merge.current_dir(root).args(["-c", "core.quotePath=false", "merge-tree", "--write-tree", "--name-only", "--no-messages"]);
        merge.args(extra).args([mine, theirs]).output().ok()
    };
    let merged = match base {
        None => merge_tree(&[], mine, theirs)?,
        Some(base) => match merge_tree(&["--merge-base", base], mine, theirs) {
            // Git before 2.40 (macOS's own) has no --merge-base: the two sides
            // made children of the base alone, which is then theirs in common.
            Some(merged) if merged.status.code() != Some(129) => merged,
            _ => merge_tree(&[], &on_base(root, mine, base)?, &on_base(root, theirs, base)?)?,
        },
    };
    match merged.status.code() {
        Some(0) => Some(Vec::new()),
        // The tree it made, then the files it could not merge.
        Some(1) => Some(String::from_utf8_lossy(&merged.stdout).lines().skip(1).filter(|line| !line.is_empty()).map(str::to_string).collect()),
        _ => None,
    }
}

/// `rev`'s files as a commit whose one parent is `base`, kept nowhere.
fn on_base(root: &Path, rev: &str, base: &str) -> Option<String> {
    let made = crate::procs::command("git")
        .current_dir(root)
        .envs([("GIT_AUTHOR_NAME", "ideae"), ("GIT_AUTHOR_EMAIL", "ideae@localhost"), ("GIT_COMMITTER_NAME", "ideae"), ("GIT_COMMITTER_EMAIL", "ideae@localhost")])
        .args(["commit-tree", &format!("{rev}^{{tree}}"), "-p", base, "-m", "ideae"])
        .output()
        .ok()?;
    made.status.success().then(|| String::from_utf8_lossy(&made.stdout).trim().to_string())
}

/// A commit of the worktree as it is, kept nowhere: HEAD when it is clean.
/// Made in an index of its own, a copy of the repository's: `git stash
/// create` takes the index's lock and writes the index, which fails an
/// agent's `git add` at that moment and has every watcher look again.
fn worktree_commit(root: &Path) -> Option<String> {
    worktree_snapshot(root, false)
}

/// `worktree_commit`, with files git does not follow yet when `new` (not
/// those it ignores).
fn worktree_snapshot(root: &Path, new: bool) -> Option<String> {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let git_dir = PathBuf::from(git(root, &["rev-parse", "--absolute-git-dir"])?.trim());
    let index = std::env::temp_dir().join(format!("ideae-index-{}-{}", std::process::id(), NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)));
    std::fs::copy(git_dir.join("index"), &index).ok()?;
    let run = |args: &[&str]| {
        let done = crate::procs::command("git")
            .current_dir(root)
            .env("GIT_INDEX_FILE", &index)
            // Whoever the user is to git, or nobody: the commit is shown to no one.
            .envs([("GIT_AUTHOR_NAME", "ideae"), ("GIT_AUTHOR_EMAIL", "ideae@localhost"), ("GIT_COMMITTER_NAME", "ideae"), ("GIT_COMMITTER_EMAIL", "ideae@localhost")])
            .args(args)
            .output()
            .ok()
            .filter(|done| done.status.success())?;
        Some(String::from_utf8_lossy(&done.stdout).trim().to_string())
    };
    let made = (|| {
        run(&["add", if new { "-A" } else { "-u" }])?;
        let tree = run(&["write-tree"])?;
        if run(&["rev-parse", "HEAD^{tree}"])? == tree {
            return Some("HEAD".to_string());
        }
        run(&["commit-tree", &tree, "-p", "HEAD", "-m", "worktree"])
    })();
    let _ = std::fs::remove_file(&index);
    made
}

/// The files of a patch that have a conflict left in them, as markers at
/// the start of added lines, and the line of the first.
pub fn conflict_marked(patch: &str) -> Vec<(String, u32)> {
    let mut found: Vec<(String, u32)> = Vec::new();
    let (mut path, mut line, mut opened) = (String::new(), 0u32, None);
    for text in patch.lines() {
        if text.starts_with("diff --git ") {
            (path, opened) = (String::new(), None);
        } else if let Some(name) = text.strip_prefix("+++ b/") {
            path = name.trim_end_matches('\t').to_string();
        } else if let Some(at) = text.strip_prefix("@@ -") {
            line = at.split(" +").nth(1).and_then(|new| new.split([',', ' ']).next()).and_then(|n| n.parse().ok()).unwrap_or(0);
        } else if text.starts_with('+') {
            if text.starts_with("+<<<<<<< ") {
                opened = Some(line);
            } else if text.starts_with("+>>>>>>> ") {
                if let Some(at) = opened.take().filter(|_| !path.is_empty() && found.last().is_none_or(|(last, _)| *last != path)) {
                    found.push((path.clone(), at));
                }
            }
            line += 1;
        } else if text.starts_with(' ') {
            line += 1;
        }
    }
    found
}

pub fn pull_safe(root: &Path) -> Result<String, String> {
    run(root, "git", &["fetch", "--prune"])?;
    let ahead = run(root, "git", &["rev-list", "--count", "@{u}..HEAD"])?.trim().parse::<u32>().unwrap_or(0);
    let behind = run(root, "git", &["rev-list", "--count", "HEAD..@{u}"])?.trim().parse::<u32>().unwrap_or(0);
    if behind == 0 {
        return Ok("Already up to date".into());
    }
    // Not tried at all then: what is written but not committed would come
    // back out of the stash with the conflict in it, and nothing undoes that.
    if let Some(files) = pull_conflicts(root).filter(|files| !files.is_empty()) {
        return Err(format!("would conflict in {}, nothing changed", files.join(", ")));
    }
    if ahead == 0 {
        run(root, "git", &["merge", "--ff-only", "--autostash", "@{u}"])?;
        return Ok(format!("Fast-forwarded {behind} commit{}", if behind == 1 { "" } else { "s" }));
    }
    match run(root, "git", &["rebase", "--autostash", "@{u}"]) {
        Ok(_) => Ok(format!("Rebased {ahead} local commit{} onto {behind} new", if ahead == 1 { "" } else { "s" })),
        Err(e) => {
            let _ = run(root, "git", &["rebase", "--abort"]);
            Err(format!("conflicts, nothing changed: {}", e.lines().next().unwrap_or_default()))
        }
    }
}

#[cfg(test)]
mod snapshot_tests {
    use super::{discard, restore, run, snapshot};

    #[test]
    fn discard_then_restore_brings_back_worktree_and_index() {
        let root = std::env::temp_dir().join(format!("ideae-snap-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let git = |args: &[&str]| run(&root, "git", args).unwrap();
        git(&["init", "-q"]);
        // Windows runners have core.autocrlf on, which would check out "one\r\n".
        git(&["config", "core.autocrlf", "false"]);
        std::fs::write(root.join("a.txt"), "one\n").unwrap();
        git(&["add", "a.txt"]);
        git(&["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-qm", "init"]);
        std::fs::write(root.join("a.txt"), "staged\n").unwrap();
        git(&["add", "a.txt"]);
        std::fs::write(root.join("a.txt"), "worktree\n").unwrap();
        std::fs::write(root.join("new.txt"), "fresh\n").unwrap();
        let (a, new) = (snapshot(&root, "a.txt"), snapshot(&root, "new.txt"));
        discard(&root, "a.txt").unwrap();
        discard(&root, "new.txt").unwrap();
        assert_eq!(std::fs::read_to_string(root.join("a.txt")).unwrap(), "one\n");
        assert!(!root.join("new.txt").exists());
        restore(&root, &a).unwrap();
        restore(&root, &new).unwrap();
        assert_eq!(std::fs::read_to_string(root.join("a.txt")).unwrap(), "worktree\n");
        assert_eq!(git(&["show", ":a.txt"]), "staged");
        assert_eq!(std::fs::read_to_string(root.join("new.txt")).unwrap(), "fresh\n");
        std::fs::remove_dir_all(&root).unwrap();
    }
}

#[cfg(test)]
mod lock_tests {
    use super::*;

    #[test]
    fn lock_owner_and_liveness() {
        let me = std::process::id();
        assert_eq!(lock_owner("claude agent agent-a3 (pid 1532754 start 82664722)"), Some((1532754, Some(82664722))));
        assert_eq!(lock_owner("moved to usb disk"), None);
        assert!(process_alive(me, None));
        if cfg!(target_os = "linux") {
            assert!(!process_alive(me, Some(1)));
        }
        assert!(!process_alive(u32::MAX - 1, None));
    }
}

// They script repositories with bash, which Windows lacks or has as WSL's.
#[cfg(all(test, unix))]
mod pull_tests {
    use super::{commit_worktree, history, replay_commit, HistoryQuery, RebaseError, branch_rows, branch_worktree, delete_branch, between, conflict_marked, diff_against, hunks, pull_conflicts, pull_safe, refresh, refs, stash, stash_apply, stash_drop, stash_patch, working_patch, Known};
    use std::{path::Path, process::Command};

    fn sh(dir: &Path, script: &str) {
        let ok = Command::new("bash").arg("-ec").arg(script).current_dir(dir).status().unwrap().success();
        assert!(ok, "{script}");
    }

    fn setup(name: &str) -> std::path::PathBuf {
        let base = std::env::temp_dir().join(format!("ideae-pull-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        sh(&base, "git init -q --bare -b main remote.git
            git clone -q remote.git a 2>/dev/null; cd a; git config user.email t@t; git config user.name t
            printf 'one\\ntwo\\nthree\\n' > f.txt; git add f.txt; git commit -qm base; git push -q origin main
            cd ..; git clone -q remote.git b; cd b; git config user.email t@t; git config user.name t");
        base
    }

    #[test]
    fn a_folder_becomes_a_repository_tasks_can_start_from() {
        let dir = std::env::temp_dir().join(format!("ideae-adopt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("plain/node_modules")).unwrap();
        std::fs::create_dir_all(dir.join("svn/.svn")).unwrap();
        sh(&dir, "echo 1 > plain/dump.sql; echo 2 > plain/node_modules/i.js");
        assert!(!super::adopt(&dir.join("svn")));
        assert!(!dir.join("svn/.git").exists());
        sh(&dir, "mkdir -p svn/inner p4/inner; echo P4PORT=x > p4/.p4config");
        for inside in ["svn/inner", "p4", "p4/inner"] {
            assert!(!super::adopt(&dir.join(inside)), "{inside}");
            assert!(!dir.join(inside).join(".git").exists(), "{inside}");
        }
        let plain = dir.join("plain");
        assert!(!super::adopt(&plain), "no commit yet, no worktree");
        assert!(super::is_repository(&plain));
        assert_eq!(super::git(&plain, &["ls-files"]).unwrap(), "");
        assert_eq!(super::git(&plain, &["status", "--porcelain"]).unwrap().trim(), "?? dump.sql");
        sh(&plain, "git add -A; git -c user.name=t -c user.email=t@t commit -qm first");
        assert!(super::adopt(&plain));
        let task = dir.join("task");
        super::git_status(&plain, &["worktree", "add", "-q", "-b", "t", &task.to_string_lossy(), "HEAD"]).unwrap();
        assert!(task.join("dump.sql").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_folder_without_git_lists_its_files() {
        let dir = std::env::temp_dir().join(format!("ideae-plain-folder-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        sh(&dir, "mkdir -p sub/empty .svn node_modules/pkg; echo 0 > .svn/entries; echo 6 > node_modules/pkg/i.js; echo 1 > dump.sql; echo 2 > sub/b.sql; echo 3 > .env; echo 4 > .DS_Store; echo skipped.log > .gitignore; echo 5 > skipped.log");
        let mut known = Known::default();
        let read = refresh(&dir, None, false, &mut known);
        assert!(!read.failed);
        assert_eq!(*read.files, [".env", ".gitignore", "dump.sql", "sub/b.sql"]);
        assert_eq!(read.ignored, ["node_modules/"]);
        assert!(read.watch.0.contains(&dir.join("sub/empty")));
        assert!(!read.watch.0.iter().any(|watched| watched.starts_with(dir.join("node_modules"))));
        let again = refresh(&dir, None, false, &mut known);
        assert!(std::sync::Arc::ptr_eq(&read.files, &again.files));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn fast_forward_keeps_local_edits() {
        let base = setup("ff");
        sh(&base, "cd a; printf 'one\\ntwo\\nthree\\nfour\\n' > f.txt; git commit -qam up; git push -q origin main
            cd ../b; echo note > untracked.txt; printf 'ONE\\ntwo\\nthree\\n' > f.txt");
        assert_eq!(pull_conflicts(&base.join("b")), Some(Vec::new()), "the lines written here are not the ones that come");
        let out = pull_safe(&base.join("b")).unwrap();
        assert!(out.starts_with("Fast-forwarded"), "{out}");
        let text = std::fs::read_to_string(base.join("b/f.txt")).unwrap();
        assert_eq!(text, "ONE\ntwo\nthree\nfour\n");
        let _ = std::fs::remove_dir_all(&base);
    }

    /// What is only written, not committed, and what comes change the
    /// same line: the pull is told beforehand and not tried, where a stash
    /// coming back would have left its conflict in the file.
    #[test]
    fn a_pull_over_unsaved_work_it_would_break_is_not_tried() {
        let base = setup("unsaved");
        sh(&base, "cd a; printf 'one\\nTWO-remote\\nthree\\n' > f.txt; git commit -qam remote; git push -q origin main
            cd ../b; printf 'one\\nTWO-written\\nthree\\n' > f.txt; git fetch -q");
        let b = base.join("b");
        assert_eq!(pull_conflicts(&b), Some(vec!["f.txt".to_string()]));
        assert!(pull_safe(&b).unwrap_err().starts_with("would conflict in f.txt"));
        assert_eq!(std::fs::read_to_string(b.join("f.txt")).unwrap(), "one\nTWO-written\nthree\n", "as it was written");
        let stashes = Command::new("git").args(["stash", "list"]).current_dir(&b).output().unwrap().stdout;
        assert!(stashes.is_empty(), "nothing put away and left there");
        assert!(conflict_marked(&working_patch(&b)).is_empty());
        // The conflict as a stash would have left it is seen for what it is.
        std::fs::write(b.join("f.txt"), "one\n<<<<<<< Updated upstream\nTWO-remote\n=======\nTWO-written\n>>>>>>> Stashed changes\nthree\n").unwrap();
        assert_eq!(conflict_marked(&working_patch(&b)), [("f.txt".to_string(), 2)]);
        std::fs::write(b.join("f.txt"), "one\n    let marker = \"<<<<<<< \";\n>>>>>>> alone\nthree\n").unwrap();
        assert!(conflict_marked(&working_patch(&b)).is_empty(), "a marker in code, or one without its pair, is no conflict");
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Two folders compared as they are: what differs is a patch git itself
    /// applies, what is the same in both is not in it.
    #[test]
    fn two_worktrees_compare_file_by_file() {
        let base = setup("between");
        let (a, b) = (base.join("a"), base.join("b"));
        sh(&base, "cd a; printf 'one\\nTWO\\nthree\\n' > f.txt; echo same > s.txt; echo only-a > gone.txt
            cd ../b; echo same > s.txt; echo new > made.txt; printf '\\000\\001' > blob.bin");
        let files: Vec<String> = ["f.txt", "s.txt", "gone.txt", "made.txt", "blob.bin", "nowhere.txt"].map(String::from).to_vec();
        let patch = between(&a, &b, &files);
        assert!(!patch.contains("s.txt") && !patch.contains("nowhere.txt"), "the same in both, or in neither:\n{patch}");
        assert!(patch.contains("Binary files a/blob.bin and b/blob.bin differ"));
        assert_eq!(hunks(&patch).iter().map(|h| (h.path.as_str(), h.line)).collect::<Vec<_>>(), [("f.txt", 2)], "one change; whole files are no hunks");
        // Git takes it: a's files become b's.
        let text: String = patch.split_inclusive("diff --git ").filter(|part| !part.contains("blob.bin")).collect();
        std::fs::write(base.join("ab.patch"), text.trim_end_matches("diff --git ")).unwrap();
        sh(&a, "git apply ../ab.patch");
        assert_eq!(std::fs::read_to_string(a.join("f.txt")).unwrap(), "one\ntwo\nthree\n");
        assert!(!a.join("gone.txt").exists() && std::fs::read_to_string(a.join("made.txt")).unwrap() == "new\n");
        let _ = std::fs::remove_dir_all(&base);
    }

    /// With whitespace left out, two variants that differ by a reindent and
    /// a blank line are the same, and a real change is all there is.
    #[test]
    fn variants_compared_without_whitespace() {
        let base = setup("between-ws");
        let (a, b) = (base.join("a"), base.join("b"));
        sh(&base, "cd a; printf 'fn f() {\\na();\\n}\\n' > same.rs; printf 'x\\ny\\n' > real.rs; printf '\\000\\001' > blob.bin
            cd ../b; printf 'fn f() {\\n    a();\\n\\n}\\n' > same.rs; printf '  x\\nY\\n' > real.rs; printf '\\000\\002' > blob.bin");
        let files: Vec<String> = ["same.rs", "real.rs", "blob.bin"].map(String::from).to_vec();
        super::IGNORE_WHITESPACE.set(true);
        let patch = between(&a, &b, &files);
        super::IGNORE_WHITESPACE.set(false);
        assert!(!patch.contains("same.rs"), "{patch}");
        assert!(patch.contains("diff --git a/real.rs b/real.rs") && patch.contains("-y") && patch.contains("+Y") && !patch.contains("-x"), "{patch}");
        assert!(patch.contains("Binary files a/blob.bin and b/blob.bin differ"), "{patch}");
        assert_eq!(gpui_kit::component::diff::DiffFile::parse(&patch).unwrap().len(), 2);
        let _ = std::fs::remove_dir_all(&base);
    }

    /// What Finder and Explorer leave in folders is not among the files, in
    /// a folder git knows or one it ignores; a file of the project is.
    #[test]
    fn the_system_s_leavings_are_not_files_of_the_task() {
        let base = setup("junk");
        let b = base.join("b");
        sh(&b, "mkdir -p out src; touch .DS_Store src/.DS_Store src/._f.txt src/Thumbs.db out/.DS_Store out/a.o src/kept.txt; echo out/ > .gitignore");
        let read = refresh(&b, None, false, &mut Known::default());
        assert!(read.files.iter().all(|f| !super::is_junk(f)), "{:?}", read.files);
        assert!(read.files.iter().any(|f| f == "src/kept.txt"), "{:?}", read.files);
        assert!(read.ignored.iter().all(|f| !super::is_junk(f)), "{:?}", read.ignored);
        assert!(super::is_junk("a/b/desktop.ini") && super::is_junk(".Spotlight-V100/") && !super::is_junk("DS_Store.md") && !super::is_junk("src/main.rs"));
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Work is put away, seen in the list and as a patch, and brought back;
    /// one that would not go back in without a conflict stays in the list.
    #[test]
    fn work_is_put_away_and_brought_back() {
        let base = setup("stash");
        let b = base.join("b");
        assert!(refresh(&b, None, false, &mut Known::default()).stashes.is_empty());
        sh(&b, "printf 'one\\nTWO\\nthree\\n' > f.txt; echo note > new.txt");
        stash(&b, "before lunch").unwrap();
        assert_eq!(std::fs::read_to_string(b.join("f.txt")).unwrap(), "one\ntwo\nthree\n");
        assert!(!b.join("new.txt").exists(), "the file git did not know of went with it");
        let listed = refresh(&b, None, false, &mut Known::default()).stashes;
        assert_eq!(listed.iter().map(|s| (s.name.as_str(), s.what.as_str())).collect::<Vec<_>>(), [("stash@{0}", "On main: before lunch")]);
        let patch = stash_patch(&b, "stash@{0}");
        assert!(patch.contains("+TWO") && patch.contains("new.txt"), "{patch}");
        // The line it changed is changed again and committed: it cannot simply go back.
        sh(&b, "printf 'one\\n2\\nthree\\n' > f.txt; git commit -qam two");
        let put_away = listed[0].clone();
        let refused = stash_apply(&b, &put_away, true).unwrap_err();
        assert!(refused.ends_with("(the stash is kept)"), "{refused}");
        assert_eq!(refresh(&b, None, false, &mut Known::default()).stashes.len(), 1);
        assert!(refused.starts_with("would conflict in f.txt"), "{refused}");
        assert_eq!(std::fs::read_to_string(b.join("f.txt")).unwrap(), "one\n2\nthree\n", "and nothing was touched");
        sh(&b, "git reset -q --hard HEAD~1");
        // Another worktree put something away since: the one shown is one further down, and still the one brought back.
        sh(&b, "echo other > other.txt; git stash push -q --include-untracked -m other");
        stash_apply(&b, &put_away, true).unwrap();
        assert!(!b.join("other.txt").exists(), "not the one that has its place now");
        let other = refresh(&b, None, false, &mut Known::default()).stashes.remove(0);
        assert!(stash_drop(&b, &put_away).unwrap_err().contains("no longer in the list"));
        stash_drop(&b, &other).unwrap();
        assert_eq!(std::fs::read_to_string(b.join("f.txt")).unwrap(), "one\nTWO\nthree\n");
        assert!(b.join("new.txt").exists() && refresh(&b, None, false, &mut Known::default()).stashes.is_empty(), "back, and off the list");
        stash(&b, "").unwrap();
        let last = refresh(&b, None, false, &mut Known::default()).stashes.remove(0);
        stash_drop(&b, &last).unwrap();
        assert!(refresh(&b, None, false, &mut Known::default()).stashes.is_empty());
        let _ = std::fs::remove_dir_all(&base);
    }

    /// What there is to compare with is listed with what each is, and the
    /// worktree is compared with any of it: all of it, or one file.
    #[test]
    fn the_worktree_compares_with_a_branch_a_tag_or_a_commit() {
        let base = setup("against");
        let b = base.join("b");
        sh(&b, "git tag v1; printf 'one\\ntwo\\nthree\\nfour\\n' > f.txt; echo other > g.txt; git add g.txt; git commit -qam four
            git branch side v1; printf 'one\\ntwo\\nthree\\nfour\\nfive\\n' > f.txt");
        let found = refs(&b);
        let what = |name: &str| found.iter().find(|(known, _)| known == name).map(|(_, what)| what.split(" · ").next().unwrap_or_default().to_string());
        assert_eq!((what("main"), what("side"), what("origin/main"), what("v1")), (Some("branch".into()), Some("branch".into()), Some("remote branch".into()), Some("tag".into())));
        assert!(found.iter().any(|(_, what)| what.starts_with("commit · ") && what.ends_with("four")), "{found:?}");
        let against_tag = diff_against(&b, "v1", None);
        assert!(against_tag.contains("+four") && against_tag.contains("+five") && against_tag.contains("g.txt"), "{against_tag}");
        let one_file = diff_against(&b, "side", Some("f.txt"));
        assert!(one_file.contains("+five") && !one_file.contains("g.txt"), "{one_file}");
        assert_eq!(diff_against(&b, "HEAD", Some("g.txt")), "", "as committed");
        let _ = std::fs::remove_dir_all(&base);
    }

    /// The branches are listed with what is known of each; one gets a
    /// worktree of its own, a remote one a local branch that follows it;
    /// one is deleted, here and on the remote, not merged only when told.
    #[test]
    fn branches_are_listed_opened_and_deleted() {
        let base = setup("branches");
        let b = base.join("b");
        sh(&b, "git branch done; git checkout -q -b wip; echo more >> f.txt; git commit -qam wip; git push -q -u origin wip 2>/dev/null
            git checkout -q main; cd ../a; git checkout -q -b theirs; echo x > t.txt; git add t.txt; git commit -qm theirs; git push -q origin theirs 2>/dev/null
            cd ../b; git fetch -q");
        let rows = branch_rows(&b);
        let row = |name: &str| rows.iter().find(|r| r.name == name).unwrap_or_else(|| panic!("{name} in {rows:?}")).clone();
        assert!(rows.iter().all(|r| r.name != "main" && r.name != "origin/main"), "not the branch work starts from: {rows:?}");
        assert!(row("done").merged && !row("wip").merged && row("origin/theirs").remote && row("wip").subject == "wip");
        assert_eq!(row("done").held, None);

        let opened = branch_worktree(&b, &row("origin/theirs"), false).unwrap();
        assert!(opened.join("t.txt").is_file(), "the remote branch, in a worktree of its own");
        let rows = branch_rows(&b);
        let theirs = rows.iter().find(|r| r.name == "theirs").expect("a local branch that follows it");
        assert_eq!(theirs.held.as_ref().map(|p| p.canonicalize().unwrap()), Some(opened.canonicalize().unwrap()));
        assert!(branch_worktree(&b, &row("origin/theirs"), false).is_err(), "not twice");

        delete_branch(&b, &row("done"), false).unwrap();
        assert!(delete_branch(&b, &row("wip"), false).is_ok(), "pushed: git lets it go");
        delete_branch(&b, &row("origin/wip"), false).unwrap();
        sh(&b, "git branch loose; git checkout -q loose; echo y >> f.txt; git commit -qam loose; git checkout -q main");
        let loose = branch_rows(&b).into_iter().find(|r| r.name == "loose").unwrap();
        assert!(!loose.merged && delete_branch(&b, &loose, false).is_err(), "what it holds would be lost");
        delete_branch(&b, &loose, true).unwrap();
        let left: Vec<String> = branch_rows(&b).into_iter().map(|r| r.name).collect();
        assert!(!left.iter().any(|n| ["done", "wip", "origin/wip", "loose"].contains(&n.as_str())), "{left:?}");
        let _ = std::fs::remove_dir_all(&base);
    }

    /// History is read of any branch and narrowed by message, author and
    /// file; a commit of another branch is taken in, one is reverted, and
    /// one that would conflict changes nothing and names the files.
    #[test]
    fn history_is_filtered_and_commits_are_picked_and_reverted() {
        let base = setup("history");
        let b = base.join("b");
        sh(&b, "git checkout -q -b side; echo s > side.txt; git add side.txt; git commit -qm 'Add the side file'
            printf 'one\\nSIDE\\nthree\\n' > f.txt; git -c user.name=ann commit -qam 'Change two on the side'; git tag v2
            git checkout -q main; printf 'one\\nMAIN\\nthree\\n' > f.txt; git commit -qam 'Change two on main'");
        let subjects = |typed: &str, rev: &str| history(&b, 50, &HistoryQuery::typed(rev, typed)).into_iter().map(|c| c.subject).collect::<Vec<_>>();
        assert_eq!(subjects("", ""), ["Change two on main", "base"]);
        assert_eq!(subjects("", "side"), ["Change two on the side", "Add the side file", "base"], "another branch's");
        assert_eq!(subjects("SIDE FILE", "side"), ["Add the side file"], "in the message, whatever the case");
        assert_eq!(subjects("@ann", "side"), ["Change two on the side"]);
        assert_eq!(subjects("file:side.txt", "side"), ["Add the side file"]);
        assert_eq!(subjects("change @ann file:f.txt", "side"), ["Change two on the side"], "all of them at once");
        let side = history(&b, 50, &HistoryQuery::typed("side", ""));
        assert_eq!((side[0].refs.as_str(), side[2].refs.as_str()), ("tag: v2, side", "origin/main"), "what points at each");

        assert!(replay_commit(&b, &side[1].hash, false).unwrap().starts_with("Picked"));
        assert!(b.join("side.txt").is_file() && subjects("", "")[0] == "Add the side file");
        let Err(RebaseError::Conflicts(files)) = replay_commit(&b, &side[0].hash, false) else { panic!("the same line changed on both") };
        assert_eq!(files, ["f.txt"]);
        assert_eq!(std::fs::read_to_string(b.join("f.txt")).unwrap(), "one\nMAIN\nthree\n", "nothing changed");
        assert!(git_clean(&b), "and no pick left halfway");
        let picked = history(&b, 1, &HistoryQuery::default())[0].hash.clone();
        assert!(replay_commit(&b, &picked, true).unwrap().starts_with("Reverted"));
        assert!(!b.join("side.txt").exists() && subjects("", "")[0].starts_with("Revert"));

        let made = commit_worktree(&b, &side[1].hash, false).unwrap();
        assert!(made.join("side.txt").is_file() && made.file_name().unwrap().to_string_lossy().starts_with("from-"), "a task of its own, at that commit");
        let _ = std::fs::remove_dir_all(&base);
    }

    fn git_clean(dir: &Path) -> bool {
        let state = Command::new("git").args(["status", "--porcelain"]).current_dir(dir).output().unwrap().stdout;
        state.is_empty()
    }

    #[test]
    fn conflict_is_undone() {
        let base = setup("conflict");
        sh(&base, "cd a; printf 'one\\nTWO-remote\\nthree\\n' > f.txt; git commit -qam remote; git push -q origin main
            cd ../b; printf 'one\\nTWO-local\\nthree\\n' > f.txt; git commit -qam local; git fetch -q");
        let head_before = Command::new("git").args(["rev-parse", "HEAD"]).current_dir(base.join("b")).output().unwrap().stdout;
        assert_eq!(pull_conflicts(&base.join("b")), Some(vec!["f.txt".to_string()]));
        let err = pull_safe(&base.join("b")).unwrap_err();
        assert!(err.starts_with("would conflict in f.txt"), "{err}");
        let head_after = Command::new("git").args(["rev-parse", "HEAD"]).current_dir(base.join("b")).output().unwrap().stdout;
        assert_eq!(head_before, head_after);
        assert!(!base.join("b/.git/rebase-merge").exists());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn rebase_without_conflict() {
        let base = setup("rebase");
        sh(&base, "cd a; printf 'one\\ntwo\\nthree\\nremote\\n' > f.txt; git commit -qam remote; git push -q origin main
            cd ../b; echo local > g.txt; git add g.txt; git commit -qm local");
        let out = pull_safe(&base.join("b")).unwrap();
        assert!(out.starts_with("Rebased"), "{out}");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn fetch_base_brings_the_default_branch() {
        let base = setup("fetchbase");
        sh(&base, "cd a; echo later > h.txt; git add h.txt; git commit -qm later; git push -q origin main");
        let b = base.join("b");
        let before = super::git(&b, &["rev-parse", "origin/main"]).unwrap();
        super::fetch_base(&b).unwrap();
        let after = super::git(&b, &["rev-parse", "origin/main"]).unwrap();
        assert_ne!(before, after);
        assert_eq!(after, super::git(&base.join("a"), &["rev-parse", "HEAD"]).unwrap());
        let _ = std::fs::remove_dir_all(&base);
    }

    /// A file ignored by its path, its name or its extension, in .gitignore
    /// or in info/exclude; once.
    #[test]
    fn a_file_is_ignored_where_it_is_said() {
        let pats = |path: &str, folder| super::ignore_patterns(path, folder).into_iter().map(|(_, p, _)| p).collect::<Vec<_>>();
        assert_eq!(pats(".DS_Store", false), [".DS_Store", "/.DS_Store"], "at the root too: every one first");
        assert_eq!(pats("src/.DS_Store", false), [".DS_Store", "/src/.DS_Store"]);
        assert_eq!(pats("logs/app.log", false), ["app.log", "/logs/app.log", "*.log"]);
        assert_eq!(pats("build", true), ["build/", "/build/"]);
        let root = std::env::temp_dir().join(format!("ideae-ignore-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        assert!(std::process::Command::new("git").args(["init", "-q"]).current_dir(&root).status().unwrap().success());
        std::fs::write(root.join(".gitignore"), "target").unwrap();
        super::ignore(&root, ".DS_Store", super::IgnoreIn::Shared).unwrap();
        super::ignore(&root, ".DS_Store", super::IgnoreIn::Shared).unwrap();
        assert_eq!(std::fs::read_to_string(root.join(".gitignore")).unwrap(), "target\n.DS_Store\n", "once, on a line of its own");
        super::ignore(&root, "*.log", super::IgnoreIn::Local).unwrap();
        assert!(std::fs::read_to_string(root.join(".git/info/exclude")).unwrap().lines().any(|l| l == "*.log"));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A project's tasks go beside it, or in the folder its settings name.
    #[test]
    fn a_project_s_worktrees_go_where_it_says() {
        let repo = std::path::Path::new("/nowhere/ideae-worktrees-test/app");
        assert_eq!(super::task_worktree_dir(repo, "fix/a"), std::path::Path::new("/nowhere/ideae-worktrees-test/app.worktrees/fix-a"));
        super::set_project_worktrees(repo, "/elsewhere/app-tasks");
        assert_eq!(super::task_worktree_dir(repo, "fix"), std::path::Path::new("/elsewhere/app-tasks/fix"));
        super::set_project_worktrees(repo, "");
        assert_eq!(super::task_worktrees(repo), super::worktrees_beside(repo));
        if let Some(home) = std::env::var_os("HOME") {
            assert_eq!(super::home_expanded("~/wt"), std::path::PathBuf::from(home).join("wt"));
        }
    }

    /// A project's base: origin's branch of that name, fetched; else as said.
    #[test]
    fn a_task_starts_from_the_project_s_base() {
        let base = std::env::temp_dir().join(format!("ideae-task-chosen-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let script = "git init -q --bare -b main origin.git; git init -q -b main repo; cd repo; git config user.email t@t; git config user.name t; \
            echo a > a; git add a; git commit -qm one; git remote add origin ../origin.git; git push -q origin main; \
            git checkout -q -b develop; echo b > a; git commit -qam two; git push -q origin develop; git checkout -q main; git branch -q -D develop; \
            git tag v1 main";
        assert!(std::process::Command::new("sh").arg("-ec").arg(script).current_dir(&base).status().unwrap().success());
        let repo = base.join("repo");
        let head = |at: &std::path::Path, rev: &str| super::git(at, &["rev-parse", rev]).unwrap();
        let task = super::create_task_worktree(&repo, "from-develop", "develop", false).unwrap();
        assert_eq!(head(&task, "HEAD"), head(&repo, "origin/develop"), "origin's develop, fetched");
        let tagged = super::create_task_worktree(&repo, "from-tag", "v1", false).unwrap();
        assert_eq!(head(&tagged, "HEAD"), head(&repo, "v1"), "a name origin has no branch of, as it is");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_task_leaves_the_remote_branch_and_owns_only_its_commits() {
        let base = std::env::temp_dir().join(format!("ideae-task-base-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        // The remote added by hand: no origin/HEAD. main has commits not pushed.
        let script = "git init -q --bare -b main origin.git; git init -q -b main repo; cd repo; git config user.email t@t; git config user.name t; \
            echo a > a; git add a; git commit -qm one; git remote add origin ../origin.git; git push -q origin main; \
            for n in 2 3 4; do echo $n > a; git commit -qam $n; done";
        assert!(std::process::Command::new("sh").arg("-ec").arg(script).current_dir(&base).status().unwrap().success());
        let repo = base.join("repo");
        let task = super::create_task_worktree(&repo, "DEV-1_x", "", false).unwrap();
        let head = |at: &std::path::Path, rev: &str| super::git(at, &["rev-parse", rev]).unwrap();
        assert_eq!(head(&task, "HEAD"), head(&repo, "origin/main"), "from the remote's main, not the local one ahead of it");
        assert!(crate::archive::unpushed(&task).is_empty(), "nothing of its own to lose: main's commits not pushed are main's");
        std::fs::write(task.join("b"), "b").unwrap();
        assert!(std::process::Command::new("sh").arg("-ec").arg("git add b; git -c user.email=t@t -c user.name=t commit -qm own").current_dir(&task).status().unwrap().success());
        assert_eq!(crate::archive::unpushed(&task).len(), 1, "its own commit");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn touched_files_since_the_fork() {
        let base = setup("touched");
        sh(&base, "cd a; echo later > h.txt; git add h.txt; git commit -qm later; git push -q origin main
            cd ../b; git checkout -qb task; echo c > g.txt; echo x > Cargo.lock; git add .; git commit -qm task
            echo changed >> f.txt; echo new > n.txt; git fetch -q");
        let files: Vec<String> = super::touched_files(&base.join("b"), "origin/main").unwrap().into_iter().collect();
        assert_eq!(files, ["f.txt", "g.txt", "n.txt"]);
        let _ = std::fs::remove_dir_all(&base);
    }
}

/// Local and remote branches, for picking what to diff against.
/// A branch of the repository, here or on a remote, for the list one is
/// chosen from.
#[derive(Clone, Debug, PartialEq)]
pub struct BranchRow {
    /// `fix-login`, `origin/fix-login`.
    pub name: String,
    pub remote: bool,
    /// All of it is in the branch work starts from.
    pub merged: bool,
    pub when: String,
    pub subject: String,
    /// The worktree it is checked out in: a task has it.
    pub held: Option<PathBuf>,
    /// Commits it has that the branch work starts from has not; none when
    /// git is too old to tell.
    pub ahead: Option<u32>,
    /// What is changed and not committed in the worktree that holds it.
    pub changed: Vec<String>,
    /// That worktree is one an agent made for itself (a Claude Code subagent's).
    pub agent: bool,
    /// What that agent was asked to do, when it can be told.
    pub about: Option<String>,
}

pub fn branch_rows(root: &Path) -> Vec<BranchRow> {
    branch_rows_of(root, true)
}

/// As `branch_rows`; without `detail`, nothing of the worktrees that hold
/// them (`changed`, `agent`, `about`): a `git status` each.
pub fn branch_rows_of(root: &Path, detail: bool) -> Vec<BranchRow> {
    let base = base_ref(root).unwrap_or_else(|| "HEAD".into());
    let listed = |merged: bool| {
        let mut args = vec!["for-each-ref", "--sort=-committerdate", "--format=%(refname)%1f%(refname:short)%1f%(committerdate:relative)%1f%(subject)"];
        if merged {
            args.extend(["--merged", base.as_str()]);
        }
        args.extend(["refs/heads", "refs/remotes"]);
        git(root, &args).unwrap_or_default()
    };
    let merged: std::collections::HashSet<String> = listed(true).lines().filter_map(|line| line.split('\x1f').next().map(str::to_string)).collect();
    let all = worktrees(root);
    let paths: Vec<PathBuf> = all.iter().map(|w| w.path.clone()).collect();
    let held: std::collections::HashMap<String, PathBuf> = all.into_iter().map(|w| (w.branch, w.path)).collect();
    // One call for all of them; git before 2.41 has no ahead-behind and tells nothing.
    let ahead: std::collections::HashMap<String, u32> = git(root, &["for-each-ref", &format!("--format=%(refname)%1f%(ahead-behind:{base})"), "refs/heads", "refs/remotes"])
        .unwrap_or_default()
        .lines()
        .filter_map(|line| {
            let (name, counts) = line.split_once('\x1f')?;
            Some((name.to_string(), counts.split_whitespace().next()?.parse().ok()?))
        })
        .collect();
    let changed = |path: &Path| -> Vec<String> {
        git(path, &["status", "--porcelain"]).unwrap_or_default().lines().filter_map(|l| l.get(3..)).map(|p| p.rsplit(" -> ").next().unwrap_or(p).to_string()).collect()
    };
    let mut rows: Vec<BranchRow> = listed(false)
        .lines()
        .filter_map(|line| {
            let mut parts = line.split('\x1f');
            let (full, name) = (parts.next()?, parts.next()?.to_string());
            let remote = full.starts_with("refs/remotes/");
            // The branch work starts from is not one to tidy away, nor the remote's pointer to it.
            if name.ends_with("/HEAD") || name == "origin" || name == base || base.strip_prefix("origin/") == Some(name.as_str()) {
                return None;
            }
            let held = (!remote).then(|| held.get(&name).cloned()).flatten();
            let from = held.as_deref().filter(|_| detail).and_then(|path| crate::workspace::side::made_by_agent(path, &paths));
            Some(BranchRow {
                merged: merged.contains(full),
                ahead: ahead.get(full).copied(),
                changed: Vec::new(),
                agent: from.is_some(),
                about: held.as_deref().zip(from).and_then(|(path, from)| crate::workspace::side::about(path, from.as_deref().or(paths.first().map(PathBuf::as_path))?)),
                held,
                when: parts.next()?.to_string(),
                subject: parts.next().unwrap_or_default().to_string(),
                name,
                remote,
            })
        })
        .collect();
    if detail {
        // A few at a time: each worktree's status is a process of its own.
        let mut pending: Vec<&mut BranchRow> = rows.iter_mut().filter(|r| r.held.is_some()).collect();
        for chunk in pending.chunks_mut(8) {
            std::thread::scope(|scope| {
                for row in chunk.iter_mut() {
                    scope.spawn(|| row.changed = row.held.as_deref().map(changed).unwrap_or_default());
                }
            });
        }
    }
    rows
}

/// Deletes a branch: here, or on its remote (`origin/x` is told to drop
/// `x`). One not merged goes only with `force`.
pub fn delete_branch(root: &Path, branch: &BranchRow, force: bool) -> Result<String, String> {
    if branch.remote {
        let (remote, name) = branch.name.split_once('/').ok_or("not a remote branch")?;
        run(root, "git", &["push", remote, "--delete", name])?;
    } else {
        run(root, "git", &["branch", if force { "-D" } else { "-d" }, &branch.name])?;
    }
    Ok(format!("Deleted {}", branch.name))
}

/// A worktree for a branch there is already, next to the repository
/// where a new task's would be: the app then shows it as a task. A remote
/// branch gets a local one that follows it.
pub fn branch_worktree(root: &Path, branch: &BranchRow, copy_env: bool) -> Result<PathBuf, String> {
    let repo = main_worktree(root).ok_or("not a git repository")?;
    let local = if branch.remote { branch.name.split_once('/').map_or(branch.name.as_str(), |(_, name)| name) } else { branch.name.as_str() };
    let dir = task_worktree_dir(&repo, local);
    if dir.exists() {
        return Err(format!("{} already exists", dir.display()));
    }
    let dir_s = dir.to_string_lossy().into_owned();
    let known = git(&repo, &["rev-parse", "--verify", "-q", &format!("refs/heads/{local}")]).is_some_and(|hash| !hash.trim().is_empty());
    if branch.remote && !known {
        git_status(&repo, &["worktree", "add", "--track", "-b", local, &dir_s, &branch.name])?;
    } else {
        git_status(&repo, &["worktree", "add", &dir_s, local])?;
    }
    if copy_env {
        copy_env_files(&repo, &dir);
    }
    Ok(dir)
}

/// A worktree on a colleague's request: its head fetched from where the
/// requests are (`upstream`, else `origin`) into a local branch of its
/// name, which a branch of that name here already is used as it is. One
/// of this repository's own branches follows it on origin; a fork's has
/// nothing to push to.
pub fn request_worktree(repo: &Path, kind: crate::integrations::Kind, number: u64, head: &str, cross: bool, copy_env: bool) -> Result<(PathBuf, String), String> {
    let remote = ["upstream", "origin"].into_iter().find(|r| git(repo, &["remote", "get-url", r]).is_some()).ok_or("no remote named origin or upstream")?;
    // The request's head as it is now, whatever a branch of its name here holds.
    let refspec = match kind {
        crate::integrations::Kind::GitLab => format!("refs/merge-requests/{number}/head"),
        _ => format!("refs/pull/{number}/head"),
    };
    git_status(repo, &["fetch", "--quiet", remote, &refspec])?;
    let tip = git(repo, &["rev-parse", "FETCH_HEAD"]).map(|h| h.trim().to_string()).filter(|h| !h.is_empty()).ok_or("the request's head was not fetched")?;
    let has = |name: &str| git(repo, &["rev-parse", "--verify", "-q", &format!("refs/heads/{name}")]).is_some_and(|h| !h.trim().is_empty());
    // A branch here only behind the request's head is moved on to it; one of
    // other commits (an older copy, someone else's of that name) is left alone.
    let behind = |name: &str| git_status(repo, &["merge-base", "--is-ancestor", &format!("refs/heads/{name}"), &tip]).is_ok();
    let own = format!("request-{number}");
    let named = !head.is_empty() && head != "main" && head != "master";
    let local = if named && (!has(head) || behind(head)) { head.to_string() } else { own.clone() };
    if has(&local) && !behind(&local) {
        return Err(format!("{local} here has commits the request does not: delete or rename it first"));
    }
    let dir = task_worktree_dir(repo, &local);
    if crate::hostfs::stat(&dir).is_ok() {
        return Err(format!("{} already exists", crate::hosts::there(&dir).display()));
    }
    git_status(repo, &["branch", "-f", &local, &tip])?;
    let dir_s = crate::hosts::there(&dir).to_string_lossy().into_owned();
    git_status(repo, &["worktree", "add", &dir_s, &local])?;
    if !cross && local == head && git_status(repo, &["fetch", "--quiet", "origin", head]).is_ok() {
        let _ = git_status(repo, &["branch", &format!("--set-upstream-to=origin/{head}"), &local]);
    }
    if copy_env {
        copy_env_files(repo, &dir);
    }
    Ok((dir, local))
}

/// What the worktree can be compared with: branches, tags, the latest
/// commits. Each as git names it, and what it is.
pub fn refs(root: &Path) -> Vec<(String, String)> {
    let listed = |args: &[&str]| git(root, args).unwrap_or_default();
    let named = listed(&["for-each-ref", "--sort=-committerdate", "--format=%(refname)%1f%(refname:short)%1f%(committerdate:relative)%1f%(subject)", "refs/heads", "refs/remotes", "refs/tags"]);
    let mut found: Vec<(String, String)> = named
        .lines()
        .filter_map(|line| {
            let mut parts = line.split('\x1f');
            let (full, short, when, subject) = (parts.next()?, parts.next()?, parts.next()?, parts.next().unwrap_or_default());
            let kind = if full.starts_with("refs/tags") { "tag" } else if full.starts_with("refs/remotes") { "remote branch" } else { "branch" };
            (!short.ends_with("/HEAD") && short != "origin").then(|| (short.to_string(), format!("{kind} · {when} · {subject}")))
        })
        .collect();
    let commits = listed(&["log", "-60", "--format=%h%x1f%cr%x1f%s"]);
    found.extend(commits.lines().filter_map(|line| {
        let mut parts = line.split('\x1f');
        Some((parts.next()?.to_string(), format!("commit · {} · {}", parts.next()?, parts.next().unwrap_or_default())))
    }));
    found
}

/// The worktree against `rev`, all of it or the one file: what is here
/// that the commit, tag or branch does not have.
/// What a branch checked out in another worktree has done since it left
/// this one's HEAD, with what is not committed there yet (an agent's work in
/// progress): the patch and the commit it starts from. None for any other `rev`.
pub fn work_of(root: &Path, rev: &str, file: Option<&str>) -> Option<(String, String)> {
    let here = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let there = worktrees(root).into_iter().find(|w| w.branch == rev && w.path.canonicalize().unwrap_or_else(|_| w.path.clone()) != here)?;
    let now = worktree_snapshot(&there.path, true)?;
    let now = if now == "HEAD" { rev.to_string() } else { now };
    let base = git(root, &["merge-base", "HEAD", rev])?.trim().to_string();
    let mut args = vec!["diff", base.as_str(), now.as_str()];
    args.extend(diff_args());
    if let Some(file) = file {
        args.extend(["--", file]);
    }
    Some((git(root, &args)?, base))
}

pub fn diff_against(root: &Path, rev: &str, file: Option<&str>) -> String {
    let mut args = vec!["diff", rev];
    args.extend(diff_args());
    if let Some(file) = file {
        args.extend(["--", file]);
    }
    git(root, &args).unwrap_or_default()
}

pub fn branches(root: &Path) -> Vec<String> {
    git(root, &["for-each-ref", "--sort=-committerdate", "--format=%(refname:short)", "refs/heads", "refs/remotes"])
        .unwrap_or_default()
        .lines()
        .map(str::trim)
        .filter(|b| !b.is_empty() && !b.ends_with("/HEAD") && *b != "origin")
        .map(str::to_string)
        .collect()
}

pub(crate) fn git_status(root: &Path, args: &[&str]) -> Result<String, String> {
    // On another machine: run there, the same way.
    if crate::hosts::is_remote(root) {
        let out = crate::hostfs::run(root, "git", args, &[("GIT_TERMINAL_PROMPT", "0")], None).map_err(|e| format!("git: {e}"))?;
        return if out.success() { Ok(String::from_utf8_lossy(&out.stdout).into_owned()) } else { Err(out.stderr.trim().to_string()) };
    }
    let output = crate::procs::command("git")
        .env("GIT_TERMINAL_PROMPT", "0")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .map_err(|e| format!("git: {e}"))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_string())
    }
}

/// The main worktree of the repository containing `path`, or None when it is
/// not in one.
pub fn main_worktree(path: &Path) -> Option<PathBuf> {
    git(path, &["rev-parse", "--git-dir"])?;
    worktrees(path).into_iter().next().map(|w| w.path)
}

/// Fetches the remote's default branch, quietly: no prompt for a password
/// or key (it fails instead), no gc afterwards, given up after a minute.
/// The remote's default branch without `origin/`: what origin/HEAD names,
/// else main or master when origin has it (a repository given its remote by
/// `git remote add` has no origin/HEAD).
pub(crate) fn remote_default(repo: &Path) -> Option<String> {
    let head = git(repo, &["symbolic-ref", "--short", "refs/remotes/origin/HEAD"]).map(|s| s.trim().to_string());
    if let Some(branch) = head.as_deref().and_then(|h| h.strip_prefix("origin/")) {
        return Some(branch.to_string());
    }
    ["main", "master"]
        .into_iter()
        .find(|b| git(repo, &["rev-parse", "--verify", "-q", &format!("refs/remotes/origin/{b}")]).is_some_and(|s| !s.trim().is_empty()))
        .map(str::to_string)
}

pub fn fetch_base(repo: &Path) -> Result<(), String> {
    let branch = remote_default(repo).ok_or("no origin/main or origin/master")?;
    fetch_quietly(repo, &["fetch", "--quiet", "--no-auto-maintenance", "origin", &branch])
}

/// Every branch of `origin`, those gone from it dropped: for the graph of
/// all branches.
pub fn fetch_all(repo: &Path) -> Result<(), String> {
    if crate::hosts::is_remote(repo) {
        return git_status(repo, &["fetch", "--quiet", "--prune", "origin"]).map(|_| ());
    }
    fetch_quietly(repo, &["fetch", "--quiet", "--no-auto-maintenance", "--prune", "origin"])
}

/// A fetch that never asks for a password and gives up after a minute.
fn fetch_quietly(repo: &Path, args: &[&str]) -> Result<(), String> {
    let mut child = crate::procs::command("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("SSH_ASKPASS_REQUIRE", "never")
        .env("GCM_INTERACTIVE", "never")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| e.to_string())?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    loop {
        match child.try_wait().map_err(|e| e.to_string())? {
            Some(status) if status.success() => return Ok(()),
            Some(status) => return Err(format!("git fetch: {status}")),
            None if std::time::Instant::now() > deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("git fetch: timed out".into());
            }
            None => std::thread::sleep(std::time::Duration::from_millis(200)),
        }
    }
}

/// Where a new task branches from: the remote's default branch, fetched
/// first, else the main worktree's branch.
/// A base the project names: origin's branch of that name, fetched, when
/// there is one; else as it is said (a local branch, a tag, a commit).
fn chosen_base(repo: &Path, base: &str) -> String {
    let name = base.strip_prefix("origin/").unwrap_or(base);
    let _ = git_status(repo, &["fetch", "--quiet", "origin", name]);
    let remote = format!("origin/{name}");
    if git(repo, &["rev-parse", "--verify", "-q", &format!("{remote}^{{commit}}")]).is_some_and(|s| !s.trim().is_empty()) { remote } else { base.to_string() }
}

fn new_task_base(repo: &Path) -> String {
    if let Some(branch) = remote_default(repo) {
        let _ = git_status(repo, &["fetch", "--quiet", "origin", &branch]);
        return format!("origin/{branch}");
    }
    git(repo, &["rev-parse", "--abbrev-ref", "HEAD"]).map(|s| s.trim().to_string()).unwrap_or_else(|| "HEAD".into())
}

pub fn task_worktree_dir(repo: &Path, branch: &str) -> PathBuf {
    task_worktrees(repo).join(branch.replace('/', "-"))
}

/// The folders projects keep their tasks' worktrees in, where their own
/// settings name one.
static WORKTREES_IN: std::sync::RwLock<Option<std::collections::HashMap<PathBuf, PathBuf>>> = std::sync::RwLock::new(None);

/// Where a project's new tasks go: its own folder, set here; else the one
/// Settings names, a folder of the project's name in it; else beside the
/// repository, `<repo>.worktrees`.
pub fn task_worktrees(repo: &Path) -> PathBuf {
    // A remote project's beside it there: Settings and a folder chosen here name places here.
    if crate::hosts::is_remote(repo) {
        return worktrees_beside(repo);
    }
    let name = repo.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "repo".into());
    if let Some(own) = WORKTREES_IN.read().unwrap_or_else(|e| e.into_inner()).as_ref().and_then(|all| all.get(repo).cloned()) {
        return own;
    }
    match crate::state::with_settings(|s| s.worktrees_dir.trim().to_string()) {
        dir if !dir.is_empty() => home_expanded(&dir).join(name),
        _ => worktrees_beside(repo),
    }
}

/// `<repo>.worktrees`: where tasks went before a folder was chosen, and go by default.
pub fn worktrees_beside(repo: &Path) -> PathBuf {
    let name = repo.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "repo".into());
    repo.with_file_name(format!("{name}.worktrees"))
}

/// What a path can be ignored by, for a menu: the path itself (anchored
/// at the root), every file or folder of its name, every file of its
/// extension; each with what it is called there.
/// The first: every file (or folder) of its name, in any folder: a
/// pattern without a slash is matched at every depth. Then this one alone,
/// anchored at the root, and every file of its extension. Each with
/// whether it names this path only.
pub fn ignore_patterns(path: &str, folder: bool) -> Vec<(String, String, bool)> {
    let path = path.trim_end_matches('/');
    let name = path.rsplit('/').next().unwrap_or(path);
    let (slash, what) = if folder { ("/", "folder") } else { ("", "file") };
    let mut all = vec![
        (format!("Every {name}{slash}, in any folder"), format!("{name}{slash}"), false),
        (format!("Only this {what}: /{path}{slash}"), format!("/{path}{slash}"), true),
    ];
    if !folder && let Some((_, ext)) = name.rsplit_once('.').filter(|(stem, ext)| !stem.is_empty() && !ext.is_empty()) {
        all.push((format!("Every *.{ext} file"), format!("*.{ext}"), false));
    }
    all
}

/// Where `ignore` writes: the worktree's .gitignore, shared with the team;
/// the repository's info/exclude, for you alone; git's own file for every
/// repository of this user (`core.excludesFile`, else ~/.config/git/ignore).
#[derive(Clone, Copy, PartialEq)]
pub enum IgnoreIn {
    Shared,
    Local,
    Everywhere,
}

/// Adds `pattern` to the worktree's `.gitignore`, or with `local` to the
/// repository's `info/exclude`, which is not committed; once.
pub fn ignore(root: &Path, pattern: &str, place: IgnoreIn) -> Result<String, String> {
    let file = match place {
        IgnoreIn::Shared => root.join(".gitignore"),
        IgnoreIn::Local => {
            let at = git(root, &["rev-parse", "--git-path", "info/exclude"]).ok_or("not a git repository")?;
            let at = PathBuf::from(at.trim());
            if at.is_absolute() { at } else { root.join(at) }
        }
        IgnoreIn::Everywhere => match git(root, &["config", "--global", "--get", "core.excludesFile"]).map(|s| s.trim().to_string()).filter(|s| !s.is_empty()) {
            Some(file) => home_expanded(&file),
            None => match std::env::var_os("XDG_CONFIG_HOME").filter(|d| !d.is_empty()) {
                Some(config) => PathBuf::from(config).join("git/ignore"),
                None => home_expanded("~/.config/git/ignore"),
            },
        },
    };
    let mut text = std::fs::read_to_string(&file).unwrap_or_default();
    let named = match place {
        IgnoreIn::Shared => ".gitignore".to_string(),
        IgnoreIn::Local => "info/exclude".to_string(),
        IgnoreIn::Everywhere => file.display().to_string(),
    };
    if text.lines().any(|line| line.trim() == pattern) {
        return Ok(format!("{pattern} is in {named} already"));
    }
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    text.push_str(pattern);
    text.push('\n');
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    std::fs::write(&file, text).map_err(|e| format!("{}: {e}", file.display()))?;
    Ok(format!("{pattern} added to {named}"))
}

/// A project's own folder for its tasks, or none for what Settings say.
pub fn set_project_worktrees(repo: &Path, dir: &str) {
    let mut all = WORKTREES_IN.write().unwrap_or_else(|e| e.into_inner());
    let all = all.get_or_insert_with(Default::default);
    match dir.trim() {
        "" => all.remove(repo),
        dir => all.insert(repo.to_path_buf(), home_expanded(dir)),
    };
}

/// `~/x` as the home folder's `x`.
pub fn home_expanded(dir: &str) -> PathBuf {
    match (dir.strip_prefix("~/").or((dir == "~").then_some("")), std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"))) {
        (Some(rest), Some(home)) => PathBuf::from(home).join(rest),
        _ => PathBuf::from(dir),
    }
}

/// Creates the worktree of a new task next to the repository, in
/// `<repo>.worktrees/<branch>`, and copies the main worktree's untracked
/// `.env*` files into it.
/// `base`: where the project's tasks branch from, empty for the remote's default.
pub fn create_task_worktree(repo: &Path, branch: &str, base: &str, copy_env: bool) -> Result<PathBuf, String> {
    let base = if base.trim().is_empty() { new_task_base(repo) } else { chosen_base(repo, base.trim()) };
    worktree_at(repo, branch, &base, copy_env)
}

fn worktree_at(repo: &Path, branch: &str, base: &str, copy_env: bool) -> Result<PathBuf, String> {
    let dir = task_worktree_dir(repo, branch);
    // On the repository's machine (crate::hostfs), by its path there.
    if crate::hostfs::stat(&dir).is_ok() {
        return Err(format!("{} already exists", crate::hosts::there(&dir).display()));
    }
    let dir_s = crate::hosts::there(&dir).to_string_lossy().into_owned();
    git_status(repo, &["worktree", "add", "--no-track", "-b", branch, &dir_s, base])?;
    if copy_env {
        copy_env_files(repo, &dir);
    }
    Ok(dir)
}

/// Worktrees for several tries at one task, all from one commit so their
/// diffs compare: `from`, or where a new task would start, looked up once.
///
/// With the commit they are from: what their diffs are counted against.
pub fn create_variant_worktrees(repo: &Path, branches: &[String], from: Option<&str>, base: &str, copy_env: bool) -> Result<(String, Vec<Result<PathBuf, String>>), String> {
    let base = match from {
        Some(from) => from.to_string(),
        None if !base.trim().is_empty() => chosen_base(repo, base.trim()),
        None => new_task_base(repo),
    };
    let commit = git(repo, &["rev-parse", "--verify", "-q", &format!("{base}^{{commit}}")])
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| format!("{base} is no commit"))?;
    let made = branches.iter().map(|branch| worktree_at(repo, branch, &commit, copy_env)).collect();
    Ok((commit, made))
}

/// The names a new task would run into, read once for asking about many:
/// the repository's branches and the directories its worktrees are in.
#[derive(Default)]
pub struct TakenNames {
    branches: std::collections::HashSet<String>,
    dirs: std::collections::HashSet<String>,
}

impl TakenNames {
    pub fn read(repo: &Path) -> Self {
        let branches = git(repo, &["for-each-ref", "--format=%(refname:short)", "refs/heads"]).unwrap_or_default().lines().map(str::to_string).collect();
        let place = task_worktree_dir(repo, "");
        let dirs = if crate::hosts::is_remote(repo) {
            let there = crate::hosts::there(&place).display().to_string();
            let listed = crate::hostfs::run(repo, "sh", &["-c", r#"ls -1A "$1" 2>/dev/null || true"#, "sh", &there], &[], None);
            listed.map(|out| String::from_utf8_lossy(&out.stdout).lines().map(str::to_string).collect()).unwrap_or_default()
        } else {
            std::fs::read_dir(place).into_iter().flatten().flatten().map(|entry| entry.file_name().to_string_lossy().into_owned()).collect()
        };
        Self { branches, dirs }
    }

    pub fn has(&self, branch: &str) -> bool {
        self.branches.contains(branch) || self.dirs.contains(&branch.replace('/', "-"))
    }
}

/// Where a task's branch left its base.
pub fn fork_point(root: &Path) -> Option<String> {
    let base = base_ref(root)?;
    git(root, &["merge-base", "HEAD", &base]).map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

/// What a task changed since `base` (the commit it was made from; where
/// it forked when that is not known), uncommitted work included: files,
/// lines added, lines removed. New files not yet added count with theirs.
pub fn change_size(root: &Path, base: Option<&str>) -> Option<(usize, usize, usize)> {
    let fork = base.filter(|b| !b.is_empty()).map(str::to_string).or_else(|| fork_point(root))?;
    let (files, mut added, removed) = shortstat(&git(root, &["diff", "--shortstat", &fork])?);
    let untracked = git(root, &["ls-files", "--others", "--exclude-standard", "-z"]).unwrap_or_default();
    let new: Vec<&str> = untracked.split('\0').filter(|f| !f.is_empty()).collect();
    for file in &new {
        // Text of a sensible size; a binary or a dump adds no lines.
        let path = root.join(file);
        if std::fs::metadata(&path).is_ok_and(|m| m.len() < 1_000_000) {
            if let Ok(text) = std::fs::read_to_string(&path) {
                added += text.lines().count();
            }
        }
    }
    Some((files + new.len(), added, removed))
}

/// " 3 files changed, 84 insertions(+), 12 deletions(-)".
fn shortstat(line: &str) -> (usize, usize, usize) {
    let number = |word: &str| {
        line.split(',').find(|part| part.contains(word)).and_then(|part| part.split_whitespace().next()?.parse().ok()).unwrap_or(0)
    };
    (number("file"), number("insertion"), number("deletion"))
}

/// What a worktree changed since `base` as one text, uncommitted work
/// included: the diff, then the new files not yet added, as they are.
pub fn diff_with_new_files(root: &Path, base: &str) -> String {
    const MOST: usize = 1_500_000;
    let mut text = git(root, &["diff", base]).unwrap_or_default();
    let untracked = git(root, &["ls-files", "--others", "--exclude-standard", "-z"]).unwrap_or_default();
    for file in untracked.split('\0').filter(|f| !f.is_empty()) {
        let path = root.join(file);
        if text.len() > MOST || !std::fs::metadata(&path).is_ok_and(|m| m.len() < 200_000) {
            text.push_str(&format!("\nnew file {file} (not shown)\n"));
        } else if let Ok(content) = std::fs::read_to_string(&path) {
            text.push_str(&format!("\nnew file {file}:\n{content}\n"));
        }
    }
    if text.len() > MOST {
        let mut end = MOST;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
        text.push_str("\n… (cut)\n");
    }
    text
}

/// Where tries that were not kept are parked: refs no branch list shows.
pub const PARKED: &str = "refs/ideae/variants";

/// Removes a try's worktree and branch, and keeps what it had, uncommitted
/// work too, under `keep_ref`.
pub fn park_and_remove(repo: &Path, path: &Path, branch: &str, keep_ref: &str) -> Result<(), String> {
    if path.exists() {
        if git(path, &["status", "--porcelain"]).is_some_and(|s| !s.trim().is_empty()) {
            git_status(path, &["add", "-A"])?;
            // An identity of its own and no signing: the user's may not be
            // set, or ask for a key. Not committed is not removed.
            let own = ["-c", "user.name=ideae", "-c", "user.email=ideae@localhost", "-c", "commit.gpgsign=false"];
            git_status(path, &[&own[..], &["commit", "--no-verify", "-q", "-m", "As it was left"]].concat())?;
        }
        // A name of its own: never over what was parked before.
        let free = std::iter::once(keep_ref.to_string())
            .chain((2..).map(|n| format!("{keep_ref}-{n}")))
            .find(|name| git_status(repo, &["rev-parse", "--verify", "-q", name]).is_err())
            .unwrap_or_else(|| keep_ref.to_string());
        git_status(path, &["update-ref", &free, "HEAD"])?;
    }
    remove_task_worktree(repo, path, branch, false).map(|_| ())
}

/// Drops the parked tries whose ref, `…/<unix seconds>-<group>/<letter>`,
/// is older than `seconds`.
pub fn prune_parked(repo: &Path, now: u64, seconds: u64) {
    let Some(refs) = git(repo, &["for-each-ref", "--format=%(refname)", PARKED]) else { return };
    for name in refs.lines() {
        let parked = name.strip_prefix(PARKED).and_then(|rest| rest.trim_start_matches('/').split('-').next()?.parse::<u64>().ok());
        if parked.is_some_and(|at| now.saturating_sub(at) > seconds) {
            let _ = git_status(repo, &["update-ref", "-d", name]);
        }
    }
}

pub(crate) fn copy_env_files(from: &Path, to: &Path) {
    let Some(out) = git(from, &["ls-files", "--others", "--ignored", "--exclude-standard", "-z", "--", ":(glob)**/.env*"])
    else {
        return;
    };
    // There: copied by its shell, in one go.
    if crate::hosts::is_remote(from) {
        copy_there(from, to, out.split('\0').filter(|f| !f.is_empty()));
        return;
    }
    for rel in out.split('\0').filter(|f| !f.is_empty()) {
        let target = to.join(rel);
        if target.exists() {
            continue;
        }
        if let Some(dir) = target.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::copy(from.join(rel), &target);
    }
}

/// Files of `from` (relative paths) into `to`, where it has none of its own,
/// on their machine, by its shell in one go: for a remote project's worktrees.
pub(crate) fn copy_there<'a>(from: &Path, to: &Path, rels: impl Iterator<Item = &'a str>) {
    let (from_s, to_s) = (crate::hosts::there(from).display().to_string(), crate::hosts::there(to).display().to_string());
    let mut args: Vec<&str> = vec!["-c", r#"f=$1; t=$2; shift 2; for r; do [ -e "$t/$r" ] || { mkdir -p "$(dirname "$t/$r")" && cp -p "$f/$r" "$t/$r"; }; done"#, "sh", from_s.as_str(), to_s.as_str()];
    let rels: Vec<&str> = rels.collect();
    args.extend(rels.iter().copied());
    let _ = crate::hostfs::run(from, "sh", &args, &[], None);
}

/// The file as the last commit has it, or with `since_fork` as the commit
/// the branch left its base at has it. Empty for a file that commit lacks:
/// every line of it is new. None for what git ignores, or outside a
/// repository: there is nothing to compare such a file with.
pub fn committed_text(root: &Path, file: &Path, since_fork: bool) -> Option<String> {
    let rel = file.strip_prefix(root).ok()?.to_string_lossy().replace('\\', "/");
    let commit = since_fork.then(|| fork_point(root)).flatten().unwrap_or_else(|| "HEAD".into());
    if let Ok(text) = git_status(root, &["show", &format!("{commit}:./{rel}")]) {
        return Some(text);
    }
    let in_repository = git_status(root, &["rev-parse", "--is-inside-work-tree"]).is_ok();
    let ignored = git_status(root, &["check-ignore", "-q", "--", &rel]).is_ok();
    (in_repository && !ignored).then(String::new)
}

/// A new name for the branch a worktree is on.
pub fn rename_branch(root: &Path, from: &str, to: &str) -> Result<(), String> {
    git_status(root, &["branch", "-m", from, to]).map(|_| ())
}

/// Whether the repository has a branch of this name.
pub fn branch_exists(repo: &Path, branch: &str) -> bool {
    git_status(repo, &["rev-parse", "--verify", "-q", &format!("refs/heads/{branch}")]).is_ok()
}

/// Whether the branch was pushed: it follows a branch of a remote, or the
/// remote has one of its name (a push without `-u`).
pub fn has_upstream(root: &Path) -> bool {
    if git_status(root, &["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{u}"]).is_ok() {
        return true;
    }
    git(root, &["rev-parse", "--abbrev-ref", "HEAD"])
        .is_some_and(|branch| git_status(root, &["rev-parse", "--verify", "-q", &format!("refs/remotes/origin/{}", branch.trim())]).is_ok())
}

/// A branch name like `name` that neither this repository nor its origin
/// has: `name`, else `name-2`, `name-3`…
pub fn free_branch_name(repo: &Path, name: &str) -> String {
    let taken = |b: &str| {
        branch_exists(repo, b)
            || git_status(repo, &["rev-parse", "--verify", "-q", &format!("refs/remotes/origin/{b}")]).is_ok()
            || crate::hostfs::stat(&task_worktree_dir(repo, b)).is_ok()
    };
    std::iter::once(name.to_string()).chain((2..100).map(|n| format!("{name}-{n}"))).find(|b| !taken(b)).unwrap_or_else(|| format!("{name}-{}", crate::state::now()))
}

/// A task's worktree on a branch of origin, fetched first: the branch of a
/// pull request that is still open. The local branch follows origin's.
pub fn remote_branch_worktree(repo: &Path, branch: &str, copy_env: bool) -> Result<PathBuf, String> {
    fetch_quietly(repo, &["fetch", "--quiet", "--no-auto-maintenance", "origin", &format!("+refs/heads/{branch}:refs/remotes/origin/{branch}")])?;
    let dir = worktree_at(repo, branch, &format!("origin/{branch}"), copy_env)?;
    let _ = git_status(&dir, &["branch", "--set-upstream-to", &format!("origin/{branch}")]);
    Ok(dir)
}

/// Removes a worktree an agent made for itself. Its branch goes too when
/// everything on it is on a remote or another branch; else it stays, and
/// the reason is returned.
pub fn remove_side_worktree(repo: &Path, path: &Path, branch: &str) -> Result<Option<String>, String> {
    let named = !branch.is_empty() && !branch.starts_with('(');
    let own = if named { Some(format!("--exclude={branch}")) } else { None };
    let mut args = vec!["rev-list", "--count", "HEAD", "--not", "--remotes"];
    if let Some(own) = own.as_deref() {
        args.push(own);
    }
    args.push("--branches");
    let only_here: u32 = if path.exists() { git(path, &args).and_then(|n| n.trim().parse().ok()).unwrap_or(0) } else { 0 };
    remove_task_worktree(repo, path, "", false)?;
    if !named {
        return Ok(None);
    }
    if only_here > 0 {
        return Ok(Some(format!("The branch {branch} stays: {only_here} commit{} on it {} on no remote and no other branch.", if only_here == 1 { "" } else { "s" }, if only_here == 1 { "is" } else { "are" })));
    }
    git_status(repo, &["branch", "-D", branch]).map(|_| None)
}

/// Removes a task's worktree and its branch `branch`, the one the task
/// owns, not whatever the worktree is on; a lock is overridden only when
/// `unlock`. The branch stays when it has commits that neither the
/// worktree's HEAD nor a remote or another branch has, and the reason is
/// returned.
pub fn remove_task_worktree(repo: &Path, path: &Path, branch: &str, unlock: bool) -> Result<Option<String>, String> {
    let named = !branch.is_empty() && !branch.starts_with('(');
    // Where the worktree's machine is (crate::hostfs), and by its path there.
    let exists = crate::hostfs::stat(path).is_ok();
    let only_there: u32 = if named && exists && branch_exists(repo, branch) {
        let exclude = format!("--exclude={branch}");
        git(path, &["rev-list", "--count", &format!("refs/heads/{branch}"), "--not", "HEAD", "--remotes", &exclude, "--branches"])
            .and_then(|n| n.trim().parse().ok())
            .unwrap_or(0)
    } else {
        0
    };
    // A chat agent's conversation goes with its task (an archive keeps its copy).
    let journal = crate::acp::journal_path(path);
    let _ = std::fs::remove_file(&journal);
    let log = crate::acp::host::log_path(&journal);
    let _ = std::fs::remove_file(log.with_extension("log.old"));
    let _ = std::fs::remove_file(log);
    // Its chat's checkpoints: refs of the repository, not of the worktree.
    crate::acp::checkpoint::forget(repo, &crate::acp::checkpoint::thread_name(Some(&journal)));
    // So do its browser's logins.
    crate::browser::forget(path);
    let path_s = crate::hosts::there(path).to_string_lossy().into_owned();
    if exists {
        let mut args = vec!["worktree", "remove", "--force"];
        if unlock {
            args.push("--force");
        }
        args.push(&path_s);
        git_status(repo, &args)?;
    } else {
        let _ = git_status(repo, &["worktree", "prune"]);
    }
    if !named {
        return Ok(None);
    }
    if only_there > 0 {
        return Ok(Some(format!(
            "The branch {branch} stays: {only_there} commit{} on it {} on no remote, no other branch and not where the worktree was.",
            if only_there == 1 { "" } else { "s" },
            if only_there == 1 { "is" } else { "are" }
        )));
    }
    let _ = git_status(repo, &["branch", "-D", branch]);
    Ok(None)
}

#[cfg(test)]
mod variant_tests {
    use super::*;

    // Scripted with bash, which Windows lacks or has as WSL's.
    #[cfg(unix)]
    fn sh(dir: &Path, script: &str) {
        let ok = std::process::Command::new("bash").arg("-ec").arg(script).current_dir(dir).status().unwrap().success();
        assert!(ok, "{script}");
    }

    #[cfg(unix)]
    #[test]
    fn variants_fork_from_one_commit_and_the_ones_not_kept_are_parked() {
        let base = std::env::temp_dir().join(format!("ideae-variants-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        sh(&base, "git init -q -b main repo; cd repo; git config user.email t@t; git config user.name t
            printf 'one\\n' > f.txt; git add f.txt; git commit -qm base");
        let repo = base.join("repo");
        let branches = ["fix-a".to_string(), "fix-b".to_string()];
        let (from, made) = create_variant_worktrees(&repo, &branches, None, "", false).unwrap();
        let made: Vec<PathBuf> = made.into_iter().map(Result::unwrap).collect();
        let taken = TakenNames::read(&repo);
        assert!(taken.has("fix-a") && taken.has("main") && !taken.has("fix-c"));
        let head = |dir: &Path| git(dir, &["rev-parse", "HEAD"]).unwrap();
        assert_eq!(head(&made[0]), head(&made[1]));
        assert_eq!((head(&made[0]), from.clone()), (head(&repo), head(&repo).trim().to_string()));

        // A commits, B leaves its work uncommitted, with a new file.
        sh(&made[0], "printf 'one\\ntwo\\n' > f.txt; git commit -qam more");
        sh(&made[1], "printf 'ONE\\n' > f.txt; echo new > g.txt");
        // No remote here: the base is main itself.
        assert_eq!(change_size(&made[0], Some(&from)), Some((1, 1, 0)));
        assert_eq!(change_size(&made[1], None), Some((2, 2, 1)), "the new file's line too");
        // A later try at the task starts where the first did.
        let later = create_variant_worktrees(&repo, &["fix-c".to_string()], fork_point(&made[0]).as_deref(), "", false).unwrap().1.remove(0).unwrap();
        assert_eq!(head(&later), head(&repo));

        // What the gutter compares a file with: the last commit, or where the branch forked.
        assert_eq!(committed_text(&made[0], &made[0].join("f.txt"), false).as_deref(), Some("one\ntwo\n"), "A committed its line");
        assert_eq!(committed_text(&made[0], &made[0].join("f.txt"), true).as_deref(), Some("one\n"), "before the branch");
        assert_eq!(committed_text(&made[1], &made[1].join("g.txt"), false).as_deref(), Some(""), "a new file: all of it is new");
        sh(&later, "echo 'build/' > .gitignore; mkdir build; echo x > build/out.txt");
        assert_eq!(committed_text(&later, &later.join("build/out.txt"), false), None, "what git ignores is compared with nothing");
        assert_eq!(committed_text(&base, &base.join("state.json"), false), None, "nor what is in no repository");

        // One change of two in a file is taken back, then staged, the other stays.
        sh(&repo, "seq 1 30 > n.txt; git add n.txt; git commit -qm numbers; sed -i.bak -e 's/^2$/two/' -e 's/^28$/twenty-eight/' n.txt; rm n.txt.bak");
        let found = hunks(&working_patch(&repo));
        assert_eq!(found.iter().map(|h| (h.path.as_str(), h.line, h.modified)).collect::<Vec<_>>(), [("n.txt", 2, true), ("n.txt", 28, true)]);
        apply_hunk(&repo, &found[0].patch(), true, false).unwrap();
        let text = std::fs::read_to_string(repo.join("n.txt")).unwrap();
        assert!(text.starts_with("1\n2\n3\n") && text.contains("twenty-eight"), "the first is back, the second is there");
        apply_hunk(&repo, &hunks(&working_patch(&repo))[0].stage(), false, true).unwrap();
        assert!(git(&repo, &["diff", "--cached"]).unwrap().contains("+twenty-eight"), "staged");
        assert!(apply_hunk(&repo, &found[0].patch(), true, false).is_err(), "what is no longer there cannot be taken back");
        sh(&repo, "git reset -q --hard; rm -f gone.txt; echo new > untracked.txt");
        assert!(hunks(&working_patch(&repo)).is_empty(), "a new file is no hunk");

        // Changes a line apart are one hunk to git, and still each its own:
        // a line changed, one removed, two added.
        sh(&repo, "git checkout -q -- n.txt; git reset -q; printf '1\\ntwo\\n3\\n5\\n6\\nsix\\nand a half\\n' > m.txt; seq 7 12 >> m.txt
            seq 1 12 > n.txt; git add n.txt; git commit -qm short; mv m.txt n.txt");
        let found = hunks(&working_patch(&repo));
        assert_eq!(found.iter().map(|h| (h.line, h.modified)).collect::<Vec<_>>(), [(2, true), (4, false), (7, true)]);
        let numbers = |repo: &Path| std::fs::read_to_string(repo.join("n.txt")).unwrap().replace('\n', " ");
        apply_hunk(&repo, &found[1].patch(), true, false).unwrap();
        assert_eq!(numbers(&repo), "1 two 3 4 5 6 six and a half 7 8 9 10 11 12 ", "the removed line is back, alone");
        apply_hunk(&repo, &found[2].stage(), false, true).unwrap();
        assert_eq!(git(&repo, &["diff", "--cached", "--numstat"]).unwrap().trim(), "2\t0\tn.txt", "only the two added lines are staged");
        let left = hunks(&working_patch(&repo));
        assert_eq!(left.len(), 2, "the patch is against the commit: what is staged is still in it");
        // Its neighbour in the same hunk is staged after it, then the first is taken out again.
        stage_change(&repo, &left[0], true).unwrap();
        assert_eq!(git(&repo, &["diff", "--cached", "--numstat"]).unwrap().trim(), "3\t1\tn.txt");
        assert_eq!(hunks(&refresh(&repo, None, false, &mut Known::default()).patches.unwrap().staged).len(), 2);
        stage_change(&repo, &found[2], false).unwrap();
        assert_eq!(git(&repo, &["diff", "--cached", "--numstat"]).unwrap().trim(), "1\t1\tn.txt", "the changed line stays staged");
        assert!(stage_change(&repo, &found[2], false).is_err(), "it is not staged any more");
        stage_change(&repo, &left[0], false).unwrap();
        assert_eq!(refresh(&repo, None, false, &mut Known::default()).patches.unwrap().staged, "", "nothing staged: not asked for");
        apply_hunk(&repo, &left[0].patch(), true, false).unwrap();
        assert_eq!(numbers(&repo), "1 2 3 4 5 6 six and a half 7 8 9 10 11 12 ");
        sh(&repo, "rm untracked.txt");

        assert!(!has_upstream(&made[0]));
        let parked = format!("{PARKED}/1000-fix/b");
        park_and_remove(&repo, &made[1], "fix-b", &parked).unwrap();
        assert!(!made[1].exists() && !branch_exists(&repo, "fix-b"));
        let kept = git(&repo, &["show", "--stat", "--format=", &parked]).unwrap();
        assert!(kept.contains("f.txt") && kept.contains("g.txt"), "what it had, uncommitted work too: {kept}");

        assert!(!branch_exists(&repo, "fix"));
        rename_branch(&made[0], "fix-a", "fix").unwrap();
        assert!(branch_exists(&repo, "fix") && !branch_exists(&repo, "fix-a"));

        prune_parked(&repo, 1000 + 10, 100);
        assert!(git(&repo, &["rev-parse", "--verify", "-q", &parked]).is_some(), "still young");
        prune_parked(&repo, 1000 + 200, 100);
        assert!(git(&repo, &["rev-parse", "--verify", "-q", &parked]).is_none_or(|s| s.trim().is_empty()), "gone when old");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn shortstat_reads_what_is_there() {
        assert_eq!(shortstat(" 3 files changed, 84 insertions(+), 12 deletions(-)\n"), (3, 84, 12));
        assert_eq!(shortstat(" 1 file changed, 1 deletion(-)\n"), (1, 0, 1));
        assert_eq!(shortstat(""), (0, 0, 0));
    }
}

#[cfg(all(test, unix))]
mod refresh_tests {
    use super::*;
    use gpui_kit::component::diff::{DiffFile, DiffFileStatus};

    fn sh(dir: &Path, script: &str) {
        let ok = Command::new("bash").arg("-ec").arg(script).current_dir(dir).status().unwrap().success();
        assert!(ok, "{script}");
    }

    #[test]
    fn status_tells_the_branch_the_states_and_the_untracked() {
        let out = "# branch.oid abc\0# branch.head task\0# branch.upstream origin/task\0# branch.ab +2 -1\0\
            1 .M N... 100644 100644 100644 aaa bbb src/a b.rs\0\
            1 A. N... 000000 100644 100644 000 bbb new.rs\0\
            1 .D N... 100644 100644 000000 aaa aaa gone.rs\0\
            2 R. N... 100644 100644 100644 aaa bbb R100 to here.rs\0from.rs\0\
            u UU N... 100644 100644 100644 100644 a b c both.rs\0\
            ? notes/todo.md\0! target/\0! .env\0";
        let status = parse_status(out);
        assert_eq!((status.info.head.as_str(), status.info.upstream.as_deref(), status.info.ahead, status.info.behind), ("task", Some("origin/task"), 2, 1));
        assert_eq!(status.oid, "abc");
        let state = |path: &str| status.states.get(path).map(|s| (s.staged, s.unstaged));
        assert_eq!(state("src/a b.rs"), Some((false, true)));
        assert_eq!(state("new.rs"), Some((true, false)));
        assert_eq!(state("to here.rs"), Some((true, false)));
        assert_eq!(state("both.rs"), Some((true, true)));
        assert_eq!(state("notes/todo.md"), Some((false, true)));
        assert_eq!((status.states.len(), status.untracked.as_slice()), (6, ["notes/todo.md".to_string()].as_slice()));
        let change = |path: &str| status.states.get(path).map(|s| s.change);
        assert_eq!(
            ["src/a b.rs", "new.rs", "to here.rs", "both.rs", "notes/todo.md", "gone.rs"].map(change),
            [Change::Modified, Change::Added, Change::Modified, Change::Conflict, Change::Untracked, Change::Deleted].map(Some)
        );
        assert_eq!(status.ignored, ["target/", ".env"]);
    }

    /// The repository's own checkout on a branch the remote took in by a
    /// squash: told, and switched with the branch gone; on one with work of
    /// its own, moved to a worktree with what was not committed yet.
    #[test]
    fn the_main_checkout_off_its_default_branch() {
        let base = std::env::temp_dir().join(format!("ideae-off-default-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        sh(&base, "git init -q --bare -b main remote.git; git clone -q remote.git a 2>/dev/null; cd a
            git config user.email t@t; git config user.name t
            echo one > f.txt; git add .; git commit -qm base; git push -q origin main
            git checkout -qb old; echo two >> f.txt; git commit -qam two; echo three >> f.txt; git commit -qam three
            git checkout -q main; git merge -q --squash old; git commit -qm squashed; git push -q origin main
            git reset -q --hard HEAD~1; git checkout -q old");
        let root = base.join("a").canonicalize().unwrap();
        let read = refresh(&root, None, false, &mut Known::default());
        let off = read.info.off_default.clone().expect("off the default branch");
        assert_eq!(off, OffDefault { default: "main".into(), base: "origin/main".into(), contained: true });
        switch_to_default(&root, &off, "old").unwrap();
        let read = refresh(&root, None, false, &mut Known::default());
        assert_eq!((read.info.head.as_str(), read.info.off_default.is_none(), read.info.last_subject.as_str()), ("main", true, "squashed"));
        assert!(git(&root, &["rev-parse", "--verify", "-q", "refs/heads/old"]).is_none_or(|out| out.trim().is_empty()), "old is deleted");

        sh(&root, "git checkout -qb work; echo four >> f.txt; git commit -qam four; echo five >> f.txt; echo new > new.txt");
        let off = refresh(&root, None, false, &mut Known::default()).info.off_default.unwrap();
        assert!(!off.contained);
        let dir = move_to_task(&root, &off, "work", false).unwrap();
        assert_eq!(dir, task_worktree_dir(&root, "work"));
        let read = refresh(&root, None, false, &mut Known::default());
        assert!(read.info.head == "main" && read.states.is_empty(), "the checkout is on main and clean");
        let moved = refresh(&dir, None, false, &mut Known::default());
        assert_eq!((moved.info.head.as_str(), moved.info.off_default.is_none()), ("work", true), "a worktree of a task is not told");
        let f = std::fs::read_to_string(dir.join("f.txt")).unwrap();
        assert!(f.lines().map(str::trim).eq(["one", "two", "three", "four", "five"]) && dir.join("new.txt").exists(), "{f}");
        let _ = std::fs::remove_dir_all(&base);
        let _ = std::fs::remove_dir_all(dir.parent().unwrap());
    }

    /// A refresh of a real worktree: what it reads, and its patch of new
    /// files against the one git makes itself.
    #[test]
    fn a_refresh_reads_the_worktree_as_git_shows_it() {
        let base = std::env::temp_dir().join(format!("ideae-refresh-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        sh(&base, "git init -q --bare -b main remote.git; git clone -q remote.git a 2>/dev/null; cd a
            git config user.email t@t; git config user.name t; mkdir src
            printf 'one\\ntwo\\n' > f.txt; echo x > src/staged.txt; git add .; git commit -qm base; git push -q origin main
            git checkout -qb task
            echo three >> f.txt; echo y >> src/staged.txt; git add src/staged.txt
            printf 'new\\nfile\\n' > 'new one.txt'; printf 'no newline' > src/tail.txt; : > empty.txt
            printf 'a\\0b' > data.bin; printf '#!/bin/sh\\n' > run.sh; chmod +x run.sh; ln -s f.txt link
            head -c 700000 /dev/zero | tr '\\0' 'x' > big.txt");
        let root = base.join("a").canonicalize().unwrap();
        let mut known = Known::default();
        let mut read = refresh(&root, None, false, &mut known);
        let Patches { working, branch, .. } = read.patches.take().unwrap();

        assert_eq!((read.info.head.as_str(), read.info.base.as_deref(), read.info.has_origin, read.info.ahead), ("task", Some("origin/main"), true, 0));
        assert!(!read.info.last_hash.is_empty() && read.info.last_subject == "base");
        assert!(read.files.contains(&"src/tail.txt".to_string()) && read.files.contains(&"f.txt".to_string()));
        assert!(read.watch.0.contains(&root) && read.watch.0.contains(&root.join("src")) && read.watch.0.contains(&root.join(".git")));
        assert_eq!(read.watch.1, vec![root.join(".git/refs")]);
        let state = |path: &str| read.states.get(path).map(|s| (s.staged, s.unstaged));
        assert_eq!((state("f.txt"), state("src/staged.txt"), state("new one.txt")), (Some((false, true)), Some((true, false)), Some((false, true))));
        assert_eq!((read.base.as_deref(), branch == working), (Some("origin/main"), true), "no commit of its own: one patch");
        assert_eq!(*read.files, files(&root), "the tracked files and the new ones of the status are all of them");

        // Nothing changed: git is not asked for the patches again, by a task in view or out of it.
        let again = refresh(&root, None, false, &mut known);
        assert!(again.patches.is_none() && again.files == read.files && again.info == read.info && again.states == read.states);
        assert!(Arc::ptr_eq(&again.files, &read.files) && Arc::ptr_eq(&again.watch.0, &read.watch.0), "no file came or went: the list is the one made before, not another like it");
        assert!(refresh(&root, None, true, &mut known).patches.is_none());
        // The patches of a task out of view, and those of the few files named to git, are the same.
        let light = refresh(&root, None, true, &mut Known::default()).patches.unwrap();
        assert!(light.working == working && light.branch == branch);
        let named = changed_patch(&root, "", &["f.txt".to_string(), "src/staged.txt".to_string()], false);
        assert_eq!(named, tracked_patch(&root, "HEAD", false));
        // A changed file changes again, the status the same: asked.
        std::thread::sleep(std::time::Duration::from_millis(20));
        sh(&root, "echo four >> f.txt");
        let changed = refresh(&root, None, false, &mut known).patches.expect("another patch");
        assert!(changed.working.contains("+four") && changed.working != working);
        sh(&root, "printf 'one\\ntwo\\nthree\\n' > f.txt");
        let working = refresh(&root, None, false, &mut known).patches.unwrap().working;

        let ours = DiffFile::parse(&working).unwrap();
        // The same files as git makes them, one process each.
        let mut theirs = git(&root, &["diff", "HEAD"]).unwrap();
        for file in ["big.txt", "data.bin", "empty.txt", "link", "new one.txt", "run.sh", "src/tail.txt"] {
            theirs.push_str(&git(&root, &["diff", "--no-index", "--", "/dev/null", file]).unwrap());
        }
        let theirs = DiffFile::parse(&theirs).unwrap();
        let shape = |files: &[DiffFile]| {
            let mut all: Vec<_> = files.iter().map(|f| (f.path().to_string(), matches!(f.status(), DiffFileStatus::Added), f.additions(), f.deletions())).collect();
            all.sort();
            all
        };
        let (mut ours, theirs) = (shape(&ours), shape(&theirs));
        // Too large to show: named, where git gives all of it.
        let big = ours.iter_mut().find(|f| f.0 == "big.txt").unwrap();
        assert_eq!((big.1, big.2), (true, 0));
        big.2 = 1;
        assert_eq!(ours, theirs);
        assert!(working.len() < 10_000, "the large file is not in the patch");
        assert_eq!(first_changed_lines(&working).get("new one.txt"), Some(&1));

        // A renamed file, staged: named to git with where it was, it is a rename as in the whole diff.
        sh(&root, "git mv f.txt g.txt");
        let renamed = refresh(&root, None, false, &mut known).patches.unwrap().working;
        assert!(renamed.contains("rename from f.txt") && renamed.starts_with(&tracked_patch(&root, "HEAD", false)));
        sh(&root, "git mv g.txt f.txt");

        // A commit of its own: the branch's patch has it, the working one does not.
        sh(&root, "git commit -qm task");
        let mut read = refresh(&root, None, false, &mut known);
        assert_eq!((read.info.ahead, read.info.last_subject.as_str()), (1, "task"), "the commit is seen, what was known of the one before is not kept");
        let patches = read.patches.take().unwrap();
        let (working, branch) = (DiffFile::parse(&patches.working).unwrap(), DiffFile::parse(&patches.branch).unwrap());
        assert!(!working.iter().any(|f| f.path().as_ref() == "src/staged.txt") && branch.iter().any(|f| f.path().as_ref() == "src/staged.txt"));
        assert!(branch.iter().any(|f| f.path().as_ref() == "new one.txt"), "untracked files are in both");
        // Compared with a branch picked by hand.
        assert_eq!(refresh(&root, Some("main".into()), false, &mut known).base.as_deref(), Some("main"));
        // The base moves (a fetch): how far behind it is, is counted again.
        sh(&base, "git clone -q remote.git c 2>/dev/null; cd c; git config user.email t@t; git config user.name t; echo z > z.txt; git add z.txt; git commit -qm more; git push -q origin main");
        sh(&root, "git fetch -q");
        let read = refresh(&root, None, false, &mut known);
        assert_eq!((read.info.behind_base, read.info.base_conflicts.len()), (1, 0), "a file of its own: no conflict");
        // The base changes a line this branch changed too: told before a rebase is tried.
        sh(&base, "cd c; echo theirs > f.txt; git commit -qam clash; git push -q origin main");
        sh(&root, "echo ours > f.txt; git commit -qam ours; git fetch -q");
        let read = refresh(&root, None, false, &mut known);
        assert_eq!((read.info.behind_base, read.info.base_conflicts.clone()), (2, vec!["f.txt".to_string()]));
        let _ = std::fs::remove_dir_all(&base);
    }
}

#[cfg(all(test, unix))]
mod blame_tests {
    use super::*;

    #[test]
    fn lines_of_one_commit_share_what_is_known_of_it() {
        let base = std::env::temp_dir().join(format!("ideae-blame-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let script = "git init -q -b main .; git config user.email t@t; git config user.name Ann; printf 'one\\ntwo\\nthree\\n' > f.txt; git add f.txt; git commit -qm first
            git config user.name Bob; printf 'one\\nTWO\\nthree\\n' > f.txt; git commit -qam second
            git config user.name Cy; printf 'one\\r\\nTWO\\r\\n  three\\r\\n' > f.txt; git commit -qam endings";
        assert!(Command::new("bash").arg("-ec").arg(script).current_dir(&base).status().unwrap().success());
        let lines = blame(&base, &base.join("f.txt"));
        let told: Vec<(&str, &str)> = lines.iter().map(|l| l.as_ref().map(|l| (l.author.as_str(), l.summary.as_str())).unwrap()).collect();
        assert_eq!(told, [("Ann", "first"), ("Bob", "second"), ("Ann", "first")]);
        assert!(std::sync::Arc::ptr_eq(lines[0].as_ref().unwrap(), lines[2].as_ref().unwrap()));
        assert!(lines[0].as_ref().unwrap().time > 0 && lines[1].as_ref().unwrap().hash.len() == 40);
        let _ = std::fs::remove_dir_all(&base);
    }
}

#[cfg(all(test, unix))]
mod edge_tests {
    use super::*;

    fn sh(dir: &Path, script: &str) {
        let ok = Command::new("bash").arg("-ec").arg(script).current_dir(dir).status().unwrap().success();
        assert!(ok, "{script}");
    }

    fn out(dir: &Path, script: &str) -> String {
        String::from_utf8_lossy(&Command::new("bash").arg("-ec").arg(script).current_dir(dir).output().unwrap().stdout).into_owned()
    }

    fn repo(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ideae-edge-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        sh(&dir, "git init -q -b main .; git config user.email t@t; git config user.name t");
        dir.canonicalize().unwrap()
    }

    /// The last commit is undone: its changes staged again, the files as
    /// they were, its message given back. Not another commit than the one
    /// asked about, and not the first.
    #[test]
    fn the_last_commit_is_taken_back_with_its_changes_staged() {
        let dir = repo("undo");
        sh(&dir, "echo one > a.txt; git add a.txt; git commit -qm base; echo two >> a.txt; echo new > b.txt; git add -A; git commit -qm 'second thoughts' -m 'and why'; echo three >> a.txt");
        let (base, second) = (out(&dir, "git rev-parse --short HEAD~1").trim().to_string(), out(&dir, "git rev-parse --short HEAD").trim().to_string());
        assert_eq!(undo_commit(&dir, &base).unwrap_err(), "the last commit is another one now");
        let told = undo_commit(&dir, &second).unwrap();
        assert_eq!(told, format!("Undone {second}: its changes are staged again\nsecond thoughts\n\nand why"));
        assert_eq!(out(&dir, "git rev-parse --short HEAD").trim(), base);
        assert_eq!(out(&dir, "git diff --cached --name-only"), "a.txt\nb.txt\n", "what it changed is staged");
        assert_eq!(std::fs::read_to_string(dir.join("a.txt")).unwrap(), "one\ntwo\nthree\n", "no file changed, nor what was not committed yet");
        assert!(undo_commit(&dir, &base).unwrap_err().starts_with("it is the first commit"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// One change of a file that is also renamed and made executable is
    /// taken back, staged and unstaged alone: the name and the mode stay.
    #[test]
    fn a_change_of_a_renamed_file_alone() {
        let dir = repo("rename");
        sh(&dir, "seq 1 30 > old.txt; git add old.txt; git commit -qm base
            git mv old.txt new.txt; chmod +x new.txt; sed -i.bak -e 's/^2$/two/' -e 's/^28$/twenty-eight/' new.txt; rm new.txt.bak");
        let found = hunks(&working_patch(&dir));
        assert_eq!(found.iter().map(|h| h.path.as_str()).collect::<Vec<_>>(), ["new.txt", "new.txt"]);
        apply_hunk(&dir, &found[0].patch(), true, false).unwrap();
        assert!(!dir.join("old.txt").exists(), "the rename is not taken back with the line");
        assert_eq!(out(&dir, "sed -n 2p new.txt; test -x new.txt && echo x").trim(), "2\nx", "the line is, the mode is not");
        // The other one into the index and out again, the file there under its new name.
        let left = hunks(&working_patch(&dir));
        stage_change(&dir, &left[0], true).unwrap();
        assert!(out(&dir, "git diff --cached").contains("+twenty-eight"));
        stage_change(&dir, &left[0], false).unwrap();
        assert!(!out(&dir, "git diff --cached").contains("+twenty-eight") && out(&dir, "git status --short").starts_with("R"), "unstaged, still renamed");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Names git would quote, a file that is not UTF-8, a repository of its
    /// own in a directory, and a user whose git shows diffs their own way.
    #[test]
    fn names_encodings_and_the_users_own_git_settings() {
        let dir = repo("names");
        sh(&dir, "seq 1 9 > 'é.rs'; seq 1 9 > 'sp ace.rs'; seq 1 9 > ok.txt; printf 'caf\\xe9\\nline\\n' > latin.txt; git add .; git commit -qm base
            for f in 'é.rs' 'sp ace.rs' ok.txt; do sed -i.bak 's/^5$/five/' \"$f\"; rm \"$f.bak\"; done; printf 'caf\\xe9\\nmore\\n' > latin.txt
            mkdir nested; git init -q nested; echo x > nested/x.txt
            git config diff.noprefix true; git config diff.mnemonicPrefix true; git config color.ui always; git config diff.context 0");
        let read = refresh(&dir, None, false, &mut Known::default());
        assert!(!read.files.iter().any(|file| file.starts_with("nested")), "another repository's directory is no file of this one");
        let patches = read.patches.unwrap();
        // The Latin-1 file is shown, lossy, and the others with it.
        for name in ["é.rs", "sp ace.rs", "ok.txt", "latin.txt"] {
            assert!(patches.working.contains(&format!("+++ b/{name}")), "{name} in the patch");
        }
        let found = hunks(&patches.working);
        let mut paths: Vec<&str> = found.iter().map(|h| h.path.as_str()).collect();
        paths.sort();
        assert_eq!(paths, ["ok.txt", "sp ace.rs", "é.rs"], "a change of each, none of the file that would not apply");
        let lines = first_changed_lines(&patches.working);
        assert!(lines.contains_key("é.rs") && lines.contains_key("sp ace.rs"), "{lines:?}");
        for hunk in &found {
            stage_change(&dir, hunk, true).unwrap_or_else(|e| panic!("staging {}: {e}", hunk.path));
        }
        let staged = hunks(&refresh(&dir, None, false, &mut Known::default()).patches.unwrap().staged);
        assert_eq!(staged.len(), 3, "and found staged, whatever prefixes and colours the user's git has");
        stage_change(&dir, &found[0], false).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Work that is not committed and would not fit back on the new base:
    /// the rebase is not begun, and the files are as the agent left them.
    #[test]
    fn a_rebase_that_would_break_uncommitted_work_is_not_begun() {
        let dir = repo("rebase");
        sh(&dir, "printf 'one\\ntwo\\nthree\\n' > f.txt; echo x > g.txt; git add .; git commit -qm base; git branch base
            git checkout -qb task; echo mine > mine.txt; git add mine.txt; git commit -qm mine
            git checkout -q base; printf 'one\\nTWO\\nthree\\n' > f.txt; git commit -qam theirs; git checkout -q task");
        // The same line as the base changed, not committed.
        sh(&dir, "printf 'one\\n2\\nthree\\n' > f.txt");
        let head = out(&dir, "git rev-parse HEAD");
        assert!(matches!(rebase_onto(&dir, "base"), Err(RebaseError::Conflicts(files)) if files == ["f.txt"]));
        assert_eq!(out(&dir, "cat f.txt"), "one\n2\nthree\n", "no conflict markers in it");
        assert_eq!((out(&dir, "git rev-parse HEAD"), out(&dir, "git stash list")), (head, String::new()), "nothing moved, nothing set aside");
        // Work that does fit goes along.
        sh(&dir, "git checkout -q f.txt; echo more >> g.txt");
        assert!(rebase_onto(&dir, "base").is_ok());
        assert_eq!((out(&dir, "sed -n 2p f.txt").trim(), out(&dir, "tail -1 g.txt").trim()), ("TWO", "more"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Before the first commit what is added shows as new; and a file whose
    /// changes are megabytes is named, not shown.
    #[test]
    fn a_repository_without_commits_and_a_file_too_large_to_show() {
        let dir = repo("first");
        sh(&dir, "echo one > a.txt; git add a.txt; echo two > b.txt");
        let patches = refresh(&dir, None, false, &mut Known::default()).patches.unwrap();
        assert!(patches.working.contains("+++ b/a.txt") && patches.working.contains("+one") && patches.working.contains("+++ b/b.txt"), "{}", patches.working);

        sh(&dir, "git add .; git commit -qm base; seq 1 300000 > big.txt; echo three >> a.txt; git add big.txt; git commit -qm big; seq 300001 600000 > big.txt; echo more >> b.txt");
        let working = refresh(&dir, None, false, &mut Known::default()).patches.unwrap().working;
        assert!(working.len() < 10_000 && working.contains("Binary files a/big.txt and b/big.txt differ"), "{} bytes", working.len());
        assert!(working.contains("+more"), "the file next to it is shown");
        let files = gpui_kit::component::diff::DiffFile::parse(&working).unwrap();
        assert_eq!(files.iter().map(|f| f.path().to_string()).collect::<Vec<_>>(), ["a.txt", "b.txt", "big.txt"]);
        assert!(hunks(&working).iter().all(|hunk| hunk.path != "big.txt"), "no single change of it to take back");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// With whitespace left out the diff has the real change only, and that
    /// change is staged and reverted alone: the reindent stays as it is.
    #[test]
    fn a_change_shown_without_whitespace_is_staged_and_reverted_alone() {
        let dir = repo("whitespace");
        sh(&dir, "printf 'fn f() {\\na();\\nb();\\n}\\n' > f.rs; git add f.rs; git commit -qm base");
        sh(&dir, "printf 'fn f() {\\n    a();\\n\\n    c();\\n}\\n' > f.rs");
        IGNORE_WHITESPACE.set(true);
        let patch = diff_against(&dir, "HEAD", None);
        assert!(patch.contains("+    c();") && patch.contains("-b();"), "{patch}");
        assert!(!patch.contains("+    a();") && !patch.contains("-a();"), "{patch}");
        let change = hunks(&patch).into_iter().next().expect("one change");
        stage_change(&dir, &change, true).expect("staged");
        let staged = out(&dir, "git show :f.rs");
        // The blank line is shown with the change it is in, and goes with it.
        assert_eq!(staged, "fn f() {\na();\n\n    c();\n}\n", "the reindent of a() is not staged");
        stage_change(&dir, &change, false).expect("taken out of the index");
        assert_eq!(out(&dir, "git show :f.rs"), "fn f() {\na();\nb();\n}\n");
        apply_hunk(&dir, &change.patch(), true, false).expect("reverted");
        let now = std::fs::read_to_string(dir.join("f.rs")).unwrap();
        assert!(now.contains("    a();") && now.contains("b();") && !now.contains("c();"), "{now}");
        IGNORE_WHITESPACE.set(false);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A branch an agent works on in its own worktree is what it did there,
    /// new files too and not what this one did since; its index is left alone.
    #[test]
    fn a_branch_in_another_worktree_is_its_work_since_it_left() {
        let dir = repo("worked-on");
        sh(&dir, "echo one > f.txt; git add f.txt; git commit -qm base; git worktree add -q -b agent \"../$(basename \"$PWD\")-agent\"");
        let there = dir.with_file_name(format!("{}-agent", dir.file_name().unwrap().to_string_lossy()));
        sh(&there, "echo two >> f.txt; echo new > n.txt; echo ignored > i.log; echo '*.log' > .gitignore");
        let index = there.join(out(&there, "git rev-parse --git-path index").trim());
        let before = std::fs::read(&index).unwrap();
        sh(&dir, "echo main >> f.txt; git commit -qam later");
        let (diff, _) = work_of(&dir, "agent", None).unwrap();
        assert!(diff.contains("+two") && diff.contains("+new") && !diff.contains("main"), "{diff}");
        assert!(!diff.contains("ignored"), "{diff}");
        assert_eq!(std::fs::read(&index).unwrap(), before);
        assert!(work_of(&dir, "main", None).is_none());
        let _ = std::fs::remove_dir_all(&there);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The commit a pull is tried against leaves the repository's index
    /// alone, and is made while something else holds its lock.
    #[test]
    fn the_worktree_is_committed_without_touching_the_index() {
        let dir = repo("index");
        sh(&dir, "echo one > f.txt; git add f.txt; git commit -qm base");
        assert_eq!(worktree_commit(&dir).as_deref(), Some("HEAD"), "clean");
        sh(&dir, "echo two >> f.txt; echo new > staged.txt; git add staged.txt; echo untracked > u.txt");
        let index = dir.join(".git/index");
        let before = (stamp(&index), std::fs::read(&index).unwrap());
        std::fs::write(dir.join(".git/index.lock"), "").unwrap();
        let made = worktree_commit(&dir).expect("made with the index locked");
        std::fs::remove_file(dir.join(".git/index.lock")).unwrap();
        assert!(before == (stamp(&index), std::fs::read(&index).unwrap()), "the index is as it was");
        assert_eq!(out(&dir, &format!("git ls-tree -r --name-only {made}")).lines().collect::<Vec<_>>(), ["f.txt", "staged.txt"], "what is tracked, as on disk");
        assert_eq!(out(&dir, &format!("git show {made}:f.txt")), "one\ntwo\n");
        assert!(out(&dir, "git stash list").is_empty() && out(&dir, "git status --short").contains("A  staged.txt"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod request_tests {
    #[cfg(unix)]
    #[test]
    fn a_request_s_task_has_its_head_as_it_is_now() {
        let sh = |dir: &std::path::Path, script: &str| assert!(std::process::Command::new("bash").arg("-ec").arg(script).current_dir(dir).status().unwrap().success(), "{script}");
        let base = std::env::temp_dir().join(format!("ideae-request-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let g = "git -c user.name=t -c user.email=t@t -c init.defaultBranch=main";
        sh(&base, &format!("{g} init -q origin && cd origin && echo a > f && git add f && {g} commit -qm a && git checkout -qb fix && echo b >> f && {g} commit -qam b && git update-ref refs/pull/5/head HEAD && git checkout -q main"));
        sh(&base, &format!("git clone -q origin here && cd here && git branch fix main && git checkout -qb other main && echo c > g && git add g && {g} commit -qm c && git update-ref refs/pull/6/head refs/remotes/origin/fix 2>/dev/null; true"));
        let (repo, worktrees) = (base.join("here"), base.join("here.worktrees"));
        let tip = |r: &str| super::git(&base.join("origin"), &["rev-parse", r]).unwrap().trim().to_string();
        let head_of = |dir: &std::path::Path| super::git(dir, &["rev-parse", "HEAD"]).unwrap().trim().to_string();
        // An older `fix` here is moved on to the request's head.
        let (dir, branch) = super::request_worktree(&repo, crate::integrations::Kind::GitHub, 5, "fix", false, false).unwrap();
        assert_eq!((branch.as_str(), head_of(&dir)), ("fix", tip("fix")));
        // A branch of that name with commits of its own is left alone: request-5.
        sh(&repo, &format!("{g} worktree remove --force {} && git branch -f fix other", dir.display()));
        let (dir, branch) = super::request_worktree(&repo, crate::integrations::Kind::GitHub, 5, "fix", false, false).unwrap();
        assert_eq!((branch.as_str(), head_of(&dir)), ("request-5", tip("fix")));
        assert_eq!(super::git(&repo, &["rev-parse", "other"]), super::git(&repo, &["rev-parse", "fix"]), "fix as it was");
        let _ = std::fs::remove_dir_all(&base);
        let _ = std::fs::remove_dir_all(&worktrees);
    }

    #[test]
    fn a_request_says_what_it_does_of_its_tickets() {
        let text = super::RequestText {
            title_template: crate::layers::DEFAULT_PR_TITLE.into(),
            key: "DEV-1".into(),
            tickets: vec![("DEV-1".into(), "https://example.atlassian.net/browse/DEV-1".into()), ("o/r#4".into(), String::new())],
            closes: "fixes".into(),
        };
        assert_eq!(text.title("Fix the login"), "DEV-1 Fix the login");
        assert_eq!(text.body("Body.\n"), "Body.\n\nFixes DEV-1 (https://example.atlassian.net/browse/DEV-1)\nFixes o/r#4");
        assert_eq!(super::RequestText { closes: "none".into(), ..text.clone() }.body("Body."), "Body.");
        assert_eq!(super::RequestText { closes: "refs".into(), ..text }.body(""), "Refs DEV-1 (https://example.atlassian.net/browse/DEV-1)\nRefs o/r#4");
    }
}

#[cfg(test)]
mod push_tests {
    use super::{RebaseError, head_pushed, keep_reason, pull_and_push, push, push_or_tell, push_overwrite, push_refused, run};
    use std::path::{Path, PathBuf};

    #[test]
    fn pushed_is_on_the_upstream_or_the_base() {
        assert!(head_pushed(true, 0, true, 3), "in the upstream");
        assert!(!head_pushed(true, 1, true, 0), "a commit not pushed");
        assert!(!head_pushed(false, 0, true, 2), "never published");
        assert!(head_pushed(false, 0, true, 0), "a new branch at its base: that commit is the base's");
        assert!(!head_pushed(false, 0, false, 0), "no remote at all");
    }

    #[test]
    fn refusals_told_from_other_failures() {
        let behind = "To ../remote.git\n ! [rejected]        feature -> feature (non-fast-forward)\nerror: failed to push some refs to '../remote.git'\nhint: Updates were rejected because the tip of your current branch is behind";
        let fetch_first = "To ../remote.git\n ! [rejected]        main -> main (fetch first)\nerror: failed to push some refs";
        let stale = " ! [rejected]        feature -> feature (stale info)\nerror: failed to push some refs";
        assert!(push_refused(behind) && push_refused(fetch_first) && push_refused(stale));
        let hook = " ! [remote rejected] feature -> feature (pre-receive hook declined)\nerror: failed to push some refs";
        let auth = "fatal: Authentication failed for 'https://example.com/repo.git/'";
        let protected = " ! [remote rejected] main -> main (protected branch hook declined)";
        assert!(!push_refused(hook) && !push_refused(auth) && !push_refused(protected));
    }

    #[test]
    fn what_is_not_overwritten() {
        let mine = [("Me".to_string(), "me@example.com".to_string())];
        let theirs = [("Me".to_string(), "ME@example.com".to_string()), ("Other".to_string(), "other@example.com".to_string())];
        assert!(keep_reason("main", Some("develop"), &mine, Some("me@example.com")).is_some());
        assert!(keep_reason("develop", Some("develop"), &mine, Some("me@example.com")).is_some());
        assert_eq!(keep_reason("feature", Some("main"), &mine, Some("me@example.com")), None);
        assert_eq!(keep_reason("feature", Some("main"), &theirs, Some("me@example.com")).as_deref(), Some("the remote has commits by Other"));
    }

    fn git(dir: &Path, args: &[&str]) -> String {
        run(dir, "git", args).unwrap_or_else(|e| panic!("git {args:?}: {e}"))
    }

    fn commit(dir: &Path, file: &str, text: &str, who: &str) {
        std::fs::write(dir.join(file), text).unwrap();
        git(dir, &["add", file]);
        git(dir, &["-c", &format!("user.name={who}"), "-c", &format!("user.email={who}@example.com"), "commit", "-qm", &format!("{file} by {who}")]);
    }

    /// A remote, a clone of it here and one elsewhere, both on `feature`.
    fn two_clones(name: &str) -> (PathBuf, PathBuf, PathBuf) {
        let top = std::env::temp_dir().join(format!("ideae-push-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&top);
        std::fs::create_dir_all(&top).unwrap();
        let (here, there) = (top.join("here"), top.join("there"));
        git(&top, &["init", "-q", "--bare", "-b", "main", "remote.git"]);
        // As the clone is made: set after it, Windows' CRLF checkout looks changed.
        git(&top, &["clone", "-q", "-c", "core.autocrlf=false", "remote.git", "here"]);
        git(&here, &["config", "user.email", "me@example.com"]);
        git(&here, &["config", "user.name", "me"]);
        git(&here, &["checkout", "-qb", "main"]);
        commit(&here, "a.txt", "one\n", "me");
        git(&here, &["push", "-q", "-u", "origin", "main"]);
        git(&here, &["checkout", "-qb", "feature"]);
        commit(&here, "b.txt", "mine\n", "me");
        git(&here, &["push", "-q", "-u", "origin", "feature"]);
        git(&top, &["clone", "-q", "-c", "core.autocrlf=false", "-b", "feature", "remote.git", "there"]);
        (top, here, there)
    }

    #[test]
    fn a_refused_push_is_pulled_and_pushed() {
        let (top, here, there) = two_clones("pull");
        commit(&there, "c.txt", "theirs\n", "other");
        git(&there, &["push", "-q"]);
        commit(&here, "d.txt", "more\n", "me");
        let Err(Ok(told)) = push_or_tell(&here, true) else { panic!("not told as diverged") };
        assert_eq!(told.target.tracking(), "origin/feature");
        assert_eq!(told.theirs.len(), 1);
        assert_eq!(told.keep.as_deref(), Some("the remote has commits by other"));
        assert!(push_overwrite(&here, &told).is_err(), "others' commits are not overwritten");
        pull_and_push(&here, true).unwrap();
        assert_eq!(git(&here, &["rev-parse", "HEAD"]), git(&here, &["rev-parse", "origin/feature"]));
        assert!(here.join("c.txt").exists());
        std::fs::remove_dir_all(&top).unwrap();
    }

    #[test]
    fn a_conflict_leaves_the_branch_as_it_was() {
        let (top, here, there) = two_clones("conflict");
        commit(&there, "b.txt", "theirs\n", "me");
        git(&there, &["push", "-q"]);
        commit(&here, "b.txt", "mine again\n", "me");
        let before = git(&here, &["rev-parse", "HEAD"]);
        assert!(push(&here, true).is_err());
        match pull_and_push(&here, true) {
            Err(RebaseError::Conflicts(files)) => assert_eq!(files, ["b.txt"]),
            other => panic!("{other:?}"),
        }
        assert_eq!(git(&here, &["rev-parse", "HEAD"]), before);
        assert!(!here.join(".git/rebase-merge").exists() && !here.join(".git/rebase-apply").exists());
        std::fs::remove_dir_all(&top).unwrap();
    }

    #[test]
    fn a_rebased_branch_of_ones_own_is_overwritten_with_a_lease() {
        let (top, here, there) = two_clones("overwrite");
        // Pushed from elsewhere by the same person, then this branch rewritten.
        commit(&there, "c.txt", "old try\n", "me");
        git(&there, &["push", "-q"]);
        git(&here, &["commit", "-q", "--amend", "-m", "b.txt, reworded"]);
        let Err(Ok(told)) = push_or_tell(&here, true) else { panic!("not told as diverged") };
        assert_eq!(told.keep, None);
        // The remote moves after it was shown: the lease keeps it.
        commit(&there, "e.txt", "later\n", "me");
        git(&there, &["push", "-q"]);
        assert!(push_overwrite(&here, &told).is_err(), "moved since it was shown");
        let Err(Ok(told)) = push_or_tell(&here, true) else { panic!("not told as diverged") };
        push_overwrite(&here, &told).unwrap();
        assert_eq!(git(&here, &["rev-parse", "HEAD"]), git(&top.join("remote.git"), &["rev-parse", "feature"]));
        std::fs::remove_dir_all(&top).unwrap();
    }

    #[test]
    fn the_main_branch_is_not_overwritten() {
        let (top, here, there) = two_clones("main");
        git(&there, &["checkout", "-q", "main"]);
        commit(&there, "c.txt", "theirs\n", "me");
        git(&there, &["push", "-q"]);
        git(&here, &["checkout", "-q", "main"]);
        commit(&here, "d.txt", "mine\n", "me");
        let Err(Ok(told)) = push_or_tell(&here, true) else { panic!("not told as diverged") };
        assert!(told.keep.as_deref().is_some_and(|why| why.contains("main branch")));
        std::fs::remove_dir_all(&top).unwrap();
    }
}

#[cfg(all(test, unix))]
mod own_branch_tests {
    use super::*;

    fn sh(dir: &Path, script: &str) {
        let ok = std::process::Command::new("bash").arg("-ec").arg(script).current_dir(dir).status().unwrap().success();
        assert!(ok, "{script}");
    }

    #[test]
    fn a_task_keeps_its_branch_wherever_an_agent_puts_its_worktree() {
        let base = std::env::temp_dir().join(format!("ideae-own-branch-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        sh(&base, "git init -q --bare -b main origin.git; git init -q -b main repo; cd repo; git config user.email t@t; git config user.name t
            git remote add origin ../origin.git; echo one > f; git add f; git commit -qm base; git push -q origin main
            GIT_COMMITTER_DATE='2001-01-01T00:00:00' git branch develop
            git worktree add -q -b task ../wt; cd ../wt; echo two >> f; git commit -qam mine");
        let (repo, wt) = (base.join("repo"), base.join("wt"));
        let since = crate::state::now() - 60;

        // Its work merged into main, the agent leaves its worktree on main, detached.
        sh(&wt, "git push -q origin task:main; git fetch -q origin; git checkout -q --detach origin/main");
        let head = head_of(&wt, "(detached)");
        assert_eq!(head.off("task").as_deref(), Some("detached at origin/main"));
        // The branch is not deleted with the worktree, since it holds nothing that is not elsewhere.
        assert_eq!(remove_task_worktree(&repo, &wt, "task", false), Ok(None));
        assert!(!branch_exists(&repo, "task"));

        // On another branch: that one is not the task's, and stays.
        sh(&repo, "git worktree add -q -b task ../wt origin/main; cd ../wt; echo three >> f; git commit -qam unpushed; git switch -q develop");
        assert_eq!(head_of(&wt, "develop").off("task").as_deref(), Some("on develop"));
        assert!(!grew_from(&wt, "task", "develop", since), "made before the task, and not from its branch");
        let stays = remove_task_worktree(&repo, &wt, "task", false).unwrap();
        assert!(stays.is_some_and(|s| s.contains("1 commit")), "its commit is nowhere else");
        assert!(branch_exists(&repo, "task") && branch_exists(&repo, "develop"));

        // A branch the agent made from the task's: the same work under another name.
        sh(&repo, "git worktree add -q ../wt task; cd ../wt; git switch -q -c task-2; echo four >> f; git commit -qam more");
        assert!(grew_from(&wt, "task", "task-2", since));
        assert!(!grew_from(&wt, "task", "task-2", 0), "a worktree made elsewhere owns nothing");

        // Back on its branch: moved to where the worktree is when it has all of it.
        sh(&wt, "git checkout -q --detach HEAD");
        assert_eq!(head_of(&wt, "(detached)").off("task").as_deref(), Some("detached at task-2"));
        assert!(back_on_branch(&wt, "task").unwrap().contains("moved here"));
        assert_eq!(git(&wt, &["rev-parse", "task"]), git(&wt, &["rev-parse", "task-2"]));
        // Else the worktree goes to it, and what is on no branch gets one.
        sh(&wt, "git checkout -q --detach origin/main; echo five > g; git add g; git commit -qm loose");
        let back = back_on_branch(&wt, "task").unwrap();
        assert!(back.contains("task-here"), "{back}");
        assert_eq!(git(&wt, &["symbolic-ref", "--short", "HEAD"]).unwrap().trim(), "task");

        // Mid-rebase: told as such, the branch being rebased read from git.
        let dir = git_dir_of(&wt).unwrap();
        std::fs::create_dir_all(dir.join("rebase-merge")).unwrap();
        std::fs::write(dir.join("rebase-merge/head-name"), "refs/heads/task\n").unwrap();
        let head = head_of(&wt, "(detached)");
        assert_eq!((head.branch.as_str(), head.off("task").as_deref()), ("task", Some("rebasing")));
        assert_eq!(head.off("other").as_deref(), Some("rebasing task"));
        let _ = std::fs::remove_dir_all(&base);
    }
}
