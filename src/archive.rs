//! Archived tasks: what a task is when it is done, in one file, and what
//! brings it back. A task is a goal, its branch a carrier: archiving keeps
//! the goal (title, prompt, PR, what the agent said), the agent's sessions
//! and whatever work exists nowhere else, then the worktree and the local
//! branch go. Restoring makes a new worktree and lets the agent go on with
//! the same conversation.
//!
//! One `.tar.gz` per task, in `<state dir>/archive/<project>-<hash>/`:
//! - `task.json`, first, so a list reads only that (`Meta`);
//! - `sessions/<n>/<file>`: the files of the task's n-th agent session,
//!   relative to where its adapter keeps them (`Adapter::session_base`);
//! - `work/unpushed.bundle`: commits on no remote and no other branch;
//! - `work/uncommitted.patch`: changes not committed, new files too, binary-safe;
//! - `chat.jsonl`: a chat agent's conversation as the app kept it (src/acp).
//!
//! gzip and not zstd: `flate2` (pure Rust) is in the build already, through
//! the image crates, where zstd would add a C library to every target; an
//! archive is mostly text and written once.

use std::{
    fs::File,
    io::{BufReader, BufWriter, Read as _, Write as _},
    path::{Component, Path, PathBuf},
};

use serde::{Deserialize, Serialize};

use crate::agents::{Kind, Profile};

pub const FORMAT: u32 = 1;
const EXT: &str = ".tar.gz";
const META: &str = "task.json";
const BUNDLE: &str = "work/unpushed.bundle";
const PATCH: &str = "work/uncommitted.patch";
const CHAT: &str = "chat.jsonl";

