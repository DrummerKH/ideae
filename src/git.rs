use std::{
    path::{Path, PathBuf},
    process::Command,
};

pub struct Worktree {
    pub path: PathBuf,
    pub branch: String,
}

fn git(root: &Path, args: &[&str]) -> Option<String> {
    // No optional locks: status and diff must not rewrite the index, or the
    // file watcher would see our own refresh and refresh again forever.
    let output = Command::new("git")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .ok()?;
    // `diff --no-index` exits 1 when the files differ.
    if !output.status.success() && output.status.code() != Some(1) {
        return None;
    }
    String::from_utf8(output.stdout).ok()
}

/// Every worktree of the repository containing `path`, main one first.
pub fn worktrees(path: &Path) -> Vec<Worktree> {
    let Some(out) = git(path, &["worktree", "list", "--porcelain"]) else {
        return vec![Worktree { path: path.to_path_buf(), branch: "(no git)".into() }];
    };
    let mut result = Vec::new();
    for block in out.split("\n\n") {
        let mut wt_path = None;
        let mut branch = String::from("(detached)");
        for line in block.lines() {
            if let Some(p) = line.strip_prefix("worktree ") {
                wt_path = Some(PathBuf::from(p));
            } else if let Some(b) = line.strip_prefix("branch ") {
                branch = b.trim_start_matches("refs/heads/").to_string();
            } else if line == "bare" {
                wt_path = None;
            }
        }
        if let Some(path) = wt_path {
            result.push(Worktree { path, branch });
        }
    }
    result
}

const DIFF_ARGS: &[&str] = &[
    "--no-color",
    "--no-ext-diff",
    "--no-textconv",
    "--find-renames",
    "--src-prefix=a/",
    "--dst-prefix=b/",
];

/// Uncommitted changes against HEAD, untracked files included, as one patch.
pub fn working_patch(root: &Path) -> String {
    patch_from(root, "HEAD")
}

/// The worktree (untracked files included) against `rev`.
fn patch_from(root: &Path, rev: &str) -> String {
    let mut args = vec!["diff", rev];
    args.extend_from_slice(DIFF_ARGS);
    let mut patch = git(root, &args).unwrap_or_default();

    let untracked = git(root, &["ls-files", "--others", "--exclude-standard", "-z"]).unwrap_or_default();
    for file in untracked.split('\0').filter(|f| !f.is_empty()).take(200) {
        let mut args = vec!["diff", "--no-index"];
        args.extend_from_slice(DIFF_ARGS);
        args.extend_from_slice(&["--", "/dev/null", file]);
        if let Some(p) = git(root, &args) {
            patch.push_str(&p);
        }
    }
    patch
}

