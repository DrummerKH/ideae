use std::{
    path::{Path, PathBuf},
    process::Command,
};

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
    // No optional locks: status and diff must not rewrite the index, or the
    // file watcher would see our own refresh and refresh again forever.
    let output = crate::procs::command("git")
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
        return vec![Worktree { path: path.to_path_buf(), branch: "(no git)".into(), locked: None }];
    };
    let mut result = Vec::new();
    for block in out.split("\n\n") {
        let mut wt_path = None;
        let mut branch = String::from("(detached)");
        let mut locked = None;
        for line in block.lines() {
            if let Some(p) = line.strip_prefix("worktree ") {
                wt_path = Some(PathBuf::from(p));
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
    let untracked = git(root, &["ls-files", "--others", "--exclude-standard", "-z"]).unwrap_or_default();
    let untracked: Vec<&str> = untracked.split('\0').filter(|f| !f.is_empty()).collect();
    tracked_patch(root, "HEAD") + &new_files_patch(root, &untracked)
}

/// The tracked files of the worktree against `rev`.
fn tracked_patch(root: &Path, rev: &str) -> String {
    let mut args = vec!["diff", rev];
    args.extend_from_slice(DIFF_ARGS);
    git(root, &args).unwrap_or_default()
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
    let Ok(meta) = std::fs::symlink_metadata(&path) else { return };
    #[cfg(unix)]
    let executable = std::os::unix::fs::PermissionsExt::mode(&meta.permissions()) & 0o111 != 0;
    #[cfg(not(unix))]
    let executable = false;
    let (mode, text) = if meta.file_type().is_symlink() {
        ("120000", std::fs::read_link(&path).ok().map(|to| to.to_string_lossy().into_owned()))
    } else if !meta.is_file() {
        return;
    } else {
        // Too large, or not text (as git tells: a NUL early on): named, not shown.
        let text = (in_full && meta.len() <= NEW_FILE_BYTES)
            .then(|| std::fs::read(&path).ok())
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
    /// The file's header and this change as `git apply -R` takes it from
    /// the file as it is now: the other changes of its hunk are there.
    pub patch: String,
    /// The same for `git apply` to the file as it was: they are not.
    pub stage: String,
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
        if whole_file || path.is_empty() {
            return;
        }
        let changed = |line: &str| line.starts_with(['+', '-']);
        let mut at = 0;
        while at < lines.len() {
            if !changed(lines[at]) {
                at += 1;
                continue;
            }
            let end = (at..lines.len()).find(|&i| !changed(lines[i]) && !lines[i].starts_with('\\')).unwrap_or(lines.len());
            // With `as_it_is` the lines the other changes added are around
            // this one, and those they removed are not; without, the reverse.
            let alone = |as_it_is: bool| {
                let (mut body, mut olds, mut news, mut kept) = (String::new(), 0, 0, true);
                for (i, line) in lines.iter().enumerate() {
                    let (sign, rest) = line.split_at(1.min(line.len()));
                    let here = (at..end).contains(&i);
                    let stays = if as_it_is { "+" } else { "-" };
                    kept = match sign {
                        "\\" => kept,
                        "+" | "-" if !here => sign == stays,
                        _ => true,
                    };
                    if !kept {
                        continue;
                    }
                    let sign = if here || !changed(line) { sign } else { " " };
                    olds += u32::from(sign == " " || sign == "-");
                    news += u32::from(sign == " " || sign == "+");
                    body.push_str(sign);
                    body.push_str(rest);
                }
                format!("{header}@@ -{old},{olds} +{new},{news} @@\n{body}")
            };
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
                found.push(Hunk { path: path.to_string(), line, modified, patch: alone(true), stage: alone(false) });
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
            if line.starts_with("--- /dev/null") || line.starts_with("+++ /dev/null") || line.starts_with("Binary files") {
                whole_file = true;
            }
            if let Some(name) = line.strip_prefix("+++ b/") {
                path = name.trim_end().to_string();
            }
            header.push_str(line);
        }
    }
    close(&mut open, &header, &path, whole_file);
    found
}

/// Applies one hunk: to the worktree (`reverse` takes the change back),
/// or with `cached` to the index (stages it).
pub fn apply_hunk(root: &Path, patch: &str, reverse: bool, cached: bool) -> Result<String, String> {
    use std::io::Write as _;
    let mut command = crate::procs::command("git");
    command.current_dir(root).args(["apply", "--whitespace=nowarn"]);
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
}

fn status(root: &Path) -> Status {
    parse_status(&git(root, &["status", "--porcelain=v2", "--branch", "-z", "--untracked-files=all"]).unwrap_or_default())
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
            status.states.insert(path.to_string(), FileState { staged: false, unstaged: true });
            status.untracked.push(path.to_string());
        } else {
            let fields = match entry.as_bytes().first() {
                Some(b'1') => 8,
                Some(b'2') => {
                    entries.next(); // the original path of a rename or copy
                    9
                }
                Some(b'u') => 10,
                _ => continue,
            };
            if let Some((x, y, path)) = changed(fields) {
                status.states.insert(path, FileState { staged: x != b'.', unstaged: y != b'.' });
            }
        }
    }
    status
}