/// What an archive says of its task.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct Meta {
    pub format: u32,
    /// The version of ideae that wrote it.
    pub ideae: String,
    pub title: String,
    pub prompt: String,
    /// The project's main checkout, and the worktree the task had.
    pub project: PathBuf,
    pub worktree: PathBuf,
    pub branch: String,
    /// The commit the task's branch left its base at, and its last.
    pub base: String,
    pub head: String,
    pub pr: Option<PrMeta>,
    /// Review comments not sent to the agent.
    pub review: Vec<crate::state::ReviewNote>,
    /// What the agent said last.
    pub said: Option<String>,
    pub sessions: Vec<SessionMeta>,
    /// The agent chosen when it was made, by profile id.
    pub agent: String,
    /// What its agent alone was told, when it was a variant.
    pub note: String,
    /// Unix seconds.
    pub created: u64,
    pub archived: u64,
    /// The branch's commits since its base, newest first (at most 50).
    pub commits: Vec<CommitLine>,
    /// Of those, the ones on no remote and no other branch: in the bundle.
    pub unpushed: Vec<CommitLine>,
    /// The files with changes not committed: in the patch.
    pub uncommitted: Vec<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct PrMeta {
    pub number: u64,
    pub url: String,
    /// open, draft, merged or closed, when it was archived.
    pub state: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct SessionMeta {
    pub profile: String,
    /// The profile's name and kind then: it may be gone by the restore.
    pub name: String,
    pub kind: Kind,
    pub id: Option<String>,
    pub started: u64,
    /// Relative to the adapter's base, with `/`.
    pub files: Vec<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct CommitLine {
    pub sha: String,
    pub subject: String,
}

impl CommitLine {
    pub fn short(&self) -> &str {
        prefix(&self.sha, 8)
    }
}

/// The first `n` characters, whatever an archive holds.
pub fn prefix(text: &str, n: usize) -> &str {
    text.char_indices().nth(n).map_or(text, |(at, _)| &text[..at])
}

/// Where archives are: `IDEAE_ARCHIVE_DIR`, else next to the state (so a
/// scratch `IDEAE_STATE` has scratch archives, and ideae-dev its own).
pub fn root() -> PathBuf {
    std::env::var_os("IDEAE_ARCHIVE_DIR").filter(|v| !v.is_empty()).map(PathBuf::from).unwrap_or_else(|| crate::state::state_dir().join("archive"))
}

/// A name for a file or folder, of letters, digits, `.`, `_` and `-`.
fn clean(name: &str) -> String {
    let cleaned: String = name.chars().map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') { c } else { '-' }).take(60).collect();
    let cleaned = cleaned.trim_matches(|c| c == '-' || c == '.').to_string();
    if cleaned.is_empty() { "task".into() } else { cleaned }
}

/// The folder of a project's archives: its name and a hash of its path.
pub fn project_dir(root: &Path, project: &Path) -> PathBuf {
    use sha2::{Digest as _, Sha256};
    let name = project.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let hash = Sha256::digest(project.to_string_lossy().as_bytes());
    let hex: String = hash.iter().take(4).map(|b| format!("{b:02x}")).collect();
    root.join(format!("{}-{hex}", clean(&name)))
}

/// A new key for a task's archive: its branch and when.
pub fn new_key(branch: &str, now: u64) -> String {
    format!("{}-{now}", clean(branch))
}

pub fn file_of(root: &Path, project: &Path, key: &str) -> PathBuf {
    project_dir(root, project).join(format!("{key}{EXT}"))
}

/// The key of an archive file, its name without the extension.
pub fn key_of(file: &Path) -> String {
    file.file_name().map(|n| n.to_string_lossy().trim_end_matches(EXT).to_string()).unwrap_or_default()
}

/// A day as `2026-11-17`, UTC.
pub fn date(secs: u64) -> String {
    crate::usage::stamp(secs)[..10].to_string()
}

fn git(root: &Path, args: &[&str]) -> Result<String, String> {
    crate::git::git_status(root, args)
}

/// Commits of `git log` with these arguments, as sha and subject.
fn commit_lines(root: &Path, args: &[&str]) -> Vec<CommitLine> {
    let mut all = vec!["log", "--format=%H%x00%s"];
    all.extend_from_slice(args);
    git(root, &all)
        .unwrap_or_default()
        .lines()
        .filter_map(|line| {
            let (sha, subject) = line.split_once('\0')?;
            Some(CommitLine { sha: sha.to_string(), subject: subject.to_string() })
        })
        .collect()
}

/// The commits of `root`'s HEAD that are on no remote and no other local
/// branch: what deleting its branch would lose. Merges are left out.
pub fn unpushed(root: &Path) -> Vec<CommitLine> {
    let mut args = vec!["--no-merges".to_string(), "HEAD".into(), "--not".into(), "--remotes".into()];
    if let Ok(own) = git(root, &["symbolic-ref", "--short", "-q", "HEAD"]) {
        args.push(format!("--exclude={}", own.trim()));
    }
    args.push("--branches".into());
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    commit_lines(root, &args)
}

/// Everything not committed in `root`, new files not ignored too, as one
/// binary-safe patch against HEAD, with the names of its files. The
/// worktree's own index is not touched: a scratch one is.
fn uncommitted_patch(root: &Path, scratch_index: &Path, out: &Path) -> Result<Vec<String>, String> {
    let with_index = |args: &[&str]| -> Result<std::process::Output, String> {
        crate::procs::command("git")
            .env("GIT_INDEX_FILE", scratch_index)
            .env("GIT_TERMINAL_PROMPT", "0")
            .args(["-c", "core.quotePath=false", "-C"])
            .arg(root)
            .args(args)
            .output()
            .map_err(|e| format!("git: {e}"))
            .and_then(|o| if o.status.success() { Ok(o) } else { Err(String::from_utf8_lossy(&o.stderr).trim().to_string()) })
    };
    with_index(&["read-tree", "HEAD"])?;
    with_index(&["add", "-A"])?;
    let names = with_index(&["diff", "--cached", "--name-only", "-z", "HEAD"])?;
    let names: Vec<String> = String::from_utf8_lossy(&names.stdout).split('\0').filter(|n| !n.is_empty()).map(str::to_string).collect();
    if names.is_empty() {
        return Ok(names);
    }
    let file = File::create(out).map_err(|e| e.to_string())?;
    let status = crate::procs::command("git")
        .env("GIT_INDEX_FILE", scratch_index)
        .args(["-C"])
        .arg(root)
        .args(["diff", "--cached", "--binary", "--no-color", "--no-ext-diff", "--no-textconv", "HEAD"])
        .stdout(file)
        .status()
        .map_err(|e| format!("git: {e}"))?;
    if !status.success() {
        return Err("git diff of the uncommitted changes failed".into());
    }
    Ok(names)
}

/// What the app hands over to archive a task: its facts, and the profile
/// of each of its sessions (none: removed since).
pub struct Job {
    pub meta: Meta,
    pub profiles: Vec<Option<Profile>>,
    pub dest: PathBuf,
}

/// A folder removed when dropped.
struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Writes the archive of the task at `job.meta.worktree` to `job.dest`
/// and reads it back. Nothing of the worktree changes; on any error no
/// archive is left. Returns what was written. Not for the UI thread.
pub fn write(job: Job) -> Result<Meta, String> {
    let Job { mut meta, profiles, dest } = job;
    let root = meta.worktree.clone();
    let dir = dest.parent().ok_or("no folder for the archive")?.to_path_buf();
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let name = dest.file_name().ok_or("no name for the archive")?.to_string_lossy().into_owned();
    let scratch = Scratch(dir.join(format!(".{name}.work{}", std::process::id())));
    let _ = std::fs::remove_dir_all(&scratch.0);
    std::fs::create_dir_all(&scratch.0).map_err(|e| e.to_string())?;

    meta.format = FORMAT;
    meta.ideae = env!("CARGO_PKG_VERSION").to_string();
    meta.archived = crate::state::now();
    meta.head = git(&root, &["rev-parse", "HEAD"]).map(|h| h.trim().to_string()).map_err(|e| format!("{}: {e}", root.display()))?;
    if meta.base.is_empty() {
        meta.base = crate::git::fork_point(&root).unwrap_or_default();
    }
    if !meta.base.is_empty() && meta.base != meta.head {
        meta.commits = commit_lines(&root, &["-n", "50", &format!("{}..HEAD", meta.base)]);
    }
    meta.unpushed = unpushed(&root);

    // What exists nowhere else: in files of the scratch folder first.
    let mut work: Vec<(PathBuf, &str)> = Vec::new();
    if !meta.unpushed.is_empty() {
        let bundle = scratch.0.join("unpushed.bundle");
        let bundle_s = bundle.to_string_lossy().into_owned();
        let mut args = vec!["bundle", "create", "-q", &bundle_s, "HEAD", "--not", "--remotes"];
        let own = git(&root, &["symbolic-ref", "--short", "-q", "HEAD"]).map(|b| format!("--exclude={}", b.trim()));
        if let Ok(own) = own.as_deref() {
            args.push(own);
        }
        args.push("--branches");
        git(&root, &args).map_err(|e| format!("the bundle of unpushed commits: {e}"))?;
        work.push((bundle, BUNDLE));
    }
    let patch = scratch.0.join("uncommitted.patch");
    meta.uncommitted = uncommitted_patch(&root, &scratch.0.join("index"), &patch).map_err(|e| format!("the uncommitted changes: {e}"))?;
    if !meta.uncommitted.is_empty() {
        work.push((patch, PATCH));
    }
    let chat = crate::acp::journal_path(&root);
    if chat.is_file() {
        work.push((chat, CHAT));
    }

    // The agents' sessions, through their adapters.
    let mut sessions: Vec<(PathBuf, String)> = Vec::new();
    for (n, (session, profile)) in meta.sessions.iter_mut().zip(&profiles).enumerate() {
        let Some(profile) = profile else { continue };
        let Some(found) = profile.adapter().session_files(profile, &root, session.id.as_deref()) else { continue };
        if session.id.is_none() {
            session.id = found.id.clone();
        }
        for rel in found.files {
            let Some(name) = archive_name(&rel) else { continue };
            sessions.push((found.base.join(&rel), format!("sessions/{n}/{name}")));
            session.files.push(name);
        }
    }
    if meta.said.is_none() {
        let last = meta.sessions.iter().zip(&profiles).rev().find_map(|(s, p)| Some((s, p.as_ref()?)));
        meta.said = last.and_then(|(s, p)| p.adapter().last_answer(&p.adapter().transcript(p, &root, s.id.as_deref())?));
    }

    let tmp = dir.join(format!(".{name}.tmp{}", std::process::id()));
    let written = (|| -> Result<Vec<(String, u64)>, String> {
        let file = File::create(&tmp).map_err(|e| format!("{}: {e}", tmp.display()))?;
        let mut tar = tar::Builder::new(flate2::write::GzEncoder::new(BufWriter::new(file), flate2::Compression::default()));
        let mut expected = Vec::new();
        let json = serde_json::to_vec_pretty(&meta).map_err(|e| e.to_string())?;
        let mut header = tar::Header::new_gnu();
        header.set_size(json.len() as u64);
        header.set_mode(0o644);
        header.set_mtime(meta.archived);
        header.set_cksum();
        tar.append_data(&mut header, META, json.as_slice()).map_err(|e| e.to_string())?;
        expected.push((META.to_string(), json.len() as u64));
        for (from, name) in sessions.iter().map(|(f, n)| (f, n.as_str())).chain(work.iter().map(|(f, n)| (f, *n))) {
            let mut source = File::open(from).map_err(|e| format!("{}: {e}", from.display()))?;
            let size = source.metadata().map_err(|e| e.to_string())?.len();
            let mut header = tar::Header::new_gnu();
            header.set_size(size);
            header.set_mode(0o644);
            header.set_mtime(meta.archived);
            header.set_cksum();
            // A transcript may still grow: what it had when looked at is what goes in.
            tar.append_data(&mut header, name, (&mut source).take(size)).map_err(|e| format!("{}: {e}", from.display()))?;
            expected.push((name.to_string(), size));
        }
        let gz = tar.into_inner().map_err(|e| e.to_string())?;
        let file = gz.finish().map_err(|e| e.to_string())?.into_inner().map_err(|e| e.to_string())?;
        file.sync_all().map_err(|e| e.to_string())?;
        Ok(expected)
    })();
    let verified = written.and_then(|expected| {
        let found = entries(&tmp)?;
        if found != expected {
            return Err("the archive read back is not what was written".into());
        }
        Ok(())
    });
    if let Err(e) = verified.and_then(|()| std::fs::rename(&tmp, &dest).map_err(|e| e.to_string())) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(meta)
}

/// A relative path as an archive names it: only plain parts, with `/`.
fn archive_name(rel: &Path) -> Option<String> {
    let parts: Option<Vec<String>> = rel.components().map(|c| if let Component::Normal(part) = c { Some(part.to_string_lossy().into_owned()) } else { None }).collect();
    parts.filter(|p| !p.is_empty()).map(|p| p.join("/"))
}

/// A name from an archive as a relative path, refused when it would leave
/// the folder it is put in.
fn safe_rel(name: &str) -> Option<PathBuf> {
    let rel = PathBuf::from(name);
    rel.components().all(|c| matches!(c, Component::Normal(_))).then_some(rel)
}

fn open(file: &Path) -> Result<tar::Archive<flate2::read::GzDecoder<BufReader<File>>>, String> {
    let f = File::open(file).map_err(|e| format!("{}: {e}", file.display()))?;
    Ok(tar::Archive::new(flate2::read::GzDecoder::new(BufReader::new(f))))
}

/// Every entry of an archive with its size, all of it read (the gzip
/// checksum is checked at its end).
pub fn entries(file: &Path) -> Result<Vec<(String, u64)>, String> {
    let mut archive = open(file)?;
    let mut found = Vec::new();
    for entry in archive.entries().map_err(|e| e.to_string())? {
        let mut entry = entry.map_err(|e| e.to_string())?;
        let name = entry.path().map_err(|e| e.to_string())?.to_string_lossy().replace('\\', "/");
        let read = std::io::copy(&mut entry, &mut std::io::sink()).map_err(|e| e.to_string())?;
        found.push((name, read));
    }
    let mut rest = archive.into_inner();
    std::io::copy(&mut rest, &mut std::io::sink()).map_err(|e| e.to_string())?;
    Ok(found)
}

/// What an archive says of its task: only its first entry is read.
pub fn read_meta(file: &Path) -> Result<Meta, String> {
    let mut archive = open(file)?;
    let mut entries = archive.entries().map_err(|e| e.to_string())?;
    let mut first = entries.next().ok_or("an empty archive")?.map_err(|e| e.to_string())?;
    if first.path().map_err(|e| e.to_string())?.to_string_lossy() != META {
        return Err("not an archive of a task".into());
    }
    let mut json = String::new();
    first.read_to_string(&mut json).map_err(|e| e.to_string())?;
    serde_json::from_str(&json).map_err(|e| e.to_string())
}

/// An archive on disk.
#[derive(Clone, Debug)]
pub struct Archived {
    pub file: PathBuf,
    pub size: u64,
    pub meta: std::sync::Arc<Meta>,
}

impl Archived {
    pub fn key(&self) -> String {
        key_of(&self.file)
    }

    /// What a search looks in.
    pub fn haystack(&self) -> String {
        let m = &self.meta;
        let pr = m.pr.as_ref().map(|pr| format!("#{} pr {}", pr.number, pr.state)).unwrap_or_default();
        format!("{} {} {} {pr} {}", m.title, m.prompt, m.branch, date(m.archived)).to_lowercase()
    }
}

/// Every archive under `root`, newest first.
/// Each with its meta read once: Recent tasks lists them at every opening.
/// Kept by file, size and time written; what is gone from disk leaves it.
pub fn list(root: &Path) -> Vec<Archived> {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};
    type Seen = HashMap<PathBuf, (u64, Option<std::time::SystemTime>, Arc<Meta>)>;
    static READ: Mutex<Option<Seen>> = Mutex::new(None);
    let mut read = READ.lock().unwrap_or_else(|e| e.into_inner());
    let before = read.take().unwrap_or_default();
    let mut now: Seen = HashMap::new();
    let mut all = Vec::new();
    for dir in std::fs::read_dir(root).into_iter().flatten().flatten() {
        for entry in std::fs::read_dir(dir.path()).into_iter().flatten().flatten() {
            let file = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') || !name.ends_with(EXT) {
                continue;
            }
            let stat = entry.metadata().ok();
            let size = stat.as_ref().map_or(0, |m| m.len());
            let written = stat.and_then(|m| m.modified().ok());
            let meta = match before.get(&file) {
                Some((s, w, meta)) if *s == size && *w == written => meta.clone(),
                _ => match read_meta(&file) {
                    Ok(meta) => Arc::new(meta),
                    Err(_) => continue,
                },
            };
            now.insert(file.clone(), (size, written, meta.clone()));
            all.push(Archived { file, size, meta });
        }
    }
    *read = Some(now);
    all.sort_by(|a, b| b.meta.archived.cmp(&a.meta.archived));
    all
}