/// First line of each file's first hunk on the new side, keyed by path.
pub fn first_changed_lines(patch: &str) -> std::collections::HashMap<String, u32> {
    let mut lines = std::collections::HashMap::new();
    let mut current: Option<String> = None;
    for line in patch.lines() {
        if let Some(path) = line.strip_prefix("+++ ") {
            current = path.strip_prefix("b/").map(str::to_string);
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

/// Tracked and untracked-but-not-ignored files, relative to `root`.
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
    pub ahead: u32,
    pub behind: u32,
    pub last_hash: String,
    pub last_subject: String,
}

pub fn branch_info(root: &Path) -> BranchInfo {
    let mut info = BranchInfo::default();
    for line in git(root, &["status", "--branch", "--porcelain=v2"]).unwrap_or_default().lines() {
        if let Some(head) = line.strip_prefix("# branch.head ") {
            info.head = head.to_string();
        } else if let Some(upstream) = line.strip_prefix("# branch.upstream ") {
            info.upstream = Some(upstream.to_string());
        } else if let Some(ab) = line.strip_prefix("# branch.ab ") {
            for part in ab.split_whitespace() {
                if let Some(n) = part.strip_prefix('+') {
                    info.ahead = n.parse().unwrap_or(0);
                } else if let Some(n) = part.strip_prefix('-') {
                    info.behind = n.parse().unwrap_or(0);
                }
            }
        }
    }
    if let Some(last) = git(root, &["log", "-1", "--format=%h%x1f%s"]) {
        if let Some((hash, subject)) = last.trim_end().split_once('\x1f') {
            info.last_hash = hash.to_string();
            info.last_subject = subject.to_string();
        }
    }
    info
}

#[derive(Clone, Debug)]
pub struct Commit {
    pub hash: String,
    pub short: String,
    pub author: String,
    pub date: String,
    pub subject: String,
}

/// The newest `limit` commits reachable from HEAD.
pub fn history(root: &Path, limit: usize) -> Vec<Commit> {
    let limit = format!("-n{limit}");
    let out = git(root, &["log", &limit, "--format=%H%x1f%h%x1f%an%x1f%ar%x1f%s%x1e"]).unwrap_or_default();
    out.split('\x1e')
        .filter_map(|record| {
            let mut fields = record.trim_start_matches('\n').split('\x1f');
            Some(Commit {
                hash: fields.next().filter(|h| !h.is_empty())?.to_string(),
                short: fields.next()?.to_string(),
                author: fields.next()?.to_string(),
                date: fields.next()?.to_string(),
                subject: fields.next().unwrap_or_default().to_string(),
            })
        })
        .collect()
}

/// The patch a commit introduced, against its first parent.
pub fn commit_patch(root: &Path, hash: &str) -> String {
    let mut args = vec!["show", "--format=", "--patch", "--first-parent", "-m"];
    args.extend_from_slice(DIFF_ARGS);
    args.extend_from_slice(&[hash, "--"]);
    git(root, &args).unwrap_or_default()
}

/// Index (staged) and worktree (unstaged) state per path.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct FileState {
    pub staged: bool,
    pub unstaged: bool,
}

pub fn file_states(root: &Path) -> std::collections::HashMap<String, FileState> {
    let mut states = std::collections::HashMap::new();
    let out = git(root, &["status", "--porcelain=v1", "-z", "--untracked-files=all"]).unwrap_or_default();
    let mut entries = out.split('\0');
    while let Some(entry) = entries.next() {
        if entry.len() < 4 {
            continue;
        }
        let (x, y) = (entry.as_bytes()[0], entry.as_bytes()[1]);
        let path = entry[3..].to_string();
        if x == b'R' || x == b'C' {
            entries.next(); // the original path of a rename or copy
        }
        let untracked = x == b'?';
        states.insert(
            path,
            FileState { staged: !untracked && x != b' ', unstaged: untracked || y != b' ' },
        );
    }
    states
}

/// Runs a command in `root` for a user action: stdout on success, stderr (or
/// stdout) as the error.
pub fn run(root: &Path, program: &str, args: &[&str]) -> Result<String, String> {
    let output = Command::new(program)
        .current_dir(root)
        .args(args)
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| format!("{program}: {e}"))?;
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if output.status.success() {
        Ok(if stdout.is_empty() { stderr } else { stdout })
    } else {
        Err(if stderr.is_empty() { stdout } else { stderr })
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

/// Commits what is staged; with nothing staged, everything (`git add -A` first).
pub fn commit(root: &Path, message: &str, all: bool) -> Result<String, String> {
    if all {
        run(root, "git", &["add", "-A"])?;
    }
    run(root, "git", &["commit", "-m", message])
}

/// Pushes, setting the upstream on the first push of a branch.
pub fn push(root: &Path, has_upstream: bool) -> Result<String, String> {
    if has_upstream {
        run(root, "git", &["push"])
    } else {
        run(root, "git", &["push", "-u", "origin", "HEAD"])
    }
}

/// The branch's pull request URL, creating the PR (from the commits) if needed.
pub fn pull_request(root: &Path, has_upstream: bool) -> Result<String, String> {
    if let Ok(url) = run(root, "gh", &["pr", "view", "--json", "url", "-q", ".url"]) {
        return Ok(url);
    }
    push(root, has_upstream)?;
    let out = run(root, "gh", &["pr", "create", "--fill"])?;
    Ok(out.lines().rev().find(|l| l.starts_with("http")).unwrap_or(&out).to_string())
}

/// The directories to watch so that any change to the worktree or its git
/// state is seen: every directory holding a listed file, the worktree root,
/// the git dir (HEAD, index) and the shared refs.
pub fn watch_dirs(root: &Path, files: &[String]) -> (std::collections::BTreeSet<std::path::PathBuf>, Vec<std::path::PathBuf>) {
    let mut flat = std::collections::BTreeSet::new();
    flat.insert(root.to_path_buf());
    for file in files {
        if let Some((dir, _)) = file.rsplit_once('/') {
            flat.insert(root.join(dir));
        }
    }
    let mut recursive = Vec::new();
    if let Some(git_dir) = git(root, &["rev-parse", "--absolute-git-dir"]) {
        flat.insert(std::path::PathBuf::from(git_dir.trim()));
    }
    if let Some(common) = git(root, &["rev-parse", "--path-format=absolute", "--git-common-dir"]) {
        let refs = std::path::PathBuf::from(common.trim()).join("refs");
        if refs.is_dir() {
            recursive.push(refs);
        }
    }
    (flat, recursive)
}

/// The branch this task forks from: the remote's default branch, else the
/// usual names. Exit code 1 counts as success in `git`, hence the emptiness checks.
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
pub fn branch_patch(root: &Path, base: &str) -> Option<String> {
    let fork = git(root, &["merge-base", "HEAD", base]).map(|s| s.trim().to_string()).filter(|s| !s.is_empty())?;
    Some(patch_from(root, &fork))
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
pub fn blame(root: &Path, file: &Path) -> Vec<Option<BlameLine>> {
    let Some(out) = git(root, &["blame", "--porcelain", "--", &file.to_string_lossy()]) else {
        return Vec::new();
    };
    let mut commits: std::collections::HashMap<String, BlameLine> = std::collections::HashMap::new();
    let mut lines: Vec<Option<BlameLine>> = Vec::new();
    let mut current: Option<(String, usize)> = None;
    for line in out.lines() {
        if line.starts_with('\t') {
            if let Some((hash, final_line)) = current.take() {
                if lines.len() < final_line {
                    lines.resize(final_line, None);
                }
                lines[final_line - 1] = commits.get(&hash).cloned();
            }
            continue;
        }
        let mut parts = line.split(' ');
        let first = parts.next().unwrap_or_default();
        if first.len() == 40 && first.bytes().all(|b| b.is_ascii_hexdigit()) {
            let final_line = parts.nth(1).and_then(|n| n.parse().ok()).unwrap_or(0);
            commits.entry(first.to_string()).or_insert_with(|| BlameLine {
                hash: first.to_string(),
                author: String::new(),
                time: 0,
                summary: String::new(),
            });
            current = Some((first.to_string(), final_line));
            continue;
        }
        let Some((hash, _)) = &current else { continue };
        let Some(entry) = commits.get_mut(hash) else { continue };
        let rest = line.split_once(' ').map(|(_, r)| r).unwrap_or_default();
        match first {
            "author" => entry.author = rest.to_string(),
            "author-time" => entry.time = rest.parse().unwrap_or(0),
            "summary" => entry.summary = rest.to_string(),
            _ => {}
        }
    }
    lines
}

/// One commit's list entry, for a commit outside the loaded history.
pub fn commit_info(root: &Path, hash: &str) -> Option<Commit> {
    let out = git(root, &["log", "-1", "--format=%H%x1f%h%x1f%an%x1f%ar%x1f%s", hash])?;
    let mut fields = out.trim_end().split('\x1f');
    Some(Commit {
        hash: fields.next().filter(|h| !h.is_empty())?.to_string(),
        short: fields.next()?.to_string(),
        author: fields.next()?.to_string(),
        date: fields.next()?.to_string(),
        subject: fields.next().unwrap_or_default().to_string(),
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

pub fn fetch(root: &Path) -> Result<String, String> {
    run(root, "git", &["fetch", "--prune"]).map(|out| if out.is_empty() { "Fetched".into() } else { out })
}

/// Brings in the upstream without ever leaving a mess: a fast-forward when
/// there is nothing local, else a rebase that is aborted on any conflict.
/// Uncommitted work is stashed around it either way.
pub fn pull_safe(root: &Path) -> Result<String, String> {
    run(root, "git", &["fetch", "--prune"])?;
    let ahead = run(root, "git", &["rev-list", "--count", "@{u}..HEAD"])?.trim().parse::<u32>().unwrap_or(0);
    let behind = run(root, "git", &["rev-list", "--count", "HEAD..@{u}"])?.trim().parse::<u32>().unwrap_or(0);
    if behind == 0 {
        return Ok("Already up to date".into());
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
mod pull_tests {
    use super::pull_safe;
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
    fn fast_forward_keeps_local_edits() {
        let base = setup("ff");
        sh(&base, "cd a; printf 'one\\ntwo\\nthree\\nfour\\n' > f.txt; git commit -qam up; git push -q origin main
            cd ../b; echo note > untracked.txt; printf 'ONE\\ntwo\\nthree\\n' > f.txt");
        let out = pull_safe(&base.join("b")).unwrap();
        assert!(out.starts_with("Fast-forwarded"), "{out}");
        let text = std::fs::read_to_string(base.join("b/f.txt")).unwrap();
        assert_eq!(text, "ONE\ntwo\nthree\nfour\n");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn conflict_is_undone() {
        let base = setup("conflict");
        sh(&base, "cd a; printf 'one\\nTWO-remote\\nthree\\n' > f.txt; git commit -qam remote; git push -q origin main
            cd ../b; printf 'one\\nTWO-local\\nthree\\n' > f.txt; git commit -qam local");
        let head_before = Command::new("git").args(["rev-parse", "HEAD"]).current_dir(base.join("b")).output().unwrap().stdout;
        let err = pull_safe(&base.join("b")).unwrap_err();
        assert!(err.starts_with("conflicts"), "{err}");
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
}

/// Local and remote branches, for picking what to diff against.
pub fn branches(root: &Path) -> Vec<String> {
    git(root, &["for-each-ref", "--sort=-committerdate", "--format=%(refname:short)", "refs/heads", "refs/remotes"])
        .unwrap_or_default()
        .lines()
        .map(str::trim)
        .filter(|b| !b.is_empty() && !b.ends_with("/HEAD") && *b != "origin")
        .map(str::to_string)
        .collect()
}

fn git_status(root: &Path, args: &[&str]) -> Result<String, String> {
    let output = Command::new("git")
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

/// Where a new task branches from: the remote's default branch, fetched
/// first, else the main worktree's branch.
fn new_task_base(repo: &Path) -> String {
    if let Some(remote_head) = git(repo, &["symbolic-ref", "--short", "refs/remotes/origin/HEAD"]) {
        let remote_head = remote_head.trim().to_string();
        if let Some(branch) = remote_head.strip_prefix("origin/") {
            let _ = git_status(repo, &["fetch", "--quiet", "origin", branch]);
            return remote_head;
        }
    }
    git(repo, &["rev-parse", "--abbrev-ref", "HEAD"]).map(|s| s.trim().to_string()).unwrap_or_else(|| "HEAD".into())
}

pub fn task_worktree_dir(repo: &Path, branch: &str) -> PathBuf {
    let name = repo.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "repo".into());
    repo.with_file_name(format!("{name}.worktrees")).join(branch.replace('/', "-"))
}

/// Creates the worktree of a new task next to the repository, in
/// `<repo>.worktrees/<branch>`, and copies the main worktree's untracked
/// `.env*` files into it.
pub fn create_task_worktree(repo: &Path, branch: &str) -> Result<PathBuf, String> {
    let dir = task_worktree_dir(repo, branch);
    if dir.exists() {
        return Err(format!("{} already exists", dir.display()));
    }
    let base = new_task_base(repo);
    let dir_s = dir.to_string_lossy().into_owned();
    git_status(repo, &["worktree", "add", "--no-track", "-b", branch, &dir_s, &base])?;
    copy_env_files(repo, &dir);
    Ok(dir)
}

fn copy_env_files(from: &Path, to: &Path) {
    let Some(out) = git(from, &["ls-files", "--others", "--ignored", "--exclude-standard", "-z", "--", ":(glob)**/.env*"])
    else {
        return;
    };
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

/// What deleting a task's worktree would lose: uncommitted files, commits on
/// no remote.
pub fn unsaved_work(root: &Path) -> (usize, usize) {
    let dirty = git(root, &["status", "--porcelain"]).map_or(0, |s| s.lines().count());
    let unpushed = git(root, &["rev-list", "--count", "HEAD", "--not", "--remotes"])
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0);
    (dirty, unpushed)
}

/// Removes a task's worktree and its branch.
pub fn remove_task_worktree(repo: &Path, path: &Path, branch: &str) -> Result<(), String> {
    let path_s = path.to_string_lossy().into_owned();
    if path.exists() {
        git_status(repo, &["worktree", "remove", "--force", &path_s])?;
    } else {
        let _ = git_status(repo, &["worktree", "prune"]);
    }
    if !branch.is_empty() && !branch.starts_with('(') {
        let _ = git_status(repo, &["branch", "-D", branch]);
    }
    Ok(())
}