/// What a refresh need not ask git every time: where the repository keeps
/// itself, whether it has an origin, the branch work starts from. Asked
/// again now and then, since a remote can be added and a default changed.
#[derive(Default)]
pub struct Known {
    age: u32,
    git_dir: Option<PathBuf>,
    refs: Option<PathBuf>,
    has_origin: bool,
    base: Option<String>,
}

impl Known {
    fn fresh(&mut self, root: &Path) {
        if self.age % 30 == 0 {
            self.git_dir = git(root, &["rev-parse", "--absolute-git-dir"]).map(|dir| PathBuf::from(dir.trim()));
            self.refs = git(root, &["rev-parse", "--path-format=absolute", "--git-common-dir"])
                .map(|common| PathBuf::from(common.trim()).join("refs"))
                .filter(|refs| refs.is_dir());
            self.has_origin = git(root, &["remote", "get-url", "origin"]).is_some();
            self.base = base_ref(root);
        }
        self.age += 1;
    }
}

/// All a task shows of its worktree, read in one go.
pub struct Refreshed {
    pub files: Vec<String>,
    pub watch: (std::collections::BTreeSet<PathBuf>, Vec<PathBuf>),
    pub info: BranchInfo,
    pub states: std::collections::HashMap<String, FileState>,
    /// The uncommitted changes, untracked files included.
    pub working: String,
    /// The branch compared with, and the changes since it forked from it.
    pub base: Option<String>,
    pub branch: String,
}