/// Deletes the archives older than `max_age` seconds (by when they were
/// written), and what an archive left half-made a day ago. Returns how
/// many archives went.
pub fn clean_up(root: &Path, max_age: u64, now: u64) -> usize {
    let mut removed = 0;
    let age = |entry: &std::fs::DirEntry| {
        let written = entry.metadata().and_then(|m| m.modified()).ok()?;
        let secs = written.duration_since(std::time::UNIX_EPOCH).ok()?.as_secs();
        Some(now.saturating_sub(secs))
    };
    for dir in std::fs::read_dir(root).into_iter().flatten().flatten() {
        for entry in std::fs::read_dir(dir.path()).into_iter().flatten().flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let Some(age) = age(&entry) else { continue };
            if name.starts_with('.') && age > 86_400 {
                let path = entry.path();
                let _ = if path.is_dir() { std::fs::remove_dir_all(&path) } else { std::fs::remove_file(&path) };
            } else if !name.starts_with('.') && name.ends_with(EXT) && max_age > 0 && age > max_age && std::fs::remove_file(entry.path()).is_ok() {
                removed += 1;
            }
        }
        let _ = std::fs::remove_dir(dir.path());
    }
    removed
}

// ------------------------------------------------------------ restore

/// A PR as it is when the task is restored.
#[derive(Clone, Debug, PartialEq)]
pub struct PrNow {
    pub state: crate::pr::PrState,
    /// The branch it is of.
    pub head: String,
}

/// Where a restored task starts.
#[derive(Clone, Debug, PartialEq)]
pub enum Start {
    /// A new branch from origin's default one, as new tasks are made.
    Origin,
    /// The open PR's branch, fetched: its work is not in main yet.
    PrBranch(String),
}

/// How the agent goes on.
#[derive(Clone, Debug, PartialEq)]
pub enum Conversation {
    /// The same session, resumed by its id under the same profile.
    Resume { profile: Profile, session: usize, id: String },
    /// A new session, told the story so far.
    Summary { profile: Option<Profile>, why: String },
}

/// What a restore can do, before it is asked to.
#[derive(Clone, Debug, PartialEq)]
pub struct Offer {
    pub start: Start,
    /// The PR's branch to go on with, when the PR is open.
    pub pr_branch: Option<String>,
    pub commits: usize,
    pub changes: usize,
    pub conversation: Conversation,
}

/// What a restore can do: from origin, unless the PR is still open; the
/// work kept, if any; the conversation resumed when its profile is still
/// there and can resume, and a summary is not what is wanted.
pub fn offer(meta: &Meta, pr: Option<&PrNow>, profiles: &[Profile], fallback: Option<&Profile>, summary: bool) -> Offer {
    use crate::pr::PrState;
    let pr_branch = pr.filter(|pr| matches!(pr.state, PrState::Open | PrState::Draft) && !pr.head.is_empty()).map(|pr| pr.head.clone());
    let start = pr_branch.clone().map_or(Start::Origin, Start::PrBranch);
    let last = meta.sessions.iter().enumerate().rev().find(|(_, s)| s.id.is_some());
    let same = last.and_then(|(n, s)| Some((n, s, profiles.iter().find(|p| p.id == s.profile)?)));
    let conversation = match (same, last) {
        (Some((n, s, profile)), _) if !summary && profile.adapter().resumes() => {
            Conversation::Resume { profile: profile.clone(), session: n, id: s.id.clone().unwrap_or_default() }
        }
        (Some((_, _, profile)), _) => {
            let why = if summary { "Settings say a summary only".to_string() } else { format!("{} cannot resume a session", profile.name) };
            Conversation::Summary { profile: Some(profile.clone()), why }
        }
        (None, Some((_, s))) => Conversation::Summary { profile: fallback.cloned(), why: format!("its agent ({}) is no longer set up", if s.name.is_empty() { &s.profile } else { &s.name }) },
        (None, None) => Conversation::Summary { profile: fallback.cloned(), why: "no session of its agent was kept".into() },
    };
    Offer { start, pr_branch, commits: meta.unpushed.len(), changes: meta.uncommitted.len(), conversation }
}

/// What was asked of a restore.
#[derive(Clone, Debug)]
pub struct Plan {
    pub start: Start,
    /// The new branch, for a start from origin.
    pub branch: String,
    pub commits: bool,
    pub changes: bool,
    /// The profile of each archived session; none, its files stay out.
    pub profiles: Vec<Option<Profile>>,
    pub copy_env: bool,
}

/// How bringing back a piece of work went.
#[derive(Clone, Debug, PartialEq, Default)]
pub enum Applied {
    /// There was none.
    #[default]
    None,
    Done,
    /// Not asked for: it stays in the archive.
    Left,
    /// It did not apply; where it is kept instead.
    Failed { why: String, kept: String },
}