/// `picked`: the branch to compare with when not the one work starts from.
pub fn refresh(root: &Path, picked: Option<String>, known: &mut Known) -> Refreshed {
    known.fresh(root);
    let files = files(root);
    let mut flat: std::collections::BTreeSet<PathBuf> = files.iter().filter_map(|file| file.rsplit_once('/')).map(|(dir, _)| root.join(dir)).collect();
    flat.insert(root.to_path_buf());
    flat.extend(known.git_dir.clone());
    let watch = (flat, known.refs.clone().into_iter().collect());

    let Status { mut info, oid, states, untracked } = status(root);
    info.has_origin = known.has_origin;
    // Not for the base branch itself (main, tracking origin/main).
    if let Some(base) = known.base.clone().filter(|b| b.rsplit('/').next() != Some(info.head.as_str())) {
        info.behind_base = git(root, &["rev-list", "--count", &format!("HEAD..{base}")]).and_then(|n| n.trim().parse().ok()).unwrap_or(0);
        // Not published yet: its own commits are the ones ahead.
        if info.upstream.is_none() {
            info.ahead = git(root, &["rev-list", "--count", &format!("{base}..HEAD")]).and_then(|n| n.trim().parse().ok()).unwrap_or(0);
        }
        info.base = Some(base);
    }
    if let Some(last) = git(root, &["log", "-1", "--format=%h%x1f%s"]) {
        if let Some((hash, subject)) = last.trim_end().split_once('\x1f') {
            info.last_hash = hash.to_string();
            info.last_subject = subject.to_string();
        }
    }

    let untracked: Vec<&str> = untracked.iter().map(String::as_str).collect();
    let new_files = new_files_patch(root, &untracked);
    let working = tracked_patch(root, "HEAD") + &new_files;
    // On the base branch itself the comparison is the uncommitted diff again.
    let base = picked.or_else(|| known.base.clone().filter(|base| *base != info.head));
    let fork = base.as_ref().and_then(|base| git(root, &["merge-base", "HEAD", base])).map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    let (base, branch) = match fork {
        // No commit of its own yet: the same patch, not asked for twice.
        Some(fork) if fork == oid => (base, working.clone()),
        Some(fork) => (base, tracked_patch(root, &fork) + &new_files),
        None => (None, String::new()),
    };
    Refreshed { files, watch, info, states, working, base, branch }
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

/// Runs a command in `root` for a user action: stdout on success, stderr (or
/// stdout) as the error.
pub fn run(root: &Path, program: &str, args: &[&str]) -> Result<String, String> {
    let output = crate::procs::command(program)
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

/// A commit message for what would be committed (the staged changes, or
/// all of them), written by `claude -p` in the style of the latest commits.
pub fn suggest_message(root: &Path, staged_only: bool) -> Result<String, String> {
    use std::io::Write as _;
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
    // Quick and on its own: no tools, no MCP servers, no session kept.
    let mut child = crate::procs::command("claude")
        .current_dir(root)
        .args(["-p", "--model", "haiku", "--tools", "", "--strict-mcp-config", "--no-session-persistence"])
        .env_remove(crate::agent::SOCK_ENV)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("claude: {e}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(prompt.as_bytes());
    }
    let out = child.wait_with_output().map_err(|e| format!("claude: {e}"))?;
    let text = String::from_utf8_lossy(&out.stdout).trim().trim_matches('`').trim().to_string();
    if out.status.success() && !text.is_empty() {
        Ok(text)
    } else {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        Err(if err.is_empty() { "claude gave no message".into() } else { err })
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

fn is_lockfile(path: &str) -> bool {
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
    let Some(out) = git(root, &["blame", "--porcelain", "--", &file.to_string_lossy()]) else {
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
pub enum RebaseError {
    /// These files conflict; the rebase was undone.
    Conflicts(Vec<String>),
    Failed(String),
}

/// Fetches the base and rebases the branch onto it, uncommitted changes
/// stashed and put back. On a conflict nothing changes.
pub fn rebase_onto(root: &Path, base: &str) -> Result<String, RebaseError> {
    if let Some((remote, branch)) = base.split_once('/') {
        let _ = run(root, "git", &["fetch", remote, branch]);
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

/// Fetches the remote's default branch, quietly: no prompt for a password
/// or key (it fails instead), no gc afterwards, given up after a minute.
pub fn fetch_base(repo: &Path) -> Result<(), String> {
    let head = git(repo, &["symbolic-ref", "--short", "refs/remotes/origin/HEAD"]).map(|s| s.trim().to_string());
    let branch = match head.as_deref().and_then(|h| h.strip_prefix("origin/")) {
        Some(branch) => branch.to_string(),
        None => ["main", "master"]
            .into_iter()
            .find(|b| git(repo, &["rev-parse", "--verify", "-q", &format!("refs/remotes/origin/{b}")]).is_some_and(|s| !s.trim().is_empty()))
            .ok_or("no origin/main or origin/master")?
            .to_string(),
    };
    let mut child = crate::procs::command("git")
        .arg("-C")
        .arg(repo)
        .args(["fetch", "--quiet", "--no-auto-maintenance", "origin", &branch])
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
pub fn create_task_worktree(repo: &Path, branch: &str, copy_env: bool) -> Result<PathBuf, String> {
    worktree_at(repo, branch, &new_task_base(repo), copy_env)
}

fn worktree_at(repo: &Path, branch: &str, base: &str, copy_env: bool) -> Result<PathBuf, String> {
    let dir = task_worktree_dir(repo, branch);
    if dir.exists() {
        return Err(format!("{} already exists", dir.display()));
    }
    let dir_s = dir.to_string_lossy().into_owned();
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
pub fn create_variant_worktrees(repo: &Path, branches: &[String], from: Option<&str>, copy_env: bool) -> Result<(String, Vec<Result<PathBuf, String>>), String> {
    let base = from.map_or_else(|| new_task_base(repo), str::to_string);
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
        let dirs = std::fs::read_dir(task_worktree_dir(repo, ""))
            .into_iter()
            .flatten()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
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
    remove_task_worktree(repo, path, branch, false)
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

/// Removes a task's worktree and its branch; a lock is overridden only
/// when `unlock`.
pub fn remove_task_worktree(repo: &Path, path: &Path, branch: &str, unlock: bool) -> Result<(), String> {
    let path_s = path.to_string_lossy().into_owned();
    if path.exists() {
        let mut args = vec!["worktree", "remove", "--force"];
        if unlock {
            args.push("--force");
        }
        args.push(&path_s);
        git_status(repo, &args)?;
    } else {
        let _ = git_status(repo, &["worktree", "prune"]);
    }
    if !branch.is_empty() && !branch.starts_with('(') {
        let _ = git_status(repo, &["branch", "-D", branch]);
    }
    Ok(())
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
        let (from, made) = create_variant_worktrees(&repo, &branches, None, false).unwrap();
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
        let later = create_variant_worktrees(&repo, &["fix-c".to_string()], fork_point(&made[0]).as_deref(), false).unwrap().1.remove(0).unwrap();
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
        apply_hunk(&repo, &found[0].patch, true, false).unwrap();
        let text = std::fs::read_to_string(repo.join("n.txt")).unwrap();
        assert!(text.starts_with("1\n2\n3\n") && text.contains("twenty-eight"), "the first is back, the second is there");
        apply_hunk(&repo, &hunks(&working_patch(&repo))[0].stage, false, true).unwrap();
        assert!(git(&repo, &["diff", "--cached"]).unwrap().contains("+twenty-eight"), "staged");
        assert!(apply_hunk(&repo, &found[0].patch, true, false).is_err(), "what is no longer there cannot be taken back");
        sh(&repo, "git reset -q --hard; rm -f gone.txt; echo new > untracked.txt");
        assert!(hunks(&working_patch(&repo)).is_empty(), "a new file is no hunk");

        // Changes a line apart are one hunk to git, and still each its own:
        // a line changed, one removed, two added.
        sh(&repo, "git checkout -q -- n.txt; git reset -q; printf '1\\ntwo\\n3\\n5\\n6\\nsix\\nand a half\\n' > m.txt; seq 7 12 >> m.txt
            seq 1 12 > n.txt; git add n.txt; git commit -qm short; mv m.txt n.txt");
        let found = hunks(&working_patch(&repo));
        assert_eq!(found.iter().map(|h| (h.line, h.modified)).collect::<Vec<_>>(), [(2, true), (4, false), (7, true)]);
        let numbers = |repo: &Path| std::fs::read_to_string(repo.join("n.txt")).unwrap().replace('\n', " ");
        apply_hunk(&repo, &found[1].patch, true, false).unwrap();
        assert_eq!(numbers(&repo), "1 two 3 4 5 6 six and a half 7 8 9 10 11 12 ", "the removed line is back, alone");
        apply_hunk(&repo, &found[2].stage, false, true).unwrap();
        assert_eq!(git(&repo, &["diff", "--cached", "--numstat"]).unwrap().trim(), "2\t0\tn.txt", "only the two added lines are staged");
        let left = hunks(&working_patch(&repo));
        assert_eq!(left.len(), 2, "the patch is against the commit: what is staged is still in it");
        apply_hunk(&repo, &left[0].patch, true, false).unwrap();
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
            2 R. N... 100644 100644 100644 aaa bbb R100 to here.rs\0from.rs\0\
            u UU N... 100644 100644 100644 100644 a b c both.rs\0\
            ? notes/todo.md\0";
        let status = parse_status(out);
        assert_eq!((status.info.head.as_str(), status.info.upstream.as_deref(), status.info.ahead, status.info.behind), ("task", Some("origin/task"), 2, 1));
        assert_eq!(status.oid, "abc");
        let state = |path: &str| status.states.get(path).map(|s| (s.staged, s.unstaged));
        assert_eq!(state("src/a b.rs"), Some((false, true)));
        assert_eq!(state("new.rs"), Some((true, false)));
        assert_eq!(state("to here.rs"), Some((true, false)));
        assert_eq!(state("both.rs"), Some((true, true)));
        assert_eq!(state("notes/todo.md"), Some((false, true)));
        assert_eq!((status.states.len(), status.untracked.as_slice()), (5, ["notes/todo.md".to_string()].as_slice()));
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
        let read = refresh(&root, None, &mut known);

        assert_eq!((read.info.head.as_str(), read.info.base.as_deref(), read.info.has_origin, read.info.ahead), ("task", Some("origin/main"), true, 0));
        assert!(!read.info.last_hash.is_empty() && read.info.last_subject == "base");
        assert!(read.files.contains(&"src/tail.txt".to_string()) && read.files.contains(&"f.txt".to_string()));
        assert!(read.watch.0.contains(&root) && read.watch.0.contains(&root.join("src")) && read.watch.0.contains(&root.join(".git")));
        assert_eq!(read.watch.1, vec![root.join(".git/refs")]);
        let state = |path: &str| read.states.get(path).map(|s| (s.staged, s.unstaged));
        assert_eq!((state("f.txt"), state("src/staged.txt"), state("new one.txt")), (Some((false, true)), Some((true, false)), Some((false, true))));
        assert_eq!((read.base.as_deref(), read.branch == read.working), (Some("origin/main"), true), "no commit of its own: one patch");

        let ours = DiffFile::parse(&read.working).unwrap();
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
        assert!(read.working.len() < 10_000, "the large file is not in the patch");
        assert_eq!(first_changed_lines(&read.working).get("new one.txt"), Some(&1));

        // A commit of its own: the branch's patch has it, the working one does not.
        sh(&root, "git commit -qm task");
        let read = refresh(&root, None, &mut known);
        assert_eq!(read.info.ahead, 1);
        let (working, branch) = (DiffFile::parse(&read.working).unwrap(), DiffFile::parse(&read.branch).unwrap());
        assert!(!working.iter().any(|f| f.path().as_ref() == "src/staged.txt") && branch.iter().any(|f| f.path().as_ref() == "src/staged.txt"));
        assert!(branch.iter().any(|f| f.path().as_ref() == "new one.txt"), "untracked files are in both");
        // Compared with a branch picked by hand.
        assert_eq!(refresh(&root, Some("main".into()), &mut known).base.as_deref(), Some("main"));
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
            git config user.name Bob; printf 'one\\nTWO\\nthree\\n' > f.txt; git commit -qam second";
        assert!(Command::new("bash").arg("-ec").arg(script).current_dir(&base).status().unwrap().success());
        let lines = blame(&base, &base.join("f.txt"));
        let told: Vec<(&str, &str)> = lines.iter().map(|l| l.as_ref().map(|l| (l.author.as_str(), l.summary.as_str())).unwrap()).collect();
        assert_eq!(told, [("Ann", "first"), ("Bob", "second"), ("Ann", "first")]);
        assert!(std::sync::Arc::ptr_eq(lines[0].as_ref().unwrap(), lines[2].as_ref().unwrap()));
        assert!(lines[0].as_ref().unwrap().time > 0 && lines[1].as_ref().unwrap().hash.len() == 40);
        let _ = std::fs::remove_dir_all(&base);
    }
}