#[derive(Clone, Debug, Default)]
pub struct Restored {
    pub path: PathBuf,
    pub branch: String,
    /// The commit it starts from.
    pub base: String,
    pub start_word: String,
    pub commits: Applied,
    pub changes: Applied,
    /// Session files put back for the agent.
    pub placed: usize,
}

impl Restored {
    /// Something of the archive is not in the restored task: the archive
    /// stays when the task is archived again.
    pub fn left_something(&self) -> bool {
        !matches!(self.commits, Applied::None | Applied::Done) || !matches!(self.changes, Applied::None | Applied::Done)
    }
}

/// Makes the restored task's worktree, brings back the work asked for and
/// puts the sessions' files where their agents look for them in it. The
/// archive itself is not changed. Not for the UI thread.
pub fn restore(repo: &Path, archive: &Path, meta: &Meta, plan: &Plan) -> Result<Restored, String> {
    let path = match &plan.start {
        Start::Origin => crate::git::create_task_worktree(repo, &plan.branch, plan.copy_env && crate::project_config::resolve(repo).config.task.copy.is_none())?,
        Start::PrBranch(branch) => crate::git::remote_branch_worktree(repo, branch, plan.copy_env && crate::project_config::resolve(repo).config.task.copy.is_none())?,
    };
    // Whatever goes wrong from here takes the worktree and its branch with it:
    // left, they would come back as a plain task.
    let filled = (|| -> Result<Restored, String> {
        if let Some(copy) = crate::project_config::resolve(repo).config.task.copy {
            crate::project_config::copy_ignored(repo, &path, &copy);
        }
        let branch = match &plan.start {
            Start::Origin => plan.branch.clone(),
            Start::PrBranch(branch) => branch.clone(),
        };
        let start_word = match &plan.start {
            Start::Origin => crate::git::base_ref(&path).unwrap_or_else(|| "the main branch".into()),
            Start::PrBranch(branch) => format!("the PR's branch {branch}"),
        };
        let base = git(&path, &["rev-parse", "HEAD"]).map(|h| h.trim().to_string()).unwrap_or_default();
        let mut restored = Restored { path: path.clone(), branch, base, start_word, ..Default::default() };
        let git_path = |name: &str| -> PathBuf {
            git(&path, &["rev-parse", "--path-format=absolute", "--git-path", name]).map(|p| PathBuf::from(p.trim())).unwrap_or_else(|_| path.join(name))
        };

        // One pass through the archive: the work to a place of the worktree's
        // own git folder, the sessions where their agents look.
        let bundle = git_path("ideae-unpushed.bundle");
        let patch = git_path("ideae-uncommitted.patch");
        let bases: Vec<Option<PathBuf>> = plan.profiles.iter().map(|p| p.as_ref().and_then(|p| p.adapter().session_base(p, &path))).collect();
        let mut archive_file = open(archive)?;
        for entry in archive_file.entries().map_err(|e| e.to_string())? {
            let mut entry = entry.map_err(|e| e.to_string())?;
            let name = entry.path().map_err(|e| e.to_string())?.to_string_lossy().replace('\\', "/");
            let target = match name.as_str() {
                BUNDLE if plan.commits => Some(bundle.clone()),
                PATCH if plan.changes => Some(patch.clone()),
                CHAT => Some(crate::acp::journal_path(&path)),
                _ => name.strip_prefix("sessions/").and_then(|rest| {
                    let (n, rel) = rest.split_once('/')?;
                    let base = bases.get(n.parse::<usize>().ok()?)?.as_ref()?;
                    Some(base.join(safe_rel(rel)?)).filter(|t| !t.exists())
                }),
            };
            let Some(target) = target else { continue };
            if let Some(dir) = target.parent() {
                std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
            }
            let mut out = File::create(&target).map_err(|e| format!("{}: {e}", target.display()))?;
            std::io::copy(&mut entry, &mut out).map_err(|e| format!("{}: {e}", target.display()))?;
            out.flush().map_err(|e| e.to_string())?;
            if name.starts_with("sessions/") {
                restored.placed += 1;
            }
        }

        restored.commits = match (meta.unpushed.is_empty(), plan.commits) {
            (true, _) => Applied::None,
            (false, false) => Applied::Left,
            (false, true) => {
                let keep = format!("refs/ideae/restored/{}", key_of(archive));
                let bundle_s = bundle.to_string_lossy().into_owned();
                let fetched = git(&path, &["fetch", "-q", &bundle_s, &format!("HEAD:{keep}")]);
                let picked = fetched.and_then(|_| {
                    let mut args = vec!["cherry-pick".to_string(), "--allow-empty".into()];
                    args.extend(meta.unpushed.iter().rev().map(|c| c.sha.clone()));
                    let args: Vec<&str> = args.iter().map(String::as_str).collect();
                    git(&path, &args).inspect_err(|_| {
                        let _ = git(&path, &["cherry-pick", "--abort"]);
                    })
                });
                match picked {
                    Ok(_) => {
                        let _ = git(&path, &["update-ref", "-d", &keep]);
                        let _ = std::fs::remove_file(&bundle);
                        Applied::Done
                    }
                    Err(why) => {
                        let kept = if git(&path, &["rev-parse", "--verify", "-q", &keep]).is_ok() { keep } else { bundle_s };
                        Applied::Failed { why: first_line(&why), kept }
                    }
                }
            }
        };
        restored.changes = match (meta.uncommitted.is_empty(), plan.changes) {
            (true, _) => Applied::None,
            (false, false) => Applied::Left,
            (false, true) => {
                let patch_s = patch.to_string_lossy().into_owned();
                let applied = git(&path, &["apply", "--check", "--binary", &patch_s]).and_then(|_| git(&path, &["apply", "--binary", &patch_s]));
                match applied {
                    Ok(_) => {
                        let _ = std::fs::remove_file(&patch);
                        Applied::Done
                    }
                    Err(why) => Applied::Failed { why: first_line(&why), kept: patch_s },
                }
            }
        };
        Ok(restored)
    })();
    if filled.is_err() {
        let branch = match &plan.start {
            Start::Origin => plan.branch.as_str(),
            Start::PrBranch(branch) => branch.as_str(),
        };
        let _ = crate::git::remove_task_worktree(repo, &path, branch, false);
    }
    filled
}

fn first_line(text: &str) -> String {
    text.lines().find(|l| !l.trim().is_empty()).unwrap_or("it did not apply").trim().to_string()
}

/// What the agent of a restored task is told first: when it was archived,
/// what became of its PR, that the code moved on, what it had done, and
/// what came back. What the user wants comes next.
pub fn note(meta: &Meta, pr: Option<&PrNow>, restored: &Restored) -> String {
    use crate::pr::PrState;
    let mut text = format!(
        "This task was archived on {} and has just been restored, in a new worktree at {} on the branch {}, made from {}. The worktree you worked in before ({}) is gone.",
        date(meta.archived),
        restored.path.display(),
        restored.branch,
        restored.start_word,
        meta.worktree.display()
    );
    let state = pr.map(|p| p.state).or_else(|| meta.pr.as_ref().and_then(|p| PrState::from_word(&p.state)));
    match (&meta.pr, state) {
        (Some(p), Some(PrState::Merged)) => text.push_str(&format!("\nIts work went to PR #{} ({}), which is merged.", p.number, p.url)),
        (Some(p), Some(PrState::Open | PrState::Draft)) => text.push_str(&format!("\nIts work went to PR #{} ({}), which is still open.", p.number, p.url)),
        (Some(p), Some(PrState::Closed)) => text.push_str(&format!("\nIts work went to PR #{} ({}), which was closed without being merged.", p.number, p.url)),
        (Some(p), None) => text.push_str(&format!("\nIts work went to PR #{} ({}).", p.number, p.url)),
        (None, _) => text.push_str("\nIts work had no pull request."),
    }
    text.push_str("\nThe main branch has moved on since then: re-check the current code before relying on what you remember of it.");
    if !meta.commits.is_empty() {
        text.push_str("\nYour commits then:");
        for c in meta.commits.iter().take(10) {
            text.push_str(&format!("\n- {} {}", c.short(), c.subject));
        }
        if meta.commits.len() > 10 {
            text.push_str(&format!("\n- and {} more", meta.commits.len() - 10));
        }
    }
    let n = meta.unpushed.len();
    let commits = format!("The {n} commit{} that {} on no remote", if n == 1 { "" } else { "s" }, if n == 1 { "was" } else { "were" });
    match &restored.commits {
        Applied::None => {}
        Applied::Done => text.push_str(&format!("\n{commits} {} been applied again on this branch.", if n == 1 { "has" } else { "have" })),
        Applied::Left => text.push_str(&format!("\n{commits} {} not brought back.", if n == 1 { "was" } else { "were" })),
        Applied::Failed { why, kept } => text.push_str(&format!("\n{commits} did not apply cleanly here ({why}); they are kept at {kept}: cherry-pick them if they are still wanted.")),
    }
    match &restored.changes {
        Applied::None => {}
        Applied::Done => text.push_str("\nThe changes that were not committed then are back, not committed."),
        Applied::Left => text.push_str("\nThe changes that were not committed then were not brought back."),
        Applied::Failed { why, kept } => text.push_str(&format!("\nThe changes that were not committed then did not apply cleanly ({why}); the patch is at {kept}.")),
    }
    text.push_str("\nWhat I want next comes in my next message; until then, only say in a line that you are ready.");
    text
}

/// For an agent that starts afresh: the task, what was said last, the
/// review notes left, then the note.
pub fn summary(meta: &Meta, pr: Option<&PrNow>, restored: &Restored) -> String {
    let mut text = format!("You are taking up a task an agent worked on before it was archived; its conversation is not available to you.\n\nThe task: {}\n", meta.title);
    if !meta.prompt.trim().is_empty() && meta.prompt.trim() != meta.title.trim() {
        text.push_str(&format!("\nAs it was given:\n<task>\n{}\n</task>\n", meta.prompt.trim()));
    }
    if let Some(said) = meta.said.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        text.push_str(&format!("\nWhat the agent said last:\n<previous>\n{said}\n</previous>\n"));
    }
    if !meta.review.is_empty() {
        text.push_str("\nReview comments that were not sent to it:\n");
        for note in &meta.review {
            text.push_str(&format!("- {}: {}\n", note.location, note.text.trim()));
        }
    }
    text.push('\n');
    text.push_str(&note(meta, pr, restored).replace("Your commits then:", "Its commits then:"));
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pr::PrState;

    fn profile(id: &str, kind: Kind) -> Profile {
        Profile { id: id.into(), name: id.into(), kind, command: kind.program().into(), env: Default::default(), ..Default::default() }
    }

    #[test]
    fn short_ids_of_whatever_an_archive_says() {
        assert_eq!(CommitLine { sha: "0123456789ab".into(), subject: String::new() }.short(), "01234567");
        assert_eq!(prefix("ééééééééé", 8), "éééééééé");
        assert_eq!(prefix("abc", 8), "abc");
    }

    fn meta_with(sessions: &[(&str, Option<&str>)]) -> Meta {
        Meta {
            title: "Fix login".into(),
            sessions: sessions.iter().map(|(p, id)| SessionMeta { profile: p.to_string(), name: p.to_string(), id: id.map(str::to_string), ..Default::default() }).collect(),
            unpushed: vec![CommitLine { sha: "a".repeat(40), subject: "wip".into() }],
            uncommitted: vec!["f.txt".into()],
            ..Default::default()
        }
    }

    #[test]
    fn a_merged_pr_starts_from_origin_an_open_one_offers_its_branch() {
        let all = [profile("claude", Kind::Claude), profile("other", Kind::Generic)];
        let meta = meta_with(&[("claude", Some("s-1"))]);
        let merged = PrNow { state: PrState::Merged, head: "fix".into() };
        let open = PrNow { state: PrState::Open, head: "fix".into() };
        let o = offer(&meta, Some(&merged), &all, None, false);
        assert_eq!((o.start, o.pr_branch, o.commits, o.changes), (Start::Origin, None, 1, 1));
        assert_eq!(o.conversation, Conversation::Resume { profile: all[0].clone(), session: 0, id: "s-1".into() });
        let o = offer(&meta, Some(&open), &all, None, false);
        assert_eq!((o.start, o.pr_branch.as_deref()), (Start::PrBranch("fix".into()), Some("fix")));
        assert_eq!(offer(&meta, None, &all, None, false).start, Start::Origin, "no PR known");
        // A summary when asked, or when the profile is gone or cannot resume.
        assert!(matches!(offer(&meta, None, &all, None, true).conversation, Conversation::Summary { profile: Some(p), .. } if p.id == "claude"));
        let gone = offer(&meta, None, &all[1..], Some(&all[1]), false);
        assert!(matches!(&gone.conversation, Conversation::Summary { profile: Some(p), why } if p.id == "other" && why.contains("no longer")), "{gone:?}");
        let generic = offer(&meta_with(&[("other", Some("x"))]), None, &all, None, false);
        assert!(matches!(generic.conversation, Conversation::Summary { .. }));
    }

    #[test]
    fn the_note_tells_the_story() {
        let mut meta = meta_with(&[]);
        meta.archived = 1_760_000_000;
        meta.worktree = PathBuf::from("/w/repo.worktrees/fix");
        meta.pr = Some(PrMeta { number: 12, url: "https://example.com/pr/12".into(), state: "open".into() });
        meta.commits = vec![CommitLine { sha: "abcdef1234".into(), subject: "Fix the form".into() }];
        let restored = Restored { path: "/w/repo.worktrees/fix-2".into(), branch: "fix-2".into(), start_word: "origin's main branch".into(), commits: Applied::Done, changes: Applied::Failed { why: "patch does not apply".into(), kept: "/g/p.patch".into() }, ..Default::default() };
        let text = note(&meta, Some(&PrNow { state: PrState::Merged, head: "fix".into() }), &restored);
        for part in [
            "archived on 2025-10-09",
            "/w/repo.worktrees/fix-2",
            "PR #12 (https://example.com/pr/12), which is merged",
            "re-check the current code",
            "- abcdef12 Fix the form",
            "The 1 commit that was on no remote has been applied again",
            "the patch is at /g/p.patch",
            "comes in my next message",
        ] {
            assert!(text.contains(part), "{part} in {text}");
        }
        meta.pr = None;
        let told = summary(&meta, None, &Restored { commits: Applied::Left, ..restored });
        assert!(told.contains("no pull request") && told.contains("not brought back") && told.contains("The task: Fix login"), "{told}");
    }

    #[test]
    fn names_stay_inside() {
        assert_eq!(safe_rel("a/b.jsonl"), Some(PathBuf::from("a/b.jsonl")));
        assert_eq!(safe_rel("../x"), None);
        assert_eq!(safe_rel("/etc/x"), None);
        assert_eq!(archive_name(Path::new("s/x.jsonl")).as_deref(), Some("s/x.jsonl"));
        assert_eq!(new_key("feat/DEV-1 x", 5), "feat-DEV-1-x-5");
        assert_eq!(key_of(&file_of(Path::new("/a"), Path::new("/w/repo"), "k-1")), "k-1");
        assert!(project_dir(Path::new("/a"), Path::new("/w/repo")).file_name().unwrap().to_string_lossy().starts_with("repo-"));
    }

    #[test]
    fn old_archives_are_cleaned_up() {
        let dir = std::env::temp_dir().join(format!("ideae-archive-age-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let project = dir.join("repo-1");
        std::fs::create_dir_all(&project).unwrap();
        let now = crate::state::now();
        let made = |name: &str, days: u64| {
            let path = project.join(name);
            std::fs::write(&path, "x").unwrap();
            let at = std::time::UNIX_EPOCH + std::time::Duration::from_secs(now - days * 86_400);
            File::options().write(true).open(&path).unwrap().set_modified(at).unwrap();
            path
        };
        let old = made("old-1.tar.gz", 100);
        let new = made("new-1.tar.gz", 10);
        let half = made(".old-1.tar.gz.tmp1", 2);
        assert_eq!(clean_up(&dir, 0, now), 0, "off");
        assert!(old.exists() && !half.exists(), "what was left half-made goes anyway");
        assert_eq!(clean_up(&dir, 90 * 86_400, now), 1);
        assert!(!old.exists() && new.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    // Scripted with sh, which Windows lacks.
    #[cfg(unix)]
    fn sh(dir: &Path, script: &str) {
        let ok = std::process::Command::new("sh").arg("-ec").arg(script).current_dir(dir).status().unwrap().success();
        assert!(ok, "{script}");
    }

    /// A task with a Claude Code session (and a subagent's file), a commit
    /// on no remote, a changed file and a new binary one: archived, its
    /// worktree removed, restored from origin with all of it back, the
    /// transcript under the new worktree's folder by the same id.
    #[cfg(unix)]
    #[test]
    fn a_task_goes_into_an_archive_and_comes_back() {
        let _one = crate::workspace::variants::tests::ONE_WINDOW.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("ideae-archive-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // SAFETY: under the lock the tests share; the chat's journal goes next to this state.
        unsafe { std::env::set_var("IDEAE_STATE", dir.join("state.json")) };
        sh(&dir, "git init -q --bare remote.git; git init -q -b main repo; cd repo; git config user.email t@t; git config user.name t
            echo one > f.txt; git add f.txt; git commit -qm base; git remote add origin ../remote.git; git push -q origin main
            git fetch -q origin; git worktree add -q -b fix ../fix origin/main
            cd ../fix; echo pushed > p.txt; git add p.txt; git commit -qm 'pushed one'; git push -q origin fix
            echo two >> f.txt; git commit -qam 'not pushed'
            echo changed >> f.txt; printf 'bin\\000ary' > new.bin");
        let (repo, task) = (dir.join("repo").canonicalize().unwrap(), dir.join("fix").canonicalize().unwrap());
        let unpushed_now = unpushed(&task);
        assert_eq!(unpushed_now.iter().map(|c| c.subject.as_str()).collect::<Vec<_>>(), ["not pushed"], "the pushed commit is on a remote");

        let config = dir.join("claude");
        let mut claude = Profile::claude_code();
        claude.env.insert("CLAUDE_CONFIG_DIR".into(), config.display().to_string());
        let projects = config.join("projects").join(crate::agents::claude::project_dir(&task));
        std::fs::create_dir_all(projects.join("s-1/subagents")).unwrap();
        std::fs::write(projects.join("s-1.jsonl"), "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"Done with the form.\"}]}}\n").unwrap();
        std::fs::write(projects.join("s-1/subagents/agent-a.jsonl"), "{}\n").unwrap();
        let chat = crate::acp::journal_path(&task);
        std::fs::create_dir_all(chat.parent().unwrap()).unwrap();
        std::fs::write(&chat, "{\"op\":\"Title\",\"title\":\"login\"}\n").unwrap();

        let base = git(&task, &["merge-base", "HEAD", "origin/main"]).unwrap().trim().to_string();
        let meta = Meta {
            title: "Fix login".into(),
            project: repo.clone(),
            worktree: task.clone(),
            branch: "fix".into(),
            base,
            sessions: vec![SessionMeta { profile: "claude".into(), name: "Claude Code".into(), kind: Kind::Claude, id: Some("s-1".into()), ..Default::default() }],
            ..Default::default()
        };
        let index_before = std::fs::read(git(&task, &["rev-parse", "--path-format=absolute", "--git-path", "index"]).unwrap().trim()).unwrap();
        let archives = dir.join("archive");
        // Somewhere it cannot be written: nothing is left, nothing changed.
        std::fs::write(dir.join("a-file"), "").unwrap();
        let refused = write(Job { meta: meta.clone(), profiles: vec![Some(claude.clone())], dest: dir.join("a-file/x/k.tar.gz") });
        assert!(refused.is_err());
        let dest = file_of(&archives, &repo, "fix-1");
        let written = write(Job { meta, profiles: vec![Some(claude.clone())], dest: dest.clone() }).unwrap();
        assert_eq!(std::fs::read(git(&task, &["rev-parse", "--path-format=absolute", "--git-path", "index"]).unwrap().trim()).unwrap(), index_before, "its index untouched");
        assert_eq!(written.said.as_deref(), Some("Done with the form."));
        assert_eq!(written.uncommitted, ["f.txt", "new.bin"]);
        assert_eq!(written.commits.len(), 2);
        let names: Vec<String> = entries(&dest).unwrap().into_iter().map(|(n, _)| n).collect();
        assert_eq!(names, ["task.json", "sessions/0/s-1.jsonl", "sessions/0/s-1/subagents/agent-a.jsonl", BUNDLE, PATCH, CHAT]);
        let listed = list(&archives);
        assert_eq!((listed.len(), listed[0].meta.title.as_str(), listed[0].key()), (1, "Fix login", "fix-1".to_string()));

        // Gone, as an archive leaves it.
        crate::git::remove_task_worktree(&repo, &task, "fix", false).unwrap();
        assert!(!task.exists() && !crate::git::branch_exists(&repo, "fix"));
        assert!(!chat.exists(), "the chat went with its task");

        let plan = Plan { start: Start::Origin, branch: crate::git::free_branch_name(&repo, "fix"), commits: true, changes: true, profiles: vec![Some(claude)], copy_env: false };
        assert_eq!(plan.branch, "fix-2", "origin has a fix");
        // An archive it cannot read: no worktree or branch is left of the try.
        let broken = Plan { branch: "broken".into(), ..plan.clone() };
        assert!(restore(&repo, &dir.join("a-file"), &written, &broken).is_err());
        assert!(!crate::git::branch_exists(&repo, "broken"), "its branch went with it");
        assert!(!git(&repo, &["worktree", "list"]).unwrap().contains("broken"), "and its worktree");
        let restored = restore(&repo, &dest, &written, &plan).unwrap();
        let at = &restored.path;
        assert_eq!((&restored.commits, &restored.changes, restored.placed), (&Applied::Done, &Applied::Done, 2));
        assert_eq!(git(at, &["log", "-1", "--format=%s"]).unwrap().trim(), "not pushed");
        assert_eq!(std::fs::read_to_string(at.join("f.txt")).unwrap(), "one\ntwo\nchanged\n");
        assert_eq!(std::fs::read(at.join("new.bin")).unwrap(), b"bin\0ary");
        assert!(!at.join("p.txt").exists(), "from origin's main, not the old branch");
        assert!(std::fs::read_to_string(crate::acp::journal_path(at)).unwrap().contains("login"), "the chat back, for the new worktree");
        let moved = config.join("projects").join(crate::agents::claude::project_dir(at));
        assert!(moved.join("s-1.jsonl").is_file() && moved.join("s-1/subagents/agent-a.jsonl").is_file(), "the session under the new folder, by its id");
        let claude = Profile { env: [("CLAUDE_CONFIG_DIR".to_string(), config.display().to_string())].into(), ..Profile::claude_code() };
        assert_eq!(crate::agents::Kind::Claude.adapter().transcript(&claude, at, Some("s-1")), Some(moved.join("s-1.jsonl")));

        // Again, where the patch no longer applies: kept, and said where.
        sh(&repo, "echo other > f.txt; git commit -qam moved; git push -q origin main");
        let plan = Plan { branch: crate::git::free_branch_name(&repo, "fix"), commits: false, profiles: vec![None], ..plan };
        let again = restore(&repo, &dest, &written, &plan).unwrap();
        assert_eq!(again.commits, Applied::Left);
        let Applied::Failed { kept, .. } = &again.changes else { panic!("{:?}", again.changes) };
        assert!(Path::new(kept).is_file(), "the patch kept");
        assert!(again.left_something());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
